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
use zest_proto::{Delta, Encoder, Keyframe, SessionId};
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
    /// Highest sequence this client has confirmed applying.
    acked: u64,
}

/// A running shell, and everyone watching it.
pub struct Session {
    pub id: SessionId,
    terminal: Arc<Mutex<Terminal>>,
    pty: Arc<dyn PtyTransport + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    subscribers: Mutex<HashMap<u64, Subscriber>>,
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

        {
            let terminal = Arc::clone(&terminal);
            let exited = Arc::clone(&exited);
            let title = Arc::clone(&title);
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
                        std::thread::yield_now();
                    }

                    exited.store(true, Ordering::Release);
                    wake(id);
                })
                .map_err(|e| DaemonError::Spawn(e.to_string()))?;
        }

        Ok(Self {
            id,
            terminal,
            pty: Arc::new(pty),
            writer: Arc::new(Mutex::new(writer)),
            subscribers: Mutex::new(HashMap::new()),
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
    pub fn attach(&self) -> (u64, Keyframe) {
        let mut encoder = Encoder::new();
        let (keyframe, seq) = {
            let term = self.terminal.lock().expect("terminal lock");
            let k = encoder.keyframe(
                term.grid(),
                cursor_of(&term),
                term.modes().contains(Modes::ALT_SCREEN),
                &self.title(),
            );
            (k, ChangeSource::seq(&*term))
        };

        let mut next = self.next_subscriber.lock().expect("counter lock");
        let handle = *next;
        *next += 1;
        self.subscribers
            .lock()
            .expect("subscriber lock")
            .insert(handle, Subscriber { encoder, acked: seq });

        (handle, keyframe)
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

    /// What this subscriber has not yet seen.
    ///
    /// `None` when it is caught up — an idle terminal generates no traffic, the
    /// network counterpart of drawing no frames.
    pub fn poll(&self, handle: u64) -> Option<Update> {
        let mut subs = self.subscribers.lock().expect("subscriber lock");
        let sub = subs.get_mut(&handle)?;

        let term = self.terminal.lock().expect("terminal lock");
        let seq = ChangeSource::seq(&*term);
        if seq == sub.acked {
            return None;
        }

        let cursor = cursor_of(&term);
        let alt = term.modes().contains(Modes::ALT_SCREEN);
        let title = self.title();

        let out = match ChangeSource::update_for(&*term, sub.acked) {
            zest_core::Update::Idle => return None,
            // Far enough behind that the delta chain would exceed the state it
            // describes. Normal after a sleep, not an error.
            zest_core::Update::Keyframe { .. } => {
                Update::Keyframe(sub.encoder.keyframe(term.grid(), cursor, alt, &title))
            }
            zest_core::Update::Delta { .. } => {
                let d = sub.encoder.delta(term.grid(), cursor, alt, &title);
                if d.ops.is_empty() && d.attrs.is_empty() {
                    // The sequence moved but nothing observable changed -- a
                    // mode set, a cursor save. Acknowledge and send nothing.
                    sub.acked = seq;
                    return None;
                }
                Update::Delta(d)
            }
        };

        sub.acked = seq;
        Some(out)
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

    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.terminal
            .lock()
            .map(|t| t.modes().contains(Modes::ALT_SCREEN))
            .unwrap_or(false)
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
        shape: 0,
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
        let (handle, _) = s.attach();

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
        let (handle, _) = s.attach();
        assert!(s.attached());

        s.detach(handle);
        assert!(!s.attached(), "the subscriber was not removed");

        // The session is still usable: a new client attaches and gets state.
        let (second, keyframe) = s.attach();
        assert!(!keyframe.rows_data.is_empty(), "reattaching produced no state");
        assert_ne!(second, handle, "handles were reused");
    }

    #[test]
    fn a_caught_up_subscriber_is_sent_nothing() {
        // The 0%-idle guarantee across the network. An idle terminal must not
        // generate traffic any more than it generates frames.
        let s = session("quiet");
        let (handle, _) = s.attach();

        wait_for(|| s.poll(handle).is_some());
        // Drain whatever is outstanding, then confirm it stays quiet.
        while s.poll(handle).is_some() {}
        assert!(s.poll(handle).is_none(), "an idle session kept producing updates");
    }

    #[test]
    fn every_attach_starts_from_a_keyframe() {
        // A client that has just connected has no base for a delta, and one
        // reattaching after an hour asleep is indistinguishable from a new one.
        let s = session("state");
        wait_for(|| s.has_exited());

        let (_, k) = s.attach();
        assert_eq!(k.rows as usize, k.rows_data.len(), "the keyframe is not a whole screen");
    }

    #[test]
    fn two_clients_track_the_session_independently() {
        // Desk and phone on one session is a real case. A shared encoder would
        // give each of them the other's deltas as their base.
        let s = session("shared");
        let (a, _) = s.attach();
        wait_for(|| s.poll(a).is_some());
        while s.poll(a).is_some() {}

        // B attaches late and must still receive full state, not A's leftovers.
        let (b, kb) = s.attach();
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
