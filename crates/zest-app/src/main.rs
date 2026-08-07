//! zesterm.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ZESTERM_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "zesterm");
}
