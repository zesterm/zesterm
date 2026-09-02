//! The app's view of the fleet: hosts, presence, and their sessions.
//!
//! Aggregates four sources the picker draws from — mDNS discovery (which
//! hosts exist and where), the dial prober (whether an advertised host
//! actually answers, #22's `Unreachable`), the account listing (which machines
//! are ours, and whether the relay can reach them), and **one watching
//! connection per reachable host** (its live session list and its published
//! profiles, pushed via `Hello.watch_sessions` / `watch_hosts`). All of it runs
//! off the main thread and posts one coalesced [`Wakeup::FleetChanged`] per
//! burst of change, so the 0%-idle guarantee survives a chatty network.
//!
//! **The last of those was one connection until #265** — the window's own
//! daemon, over loopback — so every remote host's session list was `Unknown`
//! for ever and four surfaces were wrong because of it. The window's machine is
//! still watched by [`FleetModel::watch`], because that connection also carries
//! the approval queue; every other host is watched by the supervisor, which
//! reconciles [`watcher_plan`] against the roster whenever something changes.
//!
//! The roster itself stays socket-free by design (`zest-mesh`); the prober
//! here is the app-owned dialer that feeds it evidence, exactly as
//! `mesh_probe` does for the CLI. Nothing else in the process ever makes
//! `Presence::Unreachable` appear.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use winit::event_loop::EventLoopProxy;
use zest_mesh::discovery::mdns::MdnsDiscovery;
use zest_mesh::discovery::{Discovery, Presence};
use zest_mesh::identity::ClientIdentity;
use zest_proto::{frame, BlockMatch, ClientMessage, HostId, HostMessage};

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

/// The remote-watcher supervisor's backstop, for when nothing rings its
/// doorbell (#265).
///
/// Reconciliation is normally driven by observations — a host appearing over
/// mDNS or in the account listing rings it on the same change that made the
/// host visible. This is the case neither observes: a machine that *is*
/// advertising, whose daemon was down when we last dialled, comes back with
/// nothing new for discovery to say. Fifteen seconds, against a reconcile that
/// is a map comparison when there is nothing to do.
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(15);

/// Most blocks one host returns to the palette per query. The merge caps
/// again after every host has answered, so this bounds one answer's size
/// rather than the list's.
pub const PALETTE_LIMIT: u32 = 32;

/// How many questions a watcher's outbound lane holds before a `try_send`
/// fails (#527). Small on purpose: a writer stalled on a dead relay socket
/// must not queue a keystroke per frame, and a superseded query is worthless
/// anyway — the palette only ever wants the newest one answered.
const LANE_DEPTH: usize = 4;

/// How often the account listing is re-read while signed in. A minute,
/// jittered ±25% so several windows do not poll the control plane in step;
/// failures back off to five minutes — a listing that cannot be fetched is
/// stale, not urgent.
const ACCOUNT_POLL: Duration = Duration::from_secs(60);
const ACCOUNT_BACKOFF: Duration = Duration::from_secs(300);


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

/// The fleet vocabulary, re-exported where this crate has always named it.
///
/// [`zest_fleet::FleetHost`] is what is *known* about a machine and is now
/// built by consumers outside this crate too — `zest-mcp` fills the same rows
/// from mDNS and the account listing, without this module's threads, latch or
/// event-loop proxy. Knowing which machines exist was never the part that had
/// to stay here; owning a live model of them is.
pub use zest_fleet::{merge_account, AccountEntry, FleetHost, SessionsState};

use crate::route::Dial as _;

/// Which remote hosts should be watched, and which watchers should stop
/// (#265).
///
/// Pure over the facts, so the rule is a `cargo test` rather than something
/// only two machines and a network can demonstrate. [`FleetModel`]'s
/// supervisor is the plumbing around it.
#[derive(Debug, Default, PartialEq)]
pub struct WatcherPlan {
    /// Hosts with a route and no watcher yet, and how to reach each.
    pub start: Vec<(HostId, crate::route::HostRoute)>,
    /// Hosts being watched that no longer have a route.
    pub stop: Vec<HostId>,
}

/// Decide the plan.
///
/// **The local host is never in it.** The window's own machine is watched by
/// the connection `FleetModel::watch` holds — the one that also carries the
/// approval queue — so including it here would open a second connection to the
/// same daemon and double every listing push.
///
/// Routability is [`crate::route::best_route`], not a second rule: a host this
/// can reach is exactly a host a fleet card or a ⌘K row can open (#250). That
/// also means a machine reachable only through the relay is watched, which is
/// the case the whole tunnel exists for.
#[must_use]
pub fn watcher_plan(
    hosts: &[FleetHost],
    watching: &std::collections::BTreeSet<HostId>,
    relay_origin: Option<&str>,
    signed_in: bool,
) -> WatcherPlan {
    let mut start = Vec::new();
    let mut routable = std::collections::BTreeSet::new();
    for host in hosts.iter().filter(|h| !h.local) {
        let Some(route) = crate::route::best_route(host, None, relay_origin, signed_in) else {
            continue;
        };
        routable.insert(host.host);
        if !watching.contains(&host.host) {
            start.push((host.host, route));
        }
    }
    let stop = watching.difference(&routable).copied().collect();
    WatcherPlan { start, stop }
}

/// How long to wait before asking an unavailable credential store again.
///
/// The supervisor reconciles on every observation, and a machine with no store
/// would otherwise re-enter it on every mDNS packet. On macOS that path can
/// raise a Keychain prompt, which turns one refusal into a stream of dialogs —
/// so the retry is deliberately slow rather than absent: a locked keychain does
/// get unlocked, and a fleet that stayed empty until the next launch would be
/// the worse failure.
const IDENTITY_RETRY: Duration = Duration::from_secs(60);

/// The supervisor's device key, and what it has already said about not having
/// one.
#[derive(Default)]
struct IdentityCache {
    key: Option<Arc<ClientIdentity>>,
    /// The warning has been emitted; later failures go to `debug`.
    complained: bool,
    failed_at: Option<std::time::Instant>,
}

impl IdentityCache {
    /// Whether the store is worth asking right now.
    fn may_retry(&self, now: std::time::Instant) -> bool {
        self.failed_at.is_none_or(|at| now.duration_since(at) >= IDENTITY_RETRY)
    }
}

/// Write one host's listing into the model, unless its watcher has been retired.
///
/// Pure over the state, so the rule the race turns on is a `cargo test` rather
/// than a comment: the caller holds the same lock the supervisor clears under,
/// and hands the flag it read inside it.
fn apply_listing(
    state: &mut State,
    host: HostId,
    retired: bool,
    sessions: Vec<zest_proto::SessionInfo>,
    offer: Option<zest_proto::HostOffer>,
) -> bool {
    if retired {
        return false;
    }
    state.sessions.insert(host, SessionsState::Fresh(sessions));
    // Absent means "nothing new to say", never "it has none".
    if let Some(offer) = offer {
        state.offers.insert(host, offer);
    }
    true
}

/// Park one host's answer to the palette's block search, unless it is late.
///
/// Late in either of two ways, and both are dropped rather than parked: the
/// watcher was retired (the `apply_listing` race, for matches), or the
/// palette has typed on since — the echoed `query` is the correlation, and
/// an answer to a question nobody is asking any more must not overwrite the
/// one they are. Pure over the state for the reason `apply_listing` is.
fn receive_matches(
    state: &mut State,
    host: HostId,
    retired: bool,
    query: &str,
    matches: Vec<BlockMatch>,
) -> bool {
    if retired {
        return false;
    }
    let Some(search) = state.search.as_mut() else { return false };
    if search.query != query {
        return false;
    }
    search.answers.insert(host, matches);
    true
}

/// A host answered the search with its generic could-not-understand
/// `Error`: a daemon predating `SearchBlocks` (the `Enroll` bargain, which
/// is how every reply-only pair degrades). Terminal for that host: it leaves
/// `asked` so the query row can settle, and it is not asked again for as
/// long as this lane lives — a redial may reach an upgraded daemon, and the
/// lane's exit clears the mark. Without this the row said `2 of 3 hosts
/// searched` for ever, waiting on a machine that had already answered.
fn decline_search(state: &mut State, host: HostId, retired: bool) -> bool {
    if retired {
        return false;
    }
    state.declined.insert(host);
    state.search.as_mut().is_some_and(|s| s.asked.remove(&host))
}

/// Ask every host with a live lane the palette's question, and remember
/// which ones the question actually reached.
///
/// A `try_send` that fails is not "asked": the lane is full because that
/// host's writer is stalled, and counting it would leave the query row
/// saying `2 of 3 hosts searched` for a host that will never answer. The
/// question replaces any earlier one wholesale — the palette wants one
/// answer set, for the query it shows now.
fn ask_lanes(state: &mut State, query: &str) {
    let msg = ClientMessage::SearchBlocks { query: query.to_string(), limit: PALETTE_LIMIT };
    let mut search = BlockSearch { query: query.to_string(), ..Default::default() };
    for (host, lane) in &state.links {
        if state.declined.contains(host) {
            continue;
        }
        if lane.try_send(msg.clone()).is_ok() {
            search.asked.insert(*host);
        }
    }
    state.search = Some(search);
}

/// The palette's block search in flight: what was asked, whom it reached,
/// and who has answered (#527).
#[derive(Default)]
struct BlockSearch {
    query: String,
    /// Hosts the question actually reached — a full lane is not asked.
    asked: BTreeSet<HostId>,
    answers: HashMap<HostId, Vec<BlockMatch>>,
}

/// A snapshot of [`BlockSearch`] for the picker: the query the answers are
/// for, how many hosts were asked, and every answer so far.
#[derive(Debug, Clone, Default)]
pub struct BlockSearchView {
    pub query: String,
    pub asked: usize,
    pub answered: Vec<(HostId, Vec<BlockMatch>)>,
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
    /// What each host said it can offer — its facts and its own profiles
    /// (#262). Sticky on purpose: an absent `offer` on a session push means
    /// "nothing new to say", so a host keeps its last answer until it sends a
    /// different one or the watcher drops it.
    offers: HashMap<HostId, zest_proto::HostOffer>,
    /// Last successful probe's round trip per host, milliseconds. The probe
    /// was already paying for this connect; keeping the elapsed time is what
    /// turns "LAN direct" into "LAN direct 0.4 ms".
    rtt: HashMap<HostId, f32>,
    discovery: Option<MdnsDiscovery>,
    /// What the account lists, from the last successful fetch. `None` both
    /// before the first fetch and after a sign-out — in either case there is
    /// no account speaking, and the listing decays to discovery alone.
    account: Option<AccountListing>,
    /// The remote hosts a watcher is currently held open to, and the flag
    /// that asks each one to stop (#265).
    ///
    /// Keyed by `HostId` rather than by label: a machine that gets renamed is
    /// the same machine, and re-dialling it because its display name changed
    /// would drop a live session list for a cosmetic edit.
    watchers: HashMap<HostId, Arc<AtomicBool>>,
    /// One outbound lane per live watcher, keyed by host (#527). A
    /// watcher's reader parks in `read`; this is the half that can still
    /// speak, so the palette's question reaches a host without a fresh
    /// dial — which over the relay is a TLS handshake and a ticket per
    /// keystroke. Registered by the watcher itself once its listing is in,
    /// removed by whichever of the watcher and the supervisor goes first.
    links: HashMap<HostId, crossbeam_channel::Sender<ClientMessage>>,
    /// The palette's block search in flight, if any.
    search: Option<BlockSearch>,
    /// Hosts whose daemon answered a search with "could not understand":
    /// too old to be asked, until their lane is replaced.
    declined: BTreeSet<HostId>,
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
    /// Nudges the remote-watcher supervisor to reconcile (#265).
    ///
    /// Rung by [`Inner::mark_changed`], so a host appearing on the network or
    /// in the account listing gets a watcher on the same observation that made
    /// it visible, rather than at the next poll boundary. `bounded(1)` and
    /// `try_send`: a reconcile already queued is the same reconcile, and the
    /// discovery thread must never block on it.
    reconcile: crossbeam_channel::Sender<()>,
}

impl Inner {
    fn mark_changed(&self) {
        // The supervisor is rung on every observation, not only the first of a
        // burst: the UI latch below coalesces because a repaint is expensive,
        // while a reconcile is a map comparison and must not be skipped — the
        // *second* change in a burst is often the one that adds a host.
        let _ = self.reconcile.try_send(());
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
        let (reconcile, reconcile_rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new(Inner {
            proxy,
            dirty: AtomicBool::new(false),
            state: parking_lot::Mutex::new(State { local, ..State::default() }),
            decide_dial: parking_lot::Mutex::new(None),
            reconcile,
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
        Self::spawn_supervisor(&inner, reconcile_rx);
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
            //
            // Hosts too (#262): what this machine can offer — its facts and
            // its own profiles. Honoured on every transport, unlike pairings,
            // because reading a machine's launch targets is the whole point.
            zest_daemon::client::Watch {
                sessions: true,
                pairings: true,
                hosts: true,
                // The fleet watcher attaches to no session, so it can never
                // be sent one.
                signals: false,
            },
        )?;
        let host = client.host();

        // `list_with_offer`, not `list`: the first offer rides this very
        // reply, and the daemon marks it sent on the way out — dropping it
        // here would leave the launcher waiting for a generation bump that
        // only a config edit on the far machine can produce.
        let (sessions, offer) = client.list_with_offer()?;
        {
            let mut state = inner.state.lock();
            state.sessions.insert(host, SessionsState::Fresh(sessions));
            if let Some(offer) = offer {
                state.offers.insert(host, offer);
            }
        }
        inner.mark_changed();

        // Block on pushes until the connection dies; every push is the whole
        // current truth for this host. The window's own watcher is never
        // retired, so its stop flag is never set.
        let never = AtomicBool::new(false);
        Self::serve_watch(inner, host, client, &never, |message| {
            match message {
                zest_proto::HostMessage::Sessions { sessions, offer, .. } => {
                    let mut state = inner.state.lock();
                    state.sessions.insert(host, SessionsState::Fresh(sessions));
                    // Absent means "nothing new to say", never "it has none":
                    // an ordinary session push carries no offer, and clearing
                    // on one would blank the launcher's rows every time
                    // somebody opened a shell.
                    if let Some(offer) = offer {
                        state.offers.insert(host, offer);
                    }
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
                zest_proto::HostMessage::BlockMatches { query, matches, .. } => {
                    Self::receive(inner, host, &never, &query, matches);
                }
                // The search is the only question this connection asks
                // once it is streaming, so a sessionless `Error` is that
                // question not being understood.
                zest_proto::HostMessage::Error { session: None, message } => {
                    Self::refuse(inner, host, &never, &message);
                }
                _ => {}
            }
            true
        })
    }


    /// Keep one watching connection open to every remote host that has a
    /// route (#265).
    ///
    /// **The hole this closes.** Until now exactly one connection existed —
    /// the window's own daemon, over loopback — so every remote
    /// `FleetHost.sessions` was `Unknown` for ever. Four surfaces were wrong
    /// because of it, each reading as its own small bug: the ⌘K palette's
    /// Sessions group was local-only, a fleet card showed no session count for
    /// any machine but this one, the vertical sidebar's host groups held only
    /// tabs already open here, and `FleetHost::offer` — a machine's published
    /// profiles, which is the launcher's whole input (#262) — was never filled.
    ///
    /// One connection per reachable host, held open, which is the honest cost
    /// of a live listing and exactly what the browser already pays
    /// (`clients/web/packages/app/src/live-directory.ts` holds one
    /// `ConnectionClient` per enrolled machine).
    ///
    /// Reconciles on a doorbell rather than a timer, so a machine that has just
    /// appeared is watched on the same observation that made it visible; the
    /// timeout is a backstop for the case where nothing rings.
    fn spawn_supervisor(inner: &Arc<Inner>, rx: crossbeam_channel::Receiver<()>) {
        let inner = Arc::clone(inner);
        let spawned =
            std::thread::Builder::new().name("zest-fleet-hosts".into()).spawn(move || {
                // Loaded once, on this thread, on first need: the keychain
                // stays off the startup path, and off every path for someone
                // who never leaves loopback.
                let mut identity = IdentityCache::default();
                loop {
                    Self::reconcile_watchers(&inner, &mut identity);
                    // A ring or a backstop, whichever comes first. The
                    // backstop matters for the case nothing observes: a host
                    // whose daemon was down when we last looked comes back
                    // without discovery having anything new to say.
                    let _ = rx.recv_timeout(SUPERVISE_INTERVAL);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(
                error = %e,
                "no fleet supervisor; only this machine's sessions will be listed"
            );
        }
    }

    /// Start a watcher for every routable remote host, stop the rest.
    ///
    /// Plumbing only — [`watcher_plan`] owns the decision and carries its tests.
    fn reconcile_watchers(inner: &Arc<Inner>, identity: &mut IdentityCache) {
        let hosts = Self::snapshot_of(inner);
        let plan = {
            let state = inner.state.lock();
            let relay_origin = state.account.as_ref().and_then(|a| a.relay_origin.clone());
            let signed_in = state.account.is_some();
            let watching = state.watchers.keys().copied().collect();
            drop(state);
            watcher_plan(&hosts, &watching, relay_origin.as_deref(), signed_in)
        };
        if plan.start.is_empty() && plan.stop.is_empty() {
            return;
        }

        {
            let mut state = inner.state.lock();
            for host in &plan.stop {
                if let Some(stop) = state.watchers.remove(host) {
                    stop.store(true, Ordering::Release);
                }
                // Back to unknown, never a stale `Fresh`: a listing we can no
                // longer refresh is a claim about sessions that may not exist,
                // and showing it is worse than showing nothing.
                state.sessions.remove(host);
                state.offers.remove(host);
                // And no lane: the watcher may sit parked in `read` for as
                // long as the far socket stays open, and a question sent down
                // its lane would count as asked and never be answered.
                state.links.remove(host);
                if let Some(search) = state.search.as_mut() {
                    search.asked.remove(host);
                }
            }
        }
        if !plan.stop.is_empty() {
            inner.mark_changed();
        }
        if plan.start.is_empty() {
            return;
        }

        // Only now is a credential worth reading — someone who never leaves
        // loopback never reaches this line.
        let Some(identity) = Self::remote_identity(identity) else { return };
        for (host, route) in plan.start {
            let stop = Arc::new(AtomicBool::new(false));
            inner.state.lock().watchers.insert(host, Arc::clone(&stop));
            Self::spawn_host_watcher(inner, host, route, Arc::clone(&identity), stop);
        }
    }

    /// The stored device key, for dialling machines that are not this one.
    ///
    /// Never a throwaway, unlike the attach path's fallback: a watcher redials
    /// for as long as the window is open, and a key that changes per connection
    /// would make every far host ask a person to approve it again, every time.
    /// Better to list no sessions than to become a source of prompts.
    fn remote_identity(cache: &mut IdentityCache) -> Option<Arc<ClientIdentity>> {
        if let Some(key) = cache.key.as_ref() {
            return Some(Arc::clone(key));
        }
        if !cache.may_retry(std::time::Instant::now()) {
            return None;
        }
        match ClientIdentity::load_or_create(&zest_mesh::keystore::OsKeyStore) {
            Ok(i) => {
                let key = Arc::new(i);
                cache.key = Some(Arc::clone(&key));
                Some(key)
            }
            Err(e) => {
                // Once at `warn`, then quiet. The supervisor reconciles on
                // every observation, so a machine with no credential store
                // would otherwise emit this on every mDNS packet — and, worse
                // than the noise, re-enter the credential store each time. On
                // macOS that path can raise a Keychain prompt, so retrying it
                // per observation turns one refusal into a stream of dialogs.
                if cache.complained {
                    tracing::debug!(error = %e, "still no credential store");
                } else {
                    cache.complained = true;
                    tracing::warn!(
                        error = %e,
                        "no credential store; remote session lists will stay empty"
                    );
                }
                cache.failed_at = Some(std::time::Instant::now());
                None
            }
        }
    }

    /// One host's watcher: dial, list, then block on pushes, redialling with
    /// backoff until asked to stop.
    fn spawn_host_watcher(
        inner: &Arc<Inner>,
        host: HostId,
        route: crate::route::HostRoute,
        identity: Arc<ClientIdentity>,
        stop: Arc<AtomicBool>,
    ) {
        let owned = Arc::clone(inner);
        let spawned = std::thread::Builder::new()
            .name(format!("zest-fleet-{}", host.short()))
            .spawn(move || {
                let inner = owned;
                let mut wait = REWATCH_MIN;
                while !stop.load(Ordering::Acquire) {
                    {
                        let mut state = inner.state.lock();
                        // Under the lock, like every other write here: a
                        // retired watcher must not resurrect the entry the
                        // supervisor just removed.
                        if !stop.load(Ordering::Acquire) {
                            // Only if nothing is known yet: a redial after a
                            // drop must not blank a listing that is still the
                            // best answer anyone has.
                            state.sessions.entry(host).or_insert(SessionsState::Fetching);
                        }
                    }
                    match Self::watch_host_once(&inner, host, &route, &identity, &stop) {
                        Ok(()) => wait = REWATCH_MIN,
                        Err(e) => {
                            tracing::debug!(
                                host = %host.short(),
                                error = %e,
                                "remote fleet watch ended"
                            );
                            let mut state = inner.state.lock();
                            let told = !stop.load(Ordering::Acquire)
                                && match state.sessions.get_mut(&host) {
                                    // Only over a `Fetching`: a failure must not
                                    // overwrite a listing that is still the best
                                    // answer anyone has.
                                    Some(slot @ SessionsState::Fetching) => {
                                        *slot = SessionsState::Failed(e.to_string());
                                        true
                                    }
                                    _ => false,
                                };
                            drop(state);
                            if told {
                                inner.mark_changed();
                            }
                        }
                    }
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(REWATCH_MAX);
                }
                tracing::debug!(host = %host.short(), "remote fleet watcher stopped");
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, host = %host.short(), "no thread for a fleet watcher");
            inner.state.lock().watchers.remove(&host);
        }
    }

    /// Publish a listing for `host`, unless this watcher has been retired.
    ///
    /// **The flag is read under the state lock, and that is the whole point.**
    /// A watcher can be parked in `read` — or mid-dial — when the supervisor
    /// decides its host has no route, and the supervisor's "set the flag,
    /// clear the listing" runs as one critical section. Checking `stop` outside
    /// this lock leaves a window where a late reply lands *after* the clear and
    /// puts the listing back, permanently: the host is already out of
    /// `watchers`, so nothing will ever clear it again, and the fleet shows
    /// sessions for a machine it cannot reach. That is exactly the stale-`Fresh`
    /// the clear exists to prevent, arriving by the back door.
    ///
    /// Returns whether anything was published, so the caller only wakes the UI
    /// when it was.
    fn publish(
        inner: &Arc<Inner>,
        host: HostId,
        stop: &AtomicBool,
        sessions: Vec<zest_proto::SessionInfo>,
        offer: Option<zest_proto::HostOffer>,
    ) -> bool {
        let mut state = inner.state.lock();
        let published =
            apply_listing(&mut state, host, stop.load(Ordering::Acquire), sessions, offer);
        drop(state);
        if published {
            inner.mark_changed();
        }
        published
    }

    /// One connection to one remote host, until it dies.
    fn watch_host_once(
        inner: &Arc<Inner>,
        host: HostId,
        route: &crate::route::HostRoute,
        identity: &Arc<ClientIdentity>,
        stop: &AtomicBool,
    ) -> Result<(), crate::remote::RemoteError> {
        let (read, write) = (route.dialer())()?;
        let mut client = DaemonClient::connect_watching(
            read,
            write,
            identity,
            "zesterm-fleet",
            // The address came from an advertisement or an account listing,
            // which are claims; the host signs first precisely so this can be
            // checked before anything is revealed.
            Some(host),
            // Sessions and the offer, never pairings: another machine's
            // approval queue is not ours to show. The daemon would refuse the
            // flag off-loopback anyway — not sending it is saying so here
            // rather than relying on the far end to say it.
            zest_daemon::client::Watch {
                sessions: true,
                pairings: false,
                hosts: true,
                // Attaches to nothing; see the watcher above.
                signals: false,
            },
        )?;

        // `list_with_offer`, not `list`: a subscriber's first offer rides this
        // very reply and the daemon marks it sent, so dropping it would leave
        // the launcher waiting for a config edit on the far machine (#262).
        // The dial and the listing both take time, and the supervisor may
        // have retired this watcher while they did.
        let (sessions, offer) = client.list_with_offer()?;
        if !Self::publish(inner, host, stop, sessions, offer) {
            return Ok(());
        }

        // The listing, and the palette's search answers; a remote host's
        // other pushes (its approval queue) are never subscribed to.
        Self::serve_watch(inner, host, client, stop, |message| match message {
            zest_proto::HostMessage::Sessions { sessions, offer, .. } => {
                Self::publish(inner, host, stop, sessions, offer)
            }
            zest_proto::HostMessage::BlockMatches { query, matches, .. } => {
                Self::receive(inner, host, stop, &query, matches);
                true
            }
            zest_proto::HostMessage::Error { session: None, message } => {
                Self::refuse(inner, host, stop, &message);
                true
            }
            _ => true,
        })
    }

    /// Ask every connected host for the blocks matching `query` (#527).
    ///
    /// Sent on every changed keystroke, undebounced: a frame is a few
    /// hundred bytes, a daemon's scan is microseconds, the relay round trips
    /// are paid in parallel, and an answer to a superseded query is dropped
    /// by its echo. A timer would add its whole delay to every keystroke to
    /// save nothing anyone could measure; [`LANE_DEPTH`] is the backstop for
    /// a stalled link.
    pub fn search_blocks(&self, query: &str) {
        ask_lanes(&mut self.inner.state.lock(), query);
    }

    /// The block search as it stands: the query, how many hosts it reached,
    /// and every answer parked so far. Cloned out, like `snapshot`.
    #[must_use]
    pub fn block_matches(&self) -> BlockSearchView {
        let state = self.inner.state.lock();
        state.search.as_ref().map_or_else(BlockSearchView::default, |s| BlockSearchView {
            query: s.query.clone(),
            asked: s.asked.len(),
            answered: s.answers.iter().map(|(h, m)| (*h, m.clone())).collect(),
        })
    }

    /// Park one host's search answer, unless its watcher has been retired
    /// — `publish`'s rule, for matches, and the same lock.
    fn receive(
        inner: &Arc<Inner>,
        host: HostId,
        stop: &AtomicBool,
        query: &str,
        matches: Vec<BlockMatch>,
    ) {
        let mut state = inner.state.lock();
        let stored =
            receive_matches(&mut state, host, stop.load(Ordering::Acquire), query, matches);
        drop(state);
        if stored {
            inner.mark_changed();
        }
    }

    /// A host declined the search as a message it does not know — see
    /// [`decline_search`]. Under the state lock with the stop flag, like
    /// every other write from a watcher.
    fn refuse(inner: &Arc<Inner>, host: HostId, stop: &AtomicBool, message: &str) {
        tracing::debug!(host = %host.short(), message, "this daemon cannot search blocks");
        let mut state = inner.state.lock();
        let settled = decline_search(&mut state, host, stop.load(Ordering::Acquire));
        drop(state);
        if settled {
            inner.mark_changed();
        }
    }

    /// The rest of a watcher's life, once its listing is in: the connection
    /// taken apart into a reader (this thread) and a writer on a lane other
    /// threads can send down (#527).
    ///
    /// Until the split, a watcher owned its whole `DaemonClient` and parked
    /// in `next_message`, so nothing could ask the host anything — the
    /// pairing decision dials afresh for exactly that reason, which is fine
    /// once per decision and not once per keystroke. The shape is
    /// `RemoteSession`'s: split the sealed channel into its two directions
    /// (separate keys, separate counters, no lock between the threads), carry
    /// the frames the handshake already lifted off the socket (#54), and
    /// drain those *before* the first blocking read.
    ///
    /// `on_message` returns whether to keep serving; `false` ends the
    /// connection cleanly, which is how a retired watcher stops.
    fn serve_watch(
        inner: &Arc<Inner>,
        host: HostId,
        client: DaemonClient,
        stop: &AtomicBool,
        mut on_message: impl FnMut(HostMessage) -> bool,
    ) -> Result<(), crate::remote::RemoteError> {
        let halves = client.into_halves();
        let (mut reader, writer, mut frames) = (halves.read, halves.write, halves.frames);
        let (sealer, mut opener) = match halves.channel {
            Some(c) => {
                let (s, o) = c.split();
                (Some(s), Some(o))
            }
            None => (None, None),
        };
        let (tx, rx) = crossbeam_channel::bounded::<ClientMessage>(LANE_DEPTH);
        std::thread::Builder::new()
            .name(format!("zest-fleet-writer-{}", host.short()))
            .spawn(move || {
                let mut sink: Option<Box<dyn Write + Send>> = Some(writer);
                let mut sealer = sealer;
                // Ends when every sender is gone: the lane in `links` and the
                // reader's own clone, whichever is dropped last.
                while let Ok(msg) = rx.recv() {
                    crate::remote::write_msg(&mut sink, sealer.as_mut(), &msg);
                    if sink.is_none() {
                        // The reader is the supervisor and will see the same
                        // break; nothing more can be written.
                        break;
                    }
                }
            })
            .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;

        // Registered under the state lock, after the stop check — `publish`'s
        // rule: a watcher retired while it dialled must not leave a lane the
        // supervisor has already decided nobody holds.
        let lane = tx.clone();
        {
            let mut state = inner.state.lock();
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            state.links.insert(host, tx);
        }

        let served = Self::read_frames(&mut reader, &mut frames, opener.as_mut(), stop, &mut on_message);

        // Only this connection's own lane: the supervisor may already have
        // removed it, and a redial (which runs after this returns) will
        // register its own.
        let mut state = inner.state.lock();
        if state.links.get(&host).is_some_and(|l| l.same_channel(&lane)) {
            state.links.remove(&host);
        }
        // A question this host can no longer answer is not outstanding: the
        // query row must not wait on it.
        if let Some(search) = state.search.as_mut() {
            search.asked.remove(&host);
        }
        state.declined.remove(&host);
        drop(state);
        drop(lane);
        served
    }

    /// Read, open and decode frames until the link dies or the caller says
    /// stop. The frame handling is `RemoteSession`'s reader in miniature —
    /// see its comments for why a frame that will not open ends the
    /// connection and why an undecodable one does not.
    fn read_frames(
        reader: &mut Box<dyn Read + Send>,
        frames: &mut zest_proto::FrameReader,
        mut opener: Option<&mut zest_mesh::secure::Opener>,
        stop: &AtomicBool,
        on_message: &mut impl FnMut(HostMessage) -> bool,
    ) -> Result<(), crate::remote::RemoteError> {
        use crate::remote::RemoteError;
        let mut buf = vec![0u8; 64 * 1024];
        let mut drain_carried = frames.pending() > 0;
        loop {
            if drain_carried {
                drain_carried = false;
            } else {
                let n = match reader.read(&mut buf) {
                    Ok(0) => return Err(RemoteError::Io("the host closed the connection".into())),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(RemoteError::Io(e.to_string())),
                    Ok(n) => n,
                };
                frames.feed(&buf[..n]);
            }
            loop {
                let body = match frames.next_frame() {
                    Ok(Some(b)) => b,
                    Ok(None) => break,
                    Err(e) => return Err(RemoteError::Io(format!("framing is lost: {e}"))),
                };
                let body = match opener.as_deref_mut() {
                    Some(o) => o
                        .open(&body)
                        .map_err(|e| RemoteError::Io(format!("a sealed frame did not open: {e}")))?,
                    None => body,
                };
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                // A message this build cannot parse is a newer host, not a
                // broken one; a watcher subscribes to pushes it already
                // understands, so skipping is the whole degradation.
                let Ok(msg) = frame::decode::<HostMessage>(&body) else { continue };
                if !on_message(msg) {
                    return Ok(());
                }
            }
        }
    }

    /// Clear the change latch; returns whether anything had changed. The
    /// main thread calls this when it consumes [`Wakeup::FleetChanged`].
    pub fn take_changed(&self) -> bool {
        self.inner.dirty.swap(false, Ordering::AcqRel)
    }

    /// One session's context, cloned out on its own.
    ///
    /// The chip row asks per frame, and [`Self::snapshot`] per frame would
    /// clone every listing in the fleet to answer for one row. Stale entries
    /// answer `None` like absent ones: a chip describing a machine the
    /// watcher lost is a chip lying with confidence.
    #[must_use]
    pub fn session_context(
        &self,
        addr: zest_proto::SessionAddr,
    ) -> Option<zest_proto::SessionContext> {
        let state = self.inner.state.lock();
        let SessionsState::Fresh(sessions) = state.sessions.get(&addr.host)? else { return None };
        sessions.iter().find(|s| s.addr == addr).and_then(|s| s.context.clone())
    }

    /// One host's link, cloned out on its own for the link chip (#432): how
    /// this window reaches it, and the last measured round trip. `None` for
    /// an unknown host — and for the window's own machine the chip's caller
    /// declines loopback anyway, because "local 0.0 ms" is noise wearing a
    /// number.
    #[must_use]
    pub fn link_of(
        &self,
        host: zest_proto::HostId,
    ) -> Option<(zest_mesh::Reachability, Option<f32>)> {
        let state = self.inner.state.lock();
        if state.local.as_ref().is_some_and(|(h, _)| *h == host) {
            return Some((zest_mesh::Reachability::Loopback, None));
        }
        let record = state.discovery.as_ref()?.records().into_iter().find(|r| r.peer.host == host)?;
        let reach = record.peer.best_endpoint().map(|e| e.reachability)?;
        Some((reach, state.rtt.get(&host).copied()))
    }

    /// The fleet as of now: the window's own host first, then the roster in
    /// its stable order (the roster is a BTreeMap precisely so listings do
    /// not reshuffle between polls).
    #[must_use]
    pub fn snapshot(&self) -> Vec<FleetHost> {
        Self::snapshot_of(&self.inner)
    }

    /// [`Self::snapshot`] over an `Inner` directly, for the background threads
    /// that hold one but no `FleetModel`.
    fn snapshot_of(inner: &Arc<Inner>) -> Vec<FleetHost> {
        let state = inner.state.lock();
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
                offer: state.offers.get(host).cloned(),
                enrolled: false,
                // Discovery's rows carry no account fact; `merge_account`
                // is what lays one over them.
                relay_online: false,
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
                    offer: state.offers.get(&record.peer.host).cloned(),
                    enrolled: false,
                    relay_online: false,
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
        // The shared fixture's remote row, re-keyed: these tests mint their
        // own ids, and merge logic reads the id, never the address.
        let mut h = zest_fleet::fixture::host(9, label);
        h.host = host;
        h.address = Some("192.168.1.9:7717".into());
        h
    }


    fn watching(ids: &[HostId]) -> std::collections::BTreeSet<HostId> {
        ids.iter().copied().collect()
    }

    const RELAY: Option<&str> = Some("wss://relay.example");

    #[test]
    fn a_missing_credential_store_is_asked_again_slowly_not_every_observation() {
        // The supervisor reconciles on every observation, and mDNS is chatty.
        // Without a backoff a machine with no credential store re-enters it on
        // every packet — and the noise is the lesser problem: on macOS that
        // path can raise a Keychain prompt, so retrying per observation turns
        // one refusal into a stream of dialogs.
        //
        // Absent is wrong too, though. A locked keychain gets unlocked, and a
        // fleet that stayed empty until the next launch would be the worse
        // failure — so this asserts the retry happens, just slowly.
        let start = std::time::Instant::now();
        let mut cache = IdentityCache::default();
        assert!(cache.may_retry(start), "nothing has failed yet");

        cache.failed_at = Some(start);
        assert!(!cache.may_retry(start + Duration::from_secs(1)), "not on the next observation");
        assert!(!cache.may_retry(start + IDENTITY_RETRY - Duration::from_millis(1)));
        assert!(cache.may_retry(start + IDENTITY_RETRY), "but eventually, so a lock can lift");
    }

    #[test]
    fn a_retired_watcher_cannot_put_a_listing_back() {
        // The race, and it is the nastiest kind: permanent. A watcher can be
        // parked in `read` when the supervisor decides its host has no route,
        // and the supervisor's "set the flag, clear the listing" runs as one
        // critical section. A late reply that landed *after* the clear would
        // reinsert the listing — and nothing would ever clear it again, since
        // the host is already out of `watchers`. The fleet would show sessions
        // for a machine it cannot reach, for the life of the window.
        //
        // The flag is therefore read under the same lock, and this is that
        // rule with the lock taken out of the picture.
        let host = HostId::from_bytes([2; 32]);
        let info = || {
            vec![zest_proto::SessionInfo {
                addr: zest_proto::SessionAddr::new(host, zest_proto::SessionId(1)),
                title: "zsh".into(),
                cwd: "/".into(),
                cols: 80,
                rows: 24,
                alt_screen: false,
                attached: false,
                context: None,
                busy: false,
            }]
        };
        let offer = || Some(zest_proto::HostOffer { os: "linux".into(), ..Default::default() });

        let mut state = State::default();
        assert!(apply_listing(&mut state, host, false, info(), offer()), "a live watcher writes");
        assert!(matches!(state.sessions.get(&host), Some(SessionsState::Fresh(s)) if s.len() == 1));
        assert!(state.offers.contains_key(&host));

        // The supervisor's clear, as `reconcile_watchers` performs it.
        state.sessions.remove(&host);
        state.offers.remove(&host);

        assert!(
            !apply_listing(&mut state, host, true, info(), offer()),
            "a retired watcher must not write"
        );
        assert!(!state.sessions.contains_key(&host), "and the clear must survive it");
        assert!(!state.offers.contains_key(&host));
    }

    #[test]
    fn an_absent_offer_leaves_the_last_one_standing() {
        // Sticky, because "no offer on this message" means *nothing new to
        // say* (#262) — an ordinary session push carries none, and clearing on
        // one would blank a launcher's rows every time somebody opened a shell.
        let host = HostId::from_bytes([2; 32]);
        let mut state = State::default();
        apply_listing(
            &mut state,
            host,
            false,
            Vec::new(),
            Some(zest_proto::HostOffer { os: "linux".into(), ..Default::default() }),
        );
        apply_listing(&mut state, host, false, Vec::new(), None);
        assert_eq!(
            state.offers.get(&host).map(|o| o.os.as_str()),
            Some("linux"),
            "the second push said nothing about the offer, so the first still stands"
        );
    }

    #[test]
    fn every_routable_remote_host_gets_a_watcher_and_the_local_one_never_does() {
        // The hole #265 closes: before this, one connection existed — the
        // window's own daemon — so every remote host's session list was
        // `Unknown` for ever, and four surfaces were wrong because of it.
        let forge = HostId::from_bytes([2; 32]);
        let mut local = discovered(HostId::from_bytes([1; 32]), "studio");
        local.local = true;
        let hosts = [local, discovered(forge, "forge")];

        let plan = watcher_plan(&hosts, &watching(&[]), None, false);
        assert_eq!(
            plan.start,
            vec![(forge, crate::route::HostRoute::Tcp("192.168.1.9:7717".into()))],
            "the remote host is watched over its advertised address"
        );
        assert!(
            plan.start.iter().all(|(h, _)| *h != HostId::from_bytes([1; 32])),
            "the local machine is watched by FleetModel::watch — a second \
             connection to the same daemon would double every push"
        );
        assert!(plan.stop.is_empty());
    }

    #[test]
    fn a_settled_fleet_plans_nothing_at_all() {
        // Two properties in one, and the second is load-bearing.
        //
        // The supervisor reconciles on every observation, and mDNS is chatty:
        // without the first, one machine on the network would accumulate a
        // watcher per advertisement.
        //
        // And an empty plan is what keeps the 0%-idle guarantee true across
        // this change. The supervisor wakes on a doorbell and on a 15s
        // backstop; on a settled fleet it must find nothing to do, because the
        // caller only marks the model changed when the plan is not empty. A
        // rule that re-proposed a host already watched would repaint the
        // window every fifteen seconds, for ever, saying nothing.
        let forge = HostId::from_bytes([2; 32]);
        let pi = HostId::from_bytes([3; 32]);
        let mut relayed = discovered(pi, "pi");
        relayed.presence = Presence::Unseen;
        relayed.address = None;
        relayed.enrolled = true;
        let mut local = discovered(HostId::from_bytes([1; 32]), "studio");
        local.local = true;

        let hosts = [local, discovered(forge, "forge"), relayed];
        let plan = watcher_plan(&hosts, &watching(&[forge, pi]), RELAY, true);
        assert_eq!(
            plan,
            WatcherPlan::default(),
            "a settled fleet is no work — both routes are held and nothing has gone"
        );
    }

    #[test]
    fn a_host_that_loses_its_route_has_its_watcher_stopped() {
        // A machine that goes away must not leave a listing standing: a
        // `Fresh` nobody can refresh is a claim about sessions that may no
        // longer exist, and the caller clears it on exactly this signal.
        let gone = HostId::from_bytes([9; 32]);
        let plan = watcher_plan(&[], &watching(&[gone]), None, false);
        assert_eq!(plan.stop, vec![gone]);
        assert!(plan.start.is_empty());
    }

    #[test]
    fn an_enrolled_host_off_this_lan_is_watched_through_the_relay() {
        // The case the tunnel exists for, and the one a rule of its own would
        // have missed — routability is `route::best_route`, so this agrees
        // with what a fleet card and a ⌘K row can open (#250).
        let far = HostId::from_bytes([3; 32]);
        let mut host = discovered(far, "pi");
        host.presence = Presence::Unseen;
        host.address = None;
        host.enrolled = true;

        let plan = watcher_plan(&[host.clone()], &watching(&[]), RELAY, true);
        assert!(
            matches!(plan.start[..], [(h, crate::route::HostRoute::Relay { .. })] if h == far),
            "enrolled and signed in: the relay is the route, got {:?}",
            plan.start
        );

        // Signed out, the same host has no route at all — and a watcher held
        // over from before must stop rather than redial something that cannot
        // mint a ticket.
        let plan = watcher_plan(&[host], &watching(&[far]), RELAY, false);
        assert!(plan.start.is_empty());
        assert_eq!(plan.stop, vec![far], "sign-out takes the route with it");
    }

    #[test]
    fn a_host_with_no_route_is_neither_started_nor_counted() {
        // An advertisement with an empty address set (the DNS-SD instance-name
        // trap) and an unenrolled unseen host both read as "nothing to dial".
        // Neither should produce a watcher that spends its life failing.
        let mut empty = discovered(HostId::from_bytes([4; 32]), "sleepy");
        empty.address = None;
        let mut unseen = discovered(HostId::from_bytes([5; 32]), "ghost");
        unseen.presence = Presence::Unseen;
        unseen.address = None;

        let plan = watcher_plan(&[empty, unseen], &watching(&[]), RELAY, true);
        assert!(plan.start.is_empty(), "no evidence, no watcher: {:?}", plan.start);
        assert!(plan.stop.is_empty());
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

    fn a_match(host: HostId, command: &str) -> BlockMatch {
        BlockMatch {
            host,
            session: Some(zest_proto::SessionId(1)),
            block: 1,
            title: String::new(),
            command: command.into(),
            cwd: "/".into(),
            state: zest_proto::BlockState::Finished { exit_code: Some(0) },
            started_ms: Some(1),
            ended_ms: Some(2),
            context: None,
            author: None,
        }
    }

    /// The echoed query is the only correlation there is. A slow host
    /// answering `ca` after the palette has typed `cargo` must not put the
    /// broader answer where the narrower one belongs.
    #[test]
    fn a_stale_answer_is_dropped_by_its_echo() {
        let host = HostId::from_bytes([3; 32]);
        let mut state = State::default();
        let (lane, _rx) = crossbeam_channel::bounded(LANE_DEPTH);
        state.links.insert(host, lane);
        ask_lanes(&mut state, "ca");
        ask_lanes(&mut state, "cargo");
        assert!(
            !receive_matches(&mut state, host, false, "ca", vec![a_match(host, "cat x")]),
            "an answer to the earlier question is late, not wrong, and goes nowhere"
        );
        assert!(receive_matches(&mut state, host, false, "cargo", vec![a_match(host, "cargo b")]));
        let search = state.search.as_ref().expect("a search is in flight");
        assert_eq!(search.answers[&host][0].command, "cargo b");
    }

    /// `a_retired_watcher_cannot_put_a_listing_back`, for answers: the
    /// supervisor's clear and a late reply race, and the flag read under the
    /// lock is what decides it.
    #[test]
    fn a_retired_watcher_cannot_park_an_answer() {
        let host = HostId::from_bytes([3; 32]);
        let mut state = State {
            search: Some(BlockSearch { query: "x".into(), ..Default::default() }),
            ..Default::default()
        };
        assert!(!receive_matches(&mut state, host, true, "x", vec![a_match(host, "x")]));
        assert!(state.search.as_ref().is_some_and(|s| s.answers.is_empty()));
    }

    /// A daemon predating `SearchBlocks` answers it with the generic
    /// could-not-understand `Error`, which is an answer: it must leave the
    /// pending count rather than hold `2 of 3 hosts searched` on screen for
    /// ever, and it must not be asked again on the next keystroke.
    #[test]
    fn a_daemon_too_old_to_search_settles_the_count_and_is_not_asked_again() {
        let old = HostId::from_bytes([1; 32]);
        let new = HostId::from_bytes([2; 32]);
        let mut state = State::default();
        let (lane_old, rx_old) = crossbeam_channel::bounded(LANE_DEPTH);
        let (lane_new, _rx_new) = crossbeam_channel::bounded(LANE_DEPTH);
        state.links.insert(old, lane_old);
        state.links.insert(new, lane_new);
        ask_lanes(&mut state, "make");
        assert_eq!(state.search.as_ref().map(|s| s.asked.len()), Some(2), "both were asked");

        assert!(decline_search(&mut state, old, false), "the refusal moved the count");
        assert_eq!(
            state.search.as_ref().map(|s| s.asked.iter().copied().collect::<Vec<_>>()),
            Some(vec![new]),
            "the old daemon is no longer pending"
        );

        let _ = rx_old.try_recv();
        ask_lanes(&mut state, "make -j");
        assert!(rx_old.try_recv().is_err(), "a daemon that said it cannot is not asked again");
        assert_eq!(state.search.as_ref().map(|s| s.asked.len()), Some(1));

        assert!(!decline_search(&mut state, new, true), "a retired watcher's refusal is dropped like its listing");
    }

    /// The query row says how many hosts were asked, so a host the question
    /// never reached must not be one of them — otherwise `2 of 3 hosts
    /// searched` waits on a host that is not going to answer.
    #[test]
    fn a_lane_that_is_full_does_not_count_as_asked() {
        let stalled = HostId::from_bytes([1; 32]);
        let live = HostId::from_bytes([2; 32]);
        let mut state = State::default();
        let (full, _keep) = crossbeam_channel::bounded(1);
        full.send(ClientMessage::ListSessions).expect("fill the lane");
        state.links.insert(stalled, full);
        let (free, rx) = crossbeam_channel::bounded(LANE_DEPTH);
        state.links.insert(live, free);
        ask_lanes(&mut state, "make");
        let search = state.search.as_ref().expect("a search is in flight");
        assert_eq!(search.asked.iter().copied().collect::<Vec<_>>(), [live]);
        assert!(
            matches!(rx.try_recv(), Ok(ClientMessage::SearchBlocks { query, limit }) if query == "make" && limit == PALETTE_LIMIT),
            "the live host got the question, with the palette's own limit"
        );
    }
}
