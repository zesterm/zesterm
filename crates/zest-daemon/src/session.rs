//! A session the daemon owns.
//!
//! The pty, the terminal it feeds, and one encoder per attached client.
//!
//! # The property this exists for
//!
//! **A session outlives every client attached to it.** Detaching stops the
//! updates and nothing else: the shell keeps running, output keeps accumulating,
//! and reattaching from anywhere resumes it. Close the lid on the laptop and
//! pick the same shell up on a phone. → ADR-007.
//!
//! That is why nothing here holds a reference to a connection, and why
//! [`Session::detach`] does not touch the child.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zest_core::{ChangeSource, Modes, Terminal, TermEvent};
use zest_proto::delta::CursorState;
use zest_proto::delta::Progress;
use zest_proto::{AttentionCause, AttrDef, Delta, Encoder, Keyframe, RowPayload, SessionId};
use zest_pty::{CommandSpec, PtySize, PtyTransport};

use crate::DaemonError;

/// Milliseconds since the Unix epoch — the block-timestamp clock.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// How much the parser consumes per lock acquisition.
///
/// The same bound the app uses, for the same reason: without it a single
/// `advance` of a huge read holds the lock long enough to starve everything
/// waiting on it.
const PARSE_CHUNK: usize = 64 * 1024;

/// One client's view of a session.
struct Subscriber {
    encoder: Encoder,
    /// Sequence of the last update this subscriber was *sent*.
    ///
    /// The `base` of the next one, and the only thing the delta chain depends
    /// on — the encoder's shadow holds exactly the state this names, so a delta
    /// is a difference from it whether or not the client ever confirmed it.
    sent: u64,
    /// Highest sequence the client has confirmed *applying*.
    ///
    /// Deliberately a second number rather than the same one. Conflating them
    /// is what the daemon used to do, and it amounts to the host asserting that
    /// everything it wrote was applied — which is false for exactly the client
    /// that matters, the one that died mid-write.
    ///
    /// Nothing in the delta chain reads this: correctness rests on `base` being
    /// on the wire and the client refusing what it cannot apply. What this
    /// buys is evidence — how far behind a client is, and whether it is
    /// claiming to hold something that was never sent.
    acked: u64,
    /// Force a keyframe on the next poll.
    ///
    /// Set by `RequestKeyframe`, and by an acknowledgement of a sequence this
    /// subscriber was never sent.
    needs_keyframe: bool,
    /// The session asked to be noticed and this subscriber has not been told.
    ///
    /// One slot, last-wins, deliberately: attention is not a log. A shell
    /// ringing forty times while nobody polls is still one thing to look at,
    /// and a queue here would be an unbounded buffer fed by the remote end of
    /// a pty.
    pending_attention: Option<AttentionCause>,
    /// Progress as this subscriber was last told it.
    ///
    /// A shadow rather than a pending slot, because progress is *state*: a
    /// build ticking 1..100 owes one message per change and none while it sits
    /// still, and a subscriber that attaches at 60% has to be told 60 rather
    /// than nothing. Starting at `None` is what makes that second half work
    /// with no special case — the first comparison differs, so the first poll
    /// says so.
    sent_progress: Progress,
    /// Called when this subscriber has something to collect.
    ///
    /// Without it a connection blocked in `read` never learns that the shell
    /// produced output, so a client that attaches and then goes quiet -- which
    /// is every client, most of the time -- sees nothing again. Polling on a
    /// timer would fix it and cost the 0%-idle guarantee.
    wake: Box<dyn Fn() + Send>,
    /// The size this client asked to render at.
    ///
    /// A vote, not a grant: the session's size is the smallest attached
    /// client, so every viewer sees a complete screen (#215). `None` never
    /// constrains -- a subscriber that only watches has no pane to protect.
    size: Option<(u16, u16)>,
}

/// The floor a declared size is held to.
///
/// Matches the web client's own floor, and `PtySize` must never see zero.
fn clamp_size((cols, rows): (u16, u16)) -> (u16, u16) {
    (cols.max(2), rows.max(1))
}

/// A running shell, and everyone watching it.
pub struct Session {
    pub id: SessionId,
    terminal: Arc<Mutex<Terminal>>,
    pty: Arc<dyn PtyTransport + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    subscribers: Arc<Mutex<HashMap<u64, Subscriber>>>,
    next_subscriber: Mutex<u64>,
    exited: Arc<AtomicBool>,
    /// Whether anyone has ever attached.
    ///
    /// Sweeping keys on this and not only on "nobody is attached now", because
    /// creating a session and attaching to it are two round trips. A short
    /// command exits in between, and a session collected in that window leaves
    /// the client that just created it holding an address the host has already
    /// forgotten -- it gets "no session" for a shell it asked for and that ran
    /// perfectly.
    ever_attached: Arc<AtomicBool>,
    /// The child's status, once anyone has asked for it and it was available.
    ///
    /// Memoized because `Connection::poll` asks on every pass for as long as an
    /// exited session stays attached, and the answer cannot change once it
    /// exists. The transport is what guarantees the *ask* is cheap — it may not
    /// block, and answers `None` rather than waiting — so this is about not
    /// re-doing settled work, not about avoiding a stall.
    exit_code: Arc<Mutex<Option<i32>>>,
    title: Arc<Mutex<String>>,
}

impl Session {
    /// Spawn a shell and start pumping it.
    ///
    /// `wake` fires when there is something new to send. It is called on the
    /// *transition* to dirty, not per byte, so a flood is one wakeup rather
    /// than millions.
    pub fn spawn(
        id: SessionId,
        cmd: &CommandSpec,
        size: PtySize,
        scrollback: usize,
        wake: impl Fn(SessionId) + Send + Sync + 'static,
    ) -> Result<Self, DaemonError> {
        let mut pty = zest_pty::NativePty::spawn(cmd, size)
            .map_err(|e| DaemonError::Spawn(e.to_string()))?;
        let mut reader = pty.take_reader().expect("a fresh pty always has a reader");
        let writer = pty.writer();

        let mut term = Terminal::new(size.cols as usize, size.rows as usize, scrollback);
        // A restating pty has the last word on the viewport, which changes what
        // the grid may put there when it grows. (#200)
        term.set_pty_restates_viewport(pty.restates_on_resize());
        let terminal = Arc::new(Mutex::new(term));
        let exited = Arc::new(AtomicBool::new(false));
        let title = Arc::new(Mutex::new(String::new()));

        let subscribers: Arc<Mutex<HashMap<u64, Subscriber>>> = Arc::default();
        // Shared because two things now report the child leaving: the reader
        // reaching EOF, and -- on Windows, where it cannot -- the process
        // watcher below.
        let wake = Arc::new(wake);

        {
            let wake = Arc::clone(&wake);
            let terminal = Arc::clone(&terminal);
            let exited = Arc::clone(&exited);
            let title = Arc::clone(&title);
            let subscribers = Arc::clone(&subscribers);
            let mut reply = pty.writer();

            std::thread::Builder::new()
                .name(format!("zest-daemon-session-{}", id.0))
                .spawn(move || {
                    let mut buf = vec![0u8; PARSE_CHUNK];
                    loop {
                        // `Err(_) => break` here treated a signal as the end of the
                        // stream, which closes a healthy peer or ends a live shell.
                        let n = match crate::read_retrying(&mut reader, &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };

                        let events = {
                            let Ok(mut term) = terminal.lock() else { break };
                            // The parser has no clock (`no_std`); the reader
                            // is where wall time and bytes meet, so blocks
                            // get their start/end stamps from here.
                            term.set_now_ms(unix_ms());
                            term.advance(&buf[..n]);
                            term.take_events()
                        };

                        for event in events {
                            match event {
                                // Replies go straight back. Dropping them hangs
                                // whatever asked -- a DSR or an OSC 11 query
                                // waits forever for an answer it will not get.
                                TermEvent::Reply(bytes) => {
                                    let _ = reply.write_all(&bytes);
                                    let _ = reply.flush();
                                }
                                TermEvent::Title(t) => {
                                    if let Ok(mut slot) = title.lock() {
                                        *slot = t;
                                    }
                                }
                                // The repaint that answered a grow has closed
                                // and the grid gave back the rows the shrink
                                // displaced, so the viewport/scrollback boundary
                                // moved. No delta can say that -- there is no
                                // `DeltaOp::Resize`, on purpose (`CONTRACTS.md`)
                                // -- and a client applying deltas over it would
                                // hold rows the host now calls visible while
                                // still filing them as history. (#247)
                                TermEvent::ViewportRebased => {
                                    keyframe_everyone(&subscribers);
                                }
                                // ED 3 destroyed scrollback. Same argument one
                                // notch further: the rows are not damaged, they
                                // are gone, and the keyframe also carries
                                // `history_clears` so a client that misses this
                                // one still learns on its next. (#314)
                                TermEvent::HistoryCleared => {
                                    keyframe_everyone(&subscribers);
                                }
                                // Every subscriber is told, and the connection
                                // decides whether its client asked. The session
                                // has no idea who is watching it, which is the
                                // same reason it does not try to remember who
                                // has *seen* one.
                                TermEvent::Attention { cause } => {
                                    tell_everyone(&subscribers, cause.into());
                                }
                                _ => {}
                            }
                        }

                        wake(id);
                        wake_subscribers(&subscribers);
                        std::thread::yield_now();
                    }

                    exited.store(true, Ordering::Release);
                    wake(id);
                    wake_subscribers(&subscribers);
                })
                .map_err(|e| DaemonError::Spawn(e.to_string()))?;
        }

        let pty: Arc<dyn PtyTransport + Send + Sync> = Arc::new(pty);

        // Windows only, in effect: there the reader can never observe the child
        // exiting, because ConPTY holds the output pipe's write end until the
        // pseudoconsole closes. Without this a shell that exits on its own is
        // never noticed -- no `Exited` reaches any client and the session is
        // kept forever. On unix the reader's EOF already says it and this does
        // nothing. See `PtyTransport::watch_exit`.
        {
            let exited = Arc::clone(&exited);
            let subscribers = Arc::clone(&subscribers);
            let wake = Arc::clone(&wake);
            pty.watch_exit(Box::new(move || {
                exited.store(true, Ordering::Release);
                wake(id);
                wake_subscribers(&subscribers);
            }));
        }

        Ok(Self {
            id,
            terminal,
            pty,
            writer: Arc::new(Mutex::new(writer)),
            subscribers,
            next_subscriber: Mutex::new(0),
            exited,
            ever_attached: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(Mutex::new(None)),
            title,
        })
    }

    /// Begin watching. Returns the subscriber handle and the state to start from.
    ///
    /// Every attach starts with a keyframe: a client that has just connected has
    /// no base for a delta, and one that is reattaching after an hour asleep is
    /// indistinguishable from a new one.
    pub fn attach(&self) -> (u64, u64, Keyframe) {
        self.attach_with(Box::new(|| {}), None)
    }

    /// Attach, and be told when there is something to collect.
    ///
    /// Returns `(handle, seq, keyframe)`. The sequence is what the keyframe
    /// describes and what the client will acknowledge; without it on the wire a
    /// client has no baseline to compare the next update's `base` against.
    ///
    /// `size` is what this client renders at -- a vote in the arbitration, not
    /// a command (#215). The keyframe is built *after* the vote is counted, so
    /// it carries the granted size whether or not that equals the ask.
    pub fn attach_with(
        &self,
        wake: Box<dyn Fn() + Send>,
        size: Option<(u16, u16)>,
    ) -> (u64, u64, Keyframe) {
        self.ever_attached.store(true, Ordering::Release);
        let handle = {
            let mut next = self.next_subscriber.lock().expect("counter lock");
            let h = *next;
            *next += 1;
            h
        };

        let mut subs = self.subscribers.lock().expect("subscriber lock");
        subs.insert(
            handle,
            Subscriber {
                encoder: Encoder::new(),
                sent: 0,
                acked: 0,
                needs_keyframe: false,
                pending_attention: None,
                sent_progress: Progress::None,
                wake,
                size: size.map(clamp_size),
            },
        );
        self.reconcile_size(&mut subs, Some(handle));

        let sub = subs.get_mut(&handle).expect("just inserted");
        let term = self.terminal.lock().expect("terminal lock");
        let seq = ChangeSource::seq(&*term);
        let title = self.title.lock().map(|t| t.clone()).unwrap_or_default();
        let keyframe =
            sub.encoder.keyframe(term.grid(), cursor_of(&term), term.modes(), &title, term.blocks());
        sub.sent = seq;
        sub.acked = seq;

        (handle, seq, keyframe)
    }

    /// Record what `handle` now renders at, and re-arbitrate.
    ///
    /// Answers `ClientMessage::Resize`. Returns whether the session's
    /// effective size changed. The caller is *not* exempt from the keyframe
    /// push: the web client sends no `RequestKeyframe` after a resize and
    /// relies on it when its own shrink is granted.
    pub fn set_client_size(&self, handle: u64, cols: u16, rows: u16) -> bool {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let Some(sub) = subs.get_mut(&handle) else { return false };
        sub.size = Some(clamp_size((cols, rows)));
        self.reconcile_size(&mut subs, None)
    }

    /// Recompute the arbitrated size and apply it.
    ///
    /// The session's size is the smallest attached client -- min cols and min
    /// rows over every subscriber that declared one -- recomputed on attach,
    /// resize and detach, so every viewer sees a complete screen (#215).
    /// Larger viewers letterbox; that is the clients' side of the deal.
    ///
    /// On a change every subscriber except `exempt` is marked for a keyframe
    /// and woken. A keyframe and not a delta, because a delta describing a
    /// *smaller* grid lands entirely inside a stale larger one without ever
    /// tripping `NeedsKeyframe` (see `zest_proto::apply`) -- only a full state
    /// can tell a client whose own pane never changed that the session is a
    /// different shape. `exempt` is the attach path's fresh subscriber, whose
    /// keyframe its caller builds right after this.
    ///
    /// Equal-size recomputes touch nothing at all: a pty resize is a ConPTY
    /// repaint on Windows (#200), so it must not happen on every attach.
    fn reconcile_size(&self, subs: &mut HashMap<u64, Subscriber>, exempt: Option<u64>) -> bool {
        let Some(want) =
            subs.values().filter_map(|s| s.size).reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        else {
            // Nobody declared a size. The last detach lands here: the session
            // outlives its clients and gets no parting resize.
            return false;
        };
        if want == self.size() {
            return false;
        }
        // The resize happens under the subscribers lock on purpose. Released
        // between computing `want` and applying it, the min goes stale the
        // moment another attach or resize interleaves, and two racing resizes
        // can land on the pty out of order -- the grid then disagrees with
        // the votes until the next change. The cost is one bounded syscall at
        // human cadence blocking concurrent polls for its duration; the
        // serialization is not incidental, it *is* the arbitration.
        self.resize(want.0, want.1);
        for (h, sub) in subs.iter_mut() {
            if Some(*h) == exempt {
                continue;
            }
            sub.needs_keyframe = true;
            (sub.wake)();
        }
        true
    }

    /// Rebuild a complete state for a subscriber that cannot apply what it has.
    ///
    /// Answers `ClientMessage::RequestKeyframe`. The subscriber's shadow is
    /// replaced by what the keyframe describes, so the *next* delta is a
    /// difference from a state the client demonstrably holds — which is the
    /// whole point, and the reason this cannot be done by simply re-encoding.
    ///
    /// Before this existed a client with a dropped frame had to detach and
    /// reattach, tearing down a subscriber to recover from one lost message.
    pub fn keyframe_for(&self, handle: u64) -> Option<(u64, Keyframe)> {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let sub = subs.get_mut(&handle)?;
        let term = self.terminal.lock().expect("terminal lock");
        let seq = ChangeSource::seq(&*term);
        let title = self.title.lock().expect("title lock").clone();
        let k = sub.encoder.keyframe(term.grid(), cursor_of(&term), term.modes(), &title, term.blocks());
        // A keyframe *is* the new baseline: the encoder's shadow now holds
        // exactly what was sent, so the next delta is a difference from it.
        sub.sent = seq;
        sub.needs_keyframe = false;
        Some((seq, k))
    }

    /// Stop watching.
    ///
    /// **Does not touch the child.** A session whose last client left keeps
    /// running; that is the entire point of the daemon owning it.
    pub fn detach(&self, handle: u64) {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        subs.remove(&handle);
        // The remaining clients may have been held down by the one that left;
        // give them their space back. The *last* detach changes nothing --
        // see `reconcile_size`.
        self.reconcile_size(&mut subs, None);
    }

    /// Whether anyone is watching.
    #[must_use]
    pub fn attached(&self) -> bool {
        !self.subscribers.lock().expect("subscriber lock").is_empty()
    }

    /// End the child, and let the reader thread finish.
    ///
    /// The opposite of [`detach`](Self::detach), and the only thing in this
    /// module that deliberately kills anything. Blocking, and bounded by the
    /// transport's own escalation.
    pub fn hangup(&self) {
        self.pty.hangup();
    }

    /// What this subscriber has not yet seen, and the sequences that name it.
    ///
    /// `None` when it is caught up — an idle terminal generates no traffic, the
    /// network counterpart of drawing no frames.
    ///
    /// Returns `(base, seq, update)`: `base` is the state the update is a
    /// difference *from*, `seq` the state it produces. A client whose own
    /// applied sequence is not `base` must discard the update and resync, which
    /// is the whole reason both numbers are on the wire.
    pub fn poll(&self, handle: u64) -> Option<(u64, u64, Update)> {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let sub = subs.get_mut(&handle)?;

        let term = self.terminal.lock().expect("terminal lock");
        let seq = ChangeSource::seq(&*term);
        if seq == sub.sent && !sub.needs_keyframe {
            return None;
        }

        let cursor = cursor_of(&term);
        let modes = term.modes();
        let title = self.title();
        let base = sub.sent;

        // `update_for` is asked about `sent`, not `acked`. The encoder's shadow
        // holds the state named by `sent`, so that is what a delta can be a
        // difference from. Asking about `acked` -- which lags by however long a
        // round trip takes -- would push a busy session past the keyframe
        // threshold and turn every frame into a full repaint.
        // A delta can add and update blocks, never remove one. Eviction is
        // silently absent on purpose -- a client keeping more history than the
        // host should keep it -- but destruction is not eviction: `cls` erases
        // the rows a block described, and a client left holding it paints a
        // stale header over the row the user is typing on. Resync instead. The
        // whole screen just changed, so a keyframe costs nothing extra.
        let out = if sub.needs_keyframe || sub.encoder.blocks_need_keyframe(term.blocks()) {
            sub.needs_keyframe = false;
            Update::Keyframe(sub.encoder.keyframe(term.grid(), cursor, modes, &title, term.blocks()))
        } else {
            match ChangeSource::update_for(&*term, sub.sent) {
                zest_core::Update::Idle => return None,
                // Far enough behind that the delta chain would exceed the state
                // it describes. Normal after a sleep, not an error.
                zest_core::Update::Keyframe { .. } => {
                    Update::Keyframe(sub.encoder.keyframe(term.grid(), cursor, modes, &title, term.blocks()))
                }
                zest_core::Update::Delta { .. } => {
                    let d = sub.encoder.delta(term.grid(), cursor, modes, &title, term.blocks());
                    if d.ops.is_empty() && d.attrs.is_empty() && d.blocks.is_empty() {
                        // The sequence moved but nothing observable changed -- a
                        // cursor save, a mode the client already has. Advance
                        // the baseline and send nothing.
                        //
                        // `blocks` belongs in this test, not outside it: a
                        // command that finished having printed nothing new
                        // changes only its block, and dropping that batch would
                        // leave the client showing it as running forever.
                        sub.sent = seq;
                        return None;
                    }
                    Update::Delta(d)
                }
            }
        };

        sub.sent = seq;
        Some((base, seq, out))
    }

    /// What this subscriber has not been told about the session's progress.
    ///
    /// `None` when it is already up to date, which is every poll of a session
    /// that is not running anything — the 0%-idle rule applied to a second
    /// kind of traffic.
    ///
    /// **Subscribers first, then the terminal** — the order `poll` takes, and
    /// the only reason to care is that taking them the other way round is a
    /// deadlock waiting for two connections to poll the same session at once.
    /// It happens not to be one today, because the terminal guard would fall
    /// out of scope before the second lock is asked for; a rule that holds by
    /// where a brace sits is one an edit breaks silently.
    pub fn progress_for(&self, handle: u64) -> Option<Progress> {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let sub = subs.get_mut(&handle)?;
        let now: Progress = self.terminal.lock().expect("terminal lock").progress().into();
        (sub.sent_progress != now).then(|| {
            sub.sent_progress = now;
            now
        })
    }

    /// Take this subscriber's pending signal, if it has one.
    ///
    /// Separate from [`Self::poll`] because it is not an update: an idle
    /// session that rings produces no delta at all, and folding the two would
    /// mean a bell only arrived behind output that may never come.
    pub fn take_attention(&self, handle: u64) -> Option<AttentionCause> {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        subs.get_mut(&handle)?.pending_attention.take()
    }

    /// Record what a client says it has applied.
    ///
    /// An acknowledgement of something never sent means the client is talking
    /// about a different session — a daemon that restarted under it, or state
    /// kept across a host it should not have. There is nothing to rewind to,
    /// because the encoder keeps a shadow rather than a history, so the honest
    /// answer is a full state.
    pub fn ack(&self, handle: u64, seq: u64) {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let Some(sub) = subs.get_mut(&handle) else { return };
        if seq > sub.sent {
            tracing::warn!(
                session = self.id.0,
                acked = seq,
                sent = sub.sent,
                "client acknowledged a sequence it was never sent; resending the state"
            );
            sub.needs_keyframe = true;
            return;
        }
        sub.acked = sub.acked.max(seq);
    }

    /// How far behind a subscriber's confirmations are, in sequence numbers.
    ///
    /// For diagnostics: a number that keeps growing is a client that is
    /// receiving and not applying, which looks identical to a healthy one from
    /// every other angle.
    #[must_use]
    pub fn ack_lag(&self, handle: u64) -> Option<u64> {
        let subs = self.subscribers.lock().expect("subscriber lock");
        subs.get(&handle).map(|s| s.sent.saturating_sub(s.acked))
    }

    /// History this client does not hold, oldest first.
    ///
    /// Answers `RequestScrollback`. Bounded by what the host still has: a phone
    /// that was asleep for an hour may ask for lines this session evicted long
    /// ago, and the honest answer is the part that survives rather than an
    /// error.
    pub fn scrollback(&self, from_line: u64, count: usize) -> (Vec<RowPayload>, Vec<AttrDef>) {
        let term = self.terminal.lock().expect("terminal lock");
        // A fresh encoder, so the payloads are self-contained -- see
        // `Encoder::history` for why a subscriber's must not be used.
        let mut enc = Encoder::new();
        let rows = term.grid().lines_by_id(from_line, count);
        enc.history(&rows)
    }

    /// Send bytes to the child.
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Ok(mut w) = self.writer.lock() else { return };
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    /// Resize the grid and the pty together, **grid first**.
    ///
    /// Both, always: a grid that disagrees with what the shell believes produces
    /// output wrapped for a screen that does not exist.
    ///
    /// The order is load-bearing on Windows. `ResizePseudoConsole` is answered
    /// by restating the entire viewport -- `ESC[?25l`, the new size, `ESC[H`,
    /// then every row rewritten in place and terminated with `ESC[K` (#205) --
    /// and the reader thread parses that under this same lock. Told first, the
    /// pty can have a repaint laid out for the *new* size parsed into a grid
    /// still at the *old* one, and nothing afterwards recovers: the rows were
    /// overwritten in place while their line ids never moved, so the reflow is
    /// correct, the re-anchor is correct, and every block still ends up naming
    /// text that is not its own. (#200)
    ///
    /// The lock is *released* before the pty call rather than held across it,
    /// and that is enough: the repaint cannot exist before the call that causes
    /// it, and the release/acquire pairs the two threads. Holding it is the
    /// stronger-looking option and deadlocks -- the reader cannot drain what
    /// ConPTY is writing while it waits for this lock, and a ConPTY that cannot
    /// write is `ClosePseudoConsole` in another costume (`zest-pty`, gotcha 2b).
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(mut term) = self.terminal.lock() {
            term.resize(cols as usize, rows as usize);
        }
        if let Err(e) = self.pty.resize(PtySize::new(cols, rows)) {
            tracing::warn!(session = self.id.0, error = %e, "pty resize failed");
        }
    }

    /// Whether any client has ever attached to this session.
    ///
    /// See the field: this is what keeps a session alive across the gap between
    /// `CreateSession` and `Attach`.
    #[must_use]
    pub fn ever_attached(&self) -> bool {
        self.ever_attached.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// What the child exited with, if it has and the platform could say.
    ///
    /// `None` is "no status to report" — still running, or a transport that
    /// cannot determine one — and is deliberately not zero. That distinction
    /// survives all the way to the wire, because a green `exit 0` on a command
    /// that actually failed is worse than admitting ignorance.
    ///
    /// **This is the one exit status in the system nobody can forge.** A
    /// block's `exit_code` comes from OSC 133;D, which any program can print;
    /// this is read from the process. Anything reporting the two to an agent
    /// has to say which it is holding.
    ///
    /// Distinct from [`has_exited`](Self::has_exited), which the reader sets on
    /// EOF: seeing EOF is not the same as having waited on the process, and
    /// there is a brief window where the first is true and this still answers
    /// `None`.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        if let Ok(cached) = self.exit_code.lock() {
            if cached.is_some() {
                return *cached;
            }
        }
        let code = self.pty.exit_code();
        if code.is_some() {
            if let Ok(mut slot) = self.exit_code.lock() {
                *slot = code;
            }
        }
        code
    }

    #[must_use]
    pub fn title(&self) -> String {
        self.title.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Size in cells, as the terminal currently holds it.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let term = self.terminal.lock().expect("terminal lock");
        (
            u16::try_from(term.grid().cols()).unwrap_or(u16::MAX),
            u16::try_from(term.grid().rows()).unwrap_or(u16::MAX),
        )
    }

    /// Where the shell says it is, from OSC 7.
    ///
    /// Empty until a shell reports one, which is what shell integration
    /// installs. Not guessed from the child's process state: that answers where
    /// the *process* is, which stops being where the next command runs the
    /// moment a subshell is involved.
    #[must_use]
    pub fn cwd(&self) -> String {
        self.terminal.lock().map(|t| t.cwd().to_string()).unwrap_or_default()
    }

    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.terminal
            .lock()
            .map(|t| t.modes().contains(Modes::ALT_SCREEN))
            .unwrap_or(false)
    }
}

/// Tell every subscriber there is something new.
///
/// Called from the session's reader thread, so it must not block: the wakers
/// are channel sends, and a client that has gone away simply fails to receive.
fn wake_subscribers(subscribers: &Mutex<HashMap<u64, Subscriber>>) {
    if let Ok(subs) = subscribers.lock() {
        for sub in subs.values() {
            (sub.wake)();
        }
    }
}

/// Owe every subscriber a full state rather than a delta.
///
/// For a change no delta can describe. The one there is today is the viewport
/// giving back the rows a shrink displaced once a restating pty's repaint has
/// closed (`TermEvent::ViewportRebased`): rows that were history are on screen,
/// and a client applying deltas over that would hold the same lines twice —
/// once in its own scrollback, once in the viewport — which is what
/// `sliceBlocks` and every other id-ordered walk assume cannot happen. There is
/// no `DeltaOp::Resize` on purpose; `docs/CONTRACTS.md` has the argument. (#247)
fn keyframe_everyone(subscribers: &Mutex<HashMap<u64, Subscriber>>) {
    if let Ok(mut subs) = subscribers.lock() {
        for sub in subs.values_mut() {
            sub.needs_keyframe = true;
        }
    }
}

/// Hand every current subscriber the same signal.
///
/// Current, not future: a client that attaches after the bell is never told,
/// which is the right answer for something that means "look at this now".
fn tell_everyone(subscribers: &Mutex<HashMap<u64, Subscriber>>, cause: AttentionCause) {
    if let Ok(mut subs) = subscribers.lock() {
        for sub in subs.values_mut() {
            sub.pending_attention = Some(cause);
        }
    }
}

/// What a subscriber should be sent.
#[derive(Debug, Clone)]
pub enum Update {
    Delta(Delta),
    Keyframe(Keyframe),
}

fn cursor_of(term: &Terminal) -> CursorState {
    let c = term.cursor();
    CursorState {
        row: u16::try_from(c.row).unwrap_or(0),
        col: u16::try_from(c.col).unwrap_or(0),
        visible: term.modes().contains(Modes::SHOW_CURSOR),
        // The real style, not 0. A program that asked for a bar cursor with
        // DECSCUSR got a block on every remote client, which reads as the
        // terminal ignoring the escape rather than as the wire dropping it.
        shape: u8::try_from(term.cursor_style().to_decscusr()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The reader trait is only named by the fakes below; `read_retrying` is
    // what the module itself reads through.
    use std::io::Read;
    use std::time::{Duration, Instant};

    /// A command that prints `text` and exits, on either platform.
    fn echo(text: &str) -> CommandSpec {
        let command_line = if cfg!(windows) {
            format!("cmd.exe /c echo {text}")
        } else {
            format!("/bin/echo {text}")
        };
        CommandSpec { command_line, cwd: None, env: zest_pty::terminal_env() }
    }

    fn session(text: &str) -> Session {
        Session::spawn(SessionId(1), &echo(text), PtySize::new(80, 24), 100, |_| {})
            .expect("spawn")
    }

    /// Wait for the child to produce something, or give up.
    ///
    /// Polling with a deadline rather than sleeping a fixed time: a fixed sleep
    /// is either flaky on a loaded machine or slow on an idle one.
    ///
    /// The deadline is generous because it is only ever paid by a run that was
    /// going to fail: a condition that holds returns immediately. Ten seconds
    /// was not enough — on `test (windows-latest)` the scrollback test below
    /// failed twice in a row roughly ten seconds after it started, which is
    /// this returning `false` for `has_exited` on a PowerShell child that a
    /// runner busy building the rest of the workspace had not finished
    /// starting (#80). Anything that genuinely needs half a minute here is a
    /// hang, and reports as one.
    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A transport that records what the grid looked like when it was resized.
    ///
    /// `PtyTransport` reports no size of its own and the thing under test is an
    /// *order*, so the probe reads the size off the terminal the session shares
    /// with it.
    struct SizeProbePty {
        terminal: Arc<Mutex<Terminal>>,
        /// `None` until a resize arrives. `Some(None)` means the terminal lock
        /// was held at that moment, which is a different failure -- see the
        /// assertion.
        seen: Mutex<Option<Option<(usize, usize)>>>,
    }

    impl PtyTransport for SizeProbePty {
        fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
            None
        }
        fn writer(&self) -> Box<dyn Write + Send> {
            Box::new(std::io::sink())
        }
        fn hangup(&self) {}

        fn resize(&self, _size: PtySize) -> Result<(), zest_pty::PtyError> {
            // `try_lock`, never `lock`. A "fix" that held the terminal lock
            // across the pty call would deadlock right here, and a test that
            // hangs says nothing -- this way that ordering is a named failure.
            let seen =
                self.terminal.try_lock().ok().map(|t| (t.grid().cols(), t.grid().rows()));
            *self.seen.lock().expect("probe") = Some(seen);
            Ok(())
        }
    }

    #[test]
    fn the_grid_is_resized_before_the_pty_is_told() {
        // ConPTY answers `ResizePseudoConsole` by restating the whole viewport,
        // laid out for the size it was just given (#205). Those bytes arrive on
        // the reader thread, which parses them under the terminal lock -- so
        // telling the pty first opens a window in which a repaint for the NEW
        // size is parsed into a grid still at the OLD one.
        //
        // Nothing recovers from that afterwards, which is why the order is the
        // fix rather than a tidiness: the repaint overwrites rows *in place*
        // while their line ids stay where they were, so the reflow renumbers
        // correctly, `BlockIndex::reanchor` maps correctly, and every block
        // still ends up naming text that is no longer its own -- a listing
        // split across two cards, the live prompt swallowed into a finished
        // block, the trailing block covering no rows at all. (#200)
        let terminal = Arc::new(Mutex::new(Terminal::new(80, 24, 100)));
        let probe = Arc::new(SizeProbePty {
            terminal: Arc::clone(&terminal),
            seen: Mutex::new(None),
        });
        let s = Session {
            id: SessionId(1),
            terminal: Arc::clone(&terminal),
            pty: Arc::clone(&probe) as Arc<dyn PtyTransport + Send + Sync>,
            writer: Arc::new(Mutex::new(Box::new(std::io::sink()) as Box<dyn Write + Send>)),
            subscribers: Arc::default(),
            next_subscriber: Mutex::new(0),
            exited: Arc::new(AtomicBool::new(false)),
            ever_attached: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(String::new())),
        };

        s.resize(40, 12);

        let seen = *probe.seen.lock().expect("probe");
        match seen {
            Some(Some(size)) => assert_eq!(
                size,
                (40, 12),
                "the pty was told to resize while the grid was still {size:?}; the repaint \
                 ConPTY sends back is laid out for 40x12 and would be parsed at that width"
            ),
            Some(None) => panic!(
                "the terminal lock was held across the pty resize -- the reader thread \
                 cannot drain ConPTY's repaint until it is released, which is the \
                 `ClosePseudoConsole` deadlock in another costume"
            ),
            None => panic!("the pty was never resized"),
        }
    }

    #[test]
    fn a_session_runs_a_command_and_a_client_sees_its_output() {
        let s = session("daemon-probe");
        let (handle, _, _) = s.attach();

        assert!(
            wait_for(|| s.poll(handle).is_some()),
            "nothing was ever sent to the subscriber"
        );
    }

    #[test]
    fn a_bell_from_a_real_child_reaches_every_subscriber_exactly_once() {
        // Driven through a real pty rather than injected into the subscriber
        // map, because the seam a helper would skip -- `TermEvent::Attention`
        // reaching `tell_everyone` from the reader thread -- is precisely the
        // half that can stop working while everything either side of it still
        // passes.
        let s = Session::spawn(
            SessionId(1),
            &echo("\u{7}"),
            PtySize::new(80, 24),
            100,
            |_| {},
        )
        .expect("spawn");
        let (a, _, _) = s.attach();
        let (b, _, _) = s.attach();

        let mut first = None;
        assert!(
            wait_for(|| {
                first = first.or_else(|| s.take_attention(a));
                first.is_some()
            }),
            "the child's bell never reached the subscriber"
        );
        assert_eq!(first, Some(AttentionCause::Bell));
        assert_eq!(
            s.take_attention(b),
            Some(AttentionCause::Bell),
            "every subscriber is told, not whichever asked first: two devices \
             watching one shell must both see it"
        );
        assert_eq!(s.take_attention(b), None, "and taking it is what makes it seen");
    }

    #[test]
    fn progress_is_told_once_per_change_and_to_a_late_subscriber_at_once() {
        // The property that makes this state rather than an event, and the
        // reason it is a shadow rather than a pending slot like attention's.
        let s = Session::spawn(
            SessionId(1),
            &echo("\u{1b}]9;4;1;60\u{7}"),
            PtySize::new(80, 24),
            100,
            |_| {},
        )
        .expect("spawn");
        let (a, _, _) = s.attach();

        let mut first = None;
        assert!(
            wait_for(|| {
                first = first.or_else(|| s.progress_for(a));
                first.is_some()
            }),
            "the child's progress never reached the subscriber"
        );
        assert_eq!(
            first,
            Some(Progress::At { percent: 60, state: zest_proto::delta::ProgressState::Normal })
        );
        assert_eq!(
            s.progress_for(a),
            None,
            "and nothing again until it moves -- the 0%-idle rule, applied to \
             a second kind of traffic"
        );

        // A subscriber that arrives now is behind, and must be told at once
        // rather than waiting for the next change that may never come. Its
        // shadow starts at `None`, so this needs no special case.
        let (b, _, _) = s.attach();
        assert_eq!(
            s.progress_for(b),
            Some(Progress::At {
                percent: 60,
                state: zest_proto::delta::ProgressState::Normal
            }),
            "attaching halfway through a build tells you where the build is"
        );
        assert_eq!(s.progress_for(b), None);
    }

    #[test]
    fn detaching_leaves_the_session_running() {
        // The property the whole daemon exists for. If detaching ever implied
        // stopping the shell, the fleet story is gone.
        let s = session("survives");
        let (handle, _, _) = s.attach();
        assert!(s.attached());

        s.detach(handle);
        assert!(!s.attached(), "the subscriber was not removed");

        // The session is still usable: a new client attaches and gets state.
        let (second, _, keyframe) = s.attach();
        assert!(!keyframe.rows_data.is_empty(), "reattaching produced no state");
        assert_ne!(second, handle, "handles were reused");
    }

    #[test]
    fn a_caught_up_subscriber_is_sent_nothing() {
        // The 0%-idle guarantee across the network. An idle terminal must not
        // generate traffic any more than it generates frames.
        let s = session("quiet");
        let (handle, _, _) = s.attach();

        wait_for(|| s.poll(handle).is_some());
        // Drain whatever is outstanding, then confirm it stays quiet.
        while s.poll(handle).is_some() {}
        assert!(s.poll(handle).is_none(), "an idle session kept producing updates");
    }

    /// Run a shell script, on whichever shell this platform has.
    ///
    /// These tests need a child that *writes several times*, which is the only
    /// way to produce a chain of updates rather than one. `/bin/sh` was
    /// hardcoded, so both of them failed on Windows with "The system cannot find
    /// the file specified" -- an error about a path, for a test about sequence
    /// numbers.
    fn script_cmd(sh: &str, ps: &str) -> CommandSpec {
        let command_line = if cfg!(windows) {
            format!("powershell.exe -NoProfile -Command \"{ps}\"")
        } else {
            format!("/bin/sh -c \"{sh}\"")
        };
        CommandSpec { command_line, cwd: None, env: zest_pty::terminal_env() }
    }

    #[test]
    fn every_update_chains_onto_the_last() {
        // The property a client's resync rule rests on: each update's `base` is
        // the previous update's `seq`, unbroken. A gap means the client is
        // being asked to apply a difference from a state it was never sent, and
        // it has no way to know unless both numbers are on the wire.
        //
        // Both were hardcoded to 0 before this, which made every update look
        // like a valid continuation of every other.
        //
        // Five separate writes, so there is usually a chain rather than one
        // update -- but how many arrive is deliberately not asserted beyond
        // "at least one". Demanding two, as this used to, is a claim about
        // *coalescing* rather than about the chain: the writer is a child, so
        // the test cannot interleave its polls with writes it does not
        // control, and a daemon that turns five writes into one update is
        // doing exactly what ADR-004 designed it to do -- the more so on
        // Windows, where ConPTY repaints on its own schedule and hands over
        // together what the child spaced out. `test (windows-latest)` duly
        // failed with "only 1 updates arrived" on a diff that touched no Rust
        // (#80). The property survives the collapse, because it is checked per
        // update rather than over the run: each `base` must be the sequence
        // its client was last given, which one update pins as well as five.
        let spec = script_cmd(
            "for i in 1 2 3 4 5; do printf 'line %s\\n' $i; sleep 0.05; done",
            "1..5 | ForEach-Object { Write-Host \\\"line $_\\\"; Start-Sleep -Milliseconds 50 }",
        );
        let s = Session::spawn(SessionId(5), &spec, PtySize::new(80, 24), 100, |_| {})
            .expect("spawn");

        // A throwaway subscriber first, and it is what makes a single update
        // enough to pin the chain.
        //
        // `attach_with` takes its baseline from the terminal as it stands, so a
        // session that has parsed nothing hands out `attach_seq == 0`. Then the
        // only assertion a lone update runs is `assert_eq!(base, 0)` — which a
        // `base` hardcoded to 0, the exact regression named above, satisfies.
        // Measured, not reasoned: with the baseline at 0 and the writes
        // coalesced into one update, this test passed with `let base = 0;`
        // substituted into `poll`. On macOS the writes do not coalesce, three
        // updates arrive and it fails — so the hole was open on precisely the
        // platform #80 is about.
        //
        // Draining one update through a subscriber that is then dropped leaves
        // the terminal's sequence past zero, so the real subscriber's baseline
        // is a number that can disagree with a hardcoded one.
        let (warm, _, _) = s.attach();
        assert!(
            wait_for(|| s.poll(warm).is_some()),
            "the child produced nothing, so there is no sequence to chain from"
        );
        s.detach(warm);

        // Attach *before* anything else is drained: `poll` consumes, so waiting
        // for output by polling would eat the very updates under test.
        let (handle, attach_seq, _) = s.attach();
        assert!(
            attach_seq > 0,
            "the baseline is still zero, so a `base` hardcoded to zero would look correct"
        );

        let mut previous = attach_seq;
        let mut seen = 0;
        // Drain everything outstanding on each turn, then stop once a few
        // updates have chained -- or once the child is gone and one has, since
        // waiting out the deadline for a second one that coalescing may never
        // produce is how this test used to fail.
        let settled = wait_for(|| {
            while let Some((base, seq, _)) = s.poll(handle) {
                assert_eq!(
                    base, previous,
                    "update {seen} claimed to build on {base} when the last one produced {previous}"
                );
                assert!(seq > base, "an update must advance the sequence: {base} -> {seq}");
                previous = seq;
                seen += 1;
            }
            seen >= 3 || (seen >= 1 && s.has_exited())
        });
        // Both, and the `bool` is not optional however tempting it looks.
        //
        // The condition is "three updates, or one and a finished child", and a
        // child that printed five lines does one or the other quickly. If
        // neither ever becomes true the run burns the full deadline and then —
        // with only `seen >= 1` asserted — passes, slowly, having proved
        // nothing about a session that stalled. That is the
        // waited-out-the-deadline-but-did-not-fail mode this whole change
        // exists to remove, and leaving it at one call site while fixing the
        // others is how it survives.
        assert!(seen >= 1, "no update ever arrived, so nothing above was checked");
        assert!(
            settled,
            "after {seen} update(s) the child neither produced a third nor exited, so this \
             waited out its deadline on a session that had stalled"
        );
    }

    #[test]
    fn a_requested_keyframe_names_the_sequence_it_describes() {
        // `Connection` used to send `Seq(0)` here while `keyframe_for` set
        // `sub.sent` to the real value. The client's baseline went to 0, every
        // following update was refused as stale, and each refusal asked for
        // another keyframe that again said 0 -- so a session that had resized
        // once did a full repaint round trip for every byte the shell printed,
        // forever. The screen still updated, which is why it was not obvious.
        let s = session("kf");
        let (handle, attach_seq, _) = s.attach();
        wait_for(|| s.poll(handle).is_some());

        let (seq, _k) = s.keyframe_for(handle).expect("attached");
        assert!(
            seq >= attach_seq,
            "a requested keyframe named {seq}, behind the attach at {attach_seq}"
        );

        // And the chain continues from it rather than from zero.
        let subs_sent = {
            let subs = s.subscribers.lock().expect("subscriber lock");
            subs.get(&handle).expect("attached").sent
        };
        assert_eq!(
            seq, subs_sent,
            "the keyframe told the client {seq} while the daemon recorded {subs_sent}; \
             every later update would be refused as stale"
        );
    }

    #[test]
    fn a_sequence_that_was_never_sent_is_refused_with_a_keyframe() {
        // A client acknowledging something it cannot have been sent is talking
        // about a different session -- a daemon that restarted under it. There
        // is nothing to rewind to, because the encoder keeps a shadow rather
        // than a history, so the only honest answer is a full state.
        let s = session("badack");
        let (handle, _, _) = s.attach();
        wait_for(|| s.poll(handle).is_some());
        while s.poll(handle).is_some() {}

        s.ack(handle, u64::MAX);

        let (_, _, update) = s.poll(handle).expect("a bad ack must produce a resend");
        assert!(
            matches!(update, Update::Keyframe(_)),
            "expected a keyframe after an impossible acknowledgement"
        );
    }

    #[test]
    fn acknowledgements_are_tracked_separately_from_what_was_sent() {
        // The distinction this stage exists for. The host used to advance one
        // number on send and call it "acked", which is the host asserting that
        // everything it wrote was applied -- false for exactly the client that
        // matters, the one that died mid-write.
        let s = session("lag");
        let (handle, _, _) = s.attach();
        wait_for(|| s.poll(handle).is_some());
        while s.poll(handle).is_some() {}

        let lag = s.ack_lag(handle).expect("attached");
        assert!(lag > 0, "a client that has acknowledged nothing must show as behind");

        // Acknowledging what was actually sent brings it level.
        let sent = {
            let subs = s.subscribers.lock().expect("subscriber lock");
            subs.get(&handle).expect("attached").sent
        };
        s.ack(handle, sent);
        assert_eq!(s.ack_lag(handle), Some(0), "a caught-up client must show no lag");
    }

    #[test]
    fn scrollback_comes_back_with_the_attributes_it_names() {
        // Scrollback is prepended to a client's history rather than diffed, so
        // no later delta will define these attribute ids. Without them the
        // client renders history in whatever style it last held.
        // Enough coloured lines to push some off a three-row screen, so there
        // is history that carries a non-default attribute.
        let spec = script_cmd(
            "for i in 1 2 3 4 5 6 7 8 9 10; do \
             printf '\\033[31mline %s\\033[0m\\n' $i; done",
            "1..10 | ForEach-Object { Write-Host -ForegroundColor Red \\\"line $_\\\" }",
        );
        let s = Session::spawn(SessionId(77), &spec, PtySize::new(40, 3), 200, |_| {})
            .expect("spawn");
        // Two waits, both asserted, because "the child never ran" and "the
        // child ran and produced no history" are different failures and only
        // the second one is about scrollback. Discarding this `bool` is what
        // made #80 report the history assertion on `test (windows-latest)`
        // for a PowerShell that had not exited at all.
        assert!(
            wait_for(|| s.has_exited()),
            "the child never finished, so nothing after this is about scrollback"
        );
        // Exiting is not the condition that matters, which is why the fixed
        // 50ms sleep that used to stand here was a guess: on Windows it is the
        // process watcher that reports the exit, and it can fire while the tail
        // of the output is still in the ConPTY pipe, unread and unparsed.
        assert!(
            wait_for(|| !s.scrollback(0, 20).0.is_empty()),
            "a session that scrolled must have history"
        );

        let (rows, attrs) = s.scrollback(0, 20);
        for row in &rows {
            for run in &row.runs {
                assert!(
                    attrs.iter().any(|a| a.id == run.attr),
                    "history names attribute {:?} that no AttrDef defines",
                    run.attr
                );
            }
        }
    }

    #[test]
    fn every_attach_starts_from_a_keyframe() {
        // A client that has just connected has no base for a delta, and one
        // reattaching after an hour asleep is indistinguishable from a new one.
        let s = session("state");
        wait_for(|| s.has_exited());

        let (_, _, k) = s.attach();
        assert_eq!(k.rows as usize, k.rows_data.len(), "the keyframe is not a whole screen");
    }

    #[test]
    fn two_clients_track_the_session_independently() {
        // Desk and phone on one session is a real case. A shared encoder would
        // give each of them the other's deltas as their base.
        let s = session("shared");
        let (a, _, _) = s.attach();
        wait_for(|| s.poll(a).is_some());
        while s.poll(a).is_some() {}

        // B attaches late and must still receive full state, not A's leftovers.
        let (b, _, kb) = s.attach();
        assert!(!kb.rows_data.is_empty());
        assert!(s.poll(b).is_none(), "a freshly attached client was owed a delta");
    }

    #[test]
    fn a_session_reports_its_own_size() {
        let s = session("size");
        assert_eq!(s.size(), (80, 24));
        s.resize(100, 30);
        assert_eq!(s.size(), (100, 30), "the grid did not follow the resize");
    }

    /// A sized attach for the arbitration tests below.
    fn attach_at(s: &Session, cols: u16, rows: u16) -> (u64, u64, Keyframe) {
        s.attach_with(Box::new(|| {}), Some((cols, rows)))
    }

    #[test]
    fn the_smallest_attached_client_sets_the_session_size() {
        // Desk and phone on one session: every viewer must see a complete
        // screen, so the smallest pane wins and a detach gives the space back.
        let s = session("min");
        let (_a, _, _) = attach_at(&s, 80, 24);
        assert_eq!(s.size(), (80, 24));

        let (b, _, kb) = attach_at(&s, 60, 20);
        assert_eq!(s.size(), (60, 20), "the smaller client must win the arbitration");
        assert_eq!((kb.cols, kb.rows), (60, 20), "the attach keyframe must say what was granted");

        s.detach(b);
        assert_eq!(s.size(), (80, 24), "detaching the constraining client must restore the size");
    }

    #[test]
    fn attaching_larger_grants_the_existing_size() {
        let s = session("grant");
        let (_a, _, _) = attach_at(&s, 80, 24);
        let (_b, _, kb) = attach_at(&s, 100, 40);
        assert_eq!(s.size(), (80, 24), "a larger attach must not grow the shared pty");
        assert_eq!(
            (kb.cols, kb.rows),
            (80, 24),
            "the keyframe must carry the granted size, not the ask"
        );
    }

    #[test]
    fn a_foreign_size_change_sends_the_other_client_a_keyframe() {
        // A client whose own pane never changed has no reason to re-render;
        // the daemon must push it a full state, because a shrink described by
        // deltas lands inside the stale larger grid without tripping
        // NeedsKeyframe (zest_proto::apply).
        let s = session("foreign");
        let (a, _, _) = attach_at(&s, 80, 24);
        let (b, _, _) = attach_at(&s, 80, 24);
        wait_for(|| s.has_exited());
        while s.poll(a).is_some() {}
        while s.poll(b).is_some() {}

        assert!(s.set_client_size(b, 60, 20), "the shrink moves the min and must be granted");
        match s.poll(a) {
            Some((_, _, Update::Keyframe(k))) => {
                assert_eq!((k.cols, k.rows), (60, 20), "the keyframe must carry the new size");
            }
            other => panic!("the unchanged client was owed a keyframe, got {other:?}"),
        }
    }

    #[test]
    fn a_rebased_viewport_owes_every_client_a_keyframe() {
        // When a restating pty's repaint closes, the grid gives back the rows
        // the shrink displaced and the viewport/scrollback boundary moves
        // (#247). Deltas cannot say that -- there is no `DeltaOp::Resize` -- so
        // a client left to apply them would go on filing those lines as history
        // while the host calls them visible, and hold each of them twice.
        //
        // The reader thread's arm is one line calling this; what it has to do
        // is here, because the arm itself needs a real ConPTY to reach.
        let s = session("rebase");
        let (a, _, _) = attach_at(&s, 80, 24);
        let (b, _, _) = attach_at(&s, 80, 24);
        wait_for(|| s.has_exited());
        while s.poll(a).is_some() {}
        while s.poll(b).is_some() {}
        assert!(s.poll(a).is_none(), "the fixture is not quiet");

        keyframe_everyone(&s.subscribers);

        for (who, h) in [("a", a), ("b", b)] {
            match s.poll(h) {
                Some((_, _, Update::Keyframe(_))) => {}
                other => panic!("client {who} was owed a keyframe, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_ungranted_resize_touches_nothing() {
        // Equal-size recomputes are a no-op all the way down: a pty resize is
        // a ConPTY repaint on Windows (#200), so nothing may move unless the
        // min does.
        let s = session("noop");
        let (a, _, _) = attach_at(&s, 80, 24);
        let (b, _, _) = attach_at(&s, 60, 20);
        wait_for(|| s.has_exited());
        while s.poll(a).is_some() {}
        while s.poll(b).is_some() {}

        assert!(!s.set_client_size(a, 70, 22), "a resize that does not move the min changed it");
        assert_eq!(s.size(), (60, 20));
        assert!(s.poll(a).is_none(), "an ungranted resize must not repaint anyone");
        assert!(s.poll(b).is_none(), "an ungranted resize must not repaint anyone");
    }

    #[test]
    fn the_last_detach_keeps_the_size() {
        // The session outlives its clients (ADR-007) and gets no parting
        // resize -- a reattach from the same device finds the shape it left.
        let s = session("keep");
        let (a, _, _) = attach_at(&s, 60, 20);
        assert_eq!(s.size(), (60, 20));
        s.detach(a);
        assert_eq!(s.size(), (60, 20), "the last detach must leave the grid alone");
    }

    #[test]
    fn an_undeclared_attach_never_constrains_the_size() {
        // `attach()` declares nothing -- a watch-only subscriber has no pane
        // to protect and must not drag the session to some default.
        let s = session("watch");
        let (_w, _, _) = s.attach();
        let (_a, _, _) = attach_at(&s, 60, 20);
        assert_eq!(s.size(), (60, 20), "the undeclared subscriber must not out-vote the sized one");
    }
}
