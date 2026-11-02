//! Recording through cpal, on macOS and Windows.
//!
//! cpal's stream isn't `Send` everywhere, so a thread of the capture's own builds it, keeps it
//! and drops it. cpal calls `on_audio` on its own audio thread.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};

/// How long opening may take, from the start.
const TIMEOUT: Duration = Duration::from_secs(3);
/// How many captures may still be opening after their deadline before opening fails at once.
const MAX_LATE_STARTS: usize = 4;

/// Captures whose thread is still opening the device after `open` gave up on it.
static LATE_STARTS: AtomicUsize = AtomicUsize::new(0);
/// The last capture's number. Numbers start at 1.
static LAST_CAPTURE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The number of the capture whose `on_audio` this thread is in, or 0.
    static IN_CALLBACK_OF: Cell<u64> = const { Cell::new(0) };
}

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .and_then(|device| device.id().ok());
    let devices = host
        .input_devices()
        .map_err(|error| convert(error, "listing the devices"))?;
    Ok(devices
        .map(|device| describe(&device, default.as_ref()))
        .collect())
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

/// Whether the opening thread is still at it, done, or given up on.
const STARTING: u8 = 0;
const STARTED: u8 = 1;
const ABANDONED: u8 = 2;

pub(crate) fn open(
    options: &Options,
    on_audio: OnAudio,
    on_error: OnError,
) -> Result<Opened, Error> {
    let deadline = Instant::now() + TIMEOUT;
    if LATE_STARTS.load(Ordering::SeqCst) >= MAX_LATE_STARTS {
        return Err(Error::new(
            ErrorKind::Stalled,
            "the audio system is still opening earlier captures",
        ));
    }
    let number = LAST_CAPTURE.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(AtomicU8::new(STARTING));
    let (messages, inbox) = mpsc::channel();
    let (started_sender, started) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("capta".into())
        .spawn({
            let options = options.clone();
            let (stop, state) = (Arc::clone(&stop), Arc::clone(&state));
            let messages = messages.clone();
            move || {
                let failed = Arc::new(AtomicBool::new(false));
                let gate = Gate {
                    number,
                    stop: Arc::clone(&stop),
                    failed: Arc::clone(&failed),
                };
                let result = start(&options, deadline, on_audio, gate, messages);
                let _ = started_sender.send(
                    result
                        .as_ref()
                        .map(|(_, device, rate)| (device.clone(), *rate))
                        .map_err(Error::clone),
                );
                if state
                    .compare_exchange(STARTING, STARTED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    // `open` gave up on this capture, and the gate is closed.
                    LATE_STARTS.fetch_sub(1, Ordering::SeqCst);
                    return;
                }
                let Ok((stream, _, _)) = result else { return };
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
        })
        .map_err(|error| {
            Error::new(
                ErrorKind::Io,
                format!("starting the capture thread failed ({error})"),
            )
        })?;
    let stream = Stream {
        number,
        stop,
        messages,
        thread: Some(thread),
    };
    let result = match started.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::new(
            ErrorKind::Io,
            "the capture thread ended before the recording started",
        )),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            LATE_STARTS.fetch_add(1, Ordering::SeqCst);
            if state
                .compare_exchange(STARTING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                // Closing the gate keeps a late start silent, and the thread ends by itself.
                let mut stream = stream;
                stream.stop.store(true, Ordering::SeqCst);
                stream.thread.take();
                return Err(Error::new(
                    ErrorKind::Stalled,
                    format!(
                        "the audio system didn't open the device within {:.1} s",
                        TIMEOUT.as_secs_f32()
                    ),
                ));
            }
            // It started just after the deadline.
            LATE_STARTS.fetch_sub(1, Ordering::SeqCst);
            started.try_recv().unwrap_or_else(|_| {
                Err(Error::new(
                    ErrorKind::Io,
                    "the capture thread ended before the recording started",
                ))
            })
        }
    };
    let (device, sample_rate) = result?;
    Ok(Opened {
        stream,
        device,
        sample_rate,
    })
}

/// What stops callbacks, once the capture is dropped or has failed.
struct Gate {
    number: u64,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

impl Gate {
    fn is_open(&self) -> bool {
        !self.stop.load(Ordering::SeqCst) && !self.failed.load(Ordering::SeqCst)
    }
}

/// Opens the device and starts it, at the rate asked for when it has it.
fn start(
    options: &Options,
    deadline: Instant,
    on_audio: OnAudio,
    gate: Gate,
    messages: mpsc::Sender<Message>,
) -> Result<(cpal::Stream, Device, u32), Error> {
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
    let rate = config.sample_rate();
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let timeout = Some(deadline.saturating_duration_since(Instant::now()));
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
