//! Recording through cpal, on macOS and Windows.
//!
//! cpal's stream isn't `Send` everywhere, so a thread of the capture's own builds it, keeps it
//! and drops it. cpal calls `on_audio` on its own audio thread.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::startup::{self, Late, STARTS, Start};
use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};

/// How long listing or opening may take, from the start.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The last capture's number. Numbers start at 1.
static LAST_CAPTURE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The number of the capture whose `on_audio` this thread is in, or 0.
    static IN_CALLBACK_OF: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let listing = |start: Start<Result<Vec<Device>, Error>>| {
        let host = cpal::default_host();
        let default = host
            .default_input_device()
            .and_then(|device| device.id().ok());
        let devices = host
            .input_devices()
            .map_err(|error| convert(error, "listing the devices"))
            .map(|devices| {
                devices
                    .map(|device| describe(&device, default.as_ref()))
                    .collect()
            });
        let _ = start.finish(devices);
    };
    match startup::run("capta-devices", deadline, &STARTS, listing) {
        Ok((devices, _thread)) => devices,
        Err(late) => Err(late_error(late, "listing the devices")),
    }
}

/// Why a startup gave no result, as an error.
fn late_error(late: Late, doing: &str) -> Error {
    let allowed = TIMEOUT.as_secs_f32();
    match late {
        Late::Deadline => Error::new(
            ErrorKind::Stalled,
            format!("{doing} took more than the {allowed:.1} s allowed"),
        ),
        Late::Crowded => Error::new(
            ErrorKind::Stalled,
            format!("earlier starts were still waiting on the audio system after {allowed:.1} s"),
        ),
        Late::Failed => Error::new(ErrorKind::Io, format!("the thread {doing} failed")),
    }
}

fn describe(device: &cpal::Device, default: Option<&cpal::DeviceId>) -> Device {
    let id = device.id().ok();
    let name = device
        .description()
        .map(|description| description.name().to_owned());
    Device {
        is_default: id.is_some() && id.as_ref() == default,
        id: id.map(|id| id.to_string()).unwrap_or_default(),
        name: name.unwrap_or_default(),
    }
}

/// A running capture's handle.
pub(crate) struct Stream {
    number: u64,
    stop: Arc<AtomicBool>,
    messages: mpsc::Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

/// What the capture thread waits for.
enum Message {
    Stop,
    Failed(cpal::Error),
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.messages.send(Message::Stop);
        // From one of this capture's callbacks, the thread can't drop cpal's stream until the
        // callback returns, so it's left to end by itself.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
            && IN_CALLBACK_OF.get() != self.number
        {
            let _ = thread.join();
        }
    }
}

pub(crate) fn open(
    options: &Options,
    on_audio: OnAudio,
    on_error: OnError,
) -> Result<Opened, Error> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let number = LAST_CAPTURE.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    let (messages, inbox) = mpsc::channel();
    let controller = {
        let options = options.clone();
        let stop = Arc::clone(&stop);
        let messages = messages.clone();
        move |start: Start<Result<(Device, u32), Error>>| {
            let gate = Gate {
                number,
                handed_over: Arc::new(AtomicBool::new(false)),
                stop: Arc::clone(&stop),
                failed: Arc::new(AtomicBool::new(false)),
            };
            let (handed_over, failed) = (Arc::clone(&gate.handed_over), Arc::clone(&gate.failed));
            let stream = match start_stream(&options, &start, on_audio, gate, messages) {
                Ok((stream, device, rate)) => {
                    if start.finish(Ok((device, rate))).is_err() {
                        // Too late. The gate never opened, so dropping the stream calls nothing.
                        drop(stream);
                        return;
                    }
                    handed_over.store(true, Ordering::SeqCst);
                    stream
                }
                Err(error) => {
                    let _ = start.finish(Err(error));
                    return;
                }
            };
            // The stream is handed over, so the permit goes back.
            drop(start);
            if let Ok(Message::Failed(error)) = inbox.recv() {
                failed.store(true, Ordering::SeqCst);
                drop(stream);
                if !stop.load(Ordering::SeqCst) {
                    let error = convert(error, "recording");
                    on_error(match error.kind() {
                        ErrorKind::NoDevice => {
                            Error::new(ErrorKind::Disconnected, error.to_string())
                        }
                        _ => error,
                    });
                }
            }
        }
    };
    match startup::run("capta", deadline, &STARTS, controller) {
        Ok((Ok((device, sample_rate)), thread)) => Ok(Opened {
            stream: Stream {
                number,
                stop,
                messages,
                thread: Some(thread),
            },
            device,
            sample_rate,
        }),
        Ok((Err(error), _thread)) => Err(error),
        Err(late) => Err(late_error(late, "opening the device")),
    }
}

/// What lets callbacks through, once the stream is handed over and until the capture is
/// dropped or has failed.
struct Gate {
    number: u64,
    handed_over: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Gate {
    fn is_open(&self) -> bool {
        self.handed_over.load(Ordering::SeqCst)
            && !self.stop.load(Ordering::SeqCst)
            && !self.failed.load(Ordering::SeqCst)
    }
}

/// Opens the device and starts it, at the rate asked for when it has it. Each phase begins
/// only while the startup is on.
fn start_stream<T>(
    options: &Options,
    start: &Start<T>,
    on_audio: OnAudio,
    gate: Gate,
    messages: mpsc::Sender<Message>,
) -> Result<(cpal::Stream, Device, u32), Error> {
    let over = || Error::new(ErrorKind::Stalled, "opening the device took too long");
    let host = cpal::default_host();
    let default = host.default_input_device();
    let default_id = default.as_ref().and_then(|device| device.id().ok());
    let device = match &options.device {
        None => default
            .ok_or_else(|| Error::new(ErrorKind::NoDevice, "the system has no microphone"))?,
        Some(name) => name
            .parse()
            .ok()
            .and_then(|id| host.device_by_id(&id))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NoDevice,
                    format!("the system has no device named {name}"),
                )
            })?,
    };
    let described = describe(&device, default_id.as_ref());
    if start.is_over() {
        return Err(over());
    }

    let formats = device
        .supported_input_configs()
        .map_err(|error| convert(error, "reading the device's formats"))?;
    let config = formats
        .filter_map(|range| range.try_with_sample_rate(options.sample_rate))
        .filter(|config| is_decodable(config.sample_format()))
        .min_by_key(|config| {
            (
                config.sample_format() != SampleFormat::F32,
                config.channels(),
            )
        });
    let config = match config {
        Some(config) => config,
        None => device
            .default_input_config()
            .map_err(|error| convert(error, "reading the device's formats"))?,
    };
    if start.is_over() {
        return Err(over());
    }
    let rate = config.sample_rate();
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let timeout = Some(start.time_left());
    let on_failure = failure_callback(Arc::clone(&gate.failed), messages);
    let channels = usize::from(config.channels().max(1));
    macro_rules! build {
        ($sample:ty) => {{
            let mut on_data = data_callback::<$sample>(channels, rate, on_audio, gate);
            let on_data = move |data: &[$sample], _: &cpal::InputCallbackInfo| on_data(data);
            device.build_input_stream::<$sample, _, _>(stream_config, on_data, on_failure, timeout)
        }};
    }
    let stream = match config.sample_format() {
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        other => {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!("the device's sample format {other} isn't supported"),
            ));
        }
    }
    .map_err(|error| convert(error, "opening the device"))?;
    if start.is_over() {
        return Err(over());
    }
    stream
        .play()
        .map_err(|error| convert(error, "starting the device"))?;
    Ok((stream, described, rate))
}

fn is_decodable(format: SampleFormat) -> bool {
    use SampleFormat::*;
    matches!(format, F32 | F64 | I8 | I16 | I32 | U8 | U16 | U32)
}

/// What cpal calls with each buffer. It mixes the device's channels down to mono and hands
/// them to `on_audio` while the gate is open.
fn data_callback<T>(
    channels: usize,
    rate: u32,
    mut on_audio: OnAudio,
    gate: Gate,
) -> impl FnMut(&[T]) + Send + 'static
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut mono = Vec::new();
    move |data: &[T]| {
        if !gate.is_open() {
            return;
        }
        mono.clear();
        mono.extend(data.chunks_exact(channels).map(|frame| {
            frame
                .iter()
                .map(|sample| sample.to_sample::<f32>())
                .sum::<f32>()
                / channels as f32
        }));
        let _inside = InCallback::enter(gate.number);
        on_audio(&mono, rate);
    }
}

/// Marks this thread as inside a capture's `on_audio` until dropped.
struct InCallback {
    outer: u64,
}

impl InCallback {
    fn enter(number: u64) -> InCallback {
        InCallback {
            outer: IN_CALLBACK_OF.replace(number),
        }
    }
}

impl Drop for InCallback {
    fn drop(&mut self) {
        IN_CALLBACK_OF.set(self.outer);
    }
}

/// What cpal calls with an error. One that ends the recording closes the gate at once, so the
/// buffer after it never reaches `on_audio`, and goes to the capture thread.
fn failure_callback(
    failed: Arc<AtomicBool>,
    messages: mpsc::Sender<Message>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    move |error: cpal::Error| {
        if is_fatal(error.kind()) && !failed.swap(true, Ordering::SeqCst) {
            let _ = messages.send(Message::Failed(error));
        }
    }
}

/// Whether an error ends the recording. An overrun means audio was lost, so it does, though
/// cpal goes on. A change of route and a refused real-time priority don't.
fn is_fatal(kind: cpal::ErrorKind) -> bool {
    !matches!(
        kind,
        cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied
    )
}

fn convert(error: cpal::Error, doing: &str) -> Error {
    let kind = match error.kind() {
        cpal::ErrorKind::DeviceNotAvailable => ErrorKind::NoDevice,
        cpal::ErrorKind::HostUnavailable => ErrorKind::NoServer,
        cpal::ErrorKind::DeviceBusy => ErrorKind::Busy,
        cpal::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        cpal::ErrorKind::StreamInvalidated => ErrorKind::Disconnected,
        cpal::ErrorKind::Xrun => ErrorKind::Overrun,
        cpal::ErrorKind::UnsupportedConfig
        | cpal::ErrorKind::UnsupportedOperation
        | cpal::ErrorKind::InvalidInput => ErrorKind::Unsupported,
        _ => ErrorKind::Io,
    };
    Error::new(kind, format!("{doing} failed ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(number: u64) -> Gate {
        Gate {
            number,
            handed_over: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn an_overrun_ends_the_recording_before_the_next_buffer() {
        let gate = gate(1);
        let failed = Arc::clone(&gate.failed);
        let (heard, pieces) = mpsc::channel();
        let mut on_data = data_callback::<f32>(
            2,
            48_000,
            Box::new(move |samples: &[f32], _: u32| heard.send(samples.to_vec()).unwrap()),
            gate,
        );
        let (messages, inbox) = mpsc::channel();
        let mut on_failure = failure_callback(failed, messages);
        on_data(&[0.5, 0.25, -0.5, 0.0]);
        assert_eq!(pieces.try_recv().unwrap(), [0.375, -0.25]);
        on_failure(cpal::Error::new(cpal::ErrorKind::Xrun));
        on_failure(cpal::Error::new(cpal::ErrorKind::Xrun));
        on_data(&[0.5, 0.5]);
        assert!(pieces.try_recv().is_err(), "audio after the gap");
        assert!(matches!(inbox.try_recv(), Ok(Message::Failed(_))));
        assert!(inbox.try_recv().is_err(), "a second failure");
        let error = convert(cpal::Error::new(cpal::ErrorKind::Xrun), "recording");
        assert_eq!(error.kind(), ErrorKind::Overrun);
    }

    #[test]
    fn nothing_comes_before_the_stream_is_handed_over() {
        let gate = gate(1);
        let handed_over = Arc::clone(&gate.handed_over);
        handed_over.store(false, Ordering::SeqCst);
        let (heard, pieces) = mpsc::channel();
        let mut on_data = data_callback::<f32>(
            1,
            48_000,
            Box::new(move |samples: &[f32], _: u32| heard.send(samples.len()).unwrap()),
            gate,
        );
        on_data(&[0.5]);
        assert!(pieces.try_recv().is_err());
        handed_over.store(true, Ordering::SeqCst);
        on_data(&[0.5]);
        assert_eq!(pieces.try_recv().unwrap(), 1);
    }

    #[test]
    fn a_route_change_keeps_recording() {
        let gate = gate(1);
        let failed = Arc::clone(&gate.failed);
        let (heard, pieces) = mpsc::channel();
        let mut on_data = data_callback::<i16>(
            1,
            48_000,
            Box::new(move |samples: &[f32], _: u32| heard.send(samples.len()).unwrap()),
            gate,
        );
        let (messages, inbox) = mpsc::channel();
        let mut on_failure = failure_callback(failed, messages);
        on_failure(cpal::Error::new(cpal::ErrorKind::DeviceChanged));
        on_data(&[1, 2, 3]);
        assert_eq!(pieces.try_recv().unwrap(), 3);
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn a_callback_knows_its_own_capture() {
        let (heard, inside) = mpsc::channel();
        let mut on_data = data_callback::<f32>(
            1,
            48_000,
            Box::new(move |_: &[f32], _: u32| heard.send(IN_CALLBACK_OF.get()).unwrap()),
            gate(7),
        );
        let outer = InCallback::enter(3);
        on_data(&[0.0]);
        assert_eq!(inside.try_recv().unwrap(), 7);
        assert_eq!(IN_CALLBACK_OF.get(), 3);
        drop(outer);
        assert_eq!(IN_CALLBACK_OF.get(), 0);
    }
}
