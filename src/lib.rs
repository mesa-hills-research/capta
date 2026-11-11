//! Microphone capture in pure Rust.
//!
//! Capta needs no C library and no system development package. On Linux it speaks the
//! PulseAudio native protocol over its Unix socket, which both PulseAudio and PipeWire (through
//! pipewire-pulse) serve. On Windows it records through WASAPI and on macOS through Core Audio,
//! calling the system's own libraries. The system mixes the device's channels down and
//! resamples to the rate asked for.
//!
//! ```no_run
//! let options = capta::Options { sample_rate: 16_000, ..capta::Options::default() };
//! let capture = capta::open(
//!     &options,
//!     |samples, rate| println!("{} samples at {rate} Hz", samples.len()),
//!     |error| eprintln!("The microphone stopped ({error})."),
//! )?;
//! std::thread::sleep(std::time::Duration::from_secs(5));
//! drop(capture);
//! # Ok::<(), capta::Error>(())
//! ```
//!
//! Each capture runs on a thread of its own, which calls `on_audio` with mono `f32` samples
//! and their rate. The callback should hand the samples on and return, since a callback
//! that blocks for seconds makes the system drop audio.

#![cfg_attr(target_os = "linux", forbid(unsafe_code))]

mod error;
#[cfg_attr(
    not(any(target_os = "linux", target_os = "windows", target_os = "macos")),
    allow(dead_code)
)]
mod startup;

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod native;

#[cfg(target_os = "linux")]
mod pulse;
#[cfg(target_os = "linux")]
use pulse as backend;

#[cfg(target_os = "windows")]
mod wasapi;
#[cfg(target_os = "windows")]
use wasapi as backend;

#[cfg(target_os = "macos")]
mod coreaudio;
#[cfg(target_os = "macos")]
use coreaudio as backend;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
use unsupported as backend;

use std::time::Duration;

pub use error::{Error, ErrorKind};

/// An input device the system offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// Names the device in [`Options::device`]. On Linux it's the sound server's source name,
    /// such as `alsa_input.pci-0000_00_1f.3.analog-stereo`. On Windows it's `wasapi:` and the
    /// endpoint's id, and on macOS `coreaudio:` and the device's UID.
    pub id: String,
    /// What to show a person, such as "Built-in Audio Analog Stereo".
    pub name: String,
    pub is_default: bool,
}

/// What to record, and how.
#[derive(Clone, Debug)]
pub struct Options {
    /// The device's [`Device::id`], or `None` for the system's default. On Linux a capture of
    /// the default follows it when the user picks another. On macOS and Windows it stays on
    /// the device that was the default when it started.
    pub device: Option<String>,
    /// The rate to deliver, in Hz. On Linux the sound server delivers exactly this. On macOS
    /// and Windows the device's own rate comes when the system can't convert to it, and
    /// `on_audio` says which.
    pub sample_rate: u32,
    /// How much audio each call of `on_audio` carries, from 1 ms to 1 s. On Linux it's about
    /// this much, as the sound server packs it, and on macOS and Windows exactly this much.
    pub piece: Duration,
    /// The name the sound server shows for the recording program, on Linux.
    pub app_name: String,
}

impl Default for Options {
    /// The default device at 16 kHz, in pieces of 20 ms, named after the program.
    fn default() -> Self {
        Options {
            device: None,
            sample_rate: 16_000,
            piece: Duration::from_millis(20),
            app_name: program_name(),
        }
    }
}

/// The running program's file name without its extension.
pub(crate) fn program_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| Some(path.file_stem()?.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "capta".into())
}

/// What takes the audio, mono samples and their rate.
pub(crate) type OnAudio = Box<dyn FnMut(&[f32], u32) + Send>;
/// What hears that a running capture stopped, and why.
pub(crate) type OnError = Box<dyn FnOnce(Error) + Send>;

/// The input devices, without the monitors of outputs. Blocks for a few round trips to the
/// sound server and gives up after 3 s, so call it off a UI thread.
pub fn devices() -> Result<Vec<Device>, Error> {
    backend::devices()
}

/// Starts recording.
///
/// It blocks until the recording runs or fails. That usually takes tens to hundreds of
/// milliseconds, and it gives up 3 s after it starts. So call it off a UI thread and off an
/// async runtime's workers.
///
/// `on_audio` and `on_error` run on a thread of the capture's own. `on_error` runs at most
/// once, when a running capture stops, and `on_audio` never runs after it. A capture fails as
/// `Stalled` when no audio comes for 5 s, on Linux while the stream isn't suspended.
pub fn open(
    options: &Options,
    on_audio: impl FnMut(&[f32], u32) + Send + 'static,
    on_error: impl FnOnce(Error) + Send + 'static,
) -> Result<Capture, Error> {
    if !(1..=384_000).contains(&options.sample_rate) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("a rate of {} Hz isn't supported", options.sample_rate),
        ));
    }
    if !(Duration::from_millis(1)..=Duration::from_secs(1)).contains(&options.piece) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("pieces of {:?} aren't supported", options.piece),
        ));
    }
    let opened = backend::open(options, Box::new(on_audio), Box::new(on_error))?;
    Ok(Capture {
        device: opened.device,
        sample_rate: opened.sample_rate,
        _stream: opened.stream,
    })
}

/// A recording, until dropped.
///
/// Dropping it stops the recording and waits for a callback in progress, `on_audio` or
/// `on_error`, to return, so none runs after the drop. So neither callback may wait for the
/// thread that drops the capture. Dropping a capture from inside one of its own callbacks
/// returns at once.
pub struct Capture {
    device: Device,
    sample_rate: u32,
    _stream: backend::Stream,
}

impl Capture {
    /// The device the recording started on.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// The rate `on_audio` gets.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("device", &self.device)
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

/// What a backend hands back from `open`.
pub(crate) struct Opened {
    pub stream: backend::Stream,
    pub device: Device,
    pub sample_rate: u32,
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_capture_can_move_between_threads() {
        fn send_and_sync<T: Send + Sync>() {}
        send_and_sync::<super::Capture>();
        send_and_sync::<super::Device>();
        send_and_sync::<super::Error>();
    }
}
