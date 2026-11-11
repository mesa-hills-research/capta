//! Recording from the audio system this machine runs, PulseAudio or PipeWire with
//! pipewire-pulse on Linux, WASAPI on Windows and Core Audio on macOS.
//!
//! `cargo test -- --ignored` records from the default microphone. When `CAPTA_TEST_TONE` holds
//! the frequency of a tone that plays into the default input, as through a loopback device, the
//! recordings must carry it.

#![cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use capta::{Capture, ErrorKind, Options};

/// What a device id starts with on this system.
const PREFIX: &str = if cfg!(target_os = "windows") {
    "wasapi:"
} else if cfg!(target_os = "macos") {
    "coreaudio:"
} else {
    ""
};

/// Whether each piece holds exactly `Options::piece`, as on macOS and Windows.
const EXACT_PIECES: bool = !cfg!(target_os = "linux");

#[test]
fn listing_and_opening_agree() {
    let devices = match capta::devices() {
        Ok(devices) => devices,
        Err(error) => {
            // No sound server, as on a build machine. Opening fails as well.
            let failed = capta::open(&Options::default(), |_, _| {}, |_| {}).unwrap_err();
            assert_eq!(failed.kind(), error.kind(), "{failed}");
            return;
        }
    };
    eprintln!("{devices:#?}");
    assert!(devices.iter().filter(|device| device.is_default).count() <= 1);
    for device in &devices {
        assert!(device.id.starts_with(PREFIX), "{device:?}");
        assert!(device.id.len() > PREFIX.len(), "{device:?}");
    }
    if !devices.iter().any(|device| device.is_default) {
        let error = capta::open(&Options::default(), |_, _| {}, |_| {}).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NoDevice, "{error}");
    }
    let unknown = Options {
        device: Some(format!("{PREFIX}fixture-no-such-device")),
        ..Options::default()
    };
    let error = capta::open(&unknown, |_, _| {}, |_| {}).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NoDevice, "{error}");
}

/// What a capture heard, piece by piece.
struct Heard {
    pieces: mpsc::Receiver<Vec<f32>>,
    errors: mpsc::Receiver<capta::Error>,
}

fn record(options: &Options) -> (Capture, Heard) {
    let (audio, pieces) = mpsc::channel();
    let (failure, errors) = mpsc::channel();
    let capture = capta::open(
        options,
        move |samples, _| {
            let _ = audio.send(samples.to_vec());
        },
        move |error| {
            let _ = failure.send(error);
        },
    )
    .unwrap();
    (capture, Heard { pieces, errors })
}

/// Records for `length` from the first audio. Returns the samples and how long they took to
/// come.
fn listen(heard: &Heard, length: Duration, piece: usize) -> (Vec<f32>, Duration) {
    let first = heard.pieces.recv_timeout(Duration::from_secs(3)).unwrap();
    let began = Instant::now();
    let mut samples = first;
    while began.elapsed() < length {
        if let Ok(error) = heard.errors.try_recv() {
            panic!("the capture stopped ({error})");
        }
        let piece_samples = heard.pieces.recv_timeout(Duration::from_secs(3)).unwrap();
        if EXACT_PIECES {
            assert_eq!(piece_samples.len(), piece);
        }
        samples.extend(piece_samples);
    }
    (samples, began.elapsed())
}

/// Checks that `samples` carry the tone in `CAPTA_TEST_TONE`, when it's set.
fn check_tone(samples: &[f32], rate: u32) {
    let Some(tone) = std::env::var("CAPTA_TEST_TONE")
        .ok()
        .and_then(|tone| tone.parse::<f64>().ok())
    else {
        return;
    };
    // The last second, away from the start.
    let last = &samples[samples.len().saturating_sub(rate as usize)..];
    let peak = last
        .iter()
        .fold(0f32, |peak, sample| peak.max(sample.abs()));
    let crossings = last
        .windows(2)
        .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
        .count();
    let heard = crossings as f64 / 2.0 / (last.len() as f64 / f64::from(rate));
    eprintln!("tone of {heard:.1} Hz, peak {peak:.3}");
    assert!(peak > 0.05 && peak <= 1.0, "peak {peak}");
    assert!(
        (heard - tone).abs() < tone * 0.05,
        "{heard} Hz, not {tone} Hz"
    );
}

fn records_at(rate: u32) {
    let options = Options {
        sample_rate: rate,
        app_name: "capta-test".into(),
        ..Options::default()
    };
    let opening = Instant::now();
    let (capture, heard) = record(&options);
    eprintln!(
        "recording {:?} at {} Hz, opened in {} ms",
        capture.device(),
        capture.sample_rate(),
        opening.elapsed().as_millis()
    );
    assert!(capture.device().is_default);
    assert_eq!(capture.sample_rate(), rate);
    let piece = rate as usize / 50;
    let (samples, took) = listen(&heard, Duration::from_secs(3), piece);
    let delivered = samples.len() as f64 / took.as_secs_f64();
    eprintln!("{delivered:.0} samples a second");
    assert!(
        (delivered - f64::from(rate)).abs() < f64::from(rate) * 0.1,
        "{delivered} samples a second at {rate} Hz"
    );
    assert!(samples.iter().all(|sample| sample.is_finite()));
    check_tone(&samples, rate);
    drop(capture);
    assert!(heard.errors.try_recv().is_err());
}

#[test]
#[ignore = "needs a running sound server and a microphone"]
fn records_from_the_default_device() {
    let devices = capta::devices().unwrap();
    assert!(
        devices.iter().any(|device| device.is_default),
        "{devices:?}"
    );
    records_at(16_000);
}

#[test]
#[ignore = "needs a running sound server and a microphone"]
fn records_at_the_rate_asked_for() {
    records_at(48_000);
    records_at(22_050);
}

#[test]
#[ignore = "needs a running sound server and a microphone"]
fn a_capture_dropped_in_its_own_callback_ends() {
    let slot: Arc<Mutex<Option<Capture>>> = Arc::new(Mutex::new(None));
    let calls = Arc::new(AtomicUsize::new(0));
    let (dropped, dropped_in) = mpsc::channel();
    let capture = {
        let slot = Arc::clone(&slot);
        let calls = Arc::clone(&calls);
        capta::open(
            &Options::default(),
            move |_, _| {
                if calls.fetch_add(1, Ordering::SeqCst) == 5 {
                    let began = Instant::now();
                    drop(slot.lock().unwrap().take());
                    let _ = dropped.send(began.elapsed());
                }
            },
            |error| panic!("the capture stopped ({error})"),
        )
        .unwrap()
    };
    *slot.lock().unwrap() = Some(capture);
    let took = dropped_in.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(took < Duration::from_millis(500), "the drop took {took:?}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(calls.load(Ordering::SeqCst), 6, "audio after the drop");
}

#[test]
#[ignore = "needs a running sound server and a microphone"]
fn dropping_a_capture_stops_it_at_once() {
    let (capture, heard) = record(&Options::default());
    heard.pieces.recv_timeout(Duration::from_secs(3)).unwrap();
    let began = Instant::now();
    drop(capture);
    let took = began.elapsed();
    eprintln!("dropped in {} ms", took.as_millis());
    assert!(took < Duration::from_millis(500), "the drop took {took:?}");
    while heard.pieces.try_recv().is_ok() {}
    std::thread::sleep(Duration::from_millis(200));
    assert!(heard.pieces.try_recv().is_err(), "audio after the drop");
}

#[cfg(not(target_os = "linux"))]
#[test]
#[ignore = "needs a microphone"]
fn a_capture_that_falls_behind_ends_as_overrun() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (failure, failed) = mpsc::channel();
    let capture = {
        let calls = Arc::clone(&calls);
        capta::open(
            &Options::default(),
            move |_, _| {
                // Longer than the 2 s the system holds.
                if calls.fetch_add(1, Ordering::SeqCst) == 10 {
                    std::thread::sleep(Duration::from_secs(4));
                }
            },
            move |error| {
                let _ = failure.send(error);
            },
        )
        .unwrap()
    };
    let error = failed.recv_timeout(Duration::from_secs(12)).unwrap();
    eprintln!("ended with {:?} ({error})", error.kind());
    assert_eq!(error.kind(), ErrorKind::Overrun, "{error}");
    let calls_at_error = calls.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_at_error,
        "audio after the error"
    );
    drop(capture);
}
