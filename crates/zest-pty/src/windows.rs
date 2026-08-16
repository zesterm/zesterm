//! ConPTY.
//!
//! # The sharp edges, and how each is handled
//!
//! These are the reasons this is written directly rather than via a wrapper
//! crate — every one of them needs control over ordering or handle lifetime.
//!
//! 1. **`ClosePseudoConsole` deadlocks** if the output pipe has stopped being
//!    drained ([microsoft/terminal#17688]). ConPTY flushes a final frame on
//!    close and blocks until it is consumed. Handled by [`ConPty::drop`]:
//!    the read handle is deliberately *not* closed first, and shutdown is
//!    ordered close-pseudoconsole → drop pipe handles.
//!
//! 2. **The child-side handles must be closed in the parent immediately after
//!    `CreateProcessW`**, or the pipe never reaches EOF and the reader thread
//!    hangs forever after the shell exits. Handled in [`ConPty::spawn`] by
//!    dropping `child_read`/`child_write` as soon as the process is created.
//!
//! 2b. **Even then, the reader does not see EOF when the child exits.** ConPTY
//!    keeps its own copy of the output pipe's write end for as long as the
//!    `HPCON` lives, so a blocked `ReadFile` stays blocked after the shell is
//!    gone. Only `ClosePseudoConsole` releases it. Combined with (1), this
//!    dictates the whole shutdown protocol:
//!
//!    ```text
//!    reader thread          owner thread
//!    ------------           ------------
//!    ReadFile (blocks)      wait_for_child()         <- child exits
//!         .                 drop(ConPty)
//!         .                   -> ClosePseudoConsole  <- flushes final frame,
//!         .                                             blocks until drained
//!    reads final frame  <---------'
//!    ReadFile -> EOF    <--  pipe write end released
//!    thread exits             drop returns
//!    ```
//!
//!    So the reader must still be draining while the pseudoconsole is closed.
//!    Closing from the reader's own thread, or dropping the reader first,
//!    deadlocks. Use [`ConPty::wait_for_child`] from the owning thread and let
//!    the reader hit EOF on its own.
//!
//! 2c. **Output is asynchronous to child exit.** ConPTY renders the console
//!    buffer to VT on its own schedule, so a short-lived command can exit
//!    *before* its output has been painted into the pipe. Closing the
//!    pseudoconsole the instant `wait_for_child` returns therefore truncates
//!    the tail. Interactive shells never expose this because the session
//!    outlives any single command, but `zesterm -e <cmd>` and the test corpus
//!    recorder both do. Drain until the output goes quiet rather than closing
//!    on process exit alone.
//!
//! 3. **`ResizePseudoConsole` makes ConPTY re-emit the entire screen.** That is
//!    a large byte burst plus visible flicker; the damage model has to absorb
//!    it. Nothing to do here beyond knowing it happens.
//!
//! 4. **win32-input-mode** (`CSI ? 9001 h`) is what delivers key-up events and
//!    modifier-only keypresses to console applications. Enabled by the terminal
//!    layer, not here, but this is where it matters.
//!
//! 5. **`STARTF_USESTDHANDLES` with null handles is required**, or the child
//!    writes to the parent's stdout instead of the pty whenever ours is
//!    redirected — which, for a terminal emulator, is always. Every API call
//!    still reports success; only the output goes missing. See the comment in
//!    [`spawn_child`], which is where the reasoning lives.
//!
//! 6. `CreatePseudoConsole` rejects a zero dimension, so sizes are clamped.
//!
//! [microsoft/terminal#17688]: https://github.com/microsoft/terminal/issues/17688

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, FALSE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::{CommandSpec, PtyError, PtySize, PtyTransport};

// `ChildStatus` moved up to the crate root when the unix backend needed the
// same type. Re-exported here so `zest_pty::windows::ChildStatus` keeps
// resolving for anything already naming it.
pub use crate::ChildStatus;

/// An owned Win32 handle that closes exactly once.
#[derive(Debug)]
struct OwnedHandle(HANDLE);

// SAFETY: a Win32 HANDLE is just a kernel object index. The kernel serializes
// operations on it, and this type guarantees a single close.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }

    /// Duplicate for a second owner. Used to hand a writer to another thread
    /// without either side being able to close the other's handle.
    fn try_clone(&self) -> io::Result<Self> {
        let mut dup: HANDLE = ptr::null_mut();
        // SAFETY: self.0 is a live handle owned by this process; `dup` is a
        // valid out-pointer.
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.0,
                GetCurrentProcess(),
                &mut dup,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(dup))
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle is live and owned; this runs exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// When the reader last saw a byte, so the exit watcher can tell "the child
/// is gone" from "the child is gone *and* its output has been painted".
///
/// ConPTY renders the console buffer to VT on its own schedule (gotcha 2c), so
/// a command can exit before its last line reaches the pipe. Only the reader
/// knows when that stopped happening.
#[derive(Debug)]
struct LastRead {
    started: std::time::Instant,
    at_ms: std::sync::atomic::AtomicU64,
}

impl LastRead {
    fn new() -> Self {
        Self { started: std::time::Instant::now(), at_ms: std::sync::atomic::AtomicU64::new(0) }
    }

    fn touch(&self) {
        let ms = self.started.elapsed().as_millis() as u64;
        self.at_ms.store(ms, Ordering::Relaxed);
    }

    /// How long the stream has been silent.
    fn quiet_for(&self) -> std::time::Duration {
        let now = self.started.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(now.saturating_sub(self.at_ms.load(Ordering::Relaxed)))
    }
}

/// The read half of the pty. Blocking.
pub struct PtyReader {
    handle: OwnedHandle,
    last: Arc<LastRead>,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;

        let mut read = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: handle is live; buf is valid for `len` bytes.
        let ok =
            unsafe { ReadFile(self.handle.raw(), buf.as_mut_ptr(), len, &mut read, ptr::null_mut()) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            // The child exiting closes its end. That is EOF, not a failure --
            // reporting it as an error would make every clean shell exit look
            // like a crash.
            if err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(err);
        }
        if read > 0 {
            self.last.touch();
        }
        Ok(read as usize)
    }
}

/// The write half of the pty.
pub struct PtyWriter(OwnedHandle);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;

        let mut written = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        // SAFETY: handle is live; buf is valid for `len` bytes.
        let ok = unsafe { WriteFile(self.0.raw(), buf.as_ptr(), len, &mut written, ptr::null_mut()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Pipe writes are not buffered on our side.
        Ok(())
    }
}

/// The pseudoconsole handle, shared so `resize` can be called from the UI thread
/// while the reader thread holds the pty alive.
#[derive(Debug)]
struct Pseudoconsole {
    handle: HPCON,
    /// Set by whoever closes it, so `hangup` and `drop` cannot both close.
    closed: AtomicBool,
}

// SAFETY: HPCON is an opaque kernel-managed pointer. ResizePseudoConsole and
// ClosePseudoConsole are internally synchronized by conhost.
unsafe impl Send for Pseudoconsole {}
unsafe impl Sync for Pseudoconsole {}

impl Pseudoconsole {
    /// Close it, exactly once, from whichever thread got here first.
    ///
    /// **Must not be called from the reader thread**, and the reader must still
    /// be draining — see gotcha 2b. Both hold for the two callers: `hangup`,
    /// which the session layer calls from a connection thread, and `drop`.
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // ORDER MATTERS. ClosePseudoConsole flushes a final frame and blocks
        // until it is drained, so the read handle must still be open and, ideally,
        // still being read. Closing the reader first deadlocks here
        // (microsoft/terminal#17688). ConPty's field order guarantees this runs
        // before the pipe handles drop.
        // SAFETY: the HPCON is live and closed exactly once, guarded above.
        unsafe { ClosePseudoConsole(self.handle) };
    }
}

impl Drop for Pseudoconsole {
    fn drop(&mut self) {
        self.close();
    }
}

/// A ConPTY session and its child process.
///
/// **Field order is load-bearing** — Rust drops fields in declaration order, and
/// `pcon` must close before the pipe handles. See [`Pseudoconsole::drop`].
pub struct ConPty {
    pcon: Arc<Pseudoconsole>,
    reader: Option<PtyReader>,
    writer: OwnedHandle,
    process: OwnedHandle,
    _thread: OwnedHandle,
    /// Shared with the reader; read by [`PtyTransport::watch_exit`].
    last_read: Arc<LastRead>,
    /// The child's process tree. **Declared last on purpose**: it carries
    /// `KILL_ON_JOB_CLOSE`, so dropping it is a kill, and it must not fire
    /// until `pcon` above has closed and flushed (gotcha 1). Putting this
    /// first would kill the shell mid-flush — a new route into the documented
    /// deadlock.
    _job: Option<OwnedHandle>,
}

impl ConPty {
    /// Create a pseudoconsole and spawn a command inside it.
    pub fn spawn(cmd: &CommandSpec, size: PtySize) -> Result<Self, PtyError> {
        // Two pipes: one carries our writes to the child, one carries the
        // child's output to us. ConPTY takes the ends it needs.
        let (child_read, our_write) = create_pipe().map_err(PtyError::Create)?;
        let (our_read, child_write) = create_pipe().map_err(PtyError::Create)?;

        // CreatePseudoConsole rejects zero in either dimension.
        let coord = COORD { X: size.cols.max(1) as i16, Y: size.rows.max(1) as i16 };
        // HPCON is an opaque isize in windows-sys, not a pointer.
        let mut hpcon: HPCON = 0;
        // SAFETY: both handles are live; hpcon is a valid out-pointer.
        let hr = unsafe {
            CreatePseudoConsole(coord, child_read.raw(), child_write.raw(), 0, &mut hpcon)
        };
        if hr != 0 {
            return Err(PtyError::Create(io::Error::from_raw_os_error(hr)));
        }
        let pcon = Arc::new(Pseudoconsole { handle: hpcon, closed: AtomicBool::new(false) });

        // Everything the shell starts joins this, so `hangup` can reap the
        // tree rather than just the shell.
        let mut job = create_job()
            .map_err(|e| tracing::warn!(error = %e, "no job object; a hangup will leak grandchildren"))
            .ok();

        let process = match spawn_child(cmd, hpcon, job.as_ref()) {
            Ok(p) => p,
            // A daemon that is itself inside a job — VS Code's terminal does
            // this, and so do some CI runners — can have nested assignment
            // refused outright. A shell with a leaky process tree beats a
            // terminal that will not open, so drop the job and try once more.
            //
            // `job` is cleared as well as unused: keeping a job the child is
            // not in would make `hangup` terminate an empty one and believe it
            // had done something.
            Err(e) if job.is_some() => {
                tracing::warn!(
                    error = %e,
                    "spawning into a job failed; retrying without one (a hangup will leak grandchildren)"
                );
                job = None;
                spawn_child(cmd, hpcon, None).map_err(|source| PtyError::Spawn {
                    command: cmd.command_line.clone(),
                    source,
                })?
            }
            Err(source) => {
                return Err(PtyError::Spawn { command: cmd.command_line.clone(), source })
            }
        };

        // Close our copies of the child's ends NOW. ConPTY duplicated what it
        // needs, and the child inherited its own. If these stay open, the read
        // pipe never reaches EOF and the reader thread hangs forever after the
        // shell exits.
        drop(child_read);
        drop(child_write);

        let last_read = Arc::new(LastRead::new());
        Ok(Self {
            pcon,
            reader: Some(PtyReader { handle: our_read, last: Arc::clone(&last_read) }),
            writer: our_write,
            process: process.0,
            _thread: process.1,
            last_read,
            _job: job,
        })
    }

    /// Block until the child exits, or until `timeout` elapses.
    ///
    /// This is how the owning thread learns the session is over, because the
    /// reader cannot tell it — see gotcha 2b in the module docs. Once this
    /// returns [`ChildStatus::Exited`], drop the `ConPty` (which closes the
    /// pseudoconsole) and *then* join the reader thread.
    pub fn wait_for_child(&self, timeout: Option<std::time::Duration>) -> ChildStatus {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, WaitForSingleObject, INFINITE,
        };

        let ms = timeout.map_or(INFINITE, |d| {
            u32::try_from(d.as_millis()).unwrap_or(INFINITE)
        });

        // SAFETY: the process handle is live for as long as `self` is.
        match unsafe { WaitForSingleObject(self.process.raw(), ms) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: live handle; `code` is a valid out-pointer.
                unsafe { GetExitCodeProcess(self.process.raw(), &mut code) };
                ChildStatus::Exited(code)
            }
            WAIT_TIMEOUT => ChildStatus::StillRunning,
            _ => {
                tracing::warn!(error = %io::Error::last_os_error(), "waiting on child failed");
                ChildStatus::StillRunning
            }
        }
    }
}

impl PtyTransport for ConPty {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take().map(|r| Box::new(r) as Box<dyn Read + Send>)
    }

    /// See [`PtyTransport::exit_code`].
    ///
    /// A zero timeout makes `WaitForSingleObject` a poll: it answers
    /// `WAIT_TIMEOUT` immediately for a live child rather than blocking. The
    /// process handle outlives the child, so `GetExitCodeProcess` keeps
    /// answering afterwards and this stays callable however late it is asked.
    ///
    /// **The cast is deliberate.** A Windows exit code is a `u32` and an
    /// abnormal one is an `NTSTATUS` above `i32::MAX` — an access violation is
    /// `0xC0000005`. Wrapping it into `i32` produces `-1073741819`, which is
    /// the spelling Windows itself prints and the one a person searching for it
    /// will recognise. Saturating instead would map every distinct crash onto
    /// `i32::MAX`.
    fn exit_code(&self) -> Option<i32> {
        match self.wait_for_child(Some(std::time::Duration::ZERO)) {
            ChildStatus::Exited(code) => Some(code as i32),
            ChildStatus::StillRunning => None,
        }
    }

    fn writer(&self) -> Box<dyn Write + Send> {
        // Duplicating rather than sharing means the writer thread owning its
        // handle cannot close ours, and vice versa.
        match self.writer.try_clone() {
            Ok(h) => Box::new(PtyWriter(h)),
            Err(e) => {
                tracing::error!(error = %e, "could not duplicate pty write handle");
                Box::new(io::sink())
            }
        }
    }

    fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        let coord = COORD { X: size.cols.max(1) as i16, Y: size.rows.max(1) as i16 };
        // SAFETY: the HPCON is live for as long as `self` is.
        let hr = unsafe { ResizePseudoConsole(self.pcon.handle, coord) };
        if hr != 0 {
            return Err(PtyError::Resize(io::Error::from_raw_os_error(hr)));
        }
        // Caller beware: ConPTY responds by re-emitting the whole screen — so
        // resize the grid *first* and do not hold its lock across this call.
        // See `zest_daemon::session::Session::resize`, which carries both
        // halves of the reasoning and the test that pins them. (#200)
        Ok(())
    }

    /// Yes, and it is why `restates_on_resize` exists.
    ///
    /// ConPTY's buffer is only as tall as the viewport, so a resize both
    /// discards what no longer fits and repaints what does — the repaint has
    /// the last word on every visible row. (#200)
    fn restates_on_resize(&self) -> bool {
        true
    }

    fn hangup(&self) {
        // This *is* the shutdown protocol of gotcha 2b, run deliberately rather
        // than as a side effect of drop timing, and both of its preconditions
        // hold: the caller is the session layer's thread, never the reader, and
        // the reader is still parked in `ReadFile` draining the final frame.
        //
        // Closing the pseudoconsole is also the only thing that releases
        // ConPTY's copy of the pipe's write end, so it is what lets the reader
        // reach EOF at all. Dropping `ConPty` would do the same — but only if
        // this `Arc` were the last, which the session layer cannot promise while
        // a listing or a poll may hold a clone.
        self.pcon.close();

        // ConPTY signals its clients and they normally leave at once. The
        // escalation mirrors the unix backend's SIGHUP → SIGKILL: a program that
        // declines to exit still has to, since the user asked for this session
        // to end and something must reach the child that ignores the close.
        let left_politely = matches!(self.wait_for_child(Some(HANGUP_GRACE)), ChildStatus::Exited(_));

        // The job goes regardless of whether the *shell* left politely, and
        // that distinction is the whole point of having one.
        //
        // `ClosePseudoConsole` signals the console's attached clients — the
        // shell. It says nothing to anything the shell started detached, so a
        // well-behaved shell exits on cue and its background `ping` keeps
        // running. Returning early there is what made this leak: unix does not
        // have the choice, because `SIGHUP` goes to the process *group*.
        //
        // conhost is deliberately not in the job: `CreatePseudoConsole` starts
        // it from *this* process before `CreateProcessW`, so only the shell and
        // its descendants were ever assigned. Killing conhost out from under
        // `ClosePseudoConsole` is precisely gotcha 1 — and the close above has
        // already returned by here, so its flush is done either way.
        if let Some(job) = self._job.as_ref() {
            // SAFETY: a live job handle we own.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(job.raw(), 1) };
            return;
        }
        if left_politely {
            return;
        }
        tracing::info!("child outlived the pseudoconsole closing; terminating");
        // SAFETY: the process handle is live for as long as `self` is.
        unsafe { windows_sys::Win32::System::Threading::TerminateProcess(self.process.raw(), 1) };
    }

    /// Notice a shell that exits on its own.
    ///
    /// Without this the reader never learns anything: ConPTY holds the output
    /// pipe's write end until the pseudoconsole closes (gotcha 2b), so EOF
    /// cannot be the signal, and until now no `Exited` ever reached a client on
    /// Windows — a tab whose shell had exited simply stayed, and the daemon
    /// kept the session with its whole scrollback forever.
    ///
    /// # The three constraints, and how each is met
    ///
    /// A previous attempt at this hung Windows CI for over an hour and was
    /// backed out. It was later established that the hang was something else
    /// entirely — a test asking for `/bin/echo` behind a decorative timeout —
    /// so the code was never shown wrong. It was, however, incomplete, and
    /// re-landing it verbatim would have been wrong for a reason nobody had
    /// stated. All three of these have to hold at once:
    ///
    /// 1. **The signal has to arrive.** `WaitForSingleObject` on a duplicate of
    ///    the process handle, on its own thread: the OS's own wait, never a
    ///    poll, so an idle session costs one parked thread and no wakeups.
    ///    Two waiters on one Windows process handle is legal and free — which
    ///    is exactly why unix keeps the no-op default, where a second waiter
    ///    would need the same child mutex `hangup` holds.
    ///
    /// 2. **The reader must still be draining when the pseudoconsole closes**
    ///    (gotcha 1). This function *never touches the HPCON*. The only closers
    ///    stay what they were: [`ConPty::hangup`], called from the session
    ///    layer's thread, and `Pseudoconsole::drop`. The watcher's entire job
    ///    is to set a flag and call a waker. **A future edit that closes
    ///    anything from here reintroduces the documented deadlock.**
    ///
    /// 3. **Closing on process exit alone truncates the tail** (gotcha 2c) —
    ///    ConPTY paints the console buffer to VT on its own schedule, so a
    ///    command can be gone before its last line is in the pipe. This is the
    ///    part the earlier attempt was missing: `exited` going true is what
    ///    eventually reaches `ClosePseudoConsole` through the registry sweep,
    ///    so reporting it early cuts off the output. The watcher waits for the
    ///    stream to go quiet first.
    ///
    /// The quiet wait *is* a poll, and [`PtyTransport::watch_exit`]'s contract
    /// says not to poll. The contract means "do not run a per-session timer" —
    /// that is what costs the 0%-idle guarantee. This loop runs **once**, only
    /// **after** the child is already dead, is bounded at
    /// [`EXIT_DRAIN_MAX`], and then the thread exits. A session that lives for
    /// eight hours polls zero times.
    fn watch_exit(&self, on_exit: Box<dyn FnOnce() + Send>) {
        let process = match self.process.try_clone() {
            Ok(h) => h,
            Err(e) => {
                // Not fatal: the session simply keeps the pre-existing
                // behaviour of never noticing. Saying so beats a silent
                // regression to it.
                tracing::warn!(error = %e, "cannot watch for child exit; the session will not self-close");
                return;
            }
        };
        let last = Arc::clone(&self.last_read);
        let spawned = std::thread::Builder::new()
            .name("zest-pty-exit".into())
            .spawn(move || {
                use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
                // SAFETY: our own duplicate of a live process handle, owned by
                // this closure for the whole wait.
                unsafe { WaitForSingleObject(process.raw(), INFINITE) };

                // The child is gone; its output may not all be here yet.
                let deadline = std::time::Instant::now() + EXIT_DRAIN_MAX;
                while last.quiet_for() < EXIT_QUIET && std::time::Instant::now() < deadline {
                    std::thread::sleep(EXIT_DRAIN_TICK);
                }
                on_exit();
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot spawn the exit watcher; the session will not self-close");
        }
    }
}

/// How long the output must be silent before an exited child is reported.
///
/// Matches [`HANGUP_GRACE`], for the same reason: it is how long ConPTY is
/// given to finish saying what it had to say.
const EXIT_QUIET: std::time::Duration = std::time::Duration::from_millis(150);
/// The ceiling on that wait, so a chatty program that exits while still being
/// painted cannot hold a session open indefinitely.
const EXIT_DRAIN_MAX: std::time::Duration = std::time::Duration::from_secs(2);
/// Granularity of the drain wait. Coarse on purpose — it runs once, after the
/// child is dead, and nothing is waiting on the difference.
const EXIT_DRAIN_TICK: std::time::Duration = std::time::Duration::from_millis(25);

/// How long a child gets to leave on its own before it is terminated.
///
/// Matches the unix backend's grace so that "closing a session" costs the same
/// on both, and short enough that it is not perceived as a hang.
const HANGUP_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    // SAFETY: both are valid out-pointers; null attributes means default
    // security and non-inheritable, which is what we want -- ConPTY duplicates
    // the handles it passes to the child itself.
    let ok = unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// A job object the child and everything it starts belong to.
///
/// `TerminateProcess` reaches the process it names and nothing else, so without
/// this a `hangup` killed the shell and left its grandchildren running — where
/// the unix backend's `kill_process_group` reaps the lot. That asymmetry is why
/// `hangup_ends_everything_the_shell_started` had no Windows counterpart.
///
/// `KILL_ON_JOB_CLOSE` means the handle dropping is itself the kill, which is
/// why `ConPty`'s field order puts this **last**: it must not fire until
/// `ClosePseudoConsole` has flushed its final frame (gotcha 1).
fn create_job() -> io::Result<OwnedHandle> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        SetInformationJobObject,
    };

    // SAFETY: null attributes and no name is the documented anonymous form.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = OwnedHandle(job);

    // SAFETY: a zeroed limit struct is valid; we set exactly one flag.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: live job handle; `info` is correctly sized for the class.
    let ok = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// `PROC_THREAD_ATTRIBUTE_JOB_LIST`, absent from windows-sys.
const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002_000D;

/// `CreateProcessW` with the pseudoconsole attached via the extended startup info.
///
/// `job`, when present, is applied through the same attribute list rather than
/// by `AssignProcessToJobObject` after the fact: assignment is then atomic with
/// creation, so the child cannot spawn a grandchild in the gap. The
/// `CREATE_SUSPENDED` + assign + `ResumeThread` alternative has exactly that
/// race, and leaks a suspended process on any error path between the two calls.
fn spawn_child(
    cmd: &CommandSpec,
    hpcon: HPCON,
    job: Option<&OwnedHandle>,
) -> io::Result<(OwnedHandle, OwnedHandle)> {
    // Size the attribute list, then allocate. The first call is expected to
    // fail with ERROR_INSUFFICIENT_BUFFER; it reports the size through `bytes`.
    let count = if job.is_some() { 2 } else { 1 };
    let mut bytes: usize = 0;
    // SAFETY: passing null is the documented sizing call.
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut bytes) };
    if bytes == 0 {
        return Err(io::Error::last_os_error());
    }

    // ALIGNMENT MATTERS, and getting it wrong fails *silently*.
    //
    // PROC_THREAD_ATTRIBUTE_LIST is an opaque struct of pointer-sized fields and
    // needs pointer alignment. A `Vec<u8>` is allocated with align 1, so the
    // buffer can land on an odd address. When it does, both
    // InitializeProcThreadAttributeList and UpdateProcThreadAttribute still
    // report success, CreateProcessW still succeeds -- and the pseudoconsole
    // attribute is simply ignored. The child then inherits the parent's stdout
    // instead of attaching to the pty, so output goes to the wrong place with
    // no error anywhere. Allocating as `Vec<usize>` guarantees the alignment.
    let words = bytes.div_ceil(std::mem::size_of::<usize>());
    let mut attr_buf: Vec<usize> = vec![0usize; words];
    let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    // SAFETY: attr_list points at `bytes` writable bytes, as just sized.
    if unsafe { InitializeProcThreadAttributeList(attr_list, count, 0, &mut bytes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // From here on the list must be deleted before returning. `AttrListGuard`
    // does that even on the error paths below.
    let _guard = AttrListGuard(attr_list);

    // The PSEUDOCONSOLE attribute takes the HPCON *value* as lpValue, not its
    // address -- the handle is passed by value the way a pointer-sized token is.
    // SAFETY: attr_list is initialized; hpcon is live and outlives this call.
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpcon as *mut core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    // The job list takes a *pointer to an array* of handles, unlike the
    // pseudoconsole attribute above which takes its handle by value. The array
    // must outlive `CreateProcessW`, which is why it is bound here rather than
    // written inline — the same class of lifetime trap as the alignment one.
    let job_handles: [HANDLE; 1] = [job.map_or(ptr::null_mut(), OwnedHandle::raw)];
    if job.is_some() {
        // SAFETY: attr_list is initialized with room for two; `job_handles`
        // lives until after CreateProcessW below.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                job_handles.as_ptr() as *mut core::ffi::c_void,
                std::mem::size_of::<HANDLE>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;

    // This is gotcha 5, and it is the one that cost the most to find.
    //
    // Without STARTF_USESTDHANDLES, Windows propagates *the parent's* standard
    // handle values into the child's process parameters, and those take
    // precedence over the pseudoconsole. So whenever our own stdout is
    // redirected -- under `cargo test`, in any shell pipeline, or in a GUI
    // process with no console -- the child writes there instead of into the
    // pty. Everything reports success: CreatePseudoConsole, the attribute list,
    // UpdateProcThreadAttribute, and CreateProcessW all return cleanly, and the
    // pseudoconsole even emits its startup and teardown frames. Only the
    // child's actual output is missing.
    //
    // Microsoft's ConPTY sample never shows this because it is run from an
    // interactive console, where the parent's std handles already *are* the
    // console. For a terminal emulator -- which is exactly a process whose
    // stdout is not a console -- it is the normal case.
    //
    // Declaring STARTF_USESTDHANDLES with null handles suppresses the
    // propagation, and the child falls back to its console, which is the pty.
    si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = ptr::null_mut();
    si.StartupInfo.hStdOutput = ptr::null_mut();
    si.StartupInfo.hStdError = ptr::null_mut();

    // CreateProcessW may modify the command line buffer in place, so it must be
    // a writable, NUL-terminated copy.
    let mut command_line = to_wide(OsStr::new(&cmd.command_line));
    let cwd = cmd.cwd.as_ref().map(|p| to_wide(p.as_os_str()));
    let env_block = build_env_block(&cmd.env);

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: command_line is writable and NUL-terminated; si is correctly
    // sized with a live attribute list; pi is a valid out-pointer.
    let ok = unsafe {
        CreateProcessW(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE, // no handle inheritance -- ConPTY passes what the child needs
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env_block
                .as_ref()
                .map_or(ptr::null(), |b| b.as_ptr().cast()),
            cwd.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
            &mut si as *mut STARTUPINFOEXW as *mut _,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok((OwnedHandle(pi.hProcess), OwnedHandle(pi.hThread)))
}

const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;

struct AttrListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);

impl Drop for AttrListGuard {
    fn drop(&mut self) {
        // SAFETY: the list was successfully initialized and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.0) };
    }
}

fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Build a UTF-16 environment block: `KEY=VAL\0KEY=VAL\0\0`.
///
/// Returns `None` when there are no overrides, so the child simply inherits.
fn build_env_block(extra: &[(String, String)]) -> Option<Vec<u16>> {
    if extra.is_empty() {
        return None;
    }

    // Start from the parent's environment; overrides replace case-insensitively,
    // because Windows environment variable names are case-insensitive and a
    // duplicate `Path`/`PATH` pair produces genuinely confusing behavior.
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in extra {
        // An empty value means *unset*, not "set to the empty string". Clearing
        // an inherited terminal identity needs the variable to be genuinely
        // absent: the common test is `if os.Getenv("WT_SESSION") != ""` but
        // plenty of code only checks for presence, and that code would still
        // take the wrong branch.
        if v.is_empty() {
            vars.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
        } else if let Some(slot) = vars.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(k)) {
            slot.1 = v.clone();
        } else {
            vars.push((k.clone(), v.clone()));
        }
    }

    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Render control bytes visibly.
    ///
    /// A failing pty test must never dump raw VT into the developer's terminal:
    /// the stream contains `ESC[2J` and `ESC[H`, which clear the screen and
    /// home the cursor, scrambling the test output so thoroughly that the
    /// failure looks like something else entirely. (Learned the hard way.)
    fn visible(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            match b {
                0x1b => s.push_str("<ESC>"),
                b'\n' => s.push_str("<LF>"),
                b'\r' => s.push_str("<CR>"),
                0x07 => s.push_str("<BEL>"),
                0x00..=0x1f | 0x7f => s.push_str(&format!("<{b:02x}>")),
                _ => s.push(b as char),
            }
        }
        s
    }

    /// The full session lifecycle, in the exact shape the app will use it.
    ///
    /// This is the test that catches the ConPTY deadlock, and it is written to
    /// *fail* rather than hang: the whole thing runs on a worker thread with a
    /// hard timeout, because a deadlock here would otherwise wedge CI forever.
    ///
    /// The ordering being verified is the one described in module gotcha 2b:
    /// the reader keeps draining while the owner closes the pseudoconsole.
    /// Reversing those two deadlocks.
    #[test]
    fn full_session_lifecycle_shuts_down_cleanly() {
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let cmd = CommandSpec {
                command_line: "cmd.exe /c echo zesterm-probe".into(),
                cwd: None,
                env: Vec::new(),
            };
            let mut pty = match ConPty::spawn(&cmd, PtySize::new(80, 24)) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Err(format!("spawn failed: {e}")));
                    return;
                }
            };
            let mut reader = pty.take_reader().expect("reader is available exactly once");

            // Reader thread: drain to EOF, publishing as it goes. It cannot
            // know when the child exits; it finds out because
            // ClosePseudoConsole releases the pipe.
            let seen = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            let seen_writer = Arc::clone(&seen);
            let reader_thread = std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => seen_writer.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(e) => return Err(format!("read failed: {e}")),
                    }
                }
                Ok(())
            });

            // Wait for the child, then keep draining until the marker actually
            // appears -- process exit does not mean ConPTY has painted yet
            // (gotcha 2c). Polling for the real signal rather than sleeping a
            // fixed interval keeps this from being flaky on a loaded machine.
            let status = pty.wait_for_child(Some(Duration::from_secs(15)));
            if status == ChildStatus::StillRunning {
                let _ = tx.send(Err("child did not exit within 15s".into()));
                return;
            }

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                let got = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
                if got.contains("zesterm-probe") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            // Close the pseudoconsole while the reader is still blocked in
            // ReadFile. This is the ordering under test.
            drop(pty);

            match reader_thread.join() {
                Ok(Ok(())) => {
                    let out = seen.lock().unwrap().clone();
                    let _ = tx.send(Ok(out));
                }
                Ok(Err(e)) => {
                    let _ = tx.send(Err(e));
                }
                Err(_) => {
                    let _ = tx.send(Err("reader thread panicked".into()));
                }
            }
        });

        match rx.recv_timeout(Duration::from_secs(45)) {
            Ok(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                assert!(
                    text.contains("zesterm-probe"),
                    "expected the echoed marker in the ConPTY stream, got {} bytes:\n{}",
                    bytes.len(),
                    visible(&bytes)
                );
                // ConPTY announces win32-input-mode and focus reporting as it
                // starts up. Their presence confirms we are talking to a real
                // pseudoconsole rather than an inherited console.
                assert!(
                    text.contains("\x1b[?9001h"),
                    "expected ConPTY's win32-input-mode announcement, got:\n{}",
                    visible(&bytes)
                );
            }
            Ok(Err(e)) => panic!("{e}"),
            Err(_) => panic!(
                "the ConPTY session never finished. Almost certainly one of:\n \
                 - ClosePseudoConsole deadlocked because the reader stopped draining \
                   (microsoft/terminal#17688), or\n \
                 - the child-side pipe handles were not closed after CreateProcessW, so the \
                   reader never reached EOF.\n\
                 See the shutdown protocol in this module's docs."
            ),
        }
    }

    /// The gap that made `exit` do nothing on Windows.
    ///
    /// The reader cannot report this — ConPTY holds the pipe's write end until
    /// the pseudoconsole closes (gotcha 2b) — so without `watch_exit` no
    /// `Exited` ever reached a client here and the daemon kept the session
    /// forever. Written through a channel with a deadline, like everything else
    /// in this module: a regression here is a deadlock, and a deadlock that
    /// fails a `join()` tells you nothing at all.
    #[test]
    fn a_child_that_exits_on_its_own_is_noticed() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let cmd = CommandSpec {
                command_line: "cmd.exe /c echo watch-probe".into(),
                cwd: None,
                env: Vec::new(),
            };
            let mut pty = match ConPty::spawn(&cmd, PtySize::new(80, 24)) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Err(format!("spawn failed: {e}")));
                    return;
                }
            };
            let mut reader = pty.take_reader().expect("reader is available exactly once");
            let seen = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            let seen_writer = Arc::clone(&seen);
            // The reader must keep draining throughout: that is the
            // precondition `watch_exit` promises not to break.
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen_writer.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });

            let seen_at_exit = Arc::clone(&seen);
            pty.watch_exit(Box::new(move || {
                // Snapshot at the instant the exit is *reported*, which is what
                // the tail assertion below is about.
                let out = seen_at_exit.lock().unwrap().clone();
                let _ = done_tx.send(out);
            }));

            match done_rx.recv_timeout(Duration::from_secs(20)) {
                Ok(at_exit) => {
                    drop(pty);
                    let _ = tx.send(Ok(at_exit));
                }
                Err(_) => {
                    let _ = tx.send(Err("watch_exit never fired".into()));
                }
            }
        });

        match rx.recv_timeout(Duration::from_secs(45)) {
            Ok(Ok(at_exit)) => {
                let text = String::from_utf8_lossy(&at_exit);
                // Gotcha 2c, as an assertion: the child is dead well before
                // ConPTY has painted its output, so an exit reported the
                // instant `WaitForSingleObject` returns truncates the tail.
                // The quiet-drain is what makes this hold, and without it this
                // assertion is what fails.
                assert!(
                    text.contains("watch-probe"),
                    "the exit was reported before the output was painted -- the tail is being \
                     truncated (gotcha 2c). Got {} bytes:\n{}",
                    at_exit.len(),
                    visible(&at_exit)
                );
            }
            Ok(Err(e)) => panic!("{e}"),
            Err(_) => panic!(
                "watch_exit never reported, and the session never finished. Either the wait on \
                 the process handle is not firing, or something in the watcher is holding the \
                 shutdown protocol up -- the watcher must never touch the HPCON."
            ),
        }
    }

    /// The Windows counterpart of `unix.rs`'s
    /// `hangup_ends_everything_the_shell_started`, and unlike that one it
    /// genuinely discriminates: before the job object, `TerminateProcess`
    /// killed the shell and left its children running, so closing a session
    /// leaked a process per background command for the life of the machine.
    #[test]
    fn hangup_ends_everything_the_shell_started() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // A grandchild that would outlive its parent: pwsh starts a
            // detached `ping` and prints its pid, then sleeps.
            let cmd = CommandSpec {
                command_line: "powershell.exe -NoProfile -Command \"$p = Start-Process -PassThru \
                               ping -ArgumentList '-n','120','127.0.0.1'; \
                               Write-Host GRANDCHILD=$($p.Id); Start-Sleep 120\""
                    .into(),
                cwd: None,
                env: Vec::new(),
            };
            let mut pty = match ConPty::spawn(&cmd, PtySize::new(120, 24)) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Err(format!("spawn failed: {e}")));
                    return;
                }
            };
            let mut reader = pty.take_reader().expect("reader");
            let seen = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            let seen_writer = Arc::clone(&seen);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    seen_writer.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            });

            // Read the pid off the pty. ConPTY wraps lines at the window
            // width, hence the wide pty above.
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut pid: Option<u32> = None;
            while std::time::Instant::now() < deadline && pid.is_none() {
                let text = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
                if let Some(at) = text.find("GRANDCHILD=") {
                    let digits: String = text[at + "GRANDCHILD=".len()..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    if !digits.is_empty() {
                        // Wait for the line to be complete rather than racing
                        // a half-painted number.
                        if text[at..].contains('\n') || digits.len() >= 3 {
                            pid = digits.parse().ok();
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let Some(pid) = pid else {
                let _ = tx.send(Err(format!(
                    "never saw the grandchild's pid:\n{}",
                    visible(&seen.lock().unwrap())
                )));
                return;
            };

            pty.hangup();

            // Poll for the grandchild to be gone.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let alive = |pid: u32| -> bool {
                // SAFETY: query-only access to a pid that may already be gone;
                // a null handle means it is.
                unsafe {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
                    if h.is_null() {
                        return false;
                    }
                    let mut code = 0u32;
                    let ok = GetExitCodeProcess(h, &mut code);
                    CloseHandle(h);
                    ok != 0 && code == STILL_ACTIVE
                }
            };
            while std::time::Instant::now() < deadline && alive(pid) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = tx.send(if alive(pid) {
                Err(format!("grandchild {pid} outlived the hangup"))
            } else {
                Ok(())
            });
            drop(pty);
        });

        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!(
                "{e}\nA hangup must end the shell's whole tree. `TerminateProcess` reaches only \
                 the process it names, so this needs the job object in `ConPty::spawn`."
            ),
            Err(_) => panic!("the hangup test never finished"),
        }
    }

    /// Reading and dropping in the wrong order is the deadlock, so the reader
    /// being takeable only once is a real safety property, not tidiness.
    #[test]
    fn reader_can_only_be_taken_once() {
        let cmd = CommandSpec {
            command_line: "cmd.exe /c exit".into(),
            cwd: None,
            env: Vec::new(),
        };
        let mut pty = ConPty::spawn(&cmd, PtySize::new(80, 24)).expect("spawn");
        assert!(pty.take_reader().is_some());
        assert!(
            pty.take_reader().is_none(),
            "two readers would interleave VT bytes and corrupt the stream"
        );
        let _ = pty.wait_for_child(Some(Duration::from_secs(10)));
    }

    #[test]
    fn resize_is_accepted_and_clamps_zero() {
        let cmd = CommandSpec {
            command_line: "cmd.exe /c exit".into(),
            cwd: None,
            env: Vec::new(),
        };
        let pty = ConPty::spawn(&cmd, PtySize::new(80, 24)).expect("spawn");
        pty.resize(PtySize::new(120, 40)).expect("resize");
        // CreatePseudoConsole/ResizePseudoConsole reject zero dimensions; we
        // clamp rather than propagate an error, because a zero-sized window is
        // a transient state during minimize.
        pty.resize(PtySize::new(0, 0)).expect("zero size is clamped, not an error");
    }

    #[test]
    fn env_block_overrides_case_insensitively() {
        std::env::set_var("ZESTERM_TEST_VAR", "original");
        let block = build_env_block(&[("zesterm_test_var".into(), "override".into())])
            .expect("non-empty overrides produce a block");

        let text = String::from_utf16_lossy(&block);
        let entries: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        let matches: Vec<&&str> = entries
            .iter()
            .filter(|e| e.to_ascii_lowercase().starts_with("zesterm_test_var="))
            .collect();

        assert_eq!(matches.len(), 1, "the variable must not be duplicated: {matches:?}");
        assert!(matches[0].ends_with("=override"));
    }

    #[test]
    fn no_overrides_means_inherit() {
        assert!(build_env_block(&[]).is_none());
    }

    #[test]
    fn an_empty_value_removes_the_variable_entirely() {
        // Not "sets it to empty" -- code that only checks for presence would
        // still see an inherited WT_SESSION and take the Windows Terminal path.
        std::env::set_var("ZESTERM_TEST_STALE", "inherited");
        let block = build_env_block(&[("ZESTERM_TEST_STALE".into(), String::new())])
            .expect("a removal is still an override");

        let text = String::from_utf16_lossy(&block);
        assert!(
            !text.to_ascii_lowercase().contains("zesterm_test_stale="),
            "the variable is still present in the block"
        );
    }

    #[test]
    fn the_default_shell_advertises_a_colour_terminal() {
        // The bug this guards is invisible in the renderer and total in effect:
        // with no TERM, oh-my-posh emits no colour at all and the prompt is
        // monochrome no matter how correct the GPU path is.
        let env = CommandSpec::default_shell().env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());

        assert_eq!(get("TERM"), Some("xterm-256color"));
        assert_eq!(get("COLORTERM"), Some("truecolor"));
        assert_eq!(get("TERM_PROGRAM"), Some("zesterm"));
        assert!(get("TERM_PROGRAM_VERSION").is_some_and(|v| !v.is_empty()));
    }
}
