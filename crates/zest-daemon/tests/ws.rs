//! Serving browsers, over a real WebSocket on loopback.
//!
//! The client here is `tungstenite` — an independent RFC 6455 implementation —
//! so the hand-rolled server in `ws.rs` is proven against the RFC rather than
//! against a mirror of itself. The protocol inside the frames is untouched by
//! the transport, which these tests confirm by running the same handshake and
//! refusal paths `tests/lan.rs` runs over raw TCP.

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use zest_daemon::{Authenticator, DaemonConfig, Registry, WsListener};
use zest_mesh::identity::{ClientIdentity, HostIdentity, Nonce, Purpose};
use zest_mesh::pairing::{auth_transcript, PairingQueue, Transcript};
use zest_mesh::trust::{MemoryTrustStore, TrustRecord, TrustStore};
use zest_proto::{frame, ClientMessage, FrameReader, HostMessage, PROTOCOL_VERSION};

const HANDSHAKE_TEST_TIMEOUT: Duration = Duration::from_millis(300);

/// Longer than the test-mode handshake timeout, so a connection that survives
/// this really did outlive the watchdog.
const PAST_THE_WATCHDOG: Duration = Duration::from_millis(900);

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

struct Host {
    addr: std::net::SocketAddr,
    trust: Arc<MemoryTrustStore>,
    host_id: zest_proto::HostId,
}

fn host() -> Host {
    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let host_id = identity.host_id();
    let trust = Arc::new(MemoryTrustStore::new());
    let auth = Arc::new(Authenticator::new(
        Arc::clone(&identity),
        Arc::clone(&trust) as Arc<dyn TrustStore>,
        PairingQueue::new(),
        "ws-test",
    ));

    let listener = WsListener::bind("127.0.0.1", 0)
        .expect("bind")
        .with_handshake_timeout(HANDSHAKE_TEST_TIMEOUT);
    let addr = listener.local_addr();
    let config = DaemonConfig {
        host: host_id,
        label: "ws-test".into(),
        local_socket: String::new(),
        listen_lan: false,
        lan_bind: "127.0.0.1".into(),
        lan_port: 0,
        listen_ws: true,
        ws_bind: "127.0.0.1".into(),
        ws_port: addr.port(),
        shell_integration: true,
    };
    let registry = Arc::new(Registry::new());
    std::thread::spawn(move || {
        let _ = listener.serve_forever(config, registry, auth);
    });

    Host { addr, trust, host_id }
}

fn dial(h: &Host) -> Ws {
    let (ws, _) = tungstenite::connect(format!("ws://{}", h.addr)).expect("connect + upgrade");
    // Without a read timeout the deadlines below are decorative — same lesson
    // tests/lan.rs already paid for on Windows CI.
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_millis(200))).expect("read timeout");
    }
    ws
}

fn trust(h: &Host, client: &ClientIdentity) {
    h.trust
        .insert(TrustRecord {
            client: client.client_id(),
            label: "known".into(),
            paired_at: std::time::SystemTime::now(),
            last_seen: None,
        })
        .expect("insert");
}

fn send(ws: &mut Ws, msg: &ClientMessage) {
    let bytes = frame::encode(msg).expect("encode");
    ws.send(Message::Binary(bytes.into())).expect("send");
}

/// Read WebSocket messages, feed every binary payload through the *same
/// streaming FrameReader a browser runs*, and return the first decoded message
/// the filter accepts.
fn wait_for(
    ws: &mut Ws,
    frames: &mut FrameReader,
    mut f: impl FnMut(&HostMessage) -> bool,
) -> Option<HostMessage> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Ok(Some(body)) = frames.next_frame() {
            let msg = frame::decode::<HostMessage>(&body).expect("decode");
            if f(&msg) {
                return Some(msg);
            }
        }
        if Instant::now() > deadline {
            return None;
        }
        match ws.read() {
            Ok(Message::Binary(payload)) => frames.feed(&payload),
            // Control frames are the transport's business, not the protocol's.
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
}

/// Say hello and answer the challenge, exactly as over TCP — the transport
/// must not change one byte of the protocol.
fn handshake(ws: &mut Ws, frames: &mut FrameReader, identity: &ClientIdentity) {
    let client_nonce = Nonce::from_bytes([0x5c; 32]);
    send(
        ws,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            client: identity.client_id(),
            label: "ws-device".into(),
            nonce: zest_proto::Nonce32::from_bytes(*client_nonce.as_bytes()),
            watch_sessions: false,
        },
    );

    let challenge =
        wait_for(ws, frames, |m| matches!(m, HostMessage::Challenge { .. })).expect("no challenge");
    let HostMessage::Challenge { version, host, label, nonce, .. } = challenge else {
        unreachable!()
    };

    let transcript = Transcript {
        version,
        host,
        client: identity.client_id(),
        host_nonce: Nonce::from_bytes(nonce.0),
        client_nonce,
        host_label: label,
        client_label: "ws-device".into(),
    };
    let sig = identity.sign(Purpose::Auth, &auth_transcript(&transcript));
    send(ws, &ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes(sig.to_bytes()) });
}

#[test]
fn a_paired_device_drives_a_session_over_websocket() {
    // The whole point of the transport: hello, challenge, auth, welcome, a
    // session created and attached, a keyframe painted — over a socket a
    // browser could have opened.
    let h = host();
    let client = ClientIdentity::generate().expect("client key");
    trust(&h, &client);

    let mut ws = dial(&h);
    let mut frames = FrameReader::new();
    handshake(&mut ws, &mut frames, &client);

    let welcome = wait_for(&mut ws, &mut frames, |m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");
    let HostMessage::Welcome { host, .. } = welcome else { unreachable!() };
    assert_eq!(host, h.host_id, "the host announced an identity that is not its own");

    let cmd = if cfg!(windows) { "cmd.exe /c echo over-websocket" } else { "/bin/echo over-websocket" };
    send(
        &mut ws,
        &ClientMessage::CreateSession {
            command: cmd.into(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
        },
    );
    let listing = wait_for(&mut ws, &mut frames, |m| {
        matches!(m, HostMessage::Sessions { .. } | HostMessage::Error { .. })
    })
    .expect("no listing");
    let HostMessage::Sessions { sessions, .. } = listing else {
        panic!("the session was refused: {listing:?}");
    };
    assert_eq!(sessions.len(), 1, "the session was not created");

    send(&mut ws, &ClientMessage::Attach { session: sessions[0].addr, cols: 80, rows: 24 });
    let keyframe = wait_for(&mut ws, &mut frames, |m| matches!(m, HostMessage::Keyframe { .. }));
    assert!(keyframe.is_some(), "every attach starts from a keyframe, on every transport");
}

#[test]
fn an_unpaired_device_is_not_served_over_websocket() {
    // The refusal path is the half that matters, and it must not soften on a
    // transport that faces browsers.
    let h = host();
    let mut ws = dial(&h);
    let mut frames = FrameReader::new();
    let client = ClientIdentity::generate().expect("client key");

    handshake(&mut ws, &mut frames, &client);
    let answer = wait_for(&mut ws, &mut frames, |m| {
        matches!(m, HostMessage::AuthPending { .. } | HostMessage::Welcome { .. })
    })
    .expect("no answer");
    assert!(
        matches!(answer, HostMessage::AuthPending { .. }),
        "an unpaired device was welcomed: {answer:?}"
    );

    send(&mut ws, &ClientMessage::ListSessions);
    let answer = wait_for(&mut ws, &mut frames, |m| {
        matches!(m, HostMessage::Sessions { .. } | HostMessage::Error { .. })
    })
    .expect("no answer");
    assert!(
        matches!(answer, HostMessage::Error { .. }),
        "a device waiting for approval was served: {answer:?}"
    );
}

#[test]
fn a_connection_that_never_sends_http_is_cut() {
    // The watchdog is armed before the upgrade, so a TCP client that opens a
    // socket to the WebSocket port and says nothing is bounded by the same
    // deadline as a silent LAN client.
    let h = host();
    let mut silent = TcpStream::connect(h.addr).expect("connect");

    std::thread::sleep(PAST_THE_WATCHDOG);

    silent.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut buf = [0u8; 16];
    let n = silent.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "a connection that never sent its HTTP request was left open");
}

#[test]
fn a_connection_that_upgrades_but_never_speaks_is_cut() {
    // Finishing the HTTP upgrade is not the handshake. Only `Hello`→`Auth` is,
    // and a peer that stops after the 101 must still be cut.
    let h = host();
    let mut ws = dial(&h);

    std::thread::sleep(PAST_THE_WATCHDOG);

    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    }
    // The daemon shut the socket down; any read now ends in a close or an
    // error rather than blocking forever.
    let outcome = ws.read();
    assert!(
        !matches!(outcome, Ok(Message::Binary(_))),
        "a connection that upgraded and then said nothing was served: {outcome:?}"
    );
}

#[test]
fn a_ping_mid_session_is_answered_and_the_stream_survives() {
    // The interleaving property, end to end: the pong reply and the data
    // frames share one write lock, so a ping must neither stall the session
    // nor corrupt the next reply.
    let h = host();
    let client = ClientIdentity::generate().expect("client key");
    trust(&h, &client);

    let mut ws = dial(&h);
    let mut frames = FrameReader::new();
    handshake(&mut ws, &mut frames, &client);
    wait_for(&mut ws, &mut frames, |m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");

    ws.send(Message::Ping(vec![0xab, 0xcd].into())).expect("ping");
    send(&mut ws, &ClientMessage::ListSessions);

    let mut pong = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    let sessions = loop {
        if Instant::now() > deadline {
            panic!("no reply after a ping");
        }
        match ws.read() {
            Ok(Message::Pong(p)) => pong = Some(p),
            Ok(Message::Binary(payload)) => {
                frames.feed(&payload);
                if let Ok(Some(body)) = frames.next_frame() {
                    break frame::decode::<HostMessage>(&body).expect("decode");
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("the stream died after a ping: {e}"),
        }
    };
    assert_eq!(pong.as_deref(), Some(&[0xab, 0xcd][..]), "the ping was not answered in kind");
    assert!(
        matches!(sessions, HostMessage::Sessions { .. }),
        "the reply after a ping did not decode: {sessions:?}"
    );
}

#[test]
fn several_protocol_frames_share_one_websocket_message() {
    // Both halves of the byte-pipe mapping at once. Inbound: two zest-proto
    // frames in one WebSocket message must both be processed. Outbound: their
    // replies are produced in one wake, so they coalesce into one WebSocket
    // message carrying two length-prefixed frames — which the browser's
    // streaming FrameReader must split, and this test does split.
    let h = host();
    let client = ClientIdentity::generate().expect("client key");
    trust(&h, &client);

    let mut ws = dial(&h);
    let mut frames = FrameReader::new();
    handshake(&mut ws, &mut frames, &client);
    wait_for(&mut ws, &mut frames, |m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");

    let mut two = frame::encode(&ClientMessage::ListSessions).expect("encode");
    two.extend(frame::encode(&ClientMessage::ListSessions).expect("encode"));
    ws.send(Message::Binary(two.into())).expect("send");

    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut decoded, mut messages) = (0usize, 0usize);
    while decoded < 2 {
        assert!(Instant::now() < deadline, "only {decoded} of 2 replies arrived");
        match ws.read() {
            Ok(Message::Binary(payload)) => {
                messages += 1;
                frames.feed(&payload);
                while let Ok(Some(body)) = frames.next_frame() {
                    let msg = frame::decode::<HostMessage>(&body).expect("decode");
                    assert!(matches!(msg, HostMessage::Sessions { .. }));
                    decoded += 1;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("stream died: {e}"),
        }
    }
    assert_eq!(
        messages, 1,
        "two replies produced in one wake batch must coalesce into one \
         WebSocket message — this is the whole-frames-per-flush contract"
    );
}

#[test]
fn the_daemons_own_client_codec_interoperates() {
    // `examples/attach.rs --ws` uses `ws::client`, which masks its frames —
    // the half of the codec the server-side tests cannot reach. If this
    // handshake completes, client-role masking and server-role unmasking
    // agree.
    let h = host();
    let client = ClientIdentity::generate().expect("client key");
    trust(&h, &client);

    let stream = TcpStream::connect(h.addr).expect("connect");
    let (mut reader, mut writer) = zest_daemon::ws::client::connect(stream).expect("upgrade");

    let client_nonce = Nonce::from_bytes([0x5c; 32]);
    let hello = frame::encode(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: client.client_id(),
        label: "native-ws".into(),
        nonce: zest_proto::Nonce32::from_bytes(*client_nonce.as_bytes()),
        watch_sessions: false,
    })
    .expect("encode");
    std::io::Write::write_all(&mut writer, &hello).expect("write");
    std::io::Write::flush(&mut writer).expect("flush");

    let mut frames = FrameReader::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut next = |reader: &mut dyn Read, frames: &mut FrameReader| -> HostMessage {
        loop {
            assert!(Instant::now() < deadline, "no reply arrived");
            if let Ok(Some(body)) = frames.next_frame() {
                return frame::decode::<HostMessage>(&body).expect("decode");
            }
            match reader.read(&mut buf) {
                Ok(0) => panic!("the server hung up mid-handshake"),
                Ok(n) => frames.feed(&buf[..n]),
                Err(e) => panic!("read failed: {e}"),
            }
        }
    };

    let challenge = next(&mut reader, &mut frames);
    let HostMessage::Challenge { version, host, label, nonce, .. } = challenge else {
        panic!("expected a challenge, got {challenge:?}");
    };

    let transcript = Transcript {
        version,
        host,
        client: client.client_id(),
        host_nonce: Nonce::from_bytes(nonce.0),
        client_nonce,
        host_label: label,
        client_label: "native-ws".into(),
    };
    let sig = client.sign(Purpose::Auth, &auth_transcript(&transcript));
    let auth = frame::encode(&ClientMessage::Auth {
        signature: zest_proto::Sig64::from_bytes(sig.to_bytes()),
    })
    .expect("encode");
    std::io::Write::write_all(&mut writer, &auth).expect("write");
    std::io::Write::flush(&mut writer).expect("flush");

    // All the way to Welcome, deliberately: `Welcome` is a *small* unmasked
    // frame with a two-byte header, which is the shape that underflowed the
    // client-role parser on the first real run. Stopping at the challenge —
    // a large frame with an extended length — proved nothing about it.
    let welcome = next(&mut reader, &mut frames);
    assert!(
        matches!(welcome, HostMessage::Welcome { .. }),
        "the native client codec did not survive the full handshake: {welcome:?}"
    );
}
