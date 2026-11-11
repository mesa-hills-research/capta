//! What the Windows and macOS backends share, the deadlines and errors of startups, the limits
//! on what the system reports, and the gathering of samples into pieces.

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

/// Whether a public id names the device whose own id is `device`. The prefix matches in any
/// case and the device's id exactly.
pub(crate) fn names_device(id: &str, prefix: &str, device: &str) -> bool {
    parse_id(id, prefix) == Some(device)
}

/// How many frames `time` holds at `rate`, at least one.
pub(crate) fn frames_in(rate: u32, time: Duration) -> usize {
    let frames = (u128::from(rate) * time.as_nanos() / 1_000_000_000).max(1);
    usize::try_from(frames).unwrap_or(usize::MAX)
}

/// The most devices a list from the system may hold. Real lists hold a few.
pub(crate) const MAX_DEVICES: usize = 4096;

/// The highest rate Capta records at, in Hz.
pub(crate) const MAX_RATE: u32 = 384_000;

/// A count the system reported, when it's at most `max`.
#[cfg(any(test, target_os = "windows"))]
pub(crate) fn bounded(count: u32, max: usize) -> Option<usize> {
    usize::try_from(count).ok().filter(|&count| count <= max)
}

/// The byte length of `frames` frames of `frame_bytes` each, when it's at most `limit` and
/// `isize::MAX`, so the bytes can be a slice.
#[cfg(any(test, target_os = "windows"))]
pub(crate) fn byte_length(frames: u32, frame_bytes: usize, limit: usize) -> Option<usize> {
    usize::try_from(frames)
        .ok()?
        .checked_mul(frame_bytes)
        .filter(|&length| length <= limit && length <= isize::MAX.unsigned_abs())
}

/// Calls `visit` with each index below `count` in turn, until it returns false. Before each,
/// it checks whether the startup is over, and stops with an error once it is.
pub(crate) fn visit(
    count: usize,
    is_over: &dyn Fn() -> bool,
    mut visit: impl FnMut(usize) -> Result<bool, Error>,
) -> Result<(), Error> {
    for index in 0..count {
        if is_over() {
            return Err(too_slow());
        }
        if !visit(index)? {
            break;
        }
    }
    Ok(())
}

/// Why opening at a rate gave nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attempt<E> {
    /// The startup was over before a phase began.
    Late,
    Failed(E),
}

/// Opens with `attempt` at `rate`, and when the system refuses the rate, at the device's own
/// rate from `own_rate`. The startup must still be on before the first attempt, before asking
/// for the device's rate and before the second attempt. Returns what opened and its rate.
pub(crate) fn open_at_rate<T, E>(
    rate: u32,
    is_over: &dyn Fn() -> bool,
    mut attempt: impl FnMut(u32) -> Result<T, Attempt<E>>,
    refuses_rate: impl Fn(&E) -> bool,
    own_rate: impl FnOnce() -> Result<u32, E>,
) -> Result<(T, u32), Attempt<E>> {
    if is_over() {
        return Err(Attempt::Late);
    }
    let refusal = match attempt(rate) {
        Ok(opened) => return Ok((opened, rate)),
        Err(Attempt::Failed(error)) if refuses_rate(&error) => error,
        Err(other) => return Err(other),
    };
    if is_over() {
        return Err(Attempt::Late);
    }
    let own = own_rate().map_err(Attempt::Failed)?;
    if own == rate || !(1..=MAX_RATE).contains(&own) {
        return Err(Attempt::Failed(refusal));
    }
    if is_over() {
        return Err(Attempt::Late);
    }
    attempt(own).map(|opened| (opened, own))
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
    fn counts_from_the_system_have_a_limit() {
        assert_eq!(bounded(0, MAX_DEVICES), Some(0));
        assert_eq!(bounded(4096, MAX_DEVICES), Some(4096));
        assert_eq!(bounded(4097, MAX_DEVICES), None);
        assert_eq!(bounded(u32::MAX, MAX_DEVICES), None);
    }

    #[test]
    fn packet_lengths_stay_within_their_limit_and_fit_a_slice() {
        assert_eq!(byte_length(320, 4, 1 << 20), Some(1280));
        assert_eq!(byte_length(0, 4, 1 << 20), Some(0));
        assert_eq!(byte_length(262_144, 4, 1 << 20), Some(1 << 20));
        assert_eq!(byte_length(262_145, 4, 1 << 20), None);
        assert_eq!(byte_length(u32::MAX, 4, usize::MAX), {
            // On 64-bit systems the length fits a slice, on 32-bit ones it overflows.
            (u32::MAX as u64 * 4 <= isize::MAX as u64).then(|| u32::MAX as usize * 4)
        });
        // The most a slice may hold on a 32-bit system, i32::MAX bytes.
        let limit = i32::MAX as usize;
        assert_eq!(byte_length(0x1FFF_FFFF, 4, limit), Some(0x7FFF_FFFC));
        assert_eq!(byte_length(0x2000_0000, 4, limit), None);
    }

    #[test]
    fn a_visit_stops_once_the_startup_is_over() {
        let over = std::cell::Cell::new(false);
        let mut visited = Vec::new();
        let result = visit(3, &|| over.get(), |index| {
            visited.push(index);
            // The first device took until after the deadline.
            over.set(true);
            Ok(true)
        });
        assert_eq!(visited, [0]);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Stalled);

        let mut visited = Vec::new();
        visit(5, &|| false, |index| {
            visited.push(index);
            Ok(index < 1)
        })
        .unwrap();
        assert_eq!(visited, [0, 1], "a visit that found what it wanted stops");
    }

    type Opening = Result<(u32, u32), Attempt<&'static str>>;

    /// An opening that records its calls, refuses the rates in `refused` and reports
    /// `own_rate` as the device's rate.
    fn open_with(
        rate: u32,
        refused: &[u32],
        own_rate: Result<u32, &'static str>,
        over_after: usize,
    ) -> (Opening, Vec<String>) {
        let calls = std::cell::RefCell::new(Vec::new());
        let checks = std::cell::Cell::new(0);
        let is_over = || {
            checks.set(checks.get() + 1);
            checks.get() > over_after
        };
        let result = open_at_rate(
            rate,
            &is_over,
            |rate| {
                calls.borrow_mut().push(format!("open at {rate}"));
                if refused.contains(&rate) {
                    Err(Attempt::Failed("refused"))
                } else {
                    Ok(rate)
                }
            },
            |error| *error == "refused",
            || {
                calls.borrow_mut().push("ask for the rate".into());
                own_rate
            },
        );
        (result, calls.into_inner())
    }

    #[test]
    fn a_refused_rate_falls_back_to_the_devices_own() {
        let (result, calls) = open_with(16_000, &[16_000], Ok(48_000), usize::MAX);
        assert_eq!(result, Ok((48_000, 48_000)));
        assert_eq!(
            calls,
            ["open at 16000", "ask for the rate", "open at 48000"]
        );
        let (result, _) = open_with(16_000, &[], Ok(48_000), usize::MAX);
        assert_eq!(result, Ok((16_000, 16_000)));
    }

    #[test]
    fn a_refusal_that_comes_late_starts_no_fallback() {
        let (result, calls) = open_with(16_000, &[16_000], Ok(48_000), 1);
        assert_eq!(result, Err(Attempt::Late));
        assert_eq!(calls, ["open at 16000"]);
        let (result, calls) = open_with(16_000, &[16_000], Ok(48_000), 2);
        assert_eq!(result, Err(Attempt::Late));
        assert_eq!(calls, ["open at 16000", "ask for the rate"]);
        let (result, calls) = open_with(16_000, &[16_000], Ok(48_000), 0);
        assert_eq!(result, Err(Attempt::Late));
        assert!(calls.is_empty());
    }

    #[test]
    fn a_fallback_that_cant_help_keeps_the_first_refusal() {
        for own in [Ok(16_000), Ok(0), Ok(MAX_RATE + 1)] {
            let (result, calls) = open_with(16_000, &[16_000], own, usize::MAX);
            assert_eq!(result, Err(Attempt::Failed("refused")));
            assert_eq!(calls, ["open at 16000", "ask for the rate"]);
        }
        let (result, _) = open_with(16_000, &[16_000], Err("no rate"), usize::MAX);
        assert_eq!(result, Err(Attempt::Failed("no rate")));
        let (result, _) = open_with(16_000, &[16_000, 48_000], Ok(48_000), usize::MAX);
        assert_eq!(result, Err(Attempt::Failed("refused")));
    }

    #[test]
    fn a_device_is_named_by_its_own_id_in_any_prefix_case() {
        assert!(names_device(
            "coreaudio:Fixture-UID",
            "coreaudio",
            "Fixture-UID"
        ));
        assert!(names_device(
            "COREAUDIO:Fixture-UID",
            "coreaudio",
            "Fixture-UID"
        ));
        assert!(names_device(
            "CoreAudio:Fixture-UID",
            "coreaudio",
            "Fixture-UID"
        ));
        assert!(!names_device(
            "coreaudio:fixture-uid",
            "coreaudio",
            "Fixture-UID"
        ));
        assert!(!names_device(
            "wasapi:Fixture-UID",
            "coreaudio",
            "Fixture-UID"
        ));
        assert!(!names_device("Fixture-UID", "coreaudio", "Fixture-UID"));
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
