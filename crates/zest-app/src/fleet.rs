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

/// How often the account listing is re-read while signed in. A minute,
/// jittered ±25% so several windows do not poll the control plane in step;
/// failures back off to five minutes — a listing that cannot be fetched is
/// stale, not urgent.
const ACCOUNT_POLL: Duration = Duration::from_secs(60);
const ACCOUNT_BACKOFF: Duration = Duration::from_secs(300);

/// One machine the account lists, as the fleet consumes it.
///
/// Deliberately not `crate::cloud::AccountHosts`: this module aggregates
/// sources and owns no transport, so it names only the facts it merges on and
/// lets `app.rs` convert — the same discipline that keeps the roster
/// socket-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    pub host: HostId,
    pub label: String,
    /// The relay says this machine's control link is parked right now.
    ///
    /// The third fact, and #237 is what its absence cost: with only id and
    /// label here, a machine that discovery cannot see got `Presence::Unseen`
    /// and rendered as *asleep* — while clicking the same card opened a shell
    /// through the relay immediately, because the route is chosen from
    /// `enrolled + relay origin` and never from presence.
    pub relay_online: bool,
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
pub enum SessionsState {
    /// Never asked.
    #[default]
    Unknown,
    /// A listing is on its way.
    Fetching,
    Fresh(Vec<SessionInfo>),
    /// The dial or the listing failed, carrying why.
    ///
    /// The message is written and not yet drawn: the fleet card's "could not
    /// reach" row is #249's next item. Kept rather than dropped because the
    /// state machine is what this PR is for, and a `Failed` with nothing in it
    /// would have to be widened again by the commit that renders it.
    #[allow(dead_code, reason = "the fleet card's failure row reads this (#249)")]
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
    /// What this machine says it can offer: its os, its arch, its default
    /// shell, and its own profiles (#262).
    ///
    /// **`None` is the ordinary state, not an error.** Every reachable host is
    /// *asked* since #265, which is what makes this reachable at all — but the
    /// answer is absent until that host's first listing lands, and stays absent
    /// for a daemon that predates the field or one nothing can reach. All three
    /// read the same way on purpose: nobody has told us anything, so nothing is
    /// drawn. A consumer that treats `None` as "it has no profiles" would show
    /// an empty group for a machine whose watcher simply has not connected yet.
    ///
    /// Not *drawn* anywhere yet — the launcher's host groups and the fleet
    /// card's `os` row are #249's next items.
    #[allow(dead_code, reason = "the launcher's host groups and the fleet card read this (#249)")]
    pub offer: Option<zest_proto::HostOffer>,
    /// The relay had proof of a parked control link when the listing was
    /// fetched — so this machine is reachable through the tunnel even when
    /// nothing on this network has ever heard of it.
    ///
    /// Kept beside `presence` rather than folded into it, and that is the
    /// whole design: `Presence` is `zest_mesh`'s word for what *discovery*
    /// observed, and minting `Online` there for a machine mDNS never saw would
    /// send the prober off to dial a LAN address that does not exist. Read
    /// them together through [`FleetHost::is_online`].
    pub relay_online: bool,
}

impl FleetHost {
    /// Can anything reach this machine right now, by any route?
    ///
    /// The one place the rule lives, because it had five callers and every one
    /// of them spelled it `local || presence == Online` — which is exactly the
    /// expression that made #237 possible. A card, a sidebar count, a picker
    /// row and a settings pill must agree, and the way to make them agree is
    /// to give them one function rather than one convention.
    ///
    /// LAN evidence and tunnel evidence are OR'd rather than ranked: they are
    /// answers to the same question from two mechanisms, and a machine on the
    /// desk with a parked relay link is reachable twice over. Which route is
    /// *preferred* is `best_route`'s decision, and it still prefers the LAN.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.local || self.presence == Presence::Online || self.relay_online
    }

    /// Does this machine still need enrolling?
    ///
    /// Judged by the *daemon's* own word when it gave one
    /// (`HostOffer::has_account_token`), and by the account table's row only
    /// when it did not. The two facts share the word "enrolled" and can
    /// disagree (#245): a host key can be a live row in the account's `hosts`
    /// table while the daemon on that machine holds no token — post-revoke
    /// restore, a wiped machine, `--logout` — and gating the enrol affordance
    /// on the row alone hid the button exactly when it was needed, which is
    /// what compounded into #246's lockout.
    #[must_use]
    pub fn needs_enrollment(&self) -> bool {
        match self.offer.as_ref().and_then(|o| o.has_account_token) {
            Some(held) => !held,
            // The daemon did not say — it predates the field, or its store
            // could not be read — so the account's row is the only fact
            // left, which is exactly the old behaviour an old daemon
            // degrades to.
            None => !self.enrolled,
        }
    }
}


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
        // current truth for this host.
        loop {
            match client.next_message()? {
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
                _ => {}
            }
        }
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

        loop {
            // Only the listing matters here; a remote host's other pushes
            // (its approval queue) are never subscribed to.
            let message = client.next_message()?;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if let zest_proto::HostMessage::Sessions { sessions, offer, .. } = message {
                if !Self::publish(inner, host, stop, sessions, offer) {
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
            // Carried onto the matched row too, though the LAN decoration is
            // what the card will show: a machine on this network reached over
            // mDNS keeps its `Online` presence, its address and its measured
            // RTT, and is not relabelled "via tunnel" for also being dialable
            // that way. The fact is still recorded, because the two sources
            // can disagree — a machine whose mDNS record has gone stale but
            // whose relay link is parked is reachable, and `is_online` is
            // where that gets decided.
            seen.relay_online = entry.relay_online;
        } else {
            out.push(FleetHost {
                host: entry.host,
                label: entry.label.clone(),
                // Still `Unseen`, and deliberately: this is discovery's word,
                // and nothing local has observed this machine. What stops the
                // card saying *asleep* is `relay_online` beside it — #237.
                presence: Presence::Unseen,
                local: false,
                address: None,
                reachability: Some(zest_mesh::Reachability::Cloud),
                rtt_ms: None,
                sessions: SessionsState::default(),
                // Nothing has connected to this machine, so it has told us
                // nothing — the same `None` a daemon predating the field
                // produces, and read the same way.
                offer: None,
                enrolled: true,
                relay_online: entry.relay_online,
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
            offer: None,
            enrolled: false,
            relay_online: false,
        }
    }


    fn watching(ids: &[HostId]) -> std::collections::BTreeSet<HostId> {
        ids.iter().copied().collect()
    }

    const RELAY: Option<&str> = Some("wss://relay.example");

    #[test]
    fn an_enrolled_row_with_a_tokenless_daemon_still_needs_enrolling() {
        // #245: `enrolled` is the account table's fact, and what the enrol
        // button grants is a machine token for the *daemon* — a host row can
        // be live while the machine holds nothing (post-revoke restore, a
        // wiped machine, `--logout`). Hiding the affordance on the row is
        // what sent someone to the Remove button and into #246's lockout.
        let mut h = discovered(HostId::from_bytes([7; 32]), "studio");
        h.enrolled = true;
        h.offer = Some(zest_proto::HostOffer {
            has_account_token: Some(false),
            ..Default::default()
        });
        assert!(
            h.needs_enrollment(),
            "the daemon says its store holds no token; the account's row must not outvote it"
        );
    }

    #[test]
    fn a_daemon_holding_a_token_needs_no_enrolling_whatever_the_row_says() {
        // The mirror case: the daemon holds a token and the account listing
        // has not caught up. Offering the button would mint a one-shot code
        // for a machine that needs nothing.
        let mut h = discovered(HostId::from_bytes([7; 32]), "studio");
        h.enrolled = false;
        h.offer = Some(zest_proto::HostOffer {
            has_account_token: Some(true),
            ..Default::default()
        });
        assert!(
            !h.needs_enrollment(),
            "the daemon says it holds a token; a stale account listing must not re-offer enrolment"
        );
    }

    #[test]
    fn a_daemon_that_did_not_say_falls_back_to_the_accounts_row() {
        // `None` is a daemon predating the field, or a credential store that
        // could not be read — either way the account's row is the only fact
        // left, which is exactly today's behaviour, so an old daemon
        // degrades to it rather than to a button that flaps.
        let mut h = discovered(HostId::from_bytes([7; 32]), "studio");

        h.offer = None;
        h.enrolled = false;
        assert!(h.needs_enrollment(), "no offer at all: the row is the only fact");
        h.enrolled = true;
        assert!(!h.needs_enrollment());

        h.offer = Some(zest_proto::HostOffer::default());
        assert!(
            !h.needs_enrollment(),
            "an offer whose daemon did not say still falls back to the row"
        );
    }

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
    fn a_host_both_sources_know_is_one_row_carrying_both_facts() {
        let id = HostId::from_bytes([1; 32]);
        let mut out = vec![discovered(id, "studio")];
        merge_account(
            &mut out,
            Some(&[AccountEntry {
                host: id,
                label: "studio (enrolled label)".into(),
                relay_online: false,
            }]),
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
    fn an_account_only_host_the_relay_can_reach_is_online_through_the_tunnel() {
        // #237, in the shape it was reported: the `win` card read *asleep*
        // while clicking it opened a Windows shell through the relay
        // immediately. Nothing local has observed the machine, so discovery's
        // word is still `Unseen` — what changed is that the account now
        // carries a second fact, and `is_online` reads both.
        let id = HostId::from_bytes([3; 32]);
        let mut out = Vec::new();
        merge_account(
            &mut out,
            Some(&[AccountEntry { host: id, label: "win".into(), relay_online: true }]),
        );

        let row = &out[0];
        assert!(
            row.is_online(),
            "a machine whose control link is parked at the relay is reachable right now, \
             and a card that says asleep must mean nobody can reach it"
        );
        assert_eq!(
            row.presence,
            Presence::Unseen,
            "discovery's word is untouched: minting `Online` here would send the prober \
             off to dial a LAN address this machine does not have"
        );
        assert_eq!(
            row.reachability,
            Some(zest_mesh::Reachability::Cloud),
            "and the route it is online *by* is still the tunnel, which is what the pill says"
        );
    }

    #[test]
    fn an_account_only_host_the_relay_cannot_reach_stays_asleep() {
        // The other direction, and the reason the flag is a bound rather than
        // a latch: a machine that is enrolled and switched off must keep
        // reading asleep, or the fix would simply invert the bug.
        let id = HostId::from_bytes([4; 32]);
        let mut out = Vec::new();
        merge_account(
            &mut out,
            Some(&[AccountEntry { host: id, label: "attic-pc".into(), relay_online: false }]),
        );

        assert!(
            !out[0].is_online(),
            "enrolment is not reachability — the account lists machines that are off"
        );
    }

    #[test]
    fn a_machine_on_the_lan_keeps_its_lan_decoration_either_way() {
        // mDNS facts win for a machine on your desk: it is Online with an RTT
        // and an address, not "online via tunnel". The relay fact is still
        // recorded — the two sources can disagree, and `is_online` is where
        // that is resolved — but none of discovery's decoration is disturbed.
        let id = HostId::from_bytes([5; 32]);
        for relay_online in [false, true] {
            let mut out = vec![discovered(id, "studio")];
            merge_account(
                &mut out,
                Some(&[AccountEntry { host: id, label: "studio".into(), relay_online }]),
            );

            let row = &out[0];
            assert!(row.is_online(), "it is on the LAN and advertising, whatever the relay says");
            assert_eq!(
                row.reachability,
                Some(zest_mesh::Reachability::Lan),
                "the LAN route is the better one and must not be relabelled as a tunnel"
            );
            assert_eq!(row.rtt_ms, Some(0.4), "nor may its measured round trip be dropped");
            assert_eq!(row.relay_online, relay_online, "and the account's fact is still carried");
        }
    }

    #[test]
    fn every_way_of_being_reachable_counts_as_online() {
        // The rule had five callers before it was a function, each spelling it
        // `local || presence == Online` — which is exactly the expression that
        // made #237 possible in four places at once. Pinned here so a fifth
        // caller cannot quietly disagree.
        let id = HostId::from_bytes([6; 32]);
        let mut lan = discovered(id, "studio");
        assert!(lan.is_online(), "advertising on the LAN");

        lan.presence = Presence::Away;
        assert!(!lan.is_online(), "and a lid that closed is not");

        lan.relay_online = true;
        assert!(lan.is_online(), "but the same machine reachable through the relay is");

        lan.relay_online = false;
        lan.local = true;
        assert!(lan.is_online(), "and the machine the window is running on always is");
    }

    #[test]
    fn an_account_only_host_is_listed_durable_with_nothing_it_does_not_have() {
        let id = HostId::from_bytes([2; 32]);
        let mut out = Vec::new();
        merge_account(
            &mut out,
            Some(&[AccountEntry { host: id, label: "attic-pc".into(), relay_online: false }]),
        );

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
