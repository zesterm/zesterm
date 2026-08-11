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
    F: Fn(Severable, String, WatchdogHandle, Countdown) -> Result<(), DaemonError>
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

        // Before anything can read: the poll has to be armed while the reader
        // is still on this thread, or the cut cannot reach it (see
        // [`READ_POLL`]). A socket that will not take a timeout is one the
        // watchdog could never cut, so it is refused rather than served
        // unwatchable.
        //
        // Logged, and not only for symmetry with the two refusals below: a
        // silent `continue` here would refuse every connection with nothing to
        // read out of the daemon, which is the exact shape of the outage this
        // whole module was just fixed for.
        let stream = match Severable::new(stream) {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!(
                    %peer,
                    error = %e,
                    "refusing: the socket would not take a read timeout, so the handshake \
                     watchdog could never cut this connection"
                );
                continue;
            }
        };

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
/// A trait because the *cut* is what varies and the watchdog is not: a TLS
/// stream is not a `TcpStream` and cannot be `try_clone`d into one, and under
/// ADR-009's dial-back a relayed stream is a socket of its own to shut down.
///
/// **There is deliberately no `impl Cut for TcpStream`.** There was, it was
/// `shutdown(Both)`, and on Windows it did nothing — see [`Severable`].
pub trait Cut: Send + Sync + 'static {
    fn cut(&self);

    /// This connection finished its handshake and will never be cut, so
    /// whatever the cut costs while it is possible can stop now.
    ///
    /// Default empty: a cut that costs nothing to stay ready has nothing to
    /// stand down from.
    fn stand_down(&self) {}
}

/// How often a reader that is parked in `read` surfaces to ask whether its
/// connection still exists — and the only reason the cut works on Windows.
///
/// **This constant is the whole of issue #99 and two rounds of Windows CI on
/// #94, so it is worth the paragraph.** A cut has to unpark a reader sitting in
/// `read`. On POSIX `shutdown(Both)` does that. On Winsock it does not, and
/// neither does arming `SO_RCVTIMEO` at cut time: a socket timeout applies to
/// calls issued *after* it is set, never to one already in flight. So the
/// reader stayed parked, its thread lived for ever, and the [`Countdown`] it
/// held kept one of [`MAX_UNAUTHENTICATED`] slots for ever — a budget now
/// shared by every transport, and 32. Thirty-two connections that open and then
/// say nothing would have stopped the daemon accepting any client at all, with
/// no error, no log, and every call involved reporting success.
///
/// Arming the timeout *before the reader can park* is what makes it portable:
/// the reader is never blocked longer than this, so it always comes back to
/// check `severed`. `zest_cloud::tls::READ_POLL` is the same constant for the
/// same reason, arrived at first.
///
/// The cost is one syscall per second per connection, and it is bounded to the
/// handshake rather than paid for the life of a session: [`Cut::stand_down`]
/// takes the timeout back off once the handshake completes, because from that
/// moment the watchdog can never fire. That matters here in a way it does not
/// in `tls.rs` — a daemon that wakes on a timer to find nothing is a laptop
/// that does not sleep, and the 0%-idle property is load-bearing enough to have
/// its own test.
const READ_POLL: Duration = Duration::from_secs(1);

/// A socket whose reader can be cut out from under it on every platform.
///
/// The socket carries a [`READ_POLL`] read timeout while the handshake
/// watchdog is armed, and this wrapper is what makes that invisible to the
/// reader above it: `serve`'s read thread treats *any* `Err` as fatal, so a
/// bare timeout would end a healthy connection the moment its peer went quiet —
/// which, for a device waiting on a human to approve it, is the entire pairing
/// window. [`Read::read`] therefore swallows an elapsed poll and blocks again,
/// and only a real cut ends the read.
pub(crate) struct Severable {
    sock: TcpStream,
    /// Set by the cut, before the socket is shut down. What tells an elapsed
    /// poll from the end of the connection.
    severed: Arc<AtomicBool>,
    /// Whether a cut is still possible — cleared by [`Cut::stand_down`] when
    /// the handshake completes.
    ///
    /// A flag the reader observes rather than a timeout somebody else takes
    /// off, and that is not a style choice. **Measured on Windows: the read
    /// timeout is per *handle*, not per socket.** A `try_clone`d handle
    /// inherits the value at the moment it is duplicated and is independent of
    /// the original from then on, so clearing it through the watchdog's handle
    /// leaves the reader's own poll running for ever — a stand-down that
    /// reports success and does nothing. The reader owns the only handle whose
    /// timeout matters, so the reader is who disarms it.
    watched: Arc<AtomicBool>,
}

impl Severable {
    /// Arm the poll and take ownership. Fails only if the socket cannot carry
    /// a timeout, in which case the caller has no cuttable connection.
    fn new(sock: TcpStream) -> std::io::Result<Self> {
        sock.set_read_timeout(Some(READ_POLL))?;
        Ok(Self {
            sock,
            severed: Arc::new(AtomicBool::new(false)),
            watched: Arc::new(AtomicBool::new(true)),
        })
    }

    /// A handle that can sever this connection from another thread.
    ///
    /// A separate socket handle because the one the reader owns is parked in
    /// `read` at exactly the moment this is used.
    fn scissors(&self) -> std::io::Result<Scissors> {
        Ok(Scissors {
            sock: self.sock.try_clone()?,
            severed: Arc::clone(&self.severed),
            watched: Arc::clone(&self.watched),
        })
    }

    /// Another handle on the same connection — the write half — which shares
    /// the severed flag so that cutting ends both.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            sock: self.sock.try_clone()?,
            severed: Arc::clone(&self.severed),
            watched: Arc::clone(&self.watched),
        })
    }

    /// Whether a poll is armed, and how long.
    ///
    /// Only the tests need this, and they need it because "the cut can still
    /// land" is otherwise not observable from outside — which is precisely how
    /// the bug in #99 survived being reviewed.
    #[cfg(test)]
    fn read_timeout(&self) -> std::io::Result<Option<Duration>> {
        self.sock.read_timeout()
    }
}

impl std::io::Read for Severable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            return match self.sock.read(buf) {
                // A poll elapsing. Windows reports it as `TimedOut` and unix as
                // `WouldBlock`; both spellings are the same event and matching
                // only one of them is a silent half-fix.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    if self.severed.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    // Severed first, then watched: a cut sets `severed` and
                    // could in principle be seen in the same wake-up as a
                    // stand-down, and ending the connection is the answer that
                    // must win.
                    if !self.watched.load(Ordering::Acquire) {
                        // The handshake is over and nothing can cut this
                        // connection now, so stop waking to ask. Best-effort:
                        // if the option will not come off, the cost is a
                        // syscall a second, not a broken connection.
                        let _ = self.sock.set_read_timeout(None);
                    }
                    continue;
                }
                // Any failure on a connection that has been cut is the cut.
                //
                // **A cut is not a fault**, and on Windows it does not arrive
                // looking like one thing: a read issued *after* the
                // `shutdown` — the ordering a watchdog cannot control, since
                // it fires on its own thread — comes back `ConnectionAborted`
                // (os error 10053) rather than as zero bytes, and only
                // sometimes, which is the worst of both. Reporting that up is
                // a spurious "connection ended: error" in the log for what is
                // simply the watchdog doing its job, and a test that has to
                // accept either answer. Once `severed` is set there is exactly
                // one true story about this connection and this is it.
                Err(_) if self.severed.load(Ordering::Acquire) => Ok(0),
                other => other,
            };
        }
    }
}

impl std::io::Write for Severable {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sock.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sock.flush()
    }
}

/// The cutting end of a [`Severable`], held by the watchdog.
pub(crate) struct Scissors {
    sock: TcpStream,
    severed: Arc<AtomicBool>,
    watched: Arc<AtomicBool>,
}

impl Cut for Scissors {
    fn cut(&self) {
        // Flag first, then shutdown: the reader checks the flag when its poll
        // elapses, so setting it afterwards would let one more poll go by.
        self.severed.store(true, Ordering::Release);
        // The `shutdown` still earns its place. On POSIX it unparks the reader
        // immediately, so the cut costs nothing there and the poll is only ever
        // the Windows fallback.
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }

    /// The handshake completed, so stop polling.
    ///
    /// Safe precisely because of who calls it: [`WatchdogHandle::completed`]
    /// stands down only when it wins the same `armed` swap the watchdog thread
    /// must win to cut. Exactly one of them wins, so a stood-down connection is
    /// one no cut can still be coming for.
    ///
    /// The reader takes its own timeout off when it next surfaces and sees
    /// this, which is why it is a flag rather than a `set_read_timeout(None)`
    /// from here — see [`Severable::watched`]. So the last poll of a
    /// connection's life still elapses; one syscall, once, per connection.
    fn stand_down(&self) {
        self.watched.store(false, Ordering::Release);
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
    /// Kept beyond the watching thread so a completed handshake can call
    /// [`Cut::stand_down`] — the poll that makes the cut work costs a syscall a
    /// second, and nothing is watching once the handshake is over.
    cut: Option<Arc<dyn Cut>>,
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
    fn start(stream: &Severable, timeout: Duration) -> Self {
        match stream.scissors() {
            Ok(scissors) => Self::start_with(Arc::new(scissors), timeout),
            // As before: with no second handle there is nothing to cut, so the
            // connection runs unwatched rather than being refused.
            Err(_) => Self::unwatched(timeout),
        }
    }

    fn unwatched(timeout: Duration) -> Self {
        Self {
            armed: Arc::new(AtomicBool::new(true)),
            fired: Arc::new(AtomicBool::new(false)),
            cut: None,
            // Milliseconds from now, so the waiting thread can be told to wait
            // longer without being restarted.
            deadline: Arc::new(AtomicU64::new(timeout.as_millis() as u64)),
        }
    }

    fn start_with(cut: Arc<dyn Cut>, timeout: Duration) -> Self {
        let mut this = Self::unwatched(timeout);
        this.cut = Some(Arc::clone(&cut));
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
            cut: self.cut.clone(),
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
    cut: Option<Arc<dyn Cut>>,
}

impl WatchdogHandle {
    /// The handshake completed; stop watching.
    pub fn completed(&self) {
        // `swap`, not `store`, and the difference is load-bearing: the watchdog
        // thread cuts only if it wins this same swap, so winning it here is
        // what proves no cut can still be on its way — and only then is it safe
        // to stop the poll that lets a cut land at all (see [`READ_POLL`]).
        // Losing the race means the connection has already been cut, and
        // standing its poll down would park the reader for ever.
        if self.armed.swap(false, Ordering::AcqRel) {
            if let Some(cut) = &self.cut {
                cut.stand_down();
            }
        }
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
    use std::io::Write;

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
            cut: None,
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
            cut: None,
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
            cut: None,
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
            cut: None,
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

    /// Long enough that a deadlock is not mistaken for a slow machine, short
    /// enough that a hung test is reported rather than waited out.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// How long a reader is given to reach its blocking `read` before the test
    /// starts relying on it being parked there.
    ///
    /// The race is one-sided: too short and a broken cut might pass, never the
    /// reverse. There is nothing to observe instead — "parked in a syscall" is
    /// not a state a thread can publish.
    const SETTLE: Duration = Duration::from_millis(150);

    /// A connected loopback pair, server end already armed for cutting. The
    /// client end is returned so the caller can hold it: a dropped client
    /// closes the connection and would unpark the reader for a reason these
    /// tests are not about.
    fn connected_pair() -> (Severable, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("the bound address");
        let client = TcpStream::connect(addr).expect("connect to it");
        let (server, _) = listener.accept().expect("accept the connection");
        (Severable::new(server).expect("arm the poll"), client)
    }

    /// A reader on its own thread, reporting one `read` back over a channel.
    /// Returns once the thread has had time to reach the syscall.
    fn park(mut stream: Severable) -> std::sync::mpsc::Receiver<std::io::Result<usize>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let _ = tx.send(std::io::Read::read(&mut stream, &mut buf));
        });
        std::thread::sleep(SETTLE);
        rx
    }

    /// The question #99 was opened to settle, on the platform it was opened
    /// about.
    ///
    /// Both watchdog tests above inject a fake `Cut`, so they prove the timer
    /// fires and nothing about whether `shutdown` brings a thread back out of
    /// `read`. That gap mattered because #94 hit exactly this in the TLS
    /// reader: **`shutdown` does not reliably unpark a blocked `recv` on
    /// Winsock**, and arming `SO_RCVTIMEO` at cut time does not help either,
    /// since a socket timeout applies to calls made *after* it is set and never
    /// to one already in flight.
    ///
    /// The measured answer, on this box: a bare `TcpStream` behaves the same
    /// way. This test failed against the `shutdown`-only `Cut for TcpStream`
    /// that `main` carried — the reader was still parked five seconds later —
    /// and passes against [`Severable`], which is `TlsDuplex`'s fix in this
    /// module's own shape.
    ///
    /// What was at stake, and what this test is left behind to hold: `serve`'s
    /// reader parks for ever, its thread lives for ever, and the [`Countdown`]
    /// it holds keeps one of [`MAX_UNAUTHENTICATED`] for ever. That budget is
    /// shared across every transport (#75) and is 32, so 32 connections that
    /// open and then say nothing stop the daemon accepting any client at all —
    /// silently, with every call reporting success.
    #[test]
    fn cutting_a_real_socket_unparks_a_reader_already_parked_in_read() {
        let (server, _client) = connected_pair();
        let scissors = server.scissors().expect("a second handle on the accepted socket");
        let parked = park(server);

        scissors.cut();

        let woke = parked.recv_timeout(PATIENCE).expect(
            "the cut left a parked reader parked, which is the watchdog doing nothing: the \
             connection is never dropped and its mid-handshake slot is never released",
        );
        assert_eq!(
            woke.expect("a cut is not a fault: `serve` unwinds through 'the client went away'"),
            0,
            "a cut reads as end of stream"
        );
    }

    /// The same, end to end through the watchdog's own thread and a real
    /// socket — the two halves every other watchdog test exercises only apart.
    #[test]
    fn the_watchdog_cuts_a_real_connection_whose_reader_is_parked() {
        let (server, _client) = connected_pair();
        let reader = server.try_clone().expect("a second handle on the accepted socket");
        // Parked *before* the watchdog is started, so the cut is guaranteed to
        // land on a reader already in the syscall rather than winning a race.
        // A watchdog that only cuts when it wins a race is not a watchdog.
        let parked = park(reader);

        let w = Watchdog::start(&server, Duration::from_millis(50));

        let woke = parked
            .recv_timeout(PATIENCE)
            .expect("the handshake deadline passed and the connection was left up");
        assert_eq!(woke.expect("a cut is not a fault"), 0, "a cut reads as end of stream");
        assert!(!w.authenticated(), "a connection that was cut is not an authenticated one");
    }

    /// The ordering the sibling test cannot control, made deterministic.
    ///
    /// The watchdog fires on its own thread and cannot know whether the
    /// connection's reader has reached `read` yet, so both orders are real, and
    /// a watchdog that only works in one of them is not a watchdog.
    #[test]
    fn a_cut_that_lands_before_the_read_still_ends_it() {
        let (server, _client) = connected_pair();
        let scissors = server.scissors().expect("a second handle on the accepted socket");

        scissors.cut();

        let parked = park(server);
        let woke = parked.recv_timeout(PATIENCE).expect(
            "a read issued after the cut hung instead of ending, so a watchdog that fires \
             before its connection's reader gets going pins the thread it meant to free",
        );
        assert_eq!(woke.expect("a cut is not a fault"), 0, "a cut reads as end of stream");
    }

    /// The regression the whole design is arranged around, and the reason the
    /// original `Cut` chose `shutdown` over a read timeout in the first place.
    ///
    /// `serve`'s reader treats *any* `Err` as fatal, so if an elapsed poll
    /// reached it, every connection would die the moment its peer went quiet —
    /// which for a device waiting on a human to approve it is the entire
    /// 120-second pairing window, and for an established session is almost
    /// always. [`Severable`] swallows the poll; only a cut ends the read.
    #[test]
    fn a_quiet_connection_outlives_several_polls_untouched() {
        let (server, mut client) = connected_pair();
        let parked = park(server);

        // Several `READ_POLL`s would have elapsed by now if the poll were
        // visible above the wrapper. Sub-second, because the test suite pays
        // this wall-clock: what is being proved is that a poll elapsing is not
        // *reported*, and one elapsed poll proves that as well as ten.
        std::thread::sleep(READ_POLL + READ_POLL / 2);
        assert!(
            matches!(parked.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "a healthy but quiet connection was ended by its own watchdog poll"
        );

        // And it is still a working connection, not merely an unreported one.
        client.write_all(b"hi").expect("the client's write half");
        client.flush().expect("flush");
        assert_eq!(
            parked
                .recv_timeout(PATIENCE)
                .expect("the reader never woke for real data")
                .expect("reading real data failed"),
            2,
            "the reader that swallowed a poll could no longer see real bytes"
        );
    }

    /// The poll is bounded to the handshake, not paid for the life of a
    /// session.
    ///
    /// A daemon that wakes on a timer to find nothing is a laptop that does not
    /// sleep, and this project keeps a 0%-idle property deliberately. Once the
    /// handshake completes the watchdog can never fire, so the timeout comes
    /// straight back off.
    #[test]
    fn completing_the_handshake_takes_the_poll_back_off() {
        let (server, mut client) = connected_pair();
        assert_eq!(
            server.read_timeout().expect("read the option back"),
            Some(READ_POLL),
            "the poll must be armed before the reader can park, or the cut cannot land"
        );
        let w = Watchdog::start(&server, Duration::from_secs(30));

        // Observed through the reader's own handle, given back when its read
        // returns, because that is the only handle whose timeout decides
        // anything — see `Severable::watched`. Asserting it on any other handle
        // is what made the first version of this pass while the reader polled
        // on regardless.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = server;
            let mut buf = [0u8; 8];
            let got = std::io::Read::read(&mut s, &mut buf);
            let _ = tx.send((got, s));
        });
        std::thread::sleep(SETTLE);

        w.handle().completed();
        // Long enough for the parked reader to surface once and disarm itself.
        std::thread::sleep(READ_POLL + READ_POLL / 2);

        client.write_all(b"x").expect("the client's write half");
        client.flush().expect("flush");
        let (got, reader) = rx.recv_timeout(PATIENCE).expect("the reader never returned");
        assert_eq!(got.expect("the read failed"), 1, "the disarmed reader stopped seeing data");
        assert_eq!(
            reader.read_timeout().expect("read the option back"),
            None,
            "an established session kept waking once a second for nothing, which is the \
             0%-idle property this daemon is built to keep"
        );
    }

    /// The harm, in one test: silent connections must give their slots back.
    ///
    /// This is what issue #99 was actually about, and it is asserted here
    /// rather than in `tests/lan.rs` because it cannot run there — over
    /// loopback every connection shares one `PeerKey`, so filling the cap
    /// settles 32 failed handshakes and the per-peer rate limiter refuses the
    /// test's own client before the cap is ever the reason for anything.
    ///
    /// The thread below is `accept_hardened`'s, reduced to the four things that
    /// matter: take a slot, arm the watchdog, park in `read`, and release the
    /// slot when the read returns. Against the `shutdown`-only cut on `main`
    /// the reads never returned, so no slot ever came back and the gate stayed
    /// full for ever — 32 connections that open and say nothing, and the daemon
    /// accepts no client from anywhere again.
    #[test]
    fn silent_connections_give_their_mid_handshake_slots_back() {
        let gate = Gate::new();
        let mut clients = Vec::new();

        for _ in 0..MAX_UNAUTHENTICATED {
            let (server, client) = connected_pair();
            let mut slot = gate.admit(PeerKey::Loopback).expect("still under the cap");
            let watchdog = Watchdog::start(&server, Duration::from_millis(50));
            std::thread::spawn(move || {
                let mut buf = [0u8; 256];
                let mut server = server;
                let _ = std::io::Read::read(&mut server, &mut buf);
                // Exactly where `accept_hardened` releases it: when serving
                // this connection returns, however it returned.
                slot.release();
                drop(watchdog);
            });
            // Held so the connection stays open: what is being tested is the
            // watchdog, not a peer hanging up.
            clients.push(client);
        }
        assert!(
            gate.admit(PeerKey::Loopback).is_err(),
            "the cap must actually be full, or this test proves nothing"
        );

        // Polled rather than slept: the answer arrives one `READ_POLL` after
        // the deadline on Windows and immediately on POSIX, and a flat sleep
        // long enough for the slower of those is wall-clock every CI run pays.
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if gate.admit(PeerKey::Loopback).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "every mid-handshake slot was still held after the watchdog cut all {MAX_UNAUTHENTICATED} \
             connections: the readers never woke, so their threads and their slots leak and the \
             daemon is deaf to every new client from here on"
        );
    }

    /// The race that makes standing the poll down safe at all.
    ///
    /// `completed` and the watchdog thread both settle it with a swap on
    /// `armed`, and exactly one can win. If the watchdog won — the connection
    /// is already cut — a late `completed` must **not** take the timeout off,
    /// or the reader it was about to free parks for ever and the bug this whole
    /// change removes comes back through the one door left open.
    #[test]
    fn a_late_completion_never_stands_down_a_connection_already_cut() {
        let (server, _client) = connected_pair();
        let w = Watchdog::start(&server, Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!w.authenticated(), "the watchdog was meant to have fired by now");

        w.handle().completed();

        assert!(
            server.watched.load(Ordering::Acquire),
            "a completion that lost the race stood the poll down anyway, which re-parks \
             the very reader the cut was freeing"
        );
    }
}
