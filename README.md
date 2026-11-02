# Capta

Capta records the microphone in pure Rust. On Linux it speaks the PulseAudio native protocol over
the sound server's socket, which PulseAudio and PipeWire both serve, so building it needs no C
library and no system package such as `libasound2-dev`. The server mixes the device down to mono
and resamples it to the rate you ask for. On macOS and Windows Capta records through
[cpal](https://crates.io/crates/cpal).

## Install

```text
cargo add capta
```

On Linux it needs PulseAudio, or PipeWire with pipewire-pulse, which the desktops of Ubuntu,
Fedora and Debian run by default. A Flatpak needs `--socket=pulseaudio`.

## Record

```rust
let capture = capta::open(
    &capta::Options::default(),
    |samples, rate| {
        // Mono f32 samples at `rate` Hz, 20 ms at a time. Hand them on and return.
    },
    |error| eprintln!("The microphone stopped ({error})."),
)?;
// It records until `capture` is dropped.
```

The default options record the default device at 16 kHz. `capta::devices()` lists the input
devices, and `Options::device` picks one by its id. `open` blocks until the recording runs,
usually for tens to hundreds of milliseconds, and gives up after 3 s, so call it off a UI thread.

The example records into a WAV file.

```text
cargo run --example capta-record -- speech.wav 5
```

[How it works](docs/how-it-works.md) covers the protocol, threads, errors and the macOS and
Windows path.

## Licence

MIT or Apache-2.0, at your option.
