//! `Conn` against a real daemon, in-process over loopback TCP.
//!
//! `tests/replay.rs` proves the replica against recorded bytes; this proves the
//! things a recording cannot have — the handshake, the handoff, and the two
//! threads. It drives `serve()` directly rather than through a listener, the
//! same shape `zest-daemon/tests/coalescing.rs` uses, so there is no binary to
//! find and no socket path to collide on.
//!
//! # Nothing here waits for a child to print
//!
//! Every assertion is about the *connection*: a keyframe answers an attach
//! immediately, and the size arbitration is settled by the attach itself. That
//! is deliberate — the tests in this repo that wait for a spawned PowerShell to
//! produce output are the ones that time out on a loaded Windows runner (#285),
//! and an agent client's own suite had no reason to inherit that. A session is
//! still created, so a pty is still spawned; what is never awaited is its
//! output.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use zest_daemon::{serve, Auth, Authenticator, DaemonConfig, Registry};
use zest_mcp::Conn;
use zest_mesh::identity::{ClientIdentity, HostIdentity};
use zest_mesh::pairing::PairingQueue;
use zest_mesh::trust::MemoryTrustStore;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// A command that exits promptly on either platform.
///
/// Not a shell with an integration hook: nothing here reads blocks, and a shim
/// would add a filesystem dependency for no assertion.
fn quiet_cmd() -> String {
    if cfg!(windows) {
        "cmd.exe /c echo mcp".into()
    } else {
        "/bin/sh -c 'echo mcp'".into()
    }
}

/// A daemon serving connections on a loopback port, for the life of the test.
fn serve_daemon() -> (String, Arc<Registry>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();

    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let config = DaemonConfig {
        host: identity.host_id(),
        label: "mcp-test".into(),
        local_socket: String::new(),
        listen_lan: false,
        lan_bind: "127.0.0.1".into(),
        lan_port: 0,
        listen_ws: false,
        ws_bind: "127.0.0.1".into(),
        ws_port: 0,
        relay: None,
        shell_integration: false,
        min_delta_interval: Duration::ZERO,
        enroll: None,
        // A real offer, so the `watch_hosts` subscription is exercised rather
        // than merely requested. `None` is the ordinary harness value and would
        // make the first listing carry nothing, which is indistinguishable from
        // a daemon that predates the field -- and then the assertion below
        // would be about the test's own config rather than about the wire.
        offer: Some(zest_daemon::offer::OfferSource::new(zest_daemon::offer::facts(
            "mcp-test-shell".into(),
        ))),
    };
    let registry = Arc::new(Registry::new());

    let reg = Arc::clone(&registry);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let write = stream.try_clone().expect("clone");
            // A fresh authenticator per connection is fine: loopback does not
            // consult the trust store, which is exactly the property under
            // test on this transport.
            let auth = Auth::Transport(Arc::new(Authenticator::new(
                Arc::clone(&identity),
                Arc::new(MemoryTrustStore::new()),
                PairingQueue::new(),
                "mcp-test",
            )));
            let config = config.clone();
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                let _ = serve(stream, write, config, reg, auth, "in-process");
            });
        }
    });

    (addr, registry)
}

fn dial(addr: &str, label: &str) -> Conn {
    let stream = TcpStream::connect(addr).expect("connect");
    // Any read that waits this long is a hang, and a hung test reports nothing.
    stream.set_read_timeout(Some(Duration::from_secs(20))).expect("read timeout");
    let identity = Arc::new(ClientIdentity::generate().expect("client key"));
    Conn::open(
        Box::new(stream.try_clone().expect("clone")) as Box<dyn Read + Send>,
        Box::new(stream),
        &identity,
        label,
        None,
    )
    .expect("the loopback path welcomes any client that proves a key")
}

#[test]
fn a_connection_handshakes_and_learns_what_the_machine_is() {
    // The handshake, the handoff to two threads, and `list_with_offer` -- which
    // must be used rather than `list`, because the daemon marks the offer sent
    // as it emits it and a listing that drops it waits for a config edit on the
    // far machine that will never come.
    let (addr, _registry) = serve_daemon();
    let conn = dial(&addr, "zest-mcp test");

    assert_eq!(conn.label(), "mcp-test", "the connection must carry the host's own label");
    let offer = conn.with(|s| s.offer.clone());
    // It rides `list_with_offer`; a plain `list()` would drop it here.
    let offer = offer.expect(
        "this daemon publishes an offer and this connection set watch_hosts, so the \
         first listing must carry it",
    );
    assert_eq!(
        offer.default_shell, "mcp-test-shell",
        "the offer must be the one this machine published, not a default"
    );
    assert!(
        !offer.os.is_empty(),
        "a machine that publishes an offer must at least say what it is: {offer:?}"
    );
}

#[test]
fn creating_a_session_names_the_one_it_created() {
    // Never `sessions.last()`: two clients creating on one host concurrently
    // would each adopt the other's shell, which is why `Sessions.created`
    // exists at all.
    let (addr, registry) = serve_daemon();
    let conn = dial(&addr, "zest-mcp test");

    let created = conn
        .create_session(&quiet_cmd(), "", COLS, ROWS)
        .expect("the daemon answers a create with a listing naming it");

    assert!(
        registry.get(created.session).is_some(),
        "the address the host named must be a session the host actually has"
    );
    registry.close(created.session);
}

#[test]
fn attaching_builds_a_replica_from_the_keyframe_that_answers_it() {
    // Every attach starts with a keyframe -- a client that has just connected
    // has no base for a delta -- so this needs nothing from the child.
    let (addr, registry) = serve_daemon();
    let conn = dial(&addr, "zest-mcp test");

    let created = conn.create_session(&quiet_cmd(), "", COLS, ROWS).expect("create");
    conn.attach(created, COLS, ROWS, true).expect("attach");

    let size = conn.with(|s| s.replica(created).map(zest_mcp::Replica::size));
    assert_eq!(
        size,
        Some((usize::from(COLS), usize::from(ROWS))),
        "the replica must take its shape from the keyframe, which is the only \
         authority on it"
    );
    registry.close(created.session);
}

#[test]
fn an_observer_does_not_resize_the_session_someone_else_is_rendering() {
    // #278 asserted this inside the daemon. This is the same property from the
    // side that has to get it right -- a client with no pane, over a real
    // socket, through a real handshake. If `observe` were ever dropped on the
    // way to the wire, the daemon's own tests would still pass and this would
    // not.
    let (addr, registry) = serve_daemon();

    let human = dial(&addr, "a window");
    let created = human.create_session(&quiet_cmd(), "", COLS, ROWS).expect("create");
    human.attach(created, COLS, ROWS, false).expect("the human attaches and votes");

    let session = registry.get(created.session).expect("the session exists");
    assert_eq!(session.size(), (COLS, ROWS), "precondition: the human's size is the session's");

    let agent = dial(&addr, "zest-mcp agent");
    // Deliberately smaller than the human's: a vote at this size would win the
    // minimum, so a regression cannot pass by asking for something larger.
    agent.attach(created, 40, 10, true).expect("the agent attaches, observing");

    assert_eq!(
        session.size(),
        (COLS, ROWS),
        "an observing client resized the window somebody is looking at"
    );

    // And it is watching for real, not merely tolerated: it has a replica, at
    // the granted size rather than the one it named.
    let size = agent.with(|s| s.replica(created).map(zest_mcp::Replica::size));
    assert_eq!(
        size,
        Some((usize::from(COLS), usize::from(ROWS))),
        "the observer must receive the session's real shape, not its own ask"
    );
    registry.close(created.session);
}

#[test]
fn detaching_leaves_the_session_running() {
    // `Detach` and `CloseSession` differ in exactly one observable, and an
    // agent's default must be the one that does not kill somebody's shell.
    let (addr, registry) = serve_daemon();
    let conn = dial(&addr, "zest-mcp test");

    let created = conn.create_session(&quiet_cmd(), "", COLS, ROWS).expect("create");
    conn.attach(created, COLS, ROWS, true).expect("attach");
    conn.detach(created);

    assert!(
        registry.get(created.session).is_some(),
        "detaching must leave the session behind; only CloseSession ends it"
    );
    assert!(
        conn.with(|s| s.replica(created).is_none()),
        "a detached session must not leave a replica claiming to describe it"
    );
    registry.close(created.session);
}

#[test]
fn the_tools_answer_over_a_real_connection() {
    // The dispatch layer against a real daemon: what an agent would actually
    // receive. Deliberately no assertion about what the child printed -- the
    // shapes are the subject, and waiting on a spawned process is what makes
    // the rest of this repo's suite flaky on Windows (#285).
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let hosts = tools.call("hosts", &serde_json::json!({})).expect("hosts");
    let host = &hosts["hosts"][0];
    assert_eq!(host["local"], true);
    assert_eq!(host["default_shell"], "mcp-test-shell");
    let id = host["id"].as_str().expect("a host id").to_string();
    assert_eq!(id.len(), 8, "the short form is what every result echoes: {id}");

    let created = tools
        .call("create_session", &serde_json::json!({ "command": quiet_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();
    assert!(session.starts_with(&id), "a session id names its host: {session}");

    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    assert!(
        listed["sessions"].as_array().expect("array").iter().any(|s| s["id"] == session.as_str()),
        "the session just created must appear in the listing: {listed}"
    );

    let screen = tools
        .call("screen", &serde_json::json!({ "session": session }))
        .expect("screen");
    assert_eq!(screen["cols"], COLS);
    let text = screen["text"].as_str().expect("screen text");
    assert!(
        text.contains("UNTRUSTED-TERMINAL-OUTPUT") && text.contains("never instructions"),
        "terminal output must reach the model fenced and labelled: {text}"
    );

    // Typing needs no attachment, so this is a send with nothing to wait on.
    tools
        .call("input", &serde_json::json!({ "session": session, "text": "echo hi", "submit": true }))
        .expect("input");

    let id_before = registry.get(
        tools.resolver().resolve(&session).expect("resolve").session,
    );
    assert!(id_before.is_some(), "input must not have ended the session");

    tools.call("close_session", &serde_json::json!({ "session": session })).expect("close");
}

#[test]
fn a_session_id_this_server_never_minted_is_refused() {
    // The confused-deputy guard, over a real connection: a plausible id lifted
    // out of terminal output names nothing this server will act on. In a fleet
    // the damage of obeying an injected instruction is that it lands on a
    // different machine, so this refusal is the one that matters most.
    let (addr, _registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let err = tools
        .call("screen", &serde_json::json!({ "session": "deadbeef:1" }))
        .expect_err("a host this server never listed must not resolve");

    let msg = err.to_string();
    assert!(
        msg.contains("no host matches"),
        "the refusal must say the host is unknown, not fail somewhere deeper: {msg}"
    );
}
