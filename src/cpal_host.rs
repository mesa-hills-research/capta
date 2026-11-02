//! Recording through cpal, on macOS and Windows.
//!
//! cpal's stream isn't `Send` everywhere, so a thread of the capture's own builds it, keeps it
//! and drops it. cpal calls `on_audio` on its own audio thread.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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

thread_local! {
    /// Whether this thread is inside `on_audio`, where dropping the capture mustn't wait.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
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
        // From a callback, the thread can't drop cpal's stream until the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
            && !IN_CALLBACK.get()
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
    let on_failure = move |error: cpal::Error| {
        if is_fatal(error.kind()) {
            let _ = messages.send(Message::Failed(error));
        }
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            input::<f32>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::F64 => {
            input::<f64>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::I8 => {
            input::<i8>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::I16 => {
            input::<i16>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::I32 => {
            input::<i32>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::U8 => {
            input::<u8>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::U16 => {
            input::<u16>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
        SampleFormat::U32 => {
            input::<u32>(&device, stream_config, timeout, on_audio, gate, on_failure)
        }
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

/// Builds a stream that mixes the device's channels down to mono.
fn input<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    timeout: Duration,
    mut on_audio: OnAudio,
    gate: Gate,
    on_failure: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let rate = config.sample_rate;
    let mut mono = Vec::new();
    let on_data = move |data: &[T], _: &cpal::InputCallbackInfo| {
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
        IN_CALLBACK.set(true);
        on_audio(&mono, rate);
        IN_CALLBACK.set(false);
    };
    device.build_input_stream::<T, _, _>(config, on_data, on_failure, Some(timeout))
}

/// Whether an error ends the recording. cpal reports some it recovers from.
fn is_fatal(kind: cpal::ErrorKind) -> bool {
    !matches!(
        kind,
        cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied
    )
}

fn convert(error: cpal::Error, doing: &str) -> Error {
    let kind = match error.kind() {
        cpal::ErrorKind::DeviceNotAvailable => ErrorKind::NoDevice,
        cpal::ErrorKind::HostUnavailable => ErrorKind::NoServer,
        cpal::ErrorKind::DeviceBusy => ErrorKind::Busy,
        cpal::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        cpal::ErrorKind::StreamInvalidated => ErrorKind::Disconnected,
        cpal::ErrorKind::UnsupportedConfig
        | cpal::ErrorKind::UnsupportedOperation
        | cpal::ErrorKind::InvalidInput => ErrorKind::Unsupported,
        _ => ErrorKind::Io,
    };
    Error::new(kind, format!("{doing} failed ({error})"))
}
