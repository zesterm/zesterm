//! A session that lives in a daemon.
//!
//! Implements [`SessionSource`](crate::source::SessionSource) against
//! `zest-proto` instead of against a pty. The renderer cannot tell the
//! difference, because the grid it reads is a real `zest_core::Terminal` that
//! deltas are applied into — that is the promise `docs/CONTRACTS.md` makes, and
//! `RemoteWriter` is what keeps it.
//!
//! Deliberately shaped like [`crate::session::Session`], down to the thread
//! names and the `lock_unfair` + `yield_now` pair, so the two can be read side
//! by side and any divergence is visible rather than buried.
//!
//! # Detach is not exit
//!
//! A dropped connection means the *link* died. The shell is still running in a
//! daemon that does not care, which is the entire point of ADR-007 — close the
//! lid, pick the session up from a phone. Conflating the two would close the
//! window on every Wi-Fi hiccup.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use zest_core::Terminal;
use zest_mesh::identity::ClientIdentity;
use zest_mesh::secure::Sealer;
use zest_proto::{
    frame, Applied, Applier, ClientMessage, HostMessage, SessionAddr, Seq,
};

use zest_daemon::client::DaemonClient;
use crate::fair_mutex::FairMutex;
use crate::session::Wakeup;
use crate::source::{Origin, SessionSource};

/// How often to acknowledge, at most.
///
/// One ack per applied delta would double the message count on a busy session
/// for information the host only needs approximately. A frame's worth of
/// coalescing costs nothing and matches how often the grid is looked at anyway.
const ACK_INTERVAL: Duration = Duration::from_millis(16);

/// What the writer thread does next.
enum Outbound {
    Msg(ClientMessage),
    /// The link came back. Write to this from now on.
    ///
    /// The sealer rides with the sink, and must: a redial is a fresh handshake
    /// with fresh keys and counters starting at zero (`remote.rs` has always
    /// redialled rather than resumed, which is exactly what makes that safe).
    /// A sealer that outlived its socket would encrypt under the old key and
    /// the new daemon connection would refuse every frame.
    Stream(Box<dyn Write + Send>, Option<Sealer>),
    Shutdown,
}

/// How to open a fresh connection to the daemon.
///
/// A reconnect needs to *dial*, not merely to be handed two halves — so this
/// replaces the halves the first attach used to take. The first connection is
/// then simply the first dial, which is why there is one code path rather than a
/// connect path and a reconnect path that drift apart.
pub type Dialer =
    Box<dyn Fn() -> Result<(Box<dyn Read + Send>, Box<dyn Write + Send>), RemoteError> + Send>;

/// How long to wait before redialling, and the ceiling it grows to.
const REDIAL_MIN: Duration = Duration::from_millis(200);
const REDIAL_MAX: Duration = Duration::from_secs(5);

/// What to attach to, and how to describe this client while doing it.
#[derive(Debug, Clone, Copy)]
pub struct AttachOptions<'a> {
    /// The key this client proves it holds.
    ///
    /// Not a bare id: the host challenges, and only the holder of the secret
    /// can answer. On loopback the answer authorizes nothing -- the socket's
    /// permissions already did -- which is why an ephemeral key is enough there
    /// and the OS keychain stays off the startup path.
    /// Behind an `Arc` so the supervisor thread can keep proving it: every
    /// reconnect is a fresh handshake, and the host challenges each time.
    pub identity: &'a Arc<ClientIdentity>,
    /// Shown in the host's log and its approval prompt.
    pub label: &'a str,
    /// Empty means the host's default shell.
    pub command: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub scrollback: usize,
    /// Attach to an existing unattached session instead of starting a new one.
    ///
    /// The GUI no longer uses this: restore (#23) reattaches the exact
    /// sessions a window was showing, which answers "pick it up later"
    /// without guessing. Adoption survives for `--attach <host:port>`, where
    /// "a shell on that machine" genuinely is the request, and for the
    /// reconnect supervisor's `Rebind::AdoptOrCreate`.
    pub adopt: bool,
    /// Whether this connection is the loopback socket.
    ///
    /// Decided by the transport rather than by comparing `HostId`s, because
    /// "did I reach this machine" and "did I reach the machine I dialled" are
    /// different questions and only the second is about identity.
    pub local: bool,
    /// The host this client believes it is dialling.
    ///
    /// Checked against the signed challenge, which is the whole reason the host
    /// signs first: an address learned from an mDNS advertisement is a claim,
    /// and without this "connect to my Mac" means "connect to whatever answered
    /// on that port". `None` on loopback, where the socket is the answer.
    pub expect_host: Option<zest_proto::HostId>,
}

/// What the supervisor does when the host answers but the session is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
pub enum Rebind {
    /// Adopt any unattached session, else create a fresh one. Right for a
    /// window that wants *a* shell on that machine more than a particular
    /// one — today's GUI default, and `--attach`'s.
    AdoptOrCreate,
    /// The tab is a name for one session. If the host answers and that
    /// session no longer exists, report [`Wakeup::SessionGone`] and stop —
    /// silently swapping a fresh shell in under a labeled tab is how someone
    /// types into the wrong machine's wrong shell.
    Pinned,
}

/// How the first connection picks its session.
#[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
enum Target {
    /// List, then adopt-or-create per [`AttachOptions::adopt`].
    Open,
    /// Exactly this session, which must already exist.
    Existing(SessionAddr),
    /// A fresh session, never adopted.
    Create,
}

pub struct RemoteSession {
    terminal: Arc<FairMutex<Terminal>>,
    /// Set by the reader, cleared by the renderer. Also the coalescing latch.
    needs_redraw: Arc<AtomicBool>,
    /// Interior mutability for `write`/`resize` on `&self` — the same trick
    /// `Session::pty_tx` uses, for the same reason.
    tx: Sender<Outbound>,
    /// Shared with the supervisor, which rebinds it when a restarted daemon
    /// hands out a fresh session. Before this was shared, input kept
    /// addressing the *old* session after such a rebind: output flowed (the
    /// reader uses its own copy) while every keystroke went to an address the
    /// daemon no longer had.
    addr: Arc<parking_lot::Mutex<SessionAddr>>,
    origin: Origin,
    /// Joined on drop, so the `Detach` is actually written before the process
    /// ends rather than racing it.
    writer: Option<std::thread::JoinHandle<()>>,
}

/// Detach when the client goes away.
///
/// **`Detach`, never `CloseSession`.** The shell keeps running in the daemon,
/// which is the entire payoff of ADR-007: close the lid, pick the session up
/// from a phone. A destructor rather than a `CloseRequested` arm because it
/// covers every way this process can end, including the ones no event handler
/// would see.
impl Drop for RemoteSession {
    fn drop(&mut self) {
        let addr = *self.addr.lock();
        let _ = self.tx.send(Outbound::Msg(ClientMessage::Detach { session: addr }));
        let _ = self.tx.send(Outbound::Shutdown);
        // Joined, not fire-and-forget: the writer runs on its own thread, and
        // without this the process can exit before the frame reaches the
        // socket -- leaving the daemon holding a subscriber for a client that
        // is already gone.
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

impl RemoteSession {
    /// Attach to a session on an already-connected daemon.
    ///
    /// `read`/`write` are the two halves of the connection; the handshake has
    /// not happened yet.
    pub fn attach(
        dial: Dialer,
        opts: &AttachOptions<'_>,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, RemoteError> {
        Self::start(dial, opts, Target::Open, Rebind::AdoptOrCreate, wake)
    }

    /// Attach to exactly this session, which must already exist.
    ///
    /// The tab strip's constructor: a tab is a name for one session, so the
    /// supervisor is [`Rebind::Pinned`] — a host that answers without the
    /// session makes the tab say so instead of quietly becoming a new shell.
    #[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
    pub fn attach_existing(
        dial: Dialer,
        addr: SessionAddr,
        opts: &AttachOptions<'_>,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, RemoteError> {
        Self::start(dial, opts, Target::Existing(addr), Rebind::Pinned, wake)
    }

    /// Create a fresh session and attach to it, never adopting.
    ///
    /// ⌘T's constructor. Pinned for the same reason as
    /// [`Self::attach_existing`]: once created, the tab names that session.
    #[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
    pub fn create_and_attach(
        dial: Dialer,
        opts: &AttachOptions<'_>,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, RemoteError> {
        Self::start(dial, opts, Target::Create, Rebind::Pinned, wake)
    }

    /// The session this window is currently attached to.
    ///
    /// "Currently": under [`Rebind::AdoptOrCreate`] a daemon restart rebinds
    /// the supervisor to a fresh session, and this follows it.
    #[must_use]
    #[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
    pub fn addr(&self) -> SessionAddr {
        *self.addr.lock()
    }

    /// End the session deliberately — the daemon hangs its child up — then
    /// detach.
    ///
    /// Consuming `self` is the delivery guarantee: the `CloseSession` is
    /// enqueued ahead of Drop's `Detach` + `Shutdown` on the same ordered
    /// channel, and Drop joins the writer, so the frame reaches the socket
    /// before the process moves on. The same hazard class as the explicit
    /// drop before `process::exit` in the attach probe.
    #[allow(dead_code, reason = "the tab strip is the second consumer, later in the #23 sequence")]
    pub fn kill(self) {
        let addr = *self.addr.lock();
        let _ = self.tx.send(Outbound::Msg(ClientMessage::CloseSession { session: addr }));
        // Drop runs here: Detach, Shutdown, join.
    }

    fn start(
        dial: Dialer,
        opts: &AttachOptions<'_>,
        target: Target,
        rebind: Rebind,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, RemoteError> {
        let &AttachOptions {
            identity,
            label,
            command,
            cols,
            rows,
            scrollback,
            adopt,
            local,
            expect_host,
        } = opts;
        let (tx, rx): (Sender<Outbound>, Receiver<Outbound>) = crossbeam_channel::unbounded();

        // The handshake runs inline, before any thread starts, so a failure is
        // an error the caller can fall back from rather than a window that
        // opens and then reports it has nothing to show.
        let (read, write) = dial()?;
        let mut conn = DaemonClient::connect(read, write, identity, label, expect_host, false)?;
        let addr = match target {
            Target::Open => conn.open_session(command, cols, rows, adopt)?,
            Target::Existing(a) => a,
            Target::Create => conn.create(command, "", cols, rows)?,
        };
        let (keyframe_seq, keyframe) = conn.attach(addr, cols, rows)?;
        let host_label = conn.host_label().to_string();
        let halves = conn.into_halves();
        let (mut reader, writer, channel) = (halves.read, halves.write, halves.channel);
        // The handshake's leftovers, not a fresh buffer. Everything the host
        // wrote behind the attach keyframe is already off the socket and lives
        // only here; starting the streaming reader empty discards it, and a
        // sealed channel does not survive a discarded frame. Issue #54.
        let carried = halves.frames;
        // Split rather than shared: the two directions have separate keys and
        // separate counters, so the reader thread and the writer thread need no
        // lock between them. See `SecureChannel::split`.
        let (mut sealer, mut opener) = match channel {
            Some(c) => {
                let (s, o) = c.split();
                (Some(s), Some(o))
            }
            None => (None, None),
        };
        let addr_cell = Arc::new(parking_lot::Mutex::new(addr));

        let terminal = Arc::new(FairMutex::new(Terminal::new(
            usize::from(cols),
            usize::from(rows),
            scrollback,
        )));
        let needs_redraw = Arc::new(AtomicBool::new(true));

        let mut applier = Applier::new();
        {
            let mut term = terminal.lock();
            applier.apply_keyframe(&mut term, &keyframe, keyframe_seq);
        }

        // --- writer thread ---
        let writer_thread = {
            let mut sink: Option<Box<dyn Write + Send>> = Some(writer);
            let mut sealer = sealer.take();
            // The newest resize seen while disconnected, replayed on reconnect.
            //
            // Only the newest, and only resizes. Replaying queued *keystrokes*
            // into a shell after a reconnect is how a link that dropped for
            // thirty seconds runs a command the user typed, thought better of,
            // and watched go nowhere. A stale size, by contrast, is worse than
            // useless: the shell would redraw for a window that has since
            // changed again.
            let mut pending_resize: Option<ClientMessage> = None;
            std::thread::Builder::new()
                .name("zest-remote-writer".into())
                .spawn(move || {
                    while let Ok(item) = rx.recv() {
                        match item {
                            Outbound::Stream(new_sink, new_sealer) => {
                                sink = Some(new_sink);
                                sealer = new_sealer;
                                if let Some(msg) = pending_resize.take() {
                                    write_msg(&mut sink, sealer.as_mut(), &msg);
                                }
                            }
                            Outbound::Msg(msg) => {
                                let Some(s) = sink.as_mut() else {
                                    // Disconnected. Hold the size, drop the rest.
                                    if matches!(msg, ClientMessage::Resize { .. }) {
                                        pending_resize = Some(msg);
                                    }
                                    continue;
                                };
                                let Some(bytes) = seal_msg(sealer.as_mut(), &msg) else { continue };
                                if s.write_all(&bytes).is_err() || s.flush().is_err() {
                                    // The reader is the supervisor and will
                                    // notice the same break. Dropping the sink
                                    // here is what makes the *next* message take
                                    // the branch above rather than writing into
                                    // a socket that is gone.
                                    sink = None;
                                    if matches!(msg, ClientMessage::Resize { .. }) {
                                        pending_resize = Some(msg);
                                    }
                                }
                            }
                            Outbound::Shutdown => break,
                        }
                    }
                })
                .map_err(|e| RemoteError::Thread(e.to_string()))?
        };

        // --- reader thread, which is also the supervisor ---
        {
            let terminal = Arc::clone(&terminal);
            let needs_redraw = Arc::clone(&needs_redraw);
            let tx = tx.clone();
            let identity = Arc::clone(identity);
            let label = label.to_string();
            let command = command.to_string();
            let addr_cell = Arc::clone(&addr_cell);
            std::thread::Builder::new()
                .name("zest-remote-reader".into())
                .spawn(move || {
                    let mut frames = carried;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut last_ack = Instant::now();
                    let mut pending_ack: Option<u64> = None;
                    // Where to resume from if the link comes back. Kept across
                    // reconnects on purpose: the whole point of reattaching in
                    // place is that this client's `Terminal`, and the scrollback
                    // it has accumulated, survive.
                    let mut addr = addr;

                    'supervise: loop {
                    // The handshake's reader hands this loop frames it has
                    // already lifted off the socket: the daemon writes a whole
                    // attach batch back to back and flushes once, so one `read`
                    // in `DaemonClient::recv` can take the Keyframe and
                    // everything queued behind it. `Halves::frames` carries
                    // those across the handoff.
                    //
                    // They must be drained *before* blocking on the socket.
                    // Reading first works only while the session keeps talking:
                    // a command that prints and exits sends nothing more, so
                    // the carried frames would wait for a read that never
                    // comes, and the output would be lost exactly as it was
                    // before the handoff carried it -- the same blank window,
                    // one layer further in. Carrying them and not draining them
                    // is not half a fix, it is none.
                    let mut drain_carried = frames.pending() > 0;
                    'link: loop {
                        if drain_carried {
                            drain_carried = false;
                        } else {
                            let n = match reader.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            frames.feed(&buf[..n]);
                        }

                        loop {
                            let body = match frames.next_frame() {
                                Ok(Some(b)) => b,
                                Ok(None) => break,
                                // Framing is lost, so the stream position is no
                                // longer trustworthy. Nothing to resume from --
                                // but a fresh connection has nothing to resume,
                                // so redial rather than give up. See below.
                                Err(e) => {
                                    tracing::warn!(error = %e, "framing is lost; redialling");
                                    break 'link;
                                }
                            };

                            // Opened here, where the frame stops being opaque.
                            // A frame that will not open finishes *this
                            // connection*: the counter has already advanced, so
                            // there is no position to resume from, and
                            // continuing would read every later frame under the
                            // wrong nonce.
                            //
                            // Redialling rather than returning, because the two
                            // are not the same admission. There is no way to
                            // repair this channel, and no reason a fresh one
                            // should inherit its problem: a redial is a new
                            // handshake with new keys and counters from zero,
                            // which is exactly what the loop below already does
                            // for a link that dropped. Returning here instead
                            // left a window that is alive, repaints, accepts
                            // typing and is deaf for ever, with one `warn!` as
                            // the only trace it ever happened -- which is how
                            // issue #54 presented.
                            let body = match opener.as_mut() {
                                Some(o) => match o.open(&body) {
                                    Ok(plain) => plain,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "a sealed frame did not open; redialling");
                                        break 'link;
                                    }
                                },
                                None => body,
                            };

                            let Ok(msg) = frame::decode::<HostMessage>(&body) else {
                                // A message this build cannot parse is a newer
                                // host, not a broken one. Resync rather than
                                // disconnect -- this is what lets a future
                                // DeltaOp be added without a version bump.
                                let _ = tx.send(Outbound::Msg(ClientMessage::RequestKeyframe {
                                    session: addr,
                                }));
                                continue;
                            };

                            match msg {
                                HostMessage::Keyframe { seq, rows_data, attrs, cursor, cols, rows, modes, blocks, title, .. } => {
                                    let k = zest_proto::Keyframe {
                                        cols,
                                        rows,
                                        rows_data,
                                        attrs,
                                        cursor,
                                        modes: zest_core::Modes::from_bits_truncate(modes),
                                        blocks,
                                        title,
                                    };
                                    {
                                        let mut term = terminal.lock_unfair();
                                        applier.apply_keyframe(&mut term, &k, seq.0);
                                    }
                                    pending_ack = Some(seq.0);
                                    mark(&needs_redraw, &wake);
                                }
                                HostMessage::Update { base, seq, delta, .. } => {
                                    let outcome = {
                                        let mut term = terminal.lock_unfair();
                                        applier.apply_delta(&mut term, &delta, base.0, seq.0)
                                    };
                                    match outcome {
                                        Applied::Ok => {
                                            pending_ack = Some(seq.0);
                                            mark(&needs_redraw, &wake);
                                        }
                                        // Nothing was applied. Ask for a whole
                                        // state rather than carrying on against
                                        // a grid that has silently diverged.
                                        Applied::NeedsKeyframe => {
                                            let _ = tx.send(Outbound::Msg(
                                                ClientMessage::RequestKeyframe { session: addr },
                                            ));
                                        }
                                    }
                                }
                                HostMessage::Scrollback { rows_data, attrs, .. } => {
                                    let mut term = terminal.lock_unfair();
                                    applier.absorb_attrs(&attrs);
                                    applier.apply_scrollback(&mut term, &rows_data);
                                    drop(term);
                                    mark(&needs_redraw, &wake);
                                }
                                // Returns rather than breaking, so `finish`
                                // below is only ever reached when the *link*
                                // died -- which is what makes the distinction
                                // between Exited and Detached structural.
                                HostMessage::Exited { .. } => {
                                    wake(Wakeup::Exited);
                                    return;
                                }
                                HostMessage::Error { message, .. } => {
                                    tracing::warn!(%message, "daemon reported an error");
                                }
                                _ => {}
                            }
                        }

                        // Coalesced, and always after a keyframe so the host
                        // learns the resync landed.
                        if let Some(seq) = pending_ack {
                            if last_ack.elapsed() >= ACK_INTERVAL {
                                let _ = tx.send(Outbound::Msg(ClientMessage::Ack {
                                    session: addr,
                                    seq: Seq(seq),
                                }));
                                pending_ack = None;
                                last_ack = Instant::now();
                            }
                        }

                        // The reader is the thread in the tight loop, so it
                        // must not barge ahead of a renderer already waiting.
                        std::thread::yield_now();
                    }

                    // The link ended with the shell still running -- the socket
                    // went away, or the channel did. `Detached`, never
                    // `Exited`: the `Exited` arm returns rather than breaking,
                    // which is what makes that distinction structural instead
                    // of a judgement made here. Tell the window -- it keeps
                    // showing the last state that was true -- and start
                    // dialling.
                    wake(Wakeup::Detached);

                    let mut wait = REDIAL_MIN;
                    loop {
                        std::thread::sleep(wait);
                        wait = (wait * 2).min(REDIAL_MAX);

                        let Ok((r, w)) = dial() else { continue };
                        let Ok(mut conn) =
                            DaemonClient::connect(r, w, &identity, &label, expect_host, false)
                        else {
                            continue;
                        };
                        // The session we lost first. Its shell is still running
                        // and our subscriber was released when the connection
                        // dropped, so it is sitting there unattached.
                        let attached = match conn.attach(addr, cols, rows) {
                            Ok(ok) => Ok(ok),
                            // Gone -- and what happens next is the whole
                            // difference between the two rebind modes.
                            Err(e) => match rebind {
                                // The daemon was restarted, so the shells went
                                // with it. A new one is the honest outcome for
                                // a window that wants *a* shell: one that can
                                // be typed into again, rather than one that
                                // never recovers.
                                Rebind::AdoptOrCreate => conn
                                    .open_session(&command, cols, rows, true)
                                    .and_then(|fresh| {
                                        addr = fresh;
                                        *addr_cell.lock() = fresh;
                                        conn.attach(fresh, cols, rows)
                                    }),
                                Rebind::Pinned => match e {
                                    // The host answered and said no: the
                                    // session is gone, not the link. A pinned
                                    // tab reports that and stops, rather than
                                    // guessing at a replacement.
                                    zest_daemon::DaemonError::Refused(_) => {
                                        wake(Wakeup::SessionGone(addr));
                                        return;
                                    }
                                    // Anything else is the link's fault; keep
                                    // dialling.
                                    _ => continue,
                                },
                            },
                        };
                        let Ok((seq, keyframe)) = attached else { continue };

                        let halves = conn.into_halves();
                        let (r, w, channel) = (halves.read, halves.write, halves.channel);
                        // Same reason as the first attach: a redial coalesces
                        // exactly as readily as a first connection does.
                        let carried = halves.frames;
                        let (new_sealer, new_opener) = match channel {
                            Some(c) => {
                                let (s, o) = c.split();
                                (Some(s), Some(o))
                            }
                            None => (None, None),
                        };
                        if tx.send(Outbound::Stream(w, new_sealer)).is_err() {
                            return;
                        }
                        opener = new_opener;
                        {
                            let mut term = terminal.lock_unfair();
                            applier.apply_keyframe(&mut term, &keyframe, seq);
                        }
                        pending_ack = Some(seq);
                        frames = carried;
                        reader = r;
                        tracing::info!(%addr, "reattached");
                        needs_redraw.store(true, Ordering::Release);
                        wake(Wakeup::Reattached);
                        wake(Wakeup::Redraw);
                        continue 'supervise;
                    }
                    }
                })
                .map_err(|e| RemoteError::Thread(e.to_string()))?;
        }

        Ok(Self {
            terminal,
            needs_redraw,
            tx,
            addr: addr_cell,
            origin: Origin::Daemon { host: host_label, local },
            writer: Some(writer_thread),
        })
    }
}

/// Write one message to the current sink, dropping it if the link is gone.
fn write_msg(
    sink: &mut Option<Box<dyn Write + Send>>,
    sealer: Option<&mut Sealer>,
    msg: &ClientMessage,
) {
    let Some(s) = sink.as_mut() else { return };
    let Some(bytes) = seal_msg(sealer, msg) else { return };
    if s.write_all(&bytes).is_err() || s.flush().is_err() {
        *sink = None;
    }
}

/// Serialize one message, sealed if this link has a channel.
///
/// `None` on failure rather than an error, matching what the two callers
/// already did with an encoding failure: there is nowhere to report it from a
/// writer thread, and a message that cannot be built is one the daemon simply
/// never hears. Sealing failure is different in kind — it means the counter
/// wrapped past what the epoch can carry — but it is equally unreportable here,
/// so it is logged rather than swallowed silently.
fn seal_msg(sealer: Option<&mut Sealer>, msg: &ClientMessage) -> Option<Vec<u8>> {
    let body = frame::encode_body(msg).ok()?;
    let body = match sealer {
        Some(s) => match s.seal(&body) {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::error!(error = %e, "could not seal a message");
                return None;
            }
        },
        None => body,
    };
    frame::frame_bytes(&body).ok()
}

fn mark(needs_redraw: &Arc<AtomicBool>, wake: &impl Fn(Wakeup)) {
    // Only on the false->true transition, so a 100MB `cat` posts one event per
    // frame rather than millions. Same latch as the local session.
    if !needs_redraw.swap(true, Ordering::AcqRel) {
        wake(Wakeup::Redraw);
    }
}

impl SessionSource for RemoteSession {
    fn terminal(&self) -> &Arc<FairMutex<Terminal>> {
        &self.terminal
    }

    fn write(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let session = *self.addr.lock();
        let _ = self.tx.send(Outbound::Msg(ClientMessage::Input { session, bytes }));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let session = *self.addr.lock();
        let _ = self.tx.send(Outbound::Msg(ClientMessage::Resize { session, cols, rows }));

        // And ask for the whole state back.
        //
        // Nothing else would tell this client the grid changed shape. There is
        // no `DeltaOp::Resize`, and the size only ever travels in a keyframe --
        // so on a *shrink* the deltas that follow describe rows 0..new_rows,
        // every one of which lands inside the client's older, larger grid. No
        // row falls past the end, `Applied::NeedsKeyframe` never fires, and the
        // client keeps a grid the host no longer has: the rows below the new
        // height stay on screen holding whatever they last held, and everything
        // above is misaligned against a viewport that has moved.
        //
        // Growing happened to work by accident, because a taller grid does push
        // rows past the end and trips the resync. That asymmetry is exactly why
        // this is explicit rather than left to the applier.
        //
        // The host's answer is authoritative, which matters: another client may
        // be attached at a different size, so what this one asked for is a
        // request, not a fact. Resizing is human-speed, so a whole state costs
        // nothing worth counting.
        let _ = self.tx.send(Outbound::Msg(ClientMessage::RequestKeyframe { session }));
    }

    fn take_dirty(&self) -> bool {
        self.needs_redraw.swap(false, Ordering::AcqRel)
    }

    fn mark_dirty(&self) {
        self.needs_redraw.store(true, Ordering::Release);
    }

    fn origin(&self) -> Origin {
        self.origin.clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("daemon speaks protocol {theirs}, this build speaks {ours}")]
    Version { ours: u16, theirs: u16 },
    #[error("the daemon refused this client: {0}")]
    Refused(String),
    #[error("the daemon closed the connection during the handshake")]
    Closed,
    #[error("transport failed: {0}")]
    Io(String),
    #[error("could not start a thread: {0}")]
    Thread(String),
}

/// The client half now lives in `zest-daemon`, so its errors arrive as
/// `DaemonError`.
///
/// Mapped variant by variant rather than collapsed into one string, because
/// `Refused` is load-bearing: the redial loop at `supervise` stops on a refusal
/// and keeps dialling on anything else. Flattening these would turn "this
/// device is not paired" into an infinite reconnect against a host that will
/// never say yes.
impl From<zest_daemon::DaemonError> for RemoteError {
    fn from(e: zest_daemon::DaemonError) -> Self {
        use zest_daemon::DaemonError as D;
        match e {
            D::Version { ours, theirs } => Self::Version { ours, theirs },
            D::Refused(m) => Self::Refused(m),
            D::Closed => Self::Closed,
            other => Self::Io(other.to_string()),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A daemon on a real socket, in this process.
    ///
    /// Deliberately a real socket rather than an in-memory pipe: the whole
    /// point of this type is that it speaks the wire protocol over a transport,
    /// and a test that skipped the transport would not exercise framing.
    struct Harness {
        path: String,
        registry: Arc<zest_daemon::Registry>,
        host_identity: Arc<zest_mesh::identity::HostIdentity>,
    }

    /// A read half that sleeps before every read.
    ///
    /// What it stands in for is a client thread that is not scheduled between
    /// sending `Attach` and reading the reply — a loaded CI runner, a laptop
    /// waking up, a GC-sized pause anywhere in the process. The daemon writes a
    /// whole batch back to back and flushes once (`server.rs`, the writer
    /// loop), so by the time the late read finally runs, several of the host's
    /// frames are sitting in the socket together and one `read` takes them all.
    ///
    /// That is the *normal* behaviour of a stream socket, not a fault being
    /// injected: the stall only makes coalescing happen every time instead of
    /// once in a thousand runs. Anything the client does not consume from that
    /// one read has to survive, or it is gone — and because the channel's
    /// nonces are an implicit per-direction counter, gone means the connection
    /// never opens another frame either.
    struct StalledRead<R: Read> {
        inner: R,
        stall: Duration,
    }

    impl<R: Read> Read for StalledRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.stall);
            self.inner.read(buf)
        }
    }

    /// Long enough that the host has flushed more than once into the socket
    /// before the client's next read runs, short enough that a handshake made
    /// of a handful of round trips still finishes in a few seconds.
    const STALL: Duration = Duration::from_millis(700);

    /// Flips one byte of one read, when the test says so.
    ///
    /// Enough to break a sealed record: the AEAD tag covers the whole thing, so
    /// a single flipped bit anywhere in it makes `open` fail. Armed from the
    /// test rather than after a byte count, because the handshake happens on
    /// this same socket and corrupting *that* fails the dial instead of the
    /// thing under test.
    struct CorruptWhenArmed<R: Read> {
        inner: R,
        armed: Arc<AtomicBool>,
    }

    impl<R: Read> Read for CorruptWhenArmed<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            if n > 0 && self.armed.swap(false, Ordering::AcqRel) {
                buf[n - 1] ^= 0xff;
            }
            Ok(n)
        }
    }

    /// A cuttable link between the client and the daemon.
    ///
    /// Reconnect cannot be tested without a way to break a connection, and there
    /// is none: the daemon's listener runs forever and `RemoteSession` keeps its
    /// halves to itself. So the client dials this instead, it forwards bytes to
    /// the real socket, and `cut` shuts every live pair down — which is what a
    /// dropped Wi-Fi link looks like from both ends.
    struct Link {
        path: String,
        live: Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
    }

    impl Link {
        fn new(target: &str, name: &str) -> Self {
            let path = format!("/tmp/zl-{}-{}.sock", name, std::process::id());
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind link");
            let live: Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));

            let target = target.to_string();
            let live_for_thread = Arc::clone(&live);
            std::thread::spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(client) = incoming else { break };
                    let Ok(server) = std::os::unix::net::UnixStream::connect(&target) else {
                        break;
                    };
                    // Both ends are remembered so `cut` can shut them, which is
                    // what unblocks the two pumps parked in `read`.
                    {
                        let mut live = live_for_thread.lock().expect("live lock");
                        live.push(client.try_clone().expect("clone"));
                        live.push(server.try_clone().expect("clone"));
                    }
                    for (mut from, mut to) in [
                        (client.try_clone().expect("c"), server.try_clone().expect("s")),
                        (server, client),
                    ] {
                        std::thread::spawn(move || {
                            let _ = std::io::copy(&mut from, &mut to);
                            let _ = to.shutdown(std::net::Shutdown::Both);
                        });
                    }
                }
            });

            let deadline = Instant::now() + Duration::from_secs(5);
            while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Self { path, live }
        }

        fn cut(&self) {
            for s in self.live.lock().expect("live lock").drain(..) {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    impl Drop for Link {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Harness {
        fn start(name: &str) -> Self {
            // Short path: a unix socket must fit in SUN_LEN (~104 bytes).
            let path = format!("/tmp/zr-{}-{}.sock", name, std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{path}.lock"));

            let registry = Arc::new(zest_daemon::Registry::new());
            // A real host key, because the handshake is real: these tests drive
            // the same code the daemon binary does.
            let host_identity =
                Arc::new(zest_mesh::identity::HostIdentity::generate().expect("host key"));
            let auth = Arc::new(zest_daemon::Authenticator::new(
                Arc::clone(&host_identity),
                Arc::new(zest_mesh::trust::MemoryTrustStore::new()),
                zest_mesh::pairing::PairingQueue::new(),
                "harness",
            ));
            let cfg = zest_daemon::DaemonConfig {
                host: zest_proto::HostId::from_bytes([3; 32]),
                label: "harness".into(),
                local_socket: path.clone(),
                listen_lan: false,
                lan_bind: "127.0.0.1".into(),
                lan_port: 0,
                listen_ws: false,
                ws_bind: "127.0.0.1".into(),
                ws_port: 0,
                shell_integration: true,
            };
            {
                let registry = Arc::clone(&registry);
                let path = path.clone();
                std::thread::spawn(move || {
                    let _ = zest_daemon::listen(&path, cfg, registry, auth);
                });
            }

            let deadline = Instant::now() + Duration::from_secs(5);
            while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Self { path, registry, host_identity }
        }

        fn attach(&self, command: &str, wake: impl Fn(Wakeup) + Send + 'static) -> RemoteSession {
            self.attach_with(command, false, wake)
        }

        /// Attach over a link the test can cut, rather than straight to the
        /// daemon socket.
        fn attach_through(
            &self,
            link: &Link,
            command: &str,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            self.attach_dialling(&link.path, command, false, wake)
        }

        fn attach_with(
            &self,
            command: &str,
            adopt: bool,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            self.attach_dialling(&self.path.clone(), command, adopt, wake)
        }

        /// Attach with every read on the client's half delayed by `stall`.
        ///
        /// See [`StalledRead`] for what this is simulating and why the delay is
        /// what makes the bug deterministic rather than a coin toss.
        fn attach_stalled(
            &self,
            command: &str,
            stall: Duration,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            self.attach_dialling_stalled(&self.path.clone(), command, false, stall, wake)
        }

        fn attach_dialling(
            &self,
            socket: &str,
            command: &str,
            adopt: bool,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            self.attach_dialling_stalled(socket, command, adopt, Duration::ZERO, wake)
        }

        fn attach_dialling_stalled(
            &self,
            socket: &str,
            command: &str,
            adopt: bool,
            stall: Duration,
            wake: impl Fn(Wakeup) + Send + 'static,
        ) -> RemoteSession {
            let identity = Arc::new(ClientIdentity::generate().expect("client key"));
            // A real dialer, so the reconnect path is the one under test rather
            // than a mock of it.
            let path = socket.to_string();
            let dial: Dialer = Box::new(move || {
                let stream = zest_daemon::connect(&path)
                    .map_err(|e| RemoteError::Io(e.to_string()))?;
                let write = stream.try_clone().map_err(|e| RemoteError::Io(e.to_string()))?;
                let read: Box<dyn Read + Send> = if stall.is_zero() {
                    Box::new(stream)
                } else {
                    Box::new(StalledRead { inner: stream, stall })
                };
                Ok((read, Box::new(write) as Box<dyn Write + Send>))
            });
            RemoteSession::attach(
                dial,
                &AttachOptions {
                    identity: &identity,
                    label: "test",
                    command,
                    cols: 40,
                    rows: 6,
                    scrollback: 100,
                    adopt,
                    local: true,
                    expect_host: None,
                },
                wake,
            )
            .expect("attach")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}.lock", self.path));
        }
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        wait_up_to(Duration::from_secs(10), f)
    }

    /// A deadline has to be longer than the worst case it is waiting on.
    ///
    /// Reconnecting is the one thing here that legitimately takes seconds: the
    /// redial backoff doubles from 200ms to a 5s ceiling, so five unlucky
    /// attempts spend 6.2s before the sixth is even made. Ten seconds sits
    /// *inside* that, which under load makes the test fail for doing exactly
    /// what it is documented to do — and once produced a failure that vanished
    /// on the next sixteen runs.
    fn wait_up_to(limit: Duration, f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn output_from_a_daemon_session_reaches_the_local_grid() {
        // The whole client half in one assertion: a shell running in another
        // process, its output encoded as deltas, applied into a Terminal here,
        // and readable through exactly the path the renderer uses.
        let h = Harness::start("output");
        let s = h.attach("/bin/echo hello-from-the-daemon", |_| {});

        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("hello-from-the-daemon")),
            "output never arrived; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    /// A command that prints twenty lines over two seconds and then a marker.
    ///
    /// The drip is what makes the stalled-attach tests deterministic instead of
    /// lucky: with output every 100ms and a read that takes 700ms, the host has
    /// certainly flushed at least one `Update` behind the attach `Keyframe` by
    /// the time that one read runs. A single `echo` would have to land inside a
    /// window the test cannot control.
    fn dripping(marker: &str) -> String {
        format!(
            "/bin/sh -c 'i=0; while [ $i -lt 20 ]; do echo drip-$i; sleep 0.1; \
             i=$((i+1)); done; echo {marker}'"
        )
    }

    /// Frames that arrive in the same read as the attach keyframe must not be lost.
    ///
    /// This is issue #54. The daemon batches, the socket coalesces, and the
    /// client's `recv` returns one message per call while keeping the rest in
    /// its `FrameReader` — so whatever else landed in that read exists only in
    /// that buffer. Handing the streaming reader a *fresh* `FrameReader` threw
    /// those frames away, and because the seal's nonce is an implicit counter,
    /// throwing them away is not "one lost update": every later frame is then
    /// opened under the wrong nonce and the connection is finished. The user
    /// sees a window that never shows anything.
    #[test]
    fn output_survives_a_stalled_attach() {
        let h = Harness::start("stalled");
        let s = h.attach_stalled(&dripping("tail-of-the-stalled-attach"), STALL, |_| {});

        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("tail-of-the-stalled-attach")),
            "output written after the attach keyframe never arrived, which is what \
             a user reports as 'the command ran but the window is empty'; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    /// A command that prints once and exits is the case carrying alone misses.
    ///
    /// `dripping` cannot catch this, and that is the point of having both:
    /// because it keeps printing, some later read always arrives and flushes
    /// the carried buffer as a side effect, so the test passes whether or not
    /// the buffer is drained on its own account. A command that prints and
    /// exits sends nothing more. The frames sit in the buffer the handoff just
    /// rescued, the reader blocks on a socket that will never speak again, and
    /// the window is blank forever — the same symptom as #54, one layer
    /// further in. Carrying frames without draining them is not half a fix.
    ///
    /// It asserts on `Exited` rather than on the grid, and that is the whole
    /// trick. Two earlier attempts at this test passed with the drain reverted,
    /// because a command short enough to be finished by attach time has its
    /// output *in* the keyframe — the carried `Update` is then redundant and
    /// losing it changes nothing visible. `Exited` has no such understudy: it
    /// exists in exactly one frame, that frame is behind the keyframe in the
    /// same read, and nothing is ever sent after it. If it is stranded, the
    /// window never learns the command finished and the tab never closes.
    #[test]
    fn a_command_that_exits_during_a_stalled_attach_is_still_reported() {
        let seen: Arc<std::sync::Mutex<Vec<Wakeup>>> = Arc::default();
        let sink = Arc::clone(&seen);

        let h = Harness::start("quiet");
        let _s = h.attach_stalled("/bin/echo a-quiet-command", STALL, move |w| {
            sink.lock().expect("lock").push(w);
        });

        assert!(
            wait_for(|| seen.lock().expect("lock").contains(&Wakeup::Exited)),
            "the command exited and the client was never told. Its `Exited` \
             frame arrived in the same read as the attach keyframe, was carried \
             across the handoff, and then sat in a buffer nobody drained while \
             the reader blocked on a socket with nothing left to send. Saw: {:?}",
            seen.lock().expect("lock")
        );
    }

    /// The handoff must not be able to lose a byte, and says so at the seam.
    ///
    /// Ten seconds of blank grid is a terrible way to learn that frames were
    /// dropped. This asserts the same invariant where it is cheap and named:
    /// after a deliberately stalled attach there really are unread bytes
    /// buffered, and they really do reach the streaming reader.
    #[test]
    fn a_stalled_attach_leaves_buffered_frames_for_the_streaming_reader() {
        let h = Harness::start("buffered");
        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let stream = zest_daemon::connect(&h.path).expect("connect");
        let write = stream.try_clone().expect("clone");
        let mut conn = DaemonClient::connect(
            Box::new(StalledRead { inner: stream, stall: STALL }) as Box<dyn Read + Send>,
            Box::new(write) as Box<dyn Write + Send>,
            &identity,
            "test",
            None,
            false,
        )
        .expect("handshake");
        let addr = conn.create(&dripping("tail"), "", 40, 6).expect("create");
        let _ = conn.attach(addr, 40, 6).expect("attach");

        let pending = conn.pending();
        let halves = conn.into_halves();
        assert!(
            pending > 0,
            "the stall did not coalesce anything, so this test is not exercising \
             the handoff at all -- raise STALL or lengthen the drip"
        );
        assert_eq!(
            halves.frames.pending(),
            pending,
            "into_halves dropped the frames the client had already read off the \
             socket; that is issue #54 and it costs the whole connection"
        );
    }

    /// A frame that will not open must cost a reconnect, not the window.
    ///
    /// Defence in depth for the above: whatever desynchronizes a sealed channel
    /// -- a bug like #54, a truncating middlebox, a hostile injection -- the
    /// honest recovery is a fresh handshake, which is exactly what the redial
    /// loop already does for a dropped link. Returning out of the reader thread
    /// instead leaves a window that is alive, repaints, accepts typing, and is
    /// deaf forever, with one `warn!` as the only trace.
    #[test]
    fn a_frame_that_will_not_open_redials_rather_than_going_deaf() {
        let h = Harness::start("desync");
        let armed = Arc::new(AtomicBool::new(false));
        let for_dialer = Arc::clone(&armed);
        let back = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&back);
        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let path = h.path.clone();
        let dial: Dialer = Box::new(move || {
            let stream = zest_daemon::connect(&path).map_err(|e| RemoteError::Io(e.to_string()))?;
            let write = stream.try_clone().map_err(|e| RemoteError::Io(e.to_string()))?;
            let read = CorruptWhenArmed { inner: stream, armed: Arc::clone(&for_dialer) };
            Ok((
                Box::new(read) as Box<dyn Read + Send>,
                Box::new(write) as Box<dyn Write + Send>,
            ))
        });
        let s = RemoteSession::attach(
            dial,
            &AttachOptions {
                identity: &identity,
                label: "test",
                command: "/bin/sh",
                cols: 40,
                rows: 6,
                scrollback: 100,
                adopt: false,
                local: true,
                expect_host: None,
            },
            move |w| {
                if w == Wakeup::Reattached {
                    seen.fetch_add(1, Ordering::Release);
                }
            },
        )
        .expect("attach");

        std::thread::sleep(Duration::from_millis(300));
        s.write(b"echo before-the-desync\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("before-the-desync")),
            "the shell never echoed before the corruption, so nothing below is \
             testing what it claims to; grid:\n{}",
            s.terminal().lock().screen_text()
        );

        // One byte, once. The next frame the client pulls off this socket will
        // not open -- the AEAD tag covers the whole record.
        armed.store(true, Ordering::Release);
        s.write(b"echo after-the-desync\n".to_vec());

        assert!(
            wait_up_to(Duration::from_secs(30), || back.load(Ordering::Acquire) > 0),
            "one unopenable frame ended the reader thread for good: the window is \
             alive, repaints and accepts typing, and will never show another byte"
        );

        // And the link is genuinely usable again, not merely re-dialled.
        s.write(b"echo recovered-from-the-desync\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("recovered-from-the-desync")),
            "reattached, but nothing typed afterwards comes back; grid:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn the_dirty_latch_coalesces_and_clears() {
        // The 0%-idle guarantee, on the remote path. A wake per delta rather
        // than per transition would post an event per frame forever.
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);

        let h = Harness::start("latch");
        let s = h.attach("/bin/echo latched", move |w| {
            if w == Wakeup::Redraw {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });

        assert!(wait_for(|| s.take_dirty()), "never went dirty");
        assert!(!s.take_dirty(), "the latch did not clear");

        // Whatever arrives, the wake count must stay far below the number of
        // deltas -- it is a transition counter, not a message counter.
        let seen = woken.load(Ordering::Relaxed);
        assert!(seen <= 4, "woke {seen} times for one echo; the latch is not coalescing");
    }

    #[test]
    fn typing_reaches_the_child_and_its_echo_comes_back() {
        let h = Harness::start("typing");
        let s = h.attach("/bin/sh", |_| {});

        // Wait for the shell to be up before typing, or the input races the
        // pty being ready and is swallowed.
        std::thread::sleep(Duration::from_millis(300));
        s.write(b"echo round-trip\n".to_vec());

        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("round-trip")),
            "the echo never came back; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn shrinking_the_viewport_resizes_the_local_grid_too() {
        // The size only ever travels in a keyframe -- there is no
        // DeltaOp::Resize -- so on a shrink every following row lands inside
        // the client's older, larger grid, no row falls past the end, and
        // `NeedsKeyframe` never fires. Without an explicit request the client
        // keeps a grid the host no longer has.
        //
        // Growing happened to work by accident, because a taller grid does
        // push rows past the end. That asymmetry is why this is tested from
        // the shrinking side.
        let h = Harness::start("shrink");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(s.terminal().lock().grid().rows(), 6, "attached at the wrong size");

        s.resize(20, 3);

        assert!(
            wait_for(|| {
                let t = s.terminal().lock();
                t.grid().rows() == 3 && t.grid().cols() == 20
            }),
            "the client kept a {}x{} grid after asking for 20x3",
            s.terminal().lock().grid().cols(),
            s.terminal().lock().grid().rows()
        );
    }

    #[test]
    fn reopening_picks_up_the_session_instead_of_leaking_it() {
        // Closing a window only detaches -- the shell keeps running, which is
        // the point. But creating a *new* session on every launch meant the
        // old one could never be reached again: one orphaned shell and one
        // orphaned pty per launch, growing until reboot, with nothing in the
        // UI or the CLI able to list or close them.
        let h = Harness::start("adopt");

        let first = h.attach_with("/bin/sh", true, |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.registry.len(), 1);
        drop(first);
        std::thread::sleep(Duration::from_millis(200));

        // The session survived, and reopening finds it rather than adding one.
        let _second = h.attach_with("/bin/sh", true, |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            h.registry.len(),
            1,
            "reopening created a second session; the first is now unreachable"
        );
    }

    #[test]
    fn a_session_still_updates_after_a_resize() {
        // The bug this exists for: `RequestKeyframe` replied with `Seq(0)`, so
        // the client's baseline went to 0 while the daemon's stayed at the real
        // sequence. Every following update was then refused as stale and
        // triggered another keyframe that again said 0 -- and because a resize
        // is what sends RequestKeyframe, resizing froze the terminal.
        //
        // Asserting that output arrives *after* a resize is what catches it;
        // asserting the resize itself does not.
        let h = Harness::start("afterresize");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));

        s.resize(30, 5);
        assert!(
            wait_for(|| {
                let t = s.terminal().lock();
                t.grid().rows() == 5 && t.grid().cols() == 30
            }),
            "the resize never took"
        );

        // The session must still be live afterwards.
        s.write(b"echo after-resize-works\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("after-resize-works")),
            "nothing arrived after a resize.\ngrid:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn closing_the_client_leaves_the_session_running() {
        // ADR-007's whole payoff, from the client side: close the window, the
        // shell keeps running, and it can be picked up again. Dropping the
        // session is what a closed window does.
        let h = Harness::start("outlive");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));

        assert_eq!(h.registry.len(), 1, "the session was never created");
        drop(s);
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(
            h.registry.len(),
            1,
            "closing a client ended the session -- a shell must outlive the window"
        );
    }

    /// The point of reconnecting *in place*: the same session, and the same
    /// client-side `Terminal`.
    ///
    /// The earlier version of this rebuilt the session from scratch on a
    /// dropped link, which worked and threw away everything the client had
    /// accumulated. A browser tab reconnects as a matter of course rather than
    /// as an exception, so "it works but you lose your scrollback" is not good
    /// enough there — and it was never good enough here either.
    #[test]
    fn a_cut_link_reattaches_to_the_same_session() {
        let h = Harness::start("recut");
        let link = Link::new(&h.path, "recut");
        let back = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&back);
        let s = h.attach_through(&link, "/bin/sh", move |w| {
            if w == Wakeup::Reattached {
                seen.fetch_add(1, Ordering::Release);
            }
        });

        // Something on screen that a fresh session would not have.
        s.write(b"echo before-the-cut\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("before-the-cut")),
            "the shell never echoed; grid:\n{}",
            s.terminal().lock().screen_text()
        );
        let sessions_before = h.registry.len();

        link.cut();

        // Waited for rather than assumed: input typed while the link is down is
        // deliberately dropped, so writing before the reattach would prove
        // nothing except that the drop works.
        assert!(
            wait_up_to(Duration::from_secs(30), || back.load(Ordering::Acquire) > 0),
            "never reattached; the window is dead to the user"
        );

        // The same shell, reached again: no new session, and the text that was
        // on screen before the cut is still there afterwards.
        s.write(b"echo after-the-cut\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("after-the-cut")),
            "never reattached; the window is dead to the user. Grid:\n{}",
            s.terminal().lock().screen_text()
        );
        assert!(
            s.terminal().lock().screen_text().contains("before-the-cut"),
            "reattached, but rebuilt the terminal from scratch -- everything the \
             client had accumulated is gone, which is the whole thing this is \
             meant to avoid"
        );
        assert_eq!(
            h.registry.len(),
            sessions_before,
            "a reconnect created a second session instead of picking up the one \
             whose shell is still running"
        );
    }

    #[test]
    fn a_lost_connection_is_detached_and_never_exited() {
        // Conflating the two would close the window on every Wi-Fi hiccup,
        // throwing away a session that is still running.
        let seen: Arc<std::sync::Mutex<Vec<Wakeup>>> = Arc::default();
        let sink = Arc::clone(&seen);

        let h = Harness::start("detach");
        // A command that ends immediately would report Exited legitimately, so
        // this uses a shell that stays up and has its connection cut instead.
        let s = h.attach("/bin/sh", move |w| {
            sink.lock().expect("lock").push(w);
        });
        std::thread::sleep(Duration::from_millis(300));

        // Drop the session: the writer sends Detach and Shutdown, and the
        // daemon closes the connection, which ends the reader's loop.
        drop(s);
        std::thread::sleep(Duration::from_millis(400));

        let events = seen.lock().expect("lock");
        assert!(
            !events.contains(&Wakeup::Exited),
            "a dropped connection reported Exited: {events:?}"
        );
    }

    fn dial_to(socket: &str) -> Dialer {
        let path = socket.to_string();
        Box::new(move || {
            let stream =
                zest_daemon::connect(&path).map_err(|e| RemoteError::Io(e.to_string()))?;
            let write = stream.try_clone().map_err(|e| RemoteError::Io(e.to_string()))?;
            Ok((
                Box::new(stream) as Box<dyn Read + Send>,
                Box::new(write) as Box<dyn Write + Send>,
            ))
        })
    }

    #[test]
    fn attach_existing_binds_the_session_it_named() {
        // The tab strip's whole premise: a tab names one session, and
        // attaching by address reaches that session — not the newest, not an
        // adopted one, that one.
        let h = Harness::start("existing");
        let first = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));
        let addr = first.addr();
        drop(first); // detach; the shell stays running in the daemon
        std::thread::sleep(Duration::from_millis(200));

        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let s = RemoteSession::attach_existing(
            dial_to(&h.path),
            addr,
            &AttachOptions {
                identity: &identity,
                label: "test",
                command: "",
                cols: 40,
                rows: 6,
                scrollback: 100,
                adopt: false,
                local: true,
                expect_host: None,
            },
            |_| {},
        )
        .expect("attach existing");

        assert_eq!(s.addr(), addr, "a pinned attach binds the session it named");
        std::thread::sleep(Duration::from_millis(200));
        s.write(b"echo pinned-round-trip\n".to_vec());
        assert!(
            wait_for(|| s.terminal().lock().screen_text().contains("pinned-round-trip")),
            "the named session never echoed; grid was:\n{}",
            s.terminal().lock().screen_text()
        );
    }

    #[test]
    fn kill_ends_the_session_rather_than_detaching() {
        // Closing a *local* tab means the shell should die — the opposite of
        // the drop path, and the reason `kill` consumes self: the
        // CloseSession frame must reach the socket before the process moves
        // on, which the writer join in Drop guarantees.
        let h = Harness::start("kill");
        let s = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.registry.len(), 1);

        s.kill();
        assert!(
            wait_for(|| h.registry.is_empty()),
            "CloseSession never removed the session from the registry"
        );
    }

    #[test]
    fn a_pinned_session_that_died_reports_gone_and_does_not_respawn() {
        // The AdoptOrCreate supervisor answers a missing session by making a
        // new one — right for "give me a shell", wrong for a labeled tab. The
        // pinned supervisor must say SessionGone and stop.
        let gone = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&gone);

        let h = Harness::start("gone");
        let seed = h.attach("/bin/sh", |_| {});
        std::thread::sleep(Duration::from_millis(300));
        let addr = seed.addr();
        drop(seed);
        std::thread::sleep(Duration::from_millis(200));

        let link = Link::new(&h.path, "gone");
        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let s = RemoteSession::attach_existing(
            dial_to(&link.path),
            addr,
            &AttachOptions {
                identity: &identity,
                label: "test",
                command: "",
                cols: 40,
                rows: 6,
                scrollback: 100,
                adopt: false,
                local: true,
                expect_host: None,
            },
            move |w| {
                if matches!(w, Wakeup::SessionGone(_)) {
                    flag.store(true, Ordering::Release);
                }
            },
        )
        .expect("attach existing");
        std::thread::sleep(Duration::from_millis(200));

        // Cut the link, then end the session while the supervisor is inside
        // its redial backoff — the direct registry call finishes long before
        // the first redial at +200ms.
        link.cut();
        h.registry.close(addr.session);

        assert!(
            wait_for(|| gone.load(Ordering::Acquire)),
            "the pinned supervisor never reported SessionGone"
        );
        assert_eq!(
            h.registry.len(),
            0,
            "a pinned tab must not respawn a shell for a session that ended"
        );
        drop(s);
    }

    fn opts_pinned<'a>(
        identity: &'a Arc<ClientIdentity>,
        expect_host: Option<zest_proto::HostId>,
    ) -> AttachOptions<'a> {
        AttachOptions {
            identity,
            label: "test",
            command: "/bin/sh",
            cols: 40,
            rows: 6,
            scrollback: 100,
            adopt: false,
            local: true,
            expect_host,
        }
    }

    #[test]
    fn a_wrong_expected_host_is_refused_before_anything_is_created() {
        // The picker dials addresses learned from advertisements, which are
        // claims. The host signs first precisely so the client can hang up
        // on a machine that is not the one the roster named — before a
        // session exists, before a keystroke could go anywhere.
        let h = Harness::start("pin-wrong");
        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let wrong = zest_proto::HostId::from_bytes([9; 32]);
        let result =
            RemoteSession::attach(dial_to(&h.path), &opts_pinned(&identity, Some(wrong)), |_| {});
        let err = result.err().expect("an imposter host must be refused");
        assert!(matches!(err, RemoteError::Refused(_)), "expected Refused, got {err:?}");
        assert_eq!(
            h.registry.len(),
            0,
            "refusal must happen before any session is created"
        );
    }

    #[test]
    fn the_right_expected_host_attaches_normally() {
        // The pin is a check, not a tax: dialling the machine the roster
        // actually named works exactly like an unpinned attach.
        let h = Harness::start("pin-right");
        let identity = Arc::new(ClientIdentity::generate().expect("client key"));
        let expected = h.host_identity.host_id();
        let s = RemoteSession::attach(
            dial_to(&h.path),
            &opts_pinned(&identity, Some(expected)),
            |_| {},
        )
        .expect("a correctly pinned attach succeeds");
        assert!(
            wait_for(|| h.registry.len() == 1),
            "the pinned attach must create its session"
        );
        drop(s);
    }
}
