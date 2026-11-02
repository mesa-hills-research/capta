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

The cookie comes from `$PULSE_COOKIE`, `$XDG_CONFIG_HOME/pulse/cookie` (or
`~/.config/pulse/cookie`) or `~/.pulse-cookie`, else it's 256 zero bytes. pipewire-pulse checks
only the cookie's length, and PulseAudio lets in a client of its own user by the socket's
credentials. Capta never writes a cookie.

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

`devices` and `open` are startups, and both backends run them by the same four rules. On a
desktop a startup takes tens to hundreds of milliseconds, so call it off a UI thread and off an
async runtime's workers.

### Deadline

A startup has 3 s from the moment the public call begins, for everything it does. On Linux
that's connecting, and every request and reply, however the server spreads its bytes. Through cpal it's listing the devices, choosing a format, building the stream and
starting it.

### Admission

Some of that work can block where std can't time it out, such as connecting a Unix socket or
a stuck audio driver. That work runs on a startup
thread. Before the thread starts, the startup takes one of four permits, and the thread holds
it until it has finished and cleaned up, however it ends, a panic included. So at most four
startups run blocking work at once in a process. A startup that finds all four taken waits for
one until its deadline, then fails as `Stalled`.

### Cancellation

Before each phase, the startup thread checks the deadline and stops once it has passed. On
Linux the phases are the connection attempts. Through cpal they're
finding the device, choosing its format, building the stream and starting it. The phases that
run on the caller's thread, the requests on Linux, check the deadline before every read or
write, and a reply already buffered when the deadline passes is refused.

### Acceptance

A result counts only if the startup thread finishes before the deadline. The thread and the
waiting caller settle that with one atomic step, so a result is either handed over or given
up, never both. The startup thread disposes of a result that comes too late itself. It closes
a late socket, or stops and drops a late cpal stream without calling back, and only then gives
its permit back.

## Recording

Each capture has one thread, which owns the socket. The `Capture` handle shares only a stop
flag and a clone of the socket with it, and Capta takes no locks. `on_audio` and `on_error`
run on that thread. `on_audio` should hand the samples on and return, because the server drops
audio once its 2 s buffer fills.

Dropping the `Capture` sets the flag, shuts the socket down, which wakes the thread's read at
once, and joins the thread. So the drop waits for at most one callback in progress, `on_audio`
or `on_error`, and none runs after it. Neither may wait for the thread that drops the capture.
A drop from inside one of the capture's own callbacks skips the join.

Only decoded samples count as audio. When none come for 5 s and the stream isn't suspended,
the capture ends with `Stalled`, even while events or parts of a packet keep arriving. The 5 s
start again after each `on_audio` returns, so a slow callback doesn't count against the
server.

## Errors

`on_error` runs at most once, and `on_audio` never runs after it. Each error has a kind and a
lowercase detail for logs.

| Kind | Cause |
|---|---|
| `NoDevice` | No microphone, or none with the id asked for |
| `NoServer` | No sound server answers |
| `Busy` | Another program has the device |
| `PermissionDenied` | The server refused the login, the socket refused this user, or the system refused access to the microphone |
| `Disconnected` | The server closed the connection or ended the stream |
| `Stalled` | Opening took more than 3 s, or the server sent no audio for 5 s |
| `Overrun` | Audio was lost, on macOS and Windows, so the recording has a gap |
| `Protocol` | The server sent something Capta can't read |
| `Unsupported` | A rate, piece length, network server or protocol version Capta doesn't support |
| `Io` | The operating system failed a call |

## macOS and Windows

Capta records through cpal 0.18. It picks the device's format with the rate asked for when the
device has it, preferring `f32` and the fewest channels, else the device's default format. It
mixes the channels down to mono, and `on_audio` gets the rate it records at. cpal's stream
lives on a thread of the capture's own, since it isn't `Send` everywhere, and cpal calls
`on_audio` on its audio thread.

Starting follows the lifecycle above, on the capture's thread, which gives its permit back once
the stream is handed over or dropped. When cpal reports an overrun, audio was lost, so the
capture ends with `Overrun` before the next buffer reaches `on_audio`.

Cargo resolves dependencies for every platform at once, and only one package in a project may
link ALSA. So a project that uses Capta can't also use a crate that links another version of
`alsa-sys`, such as cpal 0.15, even though Capta itself never builds cpal on Linux.

## Tests

`cargo test` runs the client against a fake server that answers with replies recorded from
pipewire-pulse 1.6.2 (`tests/fixtures/`). `cargo test -- --ignored` also records from the sound
server the machine runs.
