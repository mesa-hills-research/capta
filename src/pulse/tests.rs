//! The client against a fake server on a socket of its own, which answers with replies
//! recorded from pipewire-pulse 1.6.2 and sends events as the `pulseaudio` crate encodes them.

use std::io::Cursor;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;

use pulseaudio::protocol as reference;

use super::*;

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pipewire-pulse-1.6.2/",
            $name,
            ".bin"
        ))
        .as_slice()
    };
}

const MICROPHONE: &str = "alsa_input.pci-0000_02_02.0.analog-stereo";
const QUICK: Timeouts = Timeouts {
    open: Duration::from_millis(300),
    stall: Duration::from_millis(300),
};
const LONG: Duration = Duration::from_secs(3);

/// A recorded reply's fields, after its command and sequence number.
fn fields(packet: &[u8]) -> &[u8] {
    &packet[HEADER + 10..]
}

/// A fake sound server that takes one connection and runs `script` on it.
fn serve(script: impl FnOnce(&mut Peer) + Send + 'static) -> (Server, JoinHandle<()>) {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "capta-test-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("native");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        let started = Instant::now();
        let accepted = loop {
            match listener.accept() {
                Ok((socket, _)) => break Some(socket),
                Err(_) if started.elapsed() < LONG => thread::sleep(Duration::from_millis(5)),
                Err(_) => break None,
            }
        };
        std::fs::remove_dir_all(&dir).unwrap();
        // A client that never connects has failed already, and its test says why.
        let Some(socket) = accepted else { return };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        script(&mut Peer { socket });
    });
    (
        Server {
            paths: vec![path],
            cookie: vec![0; 256],
            timeouts: QUICK,
        },
        thread,
    )
}

/// The server's end of a connection.
struct Peer {
    socket: UnixStream,
}

impl Peer {
    /// Reads a request, and returns its sequence number and the whole body.
    fn expect(&mut self, command: u32) -> (u32, Vec<u8>) {
        let mut header = [0; HEADER];
        self.socket.read_exact(&mut header).unwrap();
        assert_eq!(be_u32(&header[4..]), CONTROL);
        let mut body = vec![0; be_u32(&header) as usize];
        self.socket.read_exact(&mut body).unwrap();
        let mut reader = Reader::new(&body);
        assert_eq!(reader.u32(), Ok(command));
        (reader.u32().unwrap(), body)
    }

    fn try_send(&mut self, channel: u32, body: &[u8]) -> bool {
        let mut packet = Vec::with_capacity(HEADER + body.len());
        packet.extend_from_slice(&(body.len() as u32).to_be_bytes());
        packet.extend_from_slice(&channel.to_be_bytes());
        packet.extend_from_slice(&[0; 12]);
        packet.extend_from_slice(body);
        self.socket.write_all(&packet).is_ok()
    }

    fn send(&mut self, channel: u32, body: &[u8]) {
        assert!(self.try_send(channel, body));
    }

    /// Answers `sequence` with a reply pipewire-pulse sent.
    fn replay(&mut self, recorded: &[u8], sequence: u32) {
        let mut packet = recorded.to_vec();
        packet[HEADER + 6..HEADER + 10].copy_from_slice(&sequence.to_be_bytes());
        self.socket.write_all(&packet).unwrap();
    }

    fn refuse(&mut self, sequence: u32, code: u32) {
        let mut w = Writer::default();
        w.u32(command::ERROR).u32(sequence).u32(code);
        self.send(CONTROL, &w.bytes);
    }

    fn event(&mut self, event: reference::Command) {
        let mut body = Vec::new();
        event.write_tag_prefixed(u32::MAX, &mut body, 35).unwrap();
        self.send(CONTROL, &body);
    }

    fn audio(&mut self, samples: &[f32]) -> bool {
        self.try_send(0, &bytes(samples))
    }

    fn log_in(&mut self) {
        let (sequence, _) = self.expect(command::AUTH);
        self.replay(fixture!("auth-reply"), sequence);
        let (sequence, _) = self.expect(command::SET_CLIENT_NAME);
        self.replay(fixture!("client-name-reply"), sequence);
        let (sequence, _) = self.expect(command::GET_SERVER_INFO);
        self.replay(fixture!("server-info-reply"), sequence);
    }

    /// Answers the requests of `open` as pipewire-pulse did.
    fn open(&mut self) {
        self.log_in();
        let (sequence, _) = self.expect(command::GET_SOURCE_INFO);
        self.replay(fixture!("source-info-reply"), sequence);
        let (sequence, _) = self.expect(command::CREATE_RECORD_STREAM);
        self.replay(fixture!("create-record-stream-reply"), sequence);
    }

    /// Waits for the client to close the connection.
    fn wait_for_close(&mut self) {
        let mut byte = [0];
        while let Ok(1) = self.socket.read(&mut byte) {}
    }
}

fn bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

fn sine(count: usize) -> Vec<f32> {
    (0..count)
        .map(|n| (n as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin() * 0.5)
        .collect()
}

/// What a capture's callbacks heard.
struct Heard {
    audio: mpsc::Receiver<Vec<f32>>,
    errors: mpsc::Receiver<Error>,
}

impl Heard {
    /// The next `count` samples, whatever the pieces.
    fn samples(&self, count: usize) -> Vec<f32> {
        let mut samples = Vec::new();
        while samples.len() < count {
            samples.extend(self.audio.recv_timeout(LONG).expect("audio"));
        }
        samples
    }

    fn error(&self) -> Error {
        self.errors.recv_timeout(LONG).expect("an error")
    }
}

fn open_fake(server: &Server) -> Result<(Opened, Heard), Error> {
    let (audio, heard_audio) = mpsc::channel();
    let (errors, heard_errors) = mpsc::channel();
    let opened = open_with(
        server,
        &Options::default(),
        Box::new(move |samples, _| {
            let _ = audio.send(samples.to_vec());
        }),
        Box::new(move |error| {
            let _ = errors.send(error);
        }),
    )?;
    Ok((
        opened,
        Heard {
            audio: heard_audio,
            errors: heard_errors,
        },
    ))
}

#[test]
fn recorded_replies_read_as_the_reference_reads_them() {
    let mut reader = Reader::new(fields(fixture!("server-info-reply")));
    reader.skip_n(6).unwrap();
    assert_eq!(reader.string().unwrap().as_deref(), Some(MICROPHONE));

    let list = fixture!("source-info-list-reply");
    let mut reader = Reader::new(fields(list));
    let mut ours = Vec::new();
    while !reader.is_empty() {
        ours.push(Source::read(&mut reader, VERSION).unwrap());
    }
    let (_, theirs) =
        reference::read_reply_message::<reference::SourceInfoList>(&mut Cursor::new(list), 35)
            .unwrap();
    assert_eq!(ours.len(), 2);
    assert_eq!(ours.len(), theirs.len());
    for (ours, theirs) in ours.iter().zip(&theirs) {
        assert_eq!(ours.index, theirs.index);
        assert_eq!(ours.name.as_bytes(), theirs.name.as_bytes());
        assert_eq!(
            ours.description.as_deref().map(str::as_bytes),
            theirs.description.as_ref().map(|text| text.as_bytes())
        );
        assert_eq!(
            ours.monitor_of != NO_INDEX,
            theirs.monitor_of_sink_index.is_some()
        );
    }

    let (_, reply) = reference::read_reply_message::<reference::CreateRecordStreamReply>(
        &mut Cursor::new(fixture!("create-record-stream-reply")),
        35,
    )
    .unwrap();
    assert_eq!(reply.channel, 0);
    assert_eq!(reply.sample_spec.format, reference::SampleFormat::Float32Le);
    assert_eq!(reply.sample_spec.channels, 1);
    assert_eq!(reply.sample_spec.sample_rate, 16_000);
    assert_eq!(reply.buffer_attr.fragment_size, 1280);
}

#[test]
fn ports_are_read_by_the_negotiated_version() {
    for version in [22, 24, 34, 35] {
        let mut w = Writer::default();
        w.u32(3).string(Some("mic")).string(Some("Mic"));
        w.sample_spec(SampleSpec {
            format: 3,
            channels: 2,
            rate: 48_000,
        })
        .channel_map(&[1, 2]);
        w.u32(NO_INDEX)
            .cvolume(&[VOLUME_NORM; 2])
            .bool(false)
            .u32(NO_INDEX)
            .string(None);
        w.bytes.push(b'U');
        w.bytes.extend_from_slice(&0u64.to_be_bytes());
        w.string(Some("driver"))
            .u32(0)
            .proplist(&[("device.class", "sound")]);
        w.bytes.push(b'U');
        w.bytes.extend_from_slice(&0u64.to_be_bytes());
        w.bytes.push(b'V');
        w.bytes.extend_from_slice(&VOLUME_NORM.to_be_bytes());
        // A state pipewire-pulse sends for a node it's still creating.
        w.u32(-2i32 as u32).u32(0).u32(NO_INDEX);
        w.u32(2);
        for port in ["analog-input-mic", "analog-input-linein"] {
            w.string(Some(port)).string(Some("A port")).u32(100);
            if version >= 24 {
                w.u32(2);
            }
            if version >= 34 {
                w.string(None).u32(0);
            }
        }
        w.string(Some("analog-input-mic")).u8(1);
        w.bytes.extend_from_slice(&[b'f', b'B', 1]);
        w.proplist(&[]);
        let mut reader = Reader::new(&w.bytes);
        let source = Source::read(&mut reader, version).unwrap();
        assert_eq!((source.index, source.name.as_str()), (3, "mic"));
        assert!(reader.is_empty(), "version {version}");
    }
}

#[test]
fn requests_read_as_the_reference_reads_them() {
    let (server, thread) = serve(|peer| {
        let read = |body: &[u8]| reference::Command::read_tag_prefixed(&mut Cursor::new(body), 35);
        let (sequence, body) = peer.expect(command::AUTH);
        let (_, reference::Command::Auth(auth)) = read(&body).unwrap() else {
            panic!()
        };
        assert_eq!(
            (auth.version, auth.supports_shm, auth.supports_memfd),
            (35, false, false)
        );
        assert_eq!(auth.cookie, [0; 256]);
        peer.replay(fixture!("auth-reply"), sequence);

        let (sequence, body) = peer.expect(command::SET_CLIENT_NAME);
        let (_, reference::Command::SetClientName(props)) = read(&body).unwrap() else {
            panic!()
        };
        assert!(
            props
                .get(reference::Prop::ApplicationName)
                .unwrap()
                .starts_with(b"Tester")
        );
        peer.replay(fixture!("client-name-reply"), sequence);

        let (sequence, body) = peer.expect(command::GET_SERVER_INFO);
        assert_eq!(read(&body).unwrap().1, reference::Command::GetServerInfo);
        peer.replay(fixture!("server-info-reply"), sequence);

        let (sequence, body) = peer.expect(command::GET_SOURCE_INFO);
        let (_, reference::Command::GetSourceInfo(info)) = read(&body).unwrap() else {
            panic!()
        };
        assert_eq!(info.index, None);
        assert_eq!(info.name.unwrap().as_bytes(), MICROPHONE.as_bytes());
        peer.replay(fixture!("source-info-reply"), sequence);

        let (sequence, body) = peer.expect(command::CREATE_RECORD_STREAM);
        let (_, reference::Command::CreateRecordStream(stream)) = read(&body).unwrap() else {
            panic!()
        };
        assert_eq!(
            stream.sample_spec.format,
            reference::SampleFormat::Float32Le
        );
        assert_eq!(
            (stream.sample_spec.channels, stream.sample_spec.sample_rate),
            (1, 16_000)
        );
        assert_eq!((stream.source_index, stream.source_name), (None, None));
        assert_eq!(stream.buffer_attr.max_length, 2 * 16_000 * 4);
        assert_eq!(stream.buffer_attr.fragment_size, 160 * 4);
        assert!(stream.flags.adjust_latency && !stream.flags.start_corked);
        assert!(!stream.flags.fix_rate && !stream.flags.fix_channels && !stream.flags.no_move);
        assert_eq!(stream.cvolume, None);
        assert!(stream.formats.is_empty());
        peer.replay(fixture!("create-record-stream-reply"), sequence);
        peer.wait_for_close();
    });
    let options = Options {
        piece: Duration::from_millis(10),
        app_name: "Tester".into(),
        ..Options::default()
    };
    let opened = open_with(&server, &options, Box::new(|_, _| {}), Box::new(|_| {})).unwrap();
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn records_what_the_server_sends() {
    let audio = sine(960);
    let sent = audio.clone();
    let (server, thread) = serve(move |peer| {
        peer.open();
        for piece in sent.chunks(320) {
            peer.audio(piece);
        }
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    assert_eq!(opened.device.id, MICROPHONE);
    assert!(opened.device.name.starts_with("ES1371/ES1373"));
    assert!(opened.device.is_default);
    assert_eq!(opened.sample_rate, 16_000);
    assert_eq!(heard.samples(960), audio);
    drop(opened);
    thread.join().unwrap();
    assert!(heard.errors.try_recv().is_err());
}

#[test]
fn samples_cut_between_packets_are_joined() {
    let audio = sine(100);
    let sent = bytes(&audio);
    let (server, thread) = serve(move |peer| {
        peer.open();
        let mut rest = &sent[..];
        for size in [7, 1, 5, 3, 2, 2, 380].into_iter().cycle() {
            let (piece, after) = rest.split_at(size.min(rest.len()));
            peer.send(0, piece);
            rest = after;
            if rest.is_empty() {
                break;
            }
        }
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    assert_eq!(heard.samples(100), audio);
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_login_without_an_answer_times_out() {
    let (server, thread) = serve(|peer| {
        peer.expect(command::AUTH);
        thread::sleep(Duration::from_secs(1));
    });
    let started = Instant::now();
    let error = open_fake(&server).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Stalled, "{error}");
    assert!(started.elapsed() < Duration::from_millis(900));
    thread.join().unwrap();
}

#[test]
fn error_replies_keep_their_kind() {
    let (server, thread) = serve(|peer| {
        let (sequence, _) = peer.expect(command::AUTH);
        peer.refuse(sequence, 1);
    });
    let error = open_fake(&server).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        error.to_string(),
        "the sound server refused the login (access denied)"
    );
    thread.join().unwrap();

    let (server, thread) = serve(|peer| {
        peer.log_in();
        let (sequence, _) = peer.expect(command::GET_SOURCE_INFO);
        peer.refuse(sequence, 5);
    });
    let options = Options {
        device: Some("usb-mic".into()),
        ..Options::default()
    };
    let error = open_with(&server, &options, Box::new(|_, _| {}), Box::new(|_| {}))
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::NoDevice);
    assert_eq!(
        error.to_string(),
        "the sound server has no device named usb-mic"
    );
    thread.join().unwrap();

    let (server, thread) = serve(|peer| {
        peer.log_in();
        let (sequence, _) = peer.expect(command::GET_SOURCE_INFO);
        peer.replay(fixture!("source-info-reply"), sequence);
        let (sequence, _) = peer.expect(command::CREATE_RECORD_STREAM);
        peer.refuse(sequence, 26);
    });
    assert_eq!(open_fake(&server).err().unwrap().kind(), ErrorKind::Busy);
    thread.join().unwrap();
}

#[test]
fn a_reply_to_another_request_is_a_protocol_error() {
    let (server, thread) = serve(|peer| {
        let (sequence, _) = peer.expect(command::AUTH);
        peer.replay(fixture!("auth-reply"), sequence + 1);
    });
    assert_eq!(
        open_fake(&server).err().unwrap().kind(),
        ErrorKind::Protocol
    );
    thread.join().unwrap();
}

#[test]
fn a_killed_stream_is_disconnected() {
    let (server, thread) = serve(|peer| {
        peer.open();
        peer.audio(&sine(320));
        peer.event(reference::Command::RecordStreamKilled(0));
        peer.audio(&sine(320));
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    heard.samples(320);
    assert_eq!(heard.error().kind(), ErrorKind::Disconnected);
    thread::sleep(Duration::from_millis(100));
    assert!(heard.audio.try_recv().is_err(), "audio after the error");
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_moved_stream_keeps_recording() {
    let (server, thread) = serve(|peer| {
        peer.open();
        peer.audio(&sine(320));
        peer.event(reference::Command::RecordStreamMoved(
            reference::RecordStreamMovedParams {
                stream_index: 0,
                device_index: 60,
                device_name: c"bluez_input.headset".into(),
                device_suspended: false,
                buffer_attr: Default::default(),
                configured_source_latency: 0,
            },
        ));
        peer.audio(&sine(320));
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    heard.samples(640);
    assert!(heard.errors.try_recv().is_err());
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_suspended_stream_waits_without_stalling() {
    let (server, thread) = serve(|peer| {
        peer.open();
        peer.audio(&sine(320));
        let suspend = |suspended| {
            reference::Command::RecordStreamSuspended(reference::StreamSuspendedParams {
                stream_index: 0,
                suspended,
            })
        };
        peer.event(suspend(true));
        thread::sleep(QUICK.stall * 3);
        peer.event(suspend(false));
        peer.audio(&sine(320));
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    heard.samples(640);
    assert!(heard.errors.try_recv().is_err(), "a stall while suspended");
    let stalled = Instant::now();
    assert_eq!(heard.error().kind(), ErrorKind::Stalled);
    assert!(stalled.elapsed() >= QUICK.stall / 2);
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_silent_server_stalls() {
    let (server, thread) = serve(|peer| {
        peer.open();
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let error = heard.error();
    assert_eq!(error.kind(), ErrorKind::Stalled);
    assert_eq!(
        error.to_string(),
        "the sound server sent no audio for 0.3 s"
    );
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_connection_closed_mid_packet_is_disconnected() {
    let (server, thread) = serve(|peer| {
        peer.open();
        let mut header = 1280u32.to_be_bytes().to_vec();
        header.extend_from_slice(&[0; 16]);
        header.extend_from_slice(&[0; 100]);
        peer.socket.write_all(&header).unwrap();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    assert_eq!(heard.error().kind(), ErrorKind::Disconnected);
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn an_oversized_packet_is_refused() {
    let (server, thread) = serve(|peer| {
        peer.open();
        let mut header = 0xffff_fff0u32.to_be_bytes().to_vec();
        header.extend_from_slice(&[0; 16]);
        peer.socket.write_all(&header).unwrap();
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let error = heard.error();
    assert_eq!(error.kind(), ErrorKind::Protocol);
    assert!(error.to_string().contains("4294967280 bytes"), "{error}");
    drop(opened);
    thread.join().unwrap();
}

#[test]
fn a_packet_cut_by_a_deadline_stays_buffered() {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let mut connection = Connection {
        socket,
        buffer: vec![0; 16],
        start: 0,
        end: 0,
        version: VERSION,
        next_sequence: 0,
        timeouts: QUICK,
        deadline: Instant::now(),
    };
    let soon = || Instant::now() + Duration::from_millis(50);
    let mut packet = 40u32.to_be_bytes().to_vec();
    packet.extend_from_slice(&7u32.to_be_bytes());
    packet.extend_from_slice(&[0; 12]);
    packet.extend((0..40).map(|n| n as u8));
    peer.write_all(&packet[..30]).unwrap();
    assert!(connection.read_packet(soon()).unwrap().is_none());
    peer.write_all(&packet[30..]).unwrap();
    peer.write_all(&packet).unwrap();
    for _ in 0..2 {
        let read = connection.read_packet(soon()).unwrap().unwrap();
        assert_eq!(read.channel, 7);
        assert_eq!(read.body, &packet[HEADER..]);
    }
    assert!(connection.read_packet(soon()).unwrap().is_none());
}

#[test]
fn work_that_misses_its_deadline_is_bounded() {
    static WAITING: AtomicUsize = AtomicUsize::new(0);
    let slow = || thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    for _ in 0..MAX_WAITING {
        let deadline = Instant::now() + Duration::from_millis(20);
        assert_eq!(finish_by(deadline, &WAITING, slow), Err(Late::Deadline));
    }
    assert!(started.elapsed() < Duration::from_millis(400));
    let deadline = Instant::now() + Duration::from_secs(1);
    assert_eq!(finish_by(deadline, &WAITING, slow), Err(Late::Crowded));
    thread::sleep(Duration::from_millis(700));
    assert_eq!(WAITING.load(Ordering::SeqCst), 0);
    assert_eq!(
        finish_by(Instant::now() + Duration::from_secs(1), &WAITING, || 7),
        Ok(7)
    );
}

/// Sends audio until the client goes away.
fn stream_until_closed(peer: &mut Peer) {
    peer.open();
    let piece = sine(320);
    while peer.audio(&piece) {
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn dropping_waits_for_a_callback_in_progress() {
    let (server, thread) = serve(stream_until_closed);
    let calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let (entered, first_call) = mpsc::channel();
    let on_audio = {
        let (calls, finished) = (Arc::clone(&calls), Arc::clone(&finished));
        move |_: &[f32], _: u32| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = entered.send(());
                thread::sleep(Duration::from_millis(300));
            }
            finished.fetch_add(1, Ordering::SeqCst);
        }
    };
    let opened = open_with(
        &server,
        &Options::default(),
        Box::new(on_audio),
        Box::new(|_| {}),
    )
    .unwrap();
    first_call.recv_timeout(LONG).unwrap();
    drop(opened);
    let after_drop = calls.load(Ordering::SeqCst);
    assert_eq!(
        finished.load(Ordering::SeqCst),
        after_drop,
        "drop returned during a callback"
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        after_drop,
        "a callback after drop"
    );
    thread.join().unwrap();
}

#[test]
fn dropping_from_a_callback_returns() {
    let (server, thread) = serve(stream_until_closed);
    let (handover, slot) = mpsc::channel::<Opened>();
    let (dropped, done) = mpsc::channel();
    let on_audio = move |_: &[f32], _: u32| {
        if let Ok(opened) = slot.try_recv() {
            drop(opened);
            let _ = dropped.send(());
        }
    };
    let opened = open_with(
        &server,
        &Options::default(),
        Box::new(on_audio),
        Box::new(|_| {}),
    )
    .unwrap();
    handover.send(opened).unwrap();
    done.recv_timeout(LONG).unwrap();
    thread.join().unwrap();
}

#[test]
fn devices_leave_monitors_out() {
    let (server, thread) = serve(|peer| {
        peer.log_in();
        let (sequence, _) = peer.expect(command::GET_SOURCE_INFO_LIST);
        peer.replay(fixture!("source-info-list-reply"), sequence);
        peer.wait_for_close();
    });
    let devices = devices_with(&server, "Tester").unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, MICROPHONE);
    assert!(devices[0].is_default);
    thread.join().unwrap();
}

#[test]
fn a_missing_socket_is_no_server() {
    let server = Server {
        paths: vec![std::env::temp_dir().join("capta-test-missing/native")],
        cookie: vec![0; 256],
        timeouts: QUICK,
    };
    assert_eq!(
        devices_with(&server, "Tester").unwrap_err().kind(),
        ErrorKind::NoServer
    );
}

#[test]
fn a_capture_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<crate::Capture>();
}

/// Sends the header of a 100-byte packet on `channel`, then one byte every 100 ms.
fn trickle(peer: &mut Peer, channel: u32) {
    let mut header = 100u32.to_be_bytes().to_vec();
    header.extend_from_slice(&channel.to_be_bytes());
    header.extend_from_slice(&[0; 12]);
    peer.socket.write_all(&header).unwrap();
    for _ in 0..9 {
        thread::sleep(Duration::from_millis(100));
        if peer.socket.write_all(&[0]).is_err() {
            break;
        }
    }
}

#[test]
fn a_reply_trickled_past_the_deadline_times_out() {
    let (server, thread) = serve(|peer| {
        peer.expect(command::AUTH);
        trickle(peer, CONTROL);
    });
    let started = Instant::now();
    let error = open_fake(&server).err().unwrap();
    let elapsed = started.elapsed();
    thread.join().unwrap();
    assert_eq!(error.kind(), ErrorKind::Stalled, "{error}");
    assert!(elapsed < QUICK.open * 2, "took {elapsed:?}");
}

#[test]
fn audio_trickled_past_the_stall_allowance_stalls() {
    let (server, thread) = serve(|peer| {
        peer.open();
        trickle(peer, 0);
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let error = heard.errors.recv_timeout(QUICK.stall * 2);
    drop(opened);
    thread.join().unwrap();
    assert_eq!(error.expect("a stall").kind(), ErrorKind::Stalled);
}

#[test]
fn moves_without_audio_still_stall() {
    let (server, thread) = serve(|peer| {
        peer.open();
        for _ in 0..9 {
            let mut w = Writer::default();
            w.u32(command::RECORD_STREAM_MOVED).u32(u32::MAX);
            w.u32(0).u32(60).string(Some("mic")).bool(false);
            if !peer.try_send(CONTROL, &w.bytes) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let error = heard.errors.recv_timeout(QUICK.stall * 2);
    drop(opened);
    thread.join().unwrap();
    assert_eq!(error.expect("a stall").kind(), ErrorKind::Stalled);
}

#[test]
fn opening_has_one_deadline() {
    let (server, thread) = serve(|peer| {
        for command in [
            command::AUTH,
            command::SET_CLIENT_NAME,
            command::GET_SERVER_INFO,
        ] {
            let mut header = [0; HEADER];
            if peer.socket.read_exact(&mut header).is_err() {
                return;
            }
            let mut body = vec![0; be_u32(&header) as usize];
            peer.socket.read_exact(&mut body).unwrap();
            let sequence = Reader::new(&body[5..]).u32().unwrap();
            thread::sleep(QUICK.open * 2 / 3);
            let recorded = match command {
                command::AUTH => fixture!("auth-reply"),
                command::SET_CLIENT_NAME => fixture!("client-name-reply"),
                _ => fixture!("server-info-reply"),
            };
            let mut packet = recorded.to_vec();
            packet[HEADER + 6..HEADER + 10].copy_from_slice(&sequence.to_be_bytes());
            if peer.socket.write_all(&packet).is_err() {
                return;
            }
        }
    });
    let started = Instant::now();
    let error = devices_with(&server, "Tester").unwrap_err();
    let elapsed = started.elapsed();
    thread.join().unwrap();
    assert_eq!(error.kind(), ErrorKind::Stalled, "{error}");
    assert!(elapsed < QUICK.open * 3 / 2, "took {elapsed:?}");
}
#[test]
fn empty_packets_are_refused() {
    let (server, thread) = serve(|peer| {
        peer.open();
        for _ in 0..9 {
            if !peer.try_send(0, &[]) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let error = heard.errors.recv_timeout(QUICK.stall * 2);
    drop(opened);
    thread.join().unwrap();
    assert_eq!(error.expect("an error").kind(), ErrorKind::Protocol);
}

#[test]
fn a_changed_rate_is_refused() {
    let (server, thread) = serve(|peer| {
        peer.open();
        peer.wait_for_close();
    });
    let options = Options {
        sample_rate: 48_000,
        ..Options::default()
    };
    let result = open_with(&server, &options, Box::new(|_, _| {}), Box::new(|_| {}));
    let kind = result.as_ref().err().map(Error::kind);
    drop(result);
    thread.join().unwrap();
    assert_eq!(kind, Some(ErrorKind::Protocol));
}

#[test]
fn the_control_channel_is_refused_for_audio() {
    let (server, thread) = serve(|peer| {
        peer.log_in();
        let (sequence, _) = peer.expect(command::GET_SOURCE_INFO);
        peer.replay(fixture!("source-info-reply"), sequence);
        let (sequence, _) = peer.expect(command::CREATE_RECORD_STREAM);
        let mut reply = fixture!("create-record-stream-reply").to_vec();
        reply[HEADER + 11..HEADER + 15].copy_from_slice(&CONTROL.to_be_bytes());
        peer.replay(&reply, sequence);
        peer.wait_for_close();
    });
    let result = open_fake(&server);
    let kind = result.as_ref().err().map(Error::kind);
    drop(result);
    thread.join().unwrap();
    assert_eq!(kind, Some(ErrorKind::Protocol));
}

/// Sends one sample in a packet with the given offset and flags, and returns what the
/// capture heard.
fn send_framed(
    offset: u64,
    flags: u32,
) -> (Result<Vec<f32>, mpsc::RecvTimeoutError>, Option<Error>) {
    let (server, thread) = serve(move |peer| {
        peer.open();
        let mut packet = 4u32.to_be_bytes().to_vec();
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&offset.to_be_bytes());
        packet.extend_from_slice(&flags.to_be_bytes());
        packet.extend_from_slice(&0.5f32.to_le_bytes());
        peer.socket.write_all(&packet).unwrap();
        peer.wait_for_close();
    });
    let (opened, heard) = open_fake(&server).unwrap();
    let audio = heard.audio.recv_timeout(QUICK.stall);
    let error = heard.errors.recv_timeout(QUICK.stall).ok();
    drop(opened);
    thread.join().unwrap();
    (audio, error)
}

#[test]
fn audio_with_a_seek_is_refused() {
    let (audio, error) = send_framed(4, 0);
    assert!(audio.is_err(), "delivered {audio:?}");
    assert_eq!(error.map(|error| error.kind()), Some(ErrorKind::Protocol));
}

#[test]
fn shared_memory_frames_are_refused() {
    for flags in [0x4000_0000, 0x8000_0000, 0xc000_0000, 0x2000_0000, 1] {
        let (audio, error) = send_framed(0, flags);
        assert!(audio.is_err(), "flags {flags:#x} delivered {audio:?}");
        assert_eq!(
            error.map(|error| error.kind()),
            Some(ErrorKind::Protocol),
            "flags {flags:#x}"
        );
    }
}

#[test]
fn control_packets_with_flags_are_refused() {
    let (server, thread) = serve(|peer| {
        let (sequence, _) = peer.expect(command::AUTH);
        let mut packet = fixture!("auth-reply").to_vec();
        packet[HEADER + 6..HEADER + 10].copy_from_slice(&sequence.to_be_bytes());
        packet[16..20].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        peer.socket.write_all(&packet).unwrap();
    });
    let error = open_fake(&server).err().unwrap();
    thread.join().unwrap();
    assert_eq!(error.kind(), ErrorKind::Protocol, "{error}");
}
