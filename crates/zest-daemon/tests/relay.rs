//! The daemon's relay leg, end to end, against a fake relay in this process.
//!
//! The real relay is a Cloudflare Worker, so without something standing in for
//! it the only way to exercise any of this is `wrangler dev`, a certificate and
//! a network — which is to say, never in `cargo test`. What stands in is a
//! **fake relay**, not a fake daemon: the whole of `relay.rs` runs unchanged
//! here, including the real WebSocket client, the real Ed25519 signature over
//! the challenge, the real dial-back on a second socket, and a real
//! `serve_lan` at the end of it carrying a real `zest-proto` session.
//!
//! # Why the far end is `tungstenite`
//!
//! Same argument `tests/ws.rs` makes in the other direction. The relay's
//! WebSocket server is Cloudflare's, so proving our client against our own
//! server would prove only that the two halves of `ws.rs` agree with each
//! other. `tungstenite` is an independent RFC 6455 implementation and disagrees
//! where the RFC says it should — unmasked client frames, a bad `Sec-WebSocket-
//! Accept`, a subprotocol nobody offered.
//!
//! # What is real here and what is not
//!
//! Real: the control handshake, the signature (verified with
//! `zest_mesh::identity::verify_host`, which is what the Worker's
//! `helloSignatureIsValid` is a port of), the second connection, the
//! mid-handshake budget, the backoff, and a `zest-proto` session end to end.
//!
//! Not real: the Durable Object's own rules — enrolment in D1, ticket replay,
//! the pipe id being unguessable. Those are `cloud/packages/relay/test/`'s and
//! are asserted there against the same message shapes.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tungstenite::handshake::server::{Request, Response};
use tungstenite::{Message, WebSocket};

use zest_cloud::tls::Roots;
use zest_daemon::client::DaemonClient;
use zest_daemon::lan::{Countdown, PeerKey, Refused};
use zest_daemon::relay::{plaintext_dialler, Dialler, Wire};
use zest_daemon::{Authenticator, DaemonConfig, Gate, Registry, Relay};
use zest_mesh::identity::{ClientIdentity, HostIdentity, Nonce, Purpose, Signature};
use zest_mesh::pairing::PairingQueue;
use zest_mesh::trust::{MemoryTrustStore, TrustRecord, TrustStore};

/// Long enough that a slow machine is not mistaken for a deadlock, short enough
/// that a hung test is reported rather than waited out.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long the tests wait before concluding that something did **not** happen.
///
/// Only ever used for negative assertions, where the cost of being wrong is a
/// test that passes when it should fail — so it is generous on purpose.
const SILENCE: Duration = Duration::from_millis(750);

/// The redial delays these tests run with. A minute is not a thing a test can
/// wait for, and a ladder nobody can afford to watch is one nobody checks.
const TEST_FIRST_BACKOFF: Duration = Duration::from_millis(60);
const TEST_MAX_BACKOFF: Duration = Duration::from_millis(120);

// --- the fake relay --------------------------------------------------------

/// What the fake relay saw, in the order it saw it.
#[derive(Debug)]
enum Seen {
    /// A control link was dialled, with this request target.
    Control(String),
    /// A `hello` arrived and this is whether its signature verified against the
    /// host id it named.
    Hello { host: String, label: String, verified: bool },
    /// A keepalive `ping`, which the real relay answers without waking.
    Ping,
    /// A pipe was dialled back, with this request target.
    PipeDial(String),
}

/// What a test tells the parked control link to do.
enum Say {
    /// `{"t":"open","pipe":…}` — a browser attached.
    Open(String),
    /// Send an arbitrary text frame, for the shapes a well-behaved relay never
    /// sends.
    Raw(String),
    /// Hang up, as a deploy or a colo move does.
    Close,
    /// Drop the socket with no close frame — which is how a link ends in
    /// practice, and the case a polite `Close` does not cover. The daemon's
    /// frame reader reports it as `the peer went away mid-frame`, an `Err`.
    Abort,
}

/// How the fake relay should answer the next dial-back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PipePolicy {
    /// Upgrade it and hand the socket to the test.
    Accept,
    /// Close the socket without answering, which is what a 404 from
    /// `room.ts`'s "no such pipe" looks like from the daemon's side.
    Refuse,
}

struct FakeRelay {
    addr: SocketAddr,
    seen: Receiver<Seen>,
    /// Set before an `open` is sent; read when the dial-back lands.
    policy: Arc<Mutex<PipePolicy>>,
    /// The control links that reached `ready`, newest last.
    parked: Receiver<Sender<Say>>,
    /// Pipe legs the fake relay accepted, as byte streams.
    pipes: Receiver<WebSocket<TcpStream>>,
}

impl FakeRelay {
    /// Listen on loopback and serve until the test drops it.
    fn start(host: zest_proto::HostId) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake relay");
        let addr = listener.local_addr().expect("the fake relay's address");
        let (seen_tx, seen) = mpsc::channel();
        let (parked_tx, parked) = mpsc::channel();
        let (pipes_tx, pipes) = mpsc::channel();
        let policy = Arc::new(Mutex::new(PipePolicy::Accept));

        let policy_for_thread = Arc::clone(&policy);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let seen_tx = seen_tx.clone();
                let parked_tx = parked_tx.clone();
                let pipes_tx = pipes_tx.clone();
                let policy = Arc::clone(&policy_for_thread);
                std::thread::spawn(move || {
                    serve_one(stream, host, &seen_tx, &parked_tx, &pipes_tx, &policy);
                });
            }
        });

        Self { addr, seen, policy, parked, pipes }
    }

    /// `ws://` rather than `wss://`: `RelayOrigin::parse` admits plaintext for
    /// loopback and nothing else, and this is loopback.
    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn next_seen(&self) -> Seen {
        self.seen.recv_timeout(PATIENCE).expect("the fake relay saw nothing in time")
    }

    /// The next thing the relay saw that `f` accepts, skipping keepalives and
    /// anything else in between.
    fn wait_for(&self, mut f: impl FnMut(&Seen) -> bool) -> Seen {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
            let seen = self.seen.recv_timeout(left).expect("the fake relay never saw it");
            if f(&seen) {
                return seen;
            }
        }
    }

    fn next_parked(&self) -> Sender<Say> {
        self.parked.recv_timeout(PATIENCE).expect("no control link was ever parked")
    }

    fn set_pipe_policy(&self, policy: PipePolicy) {
        *self.policy.lock().expect("policy") = policy;
    }

    /// Assert that no pipe was dialled for `window`.
    ///
    /// Everything else the relay saw is skipped rather than treated as the
    /// answer: `Control` and `Hello` are still queued from the handshake, and a
    /// bare `recv_timeout` would return one of them and pass a test that proves
    /// nothing.
    fn no_pipe_dial_within(&self, window: Duration, why: &str) {
        let deadline = Instant::now() + window;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_default();
            match self.seen.recv_timeout(left) {
                Err(RecvTimeoutError::Timeout) => return,
                Err(e) => panic!("the fake relay stopped: {e}"),
                Ok(Seen::PipeDial(target)) => panic!("{why}: {target}"),
                Ok(_) => {}
            }
        }
    }
}

/// One connection to the fake relay: route on the path, exactly as
/// `cloud/packages/relay/src/index.ts` does.
fn serve_one(
    stream: TcpStream,
    host: zest_proto::HostId,
    seen: &Sender<Seen>,
    parked: &Sender<Sender<Say>>,
    pipes: &Sender<WebSocket<TcpStream>>,
    policy: &Arc<Mutex<PipePolicy>>,
) {
    // The request target, captured out of the upgrade — `tungstenite::accept`
    // alone would discard it, and it is the whole of how the relay tells a
    // control link from a pipe.
    let target = Arc::new(Mutex::new(String::new()));
    let seen_target = Arc::clone(&target);
    let accepted = tungstenite::accept_hdr(stream, |request: &Request, response: Response| {
        *seen_target.lock().expect("target") =
            request.uri().path_and_query().map_or_else(String::new, ToString::to_string);
        Ok(response)
    });
    let Ok(mut ws) = accepted else { return };
    let target = target.lock().expect("target").clone();

    if target.starts_with("/v1/pipe") {
        let _ = seen.send(Seen::PipeDial(target));
        if *policy.lock().expect("policy") == PipePolicy::Refuse {
            // Closed rather than upgraded-then-closed, because that is the
            // shape the daemon must survive: the relay's own answer to an id it
            // never issued is a 404, and by the time the daemon sees it there
            // is no WebSocket at all.
            return;
        }
        let _ = pipes.send(ws);
        return;
    }

    if !target.starts_with("/v1/control") {
        return;
    }
    let _ = seen.send(Seen::Control(target));

    // A fixed nonce would let a captured signature answer every challenge, and
    // a test that could not tell those apart is a test of nothing.
    let nonce = Nonce::random().expect("a nonce");
    let challenge = serde_json::json!({
        "t": "challenge",
        "v": 1,
        "nonce": hex(nonce.as_bytes()),
        // The daemon reads this and pins nothing to it; sending a real-shaped
        // value keeps that true rather than accidentally exercised.
        "relay_key": hex(&[0xab; 32]),
    });
    if ws.send(Message::Text(challenge.to_string().into())).is_err() {
        return;
    }

    // A deadline on the socket rather than a second thread: the loop below has
    // to both read the link and write to it, and one socket cannot be split.
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(25)));

    let (say_tx, say_rx) = mpsc::channel::<Say>();
    let mut ready = false;

    loop {
        // Commands first, so an `open` sent by a test is not held behind a read
        // that is about to time out anyway.
        while let Ok(command) = say_rx.try_recv() {
            let sent = match command {
                Say::Open(pipe) => ws.send(Message::Text(
                    serde_json::json!({ "t": "open", "v": 1, "pipe": pipe, "exp": 0 })
                        .to_string()
                        .into(),
                )),
                Say::Raw(text) => ws.send(Message::Text(text.into())),
                Say::Close => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
                Say::Abort => {
                    let _ = ws.get_ref().shutdown(std::net::Shutdown::Both);
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }

        match ws.read() {
            Ok(Message::Text(text)) => {
                if text.as_str() == "ping" {
                    let _ = seen.send(Seen::Ping);
                    // The real relay answers this in the runtime, without
                    // waking the object. Answering it here is what proves the
                    // daemon does not treat a bare `pong` as malformed JSON.
                    if ws.send(Message::Text("pong".into())).is_err() {
                        return;
                    }
                    continue;
                }
                if ready {
                    continue;
                }
                let Some(hello) = parse_hello(&text) else { continue };
                let verified = hello.host == host
                    && zest_mesh::identity::verify_host(
                        hello.host,
                        Purpose::Auth,
                        nonce.as_bytes(),
                        &hello.sig,
                    )
                    .is_ok();
                let _ = seen.send(Seen::Hello {
                    host: hex(&hello.host.0),
                    label: hello.label,
                    verified,
                });
                if !verified {
                    let _ = ws.send(Message::Text(
                        serde_json::json!({ "t": "error", "v": 1, "code": "bad-signature" })
                            .to_string()
                            .into(),
                    ));
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
                if ws
                    .send(Message::Text(
                        serde_json::json!({ "t": "ready", "v": 1 }).to_string().into(),
                    ))
                    .is_err()
                {
                    return;
                }
                ready = true;
                let _ = parked.send(say_tx.clone());
            }
            Ok(_) => {}
            // A read deadline elapsing. Both spellings, because Windows and
            // unix disagree and matching one is a silent half-fix — the same
            // lesson `lan.rs`'s `Severable` records.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
    }
}

struct ParsedHello {
    host: zest_proto::HostId,
    label: String,
    sig: Signature,
}

/// The `hello` as the Worker parses it, field by field and never a cast.
fn parse_hello(text: &str) -> Option<ParsedHello> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("t")?.as_str()? != "hello" || value.get("v")?.as_u64()? != 1 {
        return None;
    }
    let host = <[u8; 32]>::try_from(unhex(value.get("host")?.as_str()?)?.as_slice()).ok()?;
    let sig = Signature::from_slice(&unhex(value.get("sig")?.as_str()?)?).ok()?;
    Some(ParsedHello {
        host: zest_proto::HostId::from_bytes(host),
        label: value.get("label")?.as_str()?.to_string(),
        sig,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

// --- a pipe leg as a byte stream -------------------------------------------

/// One tungstenite WebSocket, seen as the byte stream `DaemonClient` wants.
///
/// The two halves share one socket behind a mutex, which is only sound because
/// `DaemonClient` is strictly request/response on this connection: it sends,
/// then reads, and never both at once. A streaming client would need a real
/// split — which is exactly why the daemon's own `ws.rs` has one and this
/// harness does not.
#[derive(Clone)]
struct PipeBytes {
    ws: Arc<Mutex<WebSocket<TcpStream>>>,
    /// The message being handed out, and how much of it has gone.
    pending: Arc<Mutex<(Vec<u8>, usize)>>,
    out: Vec<u8>,
}

impl PipeBytes {
    fn new(ws: WebSocket<TcpStream>) -> Self {
        // Without this a test that is going to fail hangs the suite instead,
        // which Windows CI has already paid for twice on `tests/lan.rs`.
        let _ = ws.get_ref().set_read_timeout(Some(PATIENCE));
        Self {
            ws: Arc::new(Mutex::new(ws)),
            pending: Arc::new(Mutex::new((Vec::new(), 0))),
            out: Vec::new(),
        }
    }
}

impl Read for PipeBytes {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut pending = self.pending.lock().expect("pending");
                let (buf, at) = &mut *pending;
                if *at < buf.len() {
                    let n = (buf.len() - *at).min(out.len());
                    out[..n].copy_from_slice(&buf[*at..*at + n]);
                    *at += n;
                    return Ok(n);
                }
            }
            let message = self.ws.lock().expect("ws").read().map_err(io::Error::other)?;
            match message {
                Message::Binary(bytes) => {
                    *self.pending.lock().expect("pending") = (bytes.to_vec(), 0);
                }
                Message::Close(_) => return Ok(0),
                // A text frame on a pipe is something no end of this path
                // sends; forwarding it would launder a protocol error into a
                // decode failure two layers away.
                _ => {}
            }
        }
    }
}

impl Write for PipeBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.out.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.out);
        let mut ws = self.ws.lock().expect("ws");
        ws.send(Message::Binary(bytes.into())).map_err(io::Error::other)?;
        ws.flush().map_err(io::Error::other)
    }
}

// --- the daemon under test -------------------------------------------------

struct Daemon {
    identity: Arc<HostIdentity>,
    config: DaemonConfig,
    registry: Arc<Registry>,
    auth: Arc<Authenticator>,
    trust: Arc<MemoryTrustStore>,
    gate: Arc<Gate>,
}

fn daemon(label: &str) -> Daemon {
    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let trust = Arc::new(MemoryTrustStore::new());
    let auth = Arc::new(Authenticator::new(
        Arc::clone(&identity),
        Arc::clone(&trust) as Arc<dyn TrustStore>,
        PairingQueue::new(),
        label,
    ));
    let config = DaemonConfig {
        host: identity.host_id(),
        label: label.into(),
        local_socket: String::new(),
        listen_lan: false,
        lan_bind: "127.0.0.1".into(),
        lan_port: 0,
        listen_ws: false,
        ws_bind: "127.0.0.1".into(),
        ws_port: 0,
        relay: None,
        shell_integration: true,
        min_delta_interval: Duration::ZERO,
    };
    Daemon { identity, config, registry: Arc::new(Registry::new()), auth, trust, gate: Arc::new(Gate::new()) }
}

impl Daemon {
    fn trust_client(&self, client: &ClientIdentity) {
        self.trust
            .insert(TrustRecord {
                client: client.client_id(),
                label: "known".into(),
                paired_at: std::time::SystemTime::now(),
                last_seen: None,
            })
            .expect("insert");
    }

    /// Start the relay leg against `url`, dialling through `dial`.
    fn start_relay(&self, url: &str, dial: Dialler) {
        self.start_relay_with(url, dial, TEST_FIRST_BACKOFF, TEST_MAX_BACKOFF);
    }

    /// The same, with the redial ladder named — for the tests that are about
    /// the ladder.
    fn start_relay_with(&self, url: &str, dial: Dialler, first: Duration, max: Duration) {
        let relay = Relay::new(
            url,
            Arc::clone(&self.identity),
            self.config.clone(),
            Arc::clone(&self.registry),
            Arc::clone(&self.auth),
            Arc::clone(&self.gate),
            Roots::Platform,
        )
        .expect("a loopback ws:// url is dialable")
        .with_dialler(dial)
        .with_backoff(first, max);
        std::thread::spawn(move || relay.run());
    }
}

/// The real plaintext dialler, with every dial's moment recorded.
///
/// Wrapping rather than replacing: what is under test includes the socket, the
/// `Severable` that makes a cut portable, and the real WebSocket upgrade — so
/// the seam is used for observation here and for substitution nowhere.
fn recording(inner: Dialler, log: Arc<Mutex<Vec<Instant>>>) -> Dialler {
    Arc::new(move |host, port| {
        log.lock().expect("dial log").push(Instant::now());
        inner(host, port)
    })
}

/// A dialler that never connects, for the failure path.
fn always_fails(count: Arc<AtomicUsize>) -> Dialler {
    Arc::new(move |_host, _port| -> io::Result<Wire> {
        count.fetch_add(1, Ordering::AcqRel);
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "there is no relay there"))
    })
}

/// Fill the daemon-wide mid-handshake budget, and hand back the guards.
fn fill_the_gate(gate: &Gate) -> Vec<Countdown> {
    let mut held = Vec::new();
    loop {
        match gate.admit(PeerKey::Loopback) {
            Ok(slot) => held.push(slot),
            Err(Refused::Busy) => return held,
            Err(Refused::Cooling(_)) => unreachable!("loopback is never rate-limited"),
        }
    }
}

// --- the tests -------------------------------------------------------------

#[test]
fn a_daemon_parks_a_control_link_by_signing_the_relays_challenge() {
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());

    let Seen::Control(target) = relay.next_seen() else {
        panic!("the first thing a daemon does is dial the control path")
    };
    assert!(
        target.starts_with("/v1/control?host="),
        "the room is named by `?host=`, and `index.ts` refuses anything else: {target}"
    );
    assert!(
        target.ends_with(&hex(&d.config.host.0)),
        "the id in the URL is what names the Durable Object; a different one is another \
         machine's room: {target}"
    );

    let Seen::Hello { host, label, verified } = relay.next_seen() else {
        panic!("a challenge is answered with a hello")
    };
    assert_eq!(host, hex(&d.config.host.0), "the hello names this machine");
    assert_eq!(label, "relay-host", "the label travels, unsigned, for a log line");
    assert!(
        verified,
        "this is the whole cross-implementation contract: `verify_host` here is what the \
         Worker's `helloSignatureIsValid` is a port of, and a link whose signature does not \
         verify is a machine that can never be reached"
    );

    relay.next_parked();
}

#[test]
fn an_attach_becomes_a_second_socket_carrying_a_real_session() {
    // The whole leg, and the thing `cloud/README.md` records as not yet
    // demonstrated: a `zest-proto` handshake through the relay's dial-back.
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());

    let control = relay.next_parked();
    let pipe_id = "0123456789abcdef0123456789abcdef";
    control.send(Say::Open(pipe_id.into())).expect("the control link is parked");

    let dial = relay.wait_for(|seen| matches!(seen, Seen::PipeDial(_)));
    let Seen::PipeDial(target) = dial else { unreachable!() };
    assert!(
        target.contains(&format!("pipe={pipe_id}")),
        "the pipe id is the host leg's only credential, and it has to be the one issued: {target}"
    );
    assert!(
        target.contains(&format!("host={}", hex(&d.config.host.0))),
        "`index.ts` refuses a pipe dial whose `?host=` names no machine: {target}"
    );

    let pipe = relay.pipes.recv_timeout(PATIENCE).expect("the daemon dialled back");
    let client = Arc::new(ClientIdentity::generate().expect("client key"));
    d.trust_client(&client);

    let bytes = PipeBytes::new(pipe);
    let mut session = DaemonClient::connect(
        Box::new(bytes.clone()),
        Box::new(bytes),
        &client,
        "relay-test",
        Some(d.config.host),
        false,
    )
    .expect("the zest-proto handshake must complete through the relay's pipe");

    assert_eq!(
        session.host_label(),
        "relay-host",
        "the Welcome comes from this daemon, through two hops and an ADR-008 sealed channel"
    );
    assert!(
        session.list().expect("a request/response exchange over the pipe").is_empty(),
        "a fresh daemon has no sessions, and answering at all is what proves the pipe carries \
         both directions"
    );
}

#[test]
fn a_pipe_the_daemon_cannot_serve_is_refused_before_it_costs_a_socket() {
    // `MAX_UNAUTHENTICATED` is daemon-wide on purpose: under dial-back a
    // logical stream *is* a socket, and a host with a parked control link can
    // be asked to open them without limit. A relay pipe that did not count
    // against the same budget would be the one transport that could exhaust
    // the daemon's threads.
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());
    let control = relay.next_parked();

    let held = fill_the_gate(&d.gate);
    assert!(!held.is_empty(), "the gate must have a budget at all");

    control.send(Say::Open("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())).expect("parked");
    relay.no_pipe_dial_within(
        SILENCE,
        "a relay pipe was dialled while the mid-handshake budget was full, so relay streams \
         do not count against it",
    );

    // And the refusal is of the pipe, not of the leg: the control link is still
    // parked and the next attach is served.
    drop(held);
    control.send(Say::Open("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into())).expect("still parked");
    let Seen::PipeDial(target) = relay.wait_for(|s| matches!(s, Seen::PipeDial(_))) else {
        unreachable!()
    };
    assert!(target.contains("pipe=bbbbbbbb"), "the freed budget serves the next attach: {target}");
}

#[test]
fn a_pipe_that_cannot_be_dialled_does_not_take_the_control_link_with_it() {
    // The failure a daemon meets in ordinary service: the browser hung up while
    // the host was opening its socket, or the dial missed the relay's ten-second
    // deadline, and `/v1/pipe` answers 404. One attach failing must not cost
    // every other session on the machine its route home.
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());
    let control = relay.next_parked();

    relay.set_pipe_policy(PipePolicy::Refuse);
    control.send(Say::Open("cccccccccccccccccccccccccccccccc".into())).expect("parked");
    relay.wait_for(|s| matches!(s, Seen::PipeDial(t) if t.contains("pipe=cccccccc")));

    relay.set_pipe_policy(PipePolicy::Accept);
    control.send(Say::Open("dddddddddddddddddddddddddddddddd".into())).expect("still parked");
    let Seen::PipeDial(target) = relay.wait_for(|s| matches!(s, Seen::PipeDial(_))) else {
        unreachable!()
    };
    assert!(
        target.contains("pipe=dddddddd"),
        "the control link answered a second `open` after a refused dial: {target}"
    );
    relay.pipes.recv_timeout(PATIENCE).expect("and the second pipe really came up");
}

#[test]
fn a_control_link_that_drops_is_redialled_after_a_delay() {
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    let dials = Arc::new(Mutex::new(Vec::new()));
    d.start_relay(&relay.url(), recording(plaintext_dialler(), Arc::clone(&dials)));

    let control = relay.next_parked();
    control.send(Say::Close).expect("the link is up");

    // A second parked link is the whole assertion: the daemon dialled again,
    // signed a fresh challenge, and was made ready. A machine that gave up here
    // is one that is unreachable until somebody restarts it.
    relay.next_parked();

    let dials = dials.lock().expect("dial log");
    assert_eq!(dials.len(), 2, "exactly one redial, not a spin: {} dials", dials.len());
    let gap = dials[1].duration_since(dials[0]);
    assert!(
        gap >= TEST_FIRST_BACKOFF,
        "the redial came {gap:?} after the first, which is inside the backoff: a relay that \
         is down is down for every daemon in the fleet, and they all retry at once"
    );
}

#[test]
fn a_relay_that_is_not_there_is_retried_with_a_growing_delay() {
    // The `--relay` pointing at nothing case, from the leg's side: it must
    // neither give up nor spin. That it also leaves loopback serving is
    // `main.rs`'s doing — `start_relay` returns a `Result` the caller logs, in
    // the same shape as `start_ws` — and is the one part of this not covered
    // here, because it lives in the binary.
    let d = daemon("relay-host");
    let dials = Arc::new(AtomicUsize::new(0));
    d.start_relay("ws://127.0.0.1:1", always_fails(Arc::clone(&dials)));

    // Four delays at 60ms, 120ms, 120ms… is a shade over 300ms, so a window of
    // one second bounds the ladder generously from above while still catching a
    // spin — which would be thousands.
    std::thread::sleep(Duration::from_secs(1));
    let n = dials.load(Ordering::Acquire);
    assert!(n >= 2, "a relay that refuses a connection must be retried, not given up on: {n}");
    assert!(
        n < 60,
        "{n} dials in a second is a spin: every daemon in the fleet retries a dead relay at \
         once, and the delay is what stops them arriving in step"
    );
}

#[test]
fn a_parked_link_pings_and_a_bare_pong_is_not_a_protocol_error() {
    // The keepalive is answered by the runtime rather than by the object — the
    // whole of ADR-009's "an idle host costs nothing" — so the answer is the
    // bare string `pong`, which is not JSON. A daemon that parsed before
    // checking would tear its own link down on the relay's cheapest correct
    // behaviour.
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());
    let control = relay.next_parked();

    // Sent as the relay's own keepalive would be, rather than waiting out the
    // thirty-second interval: what is under test is the daemon's handling of
    // the reply, and the interval is a constant with its own argument.
    control.send(Say::Raw("pong".into())).expect("parked");

    // Still parked afterwards: an `open` is answered, which it would not be if
    // the `pong` had ended the link.
    control.send(Say::Open("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into())).expect("parked");
    relay.wait_for(|s| matches!(s, Seen::PipeDial(t) if t.contains("pipe=eeeeeeee")));
}

#[test]
fn a_pipe_id_the_relay_could_not_have_issued_is_never_dialled() {
    // The id reaches a query string, so a value carrying `&` rewrites the
    // request rather than failing it — a relay could otherwise have this daemon
    // dial into another machine's room. `ws.rs` refuses CR and LF in a request
    // line and nothing refuses this.
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    d.start_relay(&relay.url(), plaintext_dialler());
    let control = relay.next_parked();

    control
        .send(Say::Open("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&host=deadbeef".into()))
        .expect("parked");
    relay.no_pipe_dial_within(SILENCE, "a malformed pipe id was dialled");

    // And the link survives it: a hostile or broken `open` is that attach's
    // problem and not the machine's.
    control.send(Say::Open("ffffffffffffffffffffffffffffffff".into())).expect("still parked");
    relay.wait_for(|s| matches!(s, Seen::PipeDial(t) if t.contains("pipe=ffffffff")));
}

#[test]
fn a_link_that_parked_and_then_died_redials_at_the_shortest_delay() {
    // **Found by running against a real relay Worker under `wrangler dev`, not
    // by any test that existed first.** The relay was restarted; the daemon's
    // reader ended with `the peer went away mid-frame`, which is an `Err` and
    // not a clean end of stream; and the redial waited the twenty seconds three
    // earlier refusals had built, with a healthy relay already listening.
    //
    // A polite `Say::Close` does not reproduce it — that path was green
    // throughout. The link has to die the way links die.
    const FIRST: Duration = Duration::from_millis(100);
    let d = daemon("relay-host");
    let relay = FakeRelay::start(d.config.host);
    // A wide ceiling, so a ladder that climbs is unmistakable on the clock
    // rather than a few milliseconds away from one that does not.
    d.start_relay_with(&relay.url(), plaintext_dialler(), FIRST, Duration::from_secs(4));

    // Each gap on its own, compared with the others rather than with the clock.
    //
    // The first version asserted the total against `FIRST * 5` and failed on a
    // CI runner at 688ms — three 100ms delays plus ~390ms of scheduling, which
    // is indistinguishable from a climbing ladder if you only look at the sum.
    // A gap *ratio* is not: constant overhead lands on every gap equally, so a
    // ladder shows up as growth however slow the machine is, and this repo has
    // already paid three times for tests that assert wall-clock on a loaded
    // runner.
    relay.next_parked().send(Say::Abort).expect("the first link is up");
    let mut gaps = Vec::new();
    for _ in 0..3 {
        let at = Instant::now();
        let link = relay.next_parked();
        gaps.push(at.elapsed());
        link.send(Say::Abort).ok();
    }

    let first = gaps[0];
    let last = gaps[2];
    assert!(
        first >= FIRST,
        "a redial after {first:?} is no delay at all: a relay that is down is down for every \
         daemon in the fleet, and they would all retry at once"
    );
    assert!(
        last < first * 2,
        "the gaps grew — {gaps:?} — which is the ladder climbing. A link that parked must \
         redial at the shortest delay however it then died, or a machine is unreachable for \
         the whole of the ladder an earlier outage built. Doubling is the ladder's own step, \
         so anything short of it is scheduling noise and anything past it is the bug"
    );
}
