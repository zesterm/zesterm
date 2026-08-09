//! A terminal session: a PTY, a parser thread, and a writer thread.
//!
//! # Threading
//!
//! ```text
//! main (winit)      events -> encode -> tx.send()   [synchronous, <20us]
//!                   RedrawRequested -> lock, extract, unlock, render
//!                   owns the wgpu surface and device
//!
//! reader/parser     blocking read -> vte advance -> mutate FairMutex<Terminal>
//!
//! writer            rx.recv() -> write to the pty
//! ```
//!
//! The writer is its own thread purely so a wedged child cannot freeze the UI.
//! Writes are tiny, but a stopped process can make a pty write block
//! indefinitely, and that must never happen on the event loop.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use zest_core::{TermEvent, Terminal};
use zest_pty::{CommandSpec, PtySize, PtyTransport};

use crate::fair_mutex::FairMutex;

/// How much the parser consumes per lock acquisition.
///
/// Bounding this is the second half of the anti-starvation fix (the first being
/// [`FairMutex`]). Without a bound, a single `advance` of a huge read would hold
/// the lock long enough to drop frames even with a fair mutex.
const PARSE_CHUNK: usize = 64 * 1024;

/// What the app thread needs to be woken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wakeup {
    /// New output arrived, or the terminal otherwise changed.
    Redraw,
    /// The child exited. The window closes.
    Exited,
    /// The connection to the daemon dropped.
    ///
    /// **Not `Exited`.** The shell is still running in a daemon that does not
    /// care that a client went away — that is the whole point of ADR-007, and
    /// conflating the two would close the window on every Wi-Fi hiccup. The
    /// grid stays on screen showing the last state that was true.
    Detached,
    /// The link came back and the session is live again.
    ///
    /// Exists now that something emits it: the supervisor in `remote.rs`. It is
    /// the counterpart of `Detached`, and what lets anything watching tell "the
    /// grid is stale" from "the grid is current again".
    Reattached,
    /// The config file settled after a write.
    ///
    /// Carries nothing: the watcher runs on its own thread and must not read
    /// settings, because deciding what a change costs means comparing against
    /// the live ones. The reload happens on the main thread.
    ConfigChanged,
    /// Something in the fleet view changed — a host appeared, presence
    /// moved, a session list refreshed. Coalesced by `FleetModel`'s latch,
    /// so a chatty network posts one of these per burst, not per packet.
    FleetChanged,
    /// One tab's child exited.
    ///
    /// [`Wakeup::Exited`] predates tabs and means "the window's only session
    /// ended, close the window". With several tabs the app must know *which*
    /// ended, so every tab's wake callback translates `Exited` into this,
    /// carrying the tab's address. The bare variant survives for the paths
    /// that still mean the whole window.
    TabExited(zest_proto::SessionAddr),
    /// A pinned session's host answered and said the session no longer
    /// exists.
    ///
    /// **Not `Detached`** — the link is fine; the session is gone. And not
    /// `Exited` either: that means *this window's* child ended and closes the
    /// window, while a gone session under one tab of several must only mark
    /// that tab. The supervisor that sends this has already stopped, because
    /// silently swapping a fresh shell in under a labeled tab is how someone
    /// types into the wrong machine.
    SessionGone(zest_proto::SessionAddr),
}

pub struct Session {
    pub terminal: Arc<FairMutex<Terminal>>,
    /// Set by the parser, cleared by the renderer. Also the coalescing latch.
    needs_redraw: Arc<AtomicBool>,
    pty_tx: Sender<Vec<u8>>,
    pty: Arc<dyn PtyTransport + Send + Sync>,
    exited: Arc<AtomicBool>,
}

// SAFETY-adjacent note: `PtyTransport` is `Send`, and every implementation is
// internally synchronized by the kernel. `Sync` is what lets the resize path run
// from the event loop while the reader thread holds its own handle.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    /// Spawn a shell and start pumping it.
    ///
    /// `wake` is called when the app thread should redraw. It is invoked only on
    /// the *transition* from clean to dirty — see [`Session::mark_dirty`].
    pub fn spawn(
        cmd: &CommandSpec,
        size: PtySize,
        scrollback: usize,
        wake: impl Fn(Wakeup) + Send + 'static,
    ) -> Result<Self, zest_pty::PtyError> {
        let mut pty = zest_pty::NativePty::spawn(cmd, size)?;
        let mut reader = pty.take_reader().expect("a fresh pty always has a reader");
        let mut writer = pty.writer();

        let terminal = Arc::new(FairMutex::new(Terminal::new(
            size.cols as usize,
            size.rows as usize,
            scrollback,
        )));
        let needs_redraw = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let (pty_tx, pty_rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) =
            crossbeam_channel::unbounded();

        // --- writer thread ---
        {
            let pty_rx = pty_rx.clone();
            std::thread::Builder::new()
                .name("zest-pty-writer".into())
                .spawn(move || {
                    while let Ok(bytes) = pty_rx.recv() {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                })
                .expect("spawn writer thread");
        }

        // --- reader / parser thread ---
        {
            let terminal = Arc::clone(&terminal);
            let needs_redraw = Arc::clone(&needs_redraw);
            let exited = Arc::clone(&exited);
            let reply_tx = pty_tx.clone();

            std::thread::Builder::new()
                .name("zest-pty-reader".into())
                .spawn(move || {
                    let mut buf = vec![0u8; PARSE_CHUNK];
                    loop {
                        let n = match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };

                        // Unfair acquire: this thread is the one in a tight
                        // loop, so it must never barge ahead of a renderer that
                        // is already waiting.
                        let events = {
                            let mut term = terminal.lock_unfair();
                            term.advance(&buf[..n]);
                            term.take_events()
                        };

                        // Replies go straight back to the pty. Dropping them
                        // hangs whatever asked -- a DSR or an OSC 11 background
                        // query waits forever for an answer.
                        for event in events {
                            if let TermEvent::Reply(bytes) = event {
                                let _ = reply_tx.send(bytes);
                            }
                        }

                        if !needs_redraw.swap(true, Ordering::Release) {
                            wake(Wakeup::Redraw);
                        }

                        // Give a waiting renderer a chance even under a flood.
                        // The fair mutex alone is not enough: without yielding,
                        // this thread can be back at `lock_unfair` before the
                        // scheduler has run anyone else.
                        std::thread::yield_now();
                    }

                    exited.store(true, Ordering::Release);
                    needs_redraw.store(true, Ordering::Release);
                    wake(Wakeup::Exited);
                })
                .expect("spawn reader thread");
        }

        Ok(Self {
            terminal,
            needs_redraw,
            pty_tx,
            pty: Arc::new(pty),
            exited,
        })
    }

    /// Send bytes to the child.
    ///
    /// Called synchronously from the input handler, *before* anything else.
    /// Queueing input until the next frame is the single most common latency bug
    /// in hobby terminals and costs a whole frame for free.
    pub fn write(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.pty_tx.send(bytes);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Err(e) = self.pty.resize(PtySize::new(cols, rows)) {
            tracing::warn!(error = %e, "pty resize failed");
        }
        self.terminal.lock().resize(cols as usize, rows as usize);
        self.mark_dirty();
    }

    /// True if the terminal changed since the last [`Session::take_dirty`].
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.needs_redraw.load(Ordering::Acquire)
    }

    /// Clear the dirty flag. Called after presenting a frame.
    pub fn take_dirty(&self) -> bool {
        self.needs_redraw.swap(false, Ordering::AcqRel)
    }

    /// Mark dirty from the app side (resize, focus, theme change).
    pub fn mark_dirty(&self) {
        self.needs_redraw.store(true, Ordering::Release);
    }

    /// Wanted by the coming "restart in <cwd>" affordance, which keeps a dead
    /// session on screen with its history rather than closing the window.
    #[allow(dead_code)]
    #[allow(dead_code, reason = "the daemon client will need it; the local path uses Wakeup::Exited")]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// A command that prints `text` and exits.
    ///
    /// The session layer is platform-independent, but the thing it spawns
    /// cannot be — and hardcoding `cmd.exe` is what made these tests fail the
    /// first time the workspace was built on macOS.
    fn echo(text: &str) -> CommandSpec {
        let command_line = if cfg!(windows) {
            format!("cmd.exe /c echo {text}")
        } else {
            format!("/bin/echo {text}")
        };
        CommandSpec { command_line, cwd: None, env: Vec::new() }
    }

    #[test]
    fn a_session_runs_a_command_and_reports_its_output() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&woken);

        let cmd = echo("session-probe");
        let session = Session::spawn(&cmd, PtySize::new(80, 24), 100, move |_| {
            w.fetch_add(1, Ordering::Relaxed);
        })
        .expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if session.terminal.lock().screen_text().contains("session-probe") {
                break;
            }
            assert!(Instant::now() < deadline, "output never arrived");
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(woken.load(Ordering::Relaxed) > 0, "the app was never woken");
        assert!(session.is_dirty());
    }

    /// The wakeup latch is what stops a flood drowning the event loop.
    ///
    /// Without it, a 100MB `cat` posts millions of user events and the UI dies
    /// processing the queue rather than rendering.
    #[test]
    fn wakeups_coalesce_until_the_frame_is_drawn() {
        let count = Arc::new(AtomicUsize::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        // The exact idiom used by the reader thread.
        let bump = |flag: &AtomicBool, count: &AtomicUsize| {
            if !flag.swap(true, Ordering::Release) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        };

        for _ in 0..1000 {
            bump(&flag, &count);
        }
        assert_eq!(count.load(Ordering::Relaxed), 1, "1000 chunks must post one wakeup");

        // After the frame is presented the latch resets and the next chunk wakes
        // again.
        flag.swap(false, Ordering::AcqRel);
        bump(&flag, &count);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn dirty_state_round_trips() {
        let cmd = echo("dirty-probe");
        let session = Session::spawn(&cmd, PtySize::new(80, 24), 0, |_| {}).expect("spawn");

        session.mark_dirty();
        assert!(session.is_dirty());
        assert!(session.take_dirty(), "take reports the previous state");
        assert!(!session.is_dirty(), "and clears it");
    }
}
