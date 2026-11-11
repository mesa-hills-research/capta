//! Recording through Core Audio, on macOS.
//!
//! One thread per capture makes an Audio Queue that records the device as mono floats at the
//! rate asked for, converting the device's format and rate itself. The queue's callback, on a
//! thread of Audio Toolbox's own, copies each buffer into a ring that the capture thread reads.
//! The `Capture` handle shares only a stop flag and the capture thread's handle, to wake it.

mod ffi;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ffi::{AudioObjectId, CfString, Failure, Gathered, OsStatus, Queue};

use crate::native::{
    Pieces, STALL, TIMEOUT, frames_in, late_error, overrun, parse_id, stalled, too_slow,
};
use crate::startup::{self, STARTS, Start};
use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};

/// What starts a device's id, as in Capta 0.1.
const PREFIX: &str = "coreaudio";
/// How much audio the queue's buffers hold together, and the ring the callback fills, 2 s as
/// the Linux server's buffer.
const QUEUED: Duration = Duration::from_secs(2);
/// How often the capture thread looks for audio. The callback doesn't wake it, so that it calls
/// nothing that might block.
const POLL: Duration = Duration::from_millis(5);
/// How often the capture thread checks that the device is still there.
const CHECK: Duration = Duration::from_millis(250);

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    let deadline = Instant::now() + TIMEOUT;
    let listing = |start: Start<Result<Vec<Device>, Error>>| {
        if start.is_over() {
            return;
        }
        let _ = start.finish(list());
    };
    match startup::run("capta-devices", deadline, &STARTS, listing) {
        Ok((devices, _thread)) => devices,
        Err(late) => Err(late_error(late, "listing the devices")),
    }
}

fn list() -> Result<Vec<Device>, Error> {
    let failed = |failed| failure(failed, "listing the devices");
    let default = ffi::default_input().map_err(failed)?;
    let devices = ffi::devices().map_err(failed)?;
    Ok(devices
        .into_iter()
        .filter_map(|object| describe(object, default))
        .collect())
}

/// A device with input as `devices` lists it, or none for one without input or whose UID
/// can't be read.
fn describe(object: AudioObjectId, default: Option<AudioObjectId>) -> Option<Device> {
    if !ffi::has_input(object).ok()? {
        return None;
    }
    let uid = ffi::uid(object).ok()?;
    Some(Device {
        id: format!("{PREFIX}:{uid}"),
        name: ffi::name(object).ok().flatten().unwrap_or_default(),
        is_default: default == Some(object),
    })
}

/// A running capture's handle.
pub(crate) struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // From a callback, the capture thread ends by itself once the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// What the capture thread hands over once the device runs.
type Handover = Result<(Device, u32), Error>;

pub(crate) fn open(
    options: &Options,
    on_audio: OnAudio,
    on_error: OnError,
) -> Result<Opened, Error> {
    let deadline = Instant::now() + TIMEOUT;
    let stop = Arc::new(AtomicBool::new(false));
    let capture = {
        let options = options.clone();
        let stop = Arc::clone(&stop);
        move |start: Start<Handover>| record(&options, start, &stop, on_audio, on_error)
    };
    match startup::run("capta", deadline, &STARTS, capture) {
        Ok((Ok((device, sample_rate)), thread)) => Ok(Opened {
            stream: Stream {
                stop,
                thread: Some(thread),
            },
            device,
            sample_rate,
        }),
        Ok((Err(error), _thread)) => Err(error),
        Err(late) => Err(late_error(late, "opening the device")),
    }
}

/// The capture thread's work, from opening the device to the end of the recording.
fn record(
    options: &Options,
    start: Start<Handover>,
    stop: &AtomicBool,
    mut on_audio: OnAudio,
    on_error: OnError,
) {
    let recorder = match Recorder::open(options, &start) {
        Ok(recorder) => recorder,
        Err(error) => {
            let _ = start.finish(Err(error));
            return;
        }
    };
    let handover = Ok((recorder.device.clone(), recorder.rate));
    if start.finish(handover).is_err() {
        // Too late. Nothing has read the ring, so dropping the queue calls nothing.
        return;
    }
    // The capture is handed over, so the permit goes back.
    drop(start);
    let result = recorder.run(stop, &mut on_audio, frames_in(recorder.rate, options.piece));
    drop(recorder);
    if let Err(error) = result
        && !stop.load(Ordering::SeqCst)
    {
        on_error(error);
    }
}

/// A started queue and the device it records.
struct Recorder {
    queue: Queue,
    object: AudioObjectId,
    device: Device,
    rate: u32,
}

impl Recorder {
    /// Opens the device and starts it. Each phase begins only while the startup is on.
    fn open<T>(options: &Options, start: &Start<T>) -> Result<Recorder, Error> {
        let failed = |failed| failure(failed, "opening the device");
        if start.is_over() {
            return Err(too_slow());
        }
        let default = ffi::default_input().map_err(failed)?;
        let (object, device) = match &options.device {
            None => {
                let none = || Error::new(ErrorKind::NoDevice, "the system has no microphone");
                let object = default.ok_or_else(none)?;
                (object, describe(object, default).ok_or_else(none)?)
            }
            Some(wanted) => {
                let unknown = || {
                    Error::new(
                        ErrorKind::NoDevice,
                        format!("the system has no device named {wanted}"),
                    )
                };
                parse_id(wanted, PREFIX).ok_or_else(unknown)?;
                ffi::devices()
                    .map_err(failed)?
                    .into_iter()
                    .find_map(|object| {
                        let device = describe(object, default)?;
                        (device.id == *wanted).then_some((object, device))
                    })
                    .ok_or_else(unknown)?
            }
        };
        if start.is_over() {
            return Err(too_slow());
        }

        let uid = parse_id(&device.id, PREFIX).unwrap_or_default();
        let uid = CfString::new(uid).map_err(failed)?;
        let mut rate = options.sample_rate;
        let queue = loop {
            let started = prepare_queue(&uid, rate, options.piece).and_then(|queue| {
                if start.is_over() {
                    return Ok(None);
                }
                queue.start()?;
                Ok(Some(queue))
            });
            match started {
                Ok(Some(queue)) => break queue,
                Ok(None) => return Err(too_slow()),
                Err(Failure::Status(status))
                    if is_format_refusal(status) && rate == options.sample_rate =>
                {
                    // Record at the device's own rate instead.
                    let own = ffi::nominal_rate(object).map_err(failed)?.round();
                    rate = Some(own)
                        .filter(|own| (1.0..=384_000.0).contains(own))
                        .map(|own| own as u32)
                        .filter(|&own| own != rate)
                        .ok_or_else(|| failed(Failure::Status(status)))?;
                }
                Err(failed_start) => return Err(failed(failed_start)),
            }
        };
        Ok(Recorder {
            queue,
            object,
            device,
            rate,
        })
    }

    /// Hands audio to `on_audio` in pieces of `piece` frames until the capture stops, or
    /// returns why it failed.
    fn run(&self, stop: &AtomicBool, on_audio: &mut OnAudio, piece: usize) -> Result<(), Error> {
        let mut pieces = Pieces::new(piece);
        let mut samples = Vec::new();
        let mut audio_due = Instant::now() + STALL;
        let mut check_due = Instant::now() + CHECK;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::park_timeout(POLL);
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let gathered = self.queue.drain(&mut samples);
            pieces.extend(samples.drain(..));
            pieces.deliver(|piece| {
                on_audio(piece, self.rate);
                // A slow callback doesn't count against the device.
                audio_due = Instant::now() + STALL;
                !stop.load(Ordering::SeqCst)
            });
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            match gathered {
                Gathered::Fine => {}
                Gathered::Gap | Gathered::Full => return Err(overrun()),
                Gathered::Malformed => {
                    return Err(Error::new(
                        ErrorKind::Protocol,
                        "the system handed over a buffer that ends inside a sample",
                    ));
                }
                Gathered::Failed(status) => return Err(recording(Failure::Status(status))),
            }
            if Instant::now() >= check_due {
                if !ffi::is_alive(self.object).unwrap_or(false) {
                    return Err(Error::new(
                        ErrorKind::Disconnected,
                        "recording failed (the device is gone)",
                    ));
                }
                check_due = Instant::now() + CHECK;
            }
            if Instant::now() >= audio_due {
                return Err(stalled());
            }
        }
    }
}

/// Makes a queue that records the device with `uid` at `rate`, and gives it buffers of about
/// `piece` each, enough for [`QUEUED`].
fn prepare_queue(uid: &CfString, rate: u32, piece: Duration) -> Result<Queue, Failure> {
    let buffer = piece.clamp(Duration::from_millis(10), Duration::from_millis(100));
    let frames = frames_in(rate, buffer);
    let bytes = frames
        .checked_mul(4)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or(Failure::Malformed("a buffer too large"))?;
    let count = QUEUED.as_nanos().div_ceil(buffer.as_nanos()).clamp(3, 200) as usize;
    let capacity = frames_in(rate, QUEUED).saturating_add(frames);
    // Start times may drift by a millisecond from where the last buffer ended.
    let tolerance = f64::from(rate) / 1000.0 + 1.0;
    let queue = Queue::new_input(rate, capacity, tolerance)?;
    queue.set_device(uid)?;
    queue.add_buffers(count, bytes)?;
    Ok(queue)
}

/// A status that refuses the format, as for a rate the system can't convert to.
fn is_format_refusal(status: OsStatus) -> bool {
    const PARAM_ERROR: OsStatus = -50;
    const QUEUE_INVALID_PARAMETER: OsStatus = -66682;
    status == PARAM_ERROR
        || status == QUEUE_INVALID_PARAMETER
        || status == four_char(b"fmt?")
        || status == four_char(b"!dat")
}

const fn four_char(code: &[u8; 4]) -> OsStatus {
    i32::from_be_bytes(*code)
}

/// A failed call, as an error.
fn failure(failed: Failure, doing: &str) -> Error {
    let status = match failed {
        Failure::Status(status) => status,
        Failure::Malformed(what) => {
            return Error::new(
                ErrorKind::Protocol,
                format!("{doing} failed (the system answered with {what})"),
            );
        }
    };
    let (kind, reason) = match status {
        s if s == four_char(b"stop") => (
            ErrorKind::NoServer,
            "the audio system isn't running".to_owned(),
        ),
        s if s == four_char(b"!dev") || s == four_char(b"!obj") || s == -66680 => {
            (ErrorKind::NoDevice, "the device is gone".to_owned())
        }
        s if s == four_char(b"!hog") => (
            ErrorKind::Busy,
            "another program has the device to itself".to_owned(),
        ),
        -66676 => (
            ErrorKind::PermissionDenied,
            "the system refused access to the microphone".to_owned(),
        ),
        s if is_format_refusal(s) => (
            ErrorKind::Unsupported,
            "the device can't record in this format".to_owned(),
        ),
        -66671 => (
            ErrorKind::Disconnected,
            "the system ended the recording".to_owned(),
        ),
        other => (ErrorKind::Io, format!("status {}", describe_status(other))),
    };
    Error::new(kind, format!("{doing} failed ({reason})"))
}

/// A failed call while recording. A device or system that went away disconnects the capture.
fn recording(failed: Failure) -> Error {
    let error = failure(failed, "recording");
    match error.kind() {
        ErrorKind::NoDevice | ErrorKind::NoServer => {
            Error::new(ErrorKind::Disconnected, error.to_string())
        }
        _ => error,
    }
}

/// A status as its four characters when they're printable, else as a number.
fn describe_status(status: OsStatus) -> String {
    let bytes = status.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_say_what_happened() {
        let error = failure(Failure::Status(four_char(b"!hog")), "opening the device");
        assert_eq!(error.kind(), ErrorKind::Busy);
        assert_eq!(
            error.to_string(),
            "opening the device failed (another program has the device to itself)"
        );
        let error = recording(Failure::Status(four_char(b"!dev")));
        assert_eq!(error.kind(), ErrorKind::Disconnected);
        assert_eq!(error.to_string(), "recording failed (the device is gone)");
        let error = failure(Failure::Status(four_char(b"what")), "listing the devices");
        assert_eq!(
            error.to_string(),
            "listing the devices failed (status 'what')"
        );
        let error = failure(Failure::Status(-66681), "starting the device");
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error.to_string(),
            "starting the device failed (status -66681)"
        );
        let error = failure(Failure::Malformed("a null queue"), "opening the device");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn a_rate_the_system_refuses_is_a_format_refusal() {
        assert!(is_format_refusal(four_char(b"fmt?")));
        assert!(is_format_refusal(-50));
        assert!(!is_format_refusal(four_char(b"!hog")));
    }
}
