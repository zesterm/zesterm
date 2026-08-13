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

use zest_daemon::client::DaemonClient;
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

/// How often the account listing is re-read while signed in. A minute,
/// jittered ±25% so several windows do not poll the control plane in step;
/// failures back off to five minutes — a listing that cannot be fetched is
/// stale, not urgent.
const ACCOUNT_POLL: Duration = Duration::from_secs(60);
const ACCOUNT_BACKOFF: Duration = Duration::from_secs(300);

/// One machine the account lists, as the fleet consumes it.
///
/// Deliberately not `crate::cloud::AccountHosts`: this module aggregates
/// sources and owns no transport, so it names only the two facts it merges
/// on and lets `app.rs` convert — the same discipline that keeps the roster
/// socket-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    pub host: HostId,
    pub label: String,
}

/// One device the account lists, as the fleet consumes it — the devices
/// section's row data, same minimal-shape discipline as [`AccountEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDevice {
    pub id: zest_proto::ClientId,
    pub label: String,
    /// `browser|phone|desktop`, as the control plane spells it; rendered,
    /// never matched on.
    pub kind: String,
    /// The one status that makes a device trusted. Everything else is
    /// pending, which is the state the Approve affordance exists for.
    pub approved: bool,
}

/// What one account fetch answers: the hosts, where the relay lives, and
/// the devices.
///
/// The origin rides beside the hosts rather than on each row because it is
/// one fact about the deployment, not a per-host one — and `None` is a
/// deployment without a relay, where enrolled-but-unseen hosts are listed
/// and honestly unreachable. The devices ride the same fetch (issue #190's
/// approver leg) rather than a second watcher, because the cadence,
/// the sign-out clearing and the poke-driven refresh are one set of
/// decisions and two watchers would make them twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountListing {
    pub relay_origin: Option<String>,
    pub hosts: Vec<AccountEntry>,
    pub devices: Vec<AccountDevice>,
}

/// Why an account fetch produced no listing.
pub enum AccountError {
    /// There is no token, or the control plane refused it. Polling stops
    /// until a poke says the account state moved — a signed-out watcher
    /// re-fetching every minute would be a drumbeat of 401s about nothing.
    SignedOut,
    /// Unreachable, or an unusable answer. Worth retrying, slowly.
    Transient(String),
}

/// Forces an account refresh out of turn — on sign-in, when the fleet
/// screen is shown, and after an approval changed what the listing says.
/// Clones share the doorbell (the approve worker holds one); the watcher
/// ends when the *last* is dropped.
#[derive(Clone)]
pub struct AccountPoke(crossbeam_channel::Sender<()>);

impl AccountPoke {
    pub fn poke(&self) {
        // `try_send` on a bounded(1) channel: a poke already queued is the
        // same poke, and blocking the event loop on a full channel would be
        // paying for coalescing twice.
        let _ = self.0.try_send(());
    }
}

/// What is known about one host's session list.
#[derive(Debug, Clone, Default)]
#[allow(dead_code, reason = "remote-host fetches (picker-open dials) construct the other states")]
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
#[derive(Clone)]
pub struct FleetHost {
    pub host: HostId,
    pub label: String,
    pub presence: Presence,
    /// This is the machine the window is running on.
    pub local: bool,
    /// The best address to dial, when one is known.
    pub address: Option<String>,
    /// How the best endpoint reaches the host — loopback, LAN, or tunnel.
    /// `None` when nothing is advertised (the synthesized local row).
    pub reachability: Option<zest_mesh::Reachability>,
    /// The prober's last measured round trip to that address, milliseconds.
    /// Measured, not `typical_rtt_ms` — the status bar prints this, and an
    /// honest number is the difference between UI and decoration.
    pub rtt_ms: Option<f32>,
    pub sessions: SessionsState,
    /// The account lists this machine. The durable fact (ROADMAP WS-G:
    /// enrolment is the spine, discovery decorates) — an enrolled host stays
    /// in the listing when mDNS has never heard of it.
    pub enrolled: bool,
}

/// One push from the daemon's approval queue, forwarded to the app.
///
/// Decoded here so the app's modal state never sees a wire type: `Requested`
/// raises the modal, `Resolved` closes it (someone answered — at the stdin
/// prompt, in another window — or the device gave up or timed out).
pub enum PairingEvent {
    Requested {
        client: zest_proto::ClientId,
        label: String,
        remote: String,
        code: String,
        expires_in_secs: u32,
    },
    Resolved {
        client: zest_proto::ClientId,
    },
}

#[derive(Default)]
struct State {
    /// The window's own daemon, learned from its signed Welcome — a default
    /// local daemon is mDNS-invisible (it only advertises with
    /// `--listen-lan`), so it must be synthesized into the listing rather
    /// than expected from discovery.
    local: Option<(HostId, String)>,
    sessions: HashMap<HostId, SessionsState>,
    /// Last successful probe's round trip per host, milliseconds. The probe
    /// was already paying for this connect; keeping the elapsed time is what
    /// turns "LAN direct" into "LAN direct 0.4 ms".
    rtt: HashMap<HostId, f32>,
    discovery: Option<MdnsDiscovery>,
    /// What the account lists, from the last successful fetch. `None` both
    /// before the first fetch and after a sign-out — in either case there is
    /// no account speaking, and the listing decays to discovery alone.
    account: Option<AccountListing>,
}

struct Inner {
    proxy: EventLoopProxy<Wakeup>,
    /// The coalescing latch: many observations, one wakeup, cleared when the
    /// main thread reads the snapshot.
    dirty: AtomicBool,
    state: parking_lot::Mutex<State>,
    /// How to reach the daemon the watcher watches, kept for
    /// [`FleetModel::decide_pairing`] — the decision dials fresh rather than
    /// writing into a watcher parked in `read`.
    decide_dial: parking_lot::Mutex<Option<Arc<dyn Fn() -> Dialer + Send + Sync>>>,
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
            decide_dial: parking_lot::Mutex::new(None),
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
                    let started = std::time::Instant::now();
                    let up = addr
                        .parse()
                        .ok()
                        .and_then(|sa| std::net::TcpStream::connect_timeout(&sa, PROBE_TIMEOUT).ok())
                        .is_some();
                    let rtt_ms = up.then(|| started.elapsed().as_secs_f32() * 1000.0);
                    let changed = {
                        let mut state = inner.state.lock();
                        match rtt_ms {
                            Some(ms) => {
                                // A fresh number is not "changed": redrawing the
                                // fleet every ten seconds because 0.41 became
                                // 0.38 would spend frames saying nothing. Only a
                                // number that would *read* differently wakes the
                                // UI.
                                let old = state.rtt.insert(host, ms);
                                old.is_none_or(|o| (o - ms).abs() / o.max(0.1) > 0.5)
                            }
                            None => state.rtt.remove(&host).is_some(),
                        }
                    };
                    let dial_changed = {
                        let state = inner.state.lock();
                        state.discovery.as_ref().is_some_and(|d| d.report_dial(host, up))
                    };
                    if changed || dial_changed {
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
    pub fn watch(
        &self,
        dial: impl Fn() -> Dialer + Send + Sync + 'static,
        on_pairing: impl Fn(PairingEvent) + Send + Sync + 'static,
    ) {
        let dial = Arc::new(dial);
        *self.inner.decide_dial.lock() = Some(Arc::clone(&dial) as _);
        let on_pairing = Arc::new(on_pairing);
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new().name("zest-fleet-watch".into()).spawn(move || {
            let mut wait = REWATCH_MIN;
            loop {
                match Self::watch_once(&inner, &dial(), on_pairing.as_ref()) {
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

    /// Answer a pairing prompt: allow (or refuse) `client` onto this machine.
    ///
    /// Dials a **fresh** loopback connection for the decision rather than
    /// writing into the watcher, whose thread is parked in `read`. That is
    /// the smallest honest sender: the daemon gates `PairingDecision` on the
    /// *transport* (`may_approve_devices`), not on the connection that heard
    /// the request, so a fresh connection over the same dial carries exactly
    /// the watcher's authority — and a loopback handshake is tens of
    /// microseconds against a decision a person took seconds to make.
    /// Off the event loop, like every dial.
    pub fn decide_pairing(&self, client: zest_proto::ClientId, approve: bool) {
        let Some(dial) = self.inner.decide_dial.lock().clone() else {
            tracing::warn!("no route to the daemon; the pairing decision has nowhere to go");
            return;
        };
        let spawned = std::thread::Builder::new().name("zest-pairing-decide".into()).spawn(
            move || {
                let result = (|| -> Result<(), crate::remote::RemoteError> {
                    let identity = ClientIdentity::generate()
                        .map(Arc::new)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    let (read, write) = (dial())()?;
                    let mut c = DaemonClient::connect(
                        read,
                        write,
                        &identity,
                        "zesterm-approve",
                        None,
                        false,
                    )?;
                    c.decide_pairing(client, approve)?;
                    Ok(())
                })();
                if let Err(e) = result {
                    // The request stays pending on the daemon, so the person
                    // can still answer at its stdin; the modal has closed
                    // optimistically, which the tombstone push reconciles.
                    tracing::warn!(error = %e, approve, "could not deliver the pairing decision");
                }
            },
        );
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no thread for the pairing decision");
        }
    }

    fn watch_once(
        inner: &Arc<Inner>,
        dial: &Dialer,
        on_pairing: &(impl Fn(PairingEvent) + Send + Sync),
    ) -> Result<(), crate::remote::RemoteError> {
        // An ephemeral identity per connection: the watcher only ever talks
        // to the window's own daemon, where the socket is the authorization
        // (the same reasoning as the attach path).
        let identity = ClientIdentity::generate()
            .map(Arc::new)
            .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
        let (read, write) = dial()?;
        let mut client = DaemonClient::connect_watching(
            read,
            write,
            &identity,
            "zesterm-fleet",
            None,
            // Pairings too: this is the app's one standing loopback
            // connection, and the approval modal is its second tenant. On a
            // remote daemon the flag is silently not honoured, which is the
            // right degradation — that machine's approvals are not ours.
            zest_daemon::client::Watch { sessions: true, pairings: true },
        )?;
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
            match client.next_message()? {
                zest_proto::HostMessage::Sessions { sessions, .. } => {
                    let mut state = inner.state.lock();
                    state.sessions.insert(host, SessionsState::Fresh(sessions));
                    drop(state);
                    inner.mark_changed();
                }
                zest_proto::HostMessage::PairingRequested {
                    client: device,
                    label,
                    code,
                    remote,
                    expires_in_secs,
                    resolved,
                } => {
                    on_pairing(if resolved {
                        PairingEvent::Resolved { client: device }
                    } else {
                        PairingEvent::Requested {
                            client: device,
                            label,
                            remote,
                            code,
                            expires_in_secs,
                        }
                    });
                }
                _ => {}
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
                reachability: Some(zest_mesh::Reachability::Loopback),
                rtt_ms: state.rtt.get(host).copied(),
                sessions: state.sessions.get(host).cloned().unwrap_or_default(),
                enrolled: false,
            });
        }

        if let Some(d) = state.discovery.as_ref() {
            for record in d.records() {
                // The local daemon may also advertise (--listen-lan); the
                // synthesized row above already covers it.
                if state.local.as_ref().is_some_and(|(h, _)| *h == record.peer.host) {
                    continue;
                }
                let best = record.peer.best_endpoint();
                out.push(FleetHost {
                    host: record.peer.host,
                    label: record.peer.label.clone(),
                    presence: record.presence,
                    local: false,
                    address: best.map(|e| e.address.clone()),
                    reachability: best.map(|e| e.reachability),
                    rtt_ms: state.rtt.get(&record.peer.host).copied(),
                    sessions: state
                        .sessions
                        .get(&record.peer.host)
                        .cloned()
                        .unwrap_or_default(),
                    enrolled: false,
                });
            }
        }

        merge_account(&mut out, state.account.as_ref().map(|l| l.hosts.as_slice()));
        out
    }

    /// Where the account says the relay lives, when a listing has one.
    ///
    /// Read separately from `snapshot()` because it is a deployment fact,
    /// not a host row — `best_route` asks it once per route, not per card
    /// field.
    #[must_use]
    pub fn relay_origin(&self) -> Option<String> {
        self.inner.state.lock().account.as_ref().and_then(|l| l.relay_origin.clone())
    }

    /// The account's devices, from the last successful fetch; empty both
    /// before the first and after a sign-out — the same decay to nothing as
    /// the enrolled hosts.
    #[must_use]
    pub fn devices(&self) -> Vec<AccountDevice> {
        self.inner
            .state
            .lock()
            .account
            .as_ref()
            .map(|l| l.devices.clone())
            .unwrap_or_default()
    }

    /// Keep the account's host listing fresh, off the main thread.
    ///
    /// `fetch` is the whole transport (the `watch(dial)` shape): fleet.rs
    /// never learns what a token or an HTTP client is, and the tests below
    /// drive a real poll loop with a closure and no network. Results land
    /// through the same lock + latch as every other source.
    pub fn watch_account(
        &self,
        fetch: impl Fn() -> Result<AccountListing, AccountError> + Send + 'static,
    ) -> AccountPoke {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new().name("zest-fleet-account".into()).spawn(
            move || {
                account_loop(
                    &fetch,
                    &|entries| {
                        inner.state.lock().account = entries;
                        inner.mark_changed();
                    },
                    &rx,
                    ACCOUNT_POLL,
                    ACCOUNT_BACKOFF,
                );
            },
        );
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no account watcher; enrolled hosts will not be listed");
        }
        AccountPoke(tx)
    }
}

/// Lay the account's listing over what discovery built (ROADMAP WS-G:
/// enrolment is the spine, discovery decorates). A host both know is one row
/// keeping its observed label, address and presence — the live facts — with
/// `enrolled` flipped; a host only the account knows is appended with the
/// account's label and no address, reached (once the dialler lands) only
/// through the tunnel, and `Unseen` because nothing local has observed it.
///
/// Free of `State` so the tests can drive it with hand-built rows.
fn merge_account(out: &mut Vec<FleetHost>, account: Option<&[AccountEntry]>) {
    let Some(entries) = account else { return };
    for entry in entries {
        if let Some(seen) = out.iter_mut().find(|h| h.host == entry.host) {
            seen.enrolled = true;
        } else {
            out.push(FleetHost {
                host: entry.host,
                label: entry.label.clone(),
                presence: Presence::Unseen,
                local: false,
                address: None,
                reachability: Some(zest_mesh::Reachability::Cloud),
                rtt_ms: None,
                sessions: SessionsState::default(),
                enrolled: true,
            });
        }
    }
}

/// The account poll loop, free of the thread and the event loop so the tests
/// can drive it with injected closures and no winit proxy.
///
/// One channel is both the timer and the doorbell: `recv_timeout` is the
/// poll interval, and a poke arriving early is simply the wait ending early.
/// Signed out, the wait has no timeout at all — the *structure* is what
/// guarantees a parked watcher cannot poll, rather than a flag somebody has
/// to remember to check.
fn account_loop(
    fetch: &dyn Fn() -> Result<AccountListing, AccountError>,
    store: &dyn Fn(Option<AccountListing>),
    poke: &crossbeam_channel::Receiver<()>,
    poll: Duration,
    backoff: Duration,
) {
    loop {
        let wait = match fetch() {
            Ok(entries) => {
                store(Some(entries));
                jittered(poll)
            }
            Err(AccountError::SignedOut) => {
                // The listing is the account's word; signed out, there is no
                // account speaking, and keeping the old rows would show
                // machines this window can no longer reach as if it could.
                store(None);
                match poke.recv() {
                    Ok(()) => continue,
                    // The poke was dropped: the app is going away.
                    Err(_) => return,
                }
            }
            Err(AccountError::Transient(e)) => {
                tracing::debug!(error = %e, "account listing unavailable");
                backoff
            }
        };
        match poke.recv_timeout(wait) {
            Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// `base` ±25%, salted by the clock's nanoseconds. The point is only that
/// several windows' polls decorrelate; the quality of the randomness is
/// irrelevant, which is why this is arithmetic and not a `rand` dependency.
fn jittered(base: Duration) -> Duration {
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    jittered_from(base, salt)
}

/// The salt→interval map, split out so the bounds are testable without a
/// clock: salt buckets to [0.75, 1.25) of `base`.
fn jittered_from(base: Duration, salt: u32) -> Duration {
    base.mul_f64(0.75 + f64::from(salt % 1000) / 2000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn discovered(host: HostId, label: &str) -> FleetHost {
        FleetHost {
            host,
            label: label.into(),
            presence: Presence::Online,
            local: false,
            address: Some("192.168.1.9:7717".into()),
            reachability: Some(zest_mesh::Reachability::Lan),
            rtt_ms: Some(0.4),
            sessions: SessionsState::default(),
            enrolled: false,
        }
    }

    #[test]
    fn a_host_both_sources_know_is_one_row_carrying_both_facts() {
        let id = HostId::from_bytes([1; 32]);
        let mut out = vec![discovered(id, "studio")];
        merge_account(
            &mut out,
            Some(&[AccountEntry { host: id, label: "studio (enrolled label)".into() }]),
        );

        assert_eq!(out.len(), 1, "merge is by id; two rows for one machine would offer the \
             same shell twice and let the copies disagree");
        assert!(out[0].enrolled, "the account's word survives the merge");
        assert_eq!(
            out[0].address.as_deref(),
            Some("192.168.1.9:7717"),
            "discovery's decoration survives too — the LAN route is the better one and \
             must not be erased by the account knowing the machine"
        );
        assert_eq!(
            out[0].label, "studio",
            "the advertised label wins: the daemon speaks for its current name, the \
             account row remembers whatever it was enrolled as"
        );
    }

    #[test]
    fn an_account_only_host_is_listed_durable_with_nothing_it_does_not_have() {
        let id = HostId::from_bytes([2; 32]);
        let mut out = Vec::new();
        merge_account(&mut out, Some(&[AccountEntry { host: id, label: "attic-pc".into() }]));

        assert_eq!(out.len(), 1, "an enrolled host is in the listing whether or not the \
             LAN has ever seen it — that durability is the account's whole contribution");
        let row = &out[0];
        assert!(row.enrolled);
        assert_eq!(row.label, "attic-pc", "the account's label is the only one there is");
        assert_eq!(row.address, None, "no address may be invented for it");
        assert_eq!(
            row.reachability,
            Some(zest_mesh::Reachability::Cloud),
            "the only conceivable path is the tunnel, and the card says so"
        );
        assert_eq!(
            row.presence,
            Presence::Unseen,
            "nothing local has observed it, and Unseen is exactly that claim"
        );
        assert!(!row.local);
    }

    #[test]
    fn no_account_listing_leaves_discovery_alone() {
        let id = HostId::from_bytes([3; 32]);
        let mut out = vec![discovered(id, "studio")];
        merge_account(&mut out, None);
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].enrolled,
            "signed out (or never fetched), no row may claim the account's word"
        );
    }

    #[test]
    fn signed_out_parks_the_poll_until_a_poke() {
        // Call counts and ordering only — this repo's flake history says
        // never to assert the jittered interval against a wall clock. The
        // 1ms poll makes the loop's own cadence irrelevant to the test.
        let calls = Arc::new(AtomicUsize::new(0));
        let stored = Arc::new(parking_lot::Mutex::new(Vec::<bool>::new()));
        let (seen_tx, seen_rx) = crossbeam_channel::unbounded();
        let (poke_tx, poke_rx) = crossbeam_channel::bounded(1);

        let handle = {
            let calls = Arc::clone(&calls);
            let stored = Arc::clone(&stored);
            std::thread::spawn(move || {
                let listing = || AccountListing {
                    relay_origin: Some("wss://relay.example".into()),
                    hosts: Vec::new(),
                    devices: Vec::new(),
                };
                let fetch = move || {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    let _ = seen_tx.send(n);
                    match n {
                        1 => Ok(listing()),
                        2 => Err(AccountError::SignedOut),
                        _ => Ok(listing()),
                    }
                };
                let store = move |v: Option<AccountListing>| stored.lock().push(v.is_some());
                account_loop(
                    &fetch,
                    &store,
                    &poke_rx,
                    Duration::from_millis(1),
                    Duration::from_millis(1),
                );
            })
        };

        let deadline = Duration::from_secs(10);
        assert_eq!(seen_rx.recv_timeout(deadline), Ok(1), "the first fetch is immediate");
        assert_eq!(seen_rx.recv_timeout(deadline), Ok(2), "the poll continues while signed in");
        // Structurally parked (a recv with no timeout), so with a 1ms poll a
        // broken loop would land dozens of fetches in this window.
        assert!(
            seen_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a signed-out watcher must not keep polling — every poll would be a 401"
        );

        let poke = AccountPoke(poke_tx);
        poke.poke();
        assert_eq!(
            seen_rx.recv_timeout(deadline),
            Ok(3),
            "a poke is how sign-in resumes the listing"
        );
        assert_eq!(
            &stored.lock()[..2],
            &[true, false],
            "sign-out must clear the stored listing, not merely stop refreshing it — \
             stale rows would show machines this window can no longer reach"
        );

        // Dropping the poke is how the app ends the watcher.
        drop(poke);
        handle.join().expect("the loop exits when the poke is dropped");
    }

    #[test]
    fn the_jittered_interval_stays_within_a_quarter_of_the_base() {
        let base = Duration::from_secs(60);
        let (lo, hi) = (base.mul_f64(0.75), base.mul_f64(1.25));
        for salt in 0..2000 {
            let d = jittered_from(base, salt);
            assert!(
                d >= lo && d < hi,
                "salt {salt}: {d:?} outside [{lo:?}, {hi:?}) — drift below turns the poll \
                 into a hammer, drift above makes the listing stale"
            );
        }
        assert_ne!(
            jittered_from(base, 0),
            jittered_from(base, 999),
            "if every salt lands on one value there is no jitter and every window polls in step"
        );
    }
}
