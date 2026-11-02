//! Records the microphone into a WAV file, or lists the input devices.
//!
//! ```text
//! cargo run --example capta-record -- out.wav [SECONDS] [RATE] [DEVICE]
//! cargo run --example capta-record -- --list
//! ```

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--list") {
        let devices =
            capta::devices().map_err(|error| format!("Couldn't list the devices ({error})."))?;
        for device in devices {
            let mark = if device.is_default { "*" } else { " " };
            println!("{mark} {}  {}", device.id, device.name);
        }
        return Ok(());
    }
    let [path, rest @ ..] = &args[..] else {
        return Err(
            "Usage: capta-record OUT.wav [SECONDS] [RATE] [DEVICE], or capta-record --list".into(),
        );
    };
    let seconds: f64 = parse(rest.first(), 5.0, "length in seconds")?;
    let length = Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|length| Instant::now().checked_add(*length).is_some())
        .ok_or_else(|| {
            format!("The length in seconds is out of range ({seconds}). Give 0 or more.")
        })?;
    let rate: u32 = parse(rest.get(1), 16_000, "rate in Hz")?;
    let options = capta::Options {
        device: rest.get(2).cloned(),
        sample_rate: rate,
        app_name: "capta-record".into(),
        ..capta::Options::default()
    };

    let (audio, received) = mpsc::channel::<Vec<f32>>();
    let (failure, failed) = mpsc::channel();
    let opening = Instant::now();
    let capture = capta::open(
        &options,
        move |samples, _| {
            let _ = audio.send(samples.to_vec());
        },
        move |error| {
            let _ = failure.send(error);
        },
    )
    .map_err(|error| format!("Couldn't open the microphone ({error})."))?;
    let rate = capture.sample_rate();
    eprintln!(
        "Recording {} at {rate} Hz, opened in {} ms.",
        capture.device().name,
        opening.elapsed().as_millis()
    );

    let mut samples = Vec::new();
    let end = Instant::now()
        .checked_add(length)
        .ok_or("The length in seconds is too long.")?;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if let Ok(error) = failed.try_recv() {
            eprintln!("The microphone stopped ({error}).");
            break;
        }
        if let Ok(piece) = received.recv_timeout(left.min(Duration::from_millis(100))) {
            if samples.is_empty() {
                eprintln!("First audio after {} ms.", opening.elapsed().as_millis());
            }
            samples.extend(piece);
        }
    }
    drop(capture);
    samples.extend(received.try_iter().flatten());

    write_wav(path, rate, &samples).map_err(|error| format!("Couldn't write {path} ({error})."))?;
    let peak = samples
        .iter()
        .fold(0f32, |peak, sample| peak.max(sample.abs()));
    eprintln!(
        "Wrote {:.2} s to {path}, peak {:.3}.",
        samples.len() as f64 / f64::from(rate),
        peak
    );
    Ok(())
}

fn parse<T: std::str::FromStr>(arg: Option<&String>, default: T, what: &str) -> Result<T, String> {
    arg.map_or(Ok(default), |text| {
        text.parse()
            .map_err(|_| format!("The {what} isn't a number ({text})."))
    })
}

/// Writes 16-bit mono PCM.
fn write_wav(path: &str, rate: u32, samples: &[f32]) -> std::io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    let data = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    out.write_all(b"RIFF")?;
    out.write_all(&(36 + data).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&1u16.to_le_bytes())?; // mono
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * 2).to_le_bytes())?;
    out.write_all(&2u16.to_le_bytes())?; // bytes per frame
    out.write_all(&16u16.to_le_bytes())?; // bits per sample
    out.write_all(b"data")?;
    out.write_all(&data.to_le_bytes())?;
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.write_all(&value.to_le_bytes())?;
    }
    out.flush()
}
