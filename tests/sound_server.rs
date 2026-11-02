//! Recording from the sound server this machine runs, PulseAudio or PipeWire with
//! pipewire-pulse. Run it with `cargo test -- --ignored` on a Linux desktop.

#![cfg(target_os = "linux")]

use std::sync::mpsc;
use std::time::Duration;

#[test]
#[ignore = "needs a running sound server and a microphone"]
fn records_from_the_running_sound_server() {
    let devices = capta::devices().unwrap();
    assert!(
        devices.iter().any(|device| device.is_default),
        "{devices:?}"
    );

    let (audio, heard) = mpsc::channel();
    let (errors, failed) = mpsc::channel();
    let capture = capta::open(
        &capta::Options::default(),
        move |samples, rate| {
            let _ = audio.send((samples.len(), rate));
        },
        move |error| {
            let _ = errors.send(error);
        },
    )
    .unwrap();
    assert_eq!(capture.sample_rate(), 16_000);
    let mut samples = 0;
    while samples < 8_000 {
        let (count, rate) = heard.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(rate, 16_000);
        samples += count;
    }
    drop(capture);
    assert!(failed.try_recv().is_err());
}
