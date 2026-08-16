//! Unix pseudo-terminals.
//!
//! `posix_openpt` + `fork`/`exec`, which is a far older and far better behaved
//! interface than ConPTY. There is no equivalent of the pseudoconsole shutdown
//! dance here: the kernel owns the pty pair, and closing the master is a
//! complete and non-blocking teardown.
//!
//! The child is launched through [`std::process::Command`] rather than a raw
//! `fork`, so argv/env/cwd handling and the exec failure channel stay in std.
//! Only the two things std cannot express — `setsid` and `TIOCSCTTY` — are done
//! by hand in `pre_exec`.
//!
//! # The sharp edges
//!
//! 1. **The parent must close its copy of the slave fd immediately after
//!    spawning.** EOF on the master is defined as "no slave fds remain open
//!    anywhere". If the parent keeps one, the shell can exit and the reader
//!    thread still blocks in `read` forever. This is the direct analogue of
//!    ConPTY gotcha 2 and it is handled the same way: the slave is dropped as
//!    soon as `Command::spawn` returns.
//!
//! 2. **`read` on the master reports EOF as `EIO`, not as `0`.** When the last
//!    slave closes, both macOS and Linux fail the read with `EIO` rather than
//!    returning a zero-length read. A caller that only treats `Ok(0)` as EOF
//!    still terminates, but logs an I/O error on every clean shell exit — which
//!    looks exactly like a bug and is the single most likely thing to be
//!    misdiagnosed here. The drain thread (sharp edge 6) treats it as EOF, so
//!    what reaches [`PtyReader`] is a plain `Ok(0)` — mirroring what the
//!    Windows reader does with `ERROR_BROKEN_PIPE`.
//!
//! 3. **On macOS, `TIOCSWINSZ` on the master fails with `ENOTTY` until the
//!    slave has been opened at least once.** Setting the initial size straight
//!    after `unlockpt` — the obvious place, and what Linux accepts — therefore
//!    fails on macOS with an error that reads like the fd is not a terminal at
//!    all. The size is set on the *slave* instead, which works everywhere and
//!    is unambiguous. Later [`UnixPty::resize`] calls go to the master, which
//!    by then is legal because the child is holding the slave open.
//!
//! 4. **Writing to a master whose child is gone raises `SIGPIPE`**, whose
//!    default disposition is to kill the process — so a shell exiting at the
//!    wrong moment would take the whole terminal down with it. Rust's runtime
//!    installs `SIG_IGN` for `SIGPIPE` before `main`, which turns that into a
//!    plain `EPIPE` return. This backend depends on that; it is stated here
//!    because a future `#[unix_sigpipe]` attribute or a raw-`main` embedding
//!    would silently remove it.
//!
//! 5. **Closing the master cannot hang up a pty whose reader is parked in
//!    `read`.** The hangup fires when the last duplicate of the master fd is
//!    closed, and a blocked reader is holding one; it cannot let go until the
//!    read returns, and the read will not return until the hangup. Every API
//!    call involved succeeds and the child simply lives on. A short-lived owner
//!    never sees it — the process exits and takes every fd with it — but a
//!    daemon closing one session out of many is exactly the case that does.
//!    [`UnixPty::hangup`](PtyTransport::hangup) signals the child's process
//!    group instead of waiting for a close that can never come.
//!
//! 6. **macOS destroys a dead pty's queued output about 0.6s after the last
//!    slave closes.** A child that prints and exits — `echo`, `git status`, any
//!    one-shot command — leaves its bytes in the kernel's output queue, and if
//!    nobody has read the master by then they are gone: `read` returns 0, as if
//!    the child had printed nothing. Measured on Darwin 25.5/arm64, with a
//!    fork/exec reference in C and through this module: a first read at 590ms
//!    gets all 23 bytes, one at 610ms gets none, deterministically. Reaping the
//!    child destroys them *immediately*, which is the same trap with a shorter
//!    fuse. Linux keeps the data. Nothing reports an error on either.
//!
//!    That deadline is only reachable by a reader that has not started. A
//!    reader already parked in `read` is handed the bytes as they are written
//!    and still had them three seconds later in the same experiment — so
//!    [`UnixPty::spawn`] starts its drain thread and waits for it to reach its
//!    first `read` **before** the child is forked, and hands the caller a
//!    [`PtyReader`] over the drained bytes rather than over the fd. The window
//!    is not narrowed, it is removed: whenever the owner gets round to reading,
//!    the bytes are already out of the kernel.
//!
//!    This also fixes the ordering that made it silent. EOF used to be able to
//!    arrive *instead of* the data; now it is a `Vec` queued behind it, so a
//!    caller doing the usual `Ok(0) => break` cannot break before consuming
//!    what the child wrote. (Issue #54. Keeping a slave fd open in the parent
//!    also preserves the data — measured — and is rejected because it destroys
//!    EOF entirely, which is sharp edge 1.)
//!
//! Unlike `ResizePseudoConsole`, [`UnixPty::resize`] does *not* cause the screen
//! to be re-emitted. It updates the kernel's `winsize` and raises `SIGWINCH`;
//! whether anything is redrawn is the child's decision.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use rustix::termios::Winsize;

use crate::{ChildStatus, CommandSpec, PtyError, PtySize, PtyTransport};

/// How much the drain thread lifts out of the kernel at a time.
const DRAIN_CHUNK: usize = 64 * 1024;

/// How many chunks may sit between the kernel and the parser.
///
/// Bounded, and that bound is the whole design. The pty's own output queue is
/// what makes a runaway `cat /dev/urandom` block its producer instead of eating
/// the machine; an unbounded hand-off in front of it would quietly replace that
/// backpressure with unbounded memory. Sixteen chunks is a megabyte in flight —
/// far more than a parser that is keeping up ever holds, and a hard ceiling for
/// one that is not.
const DRAIN_DEPTH: usize = 16;

/// The read half of the pty. Blocking.
///
/// Reads from the drain thread rather than from the fd — sharp edge 6. By the
/// time a byte is offered here it is already out of the kernel, so how long the
/// owner takes to ask for it no longer decides whether it exists.
pub struct PtyReader {
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    /// The chunk being handed out, and how much of it has gone. A caller's
    /// buffer is smaller than `DRAIN_CHUNK` often enough that this is the
    /// normal path, not an edge case.
    chunk: Vec<u8>,
    at: usize,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == self.chunk.len() {
            let Ok(next) = self.rx.recv() else {
                // The drain thread is gone, which it only becomes at EOF — and
                // it sends before it goes, so everything the child wrote has
                // already been handed over above.
                return Ok(0);
            };
            self.chunk = next;
            self.at = 0;
        }
        let n = (self.chunk.len() - self.at).min(buf.len());
        buf[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// The write half of the pty.
pub struct PtyWriter(File);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Unbuffered on our side; the kernel owns the line discipline queue.
        Ok(())
    }
}

/// A pty pair and the process running on the far side of it.
pub struct UnixPty {
    /// Shared so `resize` can run on the event loop while the reader thread
    /// holds its own duplicate.
    master: Arc<OwnedFd>,
    reader: Option<PtyReader>,
    /// `wait_for_child` takes `&self` (the trait object is shared), but
    /// reaping needs `&mut Child`.
    child: Mutex<Child>,
}

impl UnixPty {
    /// Open a pty pair and spawn a command on the slave side.
    pub fn spawn(cmd: &CommandSpec, size: PtySize) -> Result<Self, PtyError> {
        let master = open_master().map_err(PtyError::Create)?;
        let slave_path = rustix::pty::ptsname(&master, Vec::new())
            .map_err(|e| PtyError::Create(e.into()))?;
        let slave = open_slave(&slave_path).map_err(PtyError::Create)?;

        // Gotcha 3: on the slave, and only once it exists. Set before the child
        // is created so it never observes a bogus 0x0 size.
        set_winsize(&slave, size).map_err(PtyError::Create)?;

        // Sharp edge 6: the drain starts, and is proven to be *inside* `read`,
        // before the child exists. Once the child can exit, a clock starts on
        // its output that this process cannot see and cannot restart; being
        // parked in `read` first is the only state from which that clock is
        // unreachable. Everything between here and the caller's first read --
        // fork, exec, allocating a Terminal, spawning threads, whatever the
        // scheduler does with a loaded machine -- stops mattering.
        let reader = spawn_drain(&master)?;

        let mut command = build_command(cmd).ok_or_else(|| PtyError::Spawn {
            command: cmd.command_line.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "empty command line"),
        })?;

        // Each standard stream gets its own duplicate: `Stdio` consumes the fd,
        // and all three must remain independently closeable in the child.
        let (stdin, stdout, stderr) = (
            slave.try_clone().map_err(PtyError::Create)?,
            slave.try_clone().map_err(PtyError::Create)?,
            slave.try_clone().map_err(PtyError::Create)?,
        );
        command.stdin(Stdio::from(stdin)).stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));

        // SAFETY: everything called here is async-signal-safe. `setsid` and
        // `ioctl` are raw syscalls through rustix, and no allocation, locking
        // or Rust runtime machinery is involved.
        //
        // Ordering note: std performs the stdio `dup2` *before* running these
        // closures, so fd 0 is already the slave. Reaching for it by number
        // rather than moving an extra duplicate into the closure keeps the
        // parent from having a fourth slave fd to remember to close (gotcha 1).
        unsafe {
            command.pre_exec(|| {
                // A new session, so the child is not still attached to whatever
                // terminal launched zesterm.
                rustix::process::setsid()?;
                // Without this the child has no controlling terminal: job
                // control never initializes, ^C sends nothing, and interactive
                // shells complain or silently degrade. It is the unix
                // counterpart of the STARTF_USESTDHANDLES trap — everything
                // else works, so the cause is nowhere near the symptom.
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                Ok(())
            });
        }

        let child = command.spawn().map_err(|source| PtyError::Spawn {
            command: cmd.command_line.clone(),
            source,
        })?;

        // Gotcha 1. Explicit rather than incidental: this is what lets the
        // reader ever see EOF.
        drop(slave);

        Ok(Self { master: Arc::new(master), reader: Some(reader), child: Mutex::new(child) })
    }

    /// Block until the child exits, or until `timeout` elapses.
    ///
    /// Also what reaps the child, so it is not optional: skipping it leaves a
    /// zombie for the lifetime of the process.
    pub fn wait_for_child(&self, timeout: Option<std::time::Duration>) -> ChildStatus {
        let Ok(mut child) = self.child.lock() else {
            return ChildStatus::StillRunning;
        };
        wait_locked(&mut child, timeout)
    }
}

/// How long a child gets to act on `SIGHUP` before it is killed.
///
/// A shell running its exit traps needs microseconds; this is generous enough
/// that a slow one is not cut off, and short enough that closing a session is
/// not perceived as a hang.
const HANGUP_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

/// How long the kernel gets to deliver `SIGKILL` before the wait is abandoned.
///
/// Only reached by a process ignoring `SIGHUP`. Giving up leaves a zombie, which
/// is why it is logged: one is a curiosity, a stream of them is a bug.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

impl UnixPty {
    /// See [`PtyTransport::hangup`].
    ///
    /// `SIGHUP` to the child's process *group*, not to the child, because that
    /// is what a real hangup delivers: the kernel signals the terminal's
    /// foreground group. The child is a group leader (`setsid` in `pre_exec`,
    /// so its pgid is its pid), so the group is exactly the right target.
    ///
    /// In practice the pid alone would usually do — when a session leader dies
    /// the kernel runs the same cascade — but "usually" depends on which group
    /// the shell put its jobs in, and this is the form that does not.
    ///
    /// A shell gets to run its exit traps. Only a program that *ignores*
    /// `SIGHUP` reaches the `SIGKILL`, and by then the user has asked for the
    /// session to end twice over.
    fn hangup_child(&self) {
        let Ok(mut child) = self.child.lock() else { return };

        // Already gone and reaped: signalling now would either hit nothing or,
        // far worse, hit whatever the OS has since given the pid to.
        match child.try_wait() {
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::warn!(error = %e, "could not tell whether the child is alive; not signalling");
                return;
            }
            Ok(None) => {}
        }

        if let Some(pid) = i32::try_from(child.id()).ok().and_then(rustix::process::Pid::from_raw) {
            if let Err(e) = rustix::process::kill_process_group(pid, rustix::process::Signal::HUP) {
                tracing::debug!(error = %e, "SIGHUP to the session group failed");
            }
        }

        if matches!(wait_locked(&mut child, Some(HANGUP_GRACE)), ChildStatus::Exited(_)) {
            return;
        }

        tracing::info!(pid = child.id(), "child ignored SIGHUP; killing");
        let _ = child.kill();
        if !matches!(wait_locked(&mut child, Some(KILL_GRACE)), ChildStatus::Exited(_)) {
            tracing::warn!(pid = child.id(), "child survived SIGKILL; leaving it unreaped");
        }
    }
}

/// The body of [`UnixPty::wait_for_child`], with the lock already held.
///
/// Split out because `hangup_child` waits twice with the same lock held, and
/// calling the `&self` method there would deadlock on the second acquisition.
fn wait_locked(child: &mut Child, timeout: Option<std::time::Duration>) -> ChildStatus {
    match timeout {
        None => match child.wait() {
            Ok(status) => ChildStatus::Exited(exit_code(status)),
            Err(e) => {
                tracing::warn!(error = %e, "waiting on child failed");
                ChildStatus::StillRunning
            }
        },
        Some(limit) => {
            // No portable "wait with timeout": `waitpid` is all-or-nothing
            // and signal-based alternatives differ per platform. Polling is
            // adequate because every caller is either shutting down or
            // draining, and neither is latency-sensitive.
            let deadline = std::time::Instant::now() + limit;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return ChildStatus::Exited(exit_code(status)),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "polling child failed");
                        return ChildStatus::StillRunning;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return ChildStatus::StillRunning;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

impl PtyTransport for UnixPty {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take().map(|r| Box::new(r) as Box<dyn Read + Send>)
    }

    /// See [`PtyTransport::exit_code`].
    ///
    /// A zero timeout runs exactly one `try_wait` and returns — `wait_locked`
    /// probes before it checks the deadline — so this reaps an exited child
    /// without ever blocking on a live one.
    ///
    /// **`try_lock`, because `lock` would break the non-blocking contract in
    /// exactly the case this is called in.** `hangup_child` holds this mutex
    /// across `HANGUP_GRACE` and then `KILL_GRACE` while it escalates, and the
    /// daemon asks for the exit code from `Connection::poll` — which serves
    /// *every* session on that connection, so blocking here would stall
    /// unrelated sessions' updates behind one that is being closed.
    /// Contention means "no status yet", which the caller already handles: the
    /// daemon re-sends `Exited` on every pass, so a contended read is deferred
    /// rather than lost, and `hangup_child` reaps on its way out so the next
    /// call finds an answer waiting.
    ///
    /// `std::process::Child` caches the status once it has reaped, so calling
    /// this after `hangup` has already waited still answers rather than
    /// reporting the child as running for ever.
    fn exit_code(&self) -> Option<i32> {
        let mut child = self.child.try_lock().ok()?;
        match wait_locked(&mut child, Some(std::time::Duration::ZERO)) {
            ChildStatus::Exited(code) => Some(code as i32),
            ChildStatus::StillRunning => None,
        }
    }

    fn writer(&self) -> Box<dyn Write + Send> {
        // Duplicated rather than shared so the writer thread closing its half
        // cannot pull the fd out from under the reader.
        match self.master.try_clone() {
            Ok(fd) => Box::new(PtyWriter(File::from(fd))),
            Err(e) => {
                tracing::error!(error = %e, "could not duplicate pty master fd");
                Box::new(io::sink())
            }
        }
    }

    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        set_winsize(self.master.as_fd(), size).map_err(PtyError::Resize)
    }

    fn hangup(&self) {
        self.hangup_child();
    }
}

// Dropping the last master fd hangs up the pty, which sends SIGHUP to the
// child's session — so there is no explicit kill in `Drop`, and none is wanted:
// a shell asked to hang up gets to run its exit traps. The writer and the drain
// thread hold their own duplicates, so the hangup lands when the last of them is
// dropped, not when `UnixPty` is.
//
// Sharp edge 5: that is enough for a *short-lived* owner, which is why it went
// unnoticed for so long. It is not enough for a daemon. A reader thread parked
// in `read` holds a duplicate it cannot release until the read returns, and the
// read cannot return until the hangup — so a session that is closed while its
// shell is idle keeps the shell, the thread, and the whole terminal with its
// scrollback, forever. `hangup` breaks the cycle by signalling instead of
// waiting for a close that can never come.
//
// Since sharp edge 6 that parked reader is the drain thread, and it is parked
// unconditionally rather than only when someone took the reader. Nothing
// changes for the caller — `hangup` was already the only teardown that works
// and is what both session layers call — but it does mean a `UnixPty` dropped
// without `hangup`, while its child is alive, leaves that thread parked. The
// consumer's own reader thread was already in exactly that state, so this
// leaks nothing new; it just no longer depends on a consumer existing.

/// Start the thread that lifts this pty's output out of the kernel.
///
/// Returns once that thread is at its first `read`, not merely once it has been
/// created: a thread that exists but has not been scheduled is exactly the
/// state sharp edge 6 punishes, so `spawn` would be back to hoping. The
/// rendezvous is a zero-capacity channel, whose `send` completes only when the
/// far side has taken the value.
///
/// Called before `Command::spawn`, which forks. That is safe for the same
/// reason `pre_exec` is: the child runs only async-signal-safe code before
/// `exec`, and it inherits no lock this thread could be holding — the drain
/// thread is blocked in a syscall on a `CLOEXEC` fd it does not share.
fn spawn_drain(master: &OwnedFd) -> Result<PtyReader, PtyError> {
    let fd = master.try_clone().map_err(PtyError::Create)?;
    let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(DRAIN_DEPTH);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<()>(0);

    std::thread::Builder::new()
        .name("zest-pty-drain".into())
        .spawn(move || {
            let mut master = File::from(fd);
            let mut buf = vec![0u8; DRAIN_CHUNK];
            // Last statement before the read, so what the caller waited for is
            // true rather than nearly true.
            let _ = ready_tx.send(());
            loop {
                let n = match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    // Gotcha 2: the child exiting is EOF, not a failure. Kept
                    // here rather than in `PtyReader` because this is now the
                    // only place an fd is read.
                    Err(e)
                        if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
                    {
                        break
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::debug!(error = %e, "pty master read failed; treating as EOF");
                        break;
                    }
                };
                // Blocks when the parser is behind, which is the point: it
                // stops here, the kernel's queue fills, and the child blocks on
                // its own write, exactly as it did when the parser read the fd
                // directly. An error means the `PtyReader` was dropped and
                // nobody wants these bytes.
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        })
        .map_err(PtyError::Create)?;

    // The drain thread must be *reading* before this returns, or the window
    // this whole thread exists to close is still open.
    //
    // A disconnected channel means it died before signalling, and that has to
    // be an error rather than a shrug: returning `Ok` here hands back a reader
    // that will EOF immediately, which is indistinguishable from a child that
    // printed nothing — the exact silent-loss shape this file is fixing. Fail
    // where the cause is still named.
    ready_rx
        .recv()
        .map_err(|_| PtyError::Create(io::Error::other("the pty drain thread died before it read")))?;
    Ok(PtyReader { rx, chunk: Vec::new(), at: 0 })
}

fn open_master() -> io::Result<OwnedFd> {
    use rustix::pty::OpenptFlags;

    // NOCTTY: this process must not acquire the pty as *its* controlling
    // terminal. Only the child does that, in pre_exec.
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;

    // `openpt` cannot request CLOEXEC portably (macOS has no O_CLOEXEC for
    // posix_openpt), so it is set afterwards. Without it the master leaks into
    // the child and every process it spawns, which keeps the pty alive long
    // past the point anything is reading it.
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;

    Ok(master)
}

fn set_winsize(fd: impl AsFd, size: PtySize) -> io::Result<()> {
    let winsize = Winsize {
        ws_row: size.rows.max(1),
        ws_col: size.cols.max(1),
        // Pixel dimensions are advisory and almost nothing reads them; the
        // programs that do (sixel, kitty graphics) query the terminal directly
        // instead.
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    Ok(rustix::termios::tcsetwinsize(fd, winsize)?)
}

fn open_slave(path: &CString) -> io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags};

    // NOCTTY again for the same reason as the master; the child re-opens the
    // relationship deliberately via TIOCSCTTY.
    Ok(rustix::fs::open(path.as_c_str(), OFlags::RDWR | OFlags::NOCTTY, Mode::empty())?)
}

/// Turn a [`CommandSpec`] into a `Command`, or `None` if it names nothing.
fn build_command(cmd: &CommandSpec) -> Option<Command> {
    let argv = split_command_line(&cmd.command_line);
    let (program, args) = argv.split_first()?;

    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = &cmd.cwd {
        command.current_dir(cwd);
    }
    for (k, v) in &cmd.env {
        // An empty value means *unset*, not "set to the empty string" — the
        // same convention the Windows env block uses, and for the same reason:
        // clearing an inherited terminal identity has to make the variable
        // genuinely absent, because plenty of code only tests for presence.
        if v.is_empty() {
            command.env_remove(k);
        } else {
            command.env(k, v);
        }
    }
    Some(command)
}

/// Split a command line into argv, honouring quotes.
///
/// [`CommandSpec::command_line`] is a single string because that is the shape
/// `CreateProcessW` wants, and it does its own parsing. On unix there is no
/// equivalent, so this is the other half of that contract. Deliberately not a
/// shell: no globbing, no variable expansion, no operators — a command line
/// that needs those should name a shell explicitly.
fn split_command_line(line: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match (quote, c) {
            // Backslash escapes only outside single quotes, matching sh.
            (q, '\\') if q != Some('\'') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    started = true;
                }
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                // An empty quoted string is still an argument.
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (_, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        argv.push(current);
    }
    argv
}

fn exit_code(status: std::process::ExitStatus) -> u32 {
    match status.code() {
        Some(code) => code as u32,
        // Killed by a signal. 128+n is the shell convention, and the only thing
        // above this crate that reads the code is a human.
        None => status.signal().map_or(1, |s| 128 + s as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_plain_command_line() {
        assert_eq!(split_command_line("/bin/echo hi there"), ["/bin/echo", "hi", "there"]);
    }

    #[test]
    fn keeps_quoted_arguments_whole() {
        assert_eq!(
            split_command_line(r#"/usr/bin/env "My Shell" 'two words'"#),
            ["/usr/bin/env", "My Shell", "two words"],
            "a path with spaces must survive, or the configured shell setting is unusable"
        );
    }

    #[test]
    fn an_empty_quoted_argument_still_counts() {
        assert_eq!(
            split_command_line(r#"prog "" x"#),
            ["prog", "", "x"],
            "dropping it would silently shift every later argument left"
        );
    }

    #[test]
    fn spawns_a_child_and_reads_its_output() {
        let spec = CommandSpec {
            command_line: "/bin/echo hello".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/echo");
        let mut reader = pty.take_reader().expect("a fresh pty always has a reader");

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            // Reaching EOF at all is the assertion: if the parent's slave fd
            // leaked, this blocks forever and the test hangs rather than fails.
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("hello"), "echo output should reach the master: {text:?}");
        assert!(
            matches!(pty.wait_for_child(None), ChildStatus::Exited(0)),
            "a successful echo exits 0"
        );
    }

    /// A child that prints and exits must not lose its output to a late reader.
    ///
    /// Sharp edge 6, and issue #54's macOS half. `/bin/echo` writes 23 bytes
    /// and is gone in microseconds; the owner is still building a `Terminal`,
    /// two channels and a writer thread, and its reader thread has not been
    /// scheduled yet. On macOS the kernel discards a dead pty's queued output
    /// roughly 0.6s after the last slave closes — measured on Darwin 25.5,
    /// arm64: a first read at 550ms gets all 23 bytes, one at 610ms gets
    /// `read() == 0` and the bytes are simply gone. A loaded CI runner losing
    /// 0.6s between spawning a thread and running it is not exotic, and the
    /// result is a window that shows nothing, for ever, with no error anywhere.
    ///
    /// 1200ms so the assertion is on the far side of that deadline with margin.
    /// Linux keeps the data indefinitely and passes either way, which is
    /// exactly why this is stated as an invariant rather than left to macOS: it
    /// is the *contract* that a spawned pty starts draining immediately, and
    /// only one platform is currently rude enough to punish breaking it.
    #[test]
    fn output_survives_a_reader_that_arrives_late() {
        let spec = CommandSpec {
            command_line: "/bin/echo hello".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/echo");

        // Past macOS's ~0.6s discard, measured: the cliff sits between 590ms
        // and 610ms and does not move under load. Only macOS is rude enough to
        // punish a late reader, so only macOS pays the wait — on Linux the
        // queued bytes survive indefinitely and a 1.2s sleep would buy nothing
        // but a slower test run on every CI job. The short sleep still keeps
        // the test meaningful there: it proves the drain hands over what it
        // buffered rather than racing the handover.
        let late = if cfg!(target_os = "macos") { 1200 } else { 100 };
        std::thread::sleep(std::time::Duration::from_millis(late));

        let mut reader = pty.take_reader().expect("a fresh pty always has a reader");
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("hello"),
            "a command that ran and exited lost its output because nobody was \
             reading yet -- the user sees an empty window and no error: {text:?}"
        );
    }

    /// The one test that actually proves `TIOCSCTTY` worked.
    ///
    /// ^C is not a byte the child reads — it is the line discipline signalling
    /// the pty's *foreground process group*. A child that got `setsid` but no
    /// controlling terminal has no such group, so the byte goes nowhere and the
    /// sleep runs to completion. Everything else about that pty still behaves
    /// perfectly, which is what makes the failure so hard to place.
    ///
    /// `/bin/sleep` directly, deliberately not `sh -c 'sleep 30'`: macOS's
    /// `/bin/sh` is bash in POSIX mode, and a *non-interactive* bash does not
    /// pass SIGINT on to its caller's exit status the way an interactive shell
    /// would — that wrapper survives the ^C and makes a working pty look
    /// broken. Verified against a C reference implementation before believing
    /// it. Interactive shells are unaffected, which is why this only ever bites
    /// in tests.
    #[test]
    fn ctrl_c_interrupts_the_child() {
        let spec = CommandSpec {
            command_line: "/bin/sleep 30".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/sleep");
        let mut writer = pty.writer();

        // Give sh time to exec sleep, so the signal lands on a foreground
        // process group that actually exists.
        std::thread::sleep(std::time::Duration::from_millis(300));
        writer.write_all(&[0x03]).expect("send ^C");

        let status = pty.wait_for_child(Some(std::time::Duration::from_secs(5)));
        assert!(
            matches!(status, ChildStatus::Exited(_)),
            "^C must kill the child; still running means it has no controlling \
             terminal and job control is broken: {status:?}"
        );
    }

    /// A resize the child never hears about is a resize that did not happen.
    ///
    /// `tcsetwinsize` is only half the job on paper — the size is stored, but
    /// full-screen programs redraw on `SIGWINCH`, not on polling. The kernel
    /// raises it for us as part of `TIOCSWINSZ`; this test is what stops that
    /// assumption from silently becoming false.
    #[test]
    fn a_resize_raises_sigwinch_in_the_child() {
        let spec = CommandSpec {
            // The trap fires between commands, which is why the child loops on
            // short sleeps instead of one long one.
            command_line: "/bin/sh -c 'trap \"stty size\" WINCH; i=0; while [ $i -lt 50 ]; \
                           do sleep 0.1; i=$((i+1)); done'"
                .into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/sh");
        let mut reader = pty.take_reader().expect("reader");

        let reading = std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&out).contains("30 90") {
                    break;
                }
            }
            out
        });

        std::thread::sleep(std::time::Duration::from_millis(400));
        pty.resize(PtySize::new(90, 30)).expect("resize");

        let out = reading.join().expect("reader thread");
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("30 90"),
            "the child must be signalled, not just have its winsize updated: {text:?}"
        );
    }

    #[test]
    fn resize_is_visible_to_the_child() {
        let spec = CommandSpec {
            command_line: "/bin/sh -c 'stty size'".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(101, 37)).expect("spawn /bin/sh");
        let mut reader = pty.take_reader().expect("reader");

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("37 101"),
            "the initial winsize must reach the child before it runs, or every \
             full-screen program starts at the wrong size: {text:?}"
        );
    }

    /// Sharp edge 5, in the exact shape a daemon produces.
    ///
    /// A reader thread parked in `read` holds a duplicate of the master, so the
    /// implicit hangup can never fire: the duplicate is not released until the
    /// read returns, and the read does not return until the hangup. This test
    /// therefore drops the `UnixPty` first — which is all the session layer used
    /// to do — and asserts the reader is *still* parked, before proving that
    /// `hangup` on a clone of the transport unparks it.
    #[test]
    fn hangup_ends_a_child_whose_reader_is_parked() {
        let spec = CommandSpec {
            command_line: "/bin/sleep 30".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/sleep");
        let mut reader = pty.take_reader().expect("reader");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            // `sleep` writes nothing, so this parks until the pty hangs up.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            let _ = tx.send(());
        });

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            rx.try_recv().is_err(),
            "the reader must still be parked; if it is not, this test is no \
             longer reproducing the case it exists for"
        );

        pty.hangup();

        assert!(
            matches!(pty.wait_for_child(Some(std::time::Duration::from_secs(5))), ChildStatus::Exited(_)),
            "hangup must end the child. Dropping the transport cannot: the \
             parked reader holds the last duplicate of the master hostage"
        );
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "the reader must reach EOF, or its thread and the terminal behind \
             it leak for the lifetime of the daemon"
        );
    }

    /// Nothing the shell started survives the hangup.
    ///
    /// Asserted on the grandchild's pid directly, because "the reader unparked"
    /// cannot tell *everything* going from *the one process we knew about*
    /// going.
    ///
    /// **This does not discriminate `kill_process_group` from `kill_process`,
    /// and is not evidence for that choice** — it passes under both. When a
    /// session leader dies the kernel hups the terminal's foreground process
    /// group itself, and with job control off that group is the shell's, so the
    /// cascade happens either way. Recorded here so the test is not later read
    /// as proof of something it cannot see.
    #[test]
    fn hangup_ends_everything_the_shell_started() {
        let spec = CommandSpec {
            // `exec` on the last line is what makes this discriminating: the
            // shell replaces itself, so the group leader is a `sleep` that will
            // not forward SIGHUP to anything. With a *shell* still sitting there
            // the test passes either way, because a shell hups its own jobs on
            // the way out and does the group's work for us.
            command_line: "/bin/sh -c 'sleep 30 & echo pid=$!; exec sleep 30'".into(),
            cwd: None,
            env: crate::terminal_env(),
        };
        let mut pty = UnixPty::spawn(&spec, PtySize::new(80, 24)).expect("spawn /bin/sh");
        let mut reader = pty.take_reader().expect("reader");

        // The pid of the backgrounded sleep, straight from the shell.
        let mut out = String::new();
        let mut buf = [0u8; 256];
        let grandchild = loop {
            let n = reader.read(&mut buf).expect("read pid from the shell");
            assert!(n > 0, "the shell exited before printing a pid");
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(rest) = out.split("pid=").nth(1) {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if out.contains('\n') && !digits.is_empty() {
                    break digits.parse::<i32>().expect("a pid is a number");
                }
            }
        };

        let pid = rustix::process::Pid::from_raw(grandchild).expect("a live pid is non-zero");
        assert!(
            rustix::process::test_kill_process(pid).is_ok(),
            "the grandchild should be running before the hangup; without that \
             this test asserts nothing"
        );

        pty.hangup();

        // It is not our child, so it is reaped by init rather than by us --
        // poll instead of waiting on it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell's background job survived the hangup; a session \
                 that half-ends is worse than one that does not end at all, \
                 because nothing is left to show for it"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
