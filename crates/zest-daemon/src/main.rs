//! `zest-daemon` — serves this machine's terminals.
//!
//! Owns the PTYs and hands their grids to whoever attaches: the local GUI app
//! over a loopback socket, other machines over the LAN or a tunnel. Sessions
//! outlive every client, which is the entire reason this is a separate process.
//! → ADR-007.

use std::sync::Arc;

use zest_daemon::{default_socket_path, listen, DaemonConfig, Registry};
use zest_proto::HostId;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "zest-daemon\n\n\
             --socket <path>   where to listen (default: per-user)\n\
             --label <name>    how this machine appears in a fleet listing\n\
             --socket-path     print the default socket path and exit\n\n\
             Sessions outlive the clients attached to them. Closing a window\n\
             does not end a shell."
        );
        return;
    }

    let socket = opt("--socket").unwrap_or_else(default_socket_path);

    if args.iter().any(|a| a == "--socket-path") {
        println!("{socket}");
        return;
    }

    let config = DaemonConfig {
        // A placeholder until WS-H (#7) lands real keypairs. `HostId` is meant
        // to be the fingerprint of this machine's public key, so that a peer
        // can verify it by asking it to sign a nonce; a fixed value proves
        // nothing and must not survive into anything reachable from the LAN.
        host: HostId::from_bytes([0; 32]),
        label: opt("--label").unwrap_or_else(machine_label),
        local_socket: socket.clone(),
        // Off until there is an identity to authorize against. A daemon that
        // served the network before it could tell peers apart would be handing
        // out shells.
        listen_lan: false,
    };

    tracing::info!(socket = %socket, label = %config.label, "starting");

    let registry = Arc::new(Registry::new());
    if let Err(e) = listen(&socket, config, registry) {
        tracing::error!(error = %e, "could not listen");
        std::process::exit(1);
    }
}

/// A human name for this machine, for the fleet listing.
fn machine_label() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(var) {
            if !name.is_empty() {
                return name;
            }
        }
    }
    "unnamed".to_string()
}
