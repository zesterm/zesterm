//! Finding this machine's daemon, or starting one.
//!
//! ADR-007 makes the app a client of its own daemon over a loopback socket,
//! using exactly the protocol the phone uses over the network. That only works
//! if opening a terminal does not require the user to have started a daemon
//! first, so the app finds one or spawns one.
//!
//! # Never fatal
//!
//! If the daemon cannot be found or will not start, the app falls back to an
//! in-process pty and says so in the log. Both paths already exist behind
//! `SessionSource`, so it is one branch — and a terminal that refuses to open
//! because a helper binary is missing has failed at the only job it has.
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
}

/// Connect to this machine's daemon, starting one if there is none.
pub fn find_or_spawn(socket: &str, deadline: Duration) -> Result<Attached, DaemonStartError> {
    let started = Instant::now();

    // Probe first. This is the warm path and it is almost always the answer.
    if let Ok(stream) = zest_daemon::connect(socket) {
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

    spawn_detached(&binary, socket)?;

    // Poll rather than sleeping a fixed time: a fixed sleep is either flaky on
    // a loaded machine or wasted on an idle one, and this sits on the startup
    // path where both matter.
    let give_up = Instant::now() + deadline;
    while Instant::now() < give_up {
        if let Ok(stream) = zest_daemon::connect(socket) {
            return Ok(attached(stream, true, started.elapsed()));
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
fn attached(stream: zest_daemon::PipeStream, spawned: bool, elapsed: Duration) -> Attached {
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

/// Start the daemon so it outlives this process.
///
/// The detaching is the point. A daemon that dies with the window that started
/// it cannot host a session that outlives its window, which is the property
/// ADR-007 exists for.
fn spawn_detached(binary: &Path, socket: &str) -> Result<(), DaemonStartError> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
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
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NO_WINDOW. Without the first, a zesterm
        // started from a shell leaves the daemon holding that console, and
        // closing the shell later kills every shell in the fleet.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }

    // The child is deliberately not waited on: it is a daemon, and reaping it
    // is not this process's job. On unix `setsid` reparents it away from us.
    cmd.spawn().map(|_| ()).map_err(|e| DaemonStartError::Spawn(e.to_string()))
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

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("zesterm-daemon-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir");
        base
    }
}
