//! What the Windows and macOS backends share, the deadlines, the errors of a late startup and
//! the gathering of samples into pieces.

use std::time::Duration;

use crate::startup::Late;
use crate::{Error, ErrorKind};

/// How long listing or opening may take, from the start.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(3);

/// How long a running capture may go without audio before it fails as stalled.
pub(crate) const STALL: Duration = Duration::from_secs(5);

/// Why a startup gave no result, as an error.
pub(crate) fn late_error(late: Late, doing: &str) -> Error {
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

/// The error of a startup whose deadline passed between two phases.
pub(crate) fn too_slow() -> Error {
    Error::new(ErrorKind::Stalled, "opening the device took too long")
}

/// The error of a capture that got no audio for [`STALL`].
pub(crate) fn stalled() -> Error {
    Error::new(
        ErrorKind::Stalled,
        format!("the device sent no audio for {:.1} s", STALL.as_secs_f32()),
    )
}

/// The error of a capture that lost audio.
pub(crate) fn overrun() -> Error {
    Error::new(
        ErrorKind::Overrun,
        "audio was lost, so the recording has a gap",
    )
}

/// The device a public id names, given its system's prefix, such as `wasapi` in
/// `wasapi:{0.0.1.00000000}.{…}`. The prefix matches in any case, as it did in Capta 0.1.
pub(crate) fn parse_id<'a>(id: &'a str, prefix: &str) -> Option<&'a str> {
    let (host, device) = id.split_once(':')?;
    (host.eq_ignore_ascii_case(prefix) && !device.is_empty()).then_some(device)
}

/// How many frames `time` holds at `rate`, at least one.
pub(crate) fn frames_in(rate: u32, time: Duration) -> usize {
    let frames = (u128::from(rate) * time.as_nanos() / 1_000_000_000).max(1);
    usize::try_from(frames).unwrap_or(usize::MAX)
}

/// Notices audio lost between buffers from the sample times they start at.
#[cfg(any(test, target_os = "macos"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeline {
    next: Option<f64>,
    tolerance: f64,
}

#[cfg(any(test, target_os = "macos"))]
impl Timeline {
    /// A timeline that lets the start of a buffer drift from where the last one ended by up to
    /// `tolerance` frames, and expects the next buffer at `next`, if anywhere.
    pub(crate) fn resume(next: Option<f64>, tolerance: f64) -> Timeline {
        Timeline { next, tolerance }
    }

    /// Where the next buffer should start.
    pub(crate) fn next(&self) -> Option<f64> {
        self.next
    }

    /// Takes the sample time a buffer of `frames` starts at. Returns false when frames went
    /// missing between the last buffer and this one. A timeline that goes back starts over.
    pub(crate) fn follows(&mut self, start: f64, frames: usize) -> bool {
        let follows = self.next.is_none_or(|next| start <= next + self.tolerance);
        self.next = Some(start + frames as f64);
        follows
    }
}

/// A queue of samples from one thread to another that takes no lock and, once made, allocates
/// nothing. One thread pushes and one thread pops.
#[cfg(any(test, target_os = "macos"))]
pub(crate) struct Ring {
    slots: Box<[std::sync::atomic::AtomicU32]>,
    /// How many samples were ever pushed, and popped.
    pushed: std::sync::atomic::AtomicUsize,
    popped: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, target_os = "macos"))]
impl Ring {
    pub(crate) fn new(capacity: usize) -> Ring {
        use std::sync::atomic::{AtomicU32, AtomicUsize};
        Ring {
            slots: (0..capacity.max(1)).map(|_| AtomicU32::new(0)).collect(),
            pushed: AtomicUsize::new(0),
            popped: AtomicUsize::new(0),
        }
    }

    /// Adds all of `samples`, or none of them when they don't fit. Returns whether they did.
    pub(crate) fn push(&self, samples: impl ExactSizeIterator<Item = f32>) -> bool {
        use std::sync::atomic::Ordering;
        let pushed = self.pushed.load(Ordering::Relaxed);
        let popped = self.popped.load(Ordering::Acquire);
        let free = self.slots.len().saturating_sub(pushed.wrapping_sub(popped));
        let count = samples.len();
        if count > free {
            return false;
        }
        for (offset, sample) in samples.enumerate() {
            let slot = pushed.wrapping_add(offset) % self.slots.len();
            self.slots[slot].store(sample.to_bits(), Ordering::Relaxed);
        }
        self.pushed
            .store(pushed.wrapping_add(count), Ordering::Release);
        true
    }

    /// Moves every sample pushed so far to `out`.
    pub(crate) fn pop_into(&self, out: &mut Vec<f32>) {
        use std::sync::atomic::Ordering;
        let popped = self.popped.load(Ordering::Relaxed);
        let pushed = self.pushed.load(Ordering::Acquire);
        let count = pushed.wrapping_sub(popped);
        out.extend((0..count).map(|offset| {
            let slot = popped.wrapping_add(offset) % self.slots.len();
            f32::from_bits(self.slots[slot].load(Ordering::Relaxed))
        }));
        self.popped.store(pushed, Ordering::Release);
    }
}

/// Gathers samples into pieces of one length, for `on_audio`.
pub(crate) struct Pieces {
    length: usize,
    samples: Vec<f32>,
}

impl Pieces {
    pub(crate) fn new(length: usize) -> Pieces {
        let length = length.max(1);
        Pieces {
            length,
            samples: Vec::with_capacity(length.saturating_mul(2)),
        }
    }

    pub(crate) fn extend(&mut self, samples: impl IntoIterator<Item = f32>) {
        self.samples.extend(samples);
    }

    /// Hands each whole piece to `deliver` while it returns true, and keeps the rest.
    pub(crate) fn deliver(&mut self, mut deliver: impl FnMut(&[f32]) -> bool) {
        let mut used = 0;
        for piece in self.samples.chunks_exact(self.length) {
            used += self.length;
            if !deliver(piece) {
                break;
            }
        }
        self.samples.drain(..used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_have_one_length_and_keep_the_rest() {
        let mut pieces = Pieces::new(3);
        let mut heard = Vec::new();
        pieces.extend([1.0, 2.0]);
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            true
        });
        assert!(heard.is_empty());
        pieces.extend([3.0, 4.0, 5.0, 6.0, 7.0]);
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            true
        });
        assert_eq!(heard, [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        pieces.extend([8.0, 9.0]);
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            true
        });
        assert_eq!(heard[2], [7.0, 8.0, 9.0]);
    }

    #[test]
    fn delivery_stops_when_asked_and_resumes_later() {
        let mut pieces = Pieces::new(2);
        pieces.extend([1.0, 2.0, 3.0, 4.0, 5.0]);
        let mut heard = Vec::new();
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            false
        });
        assert_eq!(heard, [[1.0, 2.0]]);
        pieces.deliver(|piece| {
            heard.push(piece.to_vec());
            true
        });
        assert_eq!(heard, [[1.0, 2.0], [3.0, 4.0]]);
    }

    #[test]
    fn a_piece_holds_at_least_one_frame() {
        assert_eq!(frames_in(16_000, Duration::from_millis(20)), 320);
        assert_eq!(frames_in(48_000, Duration::from_millis(20)), 960);
        assert_eq!(frames_in(10, Duration::from_millis(1)), 1);
        let mut pieces = Pieces::new(0);
        pieces.extend([1.0]);
        let mut count = 0;
        pieces.deliver(|_| {
            count += 1;
            true
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn a_timeline_notices_missing_frames() {
        let mut timeline = Timeline::resume(None, 2.0);
        assert!(timeline.follows(1_000.0, 320));
        assert!(timeline.follows(1_320.0, 320));
        // A little drift, as from resampling.
        assert!(timeline.follows(1_641.5, 320));
        assert!(!timeline.follows(2_600.0, 320), "a gap of 640 frames");
        assert!(timeline.follows(2_920.0, 320));
        assert!(timeline.follows(10.0, 320), "a timeline that starts over");
        assert_eq!(timeline.next(), Some(330.0));
        let mut resumed = Timeline::resume(timeline.next(), 2.0);
        assert!(!resumed.follows(400.0, 320));
    }

    #[test]
    fn a_ring_keeps_order_and_refuses_what_doesnt_fit() {
        let ring = Ring::new(4);
        let mut out = Vec::new();
        assert!(ring.push([1.0, 2.0, 3.0].into_iter()));
        assert!(
            !ring.push([4.0, 5.0].into_iter()),
            "five samples in four slots"
        );
        ring.pop_into(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        // Around the end of the slots.
        assert!(ring.push([4.0, 5.0, 6.0, 7.0].into_iter()));
        ring.pop_into(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        ring.pop_into(&mut out);
        assert_eq!(out.len(), 7);
    }

    #[test]
    fn a_ring_carries_samples_between_threads() {
        let ring = std::sync::Arc::new(Ring::new(64));
        let pushing = std::sync::Arc::clone(&ring);
        let pusher = std::thread::spawn(move || {
            let mut next = 0u32;
            while next < 10_000 {
                if pushing.push((next..next + 7).map(|n| n as f32)) {
                    next += 7;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let mut out = Vec::new();
        while out.len() < 10_003 {
            ring.pop_into(&mut out);
            std::thread::yield_now();
        }
        pusher.join().unwrap();
        assert!(
            out.iter()
                .enumerate()
                .all(|(n, &sample)| sample == n as f32)
        );
    }

    #[test]
    fn ids_name_their_system() {
        assert_eq!(
            parse_id("wasapi:{0.0.1}.{a}", "wasapi"),
            Some("{0.0.1}.{a}")
        );
        assert_eq!(parse_id("WASAPI:x", "wasapi"), Some("x"));
        assert_eq!(
            parse_id("coreaudio:Fixture:UID", "coreaudio"),
            Some("Fixture:UID")
        );
        assert_eq!(parse_id("coreaudio:x", "wasapi"), None);
        assert_eq!(parse_id("wasapi:", "wasapi"), None);
        assert_eq!(parse_id("no prefix", "wasapi"), None);
    }
}
