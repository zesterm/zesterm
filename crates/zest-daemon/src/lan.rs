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
//! - **A per-address failed-auth limit.** This is what makes a six-digit
//!   pairing code sound: one online guess per connection at 1-in-10⁶ is only an
//!   argument if connections cannot be made without limit.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::auth::{Auth, Authenticator};
use crate::server::{serve, Registry};
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
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections may be mid-handshake at once.
const MAX_UNAUTHENTICATED: usize = 32;

/// Failed handshakes from one address before it is made to wait.
const MAX_FAILURES: u32 = 5;

/// How long a rate-limited address waits.
const COOLDOWN: Duration = Duration::from_secs(60);

/// A bound port, not yet serving.
pub struct LanListener {
    listener: TcpListener,
    addr: SocketAddr,
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
        Ok(Self { listener, addr })
    }

    /// What was actually bound. **Advertise this, never the requested port.**
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.addr
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
    ) -> Result<(), DaemonError> {
        tracing::info!(addr = %self.addr, "serving the LAN");
        let limiter = Arc::new(RateLimiter::default());
        let unauthenticated = Arc::new(AtomicUsize::new(0));

        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let peer = stream
                .peer_addr()
                .map_or_else(|_| "unknown".to_string(), |a| a.to_string());

            if let Some(wait) = limiter.blocked(&peer) {
                tracing::warn!(%peer, ?wait, "refusing: too many failed handshakes");
                drop(stream);
                continue;
            }

            // Accept-and-close rather than leaving them queued: a peer told no
            // immediately can back off, where one left hanging cannot.
            if unauthenticated.load(Ordering::Acquire) >= MAX_UNAUTHENTICATED {
                tracing::warn!(%peer, "refusing: too many connections are mid-handshake");
                drop(stream);
                continue;
            }

            let Ok(write_half) = stream.try_clone() else { continue };
            let config = config.clone();
            let registry = Arc::clone(&registry);
            let auth = Arc::clone(&auth);
            let limiter = Arc::clone(&limiter);
            let unauthenticated = Arc::clone(&unauthenticated);

            unauthenticated.fetch_add(1, Ordering::AcqRel);
            std::thread::spawn(move || {
                let guard = Countdown(unauthenticated);
                let watchdog = Watchdog::start(&stream);

                // `Auth::Proof`: the trust store decides, and this connection
                // may not approve other devices.
                let result = serve(
                    stream,
                    write_half,
                    config,
                    registry,
                    Auth::Proof(auth),
                    peer.clone(),
                );
                let authenticated = watchdog.finish();
                if authenticated {
                    limiter.succeeded(&peer);
                } else {
                    limiter.failed(&peer);
                }
                drop(guard);
                if let Err(e) = result {
                    tracing::warn!(%peer, error = %e, "connection ended");
                }
            });
        }
        Ok(())
    }
}

/// Decrements the in-flight count however the thread ends.
struct Countdown(Arc<AtomicUsize>);

impl Drop for Countdown {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Cuts a connection that never finishes its handshake.
///
/// `shutdown(Both)` rather than a read timeout, and that is the whole trick:
/// `serve`'s reader treats *any* `Err` as fatal, so setting a timeout on the
/// socket would kill healthy connections the moment a session went quiet —
/// which is most of the time. Shutting the socket down unblocks the read once,
/// and `serve` unwinds through its ordinary "the client went away" path with no
/// change to its loop.
struct Watchdog {
    done: Arc<Mutex<bool>>,
}

impl Watchdog {
    fn start(stream: &TcpStream) -> Self {
        let done = Arc::new(Mutex::new(false));
        if let Ok(cut) = stream.try_clone() {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                std::thread::sleep(HANDSHAKE_TIMEOUT);
                if !*done.lock().expect("watchdog lock") {
                    tracing::warn!("a connection never finished its handshake; closing");
                    let _ = cut.shutdown(std::net::Shutdown::Both);
                }
            });
        }
        Self { done }
    }

    /// Stop the watchdog. Returns whether the connection lived past it.
    fn finish(self) -> bool {
        let mut done = self.done.lock().expect("watchdog lock");
        let already = *done;
        *done = true;
        !already
    }
}

/// Failed handshakes per address.
///
/// The reason a six-digit code is defensible: an attacker gets one guess per
/// connection, and connections are not free.
#[derive(Default)]
struct RateLimiter {
    seen: Mutex<HashMap<String, (u32, Instant)>>,
}

impl RateLimiter {
    /// How long this address must wait, if at all.
    fn blocked(&self, peer: &str) -> Option<Duration> {
        let key = address_of(peer);
        let seen = self.seen.lock().expect("rate limiter lock");
        let (failures, last) = seen.get(&key)?;
        if *failures < MAX_FAILURES {
            return None;
        }
        let elapsed = last.elapsed();
        (elapsed < COOLDOWN).then(|| COOLDOWN - elapsed)
    }

    fn failed(&self, peer: &str) {
        let key = address_of(peer);
        let mut seen = self.seen.lock().expect("rate limiter lock");
        let entry = seen.entry(key).or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();
    }

    fn succeeded(&self, peer: &str) {
        let key = address_of(peer);
        self.seen.lock().expect("rate limiter lock").remove(&key);
    }
}

/// The address without the port.
///
/// Keyed on the address alone: a client reconnecting comes from a *new*
/// ephemeral port every time, so counting per `addr:port` would count to one
/// forever and never limit anything.
fn address_of(peer: &str) -> String {
    peer.rsplit_once(':').map_or_else(|| peer.to_string(), |(addr, _)| addr.to_string())
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
    fn an_address_with_no_port_is_still_usable_as_a_key() {
        assert_eq!(address_of("192.168.1.42:51314"), "192.168.1.42");
        assert_eq!(address_of("unknown"), "unknown");
    }
}
