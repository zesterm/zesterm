//! How this window reaches a machine — the transport, and where the rule went.
//!
//! The rule itself now lives in `zest-fleet`: [`zest_fleet::best_route`] over
//! [`zest_fleet::FleetHost`], returning [`zest_fleet::HostRoute`]. It moved
//! because it had a second consumer that could not have it — the MCP server is
//! a client of the daemon (ADR-015) and its `check-deps` boundary forbids
//! `winit` and `wgpu` by name, which this crate carries. A rule whose whole
//! point is that every surface agrees is the last thing to keep two copies of.
//!
//! What stayed is what needs sockets. [`Dial::dialer`] turns a decision into a
//! transport, and doing that needs a keychain, a control-plane client and a TLS
//! stack — each consumer's own, and none of them the rule's business. That is
//! the same split #250 made when the rule was gathered from three places into
//! one; this change only moved the pure half somewhere a second crate can
//! reach it.

/// Re-exported so this crate's own paths keep naming the rule where it has
/// always named it. The definitions live in `zest-fleet`; these are the app's
/// local names for them, not a second copy.
pub use zest_fleet::{best_route, HostRoute};

/// Turning a [`HostRoute`] into a live transport.
///
/// An extension trait rather than an inherent `impl`, because the type is not
/// this crate's any more and the sockets are. A consumer with a different
/// keychain and a different HTTP client implements its own; what they share is
/// the decision that produced the route.
pub trait Dial {
    fn dialer(&self) -> crate::remote::Dialer;
}

impl Dial for HostRoute {
    fn dialer(&self) -> crate::remote::Dialer {
        match self {
            HostRoute::LocalSocket(path) => {
                let path = path.clone();
                Box::new(move || {
                    let a = zest_daemon::find_or_spawn(&path, crate::app::DAEMON_START_TIMEOUT)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((a.read, a.write))
                })
            }
            HostRoute::Tcp(addr) => {
                let addr = addr.clone();
                Box::new(move || {
                    let stream = std::net::TcpStream::connect(&addr)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    // A terminal's writes are keystrokes: small, latency-bound,
                    // never worth coalescing.
                    let _ = stream.set_nodelay(true);
                    let read = stream
                        .try_clone()
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((
                        Box::new(read) as Box<dyn std::io::Read + Send>,
                        Box::new(stream) as Box<dyn std::io::Write + Send>,
                    ))
                })
            }
            HostRoute::Relay { host, relay_origin } => {
                let host = *host;
                let origin = relay_origin.clone();
                Box::new(move || {
                    // Every dial — every redial — runs the whole ladder fresh:
                    // token from the store, ticket from the control plane, TLS
                    // to the relay. This closure runs on the tab worker or the
                    // reconnect supervisor, so the keychain and two network
                    // round trips stay off the event loop.
                    //
                    // The ladder itself is `zest_daemon::account`'s, and there
                    // is exactly one of it: `zest-mcp` dials the same relay
                    // and must not get a second copy of a path where a reused
                    // ticket or a captured token is the failure (#274).
                    crate::cloud::relay_dialer(
                        host,
                        &origin,
                        zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
                        zest_cloud::tls::Roots::Platform,
                        &zest_mesh::keystore::OsKeyStore,
                    )
                    .map_err(|e| match e {
                        // Kept apart, because `supervise` acts on the
                        // difference: it stops on `SignedOut` and backs off on
                        // anything else, and no redial can succeed until a
                        // person signs in again.
                        crate::cloud::RelayDialError::SignedOut => {
                            crate::remote::RemoteError::SignedOut
                        }
                        crate::cloud::RelayDialError::Io(e) => crate::remote::RemoteError::Io(e),
                    })
                })
            }
        }
    }
}
