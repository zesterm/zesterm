//! `zest-mcp` — terminals across a zesterm fleet, as an agent's tools.
//!
//! Registered with a harness as a stdio MCP server:
//!
//! ```jsonc
//! { "mcpServers": { "zesterm": { "command": "zest-mcp" } } }
//! ```
//!
//! It finds this machine's daemon or starts one, exactly as the window does
//! (ADR-007), and then is a client like any other — it pairs with a device key,
//! attaches, and reads the deltas the window reads. → #60, #274.
//!
//! # stdout is the protocol
//!
//! Logging goes to **stderr**, always. One stray line on stdout and the harness
//! reports a JSON parse error rather than whatever actually went wrong, which
//! is the classic confusing failure for a stdio MCP server.

use std::io::{BufReader, IsTerminal};
use std::sync::Arc;

use zest_mcp::rpc::Server;
use zest_mcp::{Conn, LiveFleet, ToolSet, DAEMON_START};

fn main() -> std::process::ExitCode {
    // stderr, not stdout. See the module doc -- this is not a preference.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("{HELP}");
        return std::process::ExitCode::SUCCESS;
    }

    let socket = opt("--socket").unwrap_or_else(zest_daemon::default_socket_path);
    if args.iter().any(|a| a == "--socket-path") {
        // The one thing worth printing on stdout, and only when nothing else
        // will be: this mode is a question from a person, not from a harness.
        println!("{socket}");
        return std::process::ExitCode::SUCCESS;
    }

    if std::io::stdin().is_terminal() {
        eprintln!(
            "zest-mcp speaks MCP over stdin/stdout and is meant to be launched by an \
             agent harness, not typed at.\n\n{HELP}"
        );
        return std::process::ExitCode::FAILURE;
    }

    let attached = match zest_daemon::find_or_spawn(&socket, DAEMON_START) {
        Ok(a) => a,
        Err(e) => {
            // A harness shows stderr when a server fails to start, so this is
            // the one message a person will actually see. Name the socket: the
            // usual cause is a daemon on a different path.
            eprintln!("zest-mcp: no daemon at {socket}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(spawned = attached.spawned, elapsed = ?attached.elapsed, "daemon");

    // A throwaway key on loopback, and deliberately so. The trust store is not
    // consulted there -- `auth.rs` argues that a check would be theatre, since
    // a process that can open the socket can already read the key it would
    // check -- and minting one keeps the OS keychain off the startup path
    // entirely. On macOS that path is a modal prompt after every rebuild, and a
    // tool server that hangs at startup is a broken tool server.
    //
    // The durable `agent-key` is `ToolSet`'s, minted on the first *remote* dial
    // and never before -- so this startup path is unchanged for a session that
    // never leaves this machine.
    let identity = match zest_mesh::identity::ClientIdentity::generate() {
        Ok(i) => Arc::new(i),
        Err(e) => {
            eprintln!("zest-mcp: could not mint a client key: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let label = opt("--label").unwrap_or_else(zest_mcp::client_label);
    let conn = match Conn::open(attached.read, attached.write, &identity, &label, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zest-mcp: the daemon refused this client: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(host = %conn.host().short(), label = conn.label(), "connected");

    // Nothing is discovered here. `LiveFleet` opens its multicast socket and
    // asks the control plane on the first `hosts` call and at no other moment,
    // so a server nobody asks about the fleet touches neither the network nor
    // the keychain -- which is what keeps this the same startup it has always
    // been.
    let fleet = LiveFleet::new(conn.host(), conn.label());
    let mut server =
        Server::new(ToolSet::new(conn, Box::new(fleet)).with_local_socket(&socket));
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match server.serve(BufReader::new(stdin.lock()), stdout.lock()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zest-mcp: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

const HELP: &str = "\
usage: zest-mcp [--socket <path>] [--label <name>] [--socket-path]

An MCP server for zesterm: terminals across the fleet, as an agent's tools.
Speaks MCP over stdin/stdout; a harness launches it.

  --socket <path>   the daemon to talk to (default: this user's)
  --socket-path     print that path and exit
  --label <name>    how this client names itself to a host
  --help            this

Register it with a harness as:
  { \"mcpServers\": { \"zesterm\": { \"command\": \"zest-mcp\" } } }
";
