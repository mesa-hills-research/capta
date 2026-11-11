# How Capta works

## Linux

Capta is a client of the PulseAudio native protocol, for recording only. PulseAudio serves the
protocol, and so does PipeWire through pipewire-pulse, which the major distributions' desktops
run by default.

### Finding the server

Capta connects to the first socket that answers, in libpulse's order.

1. Each `unix:` path or absolute path in `$PULSE_SERVER`. When it's set, nothing else is tried,
   and network addresses in it are reported as unsupported.
2. `$PULSE_RUNTIME_PATH/native`
3. `$XDG_RUNTIME_DIR/pulse/native`
4. `/run/user/<uid>/pulse/native`
5. `/run/pulse/native`

The cookie comes from the first regular file of `$PULSE_COOKIE`, `$XDG_CONFIG_HOME/pulse/cookie`
(or `~/.config/pulse/cookie`) and `~/.pulse-cookie` that holds 256 bytes, else it's 256 zero
bytes. Capta reads only those 256 bytes, and passes over pipes and devices. pipewire-pulse
checks only the cookie's length, and PulseAudio lets in a client of its own user by the
socket's credentials. Capta never writes a cookie.

When no socket answers but PipeWire's own socket exists, the error says to install
pipewire-pulse. Taking the device from under PipeWire would break every other program's audio.

### The protocol

Capta speaks protocol version 35, PulseAudio 15's, and accepts servers from version 22. It
offers neither shared memory nor memfd, so audio comes inline over the socket.

| Request | When |
|---|---|
| `AUTH`, `SET_CLIENT_NAME` | Every connection, with `application.name` from `Options::app_name` |
| `GET_SERVER_INFO` | Every connection, for the default source's name |
| `GET_SOURCE_INFO_LIST` | `devices()`, leaving out the monitors of outputs |
| `GET_SOURCE_INFO` | `open()`, for the device's description |
| `CREATE_RECORD_STREAM` | `open()` |

The record stream asks for little-endian `f32`, one channel, at `Options::sample_rate`. The
server mixes down and resamples. Its buffer holds 2 s, packets carry `Options::piece`, and the
stream adjusts the source's latency to the packet size. Without a device name the stream
records from the default source and follows it when the user picks another.

While recording, the server's events change what Capta does.

| Event | Effect |
|---|---|
| `RECORD_STREAM_KILLED` | The capture ends with `Disconnected` |
| `RECORD_STREAM_SUSPENDED` | The stall allowance waits while the stream is suspended, and starts again when it resumes |
| `RECORD_STREAM_MOVED` | Recording goes on from the new source, which may be suspended |

Every value in the protocol carries a type byte, so the reader skips the fields it doesn't need
whatever their values. A field added after version 22 is read only when the negotiated version
has it. Packets longer than 16 MiB, replies to requests Capta didn't send
and audio in shared memory are protocol errors. A sample cut between two packets waits for the
rest.

## Startup lifecycle

`devices` and `open` are startups, and every backend runs them by the same four rules. On a
desktop a startup takes tens to hundreds of milliseconds, so call it off a UI thread and off an
async runtime's workers.

### Deadline

A startup has 3 s from the moment the public call begins, for everything it does. On Linux
that's reading the cookie, connecting, and every request and reply, however the server spreads
its bytes. On Windows and macOS it's listing the devices, or finding the device, opening it
and starting it.

### Admission

Some of that work can block where std can't time it out, such as connecting a Unix socket,
reading a file on a hung filesystem, or a stuck audio driver. That work runs on a startup
thread. Before the thread starts, the startup takes one of four permits, and the thread holds
it until it has finished and cleaned up, however it ends, a panic included. So at most four
startups run blocking work at once in a process. A startup that finds all four taken waits for
one until its deadline, then fails as `Stalled`.

### Cancellation

Before each phase, the startup thread checks the deadline and stops once it has passed. On
Linux the phases are reading the cookie and each connection attempt. On Windows and macOS
they're reading each device of a list, opening the device in the format asked for, asking for
the device's own rate when the system refuses the format, opening it again at that rate, and
starting it. The phases that run on the caller's thread, the requests on Linux, check the
deadline before every read or write, and a reply already buffered when the deadline passes is
refused.

### Acceptance

A startup's outcome is decided once, by whichever of the startup thread and the waiting
caller gets there first. The thread hands its result over if it finishes before the deadline.
Otherwise the startup is given up, by the thread when it finishes late or by the caller when
its wait ends, and either way the caller returns `Stalled` without waiting any longer. A
decision is final.

A handover decided before the deadline counts even when the caller receives it a little after,
because sending the result follows the decision and the thread may be paused in between. So the
deadline bounds when a startup must finish, and the caller may return a moment later.

The startup thread disposes of a result that comes too late itself. It closes a late socket,
or stops a late stream before any of its audio reaches `on_audio`, and only then gives its
permit back.

## Recording

Each capture has one thread, which owns the socket on Linux, the audio client on Windows and
the Audio Queue on macOS. The `Capture` handle shares only a stop flag and a way to wake the
thread with it, a clone of the socket, a duplicate of the event's handle or the thread's own
handle.
Capta takes no locks. `on_audio` and `on_error` run on that thread. `on_audio` should hand the
samples on and return, because the system drops audio once its 2 s buffer fills.

Dropping the `Capture` sets the flag, wakes the thread at once by shutting the socket down,
setting the event or unparking the thread, and joins the thread. So the drop waits for at
most one callback in progress, `on_audio` or `on_error`, and none runs after it. Neither may
wait for the thread that drops the capture. A drop from inside one of the capture's own
callbacks skips the join.

Only decoded samples count as audio. When none come for 5 s, and on Linux the stream isn't
suspended, the capture ends with `Stalled`, even while events or parts of a packet keep
arriving. The 5 s start again after each `on_audio` returns, so a slow callback doesn't count
against the system.

On Windows and macOS the device starts before the handover, and its audio waits for the
capture's thread, which reads none until the handover is decided. So `on_audio` gets the
recording from its start, in pieces of exactly `Options::piece`.

## Errors

`on_error` runs at most once, and `on_audio` never runs after it. Each error has a kind and a
lowercase detail for logs.

| Kind | Cause |
|---|---|
| `NoDevice` | No microphone, or none with the id asked for |
| `NoServer` | No sound server answers, or the audio service of Windows or macOS isn't running |
| `Busy` | Another program has the device |
| `PermissionDenied` | The server refused the login, the socket refused this user, or the system refused access to the microphone |
| `Disconnected` | The server closed the connection or ended the stream, or the device went away |
| `Stalled` | Opening took more than 3 s, or no audio came for 5 s |
| `Overrun` | Audio was lost, on macOS and Windows, so the recording has a gap |
| `Protocol` | The server or the system sent something Capta can't read |
| `Unsupported` | A rate, piece length, network server or protocol version Capta doesn't support |
| `Io` | The operating system failed a call |

## Windows

Capta records through WASAPI, calling ole32.dll and kernel32.dll itself. The linker binds the
two by name (raw-dylib), so building needs neither an import library nor the Windows SDK.

`devices()` lists the capture endpoints that are present and enabled, with `wasapi:` and the
endpoint's id as the device's id and its friendly name as the name, and marks the default
endpoint of the console role. Capta 0.1 gave the same ids and names.

The capture's thread enters COM's multithreaded apartment and initializes an `IAudioClient` on
the endpoint in shared mode, for one channel of 32-bit floats at `Options::sample_rate`, with
`AUTOCONVERTPCM` and `SRC_DEFAULT_QUALITY`. So the audio engine mixes down and resamples. When
the engine refuses that rate, a new client records at the engine's own rate, which `on_audio`
then gets. The stream is polled rather than event-driven, so the engine keeps the 2 s buffer it
was asked for. Every 5 ms, or as soon after as the system's timer allows, the thread reads every
packet that's ready. It waits on an event that only the `Capture` handle sets, to end the wait
when it drops.

| Packet | Effect |
|---|---|
| Marked as following a gap | The capture ends with `Overrun`, except for the first packet after starting, whose mark Windows leaves undefined |
| Marked silent | Counts as silence, whatever its bytes |
| Longer than the engine's buffer, or than 6,144,000 bytes | The capture ends with `Protocol` |

A list of more than 4096 endpoints, or an engine buffer of more than 4 s at 384 kHz, is a
`Protocol` error too.

A capture of the default stays on the endpoint that was the default when it opened. When the
endpoint goes away the capture ends with `Disconnected`.

## macOS

Capta records through Core Audio, calling the CoreFoundation, CoreAudio and AudioToolbox
frameworks itself.

`devices()` lists the audio devices that have an input stream, with `coreaudio:` and the
device's UID as the id and its name as the name, and marks the default input device. Capta 0.1
gave the same ids and names. An id names a device when its prefix matches in any case and its
UID exactly, on Windows likewise with the endpoint's id.

The capture's thread makes an Audio Queue that records one channel of 32-bit floats at
`Options::sample_rate` from the device. The queue converts the device's format and rate itself
and leaves the device's own rate alone. When it refuses the rate, it records at the device's
nominal rate instead, which `on_audio` then gets. Its buffers hold `Options::piece` each, within
10 to 100 ms, and 2 s together.

The queue calls back on a thread of its own with each filled buffer. The callback copies the
samples into a ring that holds 2 s, gives the buffer back to the queue, and does nothing else.
It takes no lock, allocates nothing and doesn't wake the capture thread, which looks at the ring
every 5 ms instead. The capture ends with `Overrun` when the ring is full because the capture
thread fell behind, or when a buffer starts later than the last one ended because the queue
dropped audio. Two callbacks at once, which Apple's documentation doesn't rule out, end it with
`Protocol`, since their samples' order is unknown. Every 250 ms the capture thread checks that
the device is still there, and ends the capture with `Disconnected` when it isn't.

macOS asks the person whether the program may use the microphone the first time it records.
Capta can't read their answer, so it fails with `PermissionDenied` only when Core Audio does.

## Safety

On Linux the crate has no `unsafe` code, and forbids it. On Windows and macOS `unsafe` code
only calls the system, in `src/wasapi/com.rs` and `src/coreaudio/ffi.rs`, each block with the
reason it's sound. [Why this is safe](why-this-is-safe.md) lists them all.

## Tests

`cargo test` runs the client against a fake server that answers with replies recorded from
pipewire-pulse 1.6.2 (`tests/fixtures/`). `cargo test -- --ignored` also records from the
machine's default microphone. CI runs both on Linux, macOS and Windows, the second against a
loopback device that plays a 440 Hz tone, with `CAPTA_TEST_TONE=440` so the tests check that
the recording carries it.
