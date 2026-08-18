//! The built binary, driven over stdio exactly as a harness drives it.
//!
//! `rpc.rs`'s own tests cover the protocol against a fake, and `live.rs` covers
//! the tools against a real daemon. Neither proves the thing a harness actually
//! does: spawn `zest-mcp`, write JSON-RPC to its stdin, and read JSON-RPC back
//! off its stdout. This does, against a real daemon on a real socket.
//!
//! # What this catches that nothing else can
//!
//! A stray `println!` anywhere in the process corrupts the stream, and the
//! harness then reports a JSON parse error rather than whatever went wrong.
//! That failure is invisible to every unit test and obvious here: this asserts
//! that **every line on stdout is JSON-RPC**, so a diagnostic that wandered on
//! to the wrong stream fails loudly rather than in somebody's editor.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use zest_daemon::{Authenticator, DaemonConfig, Registry};
use zest_mesh::identity::HostIdentity;
use zest_mesh::pairing::PairingQueue;
use zest_mesh::trust::MemoryTrustStore;

/// A socket path unique to this test process.
///
/// Windows wants a pipe name and unix a filesystem path; both must not collide
/// with the developer's own daemon, which is listening on the default.
fn socket_path() -> String {
    let unique = format!("zesterm-mcp-stdio-{}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\{unique}")
    } else {
        std::env::temp_dir().join(unique).display().to_string()
    }
}

/// A daemon on `socket`, for the life of the test.
fn serve_daemon(socket: &str) {
    let socket_for_probe = socket.to_string();
    let identity = Arc::new(HostIdentity::generate().expect("host key"));
    let config = DaemonConfig {
        host: identity.host_id(),
        label: "stdio-test".into(),
        local_socket: socket.to_string(),
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
        offer: Some(zest_daemon::offer::OfferSource::new(zest_daemon::offer::facts(
            "stdio-test-shell".into(),
        ))),
    };
    let auth = Arc::new(Authenticator::new(
        identity,
        Arc::new(MemoryTrustStore::new()),
        PairingQueue::new(),
        "stdio-test",
    ));
    let registry = Arc::new(Registry::new());
    let socket = socket.to_string();

    // `listen` serves for ever, so it owns a thread. Detached: the test process
    // exiting takes it, and nothing here needs to shut it down in order.
    std::thread::spawn(move || {
        let _ = zest_daemon::listen(&socket, config, registry, auth);
    });

    // Wait for it to be dialable rather than sleeping a fixed time. The
    // binary under test would spawn its own daemon otherwise, and then this
    // test would be about a daemon it did not configure.
    let give_up = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < give_up {
        if zest_daemon::connect(&socket_for_probe).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the test daemon never started listening");
}

/// The binary under test, as cargo built it beside this test.
fn binary() -> std::path::PathBuf {
    // `CARGO_BIN_EXE_<name>` is set by cargo for the crate's own binaries, so
    // this cannot pick up a stale `zest-mcp` from PATH -- which is exactly the
    // mistake `resolve_daemon_binary` documents for the daemon.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_zest-mcp"))
}

struct Harness {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Harness {
    fn start(socket: &str) -> Self {
        let mut child = Command::new(binary())
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a failure to start says why in the test output
            // rather than vanishing.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn zest-mcp");
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, out }
    }

    /// Send one request and read one reply.
    fn call(&mut self, msg: &serde_json::Value) -> serde_json::Value {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{msg}").expect("write");
        stdin.flush().expect("flush");
        self.read()
    }

    /// Send a notification. Nothing comes back, by design.
    fn notify(&mut self, msg: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{msg}").expect("write");
        stdin.flush().expect("flush");
    }

    fn read(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let n = self.out.read_line(&mut line).expect("read");
        assert!(n > 0, "the server closed stdout instead of answering");
        serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!(
                "stdout carried something that is not JSON-RPC -- a stray print \
                 corrupts the stream and the harness then reports a parse error \
                 rather than the real fault. Line was: {line:?} ({e})"
            )
        })
    }

    fn handshake(&mut self) {
        let init = self.call(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "stdio-test", "version": "0" }
            }
        }));
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "a tools server must advertise the tools capability: {init}"
        );
        self.notify(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Closing stdin is how a harness says it is done; the server ends on
        // EOF. Kill only if it does not.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn a_harness_can_handshake_list_tools_and_read_a_session() {
    let socket = socket_path();
    serve_daemon(&socket);
    let mut h = Harness::start(&socket);

    h.handshake();

    let listed = h.call(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list"
    }));
    let tools = listed["result"]["tools"].as_array().expect("a tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for want in ["hosts", "sessions", "screen", "blocks", "output", "input"] {
        assert!(names.contains(&want), "`{want}` must be offered; got {names:?}");
    }

    // The wait arguments have to reach the *advertised schema*, not merely the
    // handler: a model calls what `tools/list` describes, so an argument the
    // dispatcher accepts and the schema omits is one nothing will ever pass.
    let schema_of = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| t["inputSchema"]["properties"].clone())
            .unwrap_or_else(|| panic!("`{name}` must be offered; got {names:?}"))
    };
    for want in ["after_seq", "timeout_ms", "idle_ms"] {
        assert!(!schema_of("screen")[want].is_null(), "`screen` must advertise `{want}`");
    }
    for want in ["wait", "timeout_ms"] {
        assert!(!schema_of("blocks")[want].is_null(), "`blocks` must advertise `{want}`");
    }
    // Same rule for the keys surface. A model that cannot see `keys` in the
    // schema goes on hand-encoding escape sequences into `text`, which is the
    // thing #345 measured at roughly 2 attempts in 10.
    for want in ["text", "paste", "keys", "submit"] {
        assert!(!schema_of("input")[want].is_null(), "`input` must advertise `{want}`");
    }
    assert_eq!(
        schema_of("input")["keys"]["type"], "array",
        "`keys` is advertised as a list, so one call can send several"
    );

    let hosts = h.call(&serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "hosts", "arguments": {} }
    }));
    assert_eq!(hosts["result"]["isError"], false, "hosts failed: {hosts}");
    let host = &hosts["result"]["structuredContent"]["hosts"][0];
    assert_eq!(
        host["default_shell"], "stdio-test-shell",
        "the server must be talking to the daemon this test started: {hosts}"
    );

    let created = h.call(&serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "create_session", "arguments": {
            "command": if cfg!(windows) { "cmd.exe /c echo mcp" } else { "/bin/sh -c 'echo mcp'" },
            "cols": 80, "rows": 24
        }}
    }));
    assert_eq!(created["result"]["isError"], false, "create_session failed: {created}");
    let session = created["result"]["structuredContent"]["session"]
        .as_str()
        .expect("a session id")
        .to_string();

    let screen = h.call(&serde_json::json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "screen", "arguments": { "session": session } }
    }));
    assert_eq!(screen["result"]["isError"], false, "screen failed: {screen}");
    let text = screen["result"]["structuredContent"]["text"].as_str().expect("text");
    assert!(
        text.contains("UNTRUSTED-TERMINAL-OUTPUT"),
        "terminal text must reach the model fenced: {text}"
    );
}

#[test]
fn a_refused_call_comes_back_as_content_the_model_can_act_on() {
    // Not a JSON-RPC error: harnesses surface a transport failure and a tool
    // refusal very differently, and "this id is not one of mine, call `hosts`"
    // is something a model should read and act on.
    let socket = socket_path() + "-refuse";
    serve_daemon(&socket);
    let mut h = Harness::start(&socket);
    h.handshake();

    let refused = h.call(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "screen", "arguments": { "session": "deadbeef:1" } }
    }));

    assert!(refused.get("error").is_none(), "a refusal is not a transport error: {refused}");
    assert_eq!(refused["result"]["isError"], true);
    let text = refused["result"]["content"][0]["text"].as_str().expect("text");
    assert!(
        text.contains("no host matches") && text.contains("hosts"),
        "the refusal must say what was wrong and which call fixes it: {text}"
    );
}

#[test]
fn a_notification_produces_no_line_at_all() {
    // Answering a notification is itself a protocol error. Asserted over the
    // real pipe, because an extra line here desynchronizes every later reply --
    // the harness reads answer N for request N+1 and everything after it is
    // wrong.
    let socket = socket_path() + "-notify";
    serve_daemon(&socket);
    let mut h = Harness::start(&socket);
    h.handshake();

    h.notify(&serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" }));

    // If the notification produced a line, this reads *that* instead of the
    // ping's reply and the id will not match.
    let pong = h.call(&serde_json::json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" }));
    assert_eq!(pong["id"], 99, "a stray line desynchronized the stream: {pong}");
}
