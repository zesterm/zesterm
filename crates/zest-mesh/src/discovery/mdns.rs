//! The socket half: announcing this host, and hearing the others.
//!
//! Everything hard about discovery lives next door in [`super::roster`] and
//! [`super::txt`], as pure functions. What is left here is an adapter that
//! turns `mdns-sd`'s events into an [`Observation`] and a thread that pumps
//! them, which is deliberately about as much code as it sounds like.
//!
//! # `mdns_sd` appears in no signature below
//!
//! It is pre-1.0 and its `ServiceEvent` is `#[non_exhaustive]`, so a minor
//! release can rename a variant. Confining it to one `match` means the blast
//! radius of that is one function rather than every caller in the fleet.
//!
//! # Sharp edges
//!
//! 1. **Advertising and browsing are separate types.** A client-only device — a
//!    phone, a browser tab, the GUI app before its daemon is up — browses
//!    without ever advertising, and the daemon advertises only while it is
//!    willing to accept connections. Folded into one type,
//!    [`Discovery::stop`] would also silence this host, so closing a fleet
//!    panel would make the machine vanish from everyone *else's*.
//! 2. **One responder, not two.** [`node`] hands out an advertiser and a
//!    discovery sharing a single `ServiceDaemon`. Two of them in one process
//!    means two threads and two binds of UDP 5353 — which macOS tolerates
//!    because `mDNSResponder` already holds it, and Windows' `SO_REUSEADDR`
//!    treats as stealing rather than sharing. **Linux, measured** (#480):
//!    `avahi-daemon` is the analogue there and holds `0.0.0.0:5353` on a
//!    default desktop, and binding beside it is fine — `mesh_probe` reports
//!    `self-visible: yes`, and `avahi-browse -rpt _zesterm._tcp` resolves our
//!    instance, SRV target, address and TXT. Check interoperability with
//!    `avahi-browse`, never with our own browser: hearing ourselves only
//!    proves multicast loopback, which point 3 turns on deliberately.
//! 3. **Multicast loopback stays on.** We hear our own advertisement, and
//!    [`MdnsDiscovery::self_seen`] reports it. Without that, "a firewall is
//!    eating my multicast" and "nothing else is running" are the same empty
//!    listing.
//! 4. **A goodbye is one best-effort packet.** A lid that closes abruptly sends
//!    nothing, so the thread also sweeps on its idle tick. See
//!    [`super::roster::PRESENCE_TIMEOUT`].
//! 5. **The instance name is not the host name.** Spaces and parentheses are
//!    legal in a DNS-SD instance and illegal in a DNS host label. Deriving the
//!    second from the first publishes a service whose SRV target never resolves,
//!    so peers see the host with an empty address set — which looks exactly like
//!    a laptop that is asleep. The host name comes from the [`HostId`] instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::Mutex;
use zest_proto::HostId;

use super::roster::{Observation, ResolvedRecord, Roster, PRESENCE_TIMEOUT};
use super::txt::{instance_name, Advertisement, SERVICE_TYPE};
use super::{Discovery, Peer, PeerRecord};
use crate::MeshError;

/// How long the thread waits for an event before sweeping and re-checking the
/// stop flag. Short enough that `stop()` returns promptly, long enough that an
/// idle fleet costs nothing.
const TICK: Duration = Duration::from_millis(500);

/// How long to wait for a goodbye packet to go out on the way down.
///
/// Bounded, because a shutdown that can hang is worse than a peer that greys
/// out a minute late.
const GOODBYE_GRACE: Duration = Duration::from_millis(200);

fn mesh_err(context: &str, e: &impl std::fmt::Display) -> MeshError {
    MeshError::Discovery(format!("{context}: {e}"))
}

/// An `mdns-sd` responder and its thread, shut down when the last user of it
/// lets go.
///
/// Shared rather than owned by one side because [`node`] hands the same
/// responder to an advertiser and a discovery, and whichever of them was
/// arbitrarily made the owner would silently disable the other by being dropped
/// first — a machine that stops answering, or a fleet listing that stops
/// updating, with nothing logged either way.
struct Responder {
    daemon: ServiceDaemon,
}

impl Responder {
    fn open() -> Result<Arc<Self>, MeshError> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| mesh_err("could not open an mDNS responder", &e))?;
        Ok(Arc::new(Self { daemon }))
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Peers found by mDNS on the local link.
///
/// Synchronous by construction: `mdns-sd` is one background thread and a
/// channel rather than an async runtime, which is the only reason the frozen
/// [`Discovery`] trait can keep `&mut self` start/stop and a plain `&self`
/// snapshot.
pub struct MdnsDiscovery {
    local: HostId,
    roster: Arc<Mutex<Roster>>,
    responder: Option<Arc<Responder>>,
    running: Option<Running>,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl MdnsDiscovery {
    /// `local` is this machine's own id, used to keep our own advertisement out
    /// of our own fleet listing.
    ///
    /// Opens no sockets — that is [`Discovery::start`]'s job — so constructing
    /// one on a config path cannot fail.
    #[must_use]
    pub fn new(local: HostId) -> Self {
        Self {
            local,
            roster: Arc::new(Mutex::new(Roster::new(local))),
            responder: None,
            running: None,
            on_change: None,
        }
    }

    /// Called on the discovery thread whenever the roster changes.
    ///
    /// Must not touch a renderer; the intended use is posting a wakeup.
    #[must_use]
    pub fn on_change(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_change = Some(Arc::new(f));
        self
    }

    /// Everything known, including hosts that have gone away.
    #[must_use]
    pub fn records(&self) -> Vec<PeerRecord> {
        self.roster.lock().records()
    }

    /// Whether our own advertisement was heard back.
    ///
    /// `false` with an empty peer list means our multicast is not reaching the
    /// link. `true` with an empty peer list means nothing else is out there.
    #[must_use]
    pub fn self_seen(&self) -> bool {
        self.roster.lock().saw_self()
    }

    /// Record what happened when somebody dialled a peer. → `Roster::report_dial`.
    ///
    /// The dialling itself stays with the caller — a connect timeout on this
    /// crate's discovery thread would stall the event loop that keeps the
    /// listing fresh, which is exactly backwards.
    pub fn report_dial(&self, host: zest_proto::HostId, connected: bool) -> bool {
        self.roster.lock().report_dial(host, connected, std::time::Instant::now())
    }

    /// Start browsing on a responder that may be shared. See [`node`].
    fn start_on(&mut self, responder: Arc<Responder>) -> Result<(), MeshError> {
        if self.running.is_some() {
            return Ok(());
        }
        let events = responder
            .daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| mesh_err("could not browse the local link", &e))?;

        let roster = Arc::clone(&self.roster);
        let on_change = self.on_change.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("zest-mdns".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    let changed = match events.recv_timeout(TICK) {
                        Ok(event) => match observation_of(event) {
                            Some(observation) => {
                                roster.lock().apply(observation, Instant::now())
                            }
                            None => false,
                        },
                        // The idle tick, which is also where a host that
                        // stopped announcing without a goodbye greys out.
                        Err(_) => roster.lock().sweep(Instant::now(), PRESENCE_TIMEOUT),
                    };
                    if changed {
                        if let Some(notify) = &on_change {
                            notify();
                        }
                    }
                }
            })
            .map_err(|e| mesh_err("could not start the discovery thread", &e))?;

        self.running = Some(Running { stop, thread });
        self.responder = Some(responder);
        Ok(())
    }
}

impl Discovery for MdnsDiscovery {
    fn peers(&self) -> Vec<Peer> {
        self.roster.lock().peers()
    }

    fn start(&mut self) -> Result<(), MeshError> {
        if self.running.is_some() {
            return Ok(());
        }
        self.start_on(Responder::open()?)
    }

    fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.store(true, Ordering::Relaxed);
            // Joining is safe rather than deadlock-prone because the thread's
            // only blocking call is a `recv_timeout` bounded by `TICK`.
            let _ = running.thread.join();
        }
        // Shuts the responder down only if this was the last user of it, so a
        // fleet panel closing does not also stop this machine advertising.
        self.responder = None;
        // The roster is deliberately kept. Stopping discovery is not the same
        // as learning that every machine went away.
    }
}

impl Drop for MdnsDiscovery {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for MdnsDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdnsDiscovery")
            .field("local", &self.local.short())
            .field("running", &self.running.is_some())
            .finish()
    }
}

/// The whole of this crate's knowledge of `mdns-sd`'s event vocabulary.
///
/// `ServiceFound` is ignored on purpose: it carries a fullname and no TXT
/// record, so it names no identity and there is nothing to put in a roster.
fn observation_of(event: ServiceEvent) -> Option<Observation> {
    match event {
        ServiceEvent::ServiceResolved(resolved) => {
            Some(Observation::Resolved(ResolvedRecord {
                fullname: resolved.fullname.clone(),
                txt: resolved
                    .txt_properties
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string()))
                    .collect(),
                port: resolved.port,
                addresses: resolved.addresses.iter().map(|a| a.to_ip_addr()).collect(),
            }))
        }
        ServiceEvent::ServiceRemoved(_ty, fullname) => Some(Observation::Removed { fullname }),
        _ => None,
    }
}

/// This host, announced so the others can find it.
pub struct MdnsAdvertiser {
    responder: Arc<Responder>,
    fullname: String,
    registered: bool,
}

impl MdnsAdvertiser {
    /// Announce `host` at `port` under `label`.
    ///
    /// Takes the id by value rather than a whole identity: mDNS never needs the
    /// private half, and a function that cannot see a secret cannot leak one.
    /// Callers pass `identity.host_id()`.
    pub fn start(host: HostId, label: &str, port: u16) -> Result<Self, MeshError> {
        Self::start_on(Responder::open()?, host, label, port)
    }

    fn start_on(
        responder: Arc<Responder>,
        host: HostId,
        label: &str,
        port: u16,
    ) -> Result<Self, MeshError> {
        let instance = instance_name(label, host);
        // Derived from the id rather than the label, and deliberately not from
        // the instance name. A DNS host label is `[A-Za-z0-9-]`, and the
        // instance name legitimately contains spaces and parentheses — the
        // responder will happily publish an SRV target containing them and then
        // no A record ever resolves for it, so peers find the service, resolve
        // it with an empty address set, and get a `Peer` with no route. That
        // looks exactly like a machine that is asleep.
        let hostname = format!("zesterm-{}.local.", host.short());
        let pairs = Advertisement::new(host, label).to_pairs();
        let properties: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &hostname,
            // No explicit addresses, because `enable_addr_auto` below makes
            // `mdns-sd` enumerate and *track* interfaces itself. Without it,
            // joining a Wi-Fi network or bringing up a VPN would need our own
            // interface-change detection — which is most of the reason to use
            // this crate rather than a socket.
            (),
            port,
            &properties[..],
        )
        .map_err(|e| mesh_err("could not describe this host", &e))?
        .enable_addr_auto();

        let fullname = info.get_fullname().to_string();
        responder
            .daemon
            .register(info)
            .map_err(|e| mesh_err("could not announce this host", &e))?;

        Ok(Self {
            responder,
            fullname,
            registered: true,
        })
    }

    /// The DNS-SD fullname registered, for logs and for `dns-sd -L`.
    #[must_use]
    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    /// Withdraw.
    ///
    /// Waits briefly for the goodbye to go out, so peers grey this machine out
    /// in under a second rather than after the record's TTL — the difference
    /// between "the laptop went to sleep and the listing updated" and "the
    /// listing is wrong for over a minute".
    pub fn stop(&mut self) {
        if !self.registered {
            return;
        }
        self.registered = false;
        if let Ok(status) = self.responder.daemon.unregister(&self.fullname) {
            let _ = status.recv_timeout(GOODBYE_GRACE);
        }
        // The responder itself is shut down by `Drop` on the last `Arc`, so
        // stopping the announcement does not also stop any browsing sharing it.
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for MdnsAdvertiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdnsAdvertiser")
            .field("fullname", &self.fullname)
            .finish()
    }
}

/// An advertiser and a discovery sharing one responder.
///
/// What a daemon should call. `ServiceDaemon` is `Clone` and both `unregister`
/// and `stop_browse` are per-subscription, so one responder serves both halves
/// and either can stop without silencing the other. See sharp edge 2 above for
/// why two responders in one process is a thing to avoid rather than merely
/// wasteful.
pub fn node(
    host: HostId,
    label: &str,
    port: u16,
) -> Result<(MdnsAdvertiser, MdnsDiscovery), MeshError> {
    let responder = Responder::open()?;

    let mut discovery = MdnsDiscovery::new(host);
    discovery.start_on(Arc::clone(&responder))?;
    let advertiser = MdnsAdvertiser::start_on(responder, host, label, port)?;

    Ok((advertiser, discovery))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(seed: u8) -> HostId {
        HostId::from_bytes([seed; 32])
    }

    #[test]
    fn a_discovery_that_never_started_has_no_peers() {
        let d = MdnsDiscovery::new(host(1));
        assert!(
            d.peers().is_empty(),
            "constructing a discovery must not imply a fleet; the sockets open in \
             `start`, so a config path that builds one cannot fail"
        );
    }

    #[test]
    fn stopping_a_discovery_that_never_started_is_a_no_op() {
        let mut d = MdnsDiscovery::new(host(1));
        d.stop();
        d.stop();
        assert!(
            !d.self_seen(),
            "a `stop` before a `start` must not panic and must not invent state"
        );
    }

    #[test]
    fn a_dropped_discovery_does_not_hang_the_test_run() {
        // A `Drop` that joins a thread which was never spawned is how a suite
        // starts hanging on one CI runner out of three.
        drop(MdnsDiscovery::new(host(2)));
    }

    #[test]
    #[ignore = "needs real multicast, which CI runners do not reliably have; \
                run with `cargo test -p zest-mesh -- --ignored`"]
    fn two_responders_in_one_process_find_each_other() {
        // Written with a hard deadline rather than a sleep, so that on a hostile
        // network it fails instead of hanging.
        let (_advertiser, discovery) =
            node(host(0x11), "probe-a", 7717).expect("a responder on this machine");
        let mut other = MdnsDiscovery::new(host(0x22));
        other.start().expect("browse");

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if other.peers().iter().any(|p| p.host == host(0x11)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "the advertiser was not seen within 5s (self_seen: {})",
            discovery.self_seen()
        );
    }
}
