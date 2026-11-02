//! A client of the PulseAudio native protocol, for recording only. PulseAudio serves the
//! protocol, and so does PipeWire through pipewire-pulse.
//!
//! One thread per capture owns the connection. The handle shares only a stop flag and a clone
//! of the socket with it, and shutting the socket down wakes the thread's blocking read.

mod discovery;
mod tagstruct;
#[cfg(test)]
mod tests;

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{Device, Error, ErrorKind, OnAudio, OnError, Opened, Options};
use tagstruct::{Malformed, Reader, SampleSpec, Writer};

/// The protocol version this client speaks, PulseAudio 15's and pipewire-pulse's.
const VERSION: u32 = 35;
/// The oldest version this client accepts. Every field it reads or writes is in it, so only
/// fields added later need a check of the version.
const MIN_VERSION: u32 = 22;

/// The channel of control packets. Audio comes on the stream's own channel.
const CONTROL: u32 = u32::MAX;
/// The index that means none.
const NO_INDEX: u32 = u32::MAX;
/// A packet's header holds its length, channel, a 64-bit offset and flags.
const HEADER: usize = 20;
/// The longest packet accepted, the servers' own limit.
const MAX_PACKET: usize = 16 << 20;
/// Marks a data packet whose audio is in shared memory, which this client doesn't offer.
const FLAG_SHM_DATA: u32 = 0x8000_0000;

const FLOAT32LE: u8 = 5;
const POSITION_MONO: u8 = 0;
const VOLUME_NORM: u32 = 0x1_0000;

/// The commands and events this client sends or reads.
mod command {
    pub const ERROR: u32 = 0;
    pub const REPLY: u32 = 2;
    pub const CREATE_RECORD_STREAM: u32 = 5;
    pub const AUTH: u32 = 8;
    pub const SET_CLIENT_NAME: u32 = 9;
    pub const GET_SERVER_INFO: u32 = 20;
    pub const GET_SOURCE_INFO: u32 = 23;
    pub const GET_SOURCE_INFO_LIST: u32 = 24;
    pub const RECORD_STREAM_KILLED: u32 = 65;
    pub const RECORD_STREAM_SUSPENDED: u32 = 77;
    pub const RECORD_STREAM_MOVED: u32 = 79;
}

/// How long to wait for the sound server.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    /// For each reply while opening.
    pub request: Duration,
    /// For audio while recording, unless the server suspended the stream.
    pub stall: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            request: Duration::from_secs(3),
            stall: Duration::from_secs(5),
        }
    }
}

/// Where the sound server may listen, and what to show it.
pub(crate) struct Server {
    pub paths: Vec<PathBuf>,
    pub cookie: Vec<u8>,
    pub timeouts: Timeouts,
}

impl Server {
    fn from_env() -> Result<Server, Error> {
        let env = |name: &str| std::env::var_os(name);
        Ok(Server {
            paths: discovery::candidates(&env, discovery::uid())?,
            cookie: discovery::cookie(&env),
            timeouts: Timeouts::default(),
        })
    }
}

pub(crate) fn devices() -> Result<Vec<Device>, Error> {
    devices_with(&Server::from_env()?, &crate::program_name())
}

pub(crate) fn open(
    options: &Options,
    on_audio: OnAudio,
    on_error: OnError,
) -> Result<Opened, Error> {
    open_with(&Server::from_env()?, options, on_audio, on_error)
}

fn devices_with(server: &Server, app_name: &str) -> Result<Vec<Device>, Error> {
    let mut connection = Connection::connect(server, app_name)?;
    let default = connection.default_source()?;
    let reply = connection.request(
        command::GET_SOURCE_INFO_LIST,
        "the device list request",
        |_| {},
    )?;
    let mut reader = Reader::new(&reply);
    let mut devices = Vec::new();
    while !reader.is_empty() {
        let source = Source::read(&mut reader, connection.version)?;
        if source.monitor_of == NO_INDEX {
            devices.push(source.into_device(default.as_deref()));
        }
    }
    Ok(devices)
}

fn open_with(
    server: &Server,
    options: &Options,
    on_audio: OnAudio,
    on_error: OnError,
) -> Result<Opened, Error> {
    let mut connection = Connection::connect(server, &options.app_name)?;
    let default = connection.default_source()?;
    let Some(wanted) = options.device.clone().or_else(|| default.clone()) else {
        return Err(Error::new(
            ErrorKind::NoDevice,
            "the sound server has no microphone",
        ));
    };
    let source = connection.source_info(&wanted)?;

    let rate = options.sample_rate;
    let bytes_for = |time: Duration| {
        let frames = (u128::from(rate) * time.as_nanos() / 1_000_000_000).max(1);
        u32::try_from(frames * 4).unwrap_or(u32::MAX)
    };
    let reply = connection.request(
        command::CREATE_RECORD_STREAM,
        "the recording request",
        |w| {
            w.sample_spec(SampleSpec {
                format: FLOAT32LE,
                channels: 1,
                rate,
            })
            .channel_map(&[POSITION_MONO])
            .u32(NO_INDEX)
            // No name records from the default and follows it when the user picks another.
            .string(options.device.as_deref())
            .u32(bytes_for(Duration::from_secs(2))) // the server's buffer
            .bool(false) // start corked
            .u32(bytes_for(options.piece)) // the packet size
            .bool(false) // no remapping of channels
            .bool(false) // no remixing of channels
            .bool(false) // take the device's format
            .bool(false) // take the device's rate
            .bool(false) // take the device's channels
            .bool(false) // never move
            .bool(false) // variable rate
            .bool(false) // peak detection
            .bool(true) // adjust the source's latency to the packet size
            .proplist(&[("media.name", "Microphone")])
            .u32(NO_INDEX) // record what one stream plays
            .bool(false) // early requests
            .bool(false) // keep the source from suspending
            .bool(false) // fail when the source suspends
            .u8(0) // formats
            .cvolume(&[VOLUME_NORM])
            .bool(false) // start muted
            .bool(false) // the volume is set
            .bool(false) // muting is set
            .bool(false) // relative volume
            .bool(false); // passthrough
        },
    )?;
    let mut reader = Reader::new(&reply);
    let channel = reader.u32()?;
    // The stream's index, the server's buffer and the packet size.
    reader.skip_n(3)?;
    let spec = reader.sample_spec()?;
    reader.skip()?; // the channel map
    let source_index = reader.u32()?;
    let source_name = reader.string()?.unwrap_or_default();
    let suspended = reader.bool()?;
    if spec.format != FLOAT32LE || spec.channels != 1 || spec.rate == 0 {
        return Err(Error::new(
            ErrorKind::Protocol,
            format!(
                "the sound server offered format {} with {} channels at {} Hz instead of mono floats",
                spec.format, spec.channels, spec.rate
            ),
        ));
    }
    let device = if source_index == source.index {
        source.into_device(default.as_deref())
    } else {
        // The default changed between the two requests.
        Device {
            id: source_name.clone(),
            name: source_name,
            is_default: options.device.is_none(),
        }
    };

    let stall = connection.timeouts.stall;
    let setup = |error| io_failure(error, "setting up the recording");
    connection
        .socket
        .set_read_timeout(Some(stall))
        .map_err(setup)?;
    let socket = connection.socket.try_clone().map_err(setup)?;
    let stop = Arc::new(AtomicBool::new(false));
    let recording = Recording {
        connection,
        channel,
        rate: spec.rate,
        suspended,
        stop: Arc::clone(&stop),
        on_audio,
    };
    let thread = std::thread::Builder::new()
        .name("capta".into())
        .spawn(move || recording.run(on_error))
        .map_err(|error| io_failure(error, "starting the capture thread"))?;
    Ok(Opened {
        stream: Stream {
            stop,
            socket,
            thread: Some(thread),
        },
        device,
        sample_rate: spec.rate,
    })
}

/// A running capture's handle.
pub(crate) struct Stream {
    stop: Arc<AtomicBool>,
    socket: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.socket.shutdown(Shutdown::Both);
        // From a callback, the capture thread ends by itself once the callback returns.
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

/// What the capture thread owns.
struct Recording {
    connection: Connection,
    channel: u32,
    rate: u32,
    suspended: bool,
    stop: Arc<AtomicBool>,
    on_audio: OnAudio,
}

impl Recording {
    fn run(mut self, on_error: OnError) {
        if let Err(error) = self.receive()
            && !self.stop.load(Ordering::SeqCst)
        {
            on_error(error);
        }
    }

    /// Hands audio to `on_audio` until the capture stops, or returns why it failed.
    fn receive(&mut self) -> Result<(), Error> {
        let mut samples = Vec::new();
        let mut partial = Vec::with_capacity(4);
        let mut last_audio = Instant::now();
        loop {
            let packet = self.connection.read_packet();
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            match packet? {
                None => {}
                Some(packet) if packet.channel == self.channel => {
                    if packet.flags & FLAG_SHM_DATA != 0 {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "the sound server sent audio in shared memory, which wasn't offered",
                        ));
                    }
                    decode(packet.body, &mut partial, &mut samples);
                    last_audio = Instant::now();
                    if !samples.is_empty() {
                        (self.on_audio)(&samples, self.rate);
                        samples.clear();
                    }
                }
                Some(packet) if packet.channel == CONTROL => {
                    if let Some(suspended) = event(packet.body, self.channel)? {
                        self.suspended = suspended;
                        last_audio = Instant::now();
                    }
                }
                // Audio of a stream this client didn't open.
                Some(_) => {}
            }
            if !self.suspended && last_audio.elapsed() >= self.connection.timeouts.stall {
                return Err(Error::new(
                    ErrorKind::Stalled,
                    format!(
                        "the sound server sent no audio for {:.1} s",
                        self.connection.timeouts.stall.as_secs_f32()
                    ),
                ));
            }
        }
    }
}

/// Appends the little-endian floats in `bytes` to `samples`. A sample cut between two packets
/// waits in `partial` for the rest.
fn decode(mut bytes: &[u8], partial: &mut Vec<u8>, samples: &mut Vec<f32>) {
    if !partial.is_empty() {
        let taken = bytes.len().min(4 - partial.len());
        partial.extend_from_slice(&bytes[..taken]);
        bytes = &bytes[taken..];
        if let Ok(sample) = <[u8; 4]>::try_from(&partial[..]) {
            samples.push(f32::from_le_bytes(sample));
            partial.clear();
        }
    }
    let chunks = bytes.chunks_exact(4);
    partial.extend_from_slice(chunks.remainder());
    samples.extend(chunks.map(|sample| f32::from_le_bytes(sample.try_into().unwrap())));
}

/// Reads an event of the stream on `channel`. Returns whether the server suspended the stream
/// when the event says, and an error when it ended the stream.
fn event(body: &[u8], channel: u32) -> Result<Option<bool>, Error> {
    let mut reader = Reader::new(body);
    let command = reader.u32()?;
    reader.skip()?; // the sequence number, none for events
    match command {
        command::REPLY | command::ERROR => {
            return Err(Error::new(
                ErrorKind::Protocol,
                "the sound server answered a request this client didn't send",
            ));
        }
        command::RECORD_STREAM_KILLED
        | command::RECORD_STREAM_SUSPENDED
        | command::RECORD_STREAM_MOVED => {}
        _ => return Ok(None),
    }
    if reader.u32()? != channel {
        return Ok(None);
    }
    match command {
        command::RECORD_STREAM_KILLED => Err(Error::new(
            ErrorKind::Disconnected,
            "the sound server ended the recording",
        )),
        command::RECORD_STREAM_SUSPENDED => Ok(Some(reader.bool()?)),
        _ => {
            // The new source's index and name.
            reader.skip_n(2)?;
            Ok(Some(reader.bool()?))
        }
    }
}

/// A source as the server describes it, with the fields this client needs.
struct Source {
    index: u32,
    name: String,
    description: Option<String>,
    /// The sink a monitor source records, else [`NO_INDEX`].
    monitor_of: u32,
}

impl Source {
    fn read(reader: &mut Reader<'_>, version: u32) -> Result<Source, Malformed> {
        let index = reader.u32()?;
        let name = reader
            .string()?
            .ok_or_else(|| Malformed("a source has no name".into()))?;
        let description = reader.string()?;
        // The sample spec, channel map, owning module, volume and mute.
        reader.skip_n(5)?;
        let monitor_of = reader.u32()?;
        // The monitored sink's name, the latency, driver, flags, properties, configured
        // latency, base volume, state, volume steps and card.
        reader.skip_n(10)?;
        // Each port's name, description and priority, then its availability from version 24,
        // and its availability group and type from version 34.
        let port_fields = 3 + usize::from(version >= 24) + 2 * usize::from(version >= 34);
        for _ in 0..reader.u32()? {
            reader.skip_n(port_fields)?;
        }
        reader.skip()?; // the active port
        let formats = reader.u8()?;
        reader.skip_n(formats.into())?;
        Ok(Source {
            index,
            name,
            description,
            monitor_of,
        })
    }

    fn into_device(self, default: Option<&str>) -> Device {
        Device {
            is_default: default == Some(self.name.as_str()),
            name: self.description.unwrap_or_else(|| self.name.clone()),
            id: self.name,
        }
    }
}

/// A packet as it came from the server.
struct Packet<'a> {
    channel: u32,
    flags: u32,
    body: &'a [u8],
}

/// A connection after the handshake.
struct Connection {
    socket: UnixStream,
    /// Bytes read from the socket, the unread ones from `start` to `end`.
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    version: u32,
    next_sequence: u32,
    timeouts: Timeouts,
}

impl Connection {
    /// Connects to the first server that answers and logs in.
    fn connect(server: &Server, app_name: &str) -> Result<Connection, Error> {
        let mut refused = None;
        for path in &server.paths {
            match UnixStream::connect(path) {
                Ok(socket) => return Connection::log_in(socket, server, app_name),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    refused = Some(path)
                }
                Err(_) => {}
            }
        }
        Err(match refused {
            Some(path) => Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "this user may not connect to the sound server at {}",
                    path.display()
                ),
            ),
            None => discovery::no_server(&|name| std::env::var_os(name), discovery::uid()),
        })
    }

    fn log_in(socket: UnixStream, server: &Server, app_name: &str) -> Result<Connection, Error> {
        let setup = |error| io_failure(error, "setting up the connection");
        socket
            .set_read_timeout(Some(server.timeouts.request))
            .map_err(setup)?;
        socket
            .set_write_timeout(Some(server.timeouts.request))
            .map_err(setup)?;
        let mut connection = Connection {
            socket,
            buffer: vec![0; 64 << 10],
            start: 0,
            end: 0,
            version: VERSION,
            next_sequence: 0,
            timeouts: server.timeouts,
        };
        let reply = connection.request(command::AUTH, "the login", |w| {
            // The version, with the flags for shared memory and memfd left clear.
            w.u32(VERSION).arbitrary(&server.cookie);
        })?;
        // The server's flags take the top bits.
        let server_version = Reader::new(&reply).u32()? & 0xffff;
        if server_version < MIN_VERSION {
            return Err(Error::new(
                ErrorKind::Unsupported,
                format!(
                    "the sound server speaks protocol {server_version}, older than {MIN_VERSION}"
                ),
            ));
        }
        connection.version = server_version.min(VERSION);
        let pid = std::process::id().to_string();
        connection.request(command::SET_CLIENT_NAME, "the client name", |w| {
            w.proplist(&[
                ("application.name", app_name),
                ("application.process.id", &pid),
            ]);
        })?;
        Ok(connection)
    }

    /// The name of the server's default source, if it has one.
    fn default_source(&mut self) -> Result<Option<String>, Error> {
        let reply = self.request(
            command::GET_SERVER_INFO,
            "the server information request",
            |_| {},
        )?;
        let mut reader = Reader::new(&reply);
        // The server's name, version, user and host, its sample spec and default sink.
        reader.skip_n(6)?;
        Ok(reader.string()?)
    }

    fn source_info(&mut self, name: &str) -> Result<Source, Error> {
        let reply = self
            .request(command::GET_SOURCE_INFO, "the device request", |w| {
                w.u32(NO_INDEX).string(Some(name));
            })
            .map_err(|error| match error.kind() {
                ErrorKind::NoDevice => Error::new(
                    ErrorKind::NoDevice,
                    format!("the sound server has no device named {name}"),
                ),
                _ => error,
            })?;
        Ok(Source::read(&mut Reader::new(&reply), self.version)?)
    }

    /// Sends a command and waits for its reply, which it returns without the header. Events
    /// that come first are passed over.
    fn request(
        &mut self,
        command: u32,
        what: &str,
        fields: impl FnOnce(&mut Writer),
    ) -> Result<Vec<u8>, Error> {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let mut packet = Writer {
            bytes: vec![0; HEADER],
        };
        packet.u32(command).u32(sequence);
        fields(&mut packet);
        let mut packet = packet.bytes;
        let length = u32::try_from(packet.len() - HEADER).expect("requests are small");
        packet[..4].copy_from_slice(&length.to_be_bytes());
        packet[4..8].copy_from_slice(&CONTROL.to_be_bytes());
        // The whole packet in one write, so a server that reads credentials gets them with it.
        self.socket
            .write_all(&packet)
            .map_err(|error| io_failure(error, "sending to the sound server"))?;

        let deadline = Instant::now() + self.timeouts.request;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(
                    ErrorKind::Stalled,
                    format!(
                        "the sound server didn't answer {what} within {:.1} s",
                        self.timeouts.request.as_secs_f32()
                    ),
                ));
            }
            self.socket
                .set_read_timeout(Some(remaining))
                .map_err(|error| io_failure(error, "setting up the connection"))?;
            let Some(packet) = self.read_packet()? else {
                continue;
            };
            if packet.channel != CONTROL {
                continue;
            }
            let mut reader = Reader::new(packet.body);
            let reply = reader.u32()?;
            if reply != command::REPLY && reply != command::ERROR {
                continue;
            }
            if reader.u32()? != sequence {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "the sound server answered a request this client didn't send",
                ));
            }
            if reply == command::ERROR {
                return Err(server_error(reader.u32()?, what));
            }
            return Ok(reader.rest().to_vec());
        }
    }

    /// The next packet, or `None` when the read timeout passes first. A packet the timeout
    /// cuts stays buffered for the next call.
    fn read_packet(&mut self) -> Result<Option<Packet<'_>>, Error> {
        let length = loop {
            let buffered = self.end - self.start;
            let needed = if buffered < HEADER {
                HEADER
            } else {
                let length = be_u32(&self.buffer[self.start..]) as usize;
                if length > MAX_PACKET {
                    return Err(Error::new(
                        ErrorKind::Protocol,
                        format!(
                            "the sound server sent a packet of {length} bytes, more than the {MAX_PACKET} allowed"
                        ),
                    ));
                }
                HEADER + length
            };
            if buffered >= needed {
                break needed;
            }
            if self.buffer.len() - self.start < needed {
                self.buffer.copy_within(self.start..self.end, 0);
                self.end = buffered;
                self.start = 0;
                if self.buffer.len() < needed {
                    self.buffer.resize(needed, 0);
                }
            }
            match self.socket.read(&mut self.buffer[self.end..]) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::Disconnected,
                        "the sound server closed the connection",
                    ));
                }
                Ok(read) => self.end += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(io_failure(error, "reading from the sound server")),
            }
        };
        let packet = &self.buffer[self.start..self.start + length];
        self.start += length;
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        }
        Ok(Some(Packet {
            channel: be_u32(&packet[4..]),
            flags: be_u32(&packet[16..]),
            body: &packet[HEADER..],
        }))
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn io_failure(error: io::Error, doing: &str) -> Error {
    match error.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => Error::new(
            ErrorKind::Disconnected,
            "the sound server closed the connection",
        ),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            Error::new(ErrorKind::Stalled, "the sound server stopped reading")
        }
        _ => Error::new(ErrorKind::Io, format!("{doing} failed ({error})")),
    }
}

/// An error the server answered with, by libpulse's names for its codes.
fn server_error(code: u32, what: &str) -> Error {
    const NAMES: [&str; 27] = [
        "no error",
        "access denied",
        "unknown command",
        "invalid argument",
        "entity exists",
        "no such entity",
        "connection refused",
        "protocol error",
        "timeout",
        "no authentication key",
        "internal error",
        "connection terminated",
        "entity killed",
        "invalid server",
        "module initialization failed",
        "bad state",
        "no data",
        "incompatible protocol version",
        "too large",
        "not supported",
        "unknown error code",
        "no such extension",
        "obsolete functionality",
        "missing implementation",
        "client forked",
        "input/output error",
        "device or resource busy",
    ];
    let kind = match code {
        1 | 9 => ErrorKind::PermissionDenied,
        5 => ErrorKind::NoDevice,
        8 => ErrorKind::Stalled,
        11 | 12 => ErrorKind::Disconnected,
        3 | 17 | 18 | 19 | 23 => ErrorKind::Unsupported,
        26 => ErrorKind::Busy,
        _ => ErrorKind::Protocol,
    };
    let name = NAMES
        .get(code as usize)
        .copied()
        .unwrap_or("unknown error code");
    Error::new(kind, format!("the sound server refused {what} ({name})"))
}

impl From<Malformed> for Error {
    fn from(Malformed(detail): Malformed) -> Self {
        Error::new(
            ErrorKind::Protocol,
            format!("the sound server sent a reply that can't be read ({detail})"),
        )
    }
}
