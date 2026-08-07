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
//!    misdiagnosed here. [`PtyReader`] maps it to `Ok(0)`, mirroring what the
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

/// The read half of the pty. Blocking.
pub struct PtyReader(File);

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            // Gotcha 2: the child exiting is EOF, not a failure.
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => Ok(0),
            other => other,
        }
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

        let reader = PtyReader(File::from(master.try_clone().map_err(PtyError::Create)?));

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
}

impl PtyTransport for UnixPty {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take().map(|r| Box::new(r) as Box<dyn Read + Send>)
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
}

// Dropping the last master fd hangs up the pty, which sends SIGHUP to the
// child's session — so there is no explicit kill here, and none is wanted: a
// shell asked to hang up gets to run its exit traps. The reader and writer hold
// their own duplicates, so the hangup lands when the last of them is dropped,
// not when `UnixPty` is.

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
}
