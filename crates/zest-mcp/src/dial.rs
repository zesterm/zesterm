//! Turning a route into two byte legs.
//!
//! [`zest_fleet::best_route`] decides *how* to reach a machine and deliberately
//! cannot dial one — "a rule that can dial has stopped being a rule", as its
//! crate's boundary puts it. This is the other half, and every consumer writes
//! its own because constructing a transport needs a keychain, a control-plane
//! client and a TLS stack that each caller has its own opinion about. `zest-app`
//! has a `Dial` trait saying the same thing.
//!
//! What is **not** written twice is the relay ladder. Token from the credential
//! store, ticket from the control plane, TLS to the relay, WS upgrade — every
//! step is a place to be quietly wrong, and a near-copy on that path is how a
//! reused ticket or a credential captured once instead of read per dial gets
//! shipped. It lives in `zest_daemon::account::relay_dialer` and both callers
//! go through it.

use std::io::{Read, Write};

use zest_fleet::HostRoute;

/// The two halves a handshake runs over.
pub type Halves = (Box<dyn Read + Send>, Box<dyn Write + Send>);

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("could not reach it: {0}")]
    Io(String),
    /// This device's credential cannot open a relay leg, and no retry will
    /// change that until a person acts.
    ///
    /// Carries the reason rather than collapsing it, because the four cases
    /// ask for different things -- sign in, restore, wait -- and an agent told
    /// only "signed out" would report the wrong remedy for three of them.
    #[error("{0}")]
    Credential(zest_daemon::account::CredentialRefusal),
}

/// Open a connection along `route`.
///
/// Blocking, and sometimes for a while — a relay dial is a keychain read and
/// two network round trips. The caller runs it off whatever thread must stay
/// responsive.
pub fn dial(route: &HostRoute, roots: zest_cloud::tls::Roots) -> Result<Halves, DialError> {
    match route {
        HostRoute::LocalSocket(path) => {
            let a = zest_daemon::find_or_spawn(path, crate::DAEMON_START)
                .map_err(|e| DialError::Io(e.to_string()))?;
            Ok((a.read, a.write))
        }
        HostRoute::Tcp(addr) => {
            let stream = std::net::TcpStream::connect(addr).map_err(|e| DialError::Io(e.to_string()))?;
            // A terminal's writes are keystrokes: small, latency-bound, never
            // worth coalescing.
            let _ = stream.set_nodelay(true);
            let read = stream.try_clone().map_err(|e| DialError::Io(e.to_string()))?;
            Ok((Box::new(read) as Box<dyn Read + Send>, Box::new(stream) as Box<dyn Write + Send>))
        }
        HostRoute::Relay { host, relay_origin } => zest_daemon::account::relay_dialer(
            *host,
            relay_origin,
            zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
            roots,
            &zest_mesh::keystore::OsKeyStore,
        )
        .map_err(|e| match e {
            // Kept apart because they ask different things of the person
            // reading the refusal: one names an act, the other says try again.
            zest_daemon::account::RelayDialError::Credential(r) => DialError::Credential(r),
            zest_daemon::account::RelayDialError::Io(e) => DialError::Io(e),
        }),
    }
}
