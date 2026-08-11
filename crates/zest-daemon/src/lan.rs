//! Serving other machines.
//!
//! TCP, because the peer is on another box and there is no filesystem to lean
//! on — which is exactly why this module cannot be reached without an
//! [`Authenticator`](crate::auth::Authenticator). `local.rs` explains at length
//! why the loopback path is *not* TCP; this is the case it was contrasted with.
//!
//! # Bind before advertising
//!
//! `bind` and `serve` are split, and that split is load-bearing. The port has
//! to be known before it is announced: `discovery/txt.rs` deliberately keeps
//! the port out of the TXT record because SRV already carries it, and *"two
//! sources of truth that can disagree produce a connection refused with no
//! obvious cause"*. `local_addr()` is the single source, and it only exists
//! after the socket is bound.
//!
//! # What a public port needs that a unix socket does not
//!
//! Three things, none of them optional:
//!
//! - **A handshake watchdog.** A connection that opens and says nothing pins a
//!   thread for as long as it likes.
//! - **A cap on unauthenticated connections**, so that pinning threads is
//!   bounded even when it is deliberate.
//! - **A per-peer failed-auth limit.** This is what makes a six-digit
//!   pairing code sound: one online guess per connection at 1-in-10⁶ is only an
//!   argument if connections cannot be made without limit.
//!
//! The last two live in a [`Gate`], which the process builds once and hands to
//! every transport, because the thread pinned by a stalled handshake is a
//! daemon-wide resource and not the LAN listener's.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::auth::{Auth, Authenticator};
use crate::server::{serve_lan, Registry};
use crate::{DaemonConfig, DaemonError};

/// The port a host advertises when nothing else is asked for.
///
/// Already the fixture value across `mesh_probe`, `roster.rs` and
/// `layered.rs`. A stable default keeps one firewall rule and one static-config
/// entry valid across restarts.
pub const DEFAULT_PORT: u16 = 7717;

/// How long a connection may take to finish the handshake.
///
/// Generous, because it includes a human on the other end only in the
/// *pairing* case — and that case is not waiting here, it is waiting inside an
/// authenticated connection. This bound is for a peer that connects and says
/// nothing at all.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections may be mid-handshake at once, across every transport.
const MAX_UNAUTHENTICATED: usize = 32;

/// Failed handshakes from one peer before it is made to wait.
const MAX_FAILURES: u32 = 5;

/// How long a rate-limited peer waits.
const COOLDOWN: Duration = Duration::from_secs(60);

/// A bound port, not yet serving.
pub struct LanListener {
    listener: TcpListener,
    addr: SocketAddr,
    /// How long a connection may take to prove itself.
    ///
    /// A real field rather than a constant, so a test can reach it. The first
    /// version of this watchdog cut *every* connection ten seconds after
    /// accept and every LAN test passed, because every one of them finished in
    /// milliseconds. A timeout no test can afford to wait for is a timeout no
    /// test checks — and `cfg!(test)` does not help, because it is false when
    /// the library is compiled for an integration test.
    handshake_timeout: Duration,
}

impl LanListener {
    /// Take the port.
    ///
    /// Falls back to an ephemeral port if the requested one is taken, and the
    /// caller must advertise [`Self::local_addr`] rather than what it asked
    /// for. `default_socket_path` is per-user, so two people logged into one
    /// machine legitimately run two daemons — and a hard-coded port would make
    /// the second simply fail to start.
    pub fn bind(bind_addr: &str, port: u16) -> Result<Self, DaemonError> {
        let listener = match TcpListener::bind((bind_addr, port)) {
            Ok(l) => l,
            Err(e) if port != 0 => {
                tracing::warn!(
                    port,
                    error = %e,
                    "the preferred port is taken; falling back to an ephemeral one"
                );
                TcpListener::bind((bind_addr, 0))
                    .map_err(|e| DaemonError::Transport(e.to_string()))?
            }
            Err(e) => return Err(DaemonError::Transport(e.to_string())),
        };
        let addr = listener.local_addr().map_err(|e| DaemonError::Transport(e.to_string()))?;
        Ok(Self { listener, addr, handshake_timeout: HANDSHAKE_TIMEOUT })
    }

    /// Shorten the handshake deadline. For tests.
    #[must_use]
    pub const fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// What was actually bound. **Advertise this, never the requested port.**
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Give up the socket, for a transport that wraps its own protocol around
    /// the same hardened accept loop. `ws.rs` is that transport.
    pub(crate) fn into_parts(self) -> (TcpListener, Duration) {
        (self.listener, self.handshake_timeout)
    }

    /// Accept until the process ends.
    ///
    /// Takes an [`Authenticator`] by value and not by option: there is no way
    /// to call this without one, which is what makes "do not turn on
    /// `listen_lan` before pairing exists" a property of the types.
    pub fn serve_forever(
        self,
        config: DaemonConfig,
        registry: Arc<Registry>,
        auth: Arc<Authenticator>,
        gate: Arc<Gate>,
    ) -> Result<(), DaemonError> {
        tracing::info!(addr = %self.addr, "serving the LAN");
        accept_hardened(
            self.listener,
            self.handshake_timeout,
            gate,
            move |stream, peer, watchdog, slot| {
                let write_half = stream
                    .try_clone()
                    .map_err(|e| DaemonError::Transport(e.to_string()))?;
                // `Auth::Proof`: the trust store decides, and this connection
                // may not approve other devices.
                serve_lan(
                    stream,
                    write_half,
                    config.clone(),
                    Arc::clone(&registry),
                    Auth::Proof(Arc::clone(&auth)),
                    peer,
                    watchdog,
                    slot,
                )
            },
        )
    }
}

/// Accept connections forever with the hardening every public port needs.
///
/// The module docs list the three obligations — watchdog, mid-handshake cap,
/// per-peer failure limit — and this loop is where they are applied: the last
/// two out of the caller's [`Gate`], the watchdog here. Factored out of
/// [`LanListener`] so a second public transport (the WebSocket listener) cannot
/// accidentally take a port without taking the posture. `serve_conn`
/// runs on its own thread with the watchdog already armed, so however long its
/// transport takes to say hello — an HTTP upgrade, a raw `Hello`, nothing at
/// all — the same deadline covers it.
///
/// The closure's `Result` is logged, not returned: one connection's failure is
/// that connection's news, and the accept loop outlives them all.
pub(crate) fn accept_hardened<F>(
    listener: TcpListener,
    handshake_timeout: Duration,
    gate: Arc<Gate>,
    serve_conn: F,
) -> Result<(), DaemonError>
where
    F: Fn(TcpStream, String, WatchdogHandle, Countdown) -> Result<(), DaemonError>
        + Send
        + Sync
        + 'static,
{
    let serve_conn = Arc::new(serve_conn);

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let peer = stream
            .peer_addr()
            .map_or_else(|_| "unknown".to_string(), |a| a.to_string());
        let key = PeerKey::from(&peer);

        // Refused connections are accepted and closed rather than left queued:
        // a peer told no immediately can back off, where one left hanging
        // cannot.
        let guard = match gate.admit(key.clone()) {
            Ok(guard) => guard,
            Err(Refused::Cooling(wait)) => {
                tracing::warn!(%peer, ?wait, "refusing: too many failed handshakes");
                drop(stream);
                continue;
            }
            Err(Refused::Busy) => {
                tracing::warn!(%peer, "refusing: too many connections are mid-handshake");
                drop(stream);
                continue;
            }
        };

        let serve_conn = Arc::clone(&serve_conn);
        let gate = Arc::clone(&gate);

        std::thread::spawn(move || {
            let watchdog = Watchdog::start(&stream, handshake_timeout);

            // `guard` is dropped when the handshake completes, not when the
            // connection ends -- the cap is on connections *mid-handshake*, and
            // holding it for the session made it a hard limit of 32 concurrent
            // clients.
            let result = serve_conn(stream, peer.clone(), watchdog.handle(), guard);

            // Read before disarming: `authenticated` is `!armed && !fired`,
            // so disarming first would report every stranger's dropped
            // connection as a success and starve the rate limiter -- the
            // exact bug its comment says it once had.
            let authenticated = watchdog.authenticated();
            watchdog.disarm();
            gate.settle(&key, authenticated);
            if let Err(e) = result {
                tracing::warn!(%peer, error = %e, "connection ended");
            }
        });
    }
    Ok(())
}

/// Everything a public port is not allowed to be without: the mid-handshake
/// cap and the per-peer failure limit, in one object the process owns.
///
/// **One `Gate` for the whole daemon, and that is a change.** Both were created
/// per accept loop, so the LAN listener and the WebSocket listener each had
/// their own budget of 32 mid-handshake connections. What the cap protects is
/// threads, which are a property of the process rather than of a socket — and
/// ADR-009 needs every logical relay stream counted against the same number,
/// since under dial-back a stream *is* a socket and a host with a control link
/// can be asked to open them without limit. The cost is real and worth stating:
/// a flood on the LAN port can now crowd out a relay attach, where before each
/// transport starved only itself.
pub struct Gate {
    unauthenticated: Arc<AtomicUsize>,
    limiter: RateLimiter,
}

/// Why a connection was not admitted.
#[derive(Debug)]
pub enum Refused {
    /// This peer has failed too many handshakes and must wait this long.
    Cooling(Duration),
    /// Too many connections are mid-handshake, daemon-wide.
    Busy,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    #[must_use]
    pub fn new() -> Self {
        Self { unauthenticated: Arc::new(AtomicUsize::new(0)), limiter: RateLimiter::default() }
    }

    /// Take a mid-handshake slot for this peer, or say why not.
    ///
    /// The returned [`Countdown`] gives the slot back — on drop, or earlier
    /// when the handshake completes.
    pub fn admit(&self, key: PeerKey) -> Result<Countdown, Refused> {
        if let Some(wait) = self.limiter.blocked(key) {
            return Err(Refused::Cooling(wait));
        }
        // One bounded increment, not load-then-add. The separate check was
        // inherited from the per-listener version and was already racy there;
        // it matters more now that one budget of 32 is shared by every
        // transport, because the threads that oversubscribe it are precisely
        // the ones a flood creates — every racer sees 31 and every racer adds.
        // `fetch_update` refuses at the boundary instead, so the cap is a cap.
        self.unauthenticated
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_UNAUTHENTICATED).then_some(n + 1)
            })
            .map_err(|_| Refused::Busy)?;
        Ok(Countdown(Some(Arc::clone(&self.unauthenticated))))
    }

    /// Record how a connection ended. `authenticated` is what feeds the
    /// per-peer limit, so it must be the watchdog's real answer.
    pub fn settle(&self, key: &PeerKey, authenticated: bool) {
        if authenticated {
            self.limiter.succeeded(key.clone());
        } else {
            self.limiter.failed(key.clone());
        }
    }
}

/// Decrements the mid-handshake count once, however it is released.
///
/// `Option` so that completing the handshake can release it early: the cap is
/// on connections that have not proved themselves, and a guard held for the
/// life of the session turns it into a hard limit on *total* clients.
pub struct Countdown(Option<Arc<AtomicUsize>>);

impl Countdown {
    /// Release now rather than on drop.
    pub fn release(&mut self) {
        if let Some(counter) = self.0.take() {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Countdown {
    fn drop(&mut self) {
        self.release();
    }
}

/// How a connection is severed once its deadline passes.
///
/// `shutdown(Both)` rather than a read timeout, and that is the whole trick:
/// `serve`'s reader treats *any* `Err` as fatal, so setting a timeout on the
/// socket would kill healthy connections the moment a session went quiet —
/// which is most of the time. Shutting the socket down unblocks the read once,
/// and `serve` unwinds through its ordinary "the client went away" path with no
/// change to its loop.
///
/// A trait because the *cut* is what varies and the watchdog is not: a TLS
/// stream is not a `TcpStream` and cannot be `try_clone`d into one, and under
/// ADR-009's dial-back a relayed stream is a socket of its own to shut down.
pub trait Cut: Send + Sync + 'static {
    fn cut(&self);
}

impl Cut for TcpStream {
    fn cut(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

/// Cuts a connection that never finishes **its handshake**, by way of [`Cut`].
///
/// # It has to watch the handshake, not the connection
///
/// The first version set its flag in `finish()`, which runs *after* `serve`
/// returns — that is, once the connection has already ended. So the flag was
/// never set while it mattered and the watchdog cut every connection ten
/// seconds after accept, healthy or not: a paired phone was disconnected on a
/// ten-second cycle forever, and a device waiting for approval was dropped
/// long before the 120-second window the host had just promised it.
///
/// The signal it needs is "the handshake completed", which only the
/// `Connection` knows. `armed` is shared with the watching thread and cleared
/// by [`Handshake::completed`] the moment the gate opens.
struct Watchdog {
    /// Cleared when the handshake completes. Read by the watching thread.
    armed: Arc<AtomicBool>,
    /// How long, from the connection's start, before it is cut.
    ///
    /// Movable, because "has not finished the handshake" covers two states that
    /// deserve different deadlines: silent, and waiting for a human to say yes.
    deadline: Arc<AtomicU64>,
    /// Whether the watchdog ever fired, so the caller can tell a refused
    /// connection from one that simply ended.
    fired: Arc<AtomicBool>,
}

impl Watchdog {
    fn start(stream: &TcpStream, timeout: Duration) -> Self {
        match stream.try_clone() {
            Ok(clone) => Self::start_with(Arc::new(clone), timeout),
            // As before: with no second handle there is nothing to cut, so the
            // connection runs unwatched rather than being refused.
            Err(_) => Self::unwatched(timeout),
        }
    }

    fn unwatched(timeout: Duration) -> Self {
        Self {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(false)),
            // Milliseconds from now, so the waiting thread can be told to wait
            // longer without being restarted.
            deadline: Arc::new(AtomicU64::new(timeout.as_millis() as u64)),
        }
    }

    fn start_with(cut: Arc<dyn Cut>, timeout: Duration) -> Self {
        let this = Self::unwatched(timeout);
        let armed = Arc::clone(&this.armed);
        let fired = Arc::clone(&this.fired);
        let deadline = Arc::clone(&this.deadline);
        std::thread::spawn(move || {
            let started = Instant::now();
            // Slept in slices rather than once, because the deadline can
            // move while we are asleep: a connection that reaches the
            // approval prompt is waiting on a person, not stalled, and the
            // bound that applies to it is the pairing window rather than the
            // handshake one.
            loop {
                if !armed.load(Ordering::Acquire) {
                    return;
                }
                let want = Duration::from_millis(deadline.load(Ordering::Acquire));
                let Some(left) = want.checked_sub(started.elapsed()) else { break };
                std::thread::sleep(left.min(Duration::from_millis(250)));
            }
            // `swap`, not `load`: the connection may be ending on its own
            // right now, and exactly one of us gets to say what happened.
            if armed.swap(false, Ordering::AcqRel) {
                tracing::warn!("a connection never finished its handshake; closing");
                fired.store(true, Ordering::Release);
                cut.cut();
            }
        });
        this
    }

    /// A handle the connection disarms when its handshake completes.
    fn handle(&self) -> WatchdogHandle {
        WatchdogHandle {
            armed: Arc::clone(&self.armed),
            deadline: Arc::clone(&self.deadline),
        }
    }

    /// The connection is over; there is nothing left to cut.
    ///
    /// Without this, the deadline fired for every connection that *ended*
    /// unauthenticated — including a port probe that connected and closed in a
    /// millisecond — and logged, ten seconds after the fact, that it was
    /// closing a socket which no longer existed. One reachability probe every
    /// ten seconds turned every daemon log into a scroll of warnings about
    /// nothing. The warning now means the one thing worth warning about: a
    /// live connection was cut.
    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }

    /// Whether this connection ever got past the handshake.
    ///
    /// The rate limiter depends on this being a real answer: when it was
    /// always `true`, `RateLimiter::failed` was unreachable and the per-address
    /// limit that makes a six-digit pairing code defensible did not exist.
    fn authenticated(&self) -> bool {
        !self.armed.load(Ordering::Acquire) && !self.fired.load(Ordering::Acquire)
    }
}

/// Disarms the handshake watchdog. Held by the connection.
#[derive(Clone)]
pub struct WatchdogHandle {
    armed: Arc<AtomicBool>,
    deadline: Arc<AtomicU64>,
}

impl WatchdogHandle {
    /// The handshake completed; stop watching.
    pub fn completed(&self) {
        self.armed.store(false, Ordering::Release);
    }

    /// This connection is waiting for a person to approve it.
    ///
    /// **Not the same as completing.** The connection is still unauthenticated
    /// and still holds its mid-handshake slot — releasing that here would let
    /// anyone hold slots open by asking to pair and never being answered. Only
    /// the deadline moves, to just past the pairing window, so the queue's own
    /// expiry denies the request and the client is told *why* instead of
    /// watching the socket vanish.
    ///
    /// Found on a real LAN: the host advertised a 120s window and cut the
    /// connection after 10. The in-process tests missed it because they proved
    /// the watchdog leaves a *welcomed* connection alone, and a device waiting
    /// for approval is precisely the one that has not been welcomed.
    pub fn awaiting_approval(&self, window: Duration) {
        let ms = window.as_millis().min(u128::from(u64::MAX)) as u64;
        self.deadline.fetch_max(ms, Ordering::AcqRel);
    }
}

/// What failed handshakes are counted against.
///
/// Opaque, and variants rather than a bare address, so that two peers which
/// merely *look* alike cannot share a counter: a machine on the LAN at
/// `203.0.113.4` and a client that reached us through a relay whose edge is at
/// `203.0.113.4` are not the same peer, and giving them one budget lets either
/// lock the other out.
///
/// # The relay hazard, stated before there is a relay
///
/// Behind a relay **every** connection carries the relay's address. Five failed
/// handshakes is a generous allowance for one machine guessing a six-digit code
/// and no allowance at all for a whole fleet behind one edge — one hostile peer
/// would take the address to [`MAX_FAILURES`] and deny every other device for
/// [`COOLDOWN`], repeatedly, at no cost to itself. So when the relay path lands,
/// what a relayed connection settles against must be something the *peer* owns —
/// its attach ticket — and not the socket it arrived on. [`Self::Relay`] exists
/// to keep that decision visible and to keep the two address spaces apart in
/// the meantime; it is not yet a correct key on its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PeerKey {
    /// A machine on this network, keyed on the address **without the port**: a
    /// client reconnecting comes from a *new* ephemeral port every time, so
    /// counting per `addr:port` would count to one forever and never limit
    /// anything.
    Lan(String),
    /// A client that reached us through the relay, at the edge's address.
    Relay(IpAddr),
    /// The loopback transport, which has no address and one trust boundary:
    /// reaching it at all means being this user on this machine.
    Loopback,
}

impl From<&str> for PeerKey {
    fn from(peer: &str) -> Self {
        Self::Lan(peer.rsplit_once(':').map_or_else(|| peer.to_string(), |(addr, _)| addr.to_string()))
    }
}

impl From<&String> for PeerKey {
    // Every call site has an `addr:port` it formatted or was handed as a
    // `String`, and a generic parameter does not deref-coerce for it.
    fn from(peer: &String) -> Self {
        Self::from(peer.as_str())
    }
}

/// Failed handshakes per peer.
///
/// The reason a six-digit code is defensible: an attacker gets one guess per
/// connection, and connections are not free.
#[derive(Default)]
struct RateLimiter {
    seen: Mutex<HashMap<PeerKey, (u32, Instant)>>,
}

impl RateLimiter {
    /// How long this peer must wait, if at all.
    fn blocked(&self, peer: impl Into<PeerKey>) -> Option<Duration> {
        let key = peer.into();
        let seen = self.seen.lock().expect("rate limiter lock");
        let (failures, last) = seen.get(&key)?;
        if *failures < MAX_FAILURES {
            return None;
        }
        let elapsed = last.elapsed();
        (elapsed < COOLDOWN).then(|| COOLDOWN - elapsed)
    }

    fn failed(&self, peer: impl Into<PeerKey>) {
        let mut seen = self.seen.lock().expect("rate limiter lock");
        let entry = seen.entry(peer.into()).or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();
    }

    fn succeeded(&self, peer: impl Into<PeerKey>) {
        self.seen.lock().expect("rate limiter lock").remove(&peer.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_port_is_what_gets_advertised() {
        // The port must be known before it is announced: SRV is the single
        // source of truth for it, and two sources that can disagree produce a
        // connection refused with no obvious cause.
        let l = LanListener::bind("127.0.0.1", 0).expect("bind");
        assert_ne!(l.local_addr().port(), 0, "an ephemeral port was advertised as 0");
    }

    #[test]
    fn a_taken_port_falls_back_rather_than_failing_to_start() {
        // Two users on one machine legitimately run two daemons, and a
        // hard-coded port would make the second simply not start.
        let first = LanListener::bind("127.0.0.1", 0).expect("bind");
        let taken = first.local_addr().port();

        let second = LanListener::bind("127.0.0.1", taken).expect("fall back");
        assert_ne!(second.local_addr().port(), taken, "both listeners claimed one port");
    }

    #[test]
    fn the_rate_limiter_counts_by_address_not_by_connection() {
        // A reconnecting client comes from a new ephemeral port every time, so
        // counting per addr:port would count to one forever.
        let limiter = RateLimiter::default();
        for port in 40_000..40_000 + MAX_FAILURES {
            limiter.failed(&format!("192.168.1.42:{port}"));
        }
        assert!(
            limiter.blocked("192.168.1.42:55555").is_some(),
            "failures from one machine were counted as separate peers"
        );
        assert!(limiter.blocked("192.168.1.43:40000").is_none(), "the wrong peer was blocked");
    }

    #[test]
    fn a_successful_handshake_clears_the_count() {
        // Otherwise a device that mistypes a code a few times over a week is
        // eventually locked out for reasons nobody can reconstruct.
        let limiter = RateLimiter::default();
        for _ in 0..MAX_FAILURES {
            limiter.failed("10.0.0.5:1234");
        }
        assert!(limiter.blocked("10.0.0.5:9999").is_some());
        limiter.succeeded("10.0.0.5:1234");
        assert!(limiter.blocked("10.0.0.5:9999").is_none());
    }

    #[test]
    fn a_completed_handshake_disarms_the_watchdog() {
        // The bug: `done` was only set after `serve` returned, so the watchdog
        // fired on every connection ten seconds after accept -- a paired phone
        // was disconnected on a ten-second cycle forever.
        let w = Watchdog {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(false)),
            deadline: Arc::new(AtomicU64::new(10_000)),
        };
        assert!(!w.authenticated(), "an armed watchdog means the handshake is unfinished");

        w.handle().completed();
        assert!(w.authenticated(), "completing the handshake must disarm it");
    }

    /// The case the other watchdog test is shaped wrong to see.
    ///
    /// `a_completed_handshake_disarms_the_watchdog` proves a *welcomed*
    /// connection is left alone — and a device waiting to be approved is exactly
    /// the one that has not been welcomed. So pairing over the LAN was cut at
    /// the handshake timeout, ten seconds into a window the host itself
    /// advertises as 120, and every in-process test agreed that was fine.
    ///
    /// Found by two machines on a real network, not here.
    #[test]
    fn waiting_for_a_person_extends_the_deadline_without_disarming() {
        let w = Watchdog {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(false)),
            deadline: Arc::new(AtomicU64::new(10_000)),
        };

        w.handle().awaiting_approval(Duration::from_secs(130));

        assert_eq!(
            w.deadline.load(Ordering::Acquire),
            130_000,
            "the deadline must move past the pairing window, or the socket is \
             cut long before anyone can answer the prompt"
        );
        assert!(
            !w.authenticated(),
            "still unauthenticated: waiting for approval is not the same as \
             having been approved, and treating it as such would hand an \
             unpaired device the rate limiter's benefit of the doubt"
        );
    }

    /// The extension only ever moves outward.
    #[test]
    fn a_shorter_window_never_pulls_the_deadline_in() {
        // Otherwise a second call with a smaller window -- a shorter pairing
        // timeout, a retry -- would shorten a deadline already granted, and the
        // connection would be cut mid-prompt for a reason nobody could see.
        let w = Watchdog {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(false)),
            deadline: Arc::new(AtomicU64::new(10_000)),
        };
        let h = w.handle();
        h.awaiting_approval(Duration::from_secs(130));
        h.awaiting_approval(Duration::from_secs(5));
        assert_eq!(w.deadline.load(Ordering::Acquire), 130_000);
    }

    #[test]
    fn a_watchdog_that_fired_is_not_reported_as_authenticated() {
        // This is what feeds the rate limiter. When `finish()` could only
        // return true, `RateLimiter::failed` was unreachable and the
        // per-address limit that makes a six-digit code defensible did not
        // exist at all.
        let w = Watchdog {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(true)),
            deadline: Arc::new(AtomicU64::new(10_000)),
        };
        assert!(!w.authenticated());
    }

    #[test]
    fn the_mid_handshake_slot_is_released_once_and_early() {
        // Held for the life of the connection, the cap became a hard limit of
        // 32 concurrent clients rather than 32 mid-handshake.
        let count = Arc::new(AtomicUsize::new(1));
        let mut guard = Countdown(Some(Arc::clone(&count)));
        guard.release();
        assert_eq!(count.load(Ordering::Acquire), 0, "the slot was not released");
        guard.release();
        drop(guard);
        assert_eq!(count.load(Ordering::Acquire), 0, "the slot was released more than once");
    }

    #[test]
    fn an_address_with_no_port_is_still_usable_as_a_key() {
        assert_eq!(PeerKey::from("192.168.1.42:51314"), PeerKey::Lan("192.168.1.42".into()));
        assert_eq!(PeerKey::from("unknown"), PeerKey::Lan("unknown".into()));
    }

    #[test]
    fn a_lan_peer_and_a_relayed_one_at_the_same_address_do_not_share_a_budget() {
        // The reason the key is an enum rather than an address: an edge that
        // happens to sit at a fleet member's address must not be able to spend
        // its allowance, in either direction.
        let limiter = RateLimiter::default();
        let relayed = PeerKey::Relay("203.0.113.4".parse().expect("addr"));
        for _ in 0..MAX_FAILURES {
            limiter.failed(relayed.clone());
        }
        assert!(limiter.blocked(relayed).is_some(), "the relay edge should be cooling off");
        assert!(
            limiter.blocked("203.0.113.4:44000").is_none(),
            "a LAN peer was locked out by failures that were not its own"
        );
    }

    #[test]
    fn the_mid_handshake_cap_is_one_budget_across_transports() {
        // The behaviour change this commit makes on purpose: the count used to
        // be created per accept loop, so the LAN port and the WebSocket port
        // had 32 each. Threads belong to the process.
        let gate = Gate::new();
        let mut held = Vec::new();
        for i in 0..MAX_UNAUTHENTICATED {
            held.push(
                gate.admit(PeerKey::Lan(format!("10.0.0.{i}")))
                    .unwrap_or_else(|_| panic!("slot {i} should have been free")),
            );
        }
        assert!(
            gate.admit(PeerKey::Loopback).is_err(),
            "a transport that shares the gate must see the budget the others spent"
        );

        held.pop();
        assert!(gate.admit(PeerKey::Loopback).is_ok(), "a released slot must be reusable");
    }

    #[test]
    fn a_peer_that_keeps_failing_is_refused_by_the_gate_rather_than_admitted() {
        // `settle(_, false)` is what feeds the limiter, and `admit` is what
        // reads it back. Both halves in one place, because either alone is a
        // rate limiter nothing consults.
        let gate = Gate::new();
        let key = PeerKey::from("192.168.1.9:1000");
        for _ in 0..MAX_FAILURES {
            drop(gate.admit(key.clone()).expect("still under the limit"));
            gate.settle(&key, false);
        }
        assert!(
            matches!(gate.admit(key.clone()), Err(Refused::Cooling(_))),
            "the sixth attempt from a peer that has failed five times must wait"
        );

        gate.settle(&key, true);
        assert!(
            gate.admit(key).is_ok(),
            "a success clears the count, or a device that mistypes a code over a \
             week is eventually locked out for reasons nobody can reconstruct"
        );
    }

    #[test]
    fn the_watchdog_cuts_what_it_was_given_when_the_deadline_passes() {
        // No test reached the watchdog's own thread before -- they all built
        // the struct by hand -- so the cut itself, the thing the whole object
        // exists for, was never exercised.
        struct Scissors(Arc<AtomicBool>);
        impl Cut for Scissors {
            fn cut(&self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let cut = Arc::new(AtomicBool::new(false));
        let w = Watchdog::start_with(Arc::new(Scissors(Arc::clone(&cut))), Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(300));
        assert!(cut.load(Ordering::Acquire), "a handshake that never finished was left connected");
        assert!(!w.authenticated(), "a connection that was cut is not an authenticated one");
    }

    #[test]
    fn a_completed_handshake_is_never_cut() {
        // The bug this whole object was reshaped around, now observable
        // through the thread rather than through the flag it sets.
        struct Scissors(Arc<AtomicBool>);
        impl Cut for Scissors {
            fn cut(&self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let cut = Arc::new(AtomicBool::new(false));
        let w = Watchdog::start_with(Arc::new(Scissors(Arc::clone(&cut))), Duration::from_millis(30));
        w.handle().completed();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !cut.load(Ordering::Acquire),
            "a paired device was disconnected on a timer it had already beaten"
        );
    }
}
