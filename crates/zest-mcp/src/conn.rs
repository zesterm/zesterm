//! One authenticated connection to one daemon, and the threads that keep it.
//!
//! [`DaemonClient`] does the handshake — never a tenth hand-rolled one, for the
//! reason its own module doc gives: a peer that gets the seal switch wrong
//! authenticates perfectly and then cannot be read. Once welcomed, the
//! connection is split with `into_halves` and driven by two threads, because a
//! client that both waits for a keyframe and forwards keystrokes cannot do
//! either from one blocking loop.
//!
//! # The writer thread owns the sealer outright
//!
//! Not an `Arc<Mutex<Sealer>>` beside the sink. Sealing advances a counter, so
//! two threads that seal in one order and write in the other produce frames the
//! host cannot open — and the fix for that is a documented lock ordering that
//! every future call site has to honour. `zest-app/src/remote.rs` solved it by
//! giving one thread the sealer and feeding it a channel, which makes the
//! hazard *structurally impossible* rather than commented, and that is the
//! shape copied here.
//!
//! # The reader drains what the handshake already read
//!
//! `DaemonClient::recv` reads up to 64 KiB and returns the first whole frame;
//! the daemon batches its replies and flushes once, so a single read can take
//! the keyframe and everything queued behind it. `Halves::frames` carries those
//! across the handoff — and they must be drained **before** the first blocking
//! read, because a command that prints and exits sends nothing more and the
//! carried frames would wait for a read that never comes. Both halves of #54;
//! carrying them without draining them is not half a fix.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use zest_daemon::client::{DaemonClient, Watch};
use zest_daemon::DaemonError;
use zest_mesh::identity::ClientIdentity;
use zest_mesh::secure::Sealer;
use zest_proto::{
    frame, ClientMessage, FrameReader, HostId, HostMessage, HostOffer, SessionAddr, SessionInfo,
};

use crate::session::Replica;

/// What the writer thread does next.
enum Outbound {
    Msg(Box<ClientMessage>),
    Shutdown,
}

/// Everything the reader thread learns, for the tools to read.
#[derive(Default)]
pub struct Shared {
    /// The last session listing this connection was told about.
    pub sessions: Vec<SessionInfo>,
    /// Bumped on every `Sessions` message, so a caller that asked for a fresh
    /// listing can tell the answer to its own request from the listing that was
    /// already there. A count rather than a flag: two `ListSessions` in flight
    /// would race to clear one flag, and the loser would return the listing the
    /// winner asked for.
    pub sessions_gen: u64,
    /// What this machine says it is and can launch. Sticky: `None` on a
    /// `Sessions` push means "nothing new to say", never "it has none", so it
    /// is only ever replaced by a `Some`.
    pub offer: Option<HostOffer>,
    /// Sessions this connection is attached to.
    pub replicas: Vec<Replica>,
    /// Set when the reader stops, so a tool reports a dead link rather than
    /// waiting out a deadline against a connection that has gone.
    pub closed: bool,
    /// The last error the reader saw, if any.
    pub error: Option<String>,
    /// The session the most recent `CreateSession` produced.
    ///
    /// Read from `Sessions.created` rather than by taking the newest of the
    /// listing: two clients creating on one host concurrently would each adopt
    /// the other's shell, and the field exists precisely so nobody has to guess.
    pub created: Option<SessionAddr>,
}

impl Shared {
    pub fn replica(&self, addr: SessionAddr) -> Option<&Replica> {
        self.replicas.iter().find(|r| r.addr == addr)
    }

    fn replica_mut(&mut self, addr: SessionAddr) -> Option<&mut Replica> {
        self.replicas.iter_mut().find(|r| r.addr == addr)
    }
}

/// How long a request waits for the answer to show up in [`Shared`].
///
/// Generous: it is only ever paid by a call that was going to fail, because
/// every wait is on a condition that is signalled the moment it holds. A
/// deadline this long firing means the daemon stopped answering, and it reports
/// as that rather than as the tool's own subject.
const REPLY_DEADLINE: Duration = Duration::from_secs(20);

/// State plus the signal that it changed.
///
/// A condvar rather than polling, so a tool waiting on a keyframe costs nothing
/// while it waits -- the same reason the daemon wakes subscribers instead of
/// running a timer, and the same 0%-idle property.
type State = Arc<(Mutex<Shared>, Condvar)>;

/// A live connection to one daemon.
pub struct Conn {
    host: HostId,
    label: String,
    tx: Sender<Outbound>,
    state: State,
    /// Told to the reader so it stops without waiting for the socket to close.
    stopping: Arc<AtomicBool>,
}

impl Conn {
    /// Handshake, then split into the two threads that keep it.
    ///
    /// `expect_host` pins the identity when the caller learned the address from
    /// somewhere that could have lied. `None` is right for a socket handed over
    /// by hand, which is the only case on the local path.
    pub fn open(
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        identity: &Arc<ClientIdentity>,
        label: &str,
        expect_host: Option<HostId>,
    ) -> Result<Self, DaemonError> {
        // `hosts: true` so the first listing carries this machine's facts and
        // its profiles. `sessions: false` on purpose: a push arriving while a
        // request/reply is in flight is *discarded* by `DaemonClient`, and
        // nothing here needs to hear about a session list it can ask for.
        let mut client = DaemonClient::connect_watching(
            read,
            write,
            identity,
            label,
            expect_host,
            Watch { sessions: false, pairings: false, hosts: true },
        )?;

        let host = client.host();
        let host_label = client.host_label().to_string();

        // Before the handoff, while request/reply is still safe: this is the
        // one call that carries the offer, and a plain `list()` would drop it
        // and then wait for a generation bump only a config edit on the far
        // machine can produce.
        let (sessions, offer) = client.list_with_offer()?;

        let carried = client.pending();
        let halves = client.into_halves();
        debug_assert_eq!(
            halves.frames.pending(),
            carried,
            "into_halves must carry the frames the handshake already read (#54)"
        );

        let state: State = Arc::new((
            Mutex::new(Shared { sessions, offer, ..Shared::default() }),
            Condvar::new(),
        ));
        let stopping = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();

        let (sealer, opener) = match halves.channel {
            Some(ch) => {
                let (s, o) = ch.split();
                (Some(s), Some(o))
            }
            None => (None, None),
        };

        spawn_writer(halves.write, sealer, rx);
        spawn_reader(
            halves.read,
            halves.frames,
            opener,
            Arc::clone(&state),
            Arc::clone(&stopping),
            tx.clone(),
        );

        Ok(Self { host, label: host_label, tx, state, stopping })
    }

    #[must_use]
    pub fn host(&self) -> HostId {
        self.host
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Read the shared state under its lock.
    pub fn with<T>(&self, f: impl FnOnce(&Shared) -> T) -> T {
        f(&self.state.0.lock().expect("the state lock is never held across a panic"))
    }

    /// Wait until `f` answers, or the deadline passes.
    ///
    /// Answers `Err` the moment the reader reports the link closed, rather than
    /// waiting out the deadline against a connection that has gone -- a tool
    /// that hangs for twenty seconds on a dead socket is indistinguishable from
    /// a slow one, and only one of those is worth retrying.
    pub fn wait_for<T>(&self, f: impl FnMut(&Shared) -> Option<T>) -> Result<T, ConnError> {
        self.wait_until(Instant::now() + REPLY_DEADLINE, f)
    }

    /// Wait until `f` answers, or `give_up` passes.
    ///
    /// [`wait_for`](Self::wait_for) is this with the request deadline, which is
    /// the right bound for anything the daemon answers by return. A *command*
    /// is not that: `cargo build` outlives twenty seconds routinely, and a
    /// deadline the caller cannot raise would make `run` unable to report the
    /// builds it exists to run. The caller supplies the bound; this only
    /// enforces it.
    pub fn wait_until<T>(
        &self,
        give_up: Instant,
        mut f: impl FnMut(&Shared) -> Option<T>,
    ) -> Result<T, ConnError> {
        let (lock, cv) = &*self.state;
        let mut guard = lock.lock().expect("state");
        loop {
            if let Some(v) = f(&guard) {
                return Ok(v);
            }
            if guard.closed {
                return Err(ConnError::Closed(guard.error.clone()));
            }
            let left = give_up.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ConnError::TimedOut);
            }
            let (g, _) = cv.wait_timeout(guard, left).expect("state");
            guard = g;
        }
    }

    /// Create a session and wait for the host to name it.
    pub fn create_session(
        &self,
        command: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> Result<SessionAddr, ConnError> {
        {
            let (lock, _) = &*self.state;
            lock.lock().expect("state").created = None;
        }
        self.send(ClientMessage::CreateSession {
            command: command.to_string(),
            cwd: cwd.to_string(),
            cols,
            rows,
        });
        self.wait_for(|s| s.created)
    }

    /// Ask the host for a listing, and wait for it to answer.
    ///
    /// The listing is a **question**, not a cache read. `Shared::sessions` is
    /// only written when a `Sessions` message arrives, and this connection sets
    /// `Watch { sessions: false }` -- so without asking, the newest listing is
    /// whatever our own last `CreateSession`/`CloseSession` happened to return.
    /// Everything a session acquires after it is spawned -- its title, its OSC 7
    /// cwd, whether it went to the alternate screen -- was therefore reported at
    /// the value it held a millisecond after the spawn, which is empty for all
    /// three, until the agent happened to create something else. An agent that
    /// reads `alt_screen: false` off a session running vim goes to `blocks`,
    /// which is empty on the alternate screen by design, and concludes the
    /// session is idle. (#360)
    ///
    /// Asking, rather than subscribing: a push arriving while a request/reply is
    /// in flight is discarded by `DaemonClient`, which is why the watch is off in
    /// the first place, and a subscription is in any case only as fresh as the
    /// last push where this is as fresh as the call.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, ConnError> {
        let seen = {
            let (lock, _) = &*self.state;
            let s = lock.lock().expect("state");
            s.sessions_gen
        };
        self.send(ClientMessage::ListSessions);
        self.wait_for(|s| (s.sessions_gen != seen).then(|| s.sessions.clone()))
    }

    /// Attach, and wait for the keyframe that answers it.
    ///
    /// `observe` is the whole reason this crate can read a session somebody is
    /// looking at: it takes no part in the size arbitration, so watching cannot
    /// shrink or pin their window (#274). `cols`/`rows` are still sent -- an
    /// older daemon counts them as an ordinary vote -- so pass what the session
    /// already reports rather than something invented.
    pub fn attach(
        &self,
        session: SessionAddr,
        cols: u16,
        rows: u16,
        observe: bool,
    ) -> Result<(), ConnError> {
        self.send(ClientMessage::Attach { session, cols, rows, observe });
        self.wait_for(|s| s.replica(session).map(|_| ()))
    }

    /// Stop watching. The session keeps running -- this is not `CloseSession`.
    pub fn detach(&self, session: SessionAddr) {
        self.send(ClientMessage::Detach { session });
        let (lock, cv) = &*self.state;
        let mut s = lock.lock().expect("state");
        s.replicas.retain(|r| r.addr != session);
        cv.notify_all();
    }

    /// Queue one message for the writer thread.
    ///
    /// Fire and forget: there is nowhere to report a failure from a writer
    /// thread, and a link that has gone is reported by [`Shared::closed`]
    /// rather than by every send growing a `Result` nobody can act on.
    pub fn send(&self, msg: ClientMessage) {
        let _ = self.tx.send(Outbound::Msg(Box::new(msg)));
    }
}

/// Why a request did not get its answer.
#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("the connection closed{}", .0.as_ref().map(|e| format!(": {e}")).unwrap_or_default())]
    Closed(Option<String>),
    /// The deadline passed. Whose deadline depends on the caller: a request
    /// uses [`REPLY_DEADLINE`], while `run` supplies its own, so the message
    /// cannot name a duration without being wrong for one of them.
    #[error("the deadline passed before the daemon answered (requests allow {REPLY_DEADLINE:?})")]
    TimedOut,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.tx.send(Outbound::Shutdown);
    }
}

fn spawn_writer(
    mut sink: Box<dyn Write + Send>,
    mut sealer: Option<Sealer>,
    rx: Receiver<Outbound>,
) {
    std::thread::spawn(move || {
        while let Ok(item) = rx.recv() {
            let msg = match item {
                Outbound::Msg(m) => m,
                Outbound::Shutdown => break,
            };
            let Some(bytes) = seal(&mut sealer, &msg) else { continue };
            if sink.write_all(&bytes).is_err() || sink.flush().is_err() {
                // The link is gone. Keep draining so senders do not block on a
                // bounded channel; the reader is what reports the closure.
                break;
            }
        }
    });
}

/// Serialize one message, sealed if this link has a channel.
///
/// `None` rather than an error: there is nowhere to report from a writer
/// thread. A sealing failure is different in kind -- the counter has run past
/// what the epoch can carry -- so it is logged rather than swallowed.
fn seal(sealer: &mut Option<Sealer>, msg: &ClientMessage) -> Option<Vec<u8>> {
    let body = match frame::encode_body(msg) {
        Ok(b) => b,
        Err(e) => {
            // Logged rather than swallowed. There is nowhere to *report* it
            // from a writer thread, but a message that vanishes with no trace
            // is a tool call that appears to have been sent and never was --
            // and the symptom of that is a wait that times out somewhere else
            // entirely.
            tracing::error!(error = %e, "could not encode a message; dropping it");
            return None;
        }
    };
    let body = match sealer.as_mut() {
        Some(s) => match s.seal(&body) {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::error!(error = %e, "could not seal a message");
                return None;
            }
        },
        None => body,
    };
    match frame::frame_bytes(&body) {
        Ok(framed) => Some(framed),
        Err(e) => {
            tracing::error!(error = %e, "could not frame a message; dropping it");
            None
        }
    }
}

fn spawn_reader(
    mut source: Box<dyn Read + Send>,
    mut frames: FrameReader,
    mut opener: Option<zest_mesh::secure::Opener>,
    state: State,
    stopping: Arc<AtomicBool>,
    // So a refused delta can ask for a keyframe. Without it the reader can
    // *detect* divergence and do nothing about it, which is a replica that
    // silently stops tracking the session it is describing.
    tx: Sender<Outbound>,
) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        // Whatever the handshake already lifted off the socket, before any
        // blocking read. See the module doc: a command that prints and exits
        // sends nothing more, so reading first loses exactly the output that
        // carrying the buffer was meant to save (#54).
        let mut drain_carried = frames.pending() > 0;

        loop {
            if stopping.load(Ordering::Acquire) {
                break;
            }
            if drain_carried {
                drain_carried = false;
            } else {
                match source.read(&mut buf) {
                    Ok(0) => break,
                    // A signal is not a dropped link.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        fail(&state, e.to_string());
                        break;
                    }
                    Ok(n) => frames.feed(&buf[..n]),
                }
            }

            loop {
                let body = match frames.next_frame() {
                    Ok(Some(b)) => b,
                    Ok(None) => break,
                    Err(e) => {
                        // Framing is lost, so the stream position is no longer
                        // trustworthy and nothing after this can be believed.
                        fail(&state, format!("framing lost: {e}"));
                        stopping.store(true, Ordering::Release);
                        break;
                    }
                };
                let body = match opener.as_mut() {
                    Some(o) => match o.open(&body) {
                        Ok(plain) => plain,
                        Err(e) => {
                            // The nonce is an implicit per-direction counter, so
                            // one frame that will not open means every later one
                            // is opened under the wrong key. There is no
                            // recovering the position; end the link.
                            fail(&state, format!("a sealed frame did not open: {e}"));
                            stopping.store(true, Ordering::Release);
                            break;
                        }
                    },
                    None => body,
                };
                match frame::decode::<HostMessage>(&body) {
                    Ok(m) => on_message(&state, &tx, m),
                    Err(e) => {
                        fail(&state, format!("undecodable: {e}"));
                        stopping.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        }

        let (lock, cv) = &*state;
        lock.lock().expect("state").closed = true;
        // Wake every waiter: a tool blocked on a keyframe must learn that the
        // link died rather than sit out its deadline.
        cv.notify_all();
    });
}

/// Record a reader failure and wake anyone waiting on it.
fn fail(state: &State, message: String) {
    let (lock, cv) = &**state;
    lock.lock().expect("state").error = Some(message);
    cv.notify_all();
}

fn on_message(state: &State, tx: &Sender<Outbound>, msg: HostMessage) {
    let (lock, cv) = &**state;
    let mut s = lock.lock().expect("state");
    match msg {
        HostMessage::Sessions { sessions, created, offer } => {
            // Look the new session up in the listing rather than pairing the id
            // with a host from somewhere else: `SessionInfo.addr` is already the
            // whole address, and reconstructing one is a chance to get it wrong.
            if let Some(id) = created {
                s.created = sessions.iter().find(|si| si.addr.session == id).map(|si| si.addr);
            }
            s.sessions = sessions;
            s.sessions_gen = s.sessions_gen.wrapping_add(1);
            // Sticky. `None` is "nothing new to say" -- which is also what a
            // daemon predating the field sends -- so clearing on one would
            // blank this machine's facts every time somebody opened a shell.
            if offer.is_some() {
                s.offer = offer;
            }
        }
        HostMessage::Keyframe {
            session,
            seq,
            cols,
            rows,
            rows_data,
            attrs,
            cursor,
            modes,
            blocks,
            blocks_from,
            title,
            history_clears,
        } => {
            let k = zest_proto::Keyframe {
                cols,
                rows,
                rows_data,
                attrs,
                cursor,
                modes: zest_core::Modes::from_bits_truncate(modes),
                blocks,
                blocks_from,
                title: title.clone(),
                history_clears,
            };
            match s.replica_mut(session) {
                Some(r) => {
                    r.reset(&k, seq.0);
                    r.set_title(title);
                }
                None => {
                    let mut r = Replica::new(session, &k, seq.0);
                    r.set_title(title);
                    s.replicas.push(r);
                }
            }
        }
        HostMessage::Update { session, base, seq, delta } => {
            let diverged = match s.replica_mut(session) {
                Some(r) => !r.apply(&delta, base.0, seq.0),
                // An update for a session this connection is not tracking.
                // Nothing to apply and nothing to recover.
                None => false,
            };
            if diverged {
                // Deliberately not a reattach: `RequestKeyframe` recovers
                // without tearing down the subscriber, which would also drop
                // and re-cast this connection's size vote -- and re-casting it
                // is exactly what an observer must never do.
                tracing::debug!(%session, "a delta was refused; asking for a keyframe");
                let _ = tx.send(Outbound::Msg(Box::new(ClientMessage::RequestKeyframe {
                    session,
                })));
            }
        }
        HostMessage::Exited { session, code } => {
            if let Some(r) = s.replica_mut(session) {
                r.set_exited(code);
            }
        }
        HostMessage::Error { message, .. } => {
            s.error = Some(message);
        }
        _ => {}
    }
    // One notify for every message, rather than one per interesting arm: a
    // waiter's predicate decides what it cares about, and a missed wake is a
    // tool that hangs until its deadline for no reason.
    drop(s);
    cv.notify_all();
}
