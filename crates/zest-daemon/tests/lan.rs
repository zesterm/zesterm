//! Serving the LAN, over a real TCP socket on loopback.
//!
//! The cheapest transport tests in the repo — unlike the unix-socket and
//! named-pipe ones they need no per-platform path fixture and behave
//! identically on all three OSes. They are also the only place the *refusal*
//! paths run end to end, which is the half that matters: a listener that serves
//! the right people and also serves the wrong ones is worse than no listener.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zest_daemon::{Authenticator, DaemonConfig, Gate, LanListener, Registry};
use zest_mesh::identity::{ClientIdentity, HostIdentity, Nonce};
use zest_mesh::pairing::{PairingQueue, Transcript};
use zest_mesh::trust::{MemoryTrustStore, TrustRecord, TrustStore};
use zest_proto::{frame, ClientMessage, FrameReader, HostMessage, PROTOCOL_VERSION};

/// The handshake deadline these tests run with.
const HANDSHAKE_TEST_TIMEOUT: Duration = Duration::from_millis(300);

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
        "lan-test",
    ));

    // Short enough that a test can afford to outlive it, which is the only way
    // the watchdog's behaviour can be observed at all.
    let listener = LanListener::bind("127.0.0.1", 0)
        .expect("bind")
        .with_handshake_timeout(HANDSHAKE_TEST_TIMEOUT);
    let addr = listener.local_addr();
    let config = DaemonConfig {
        host: host_id,
        label: "lan-test".into(),
        local_socket: String::new(),
        listen_lan: true,
        lan_bind: "127.0.0.1".into(),
        lan_port: addr.port(),
        listen_ws: false,
        ws_bind: "127.0.0.1".into(),
        ws_port: 0,
        relay: None,
        shell_integration: true,
        min_delta_interval: std::time::Duration::ZERO,
        enroll: None,
        offer: None,
        settings: None,
    };
    let registry = Arc::new(Registry::new());
    std::thread::spawn(move || {
        let _ = listener.serve_forever(config, registry, auth, Arc::new(Gate::new()));
    });

    Host { addr, trust, host_id }
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

/// One client connection: the socket, its framing, and its channel.
///
/// Bundled since protocol 3. They were three separate locals, which was fine
/// while a frame was a frame; now the channel has to travel with the socket and
/// advance in step with it, and a test that held the wrong one would produce
/// frames the host silently cannot open.
struct Peer {
    stream: TcpStream,
    frames: FrameReader,
    channel: Option<zest_mesh::secure::SecureChannel>,
}

impl Peer {
    fn connect(addr: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).expect("connect"),
            frames: FrameReader::new(),
            channel: None,
        }
    }

    fn send(&mut self, msg: &ClientMessage) {
        let bytes = self.frame_for(msg);
        self.write_raw(&bytes);
    }

    /// Serialize and seal, without writing — for a test that wants the bytes.
    fn frame_for(&mut self, msg: &ClientMessage) -> Vec<u8> {
        let body = frame::encode_body(msg).expect("encode");
        let body = match self.channel.as_mut() {
            Some(ch) => ch.seal(&body).expect("seal"),
            None => body,
        };
        frame::frame_bytes(&body).expect("frame")
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write");
        self.stream.flush().expect("flush");
    }

    fn wait_for(&mut self, f: impl FnMut(&HostMessage) -> bool) -> Option<HostMessage> {
        wait_for(&mut self.stream, &mut self.frames, self.channel.as_mut(), f)
    }
}

fn wait_for(
    stream: &mut TcpStream,
    frames: &mut FrameReader,
    mut channel: Option<&mut zest_mesh::secure::SecureChannel>,
    mut f: impl FnMut(&HostMessage) -> bool,
) -> Option<HostMessage> {
    let mut buf = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + Duration::from_secs(10);
    // Without a read timeout the deadline below is decorative: a host that
    // keeps the connection open but never sends the awaited frame parks the
    // read forever, and the whole suite hangs instead of going red. Windows CI
    // sat like that for over an hour, twice, with nothing to read out of it.
    stream.set_read_timeout(Some(Duration::from_millis(200))).expect("set read timeout");
    loop {
        while let Ok(Some(body)) = frames.next_frame() {
            let body = match channel.as_mut() {
                Some(ch) => match ch.open(&body) {
                    Ok(plain) => plain,
                    // Not a panic: several tests below are about a host that
                    // refuses, and a refusal it cannot seal is a connection
                    // that simply ends.
                    Err(_) => return None,
                },
                None => body,
            };
            let msg = frame::decode::<HostMessage>(&body).expect("decode");
            if f(&msg) {
                return Some(msg);
            }
        }
        if Instant::now() > deadline {
            return None;
        }
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => frames.feed(&buf[..n]),
            // Unix reports an expired read timeout as WouldBlock, Windows as
            // TimedOut; both mean "nothing yet", not "nothing ever".
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return None,
        }
    }
}

/// Say hello and answer the challenge. Returns the `Auth` message and the
/// sealed frame it went out as, so a test can replay either.
fn handshake(peer: &mut Peer, identity: &Arc<ClientIdentity>) -> (ClientMessage, Vec<u8>) {
    // The shared client half, not a hand-rolled one. A test peer with its own
    // idea of the transcript would fail every frame *after* the handshake,
    // which reads as a broken daemon rather than a broken test.
    let mut hs = zest_mesh::pairing::ClientHandshake::new(Arc::clone(identity), "test-device")
        .expect("client handshake");
    peer.send(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: identity.client_id(),
        label: "test-device".into(),
        nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
        dh: zest_proto::Pub32::from_bytes(hs.dh().0),
        watch_sessions: false,
        watch_pairings: false,
        watch_hosts: false,
        watch_signals: false,
        agent: false,
    });

    let challenge =
        peer.wait_for(|m| matches!(m, HostMessage::Challenge { .. })).expect("no challenge arrived");
    let HostMessage::Challenge { version, host, label, nonce, dh, signature } = challenge else {
        unreachable!()
    };
    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0).expect("signature");
    let (sig, _, channel) = hs
        .on_challenge(
            None,
            &zest_mesh::pairing::Challenge {
                version,
                host,
                label,
                nonce: Nonce::from_bytes(nonce.0),
                dh: zest_mesh::secure::DhPublic(dh.0),
                signature: host_sig,
            },
        )
        .expect("the host must prove itself");

    peer.channel = Some(channel);
    let msg = ClientMessage::Auth { signature: zest_proto::Sig64::from_bytes(sig.to_bytes()) };
    let bytes = peer.frame_for(&msg);
    peer.write_raw(&bytes);
    (msg, bytes)
}

#[test]
fn an_unpaired_device_is_not_served_and_gets_no_session() {
    // "Refused" alone is not the property. A host that says no and starts a
    // shell anyway has still handed one out.
    let h = host();
    let mut stream = Peer::connect(h.addr);
    let client = Arc::new(ClientIdentity::generate().expect("client key"));

    handshake(&mut stream, &client);

    let answer = stream.wait_for(|m| {
        matches!(m, HostMessage::AuthPending { .. } | HostMessage::Welcome { .. })
    })
    .expect("no answer");
    assert!(
        matches!(answer, HostMessage::AuthPending { .. }),
        "an unpaired device was welcomed: {answer:?}"
    );

    stream.send(&ClientMessage::ListSessions);
    let answer = stream.wait_for(|m| {
        matches!(m, HostMessage::Sessions { .. } | HostMessage::Error { .. })
    })
    .expect("no answer");
    assert!(
        matches!(answer, HostMessage::Error { .. }),
        "a device waiting for approval was served: {answer:?}"
    );
}

#[test]
fn a_paired_device_drives_a_session_over_tcp() {
    let h = host();
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);

    let mut stream = Peer::connect(h.addr);
    handshake(&mut stream, &client);

    let welcome = stream.wait_for(|m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");
    let HostMessage::Welcome { host, .. } = welcome else { unreachable!() };
    assert_eq!(host, h.host_id, "the host announced an identity that is not its own");

    // Platform-aware like every other daemon test: this file is the one place
    // that forgot, and on Windows the failed spawn came back as an `Error`
    // frame the old `wait_for` filter ignored -- a silent hang, not a red test.
    let cmd = if cfg!(windows) {
        "cmd.exe /c echo over-the-lan"
    } else {
        "/bin/echo over-the-lan"
    };
    stream.send(
        &ClientMessage::CreateSession {
            command: cmd.into(),
            cwd: String::new(),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        },
    );
    // Accept `Error` too, so a spawn failure names itself instead of timing out.
    let listing = stream.wait_for(|m| {
        matches!(m, HostMessage::Sessions { .. } | HostMessage::Error { .. })
    })
    .expect("no listing");
    let HostMessage::Sessions { sessions, .. } = listing else {
        panic!("the session was refused: {listing:?}");
    };
    assert_eq!(sessions.len(), 1, "the session was not created");
}

#[test]
fn a_captured_proof_cannot_be_replayed_onto_a_second_connection() {
    // Why the host contributes a nonce, and why there is no nonce cache
    // anywhere: the host's nonce is per-connection state, so the connection
    // *is* the replay window. Entirely doable in-process, which is what makes
    // this a test rather than a claim in a comment.
    let h = host();
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);

    let mut first = Peer::connect(h.addr);
    let (auth_msg, captured) = handshake(&mut first, &client);
    assert!(
        first.wait_for(|m| matches!(m, HostMessage::Welcome { .. })).is_some(),
        "the first connection was not served"
    );

    // Replayed *inside a valid channel*, which is what still tests the property
    // this test is named for. Protocol 3 would refuse the captured ciphertext
    // at the seal, and that refusal proves nothing about nonce binding — it
    // would pass just as happily if the signature covered no nonce at all. So
    // the second connection runs its own handshake, gets its own key, and seals
    // the *same signature* under it. Only the host nonce differs, and only the
    // signature can notice.
    let mut second = Peer::connect(h.addr);
    let mut hs = zest_mesh::pairing::ClientHandshake::new(Arc::clone(&client), "test-device")
        .expect("client handshake");
    second.send(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: client.client_id(),
        label: "test-device".into(),
        nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
        dh: zest_proto::Pub32::from_bytes(hs.dh().0),
        watch_sessions: false,
        watch_pairings: false,
        watch_hosts: false,
        watch_signals: false,
        agent: false,
    });
    let challenge = second
        .wait_for(|m| matches!(m, HostMessage::Challenge { .. }))
        .expect("no challenge on the second connection");
    let HostMessage::Challenge { version, host, label, nonce, dh, signature } = challenge else {
        unreachable!()
    };
    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0).expect("signature");
    let (_, _, channel) = hs
        .on_challenge(
            None,
            &zest_mesh::pairing::Challenge {
                version,
                host,
                label,
                nonce: Nonce::from_bytes(nonce.0),
                dh: zest_mesh::secure::DhPublic(dh.0),
                signature: host_sig,
            },
        )
        .expect("the host must prove itself");
    second.channel = Some(channel);
    second.send(&auth_msg);

    let answer = second
        .wait_for(|m| matches!(m, HostMessage::Welcome { .. } | HostMessage::AuthFailed { .. }))
        .expect("no answer");
    assert!(
        matches!(
            answer,
            HostMessage::AuthFailed { reason: zest_proto::AuthFailure::Signature, .. }
        ),
        "a replayed proof was accepted: {answer:?}"
    );

    // And the cruder replay — the captured bytes verbatim — is refused earlier
    // still, at the seal, with the connection simply ending. Asserted so the
    // two failures are known to be different things.
    let mut third = Peer::connect(h.addr);
    third.send(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: client.client_id(),
        label: "test-device".into(),
        nonce: zest_proto::Nonce32::from_bytes([0x5c; 32]),
        dh: zest_proto::Pub32::from_bytes([0x11; 32]),
        watch_sessions: false,
        watch_pairings: false,
        watch_hosts: false,
        watch_signals: false,
        agent: false,
    });
    assert!(
        third.wait_for(|m| matches!(m, HostMessage::Challenge { .. })).is_some(),
        "no challenge on the third connection"
    );
    third.write_raw(&captured);
    assert!(
        third.wait_for(|m| matches!(m, HostMessage::Welcome { .. })).is_none(),
        "a captured frame from another connection was served"
    );
}

/// Longer than the test-mode handshake timeout, so a connection that survives
/// this really did outlive the watchdog.
const PAST_THE_WATCHDOG: Duration = Duration::from_millis(900);

#[test]
fn an_authenticated_connection_outlives_the_handshake_watchdog() {
    // The bug this exists for: the watchdog's flag was only set after `serve`
    // returned -- i.e. once the connection had already ended -- so it cut every
    // connection, healthy or not, a fixed time after accept. A paired phone was
    // disconnected on that cycle forever.
    //
    // Every previous LAN test finished in milliseconds, which is exactly why
    // none of them noticed. This one deliberately waits.
    let h = host();
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);

    let mut stream = Peer::connect(h.addr);
    handshake(&mut stream, &client);
    stream.wait_for(|m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");

    std::thread::sleep(PAST_THE_WATCHDOG);

    // Still usable: the watchdog must not have touched an authenticated
    // connection.
    stream.send(&ClientMessage::ListSessions);
    let answer = stream.wait_for(|m| {
        matches!(m, HostMessage::Sessions { .. })
    });
    assert!(
        answer.is_some(),
        "the connection was cut after the handshake timeout despite being authenticated"
    );
}

/// A device waiting to be approved must outlive the handshake timeout.
///
/// `an_authenticated_connection_outlives_the_handshake_watchdog` above proves a
/// *welcomed* connection is left alone — and a device waiting for approval is
/// precisely the one that has **not** been welcomed. So every LAN pairing
/// attempt was cut at the handshake timeout, ten seconds into a window the host
/// itself advertises as 120, and the whole suite agreed that was correct.
///
/// Found by two machines on a real network: the Mac dialled the Windows box,
/// both screens showed the same six digits, and the socket vanished before
/// anyone could say yes.
#[test]
fn a_device_waiting_for_approval_outlives_the_handshake_watchdog() {
    let h = host();
    // Deliberately NOT trusted: being unknown is what sends this connection
    // down the approval path instead of straight to a welcome.
    let client = Arc::new(ClientIdentity::generate().expect("client key"));

    let mut stream = Peer::connect(h.addr);
    handshake(&mut stream, &client);

    let pending = stream.wait_for(|m| {
        matches!(m, HostMessage::AuthPending { .. })
    });
    assert!(pending.is_some(), "an unknown device should be asked about, not refused");

    std::thread::sleep(PAST_THE_WATCHDOG);

    // The connection must still be there for a person to approve. A read that
    // returns EOF means the host hung up on someone it had just asked to wait.
    stream.stream.set_read_timeout(Some(Duration::from_millis(300))).expect("timeout");
    let mut buf = [0u8; 16];
    // A timeout is the expected outcome: the host is waiting, quietly, for a
    // decision this test never makes. EOF is the failure.
    if let Ok(0) = stream.stream.read(&mut buf) {
        panic!(
            "the host cut a connection it had just told to wait -- pairing over \
             the LAN is impossible, because nobody can answer a prompt in less \
             than the handshake timeout"
        );
    }
}

#[test]
fn a_connection_that_never_speaks_is_cut() {
    // The other half: the watchdog must still do its job. Without this, the
    // fix above could be "never arm it" and both tests would pass.
    let h = host();
    let mut silent = TcpStream::connect(h.addr).expect("connect");

    std::thread::sleep(PAST_THE_WATCHDOG);

    // The daemon shut the socket down, so a read returns EOF rather than
    // blocking forever.
    silent.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut buf = [0u8; 16];
    // `unwrap_or(0)` here made this test unfailable, and that is not a nitpick:
    // the read timeout above turns "the daemon never cut anything" into an
    // `Err`, which that maps to 0 — the same value the passing case asserts. A
    // test whose failure mode and success mode are the same assertion is not a
    // test, and this one sat green through the whole of issue #99.
    assert_eq!(
        silent.read(&mut buf).expect("the connection was still open when the read timed out"),
        0,
        "a connection that never spoke was left open"
    );

    // **This is only half of the cut, and the easy half.** `shutdown` reaches
    // the *client*, so everything above passes on every platform — it did on
    // Windows throughout #99, while the daemon's own reader stayed parked in
    // `read` and the mid-handshake slot it held was never given back.
    //
    // The other half — that the slot returns to the `Gate` — deliberately lives
    // in `lan.rs`'s unit tests rather than here, and the reason is worth
    // writing down: filling the cap over loopback cannot work, because every
    // connection from `127.0.0.1` shares one `PeerKey`, so 32 silent ones
    // settle as 32 *failed handshakes* and the per-peer rate limiter refuses
    // the test's own client long before the cap is the reason for anything.
    // Measured, not assumed — at 4 it passes and at 32 it fails on the limiter,
    // and the better the watchdog works the more surely it fails, since a slot
    // only comes back by way of a `settle(_, false)`.
}

#[test]
fn a_silent_connection_does_not_block_a_real_one() {
    let h = host();
    let _silent = TcpStream::connect(h.addr).expect("connect");

    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);
    let mut stream = Peer::connect(h.addr);
    handshake(&mut stream, &client);
    assert!(
        stream.wait_for(|m| matches!(m, HostMessage::Welcome { .. })).is_some(),
        "a connection that said nothing blocked a real one"
    );
}

#[test]
fn a_remote_device_may_not_approve_other_devices() {
    // Refused even for a *paired* client: otherwise one compromised device
    // enrols the rest.
    let h = host();
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);

    let mut stream = Peer::connect(h.addr);
    handshake(&mut stream, &client);
    stream.wait_for(|m| matches!(m, HostMessage::Welcome { .. }))
        .expect("no welcome");

    stream.send(
        &ClientMessage::PairingDecision {
            client: zest_proto::ClientId::from_bytes([0xaa; 32]),
            approve: true,
        },
    );
    assert!(
        stream.wait_for(|m| matches!(m, HostMessage::Error { .. })).is_some(),
        "a remote device was allowed to approve another"
    );
}

#[test]
fn a_client_that_dialled_another_host_notices() {
    // What the host signing first buys. An address learned from an mDNS
    // advertisement is a claim; without this check "connect to my Mac" means
    // "connect to whatever answered on that port".
    let h = host();
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    trust(&h, &client);

    let mut stream = Peer::connect(h.addr);
    let hs = zest_mesh::pairing::ClientHandshake::new(Arc::clone(&client), "test-device")
        .expect("client handshake");
    let client_dh = hs.dh();
    stream.send(&ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        client: client.client_id(),
        label: "test-device".into(),
        nonce: zest_proto::Nonce32::from_bytes(*hs.nonce().as_bytes()),
        dh: zest_proto::Pub32::from_bytes(client_dh.0),
        watch_sessions: false,
        watch_pairings: false,
        watch_hosts: false,
        watch_signals: false,
        agent: false,
    });
    let challenge =
        stream.wait_for(|m| matches!(m, HostMessage::Challenge { .. })).expect("no challenge");
    let HostMessage::Challenge { version, host, label, nonce, dh, signature } = challenge else {
        unreachable!()
    };

    // Rebuilt by hand rather than through `on_challenge`, because this test is
    // about `verify_challenge` answering two different questions about the same
    // signature — which `on_challenge` deliberately does not expose.
    let transcript = Transcript {
        version,
        host,
        client: client.client_id(),
        host_nonce: Nonce::from_bytes(nonce.0),
        client_nonce: hs.nonce(),
        host_label: label,
        client_label: "test-device".into(),
        host_dh: dh.0,
        client_dh: client_dh.0,
    };
    let host_sig = zest_mesh::identity::Signature::from_slice(&signature.0).expect("signature");

    // The host proves itself to a client that dialled it...
    assert!(zest_mesh::pairing::verify_challenge(Some(h.host_id), &transcript, &host_sig).is_ok());
    // ...and cannot prove itself to one that meant to reach someone else.
    assert!(
        zest_mesh::pairing::verify_challenge(
            Some(zest_proto::HostId::from_bytes([0xee; 32])),
            &transcript,
            &host_sig,
        )
        .is_err(),
        "a client accepted a host it did not dial"
    );
}
