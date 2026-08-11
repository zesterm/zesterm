//! Pseudo-terminal creation and I/O.
//!
//! Written directly against ConPTY rather than taking a dependency on
//! `portable-pty`, because ConPTY is the highest-risk component on Windows and
//! its sharp edges have to be controlled rather than abstracted over. See
//! [`windows`] for the specifics. The unix side is a normal `openpt` pair and
//! is much less exotic, but it has its own three sharp edges — see [`unix`].
//!
//! Both backends are exposed as `NativePty` so nothing above this crate needs a
//! `cfg`.

use std::io::{Read, Write};

pub mod shell_integration;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::ConPty as NativePty;

#[cfg(unix)]
pub mod unix;

#[cfg(unix)]
pub use unix::UnixPty as NativePty;

/// Size of the terminal in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl Default for PtySize {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// What to run, and how.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Full command line. On Windows this is passed to `CreateProcessW`
    /// verbatim, which parses it itself — we do not attempt to re-quote.
    pub command_line: String,
    /// Working directory; inherits the parent's when `None`.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra environment entries layered over the parent's environment.
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    /// A sensible default shell for the platform.
    #[must_use]
    pub fn default_shell() -> Self {
        // PowerShell 7 if present, else Windows PowerShell, else cmd. Checked at
        // spawn time rather than baked in, because this decides what the user
        // actually gets when they have not configured a shell.
        #[cfg(windows)]
        let command_line = resolve_windows_shell(
            &std::env::var("PATH").unwrap_or_default(),
            &std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            |p| p.is_file(),
        );
        #[cfg(not(windows))]
        let command_line = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());

        Self { command_line, cwd: None, env: terminal_env() }
    }

    /// Load zesterm's OSC 133 hook into this shell, if we know how.
    ///
    /// **Call after `command_line` is final**, since which shell it is decides
    /// what to inject. `dir` is where the shim may be written — see
    /// [`shell_integration::install`] for why the caller supplies it.
    ///
    /// A shell we cannot hook, or a shim that will not write, is not an error
    /// worth failing a spawn over: the result is a working shell with no
    /// command blocks, which is what every terminal did until now. It is logged
    /// rather than silent, because "why do I have no blocks" needs an answer
    /// somewhere.
    pub fn enable_shell_integration(&mut self, dir: &std::path::Path) {
        // A command line can already carry the hook, because PowerShell's
        // injection point *is* the command line and a command line travels: the
        // app hands one to the daemon, and a user may have written their own.
        // Injecting again would emit every marker twice, which the parser reads
        // as an empty block between each real one.
        if shell_integration::already_injected(&self.command_line) {
            tracing::debug!(command = %self.command_line, "shell integration is already loaded");
            return;
        }
        let Some(shell) = shell_integration::Shell::detect(&self.command_line) else {
            // `info`, not `debug`. "Why does the status bar say 0 blocks" needs
            // an answer that a default log setup actually prints -- the silence
            // here is what made PowerShell's absence take a screenshot to
            // notice (#83). `cmd.exe` is the honest permanent case.
            tracing::info!(
                command = %self.command_line,
                "no shell integration for this shell; it will have no command blocks"
            );
            return;
        };
        match shell_integration::install(shell, &self.command_line, dir) {
            Ok(injection) if injection.env.is_empty() && injection.args.is_empty() => {
                // `install` declined — a PowerShell already running a command of
                // its own is the case — and has said why at `info`. Reporting
                // "injected" as well would contradict it two lines later, in the
                // log someone is reading precisely because their blocks are
                // missing.
            }
            Ok(injection) => {
                self.env.extend(injection.env);
                self.command_line.push_str(&injection.args);
                tracing::debug!(shell = shell.name(), "shell integration injected");
            }
            Err(e) => {
                tracing::warn!(
                    shell = shell.name(),
                    error = %e,
                    "could not write the shell integration shim; continuing without command blocks"
                );
            }
        }
    }
}

/// Which shell a Windows box gets when the user has not chosen one.
///
/// `PATH` first, and that is the fix rather than a nicety: the previous version
/// probed `C:\Program Files\PowerShell\7\pwsh.exe` and nothing else, so a
/// scoop, winget or MSIX PowerShell — none of which install there — dropped the
/// user all the way to `cmd.exe`, which has no prompt hook and therefore no
/// command blocks, ever. `PATH` is also what the user means by "pwsh", since it
/// is what typing the word runs.
///
/// Windows PowerShell 5.1 sits between PowerShell 7 and `cmd` because it ships
/// with the OS and takes the same hook. Falling past it to `cmd` trades a shell
/// with command blocks for one that structurally cannot have them.
///
/// Pure, and takes its inputs rather than reading the environment, so the order
/// can be tested without a PowerShell installed or the process environment
/// mutated.
#[cfg(windows)]
fn resolve_windows_shell(
    path_var: &str,
    comspec: &str,
    exists: impl Fn(&std::path::Path) -> bool,
) -> String {
    let on_path = |exe: &str| {
        path_var
            .split(';')
            .filter(|d| !d.is_empty())
            .map(|d| std::path::Path::new(d).join(exe))
            .find(|p| exists(p))
    };

    for exe in ["pwsh.exe", "powershell.exe"] {
        if let Some(found) = on_path(exe) {
            return quoted_shell(&found.to_string_lossy());
        }
        // Not on PATH but installed in the place its installer uses. The
        // daemon's environment is frozen at first spawn and every shell in the
        // fleet inherits it, so a PATH that predates an install is a case that
        // really happens.
        let default = match exe {
            "pwsh.exe" => r"C:\Program Files\PowerShell\7\pwsh.exe",
            _ => r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        };
        if exists(std::path::Path::new(default)) {
            return quoted_shell(default);
        }
    }
    comspec.to_string()
}

/// A path as a command line: quoted, because `CreateProcessW` parses the line
/// itself and every plausible location for these has a space in it.
#[cfg(windows)]
fn quoted_shell(path: &str) -> String {
    format!("\"{path}\" -NoLogo")
}

/// How the shell learns what terminal it is talking to.
///
/// Programs do not probe for capabilities — they read environment variables and
/// believe them. A PTY child that inherits nothing gets no `TERM` at all, and
/// the well-behaved response to that is to assume a dumb terminal: oh-my-posh
/// drops its segment backgrounds, `ls` drops its colours, `bat` and `delta` fall
/// back to plain text. The grid renders those bytes perfectly; they are just
/// never sent. That failure looks exactly like a broken renderer, which is what
/// makes it worth stating here.
///
/// Inherited terminal identity is also removed, not just overridden. Launching
/// zesterm from Windows Terminal otherwise passes `WT_SESSION` down, and
/// anything keying off it — oh-my-posh among them — takes the wrong branch.
///
/// The unix hosts have more of these than Windows does, not fewer, and they are
/// easy to miss because `TERM_PROGRAM` *is* overridden above and looks like it
/// settles the question. It does not: launching from iTerm leaves
/// `ITERM_SESSION_ID` and `LC_TERMINAL` behind, and shell integrations key off
/// those directly. `LC_TERMINAL` is the worst of them — it is an `LC_*`
/// variable, so ssh forwards it by default and the wrong identity survives a
/// hop to another machine.
#[must_use]
pub fn terminal_env() -> Vec<(String, String)> {
    let mut env = vec![
        // 256 colours is what a bare `xterm-256color` promises; COLORTERM is the
        // de-facto signal for the 24-bit support we actually have. Both are
        // needed -- most programs check one or the other, not both.
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("TERM_PROGRAM".to_string(), "zesterm".to_string()),
        ("TERM_PROGRAM_VERSION".to_string(), env!("CARGO_PKG_VERSION").to_string()),
    ];
    // An empty value means *unset* to both backends, not "set to the empty
    // string" -- code that only tests for presence must not see these at all.
    const STALE_IDENTITY: &[&str] = &[
        // Windows hosts.
        "WT_SESSION",
        "WT_PROFILE_ID",
        "TERMINUS_SUBLIME",
        "ConEmuANSI",
        // macOS Terminal.app.
        "TERM_SESSION_ID",
        // iTerm2. LC_TERMINAL travels over ssh; see the note above.
        "ITERM_SESSION_ID",
        "ITERM_PROFILE",
        "LC_TERMINAL",
        "LC_TERMINAL_VERSION",
        // The cross-platform emulators, which are as likely to be the launcher
        // as anything native.
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "KITTY_LISTEN_ON",
        "ALACRITTY_WINDOW_ID",
        "ALACRITTY_SOCKET",
        "ALACRITTY_LOG",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "WEZTERM_EXECUTABLE",
        "GHOSTTY_RESOURCES_DIR",
        // VTE-based terminals on Linux (GNOME Terminal, Tilix, Terminator).
        "VTE_VERSION",
        // Editors and agents that mark the shells *they* start.
        //
        // Not terminal identity, but the same failure with a worse blast radius:
        // a program reads one of these, concludes it is running inside a session
        // that owns it, and changes behaviour. Claude Code disables transcript
        // saving on `CLAUDE_CODE_CHILD_SESSION` and says so in a banner, which is
        // how this was noticed; anything keying off `CLAUDECODE` or
        // `VSCODE_INJECTION` does it silently.
        //
        // The blast radius is the daemon. A terminal that spawns its own shell
        // leaks only its own launch context, but zesterm's shells come from a
        // long-lived daemon (ADR-007) whose environment is frozen at first
        // spawn — so a daemon that happened to start from inside an agent
        // session hands these to every shell on the machine, for hours, in
        // windows opened long afterwards from somewhere else entirely.
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_VERSION",
        "CLAUDE_EFFORT",
        "CLAUDE_PID",
        // VS Code's integrated terminal, and the shell-integration hook it
        // injects. `TERM_PROGRAM` is already overridden above, which makes these
        // look handled; they are read independently.
        "VSCODE_INJECTION",
        "VSCODE_SHELL_INTEGRATION",
        "VSCODE_GIT_ASKPASS_MAIN",
        "VSCODE_GIT_IPC_HANDLE",
        // JetBrains IDEs.
        "TERMINAL_EMULATOR",
        "JEDITERM_SOURCE",
    ];
    for stale in STALE_IDENTITY {
        if std::env::var_os(stale).is_some() {
            env.push(((*stale).to_string(), String::new()));
        }
    }
    env
}

/// Why `wait_for_child` returned.
///
/// Lives here rather than in a backend so callers — `pty_dump` among them — can
/// match on it without a `cfg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
    /// The child process exited, with this code. A unix child killed by a
    /// signal is reported as `128 + signal`, following shell convention.
    Exited(u32),
    /// The wait timed out; the child is still running.
    StillRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("creating the pseudoconsole failed: {0}")]
    Create(#[source] std::io::Error),
    #[error("spawning `{command}` failed: {source}")]
    Spawn { command: String, #[source] source: std::io::Error },
    #[error("resizing the pseudoconsole failed: {0}")]
    Resize(#[source] std::io::Error),
    #[error("pty i/o failed: {0}")]
    Io(#[from] std::io::Error),
}

/// A source of terminal bytes with a resizable viewport.
///
/// One implementation today. It exists as a trait so the remote milestones can
/// introduce an SSH- or daemon-backed transport without the session layer above
/// having to change shape.
pub trait PtyTransport: Send {
    /// Take the read half. Returns `None` if it has already been taken —
    /// exactly one reader may exist, since two would interleave VT bytes and
    /// corrupt the stream.
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>>;
    /// A write half. Cheap to clone; writes are independently synchronized.
    fn writer(&self) -> Box<dyn Write + Send>;
    fn resize(&self, size: PtySize) -> Result<(), PtyError>;

    /// End the child, and make the reader reach EOF.
    ///
    /// Exists because on neither platform does dropping the transport suffice
    /// while a reader thread is parked in `read`, which is precisely a daemon's
    /// steady state:
    ///
    /// - On unix the hangup fires when the *last* duplicate of the master fd
    ///   closes, and the parked reader holds one. It cannot drop its duplicate
    ///   until the read returns, and the read cannot return until the hangup.
    ///   Nothing outside that cycle breaks it.
    /// - On Windows dropping is the whole shutdown protocol and does work — but
    ///   only if the last `Arc` really is this one, which the session layer
    ///   cannot promise, since a listing or a poll may be holding a clone.
    ///
    /// Idempotent, and safe to call on a child that has already exited.
    /// Blocking, but bounded: the implementation escalates on a timer rather
    /// than waiting on a program that has decided not to leave.
    ///
    /// **Not called when a client merely detaches.** Ending the child is the
    /// point of `CloseSession` and the opposite of `Detach`. → ADR-007.
    fn hangup(&self);

    /// Call `on_exit` once the child has exited, if the reader alone cannot
    /// tell.
    ///
    /// The default does nothing, which is right for unix: when the last process
    /// holding the slave goes, the master read fails with `EIO`, the reader
    /// treats that as EOF and unwinds, and the owner learns from that. A second
    /// waiter there would have to hold the child mutex to reap, which is the
    /// same mutex [`hangup`](Self::hangup) needs — a deadlock in exchange for
    /// nothing.
    ///
    /// **Windows overrides it**, because there the reader can never be the
    /// signal: ConPTY keeps its own copy of the output pipe's write end for as
    /// long as the `HPCON` lives, so a blocked `ReadFile` stays blocked after
    /// the shell is gone (gotcha 2b). Until it did, a shell exiting on its own
    /// was never noticed there — no `Exited` reached any client and the session
    /// was kept for the life of the daemon. → #18.
    ///
    /// A first attempt waited on the process handle and called back from there,
    /// and was backed out after Windows CI ran for over an hour. The hang was
    /// later traced to something else entirely, so that code was never shown
    /// wrong — but it *was* incomplete: reporting the exit the instant the wait
    /// returns truncates the tail, because gotcha 2c says ConPTY paints on its
    /// own schedule. See `ConPty::watch_exit` for how all three constraints are
    /// met at once.
    ///
    /// Implementations must block on the OS's own wait rather than polling — a
    /// per-session timer is exactly the sort of thing that costs the 0%-idle
    /// guarantee. A bounded wait that runs *once*, after the child is already
    /// gone, is not that.
    fn watch_exit(&self, on_exit: Box<dyn FnOnce() + Send>) {
        drop(on_exit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Injecting twice doubles every marker.
    ///
    /// Not hypothetical: PowerShell's hook lands *in the command line*, and a
    /// command line is the one part of a spawn that travels. The app builds one
    /// and hands it to the daemon, which detects a PowerShell in it and would
    /// inject again — and a doubled marker stream reads to the parser as an
    /// empty block between each real one, which is a subtly wrong index rather
    /// than an obviously broken one.
    #[test]
    fn a_command_line_that_already_loads_the_hook_is_not_amended_again() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut spec =
            CommandSpec { command_line: "pwsh -NoLogo".into(), cwd: None, env: Vec::new() };
        spec.enable_shell_integration(&dir);
        let once = spec.command_line.clone();
        spec.enable_shell_integration(&dir);

        assert_eq!(spec.command_line, once, "the hook was injected a second time");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A shell hooked through the environment leaves the command line alone.
    #[test]
    fn hooking_zsh_does_not_touch_its_command_line() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-zsh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut spec = CommandSpec { command_line: "/bin/zsh".into(), cwd: None, env: Vec::new() };
        spec.enable_shell_integration(&dir);

        assert_eq!(spec.command_line, "/bin/zsh");
        assert!(
            spec.env.iter().any(|(k, _)| k == "ZDOTDIR"),
            "zsh finds its hook through ZDOTDIR and nothing else"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A PowerShell already running a command of its own is left exactly alone.
    ///
    /// `-Command` swallows the rest of the line, so appending after one does not
    /// add a second `-Command` — it adds text to the user's command and breaks
    /// their shell, which is strictly worse than the missing blocks it was meant
    /// to fix.
    #[test]
    fn a_powershell_that_already_runs_a_command_is_spawned_exactly_as_asked() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut spec = CommandSpec {
            command_line: "pwsh -NoLogo -Command Get-Date".into(),
            cwd: None,
            env: Vec::new(),
        };
        spec.enable_shell_integration(&dir);

        assert_eq!(spec.command_line, "pwsh -NoLogo -Command Get-Date");
        assert!(spec.env.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A shell we cannot hook is spawned unchanged.
    #[test]
    fn a_shell_with_no_hook_is_spawned_exactly_as_asked() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-none-{}", std::process::id()));
        let mut spec = CommandSpec { command_line: "cmd.exe".into(), cwd: None, env: Vec::new() };
        spec.enable_shell_integration(&dir);

        assert_eq!(spec.command_line, "cmd.exe", "cmd has no hook and must not be given one");
        assert!(spec.env.is_empty());
        assert!(!dir.exists(), "nothing should have been written for a shell with no hook");
    }

    /// The order that decides whether a Windows user gets command blocks at all.
    ///
    /// The old code probed one hardcoded path, so a scoop or winget PowerShell
    /// — neither of which installs there — fell through to `cmd.exe`, which has
    /// no prompt hook and therefore no blocks, ever.
    #[cfg(windows)]
    #[test]
    fn the_windows_shell_is_looked_up_on_path_before_anywhere_else() {
        let scoop = r"C:\Users\andy\scoop\shims";
        let pf = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let ps51 = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
        let path = format!(r"C:\Windows\System32;{scoop}");

        let only = |want: String| move |p: &std::path::Path| p == std::path::Path::new(&want);

        assert_eq!(
            resolve_windows_shell(&path, "cmd.exe", only(format!(r"{scoop}\pwsh.exe"))),
            format!("\"{scoop}\\pwsh.exe\" -NoLogo"),
            "a PATH pwsh is what typing `pwsh` runs, so it is what the user means"
        );
        assert_eq!(
            resolve_windows_shell(&path, "cmd.exe", only(pf.to_string())),
            format!("\"{pf}\" -NoLogo"),
            "an installer-placed pwsh is still a pwsh when PATH predates the install"
        );
        assert_eq!(
            resolve_windows_shell(&path, "cmd.exe", only(ps51.to_string())),
            format!("\"{ps51}\" -NoLogo"),
            "5.1 ships with Windows and takes the same hook; falling past it to cmd \
             trades a shell with command blocks for one that cannot have them"
        );
        assert_eq!(
            resolve_windows_shell(&path, r"C:\Windows\system32\cmd.exe", |_| false),
            r"C:\Windows\system32\cmd.exe",
            "with no PowerShell at all, COMSPEC is the honest answer"
        );
    }

    /// An inherited agent-session marker must not reach the child.
    ///
    /// Found by running `claude` inside zesterm: it reported transcript saving
    /// off, because `CLAUDE_CODE_CHILD_SESSION` had been inherited and it
    /// concluded it was a child of the session that started the *terminal*.
    ///
    /// The reason this matters more here than in other terminals is the daemon.
    /// A terminal that spawns its own shell leaks only its own launch context;
    /// zesterm's shells come from a long-lived daemon whose environment is fixed
    /// at first spawn, so one daemon started from inside an agent session hands
    /// the marker to every shell on the machine until it is restarted.
    #[test]
    fn an_inherited_session_marker_is_cleared() {
        // SAFETY: single-threaded test; nothing else reads the environment here.
        unsafe { std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1") };
        let env = terminal_env();
        unsafe { std::env::remove_var("CLAUDE_CODE_CHILD_SESSION") };

        let entry = env.iter().find(|(k, _)| k == "CLAUDE_CODE_CHILD_SESSION");
        let (_, value) = entry.expect(
            "the marker was in scope and must be listed for clearing, or every \
             shell this daemon starts inherits it",
        );
        assert!(
            value.is_empty(),
            "an empty value is what both backends read as *unset*; anything else \
             sets it again"
        );
    }

    /// Only what is actually inherited is mentioned.
    ///
    /// Pushing every name unconditionally would work, but it makes the child's
    /// environment a list of variables to unset that were never set, and hides
    /// what a session really inherited when debugging one.
    #[test]
    fn a_marker_that_was_never_set_is_not_mentioned() {
        unsafe { std::env::remove_var("JEDITERM_SOURCE") };
        let env = terminal_env();
        assert!(
            !env.iter().any(|(k, _)| k == "JEDITERM_SOURCE"),
            "nothing to clear, so nothing to say"
        );
    }

    #[test]
    fn the_terminal_still_identifies_itself() {
        // The clearing list sits next to these, and an over-eager edit to it
        // would take the identity with it -- which reads as a broken renderer,
        // since a child with no TERM assumes a dumb terminal and stops emitting
        // colour at all.
        let env = terminal_env();
        for (key, want) in [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")] {
            let (_, got) = env.iter().find(|(k, _)| k == key).expect("must be set");
            assert_eq!(got, want, "{key} is what tells the child what it is talking to");
        }
    }
}
