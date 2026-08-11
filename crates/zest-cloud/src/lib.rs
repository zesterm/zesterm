//! Talking to Cloudflare: the relay dialler's TLS, and the daemon's HTTP.
//!
//! Two callers, one stack. `zest-daemon` parks a long-lived outbound control
//! link at the relay and dials a fresh socket per pipe (ADR-009), and
//! `--enroll` posts a signed code to the control plane — `NoHttpClient` is the
//! hole this crate fills. Both need TLS to a Cloudflare hostname and nothing
//! else needs it at all.
//!
//! # Why this is a crate and not a module in `zest-daemon`
//!
//! Because a boundary can be checked and a convention cannot. rustls, and any
//! HTTP client, get exactly **one** owner here, so a second cannot creep in
//! beside it — two TLS stacks in one binary is a build-time and an audit-time
//! cost that is paid quietly and never noticed. And the crates whose smallness
//! is a documented property — the ones that cross to wasm and to the browser
//! and phone clients — stay small: `cargo xtask check-deps` forbids the whole
//! family by name in each of them.
//!
//! Note what the fence does **not** claim. `zest-daemon` will depend on this
//! crate and `zest-app` depends on `zest-daemon`, so rustls reaches the desktop
//! binary and is meant to. "Keeps TLS out of the app" would be false; "TLS has
//! one owner, and the portable crates have none" is the property.
//!
//! # What is here
//!
//! [`tls`] is the transport: one connection, handed out as two independently
//! owned halves, because `zest_daemon::server::serve` drives a reader and a
//! writer from two threads and a `rustls::StreamOwned` can be neither cloned
//! nor split. [`http`] is one request/response exchange over that — enough for
//! the enrolment POST and nothing more.
//!
//! → `docs/ARCHITECTURE.md`, ADR-009.

pub mod http;
pub mod tls;

#[cfg(test)]
mod testing;
