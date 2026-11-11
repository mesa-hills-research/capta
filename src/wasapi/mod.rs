//! Recording through WASAPI, on Windows.
//!
//! One thread per capture enters COM's multithreaded apartment, opens the endpoint in shared
//! mode and waits on the event the audio engine sets for each packet. The engine converts the
//! device's format to mono floats at the rate asked for. The `Capture` handle shares only a stop
//! flag and a duplicate of the event's handle with that thread.

mod com;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use com::{Apartment, AudioClient, CaptureClient, Endpoint, Event, Hresult, WaveFormatEx};

use crate::native::{
    Pieces, STALL, TIMEOUT, frames_in, late_error, overrun, parse_id, stalled, too_slow,
};
use crate::startup::{self, STARTS, Start};
use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};

/// What starts a device's id, as in Capta 0.1.
const PREFIX: &str = "wasapi";
/// The engine's buffer in 100 ns units, 2 s as the Linux server's.
const ENGINE_BUFFER: i64 = 20_000_000;
/// How often the capture thread asks for packets when the engine signals none.
const POLL: Duration = Duration::from_millis(100);

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    let deadline = Instant::now() + TIMEOUT;
    let listing = |start: Start<Result<Vec<Device>, Error>>| {
        if start.is_over() {
            return;
        }
        let devices = Apartment::enter()
            .map_err(|code| failure(code, "listing the devices"))
            .and_then(|apartment| list(&apartment));
        let _ = start.finish(devices);
    };
    match startup::run("capta-devices", deadline, &STARTS, listing) {
        Ok((devices, _thread)) => devices,
        Err(late) => Err(late_error(late, "listing the devices")),
    }
}

fn list(apartment: &Apartment) -> Result<Vec<Device>, Error> {
    let failed = |code| failure(code, "listing the devices");
    let enumerator = apartment.enumerator().map_err(failed)?;
    let default = enumerator.default_capture().map_err(failed)?;
    let default = default.and_then(|endpoint| endpoint.id().ok().flatten());
    let endpoints = enumerator.active_captures().map_err(failed)?;
    Ok(endpoints
        .iter()
        .filter_map(|endpoint| describe(endpoint, default.as_deref()))
        .collect())
}

/// The endpoint as `devices` lists it, or none when its id can't be read.
fn describe(endpoint: &Endpoint<'_>, default: Option<&str>) -> Option<Device> {
    let id = endpoint.id().ok().flatten()?;
    Some(Device {
        is_default: default == Some(id.as_str()),
        name: endpoint.name().ok().flatten().unwrap_or_default(),
        id: format!("{PREFIX}:{id}"),
    })
}

/// A running capture's handle.
pub(crate) struct Stream {
    stop: Arc<AtomicBool>,
    wake: Event,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.set();
        // From a callback, the capture thread ends by itself once the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

/// What the capture thread hands over once the device runs.
type Handover = Result<(Device, u32, Event), Error>;

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
        Ok((Ok((device, sample_rate, wake)), thread)) => Ok(Opened {
            stream: Stream {
                stop,
                wake,
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
    let apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(code) => {
            let _ = start.finish(Err(failure(code, "opening the device")));
            return;
        }
    };
    let client = match Client::open(&apartment, options, &start) {
        Ok(client) => client,
        Err(error) => {
            let _ = start.finish(Err(error));
            return;
        }
    };
    let handover = client
        .event
        .try_clone()
        .map(|wake| (client.device.clone(), client.rate, wake))
        .map_err(|error| io_failure(&error, "opening the device"));
    let handing_over = handover.is_ok();
    if start.finish(handover).is_err() || !handing_over {
        // Too late, or nothing to hand over. Nothing has reached `on_audio`.
        let _ = client.audio.stop();
        return;
    }
    // The capture is handed over, so the permit goes back.
    drop(start);
    let result = client.run(stop, &mut on_audio, frames_in(client.rate, options.piece));
    let _ = client.audio.stop();
    drop(client);
    drop(apartment);
    if let Err(error) = result
        && !stop.load(Ordering::SeqCst)
    {
        on_error(error);
    }
}

/// A started shared-mode stream and what reading it needs.
struct Client<'a> {
    audio: AudioClient<'a>,
    capture: CaptureClient<'a>,
    event: Event,
    device: Device,
    rate: u32,
    buffer_frames: u32,
}

impl<'a> Client<'a> {
    /// Opens the device and starts it. Each phase begins only while the startup is on.
    fn open<T>(
        apartment: &'a Apartment,
        options: &Options,
        start: &Start<T>,
    ) -> Result<Client<'a>, Error> {
        let failed = |code| failure(code, "opening the device");
        if start.is_over() {
            return Err(too_slow());
        }
        let enumerator = apartment.enumerator().map_err(failed)?;
        let default = enumerator.default_capture().map_err(failed)?;
        let (endpoint, device) = match &options.device {
            None => {
                let endpoint = default.ok_or_else(|| {
                    Error::new(ErrorKind::NoDevice, "the system has no microphone")
                })?;
                let device = describe(&endpoint, None).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Protocol,
                        "the system gave the default microphone an id that isn't valid UTF-16",
                    )
                })?;
                (
                    endpoint,
                    Device {
                        is_default: true,
                        ..device
                    },
                )
            }
            Some(wanted) => {
                let unknown = || {
                    Error::new(
                        ErrorKind::NoDevice,
                        format!("the system has no device named {wanted}"),
                    )
                };
                let id = parse_id(wanted, PREFIX).ok_or_else(unknown)?;
                let default = default.and_then(|endpoint| endpoint.id().ok().flatten());
                let endpoint = enumerator
                    .active_captures()
                    .map_err(failed)?
                    .into_iter()
                    .find(|endpoint| endpoint.id().ok().flatten().as_deref() == Some(id))
                    .ok_or_else(unknown)?;
                let device = describe(&endpoint, default.as_deref()).ok_or_else(unknown)?;
                (endpoint, device)
            }
        };
        if start.is_over() {
            return Err(too_slow());
        }

        let (audio, rate) = initialize(&endpoint, options.sample_rate)?;
        let event = Event::new().map_err(|error| io_failure(&error, "opening the device"))?;
        audio.set_event(&event).map_err(failed)?;
        let buffer_frames = audio.buffer_frames().map_err(failed)?;
        let capture = audio.capture_client().map_err(failed)?;
        if start.is_over() {
            return Err(too_slow());
        }
        audio
            .start()
            .map_err(|code| failure(code, "starting the device"))?;
        Ok(Client {
            audio,
            capture,
            event,
            device,
            rate,
            buffer_frames,
        })
    }

    /// Hands audio to `on_audio` in pieces of `piece` frames until the capture stops, or
    /// returns why it failed.
    fn run(&self, stop: &AtomicBool, on_audio: &mut OnAudio, piece: usize) -> Result<(), Error> {
        let mut pieces = Pieces::new(piece);
        let mut audio_due = Instant::now() + STALL;
        let mut first = true;
        loop {
            let wait = audio_due
                .saturating_duration_since(Instant::now())
                .min(POLL);
            self.event
                .wait(wait)
                .map_err(|error| io_failure(&error, "waiting for audio"))?;
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            while self.capture.next_packet_frames().map_err(recording)? > 0 {
                let read = self.capture.read_packet(4, self.buffer_frames, |packet| {
                    take_packet(&packet, first, &mut pieces)
                });
                match read.map_err(recording)? {
                    Some(taken) => taken?,
                    None => break,
                }
                first = false;
                pieces.deliver(|piece| {
                    on_audio(piece, self.rate);
                    // A slow callback doesn't count against the device.
                    audio_due = Instant::now() + STALL;
                    !stop.load(Ordering::SeqCst)
                });
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            if Instant::now() >= audio_due {
                return Err(stalled());
            }
        }
    }
}

/// Adds a packet's samples to `pieces`. A gap before it ends the recording, except on the
/// first packet after starting, whose flag the engine leaves undefined.
fn take_packet(packet: &com::Packet<'_>, first: bool, pieces: &mut Pieces) -> Result<(), Error> {
    let Some(bytes) = packet.bytes else {
        return Err(Error::new(
            ErrorKind::Protocol,
            format!(
                "the audio engine described a packet of {} frames that its buffer can't hold",
                packet.frames
            ),
        ));
    };
    if packet.flags & com::DATA_DISCONTINUITY != 0 && !first {
        return Err(overrun());
    }
    if packet.flags & com::SILENT != 0 {
        pieces.extend(std::iter::repeat_n(0.0, bytes.len() / 4));
    } else {
        let (samples, _) = bytes.as_chunks::<4>();
        pieces.extend(samples.iter().map(|&sample| f32::from_le_bytes(sample)));
    }
    Ok(())
}

/// A client initialized for mono floats at `rate`, or at the engine's own rate when it can't
/// convert to `rate`, with the rate it records at.
fn initialize<'a>(endpoint: &Endpoint<'a>, rate: u32) -> Result<(AudioClient<'a>, u32), Error> {
    let failed = |code| failure(code, "opening the device");
    let audio = endpoint.activate_client().map_err(failed)?;
    let refused = match audio.initialize(
        &WaveFormatEx::mono_f32(rate),
        com::STREAM_FLAGS,
        ENGINE_BUFFER,
    ) {
        Ok(()) => return Ok((audio, rate)),
        Err(code) if is_format_refusal(code) => code,
        Err(code) => return Err(failed(code)),
    };
    let own = audio.mix_rate().map_err(failed)?;
    if own == rate || !(1..=384_000).contains(&own) {
        return Err(failed(refused));
    }
    // A client whose initialization failed is used no further.
    let audio = endpoint.activate_client().map_err(failed)?;
    audio
        .initialize(
            &WaveFormatEx::mono_f32(own),
            com::STREAM_FLAGS,
            ENGINE_BUFFER,
        )
        .map_err(failed)?;
    Ok((audio, own))
}

fn is_format_refusal(code: Hresult) -> bool {
    matches!(code, com::AUDCLNT_E_UNSUPPORTED_FORMAT | com::E_INVALIDARG)
}

/// A failed call, as an error.
fn failure(code: Hresult, doing: &str) -> Error {
    let (kind, reason) = match code {
        com::AUDCLNT_E_DEVICE_INVALIDATED
        | com::AUDCLNT_E_ENDPOINT_CREATE_FAILED
        | com::E_NOTFOUND => (ErrorKind::NoDevice, "the device is gone".to_owned()),
        com::AUDCLNT_E_DEVICE_IN_USE => (
            ErrorKind::Busy,
            "another program has the device to itself".to_owned(),
        ),
        com::AUDCLNT_E_SERVICE_NOT_RUNNING => (
            ErrorKind::NoServer,
            "the Windows Audio service isn't running".to_owned(),
        ),
        com::E_ACCESSDENIED => (
            ErrorKind::PermissionDenied,
            "the system refused access to the microphone".to_owned(),
        ),
        com::AUDCLNT_E_UNSUPPORTED_FORMAT => (
            ErrorKind::Unsupported,
            "the device can't record in this format".to_owned(),
        ),
        com::AUDCLNT_E_RESOURCES_INVALIDATED => (
            ErrorKind::Disconnected,
            "the audio engine ended the stream".to_owned(),
        ),
        com::E_OUTOFMEMORY => (ErrorKind::Io, "out of memory".to_owned()),
        Hresult(other) => (
            ErrorKind::Io,
            format!("error 0x{:08X}", other.cast_unsigned()),
        ),
    };
    Error::new(kind, format!("{doing} failed ({reason})"))
}

/// A failed call while recording. A device or service that went away disconnects the capture.
fn recording(code: Hresult) -> Error {
    let error = failure(code, "recording");
    match error.kind() {
        ErrorKind::NoDevice | ErrorKind::NoServer => {
            Error::new(ErrorKind::Disconnected, error.to_string())
        }
        _ => error,
    }
}

fn io_failure(error: &std::io::Error, doing: &str) -> Error {
    Error::new(ErrorKind::Io, format!("{doing} failed ({error})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(flags: u32, bytes: Option<&[u8]>) -> com::Packet<'_> {
        com::Packet {
            flags,
            frames: bytes.map_or(9, |bytes| (bytes.len() / 4) as u32),
            bytes,
        }
    }

    #[test]
    fn packets_become_samples() {
        let mut pieces = Pieces::new(2);
        let bytes: Vec<u8> = [0.5f32, -0.25]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        take_packet(&packet(0, Some(&bytes)), false, &mut pieces).unwrap();
        take_packet(&packet(com::SILENT, Some(&bytes)), false, &mut pieces).unwrap();
        let mut heard = Vec::new();
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            true
        });
        assert_eq!(heard, [[0.5, -0.25], [0.0, 0.0]]);
    }

    #[test]
    fn a_gap_ends_the_recording_except_at_the_start() {
        let mut pieces = Pieces::new(1);
        let bytes = 1f32.to_le_bytes();
        let gap = packet(com::DATA_DISCONTINUITY, Some(&bytes));
        take_packet(&gap, true, &mut pieces).unwrap();
        let error = take_packet(&gap, false, &mut pieces).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Overrun);
        let mut count = 0;
        pieces.deliver(|_| {
            count += 1;
            true
        });
        assert_eq!(count, 1, "only the first packet's sample");
    }

    #[test]
    fn a_packet_the_buffer_cant_hold_is_refused() {
        let mut pieces = Pieces::new(1);
        let error = take_packet(&packet(0, None), false, &mut pieces).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn failures_say_what_happened() {
        let error = failure(com::AUDCLNT_E_DEVICE_IN_USE, "opening the device");
        assert_eq!(error.kind(), ErrorKind::Busy);
        assert_eq!(
            error.to_string(),
            "opening the device failed (another program has the device to itself)"
        );
        let error = recording(com::AUDCLNT_E_DEVICE_INVALIDATED);
        assert_eq!(error.kind(), ErrorKind::Disconnected);
        assert_eq!(error.to_string(), "recording failed (the device is gone)");
        let error = failure(
            Hresult(0x8000_4005_u32.cast_signed()),
            "listing the devices",
        );
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error.to_string(),
            "listing the devices failed (error 0x80004005)"
        );
    }
}
