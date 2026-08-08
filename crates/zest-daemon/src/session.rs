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
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zest_core::{ChangeSource, Modes, Terminal, TermEvent};
use zest_proto::delta::CursorState;
use zest_proto::{AttrDef, Delta, Encoder, Keyframe, RowPayload, SessionId};
use zest_pty::{CommandSpec, PtySize, PtyTransport};

use crate::DaemonError;

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
    /// Called when this subscriber has something to collect.
    ///
    /// Without it a connection blocked in `read` never learns that the shell
    /// produced output, so a client that attaches and then goes quiet -- which
    /// is every client, most of the time -- sees nothing again. Polling on a
    /// timer would fix it and cost the 0%-idle guarantee.
    wake: Box<dyn Fn() + Send>,
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
        wake: impl Fn(SessionId) + Send + 'static,
    ) -> Result<Self, DaemonError> {
        let mut pty = zest_pty::NativePty::spawn(cmd, size)
            .map_err(|e| DaemonError::Spawn(e.to_string()))?;
        let mut reader = pty.take_reader().expect("a fresh pty always has a reader");
        let writer = pty.writer();

        let terminal = Arc::new(Mutex::new(Terminal::new(
            size.cols as usize,
            size.rows as usize,
            scrollback,
        )));
        let exited = Arc::new(AtomicBool::new(false));
        let title = Arc::new(Mutex::new(String::new()));

        let subscribers: Arc<Mutex<HashMap<u64, Subscriber>>> = Arc::default();

        {
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
                        let n = match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };

                        let events = {
                            let Ok(mut term) = terminal.lock() else { break };
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

        Ok(Self {
            id,
            terminal,
            pty: Arc::new(pty),
            writer: Arc::new(Mutex::new(writer)),
            subscribers,
            next_subscriber: Mutex::new(0),
            exited,
            title,
        })
    }

    /// Begin watching. Returns the subscriber handle and the state to start from.
    ///
    /// Every attach starts with a keyframe: a client that has just connected has
    /// no base for a delta, and one that is reattaching after an hour asleep is
    /// indistinguishable from a new one.
    pub fn attach(&self) -> (u64, u64, Keyframe) {
        self.attach_with(Box::new(|| {}))
    }

    /// Attach, and be told when there is something to collect.
    ///
    /// Returns `(handle, seq, keyframe)`. The sequence is what the keyframe
    /// describes and what the client will acknowledge; without it on the wire a
    /// client has no baseline to compare the next update's `base` against.
    pub fn attach_with(&self, wake: Box<dyn Fn() + Send>) -> (u64, u64, Keyframe) {
        let mut encoder = Encoder::new();
        let (keyframe, seq) = {
            let term = self.terminal.lock().expect("terminal lock");
            let k = encoder.keyframe(term.grid(), cursor_of(&term), term.modes(), &self.title(), term.blocks());
            (k, ChangeSource::seq(&*term))
        };

        let mut next = self.next_subscriber.lock().expect("counter lock");
        let handle = *next;
        *next += 1;
        self.subscribers
            .lock()
            .expect("subscriber lock")
            .insert(handle, Subscriber { encoder, sent: seq, acked: seq, needs_keyframe: false, wake });

        (handle, seq, keyframe)
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
        self.subscribers.lock().expect("subscriber lock").remove(&handle);
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
        let out = if sub.needs_keyframe {
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

    /// Resize the pty and the grid together.
    ///
    /// Both, always: a grid that disagrees with what the shell believes produces
    /// output wrapped for a screen that does not exist.
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Err(e) = self.pty.resize(PtySize::new(cols, rows)) {
            tracing::warn!(session = self.id.0, error = %e, "pty resize failed");
        }
        if let Ok(mut term) = self.terminal.lock() {
            term.resize(cols as usize, rows as usize);
        }
    }

    #[must_use]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
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
    fn wait_for(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
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

    #[test]
    fn every_update_chains_onto_the_last() {
        // The property a client's resync rule rests on: each update's `base` is
        // the previous update's `seq`, unbroken. A gap means the client is
        // being asked to apply a difference from a state it was never sent, and
        // it has no way to know unless both numbers are on the wire.
        //
        // Both were hardcoded to 0 before this, which made every update look
        // like a valid continuation of every other.
        // Several separate writes, so there is a chain rather than one update.
        let script = "for i in 1 2 3 4 5; do printf 'line %s\\n' $i; sleep 0.05; done";
        let spec = CommandSpec {
            command_line: format!("/bin/sh -c \"{script}\""),
            cwd: None,
            env: zest_pty::terminal_env(),
        };
        let s = Session::spawn(SessionId(5), &spec, PtySize::new(80, 24), 100, |_| {})
            .expect("spawn");

        // Attach *before* anything is drained: `poll` consumes, so waiting for
        // output by polling would eat the very updates under test.
        let (handle, attach_seq, _) = s.attach();

        let mut previous = attach_seq;
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && seen < 3 {
            let Some((base, seq, _)) = s.poll(handle) else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            assert_eq!(
                base, previous,
                "update {seen} claimed to build on {base} when the last one produced {previous}"
            );
            assert!(seq > base, "an update must advance the sequence: {base} -> {seq}");
            previous = seq;
            seen += 1;
        }
        assert!(seen >= 2, "only {seen} updates arrived, so the chain was barely tested");
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
        let script = "for i in 1 2 3 4 5 6 7 8 9 10; do \
                      printf '\\033[31mline %s\\033[0m\\n' $i; done";
        let spec = CommandSpec {
            command_line: format!("/bin/sh -c \"{script}\""),
            cwd: None,
            env: zest_pty::terminal_env(),
        };
        let s = Session::spawn(SessionId(77), &spec, PtySize::new(40, 3), 200, |_| {})
            .expect("spawn");
        wait_for(|| s.has_exited());
        std::thread::sleep(std::time::Duration::from_millis(50));

        let (rows, attrs) = s.scrollback(0, 20);
        assert!(!rows.is_empty(), "a session that scrolled must have history");
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
}
