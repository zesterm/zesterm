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

/// A command that stays alive without printing.
///
/// [`quiet_cmd`] is the wrong shape for a wait: it exits at once, so every wait
/// against it ends on the exit arm and no test could ever reach a *timeout*.
/// This one never finishes and never prints — so a wait on it is bounded by its
/// deadline and by nothing else, which is what makes a timeout test
/// deterministic rather than a race with a shell.
fn long_lived_cmd() -> String {
    if cfg!(windows) {
        "powershell.exe -NoProfile -Command Start-Sleep 30".into()
    } else {
        "/bin/sleep 30".into()
    }
}

/// A daemon serving connections on a loopback port, for the life of the test.
fn serve_daemon() -> (String, Arc<Registry>) {
    serve_daemon_with(false)
}

/// The same, with the OSC 133 hook injected into whatever shell is spawned.
///
/// Off for every test but one. The hook writes a shim into the config directory
/// on each spawn and only does anything for zsh, bash and PowerShell, so a test
/// that spawns `/bin/sh` pays a filesystem write for a marker that will never
/// arrive.
fn serve_daemon_with(shell_integration: bool) -> (String, Arc<Registry>) {
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
        shell_integration,
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
    let sent = tools
        .call("input", &serde_json::json!({ "session": session, "text": "echo hi", "submit": true }))
        .expect("input");
    assert_eq!(sent["writes"], 2, "the text and the Enter are two keystrokes (#344): {sent}");

    // A refusal, not a silent no-op: the whole complaint in #345 is that a key
    // which does nothing is indistinguishable from one the app ignored. Parsed
    // before anything is sent, so this needs no live session behind it.
    let err = tools
        .call("input", &serde_json::json!({ "session": session, "keys": ["nope"] }))
        .expect_err("an unknown key name must refuse");
    assert!(
        err.to_string().contains("pageup"),
        "the refusal must carry the vocabulary a model can retry from: {err}"
    );

    let id_before = registry.get(
        tools.resolver().resolve(&session).expect("resolve").session,
    );
    assert!(id_before.is_some(), "input must not have ended the session");

    tools.call("close_session", &serde_json::json!({ "session": session })).expect("close");
}

#[test]
fn an_arrow_key_reaches_a_live_session_and_lets_go_of_it_again() {
    // Arrows are the one family whose bytes depend on the session -- DECCKM
    // decides `ESC [ A` against `ESC O A` -- and that mode lives only on a
    // replica, so this call attaches, encodes, writes and detaches.
    //
    // `long_lived_cmd` rather than `quiet_cmd`: the latter exits at once, and
    // an attach to a session the registry has already swept waits out the reply
    // deadline instead of answering. That is #347's cost and it applies to
    // `screen` and `blocks` equally; what this test is about is the live path.
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));
    let created = tools
        .call(
            "create_session",
            &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }),
        )
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    let keys = tools
        .call(
            "input",
            &serde_json::json!({ "session": session, "keys": ["down", "down", "enter"] }),
        )
        .expect("named keys");
    assert_eq!(keys["writes"], 3, "three keys, three keystrokes: {keys}");

    // Reading a mode must not leave a subscriber behind on somebody's session,
    // the same property `detaching_leaves_the_session_running` holds for reads.
    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    let attached = listed["sessions"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["id"] == session.as_str())
        .map(|s| s["attached"].as_bool().unwrap_or(true));
    assert_eq!(
        attached,
        Some(false),
        "typing an arrow must not leave this server attached: {listed}"
    );

    tools.call("close_session", &serde_json::json!({ "session": session })).expect("close");
    registry.close(tools.resolver().resolve(&session).expect("resolve").session);
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

/// A command that fails, with a status nothing could have guessed.
///
/// `3` rather than `1`: a `1` is what a dozen accidents produce, so asserting
/// it would pass for a reader that reports "it failed" without having read a
/// number at all.
fn exit_3_cmd() -> String {
    if cfg!(windows) {
        "cmd.exe /c exit 3".into()
    } else {
        "/bin/sh -c 'exit 3'".into()
    }
}

/// The claim this crate exists to make, end to end.
///
/// Every other exit code here is the shell's word: OSC 133;D is forgeable, and
/// `cat`ting a file containing the markers mints a block with a green `exit 0`
/// that the parser structurally cannot tell from a real one. This one is read
/// from the process by the daemon, and until #299 it was **never sent** —
/// `HostMessage::Exited` carried `code: None` unconditionally, so the one
/// trustworthy status in the system did not reach anybody.
///
/// ⚠️ This test waits for a child to exit, which the module doc above says
/// nothing here does. That rule is about waiting for a child to *print*, which
/// is what times out on a loaded Windows runner (#285); `run_isolated` cannot
/// be tested without awaiting an exit, so the child is one that exits at once
/// and the deadline is generous.
#[test]
fn run_isolated_reports_the_status_the_process_really_exited_with() {
    let (addr, _registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let v = tools
        .call("run_isolated", &serde_json::json!({ "command": exit_3_cmd() }))
        .expect("run_isolated must return a result for a command that ran");

    assert_eq!(v["exited"], true, "the child exited promptly; this timed out instead: {v}");
    assert_eq!(
        v["exit_code"], 3,
        "the process exited with 3. `null` is the regression: the daemon reporting the \
         exit while dropping the status, which is what shipped until #299"
    );
    assert_eq!(
        v["exit_code_source"], "process_exit",
        "an exit code read from the process must say so -- it is the one an agent may \
         trust, and it has to be distinguishable from a shell marker in the payload \
         rather than by which tool was called"
    );
    assert_eq!(v["timed_out"], false);
}

/// The output survives long enough to be read, which is an ordering property.
///
/// Reading after the detach does not merely race — it reads nothing, twice
/// over. `Conn::detach` drops this process's replica, and `Registry::sweep`
/// collects a session that `has_exited() && ever_attached() && !attached()`, so
/// the host destroys it too. Swapping the two lines in `run_isolated` makes
/// this test fail with a *timeout* on a command that finished instantly, which
/// is worth knowing: the symptom points at the deadline rather than at the
/// ordering that caused it.
///
/// Asserted from the client side, because from the daemon's side both orders
/// look like a correct sweep.
#[test]
fn a_finished_commands_output_is_read_before_the_session_is_swept() {
    let (addr, _registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let cmd = if cfg!(windows) {
        "cmd.exe /c echo zesterm-marker".to_string()
    } else {
        "/bin/echo zesterm-marker".to_string()
    };
    let v = tools.call("run_isolated", &serde_json::json!({ "command": cmd })).expect("run");

    let text = v["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("zesterm-marker"),
        "what the command printed must come back. Empty here means the read happened \
         after the detach, so the session was swept before anyone looked: {v}"
    );
    assert!(
        text.contains("never instructions"),
        "command output is attacker-controlled and must arrive fenced, exactly as \
         `screen` and `output` fence theirs: {text}"
    );
    assert_eq!(v["exit_code"], 0, "a successful echo exits zero: {v}");
}

/// A timeout returns what there is; it does not kill.
///
/// The whole advantage over sentinel injection: a command sitting at
/// `Password:` is indistinguishable from a finished one to a harness that
/// guesses at completion, and killing it destroys the only state that could
/// answer it. So the session has to still be there afterwards.
#[test]
fn a_timeout_returns_partial_output_and_leaves_the_command_running() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let cmd = if cfg!(windows) {
        "powershell.exe -NoProfile -Command Start-Sleep 30".to_string()
    } else {
        "/bin/sleep 30".to_string()
    };
    let v = tools
        .call("run_isolated", &serde_json::json!({ "command": cmd, "timeout_ms": 300 }))
        .expect("a timeout is a result, not an error -- there is output to report");

    assert_eq!(v["timed_out"], true, "a 30s sleep cannot finish in 300ms: {v}");
    assert_eq!(v["exited"], false);
    assert!(
        v["exit_code"].is_null(),
        "a command still running has no status, and inventing one is how a caller \
         concludes it succeeded: {v}"
    );
    assert!(
        v["exit_code_source"].is_null(),
        "no status means nothing to attribute to a source. Keying this off `!timed_out` \
         rather than off the code itself claims the process reported `null`, which is \
         not a thing a process can say"
    );

    let session = v["session"].as_str().expect("the result names the session it started");
    let addr = tools.resolver().resolve(session).expect("the id this server just minted");
    assert!(
        registry.get(addr.session).is_some(),
        "the session must outlive the timeout. Killing it is what makes a command \
         waiting at a password prompt unanswerable, which is the case this tool \
         exists to handle"
    );

    tools.call("interrupt", &serde_json::json!({ "session": session })).expect("interrupt");
    registry.close(addr.session);
}

/// A shell that emits OSC 133, and a command that exits 3 in it.
///
/// `Shell::detect` knows zsh, bash and PowerShell — fish is unwritten and
/// cmd.exe has no prompt-function mechanism at all — so on a machine with none
/// of the three there is no way to test the correlation live and no point
/// pretending otherwise.
///
/// The command is a *subshell* on unix and a native call on Windows: `exit 3`
/// would end the shell rather than report a status, and this has to leave the
/// session at a prompt afterwards. `3` rather than `1` because a `1` is what a
/// dozen accidents produce, so asserting it would pass for a reader that never
/// read a number.
fn integrated_shell() -> Option<(String, String)> {
    if cfg!(windows) {
        // Not `-Command`/`-File`: `install_pwsh` declines to append its hook to a
        // command line that already runs something, and would then silently
        // produce a session with no blocks — which reads here as a broken
        // correlation rather than as a badly chosen test shell.
        return Some(("powershell.exe -NoLogo -NoProfile".into(), "cmd /c exit 3".into()));
    }
    // zsh candidates before bash's: on the Macs this runs on, zsh is the
    // default whose configuration realistically exists, and macOS's /bin/bash
    // is the 3.2 the hook supports by design but no machine here has verified.
    ["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh", "/opt/homebrew/bin/zsh", "/bin/bash",
     "/usr/bin/bash"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|shell| (shell.to_string(), "(exit 3)".into()))
}

/// Wait for the shell to draw its first prompt, on its own budget.
///
/// Separate from the `run` that follows it, deliberately. #285 is three daemon
/// tests timing out together because each paid PowerShell's startup inside its
/// own assertion budget, so every one of them reported as its own subject; here a
/// shell that never starts fails as a shell that never started, and the
/// correlation is measured against a shell that is already up.
fn wait_for_prompt(tools: &zest_mcp::ToolSet, session: &str, limit: Duration) -> bool {
    let give_up = std::time::Instant::now() + limit;
    while std::time::Instant::now() < give_up {
        let v = tools
            .call("blocks", &serde_json::json!({ "session": session }))
            .expect("a session that exists answers `blocks`");
        if !v["blocks"].as_array().expect("an array").is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// The claim `run` makes, against a shell that really emits the markers.
///
/// Everything else about the correlation is held by `tests/replay.rs` against
/// recorded frames, which is what makes it testable on every platform. This is
/// the half a recording cannot have: that the line `run` writes reaches a real
/// shell, is submitted, and comes back as the block this crate then correlates.
///
/// ⚠️ The one test here that waits for a child to **print**, which the module doc
/// above says nothing does — and #285 is what that rule is protecting. Held to
/// one test, with the shell's startup on its own budget above, and skipping
/// loudly where no shell with an integration hook exists rather than asserting
/// something weaker on those machines.
#[test]
fn a_run_against_a_real_shell_returns_the_shells_own_exit_code() {
    let Some((shell, exits_three)) = integrated_shell() else {
        eprintln!(
            "SKIPPED a_run_against_a_real_shell_returns_the_shells_own_exit_code: no shell \
             with an OSC 133 hook on this machine. `Shell::detect` knows zsh, bash and \
             PowerShell; a host with none of the three cannot run this. Every other property \
             of the correlation is covered by tests/replay.rs, which needs no shell."
        );
        return;
    };

    let (addr, registry) = serve_daemon_with(true);
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": shell, "cols": 100, "rows": 30 }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();
    let session_addr = tools.resolver().resolve(&session).expect("the id this server minted");

    assert!(
        wait_for_prompt(&tools, &session, Duration::from_secs(30)),
        "`{shell}` never emitted an OSC 133 prompt marker. That is shell startup or the \
         integration hook, not the correlation -- run \
         `cargo run -p zest-daemon --example attach` to see whether the markers arrive at all"
    );

    let v = tools
        .call(
            "run",
            &serde_json::json!({
                "session": session,
                "command": exits_three,
                "timeout_ms": 30_000,
            }),
        )
        .expect("a shell at a prompt must accept a command");

    assert_eq!(v["state"], "finished", "the command finished; this did not settle: {v}");
    assert_eq!(v["timed_out"], false, "{v}");
    assert_eq!(
        v["exit_code"], 3,
        "the shell reported the status in OSC 133;D and it must arrive as the number. \
         `null` is a shell whose hook emitted a bare `D`, which is a different bug: {v}"
    );
    assert_eq!(
        v["exit_code_source"], "shell_marker",
        "this one came off a marker any program could print. `process_exit` belongs to \
         `run_isolated` alone, and the two read identically in a payload"
    );
    assert!(
        v["block_id"].is_u64(),
        "a settled run names the block, so `output` and `wait` can be pointed at it: {v}"
    );
    assert_eq!(
        v["warnings"].as_array().expect("an array").len(),
        0,
        "nothing else typed in this session, so the block must record the command that \
         was sent: {v}"
    );

    // A second command in the same shell, which is the reason `run` exists at
    // all: the session persists, so this one inherits the first's environment
    // and working directory. Its block is a *new* id, minted by the prompt that
    // followed the first -- the case the correlation must accept alongside the
    // reused id above, and the one an `==` would fail.
    let second = tools
        .call(
            "run",
            &serde_json::json!({
                "session": session,
                "command": "echo zesterm-marker",
                "timeout_ms": 30_000,
            }),
        )
        .expect("the shell is back at a prompt and takes another command");

    assert_eq!(second["state"], "finished", "{second}");
    assert_eq!(second["exit_code"], 0, "an echo exits zero: {second}");
    assert!(
        second["block_id"].as_u64() > v["block_id"].as_u64(),
        "the second command landed in a block the next prompt minted: {second}"
    );
    let text = second["text"].as_str().expect("the command printed something");
    assert!(
        text.contains("zesterm-marker"),
        "what the command printed must come back, scoped to its own block: {text}"
    );
    assert!(
        text.contains("never instructions"),
        "output from somebody's live shell is attacker-controlled like any other: {text}"
    );

    // A command that destroys the block it is running in. `erase_screen` drops
    // every block not entirely above the cleared region, and an open one never
    // is -- so after `clear` there is nothing left to correlate, and the answer
    // has to come back at once rather than at the deadline. Reading the index's
    // *floor* instead of the block's own presence reported this as a command
    // that never started and then waited out the caller's whole timeout, because
    // `erase_screen` lowers the floor with `min(lowest_gone)` and a young
    // session's floor is already 0.
    //
    // The state is not pinned: what matters is that it does not hang, and the
    // two shells reach it differently -- pwsh emits its `D` from the prompt
    // function, which runs *after* `Clear-Host`.
    let cleared = tools
        .call(
            "run",
            &serde_json::json!({ "session": session, "command": "clear", "timeout_ms": 15_000 }),
        )
        .expect("clearing the screen is a result, not an error");
    assert_eq!(
        cleared["timed_out"], false,
        "a command whose block was destroyed must be answered at once; 15s here is the \
         whole deadline spent on a block that went in the first millisecond: {cleared}"
    );

    // And a command that takes the shell with it. `exit` emits no `D`, because
    // there is no `precmd` left to run, so the block it started can never close:
    // waiting on it would burn the whole deadline reporting a command as
    // "running" in a session that ended in the first millisecond. Found by
    // driving the built binary by hand with `echo hi; exit 7`, which no test in
    // this crate would have produced.
    let ended = tools
        .call(
            "run",
            &serde_json::json!({ "session": session, "command": "exit", "timeout_ms": 30_000 }),
        )
        .expect("a shell that exits is a result, not an error");
    assert_eq!(
        ended["session_exited"], true,
        "the shell has gone and the answer must say so, or an agent waits on a block \
         nothing will ever close: {ended}"
    );
    assert_eq!(
        ended["timed_out"], false,
        "and it must come back at once rather than at the deadline -- 30s here means \
         the wait is not watching for the session ending: {ended}"
    );
    assert!(
        ended["block_id"].is_u64(),
        "the block `exit` started must still be named. An exit and the output before it \
         can reach a client out of order -- the daemon's reader sets `has_exited` on EOF \
         while the bytes ahead of it are still queued for the parser -- so reading the \
         instant it arrives reports `block_id: null` for a command that plainly ran. \
         That is the drain in `settle`, and it reproduced about one run in eight: {ended}"
    );

    registry.close(session_addr.session);
}

/// A shell with no integration is told which tool to use, not left waiting.
///
/// `/bin/sh` and `cmd.exe` emit no markers ever, so `run` has nothing to
/// correlate — and the refusal has to name `run_isolated`, because a model that
/// cannot act on a refusal pays for it twice. `timeout_ms: 0` skips the grace
/// that exists for a shell which has merely not drawn its prompt yet.
#[test]
fn a_run_in_a_shell_that_emits_no_markers_names_run_isolated() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": quiet_cmd() }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    let err = tools
        .call(
            "run",
            &serde_json::json!({ "session": session, "command": "ls", "timeout_ms": 0 }),
        )
        .expect_err("a session with no command blocks cannot be correlated");

    let msg = err.to_string();
    assert!(
        msg.contains("run_isolated"),
        "the refusal must name the tool that works instead: {msg}"
    );

    // And nothing was typed into it: a refusal that has already written the
    // command is worse than one that has not, because the command still ran.
    let addr = tools.resolver().resolve(&session).expect("resolve");
    assert!(registry.get(addr.session).is_some(), "the session must be left alone");
    registry.close(addr.session);
}

/// A status and its provenance travel together, or neither travels.
///
/// The invariant across every result this crate produces: `exit_code_source`
/// answers "where did this number come from", so it is meaningless without a
/// number and dishonest beside a null one. `block_json` already keys its source
/// off the value for the same reason; this is the process-exit half of the same
/// rule, and the two must not drift apart.
#[test]
fn an_exit_code_and_its_source_are_present_together_or_not_at_all() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    for (args, what) in [
        (serde_json::json!({ "command": exit_3_cmd() }), "a command that finished"),
        (
            serde_json::json!({
                "command": if cfg!(windows) {
                    "powershell.exe -NoProfile -Command Start-Sleep 30"
                } else {
                    "/bin/sleep 30"
                },
                "timeout_ms": 300
            }),
            "a command that timed out",
        ),
    ] {
        let v = tools.call("run_isolated", &args).expect("run_isolated returns a result");
        assert_eq!(
            v["exit_code"].is_null(),
            v["exit_code_source"].is_null(),
            "{what}: a code without a source cannot be trusted and a source without a \
             code describes nothing. Got code={} source={}",
            v["exit_code"],
            v["exit_code_source"]
        );
        if let Some(s) = v["session"].as_str() {
            if let Ok(a) = tools.resolver().resolve(s) {
                registry.close(a.session);
            }
        }
    }
}

/// A wait that cannot be satisfied gives up on time and still answers.
///
/// Deliberately waiting for something that *cannot* arrive: a sequence no
/// session will ever reach, in a child that neither prints nor exits. That is
/// what makes this the one wait test with no timing to lose — it races nothing
/// and takes its deadline every run on every platform. What it pins is the
/// shape the tools promise: a timeout is a **result**, so the screen comes back
/// anyway, exactly as `run_isolated` returns partial output rather than an
/// error.
///
/// The child has to be [`long_lived_cmd`] and not [`quiet_cmd`]: written with
/// the latter this failed, because the shell had already exited and the wait
/// correctly ended on that instead. Which is the exit arm doing its job — but
/// it means a session that ends is no place to test a deadline from.
#[test]
fn a_wait_that_times_out_still_answers_with_the_screen() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    let started = std::time::Instant::now();
    let v = tools
        .call(
            "screen",
            &serde_json::json!({ "session": session, "after_seq": u64::MAX, "timeout_ms": 300 }),
        )
        .expect("a deadline passing is an answer, not a transport failure");

    assert_eq!(v["waited"], true, "asking for `after_seq` must actually wait: {v}");
    assert_eq!(v["timed_out"], true, "nothing can pass u64::MAX, so this must be a timeout: {v}");
    assert_eq!(v["changed"], false);
    assert!(
        v["text"].as_str().is_some_and(|t| t.contains("UNTRUSTED-TERMINAL-OUTPUT")),
        "a timed-out wait still returns the screen, fenced like every other read: {v}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the caller's 300ms deadline governs, not the connection's 20s reply deadline"
    );

    let a = tools.resolver().resolve(&session).expect("the id this server minted");
    registry.close(a.session);
}

/// A sequence already behind answers at once, without waiting at all.
///
/// The cheap half of the long-poll, and the one an agent hits constantly: it
/// passes the `seq` from its last read, the session has moved on since, and the
/// answer is already there. Waiting even once here would make every catch-up
/// read pay a round of latency for nothing.
///
/// `after_seq: 0` is the safe spelling of "has anything happened at all". A
/// live session has parsed something, so its sequence is past zero.
#[test]
fn a_wait_for_a_sequence_already_passed_answers_immediately() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": quiet_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    let started = std::time::Instant::now();
    let v = tools
        .call(
            "screen",
            &serde_json::json!({ "session": session, "after_seq": 0, "timeout_ms": 30_000 }),
        )
        .expect("screen");

    assert_eq!(v["changed"], true, "the session is past sequence 0 the moment it attaches: {v}");
    assert_eq!(v["timed_out"], false);
    assert!(
        v["seq"].as_u64().is_some_and(|s| s > 0),
        "and the sequence it reports is the one to pass back next time: {v}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "a condition that already holds must not wait out any part of the 30s asked for"
    );

    let a = tools.resolver().resolve(&session).expect("the id this server minted");
    registry.close(a.session);
}

/// A wait on a session whose child has gone ends, rather than sitting out.
///
/// The anti-hang property, and the reason `Replica::exited` is an arm of every
/// predicate here. Without it a wait against a dead shell costs its whole
/// deadline — up to the 30-minute ceiling — and a silent hang of that length
/// inside an agent loop is poison (#319). Asserted as elapsed time, because the
/// returned value alone cannot tell a fast answer from a slow one.
///
/// ⚠️ Waits for a child to *exit*, on the same exemption as
/// `run_isolated_reports_the_status_the_process_really_exited_with`. The module
/// rule is about waiting for a child to **print**, which is what flakes on a
/// loaded Windows runner (#285); the child here exits at once and every
/// deadline is generous.
#[test]
fn a_wait_ends_when_the_session_does_rather_than_running_out_its_deadline() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    // A session of our own, never attached, whose child exits at once. Not
    // `run_isolated`'s: that one detaches on the way out, and `Registry::sweep`
    // then collects a session that `has_exited() && ever_attached() &&
    // !attached()` -- so the *attach* inside `screen` waits out the 20s reply
    // deadline against a session the host has already destroyed. Measured, at
    // 20.02s, which is issue #319's "ghost session" complaint rather than
    // anything this test is about.
    //
    // A just-created session is safe from that sweep for as long as it takes to
    // get here: the predicate needs `ever_attached`, which is exactly what
    // keeps a new session alive until its owner attaches.
    let created = tools
        .call("create_session", &serde_json::json!({ "command": quiet_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    // A minute is far beyond anything this can legitimately take, and far below
    // the ceiling a hang would sit out -- so the assertion distinguishes the
    // two without being tight enough to flake on a loaded runner.
    let started = std::time::Instant::now();
    let v = tools
        .call(
            "screen",
            &serde_json::json!({ "session": session, "after_seq": u64::MAX, "timeout_ms": 60_000 }),
        )
        .expect("screen");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "a wait against a session whose child has exited must end on the exit, not on \
         its deadline. This took {:?} of the 60s asked for",
        started.elapsed()
    );
    assert_eq!(
        v["exited"], true,
        "and it must say *why* it stopped: `timed_out` would send an agent back round \
         the loop against a shell that is never going to answer: {v}"
    );
    assert_eq!(v["timed_out"], false, "the deadline is not what ended this wait: {v}");

    if let Ok(a) = tools.resolver().resolve(&session) {
        registry.close(a.session);
    }
}

/// A `blocks` wait gives up on time, and a flag that is not one is refused.
///
/// The child runs with no shell integration, so it mints no blocks and nothing
/// can ever finish — the `blocks` counterpart of the screen timeout above, and
/// deterministic for the same reason.
///
/// **Not covered here: the alternate-screen refusal.** `blocks` checks for it
/// *before* waiting as well as on the read, because blocks are not emitted
/// there at all and a wait would otherwise answer with a full deadline of
/// silence. Reaching that state live means starting a child and waiting for it
/// to print an escape sequence, which is the shape the module doc above
/// forbids; the detection itself is held against the `vim-macos` recording in
/// `tests/replay.rs`, and both refusal sites share one `ALT_SCREEN` constant so
/// they cannot drift into disagreeing.
#[test]
fn a_block_wait_gives_up_on_time_and_refuses_a_flag_that_is_not_one() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();

    let started = std::time::Instant::now();
    let v = tools
        .call(
            "blocks",
            &serde_json::json!({ "session": session, "wait": true, "timeout_ms": 300 }),
        )
        .expect("a deadline passing is an answer");
    assert_eq!(v["waited"], true);
    assert_eq!(v["timed_out"], true, "nothing finishes in a shell with no OSC 133: {v}");
    assert!(v["finished_block"].is_null(), "and nothing is named as having finished: {v}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    // The argument guard, which needs no session at all.
    let err = tools
        .call("blocks", &serde_json::json!({ "session": session, "wait": "yes" }))
        .expect_err("a string is not a flag");
    assert!(
        err.to_string().contains("true or false"),
        "reading a bad flag as absent would turn a long-poll into a plain read, which \
         looks like a session that never changes: {err}"
    );

    let a = tools.resolver().resolve(&session).expect("the id this server minted");
    registry.close(a.session);
}

/// A listing must describe the sessions as they are *now*, not as they were the
/// last time this connection created or closed one.
///
/// `sessions` served a cache for a long time, and the cache was only ever
/// refreshed by the reply to this connection's own `CreateSession` /
/// `CloseSession` — the connection opts out of the watch push on purpose. So
/// every field that a session acquires *after* it is spawned (its title, its
/// OSC 7 cwd, whether it went to the alternate screen) stayed at the empty value
/// it had a millisecond after the spawn, for as long as the agent did not happen
/// to create something else. An agent reading `alt_screen: false` off a session
/// running vim goes to `blocks`, which is empty on the alternate screen by
/// design, and concludes the session is idle.
///
/// The second client here is the registry itself, which is what a session
/// created from the app or from another agent looks like from this connection:
/// no reply of ours is involved, so nothing invalidates a cache. Asserting on a
/// *foreign* session rather than on a title keeps this test free of any wait for
/// a child to print, which is this module's rule. (#360)
#[test]
fn a_listing_reflects_a_session_this_connection_did_not_create() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    // One create of our own, so the cache is warm and populated: without this
    // the listing would be empty for the ordinary reason rather than the one
    // under test.
    let created = tools
        .call("create_session", &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let ours = created["session"].as_str().expect("a session id").to_string();

    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    let before = listed["sessions"].as_array().expect("array").len();

    // Somebody else opens a shell. Nothing on this connection asked for it and
    // nothing replies to us about it.
    let theirs = registry
        .create(
            &zest_pty::CommandSpec {
                command_line: long_lived_cmd(),
                cwd: None,
                env: Vec::new(),
            },
            zest_pty::PtySize::new(COLS, ROWS),
            1000,
        )
        .expect("a second client's session");

    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    let ids: Vec<&str> =
        listed["sessions"].as_array().expect("array").iter().filter_map(|s| s["id"].as_str()).collect();
    assert_eq!(
        ids.len(),
        before + 1,
        "a listing is a question asked of the daemon, not a cache invalidated by our own \
         writes — every field a session gains after it spawns is stale until it is: {listed}"
    );

    let a = tools.resolver().resolve(&ours).expect("the id this server minted");
    registry.close(a.session);
    registry.close(theirs.id);
}

/// `sessions` and `screen` must never disagree about the same session.
///
/// The invariant #346 asked to have pinned. Both tools describe one terminal, so
/// a field that differs between them means one of the two is reading something
/// other than the session — which is what a cache is. #361 made the listing a
/// question rather than a cache read; this is the assertion that keeps it one.
///
/// Size is the field moved here because it is the only one of the three that can
/// be moved without a child printing, which `tests/live.rs` does not wait on
/// (#285). It is not a lesser case: `sessions` reporting the size a session was
/// *created* with while `screen` reported the size it had been resized *to* is
/// one of the rows in #346's evidence table, and a listing rebuilt from the
/// creation request rather than from the grid reproduces it exactly. Title and
/// `alt_screen` are compared too, and travel in the same `Registry::list` answer
/// as the size, so they cannot come from a different source than the one this
/// asserts.
#[test]
fn a_listing_and_a_screen_never_disagree_about_one_session() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();
    let a = tools.resolver().resolve(&session).expect("the id this server minted");

    // Somebody's window changes shape. Nothing on this connection asked for it,
    // so nothing of ours replies and nothing would invalidate a cache.
    let grown = (COLS + 20, ROWS + 6);
    registry.get(a.session).expect("the session just created").resize(grown.0, grown.1);

    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    let mine = listed["sessions"]
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["id"] == session.as_str())
        .expect("the session this test created")
        .clone();
    let screen = tools
        .call("screen", &serde_json::json!({ "session": session }))
        .expect("screen");

    assert_eq!(
        (mine["cols"].as_u64(), mine["rows"].as_u64()),
        (Some(u64::from(grown.0)), Some(u64::from(grown.1))),
        "a listing reports the grid a session has, not the one it was asked for: {mine}"
    );
    for field in ["cols", "rows", "title", "alt_screen"] {
        assert_eq!(
            mine[field], screen[field],
            "`sessions` and `screen` describe one terminal, so a `{field}` that differs \
             means one of them is reading something that is not the session (#346): \
             {mine} vs {screen}"
        );
    }

    registry.close(a.session);
}

/// A session the daemon does not have is refused at once, not at the deadline.
///
/// The daemon has always answered an attach on an unknown session immediately,
/// with an `Error` naming it. The client recorded that into `Shared::error` --
/// the link's own state, read alongside `closed` -- and woke nobody, so the
/// waiter ran its full 20s request deadline and every tool that attaches
/// reported a session the host had denied in milliseconds only after twenty
/// seconds of silence.
///
/// That silence is the cost, not the wrong answer. From inside an agent loop it
/// is indistinguishable from a busy session or a slow host, so the agent's
/// choices are to wait it out or to abandon a session that might be alive --
/// and it is reachable by the agent's own successful `close_session`. (#347)
#[test]
fn a_dead_session_is_refused_at_once_rather_than_at_the_deadline() {
    let (addr, registry) = serve_daemon();
    let tools = zest_mcp::ToolSet::new(dial(&addr, "zest-mcp agent"));

    let created = tools
        .call("create_session", &serde_json::json!({ "command": long_lived_cmd(), "cols": COLS, "rows": ROWS }))
        .expect("create_session");
    let session = created["session"].as_str().expect("a session id").to_string();
    tools.call("close_session", &serde_json::json!({ "session": session })).expect("close");

    // The listing half of #347: a closed session leaves it. This holds because
    // the listing is a question now (#361) -- the daemon's registry no longer
    // has the session, so nothing has to remember to sweep a cache.
    let listed = tools.call("sessions", &serde_json::json!({})).expect("sessions");
    assert!(
        !listed["sessions"].as_array().expect("array").iter().any(|s| s["id"] == session.as_str()),
        "a session closed through this server must leave the listing: {listed}"
    );

    let started = std::time::Instant::now();
    let err = tools
        .call("screen", &serde_json::json!({ "session": session }))
        .expect_err("the daemon does not have this session");
    let waited = started.elapsed();

    assert!(
        err.to_string().contains("no session"),
        "the refusal must say the session is gone, not report whatever the caller \
         happened to be waiting for: {err}"
    );
    assert!(
        waited < std::time::Duration::from_secs(5),
        "a refusal the daemon sends immediately must be reported immediately: a silent \
         wait is indistinguishable from a busy session inside an agent loop, and only \
         one of those is worth waiting out -- took {waited:?}"
    );

    registry.close(tools.resolver().resolve(&session).expect("resolve").session);
}
