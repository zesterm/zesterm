//! `zest-daemon` — serves this machine's terminals.
//!
//! **Skeleton — WS-F owns the implementation.** It builds and runs so the
//! workspace stays green and so the binary name is claimed; it does not yet
//! serve anything.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    eprintln!(
        "zest-daemon is not implemented yet.\n\
         \n\
         It will own this machine's terminal sessions and serve them to attached\n\
         clients -- the local GUI app over a loopback socket, other machines over\n\
         the LAN or a tunnel. See docs/ROADMAP.md, workstream F."
    );
    std::process::exit(69); // EX_UNAVAILABLE
}
