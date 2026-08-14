//! The process that owns this machine's terminals.
//!
//! Session ownership works ([`session`]), served over the loopback transport
//! ([`local`]). The LAN transport waits on discovery landing in WS-H.
//!
//! # Why a daemon at all, on every machine
//!
//! The obvious design is for the GUI app to own its own PTYs and for a daemon to
//! exist only to expose them to other devices. That was rejected: it produces
//! two session paths that drift, and it means closing the window kills the
//! shell — which is exactly wrong for a fleet whose whole point is picking a
//! session up somewhere else.
//!
//! So the daemon owns sessions everywhere, and the GUI app attaches to its own
//! local daemon over a loopback socket exactly as the phone attaches over the
//! network. One attach path, and shells that outlive the window. → ADR-007.
//!
//! # The cost, stated plainly
//!
//! A keystroke now crosses a process boundary. Over a named pipe or a unix
//! socket that is roughly 50–100µs against a 10ms budget, so it is affordable —
//! but *startup* is where this genuinely threatens something already won. The
//! window currently paints in ~50ms and the prompt is on the first frame,
//! because the shell is spawned before GPU initialization and its ~400ms
//! overlaps the driver's ~850ms.
//!
//! Find-or-spawn-daemon must occupy that same slot. It must never sit between
//! the window being created and the window being painted, and on the warm
//! path — the daemon is already running, which is every launch after the first —
//! it is a pipe open costing microseconds. There is a regression test on first
//! paint so this has a number to break rather than a memory to argue with.

pub mod attest_sync;
pub mod audit;
pub mod client;
pub mod auth;
pub mod enroll;
pub mod history;
pub mod lan;
pub mod local;
pub mod offer;
pub mod relay;
pub mod relay_origin;
pub mod server;
pub mod session;
pub mod ws;

pub use auth::{Auth, Authenticator};
pub use lan::{Gate, LanListener};
pub use relay::Relay;
pub use ws::WsListener;
pub use local::{connect, default_socket_path, listen};
#[cfg(windows)]
pub use local::PipeStream;
pub use server::{serve, Connection, Registry};
pub use session::{Session, Update};

use std::sync::Arc;

use zest_proto::{HostId, SessionAddr, SessionId};

/// What a daemon needs to know about itself.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// This machine's identity in the fleet.
    pub host: HostId,
    /// Human name, shown in fleet listings.
    pub label: String,
    /// Where the local socket lives.
    ///
    /// A named pipe on Windows, a unix socket under the runtime dir elsewhere.
    pub local_socket: String,
    /// Accept connections from other machines at all.
    ///
    /// Off by default. A daemon that serves only its own GUI app is the
    /// behaviour someone who never asked for a fleet should get.
    pub listen_lan: bool,
    /// Which interface to serve the LAN on.
    ///
    /// `0.0.0.0` unless pinned. IPv6 dual-stack is deliberately not attempted:
    /// `IPV6_V6ONLY` defaults differ between Windows and unix and `std` cannot
    /// set it without `socket2`, so a v6-only network is a known limitation
    /// rather than something that half-works.
    pub lan_bind: String,
    /// Which port to prefer. 0 means "always ephemeral".
    ///
    /// A preference, not a promise: if it is taken the listener falls back and
    /// advertises what it actually bound.
    pub lan_port: u16,
    /// Accept WebSocket connections — the transport browsers can reach.
    ///
    /// Off by default, same posture as `listen_lan`: a public port is
    /// something someone asks for. Independent of `listen_lan` because the
    /// local web client needs this one without wanting raw TCP served.
    pub listen_ws: bool,
    /// Which interface to serve WebSocket clients on. `0.0.0.0` unless pinned.
    pub ws_bind: String,
    /// Which port to prefer for WebSocket clients. 0 means "always ephemeral".
    pub ws_port: u16,
    /// The relay this machine dials out to, so it can be reached from anywhere.
    ///
    /// `None` by default, and the third transport with that posture: `--relay`
    /// is something someone asks for. Unlike [`Self::listen_lan`] and
    /// [`Self::listen_ws`] it is **outbound** — nothing here opens a port —
    /// which is the whole point of ADR-009: a machine behind a NAT, on hotel
    /// wifi, or on a network whose admin will never forward a port is reachable
    /// because it dialled out.
    ///
    /// A `String` and not a parsed origin, because the failure has to be
    /// reportable: [`relay::RelayOrigin::parse`] refuses several shapes by name
    /// and the daemon logs which, rather than a config that cannot represent
    /// what someone typed.
    pub relay: Option<String>,
    /// Load zesterm's OSC 133 hook into shells this daemon spawns.
    ///
    /// On by default: it writes no file of the user's, and without it there are
    /// no command blocks, which is a whole milestone's worth of the terminal
    /// missing.
    ///
    /// **A daemon flag rather than a settings key**, which is not where anyone
    /// will look for it. The shell runs on the *host*, so whether to hook it is
    /// the host's decision — and `zest-daemon` has no settings reader, because
    /// it does not depend on `zest-config`. Making this a config key means
    /// either that dependency or a new field on the frozen `CreateSession`, and
    /// neither is worth doing before someone needs the switch. Recorded in
    /// `docs/ROADMAP.md` under WS-E rather than left to be rediscovered.
    pub shell_integration: bool,
    /// Least time between two *delta* sends on one connection.
    ///
    /// **Zero — no floor — unless the transport asks for one.** Loopback and
    /// the LAN pay nothing per message and are close (ADR-007's 50–100µs over a
    /// local socket, ADR-006's ~0.3ms across a desk), so a floor there would buy
    /// nothing and cost a keystroke's echo latency. The relay transport
    /// sets ~30ms: incoming messages are billed, and an object that never idles
    /// never hibernates, which is what turns ADR-009's dominant cost term from
    /// zero into continuous.
    ///
    /// **Why a floor is safe here and would not be safe over a byte stream.**
    /// `zest-proto` coalesces on *state*: a subscriber holds an encoder shadow
    /// and asks the terminal for the difference from what it last sent, so a
    /// consumer that skips a hundred polls receives one delta describing the
    /// current grid — not a backlog of a hundred. Nothing queues, so nothing
    /// can be lost by not looking. A throttle over queued bytes would drop the
    /// bytes it skipped; this drops intermediate *frames*, which is the whole
    /// design. → ADR-004, ADR-009.
    pub min_delta_interval: std::time::Duration,
    /// How a loopback connection enrols this machine (`ClientMessage::Enroll`,
    /// issue #227): the control plane's base URL — `--control-plane` or the
    /// default — plus the transport and the store the token lands in.
    ///
    /// `None` under `--ephemeral`, for `--enroll`'s own refusal reason: a key
    /// that dies with the process must not claim an account row nothing can
    /// ever answer for. Also `None` throughout the test harnesses, where a
    /// daemon that could reach a control plane would be a daemon whose tests
    /// fail on an aeroplane.
    pub enroll: Option<EnrollSeam>,
    /// What this machine tells a client it can offer — its facts and its own
    /// profiles (#262), pushed to connections that set `Hello.watch_hosts`.
    ///
    /// `None` means this daemon publishes nothing, which is what every test
    /// harness wants and what a daemon built before this field did. A client
    /// then sees `Sessions { offer: None }` — indistinguishable from an older
    /// daemon, and handled by the same branch.
    pub offer: Option<offer::OfferSource>,
}

/// Everything [`server`] needs to run `enroll::enroll` on a client's behalf.
///
/// The trait objects are the same injection seam `--enroll` has always had —
/// `ControlPlane` exists precisely so a test can watch an enrolment without a
/// socket — carried into the serving path.
#[derive(Clone)]
pub struct EnrollSeam {
    pub base_url: String,
    pub http: Arc<dyn enroll::ControlPlane + Send + Sync>,
    pub secrets: Arc<dyn zest_mesh::keystore::SecretStore>,
}

impl std::fmt::Debug for EnrollSeam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transports are behind dyn on purpose; the URL is the one part
        // of this a log line can act on.
        f.debug_struct("EnrollSeam").field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

/// A session's lifecycle, as clients see it.
///
/// `Detached` is the state that justifies the whole design: the process keeps
/// running, output keeps accumulating, and reattaching from anywhere resumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running { attached: bool },
    /// The child exited; scrollback is still readable until the session is
    /// reaped, so a client can see *why* something died rather than finding a
    /// window that vanished.
    Exited { code: Option<i32> },
}

/// The daemon's view of one session.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    pub state: SessionState,
    pub title: String,
}

impl SessionHandle {
    #[must_use]
    pub fn addr(&self, host: HostId) -> SessionAddr {
        SessionAddr::new(host, self.id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("no session {0}")]
    NoSuchSession(u64),
    #[error("the pty could not be started: {0}")]
    Spawn(String),
    #[error("transport failed: {0}")]
    Transport(String),
    // The three below belong to the client half (`client.rs`) and came down
    // with it from `zest-app`. They are separate variants rather than more
    // `Transport(String)` because the app branches on them: a version mismatch
    // is a message about upgrading, a refusal is a message about pairing, and a
    // closed socket is a retry.
    #[error("daemon speaks protocol {theirs}, this build speaks {ours}")]
    Version { ours: u16, theirs: u16 },
    #[error("the daemon refused this client: {0}")]
    Refused(String),
    #[error("the daemon closed the connection during the handshake")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_session_is_still_running() {
        // The property the whole daemon exists for. If closing a client ever
        // implies stopping the shell, the fleet story is gone.
        let h = SessionHandle {
            id: SessionId(1),
            state: SessionState::Running { attached: false },
            title: "pwsh".into(),
        };
        assert!(matches!(h.state, SessionState::Running { attached: false }));
    }

    #[test]
    fn a_session_is_addressable_in_the_fleet() {
        let h = SessionHandle {
            id: SessionId(4),
            state: SessionState::Running { attached: true },
            title: "vim".into(),
        };
        let addr = h.addr(HostId::from_bytes([3; 32]));
        assert_eq!(addr.session, SessionId(4));
    }
}
