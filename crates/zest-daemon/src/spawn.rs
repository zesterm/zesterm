//! Finding this machine's daemon, or starting one.
//!
//! ADR-007 makes a client a client of its own daemon over a loopback socket,
//! using exactly the protocol the phone uses over the network. That only works
//! if opening a terminal does not require the user to have started a daemon
//! first, so a client finds one or spawns one.
//!
//! # Why this lives beside the server it starts
//!
//! It was `zest-app`'s until a second local client needed it (`zest-mcp`,
//! #274). Copying ~250 lines would have been the smaller diff and the worse
//! answer: the search order, the detaching, and the "stop the moment it exits"
//! rule are each a bug somebody already paid for, and two copies means paying
//! twice. It sits here rather than in a new crate because it is not about the
//! fleet — it is *"connect to this machine's daemon, starting one if absent"*,
//! which is the job [`crate::connect`] and [`crate::default_socket_path`]
//! already do half of.
//!
//! Nothing in the daemon binary calls it; this is the client half of the crate,
//! next to [`crate::client`].
//!
//! # Never fatal
//!
//! If the daemon cannot be found or will not start, the caller falls back —
//! `zest-app` to an in-process pty, saying so in the log. A terminal that
//! refuses to open because a helper binary is missing has failed at the only
//! job it has.
//!
//! # Where the cost lands
//!
//! On the warm path — every launch after the first — this is a `connect(2)` on
//! a unix socket or a `CreateFileW` on a pipe, which is single-digit
//! microseconds. On the cold path it is a process spawn, and it happens in the
//! slot the shell spawn used to occupy: after the window is visible and the
//! first paint is measured, overlapping GPU initialization. Never between
//! creating the window and showing it. → ADR-007.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A connection to a daemon, and how it was obtained.
pub struct Attached {
    pub read: Box<dyn Read + Send>,
    pub write: Box<dyn Write + Send>,
    /// True if this call started the daemon, rather than finding one.
    pub spawned: bool,
    /// How long the whole thing took, for `--attach-probe`.
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonStartError {
    #[error("no zest-daemon binary found (looked beside {exe}, and on PATH)")]
    NotFound { exe: String },
    #[error("could not start zest-daemon: {0}")]
    Spawn(String),
    #[error("zest-daemon did not start listening within {0:?}")]
    Timeout(Duration),
    /// It started and gave up. Its own stderr says why; this only reports that
    /// it happened, so the app can fall back at once rather than polling a
    /// socket that will never appear.
    #[error("zest-daemon exited during startup: {0}")]
    Exited(String),
}

/// Connect to this machine's daemon, starting one if there is none.
pub fn find_or_spawn(socket: &str, deadline: Duration) -> Result<Attached, DaemonStartError> {
    let started = Instant::now();

    // Probe first. This is the warm path and it is almost always the answer.
    if let Ok(stream) = crate::connect(socket) {
        return Ok(attached(stream, false, started.elapsed()));
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let binary = resolve_daemon_binary(&exe_dir, std::env::var_os("ZESTERM_DAEMON").as_deref(), |name| {
        which(name)
    })
    .ok_or_else(|| DaemonStartError::NotFound { exe: exe_dir.display().to_string() })?;

    let mut child = spawn_detached(&binary, &[OsStr::new("--socket"), OsStr::new(socket)])?;

    // Poll rather than sleeping a fixed time: a fixed sleep is either flaky on
    // a loaded machine or wasted on an idle one, and this sits on the startup
    // path where both matter.
    let give_up = Instant::now() + deadline;
    while Instant::now() < give_up {
        if let Ok(stream) = crate::connect(socket) {
            return Ok(attached(stream, true, started.elapsed()));
        }
        // And stop the moment the daemon gives up, rather than polling a socket
        // that will never appear. A daemon with no credential store or an
        // unreadable trust file exits at once, and waiting out the full
        // deadline left the window up with no shell for two seconds -- on
        // exactly the machines where something is already wrong.
        // Still running, or we cannot tell: keep waiting. Exited: stop now.
        if let Ok(Some(reason)) = child.exited() {
            return Err(DaemonStartError::Exited(reason));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(DaemonStartError::Timeout(deadline))
}

#[cfg(unix)]
fn attached(
    stream: std::os::unix::net::UnixStream,
    spawned: bool,
    elapsed: Duration,
) -> Attached {
    let write = stream.try_clone().expect("a connected socket can be cloned");
    Attached { read: Box::new(stream), write: Box::new(write), spawned, elapsed }
}

#[cfg(windows)]
fn attached(stream: crate::PipeStream, spawned: bool, elapsed: Duration) -> Attached {
    let write = stream.try_clone().expect("a connected pipe can be cloned");
    Attached { read: Box::new(stream), write: Box::new(write), spawned, elapsed }
}

/// Where to look for the daemon, in order.
///
/// Pure, and separated from the filesystem so the search order is a table test
/// rather than something only reproducible by moving binaries around.
///
/// 1. `$ZESTERM_DAEMON` — needed by tests, and by anyone running a daemon built
///    from a different tree than the app they are debugging.
/// 2. A sibling of the running executable. One rule that covers both
///    `target/<profile>/` under `cargo run` and any install layout, because in
///    both the two binaries ship together.
/// 3. `PATH`.
pub fn resolve_daemon_binary(
    exe_dir: &Path,
    env_override: Option<&OsStr>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(explicit) = env_override {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }

    let name = format!("zest-daemon{}", std::env::consts::EXE_SUFFIX);
    let sibling = exe_dir.join(&name);
    if sibling.is_file() {
        return Some(sibling);
    }

    on_path(&name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// A [`Command`](std::process::Command) that will not put a window on screen.
///
/// # Why a plain `Command::new` is a bug on Windows (#461)
///
/// A console child inherits its parent's console -- and when the parent has
/// **no** console, Windows does not skip the step. It allocates a fresh one,
/// `conhost.exe` and a visible window included, and tears it down when the
/// child exits. [`detached::spawn`] starts the daemon `DETACHED_PROCESS`
/// precisely so it holds nobody's console, which makes *every* console child it
/// spawns that case: [`crate::gitcmd`]'s `git status` runs for ~30ms behind an
/// attach or a detach, and flashed a window every time.
///
/// Two things make it expensive to find. A daemon started by hand in a shell
/// inherits that console and never flashes -- so the bug is absent from exactly
/// the setup someone debugging it would build -- and the spawn is a background
/// thread and two crates away from the gesture that triggered it.
///
/// `CREATE_NO_WINDOW` is the whole fix, and `cargo xtask check-spawn` is why
/// this is the only door: call sites that each have to remember are how one of
/// them forgets.
#[must_use]
pub fn quiet_command(program: impl AsRef<OsStr>) -> std::process::Command {
    // The `mut` is used on Windows only, and a `cfg` on the binding would mean
    // writing the body twice.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// What [`spawn_detached`] hands back, and the only question anyone asks it.
///
/// Not [`std::process::Child`]: the Windows arm no longer goes through
/// `std::process::Command` (see [`detached`]), and a `Child` can only come out
/// of one. Both arms answer with the string [`DaemonStartError::Exited`]
/// carries, which is the only use either ever had -- a smaller seam than a
/// trait, and it keeps the wording decision on the platform side, where the
/// Windows one is a judgement call (see `detached::describe`).
///
/// **Dropping this must never wait.** A daemon that started is meant to outlive
/// us: on unix `setsid` has already reparented it, and on Windows closing a
/// process handle is not a kill, only giving up the right to ask.
struct Spawned(Started);

#[cfg(unix)]
type Started = std::process::Child;
#[cfg(windows)]
type Started = detached::Process;

impl Spawned {
    /// `Some(reason)` once the daemon has exited -- without waiting for it to.
    fn exited(&mut self) -> std::io::Result<Option<String>> {
        #[cfg(unix)]
        {
            Ok(self.0.try_wait()?.map(|status| status.to_string()))
        }
        #[cfg(windows)]
        {
            self.0.exited()
        }
    }
}

/// Start the daemon so it outlives this process.
///
/// The detaching is the point. A daemon that dies with the window that started
/// it cannot host a session that outlives its window, which is the property
/// ADR-007 exists for.
///
/// Takes the arguments rather than the socket path so a test can start a child
/// that *stays alive* -- the property the inheritance test needs, and one a real
/// daemon would only supply by being left running on the machine afterwards.
/// Same trade as [`resolve_daemon_binary`] taking its lookups as arguments.
fn spawn_detached(binary: &Path, args: &[&OsStr]) -> Result<Spawned, DaemonStartError> {
    #[cfg(unix)]
    {
        let mut cmd = std::process::Command::new(binary);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe and touches only this child's
        // process group. Without it the daemon stays in the app's process
        // group and its controlling terminal, so closing the shell that
        // launched zesterm would take every session on the machine with it.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()
                    .map(|_| ())
                    .map_err(|e: rustix::io::Errno| std::io::Error::from_raw_os_error(e.raw_os_error()))
            });
        }

        // Nothing leaks here, which is why this arm is five lines and the
        // Windows one is a module: std opens every descriptor it creates
        // CLOEXEC, so a pipe a shell handed us dies at `execve`.
        cmd.spawn().map(Spawned).map_err(|e| DaemonStartError::Spawn(e.to_string()))
    }
    #[cfg(windows)]
    {
        detached::spawn(binary, args).map(Spawned).map_err(|e| DaemonStartError::Spawn(e.to_string()))
    }
}

/// `CreateProcessW` by hand, because `std::process::Command` cannot be told
/// what *not* to give away.
///
/// # The trap, which reads as already fixed (#412)
///
/// `Command::spawn` passes `bInheritHandles = TRUE` with no
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, so the child receives a duplicate of
/// **every inheritable handle in this process's table** -- not the ones named
/// in `STARTUPINFO`. `Stdio::null()` narrows that by nothing; it only decides
/// what the child's own `GetStdHandle` returns. So a shell that hands zesterm a
/// pipe as stdout hands it to a daemon, which is built to outlive us and holds
/// it for days: `zesterm --attach-probe | Out-String` hung for 85 seconds and
/// was released by killing a daemon nobody was talking to. `zest-mcp` is the
/// same shape and worse -- its stdin and stdout *are* an agent harness's pipes.
///
/// Every call involved reports success, there is no log line, and the symptom
/// lands in somebody else's process one layer up from the mistake. That is the
/// `STARTF_USESTDHANDLES` gotcha in `zest-pty/src/windows.rs` one layer higher,
/// and that module is the worked example this one follows.
///
/// **Rejected: `SetHandleInformation` on our own three std handles.** It
/// mutates process-global state from a function two crates call with other
/// threads running -- #403's umask lesson in different clothes -- and that
/// lesson's constructive half, prefer the call that takes the property as an
/// *argument*, points straight at the handle list. It would also name three
/// handles out of a set whose size is chosen by whoever launched us.
///
/// std will be able to say this one day: `CommandExt::inherit_handles`
/// (rust#146407) and `spawn_with_attributes` (rust#114854) are both nightly.
#[cfg(windows)]
mod detached {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// The daemon, as a handle we may ask about and must not wait on.
    ///
    /// `std::os::windows::io::OwnedHandle` rather than a hand-rolled one:
    /// `zest-pty` writes its own because its handles must be `Send` and
    /// cloneable, and neither is true here -- this one never leaves the thread
    /// that made it.
    pub(super) struct Process(OwnedHandle);

    pub(super) fn spawn(binary: &Path, args: &[&OsStr]) -> io::Result<Process> {
        // Three handles rather than one shared read/write handle, because that
        // is what `Stdio::null()` opened. A daemon whose stdout and stderr are
        // the same file object is a difference nobody would think to look for.
        let stdin = open_nul(GENERIC_READ)?;
        let stdout = open_nul(GENERIC_WRITE)?;
        let stderr = open_nul(GENERIC_WRITE)?;

        // Size the list, then allocate. The first call is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER; it reports the size through `bytes`.
        let mut bytes: usize = 0;
        // SAFETY: passing null is the documented sizing call.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }

        // ALIGNMENT MATTERS, and getting it wrong fails *silently* -- the trap
        // `zest_pty`'s `spawn_child` already paid for once.
        // PROC_THREAD_ATTRIBUTE_LIST is pointer-aligned; a `Vec<u8>` is
        // allocated with align 1, and when it lands on an odd address every
        // call below still reports success, CreateProcessW still succeeds, and
        // the attribute is simply ignored -- which here means the leak is back
        // with the fix apparently in place. `Vec<usize>` guarantees it.
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut attr_buf: Vec<usize> = vec![0usize; words];
        let attr_list = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

        // SAFETY: attr_list points at `bytes` writable bytes, as just sized.
        if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // From here the list must be deleted before returning; the guard does
        // it on the error paths below too.
        let _guard = AttrListGuard(attr_list);

        // A *pointer to an array*, unlike the pseudoconsole attribute in
        // `zest-pty` which takes its handle by value -- and the array must
        // outlive `CreateProcessW`, which is why it is bound to a local here
        // rather than written inline. Same class of trap as the alignment one.
        let inherited: [HANDLE; 3] =
            [stdin.as_raw_handle(), stdout.as_raw_handle(), stderr.as_raw_handle()];
        // SAFETY: attr_list is initialized with room for one attribute, and
        // `inherited` lives until after CreateProcessW below.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_ptr().cast::<core::ffi::c_void>().cast_mut(),
                std::mem::size_of_val(&inherited),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: a zeroed STARTUPINFOEXW is valid; `cb` names the extended
        // size, which is what EXTENDED_STARTUPINFO_PRESENT below reads.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr_list;
        // The child's own three, exactly as `Stdio::null()` set them. Each is
        // named in the list above: a std handle passed *outside* the list is
        // what makes CreateProcessW fail with ERROR_INVALID_PARAMETER.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin.as_raw_handle();
        si.StartupInfo.hStdOutput = stdout.as_raw_handle();
        si.StartupInfo.hStdError = stderr.as_raw_handle();

        let application = to_wide(resolve(binary).as_os_str())?;
        // CreateProcessW may modify the command line in place, so it must be a
        // writable, NUL-terminated copy.
        let mut command_line = to_wide(&command_line(binary, args))?;
        // SAFETY: a zeroed PROCESS_INFORMATION is a valid out-parameter.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // SAFETY: `application` and `command_line` are NUL-terminated and live
        // across the call, the latter writable; `si` is correctly sized with a
        // live attribute list whose handle array is still in scope; `pi` is a
        // valid out-pointer.
        let ok = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                // TRUE, and the handle list above is what makes that safe: it
                // is the *whole* set the child receives. FALSE is not the
                // alternative -- the std handles named in `si` would then
                // reference nothing in the child's table, and every write the
                // daemon made would fail rather than reach NUL.
                TRUE,
                // DETACHED_PROCESS is load-bearing twice. Without it a zesterm
                // started from a shell leaves the daemon holding that console,
                // and closing the shell later kills every shell in the fleet --
                // and a console is inherited as a pair of handles passed
                // outside the list above, which CreateProcessW refuses.
                DETACHED_PROCESS | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                ptr::null(), // our environment, as `Command` gave it
                ptr::null(), // our working directory, as `Command` gave it
                ptr::addr_of_mut!(si).cast(),
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: both are fresh handles CreateProcessW just wrote and nobody
        // else owns.
        let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
        // Closed at once rather than kept: this is a handle *to* the daemon's
        // first thread, nothing here resumes or waits on it, and closing a
        // thread handle does not touch the thread.
        drop(unsafe { OwnedHandle::from_raw_handle(pi.hThread) });
        Ok(Process(process))
    }

    impl Process {
        pub(super) fn exited(&self) -> io::Result<Option<String>> {
            // `WaitForSingleObject` with a zero timeout first, rather than
            // `GetExitCodeProcess` alone: STILL_ACTIVE is 259, which is also a
            // perfectly legal exit code, so a daemon that exited 259 would read
            // as still running until the caller's deadline -- and this poll
            // exists precisely to catch a daemon that gave up at once. It is
            // what `Child::try_wait` does.
            // SAFETY: a live process handle this type owns.
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(None),
                _ => return Err(io::Error::last_os_error()),
            }
            let mut code: u32 = 0;
            // SAFETY: the same handle; `code` is a valid out-pointer.
            if unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(describe(code)))
        }
    }

    /// The wording `std::process::ExitStatus` would have produced.
    ///
    /// Copied deliberately, hex rule and all: this string is what
    /// [`super::DaemonStartError::Exited`] carries to two frozen consumers, and
    /// a daemon killed by an access violation exits `0xc0000005`, which as
    /// `3221225477` tells nobody anything. Keeping std's rule means moving off
    /// `Command` changes no message anyone has ever read.
    fn describe(code: u32) -> String {
        if code & 0x8000_0000 != 0 { format!("exit code: {code:#x}") } else { format!("exit code: {code}") }
    }

    /// One handle on the `NUL` device, inheritable.
    ///
    /// Read-xor-write, because that is what `Stdio::null()` opened and the
    /// point of this module is that nothing else changes.
    ///
    /// Inheritable from birth rather than marked afterwards: a handle in the
    /// list that is *not* inheritable makes CreateProcessW fail with
    /// ERROR_INVALID_PARAMETER. The window in which it is inheritable is
    /// unavoidable and harmless -- another thread spawning inside it would give
    /// its child a handle to `NUL`, which nothing waits on.
    fn open_nul(access: u32) -> io::Result<OwnedHandle> {
        let name = to_wide(OsStr::new(r"\\.\NUL"))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        // SAFETY: `name` is NUL-terminated and outlives the call; `sa` is fully
        // initialized and only read by the callee.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle nobody else owns, and not
        // INVALID_HANDLE_VALUE -- the one value `OwnedHandle` may not hold.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// Where `lpApplicationName` should point.
    ///
    /// [`super::resolve_daemon_binary`] can hand back a bare file name -- only
    /// from `$ZESTERM_DAEMON`, and only if somebody set it to one -- and
    /// `lpApplicationName` does not search `PATH` the way `Command` did.
    /// Resolved here rather than by passing null and letting CreateProcessW
    /// search from the command line: its second stop is the *current
    /// directory*, and zesterm's is whatever project the window was opened in.
    fn resolve(binary: &Path) -> PathBuf {
        if binary.parent().is_none_or(|p| p.as_os_str().is_empty()) {
            if let Some(found) = binary.to_str().and_then(super::which) {
                return found;
            }
        }
        binary.to_path_buf()
    }

    /// The binary and its arguments, quoted the way `CommandLineToArgvW`
    /// unquotes.
    ///
    /// `Command` did this for us, and the rule is not "wrap it in quotes": a
    /// run of backslashes before a quote is halved, so an argument ending in a
    /// backslash -- and a Windows directory may -- would escape the closing
    /// quote and swallow the argument after it. Both arguments here can contain
    /// spaces (`C:\Program Files\...`), so nothing may be skipped. argv[0]
    /// plays by its own rule -- see [`push_program`].
    fn command_line(binary: &Path, args: &[&OsStr]) -> OsString {
        let mut units: Vec<u16> = Vec::new();
        push_program(&mut units, binary.as_os_str());
        for arg in args {
            units.push(u16::from(b' '));
            push_quoted(&mut units, arg);
        }
        OsString::from_wide(&units)
    }

    /// argv[0], which is parsed by a different rule from every argument after
    /// it: inside its quotes a backslash is never an escape, so escaping one
    /// here is what *adds* a slash to the path. A quote cannot appear at all,
    /// because Windows forbids one in a file name. Cosmetic in any case --
    /// `lpApplicationName` chooses the binary and the daemon's own parser skips
    /// argv[0] -- but a command line nothing can parse back is a bad one to
    /// hand a debugger.
    fn push_program(out: &mut Vec<u16>, program: &OsStr) {
        const QUOTE: u16 = b'"' as u16;
        out.push(QUOTE);
        out.extend(program.encode_wide().filter(|&unit| unit != QUOTE));
        out.push(QUOTE);
    }

    /// One argument, always quoted, escaped by std's own `append_arg` rule.
    fn push_quoted(out: &mut Vec<u16>, arg: &OsStr) {
        const QUOTE: u16 = b'"' as u16;
        const BACKSLASH: u16 = b'\\' as u16;
        out.push(QUOTE);
        let mut backslashes = 0usize;
        for unit in arg.encode_wide() {
            if unit == BACKSLASH {
                backslashes += 1;
                continue;
            }
            if unit == QUOTE {
                // The run before a quote is doubled and the quote escaped, or
                // the parser reads our escape as the caller's.
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
            } else {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
            }
            backslashes = 0;
            out.push(unit);
        }
        // The trailing run precedes the quote we are about to write, so it is
        // doubled for the same reason.
        out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
        out.push(QUOTE);
    }

    struct AttrListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);

    impl Drop for AttrListGuard {
        fn drop(&mut self) {
            // SAFETY: the list was successfully initialized and is deleted once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }

    /// A NUL-terminated UTF-16 copy.
    ///
    /// Refuses an interior NUL rather than truncating: a truncated command line
    /// silently drops `--socket` and starts a daemon on the *default* path
    /// while we poll another one, which reads as a daemon that will not start.
    fn to_wide(s: &OsStr) -> io::Result<Vec<u16>> {
        let mut units: Vec<u16> = s.encode_wide().collect();
        if units.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a path or argument contains an interior NUL",
            ));
        }
        units.push(0);
        Ok(units)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// What `CommandLineToArgvW` will make of what we wrote.
        ///
        /// Round-tripped through the real parser rather than compared against a
        /// hand-written expected string: the rule being implemented *is* that
        /// parser's, so asserting our own reading of it would pin the bug.
        fn round_trip(binary: &str, args: &[&str]) -> Vec<String> {
            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

            let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            let line = to_wide(&command_line(Path::new(binary), &owned)).expect("no interior NUL");
            let mut argc = 0i32;
            // SAFETY: `line` is NUL-terminated and outlives the call; the block
            // returned is freed below.
            let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
            assert!(!argv.is_null(), "CommandLineToArgvW refused our command line");
            let mut out = Vec::new();
            for i in 0..argc as usize {
                // SAFETY: argc entries, each a NUL-terminated string.
                let p = unsafe { *argv.add(i) };
                let mut len = 0usize;
                // SAFETY: as above -- the NUL terminates the walk.
                while unsafe { *p.add(len) } != 0 {
                    len += 1;
                }
                // SAFETY: `len` units precede that NUL.
                let units = unsafe { std::slice::from_raw_parts(p, len) };
                out.push(OsString::from_wide(units).to_string_lossy().into_owned());
            }
            // SAFETY: the block CommandLineToArgvW allocated, freed once.
            unsafe { LocalFree(argv.cast()) };
            out
        }

        #[test]
        fn a_path_with_spaces_survives_being_a_command_line() {
            // The install layout this is most likely to meet.
            let got = round_trip(r"C:\Program Files\zesterm\zest-daemon.exe", &["--socket", "s"]);
            assert_eq!(
                got,
                vec![r"C:\Program Files\zesterm\zest-daemon.exe", "--socket", "s"],
                "an unquoted space splits the binary into two arguments"
            );
        }

        #[test]
        fn a_trailing_backslash_does_not_escape_the_closing_quote() {
            // The rule `Command` was applying for us, and the reason this is
            // not `format!("\"{}\"")`: the run before a closing quote is
            // halved, so one backslash would escape it and swallow `--socket`.
            let got = round_trip(r"C:\dir with space\", &["--socket", r"\\.\pipe\zesterm-x"]);
            assert_eq!(
                got,
                vec![r"C:\dir with space\", "--socket", r"\\.\pipe\zesterm-x"],
                "a pipe path ends in a name, but the directory a binary sits in may end in a slash"
            );
        }

        #[test]
        fn an_embedded_quote_reaches_the_child_as_one() {
            let got = round_trip(r"C:\z.exe", &["--socket", "a\"b"]);
            assert_eq!(got, vec![r"C:\z.exe", "--socket", "a\"b"]);
        }

        #[test]
        fn an_interior_nul_is_refused_rather_than_truncating_the_command_line() {
            // Truncation would silently drop `--socket` and start a daemon on
            // the default path while the caller polls another one.
            let nul = OsString::from_wide(&[u16::from(b'a'), 0, u16::from(b'b')]);
            assert!(to_wide(&nul).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_override_wins() {
        // What a developer running a daemon from another tree relies on, and
        // what the integration tests use.
        let picked = resolve_daemon_binary(
            Path::new("/nowhere"),
            Some(OsStr::new("/custom/zest-daemon")),
            |_| Some(PathBuf::from("/on/path/zest-daemon")),
        );
        assert_eq!(picked, Some(PathBuf::from("/custom/zest-daemon")));
    }

    #[test]
    fn an_empty_override_is_ignored_rather_than_used() {
        // `ZESTERM_DAEMON=` in a shell profile is a common way to *unset* a
        // variable. Treating it as a path spawns "" and fails confusingly.
        let picked = resolve_daemon_binary(
            Path::new("/nowhere"),
            Some(OsStr::new("")),
            |_| Some(PathBuf::from("/on/path/zest-daemon")),
        );
        assert_eq!(picked, Some(PathBuf::from("/on/path/zest-daemon")));
    }

    #[test]
    fn a_sibling_binary_is_preferred_to_one_on_path() {
        // Under `cargo run` there is very often a different, older zest-daemon
        // installed on PATH. Picking that one produces a protocol mismatch
        // that reads as a bug in the code just edited.
        let dir = tempdir();
        let name = format!("zest-daemon{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(dir.join(&name), b"#!/bin/sh\n").expect("write");

        let picked =
            resolve_daemon_binary(&dir, None, |_| Some(PathBuf::from("/on/path/zest-daemon")));
        assert_eq!(picked, Some(dir.join(&name)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_anywhere_is_none_rather_than_a_guess() {
        let picked = resolve_daemon_binary(Path::new("/nowhere"), None, |_| None);
        assert_eq!(picked, None, "a guessed path would spawn something unknown");
    }

    /// The stand-in's dead man's switch. It must outlive `PATIENCE` below, or a
    /// child that exited on its own would close the pipe and pass the test for
    /// the wrong reason; and it must be short, because on the *unfixed* code
    /// that child is also holding whatever pipe cargo itself was given.
    #[cfg(windows)]
    const CHILD_LIFETIME: Duration = Duration::from_secs(5);

    /// #412, in its smallest clothes.
    ///
    /// A shell hands zesterm a pipe as stdout; `Command::spawn` hands the
    /// daemon a duplicate of the write end; the far side never sees EOF and the
    /// pipeline hangs until somebody kills a daemon nobody was talking to.
    ///
    /// Measured on a pipe of our own rather than on this process's real stdout,
    /// for two reasons. The real one is process-global, and a test that mutates
    /// process-global state is #403 again. And the bug is not about *std*
    /// handles at all -- any inheritable handle does it, which is exactly the
    /// half a `SetHandleInformation` fix would have left open.
    ///
    /// Windows only, deliberately. On unix the invariant holds through CLOEXEC
    /// and the fixture does not exist -- a handle that is inheritable by
    /// default -- so the test would be green before and after and prove nothing.
    #[cfg(windows)]
    #[test]
    fn a_detached_child_inherits_nothing_of_ours() {
        use std::io::Read as _;
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
        use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Pipes::CreatePipe;

        /// Long enough that the read cannot be the thing that ends first on a
        /// loaded runner, short enough that a red run reports rather than
        /// hangs -- and a long way clear of the 85 seconds #412 measured, so
        /// neither answer is a coin flip. (#18 §1: a hang is not a test result.)
        const PATIENCE: Duration = Duration::from_secs(3);

        let mut read: HANDLE = std::ptr::null_mut();
        let mut write: HANDLE = std::ptr::null_mut();
        // Null attributes, so both ends are born private -- then exactly one is
        // marked inheritable. That is the state a shell's pipe reaches us in,
        // and marking a handle we own outright is not the process-global
        // `SetHandleInformation(GetStdHandle(..))` this bug's wrong fix uses.
        // SAFETY: two valid out-pointers.
        assert!(unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } != 0);
        // SAFETY: fresh handles nobody else owns.
        let read = unsafe { OwnedHandle::from_raw_handle(read) };
        let write = unsafe { OwnedHandle::from_raw_handle(write) };
        // SAFETY: a handle this scope owns.
        assert!(
            unsafe { SetHandleInformation(write.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } != 0
        );

        // The child is this very test binary, re-running one ignored test that
        // does nothing but stay alive. A *real* daemon would only supply that
        // property by being left running on the machine afterwards, and would
        // drag its key store and trust file into a test about handles.
        let me = std::env::current_exe().expect("a test binary knows its own path");
        let mut child = spawn_detached(
            &me,
            &[
                OsStr::new("--exact"),
                OsStr::new("--ignored"),
                OsStr::new("--test-threads=1"),
                OsStr::new("spawn::tests::a_child_that_only_stays_alive"),
            ],
        )
        .expect("spawn the stand-in");

        // Our own copy goes now, so the only write handle left anywhere is one
        // the child should never have received.
        drop(write);

        // Off-thread, and never joined: on the broken code this read does not
        // return, and a test that cannot outlive its own measurement cannot
        // report one.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // std maps ERROR_BROKEN_PIPE to a zero-length read, so EOF arrives
            // as `Ok`.
            let mut file = std::fs::File::from(read);
            let mut buf = Vec::new();
            let _ = tx.send(file.read_to_end(&mut buf).map(|_| ()));
        });

        let saw_eof = rx.recv_timeout(PATIENCE);
        // Read *before* asserting: an assertion unwinds, and the answer to
        // "was this run vacuous" has to be taken at the moment EOF arrived.
        let child_still_running = matches!(child.exited(), Ok(None));

        match saw_eof {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("reading our own pipe failed: {e}"),
            Err(_) => panic!(
                "#412: the pipe was still open after {PATIENCE:?}, so a process we spawned is \
                 holding our write handle. `Command::spawn` passes bInheritHandles=TRUE with no \
                 PROC_THREAD_ATTRIBUTE_HANDLE_LIST, which hands the child every inheritable \
                 handle we hold -- redirecting its stdio to null narrows nothing, because the \
                 leaked handle is not a std handle. A shell pipeline never sees EOF and hangs \
                 until the daemon is killed."
            ),
        }
        assert!(
            child_still_running,
            "the stand-in had already exited when EOF arrived, so EOF proved nothing -- a child \
             that dies closes its handles whether it inherited one or not. This run was vacuous; \
             CHILD_LIFETIME is too short, or the child never started."
        );
    }

    /// Not a test: the child half of the one above.
    ///
    /// It has to be *alive* while the parent reads, which is the whole reason
    /// [`spawn_detached`] takes its arguments rather than a socket path.
    #[cfg(windows)]
    #[test]
    #[ignore = "the child half of a_detached_child_inherits_nothing_of_ours"]
    fn a_child_that_only_stays_alive() {
        std::thread::sleep(CHILD_LIFETIME);
    }

    /// The pids attached to *this* process's console, or `None` if it has none.
    ///
    /// `GetConsoleProcessList` answers with the count when the buffer is too
    /// small and writes nothing, so the retry is not optional -- a runner with
    /// enough attached processes would otherwise read an untouched buffer of
    /// zeros as "the child is not there", which is the answer the test is
    /// looking for and would make it pass for the wrong reason.
    #[cfg(windows)]
    fn console_pids() -> Option<Vec<u32>> {
        let mut buf = vec![0u32; 64];
        loop {
            // SAFETY: a valid buffer and its true length.
            let n = unsafe {
                windows_sys::Win32::System::Console::GetConsoleProcessList(
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                )
            };
            if n == 0 {
                return None; // no console attached to this process at all
            }
            if n as usize <= buf.len() {
                buf.truncate(n as usize);
                return Some(buf);
            }
            buf = vec![0u32; n as usize];
        }
    }

    /// #461: a child of ours must not be given a console.
    ///
    /// The observable works from a console-*owning* test binary, which is what
    /// `cargo test` gives us, because the two cases differ either way: without
    /// `CREATE_NO_WINDOW` the child joins our console, with it the child does
    /// not.
    ///
    /// **What this measures is the flag, not the window, and the difference is
    /// worth stating**: `CREATE_NO_WINDOW` does not stop the child having a
    /// console, it stops that console having a window -- measured from a
    /// console-less parent, a plain child's console reports a *visible* hwnd
    /// and a `CREATE_NO_WINDOW` child's reports none at all. Reproducing that
    /// here would mean calling `FreeConsole` in the test process, which is
    /// process-global state mutated while libtest's pool is running -- #403's
    /// umask lesson exactly. So this asserts the one thing that follows from
    /// the flag and nothing else: a child that did not join our console was
    /// created with it.
    ///
    /// **The control is the point.** An assertion that a pid is absent from a
    /// list passes for any reason at all, including a list that was never
    /// filled in; a second child spawned the unfixed way has to be *present* in
    /// the same list before the absence means anything. That is the
    /// `shutdown_probe` lesson -- cross the two cases before believing either.
    #[cfg(windows)]
    #[test]
    fn a_quiet_child_gets_no_console_and_a_plain_one_gets_ours() {
        /// Long enough to cover process start on a loaded runner, short enough
        /// that a red run reports rather than hangs.
        const PATIENCE: Duration = Duration::from_secs(5);

        if console_pids().is_none() {
            // Under a runner that hands the test binary no console there is
            // nothing for a child to inherit and both arms would look alike.
            eprintln!("skipping: this test binary owns no console");
            return;
        }

        let me = std::env::current_exe().expect("a test binary knows its own path");
        // The same stand-in the #412 test uses: this binary, re-running one
        // ignored test that does nothing but stay alive long enough to be
        // asked about.
        let stand_in = |program: &mut std::process::Command| {
            program
                .args([
                    "--exact",
                    "--ignored",
                    "--test-threads=1",
                    "spawn::tests::a_child_that_only_stays_alive",
                ])
                // Null stdio only so the children's libtest chatter stays out
                // of this run's output. It is not what decides console
                // membership -- that is settled by the creation flags, at
                // creation, which is the confusion `Stdio::null()` invited in
                // #412.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the stand-in")
        };

        let mut quiet = stand_in(&mut quiet_command(&me));
        let mut plain = stand_in(&mut std::process::Command::new(&me));

        // Wait for the *control* to show up: console membership is decided at
        // CreateProcess, so once the unfixed child is listed, the fixed one
        // either is too or never will be.
        let started = Instant::now();
        let mut saw_plain = false;
        let mut saw_quiet = false;
        while started.elapsed() < PATIENCE {
            let pids = console_pids().unwrap_or_default();
            saw_plain |= pids.contains(&plain.id());
            saw_quiet |= pids.contains(&quiet.id());
            if saw_plain {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // Sample past the control for a beat, so "absent" is a property of the
        // child rather than of the instant it was read at.
        for _ in 0..8 {
            saw_quiet |= console_pids().unwrap_or_default().contains(&quiet.id());
            std::thread::sleep(Duration::from_millis(25));
        }

        let _ = quiet.kill();
        let _ = quiet.wait();
        let _ = plain.kill();
        let _ = plain.wait();

        assert!(
            saw_plain,
            "the control never joined our console, so this run measured nothing. A plain \
             `Command::new` child inherits the parent's console; if it did not appear, the \
             stand-in failed to start or died before it could be seen."
        );
        assert!(
            !saw_quiet,
            "#461: a child spawned through `quiet_command` joined our console, so it was created \
             without CREATE_NO_WINDOW. In the daemon -- which holds no console at all, being \
             DETACHED_PROCESS -- Windows answers that by allocating a fresh console, and a \
             window flashes on screen for every `git status` the dirty probe runs."
        );
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("zesterm-daemon-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        base
    }
}
