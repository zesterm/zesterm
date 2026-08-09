//! The app's view of the fleet: hosts, presence, and their sessions.
//!
//! Aggregates three sources the picker draws from — mDNS discovery (which
//! hosts exist and where), the dial prober (whether an advertised host
//! actually answers, #22's `Unreachable`), and a watching connection to the
//! window's own daemon (its live session list, pushed via
//! `Hello.watch_sessions`). All of it runs off the main thread and posts one
//! coalesced [`Wakeup::FleetChanged`] per burst of change, so the 0%-idle
//! guarantee survives a chatty network.
//!
//! The roster itself stays socket-free by design (`zest-mesh`); the prober
//! here is the app-owned dialer that feeds it evidence, exactly as
//! `mesh_probe` does for the CLI. Nothing else in the process ever makes
//! `Presence::Unreachable` appear.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use winit::event_loop::EventLoopProxy;
use zest_mesh::discovery::mdns::MdnsDiscovery;
use zest_mesh::discovery::{Discovery, Presence};
use zest_mesh::identity::ClientIdentity;
use zest_proto::{HostId, SessionInfo};

use crate::daemon_client::DaemonClient;
use crate::remote::Dialer;
use crate::session::Wakeup;

/// How often the prober re-tests hosts nothing is connected to.
///
/// Ten seconds, matching `mesh_probe`: a listing is at most ten seconds
/// wrong, and the traffic is one TCP connect per quiet host per interval.
const PROBE_INTERVAL: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait before re-establishing a broken watch connection.
const REWATCH_MIN: Duration = Duration::from_millis(500);
const REWATCH_MAX: Duration = Duration::from_secs(10);

/// What is known about one host's session list.
#[derive(Debug, Clone, Default)]
pub enum SessionsState {
    /// Never asked.
    #[default]
    Unknown,
    /// A listing is on its way.
    Fetching,
    Fresh(Vec<SessionInfo>),
    Failed(String),
}

/// One row of the fleet, ready for the picker.
pub struct FleetHost {
    pub host: HostId,
    pub label: String,
    pub presence: Presence,
    /// This is the machine the window is running on.
    pub local: bool,
    /// The best address to dial, when one is known.
    pub address: Option<String>,
    pub sessions: SessionsState,
}

#[derive(Default)]
struct State {
    /// The window's own daemon, learned from its signed Welcome — a default
    /// local daemon is mDNS-invisible (it only advertises with
    /// `--listen-lan`), so it must be synthesized into the listing rather
    /// than expected from discovery.
    local: Option<(HostId, String)>,
    sessions: HashMap<HostId, SessionsState>,
    discovery: Option<MdnsDiscovery>,
}

struct Inner {
    proxy: EventLoopProxy<Wakeup>,
    /// The coalescing latch: many observations, one wakeup, cleared when the
    /// main thread reads the snapshot.
    dirty: AtomicBool,
    state: parking_lot::Mutex<State>,
}

impl Inner {
    fn mark_changed(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.send_event(Wakeup::FleetChanged);
        }
    }
}

pub struct FleetModel {
    inner: Arc<Inner>,
}

impl FleetModel {
    /// Start browsing and probing. Called after first paint, never before:
    /// none of this is needed to show a prompt (ADR-007's budget).
    ///
    /// `local` is the window's own daemon, when it has one.
    pub fn start(proxy: EventLoopProxy<Wakeup>, local: Option<(HostId, String)>) -> Self {
        let local_id = local.as_ref().map_or(HostId::from_bytes([0; 32]), |(h, _)| *h);
        let inner = Arc::new(Inner {
            proxy,
            dirty: AtomicBool::new(false),
            state: parking_lot::Mutex::new(State { local, ..State::default() }),
        });

        // The browser: observations land in the roster on the mesh thread;
        // the callback is a bare "something changed", after which the main
        // thread re-reads snapshots. Exactly the Wakeup shape the app uses.
        let mut discovery = {
            let inner = Arc::clone(&inner);
            MdnsDiscovery::new(local_id).on_change(move || inner.mark_changed())
        };
        if let Err(e) = discovery.start() {
            // No mDNS is a degraded fleet view, not a broken terminal.
            tracing::warn!(error = %e, "fleet discovery unavailable");
        } else {
            inner.state.lock().discovery = Some(discovery);
        }

        Self::spawn_prober(&inner);
        Self { inner }
    }

    /// The dial prober: the only thing that can tell a crashed host from a
    /// sleeping one. mDNS caches keep a killed daemon's record for up to 75
    /// minutes; a refused TCP connect is evidence, and `report_dial` is how
    /// the roster hears it (#22).
    fn spawn_prober(inner: &Arc<Inner>) {
        let inner = Arc::clone(inner);
        let spawned = std::thread::Builder::new().name("zest-fleet-probe".into()).spawn(move || {
            loop {
                std::thread::sleep(PROBE_INTERVAL);
                // Snapshot under the lock, dial outside it.
                let targets: Vec<(HostId, String)> = {
                    let state = inner.state.lock();
                    let Some(d) = state.discovery.as_ref() else { continue };
                    d.records()
                        .into_iter()
                        .filter(|r| {
                            matches!(r.presence, Presence::Online | Presence::Unreachable)
                        })
                        .filter_map(|r| {
                            let addr = r.peer.best_endpoint()?.address.clone();
                            Some((r.peer.host, addr))
                        })
                        .collect()
                };
                for (host, addr) in targets {
                    let up = addr
                        .parse()
                        .ok()
                        .and_then(|sa| std::net::TcpStream::connect_timeout(&sa, PROBE_TIMEOUT).ok())
                        .is_some();
                    let changed = {
                        let state = inner.state.lock();
                        state.discovery.as_ref().is_some_and(|d| d.report_dial(host, up))
                    };
                    if changed {
                        inner.mark_changed();
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no fleet prober; Unreachable will never be shown");
        }
    }

    /// Keep one watching connection to a daemon, holding its session list
    /// fresh. The daemon pushes (`Hello.watch_sessions`); an older daemon
    /// that never pushes leaves the first listing standing, which is what a
    /// picker-open refresh (later) is for.
    ///
    /// Reconnects with backoff forever: this is a background view, and "the
    /// daemon restarted" should heal without anyone noticing.
    pub fn watch(&self, dial: impl Fn() -> Dialer + Send + 'static) {
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new().name("zest-fleet-watch".into()).spawn(move || {
            let mut wait = REWATCH_MIN;
            loop {
                match Self::watch_once(&inner, &dial()) {
                    Ok(()) => wait = REWATCH_MIN,
                    Err(e) => {
                        tracing::debug!(error = %e, "fleet watch connection ended");
                    }
                }
                std::thread::sleep(wait);
                wait = (wait * 2).min(REWATCH_MAX);
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no fleet watcher; session lists will be stale");
        }
    }

    fn watch_once(inner: &Arc<Inner>, dial: &Dialer) -> Result<(), crate::remote::RemoteError> {
        // An ephemeral identity per connection: the watcher only ever talks
        // to the window's own daemon, where the socket is the authorization
        // (the same reasoning as the attach path).
        let identity = ClientIdentity::generate()
            .map(Arc::new)
            .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
        let (read, write) = dial()?;
        let mut client =
            DaemonClient::connect(read, write, &identity, "zesterm-fleet", None, true)?;
        let host = client.host();

        let sessions = client.list()?;
        {
            let mut state = inner.state.lock();
            state.sessions.insert(host, SessionsState::Fresh(sessions));
        }
        inner.mark_changed();

        // Block on pushes until the connection dies; every push is the whole
        // current truth for this host.
        loop {
            let msg = client.next_message()?;
            if let zest_proto::HostMessage::Sessions { sessions, .. } = msg {
                let mut state = inner.state.lock();
                state.sessions.insert(host, SessionsState::Fresh(sessions));
                drop(state);
                inner.mark_changed();
            }
        }
    }

    /// Clear the change latch; returns whether anything had changed. The
    /// main thread calls this when it consumes [`Wakeup::FleetChanged`].
    pub fn take_changed(&self) -> bool {
        self.inner.dirty.swap(false, Ordering::AcqRel)
    }

    /// The fleet as of now: the window's own host first, then the roster in
    /// its stable order (the roster is a BTreeMap precisely so listings do
    /// not reshuffle between polls).
    #[must_use]
    pub fn snapshot(&self) -> Vec<FleetHost> {
        let state = self.inner.state.lock();
        let mut out = Vec::new();

        if let Some((host, label)) = &state.local {
            out.push(FleetHost {
                host: *host,
                label: label.clone(),
                presence: Presence::Online,
                local: true,
                address: None,
                sessions: state.sessions.get(host).cloned().unwrap_or_default(),
            });
        }

        if let Some(d) = state.discovery.as_ref() {
            for record in d.records() {
                // The local daemon may also advertise (--listen-lan); the
                // synthesized row above already covers it.
                if state.local.as_ref().is_some_and(|(h, _)| *h == record.peer.host) {
                    continue;
                }
                let address = record.peer.best_endpoint().map(|e| e.address.clone());
                out.push(FleetHost {
                    host: record.peer.host,
                    label: record.peer.label.clone(),
                    presence: record.presence,
                    local: false,
                    address,
                    sessions: state
                        .sessions
                        .get(&record.peer.host)
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }

        out
    }
}
