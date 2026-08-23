//! Which machines this server can name, and what is known about each.
//!
//! A *pull*: [`Fleet::view`] answers when a tool asks, and at no other moment.
//! That is the line this module exists to stay on the right side of. `zest-app`
//! answers the same question with a `FleetModel` — threads, a dirty latch, an
//! `EventLoopProxy`, an account poller running on a timer — and this crate's
//! `check-deps` boundary warns by name against becoming "a second, headless
//! copy of the app's session handling". Knowing which machines exist is not the
//! objectionable half; owning a live model of them is. It is also ADR-015's
//! rule from the other side: nothing here delivers anything with no call
//! outstanding.
//!
//! # Two sources, because neither is the fleet
//!
//! **mDNS** is the local link, and it is half a fleet: the half it misses is
//! exactly the half the relay exists for — an enrolled machine that is not on
//! this network. A server built on it alone would show a fleet that silently
//! shrank to whatever shared a subnet, which is indistinguishable from
//! machines being asleep.
//!
//! **The account directory** is the durable half (ADR-006: enrolment is the
//! spine, discovery decorates), and it is half a fleet the other way — it knows
//! nothing of a machine on the desk that has never enrolled, and nothing about
//! anyone's sessions.
//!
//! A third candidate is worth ruling out by name, because its name suggests
//! otherwise: **`Hello.watch_hosts` is not a fleet roster.** It carries *this*
//! machine's own `HostOffer` on `Sessions.offer` — "what this machine can
//! offer: its facts, and its own profiles". There is no `HostMessage::Hosts`,
//! and `zest-daemon` runs no discovery at all. A daemon knows itself and its
//! sessions; it does not know the fleet.
//!
//! # Listed is not reachable, and that is deliberate
//!
//! [`FleetView`] carries every machine either source knows, including ones
//! nothing can dial. The web client states the rule this mirrors
//! (`clients/web/packages/app/src/host-source.ts`): *"a machine whose relay is
//! unreachable is still yours, and hiding its row would make the fleet appear
//! to shrink whenever the network hiccuped"*, and what it rules out is *"the
//! row that must fail"*. For an agent that difference is concrete — a listed
//! machine with a stated reason is a refusal it can act on, where a missing row
//! is a machine it will never think to ask about.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use zest_fleet::{merge_account, AccountEntry, FleetHost, SessionsState};
use zest_mesh::discovery::{Discovery, Presence};
use zest_proto::{HostId, HostOffer};

/// One consistent answer to "what is out there right now".
///
/// One struct rather than four trait methods because the four facts are read
/// together and must describe **one moment**: a `hosts` listing whose
/// `relay_origin` came from a later fetch than its rows could route a machine
/// through a relay the same call had just reported as absent.
pub struct FleetView {
    pub hosts: Vec<FleetHost>,
    /// Where this account's relay lives, when it has one and we could ask.
    pub relay_origin: Option<String>,
    /// Whether a token was readable — [`zest_fleet::best_route`]'s last gate.
    pub signed_in: bool,
    /// Why the listing may be short: a source that could not answer.
    ///
    /// Carried rather than logged, because the caller is a tool result and
    /// stderr is not somewhere an agent can read. A fleet of one, with "the
    /// account could not be read" beside it, is a different fact from a fleet
    /// of one.
    pub notes: Vec<String>,
}

/// A source of machines. Implemented live over mDNS and the account, and
/// statically in tests — the same reason `AccountApi` is a trait.
pub trait Fleet: Send + Sync {
    fn view(&self) -> FleetView;

    /// What happened when somebody dialled. Feeds discovery's
    /// [`Presence::Unreachable`], the state a crashed daemon leaves behind a
    /// live mDNS record (#22) — without it a stale address outranks a live
    /// tunnel to the same machine for as long as the record is cached, which
    /// is up to 75 minutes.
    fn report_dial(&self, host: HostId, connected: bool);
}

/// How long to let the link answer before the first listing.
///
/// mDNS is a question asked into the dark: `MdnsDiscovery::start` opens the
/// socket and returns, and records arrive over the next few hundred
/// milliseconds. Answering immediately would report an empty fleet on the very
/// call an agent makes to find out what the fleet is — and the natural repair
/// (call it twice) is a thing no tool description can teach. Paid once per
/// process, on the first `hosts` and never again.
const FIRST_BROWSE: Duration = Duration::from_millis(600);

/// How long an account listing is treated as current.
///
/// The app's own poll interval, reached differently: it *polls* on a timer,
/// this re-fetches when a call finds the cache stale. Same freshness, no
/// thread — and a server nobody asks makes no requests at all.
const ACCOUNT_FRESH: Duration = Duration::from_secs(60);

/// mDNS on this link, plus the account's listing, merged the way `zest-app`
/// merges them — and with none of its engine.
pub struct LiveFleet {
    /// This machine, from the connection the server already holds. Synthesized
    /// rather than discovered: the local daemon need not advertise, and when it
    /// does (`--listen-lan`) its own record is skipped so the row is not
    /// doubled.
    local: HostId,
    local_label: String,
    /// Lazily started. `None` once starting has failed — a link that refused a
    /// multicast socket will refuse it again on every call, and retrying per
    /// tool call would pay `FIRST_BROWSE` forever for an answer that does not
    /// change.
    mdns: Mutex<MdnsState>,
    account: Mutex<AccountCache>,
    control_plane: String,
    roots: zest_cloud::tls::Roots,
}

enum MdnsState {
    Unstarted,
    Browsing(Box<zest_mesh::discovery::mdns::MdnsDiscovery>),
    Failed(String),
}

#[derive(Default)]
struct AccountCache {
    fetched: Option<Instant>,
    relay_origin: Option<String>,
    signed_in: bool,
    hosts: Vec<AccountEntry>,
    note: Option<String>,
}

impl LiveFleet {
    #[must_use]
    pub fn new(local: HostId, local_label: &str) -> Self {
        Self {
            local,
            local_label: local_label.to_string(),
            mdns: Mutex::new(MdnsState::Unstarted),
            account: Mutex::new(AccountCache::default()),
            control_plane: zest_daemon::enroll::DEFAULT_CONTROL_PLANE.to_string(),
            roots: zest_cloud::tls::Roots::Platform,
        }
    }

    /// Point the account half at a different control plane — a local
    /// `wrangler dev`, or a test's own.
    #[must_use]
    pub fn with_control_plane(mut self, base: &str, roots: zest_cloud::tls::Roots) -> Self {
        self.control_plane = base.to_string();
        self.roots = roots;
        self
    }

    /// What this machine's offer says, once its own listing has landed.
    ///
    /// Threaded in by the caller rather than read here: the offer arrives on
    /// the connection `ToolSet` already holds, and re-asking for it would be a
    /// second source for a fact one already has.
    fn local_row(&self, offer: Option<HostOffer>) -> FleetHost {
        FleetHost {
            host: self.local,
            label: self.local_label.clone(),
            presence: Presence::Online,
            local: true,
            // Loopback is not dialled by address. `zest-app` synthesizes the
            // same shape for the same reason.
            address: None,
            reachability: Some(zest_mesh::Reachability::Loopback),
            rtt_ms: None,
            sessions: SessionsState::default(),
            offer,
            // Discovery's rows carry no account fact; `merge_account` lays one
            // over them.
            enrolled: false,
            relay_online: false,
        }
    }

    /// Everything mDNS has heard, as fleet rows. Starts the browse if it has
    /// not been started.
    fn discovered(&self, notes: &mut Vec<String>) -> Vec<FleetHost> {
        let mut state = self.mdns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, MdnsState::Unstarted) {
            let mut d = zest_mesh::discovery::mdns::MdnsDiscovery::new(self.local);
            *state = match d.start() {
                Ok(()) => {
                    std::thread::sleep(FIRST_BROWSE);
                    MdnsState::Browsing(Box::new(d))
                }
                Err(e) => MdnsState::Failed(e.to_string()),
            };
        }
        match &*state {
            MdnsState::Browsing(d) => d
                .records()
                .into_iter()
                // The local daemon may advertise too (`--listen-lan`); the
                // synthesized row already covers it.
                .filter(|r| r.peer.host != self.local)
                .map(|record| {
                    let best = record.peer.best_endpoint();
                    FleetHost {
                        host: record.peer.host,
                        label: record.peer.label.clone(),
                        presence: record.presence,
                        local: false,
                        address: best.map(|e| e.address.clone()),
                        reachability: best.map(|e| e.reachability),
                        // Measured by a prober the window runs and this server
                        // does not: a latency number nobody asked for is not
                        // worth a dial per host per listing.
                        rtt_ms: None,
                        sessions: SessionsState::default(),
                        offer: None,
                        enrolled: false,
                        relay_online: false,
                    }
                })
                .collect(),
            MdnsState::Failed(e) => {
                notes.push(format!("this link could not be browsed, so only enrolled machines are listed ({e})"));
                Vec::new()
            }
            MdnsState::Unstarted => unreachable!("started above"),
        }
    }

    /// The account's listing, re-fetched when this call finds it stale.
    fn account(&self, notes: &mut Vec<String>) -> (Vec<AccountEntry>, Option<String>, bool) {
        let mut cache = self.account.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let stale = cache.fetched.is_none_or(|at| at.elapsed() >= ACCOUNT_FRESH);
        if stale {
            *cache = fetch_account(&self.control_plane, self.roots);
        }
        if let Some(note) = &cache.note {
            notes.push(note.clone());
        }
        (cache.hosts.clone(), cache.relay_origin.clone(), cache.signed_in)
    }
}

/// One account read, with every failure turned into a note rather than an
/// error.
///
/// Signed out is the ordinary state for a machine nobody has enrolled, and a
/// control plane that cannot be reached is a network, not a bug — neither is a
/// reason for `hosts` to fail. Both leave the LAN half of the listing intact,
/// which is the degradation the app makes too.
fn fetch_account(control_plane: &str, roots: zest_cloud::tls::Roots) -> AccountCache {
    use zest_daemon::account::{fetch_hosts, stored_app_token, HttpsAccountApi};

    let now = Some(Instant::now());
    let token = match stored_app_token(&zest_mesh::keystore::OsKeyStore) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return AccountCache {
                fetched: now,
                note: Some(
                    "this machine is not signed in to an account, so only machines on this \
                     network are listed"
                        .into(),
                ),
                ..AccountCache::default()
            }
        }
        Err(e) => {
            return AccountCache {
                fetched: now,
                note: Some(format!("the credential store could not be read ({e})")),
                ..AccountCache::default()
            }
        }
    };
    let api = match HttpsAccountApi::new(control_plane, roots) {
        Ok(a) => a,
        Err(e) => {
            return AccountCache {
                fetched: now,
                note: Some(format!("the control plane address is not usable ({e})")),
                ..AccountCache::default()
            }
        }
    };
    match fetch_hosts(&api, &token) {
        Ok(listing) => AccountCache {
            fetched: now,
            relay_origin: listing.relay_origin,
            // A listing came back, so the token is live. This is
            // `best_route`'s `signed_in` and nothing more: whether a ticket
            // could be minted at all.
            signed_in: true,
            hosts: listing
                .hosts
                .into_iter()
                .map(|h| AccountEntry {
                    host: h.host,
                    label: h.label,
                    relay_online: h.relay_online,
                })
                .collect(),
            note: None,
        },
        Err(e) => AccountCache {
            fetched: now,
            note: Some(format!("the account's machines could not be listed ({e})")),
            ..AccountCache::default()
        },
    }
}

impl Fleet for LiveFleet {
    fn view(&self) -> FleetView {
        let mut notes = Vec::new();
        let mut hosts = vec![self.local_row(None)];
        hosts.extend(self.discovered(&mut notes));
        let (entries, relay_origin, signed_in) = self.account(&mut notes);
        merge_account(&mut hosts, Some(&entries));
        FleetView { hosts, relay_origin, signed_in, notes }
    }

    fn report_dial(&self, host: HostId, connected: bool) {
        let state = self.mdns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let MdnsState::Browsing(d) = &*state {
            d.report_dial(host, connected);
        }
    }
}

/// A fixed fleet, for tests and for the local-only path.
///
/// Not a placeholder: a server whose `hosts` is exactly its own machine is
/// what running with no network at all should look like, and it is the shape
/// every fleet test uses so no test needs multicast or a control plane.
pub struct StaticFleet {
    hosts: Vec<FleetHost>,
    relay_origin: Option<String>,
    signed_in: bool,
    dials: Mutex<Vec<(HostId, bool)>>,
}

impl StaticFleet {
    #[must_use]
    pub fn new(hosts: Vec<FleetHost>) -> Self {
        Self { hosts, relay_origin: None, signed_in: false, dials: Mutex::new(Vec::new()) }
    }

    #[must_use]
    pub fn with_relay(mut self, origin: &str, signed_in: bool) -> Self {
        self.relay_origin = Some(origin.to_string());
        self.signed_in = signed_in;
        self
    }

    /// Every dial reported, in order — so a test can assert that a refused
    /// dial was fed back rather than swallowed.
    #[must_use]
    pub fn dials(&self) -> Vec<(HostId, bool)> {
        self.dials.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

impl Fleet for StaticFleet {
    fn view(&self) -> FleetView {
        FleetView {
            hosts: self.hosts.clone(),
            relay_origin: self.relay_origin.clone(),
            signed_in: self.signed_in,
            notes: Vec::new(),
        }
    }

    fn report_dial(&self, host: HostId, connected: bool) {
        self.dials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((host, connected));
    }
}

/// Why [`zest_fleet::best_route`] answered `None`, in words an agent can act
/// on.
///
/// The refusal is the product here, not a diagnostic. `best_route` is a
/// three-fact rule and each way of failing it asks for a different act — start
/// a daemon, join a network, sign in — so collapsing them to "unreachable"
/// turns every one of those into a dead end. Written beside the rule's own
/// order so the two cannot disagree about which fact was missing.
#[must_use]
pub fn why_unreachable(host: &FleetHost, relay_origin: Option<&str>, signed_in: bool) -> String {
    if host.local {
        return "this server has no route to its own daemon".into();
    }
    if host.presence == Presence::Online && host.address.is_none() {
        // The DNS-SD trap: an instance that resolves with an empty address set
        // (a SRV target that is not a DNS label). Says "advertising" and
        // offers nothing to dial, which reads exactly like a sleeping laptop.
        return "it is advertising on this network but published no address to dial".into();
    }
    if !host.enrolled {
        return match host.presence {
            Presence::Unreachable => {
                "its daemon refused a dial, and it is not one of the account's machines, so \
                 there is no tunnel to fall back to"
                    .into()
            }
            Presence::Away => {
                "it stopped advertising on this network, and it is not one of the account's \
                 machines"
                    .into()
            }
            _ => "it is not on this network, and it is not one of the account's machines".into(),
        };
    }
    if !signed_in {
        return "it is one of the account's machines, but this server is signed out and cannot \
                mint a relay ticket"
            .into();
    }
    if relay_origin.is_none() {
        return "it is one of the account's machines, but this deployment has no relay".into();
    }
    // Every gate passed and `best_route` still said no, which the rule cannot
    // currently produce. Said honestly rather than guessed at.
    "no route".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_fleet::{best_route, fixture};

    /// Every arm of [`why_unreachable`] against the row that produces it, and
    /// each one paired with `best_route` actually refusing — because a reason
    /// for a refusal that did not happen is worse than no reason, and the two
    /// live in different crates.
    #[test]
    fn every_way_of_having_no_route_says_which_fact_was_missing() {
        const RELAY: Option<&str> = Some("wss://relay.example");

        let mut never_seen = fixture::host(2, "attic");
        never_seen.presence = Presence::Unseen;
        never_seen.address = None;
        assert!(best_route(&never_seen, None, RELAY, true).is_none());
        assert!(
            why_unreachable(&never_seen, RELAY, true).contains("not on this network"),
            "a machine nothing has heard of and the account does not list"
        );

        let mut asleep = fixture::host(3, "laptop");
        asleep.presence = Presence::Away;
        asleep.address = None;
        assert!(why_unreachable(&asleep, RELAY, true).contains("stopped advertising"));

        let mut refusing = fixture::host(4, "forge");
        refusing.presence = Presence::Unreachable;
        assert!(
            why_unreachable(&refusing, RELAY, true).contains("refused a dial"),
            "#22's dead daemon behind a live record, with no tunnel behind it"
        );

        // The DNS-SD trap, and the one arm that must be checked before
        // enrolment: this row can be *both* advertising-with-no-address and
        // enrolled, and the address is the fact the agent can act on.
        let mut empty = fixture::host(5, "ghost");
        empty.address = None;
        assert!(best_route(&empty, None, None, false).is_none());
        assert!(why_unreachable(&empty, None, false).contains("published no address"));

        let mut enrolled = fixture::host(6, "win");
        enrolled.presence = Presence::Unseen;
        enrolled.address = None;
        enrolled.enrolled = true;
        assert!(best_route(&enrolled, None, RELAY, false).is_none());
        assert!(
            why_unreachable(&enrolled, RELAY, false).contains("signed out"),
            "signing in is the act, and it is not the same act as starting a daemon"
        );
        assert!(best_route(&enrolled, None, None, true).is_none());
        assert!(why_unreachable(&enrolled, None, true).contains("no relay"));
    }

    #[test]
    fn a_reachable_machine_is_never_asked_why_it_is_not() {
        // Guards the pairing rather than the text: every row `best_route`
        // answers for must not reach `why_unreachable` at all, and the fixture
        // fleet's remote rows are exactly the reachable case.
        for h in fixture::fleet().iter().filter(|h| !h.local) {
            assert!(
                best_route(h, None, None, false).is_some(),
                "an online row with an address is reachable over the LAN alone"
            );
        }
    }

    #[test]
    fn a_static_fleet_records_the_dials_it_is_told_about() {
        let f = StaticFleet::new(fixture::fleet());
        let id = fixture::host(2, "mac").host;
        f.report_dial(id, false);
        assert_eq!(f.dials(), vec![(id, false)], "a refused dial is fed back, not swallowed");
    }
}
