//! Getting the OSC 133 hook into the shell, without touching the user's files.
//!
//! Command blocks need the shell to say where a command starts and ends. The
//! shell will not do that on its own, so something has to load a hook into it.
//!
//! # Injection, not installation
//!
//! zesterm arranges the *spawn* so the shell loads the hook itself. **No file of
//! the user's is read for modification, written, or appended to.** This is what
//! kitty, Ghostty and VS Code all default to, and kitty states the property
//! directly: *"No files are added or modified."* Appending `eval "$(...)"` to
//! `~/.zshrc` — iTerm2's older model — is the alternative, and it is the one
//! that needs a consent prompt, a fingerprint of the file, and a story for what
//! happens when the user edits it afterwards.
//!
//! Which knob the spawn turns is the shell's business, not ours, which is why
//! [`install`] returns an [`Injection`] rather than a list of variables: zsh is
//! hooked purely through the environment (`ZDOTDIR`), PowerShell purely through
//! the command line (`-Command`), and WSL — when it arrives — needs both, since
//! `WSLENV` is the only way a variable crosses into the distro and the inner
//! shell still has to be named on the command line.
//!
//! # Why it can only reach the first shell
//!
//! Injection applies to the process zesterm starts. A shell started *inside*
//! that one — `ssh`, `tmux`, `nix-shell`, a container, a bare `zsh` — is not
//! touched, and no amount of cleverness changes that. The escape hatch is
//! [`hook`], which prints the same script for a person to load by hand on the
//! far side.
//!
//! # Which shells, and why not the others
//!
//! zsh, PowerShell, and bash — including a bash on the far side of a WSL
//! launcher: `wsl.exe -d Ubuntu -- bash` walks to the inner shell, the shim
//! path crosses as `WSLENV`-translated environment, and `--init-file
//! $ZESTERM_BASH_INIT` names it (see [`install`]). The absences are not an
//! oversight:
//!
//! - **fish** has a documented mechanism and cannot be *seen working* on the
//!   machines this has been written on. Writing it blind is how the attach
//!   path nearly shipped compiled and unseen.
//! - **a bare `wsl.exe`** names no shell, and guessing the distro's default
//!   would mean hooking a shell we cannot identify. Name the inner shell —
//!   `wsl.exe -d Ubuntu -- bash` — and it is hooked like any other.
//! - **`cmd.exe`** has no prompt-function mechanism at all. There is no hook to
//!   write, so a `cmd` window has no command blocks and never will. Said out
//!   loud here, and logged at `info` by [`crate::CommandSpec::enable_shell_integration`],
//!   because a silent zero is what made the PowerShell gap take a screenshot to
//!   notice (#83).

use std::io;
use std::path::{Path, PathBuf};

/// A shell zesterm knows how to hook.
///
/// A variant names a **hook dialect, not a program**. `Pwsh` covers both
/// `pwsh.exe` and `powershell.exe` because one script serves both, and a future
/// `Bash` would cover bash-in-WSL and bash-on-macOS alike. Which executable a
/// profile actually launches is the profile layer's business; splitting this
/// enum by executable would only mean writing every hook twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    /// PowerShell — Core (`pwsh`) and Windows PowerShell 5.1 alike.
    Pwsh,
    /// bash — native on unix, and named through a WSL launcher on Windows.
    Bash,
}

impl Shell {
    /// Recognise the shell from a command line, if it is one we can hook.
    ///
    /// Matches the *executable* only. A trailing argument that happens to
    /// contain `zsh` — `vim zsh-notes.md` — is not a zsh, and neither is a
    /// login shell spelled `-zsh`, which is why the leading `-` is stripped.
    ///
    /// Tokens are parsed quote-aware rather than split on whitespace: Windows'
    /// default shell lives at `"C:\Program Files\PowerShell\7\pwsh.exe"`, and
    /// splitting on whitespace makes its executable `C:\Program` — a name that
    /// matches nothing, so *every* Windows shell silently went unhooked (#83).
    ///
    /// A WSL launcher is walked to the shell it names: `wsl.exe -d Ubuntu --
    /// bash` is a bash for injection purposes, because the launcher's own
    /// flags decide nothing about the prompt. Only bash comes back through
    /// that path — an inner zsh would need `ZDOTDIR` to cross the boundary,
    /// which `WSLENV` could carry but nothing here arranges yet — and a bare
    /// `wsl.exe` names no shell at all, so it is `None` rather than a guess.
    #[must_use]
    pub fn detect(command_line: &str) -> Option<Self> {
        let tokens: Vec<&str> = tokens(command_line).collect();
        let first = tokens.first()?;
        let name = Path::new(first).file_name()?.to_str()?;
        if is_wsl_launcher(name) {
            let args = &tokens[1..];
            let inner = args[wsl_inner_index(args)?];
            let name = Path::new(inner).file_name()?.to_str()?;
            let name = name.strip_prefix('-').unwrap_or(name);
            return (name == "bash").then_some(Self::Bash);
        }
        Self::from_executable(name)
    }

    /// The variant a bare executable name maps to.
    fn from_executable(name: &str) -> Option<Self> {
        let name = name.strip_prefix('-').unwrap_or(name);
        // zsh and bash are matched exactly and the PowerShells
        // case-insensitively, which is not inconsistency but the two
        // filesystems: a `ZSH` next to a `zsh` on unix is a different file,
        // and `PWSH.EXE` on Windows is not. `bash.exe` — Git Bash — is
        // deliberately not a bash here: MSYS rewrites unix-looking arguments
        // before the program sees them, and an `--init-file /mnt/...`-style
        // path through that machinery is untested territory.
        if name == "zsh" {
            return Some(Self::Zsh);
        }
        if name == "bash" {
            return Some(Self::Bash);
        }
        if ["pwsh", "pwsh.exe", "powershell", "powershell.exe"]
            .iter()
            .any(|c| name.eq_ignore_ascii_case(c))
        {
            return Some(Self::Pwsh);
        }
        None
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Pwsh => "pwsh",
            Self::Bash => "bash",
        }
    }
}

/// Whether this executable name is the WSL launcher.
///
/// Case-insensitive because it is a Windows executable, wherever it is
/// installed from.
fn is_wsl_launcher(name: &str) -> bool {
    name.eq_ignore_ascii_case("wsl") || name.eq_ignore_ascii_case("wsl.exe")
}

/// Where a WSL launcher's own flags end and its inner command begins.
///
/// Takes and indexes the launcher's *arguments* (everything after `wsl.exe`):
/// flags that take a value consume it, bare flags are skipped, and the first
/// token that is not a flag is the inner command. An *unknown* flag returns
/// `None` — it may take a value we would misread as the command, and injecting
/// into a guess breaks the user's shell where declining merely leaves it
/// without blocks.
fn wsl_inner_index(args: &[&str]) -> Option<usize> {
    let mut i = 0;
    while let Some(token) = args.get(i) {
        match token.to_ascii_lowercase().as_str() {
            // Everything after `--` is the command, flags included -- that is
            // the marker's whole job.
            "--" => return (i + 1 < args.len()).then_some(i + 1),
            "-d" | "--distribution" | "--distribution-id" | "-u" | "--user" | "--cd"
            | "--shell-type" => i += 2,
            "--system" | "--exec" | "-e" => i += 1,
            t if t.starts_with('-') => return None,
            _ => return Some(i),
        }
    }
    None
}

/// The tokens of a command line, with a quoted path kept whole.
///
/// Not a shell-grade parser — no escapes, no single quotes, because neither
/// `CreateProcessW` nor [`crate::CommandSpec`] promises them for the program
/// name. It only has to keep `"C:\Program Files\..."` in one piece, and to
/// walk a WSL launcher's flags to the shell they name without a second,
/// subtly different parse.
fn tokens(command_line: &str) -> impl Iterator<Item = &str> {
    let mut rest = command_line;
    std::iter::from_fn(move || {
        let line = rest.trim_start();
        if let Some(after) = line.strip_prefix('"') {
            // An unterminated quote takes the rest of the line, which is what
            // `CreateProcessW` does with it too.
            let (token, remainder) = match after.find('"') {
                Some(end) => (&after[..end], &after[end + 1..]),
                None => (after, ""),
            };
            rest = remainder;
            return Some(token).filter(|t| !t.is_empty());
        }
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        rest = &line[end..];
        Some(&line[..end]).filter(|t| !t.is_empty())
    })
}

/// The hook script, for a person to load by hand.
///
/// This is the documented path for everything injection cannot reach — ssh,
/// tmux, subshells — and it is the *same* script the shim loads, so the two
/// cannot drift.
///
/// ```text
/// # ~/.zshrc, on a machine you ssh into
/// eval "$(zesterm --shell-integration zsh)"
///
/// # $PROFILE, on a Windows box you ssh into
/// zesterm --shell-integration pwsh | Out-String | Invoke-Expression
/// ```
#[must_use]
pub fn hook(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("../shell-integration/zsh/zesterm.zsh"),
        Shell::Pwsh => include_str!("../shell-integration/pwsh/zesterm.ps1"),
        Shell::Bash => include_str!("../shell-integration/bash/zesterm.bash"),
    }
}

/// The PowerShell hook's file name, and the marker that says a command line
/// already carries it.
///
/// The name is load-bearing rather than cosmetic: PowerShell's injection lands
/// *in the command line*, and a command line is the one part of a spawn that
/// travels — the app hands one to the daemon, which would otherwise detect a
/// PowerShell and inject a second time. See
/// [`already_injected`].
pub const SHIM_PWSH: &str = "zesterm.ps1";

/// The bash shim's file name, and — like [`SHIM_PWSH`] — the marker that says
/// a command line already carries it. The WSL spelling of the amendment names
/// [`WSL_SHIM_VAR`] instead of a path, so both are markers.
pub const SHIM_BASH: &str = "zesterm-shim.bash";

/// The variable that carries the bash shim's path across a WSL boundary.
///
/// `WSLENV` translates it (`/p`) into the distro's view of the same file, and
/// the command line says `--init-file $ZESTERM_BASH_INIT` — unquoted, because
/// WSL escapes a `"` into a literal character of the filename, and expanded by
/// the distro's default shell, which is why `--exec` declines injection.
pub const WSL_SHIM_VAR: &str = "ZESTERM_BASH_INIT";

/// Whether this command line already loads a hook.
///
/// Injecting twice is not merely redundant: every marker is emitted twice, which
/// the parser reads as an empty block between each real one. Cheap to check and
/// impossible to notice going wrong.
///
/// Case-insensitively, because the command lines this has to recognise are
/// Windows ones and a person who wrote their own by hand may well have typed
/// `ZESTERM.PS1`. The path opens the same file either way, so a case-sensitive
/// check would inject a second hook into a shell that already had one.
#[must_use]
pub fn already_injected(command_line: &str) -> bool {
    // Both sides lowered, rather than relying on the markers happening to be
    // lowercase already -- renaming one should not silently disarm this.
    let line = command_line.to_ascii_lowercase();
    [SHIM_PWSH, SHIM_BASH, WSL_SHIM_VAR]
        .iter()
        .any(|marker| line.contains(&marker.to_ascii_lowercase()))
}

/// What a shell needs at spawn in order to load the hook.
///
/// Two halves because the shells disagree about where the knob is. zsh takes
/// `env` only; PowerShell takes `args` only, having no `ZDOTDIR` analogue —
/// the one place to say "load this file first" is the command line itself. A
/// bare `Vec<(String, String)>` could express neither the second case nor the
/// WSL one, which needs both at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Injection {
    /// Environment entries layered over the child's. An empty *value* unsets.
    pub env: Vec<(String, String)>,
    /// Appended to the command line verbatim, leading space included. Empty for
    /// a shell hooked purely through the environment.
    pub args: String,
}

const ZSH_ZSHENV: &str = include_str!("../shell-integration/zsh/.zshenv");
const ZSH_ZPROFILE: &str = include_str!("../shell-integration/zsh/.zprofile");
const ZSH_ZSHRC: &str = include_str!("../shell-integration/zsh/.zshrc");
const ZSH_ZLOGIN: &str = include_str!("../shell-integration/zsh/.zlogin");
const BASH_SHIM: &str = include_str!("../shell-integration/bash/zesterm-shim.bash");

/// Write the shim into `dir` and return the environment that activates it.
///
/// `dir` is supplied by the caller rather than resolved here: `zest-pty` does
/// not depend on `zest-config`, and the config directory is a question about
/// where zesterm is installed rather than about how to spawn a process.
///
/// Rewritten on every spawn rather than written once and checked. The files are
/// a few hundred bytes, and a stale shim from a previous version is a class of
/// bug with no symptom — the shell starts, the prompt appears, and the blocks
/// are subtly wrong.
///
/// # Errors
///
/// If the shim cannot be written. The caller should spawn *without* integration
/// rather than refuse to open a terminal: a shell with no command blocks is a
/// working shell, and a terminal that will not start is not.
pub fn install(shell: Shell, command_line: &str, dir: &Path) -> io::Result<Injection> {
    match shell {
        Shell::Zsh => install_zsh(dir),
        Shell::Pwsh => install_pwsh(command_line, dir),
        Shell::Bash => install_bash(command_line, dir),
    }
}

/// PowerShell: dot-source the hook from the command line.
///
/// There is no `ZDOTDIR` analogue and no per-invocation rc file — `$PROFILE` is
/// a fixed path belonging to the user, and this module does not write those. So
/// the injection is `-NoExit -Command ". '<shim>'"`, which runs the hook and
/// then drops into the interactive shell as if nothing had happened.
fn install_pwsh(command_line: &str, dir: &Path) -> io::Result<Injection> {
    // `-Command` consumes the whole rest of the line, so appending ours after a
    // command line that already has one does not add a second `-Command`: it
    // silently becomes *text inside the user's command*, breaking their shell
    // outright rather than merely failing to hook it. Declining is the only
    // safe move, and it is why this function needs the command line at all.
    if !accepts_appended_args(command_line) {
        tracing::info!(
            command = %command_line,
            "this PowerShell already runs a command of its own; no command blocks"
        );
        return Ok(Injection::default());
    }

    let shim_dir = dir.join("pwsh");
    std::fs::create_dir_all(&shim_dir)?;
    let shim = shim_dir.join(SHIM_PWSH);
    std::fs::write(&shim, hook(Shell::Pwsh))?;

    let Some(shim) = shim.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shell integration directory is not valid UTF-8",
        ));
    };

    // Single quotes so a `$` in the path is not expanded; PowerShell escapes a
    // literal quote inside them by doubling it. The outer double quotes are for
    // `CreateProcessW`, which parses the line before PowerShell ever sees it --
    // and the character before the closing one is `'`, never a backslash, so
    // the trailing-backslash hazard of quoting a bare Windows path does not
    // arise here.
    Ok(Injection {
        env: Vec::new(),
        args: format!(" -NoExit -Command \". '{}'\"", shim.replace('\'', "''")),
    })
}

/// Whether appending arguments to this command line is safe.
///
/// False once PowerShell has been given something to run, since `-Command`,
/// `-File` and their abbreviations all swallow everything after them.
fn accepts_appended_args(command_line: &str) -> bool {
    // PowerShell accepts any unambiguous prefix of a parameter name, so `-c`,
    // `-com` and `-Command` are the same switch; `-f` likewise for `-File`.
    // Matching the full names only would let `pwsh -c foo` through, which is
    // the spelling people actually type.
    !command_line.split_whitespace().skip(1).any(|arg| {
        let Some(name) = arg.strip_prefix('-').or_else(|| arg.strip_prefix('/')) else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        !name.is_empty()
            && ["command", "file", "encodedcommand"].iter().any(|full| full.starts_with(&name))
    })
}

fn install_zsh(dir: &Path) -> io::Result<Injection> {
    let zdotdir: PathBuf = dir.join("zsh");
    std::fs::create_dir_all(&zdotdir)?;
    for (name, body) in [
        (".zshenv", ZSH_ZSHENV),
        (".zprofile", ZSH_ZPROFILE),
        (".zshrc", ZSH_ZSHRC),
        (".zlogin", ZSH_ZLOGIN),
        ("zesterm.zsh", hook(Shell::Zsh)),
    ] {
        std::fs::write(zdotdir.join(name), body)?;
    }

    let Some(zdotdir) = zdotdir.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shell integration directory is not valid UTF-8",
        ));
    };

    // Where the user's own dotfiles are, for the shim to hand control back to.
    // Their current ZDOTDIR if they set one -- relocating zsh's config that way
    // is documented and frameworks do it -- and `$HOME` otherwise, which is
    // what zsh itself falls back to.
    let user_zdotdir = std::env::var("ZDOTDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_default();

    Ok(Injection {
        env: vec![
            ("ZDOTDIR".to_string(), zdotdir.to_string()),
            ("ZESTERM_USER_ZDOTDIR".to_string(), user_zdotdir),
        ],
        args: String::new(),
    })
}

/// bash: source the hook through `--init-file`.
///
/// bash has no `ZDOTDIR` analogue; `--init-file` is its one per-invocation
/// knob, and it *replaces* the interactive startup files, which is why the
/// shim's first job is to run them itself — user's rc first, hook after, the
/// same order rule as the zsh shim and for the same reason.
///
/// Natively the args name the shim path, quoted. Through a WSL launcher the
/// path cannot be named directly — the distro sees a different filesystem — so
/// it crosses as [`WSL_SHIM_VAR`] with `WSLENV` doing the translation, and the
/// args say `--init-file $ZESTERM_BASH_INIT` for the distro's shell to expand.
/// This is the "both halves at once" case [`Injection`] was shaped for.
fn install_bash(command_line: &str, dir: &Path) -> io::Result<Injection> {
    let all: Vec<&str> = tokens(command_line).collect();
    let first = all.first().copied().unwrap_or_default();
    let via_wsl =
        Path::new(first).file_name().and_then(|n| n.to_str()).is_some_and(is_wsl_launcher);

    let bash_args: &[&str] = if via_wsl {
        let args = &all[1..];
        // `detect` found a bash here, so the walk succeeds; guarded anyway
        // because this function is callable with any line.
        let Some(inner) = wsl_inner_index(args) else {
            return Ok(Injection::default());
        };
        // `--exec` (and `--shell-type none`) launch the command with no shell
        // in between, so `$ZESTERM_BASH_INIT` never expands -- bash would
        // source a file literally named that, *instead of* the user's bashrc.
        // Losing their configuration is strictly worse than the missing blocks.
        let launcher = &args[..inner];
        let exec_mode = launcher
            .iter()
            .any(|f| f.eq_ignore_ascii_case("--exec") || f.eq_ignore_ascii_case("-e"))
            || launcher.windows(2).any(|w| {
                w[0].eq_ignore_ascii_case("--shell-type") && w[1].eq_ignore_ascii_case("none")
            });
        if exec_mode {
            tracing::info!(
                command = %command_line,
                "wsl --exec leaves no shell to expand the shim variable; no command blocks"
            );
            return Ok(Injection::default());
        }
        &args[inner + 1..]
    } else {
        &all[1..]
    };

    if !bash_accepts_appended_args(bash_args) {
        tracing::info!(
            command = %command_line,
            "this bash already runs or configures its own startup; no command blocks"
        );
        return Ok(Injection::default());
    }

    let shim_dir = dir.join("bash");
    let shim_path = shim_dir.join(SHIM_BASH);
    let Some(shim) = shim_path.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shell integration directory is not valid UTF-8",
        ));
    };
    if via_wsl && shim.chars().any(char::is_whitespace) {
        // The WSL spelling rides unquoted (see `WSL_SHIM_VAR`), so a space in
        // the path would word-split into arguments after expansion.
        tracing::info!(
            path = %shim,
            "the shell-integration path has whitespace, which cannot cross WSL unquoted; \
             no command blocks"
        );
        return Ok(Injection::default());
    }

    std::fs::create_dir_all(&shim_dir)?;
    std::fs::write(shim_dir.join("zesterm.bash"), hook(Shell::Bash))?;
    std::fs::write(&shim_path, BASH_SHIM)?;

    if via_wsl {
        Ok(Injection {
            env: vec![
                (WSL_SHIM_VAR.to_string(), shim.to_string()),
                (
                    "WSLENV".to_string(),
                    wslenv_value(std::env::var("WSLENV").ok().as_deref()),
                ),
            ],
            args: format!(" --init-file ${WSL_SHIM_VAR}"),
        })
    } else {
        Ok(Injection { env: Vec::new(), args: format!(" --init-file \"{shim}\"") })
    }
}

/// Whether appending `--init-file` to these bash arguments is safe and useful.
///
/// False once bash has been given something to run or told how to start:
/// `-c` and a bare script path make it non-interactive, where an init file is
/// never read; `--init-file`/`--rcfile`/`--norc` are the user turning exactly
/// the knob we would; `--posix` changes the startup files wholesale; and a
/// login bash (`-l`) ignores `--init-file` entirely — injecting there would
/// report success and do nothing, which is the silent zero all over again.
fn bash_accepts_appended_args(args: &[&str]) -> bool {
    for arg in args {
        match *arg {
            // What follows is a script path, not options.
            "--" => return false,
            a if a.starts_with("--") => {
                let name = &a[2..];
                let name = name.split('=').next().unwrap_or(name);
                if ["init-file", "rcfile", "norc", "posix", "login"].contains(&name) {
                    return false;
                }
            }
            a if a.starts_with('-') && a.len() > 1 => {
                // A short-option cluster: `-lic` is `-l -i -c`. `-s` takes its
                // script from stdin, which is just as non-interactive.
                if a[1..].chars().any(|c| matches!(c, 'c' | 'l' | 's')) {
                    return false;
                }
            }
            // A bare word is a script path: non-interactive.
            _ => return false,
        }
    }
    true
}

/// The `WSLENV` that ships the shim variable, appended to whatever already
/// crosses. `WSLENV` is itself inherited, so replacing it would silently strip
/// entries like Windows Terminal's `WT_SESSION` from every shell in the fleet.
fn wslenv_value(existing: Option<&str>) -> String {
    let entry = format!("{WSL_SHIM_VAR}/p");
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) if e.split(':').any(|v| v.split('/').next() == Some(WSL_SHIM_VAR)) => e.to_string(),
        Some(e) => format!("{e}:{entry}"),
        None => entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_is_recognised_by_its_executable_not_its_arguments() {
        assert_eq!(Shell::detect("/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::detect("/bin/zsh -l"), Some(Shell::Zsh));
        // A login shell is spelled with a leading dash by convention.
        assert_eq!(Shell::detect("-zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::detect("\"/opt/homebrew/bin/zsh\" -i"), Some(Shell::Zsh));

        // The argument mentions zsh; the program is not one. Injecting a
        // ZDOTDIR here would be harmless but wrong, and the same mistake with
        // bash's `--posix` would break the program outright.
        assert_eq!(Shell::detect("vim zsh-notes.md"), None);
        assert_eq!(Shell::detect(""), None);
    }

    #[test]
    fn bash_is_recognised_by_its_executable() {
        for line in ["bash", "/bin/bash", "/usr/bin/bash", "-bash", "/usr/local/bin/bash -i"] {
            assert_eq!(Shell::detect(line), Some(Shell::Bash), "{line} is a bash");
        }

        // The argument names a bash; the program is not one.
        assert_eq!(Shell::detect("vim bash-notes.md"), None);
        // Git Bash is not a bash here: `.exe` is part of the name, and MSYS
        // rewrites unix-looking arguments before the program sees them, so an
        // `--init-file` path through it is untested territory.
        assert_eq!(Shell::detect("bash.exe"), None);
        assert_eq!(Shell::detect(r#""C:\Program Files\Git\bin\bash.exe""#), None);
        // A unix filesystem: `BASH` is a different file from `bash`.
        assert_eq!(Shell::detect("BASH"), None);
    }

    #[test]
    fn a_wsl_launcher_is_walked_to_its_inner_shell() {
        // The launcher's flags decide nothing about the prompt; the shell they
        // leave behind does. This is the profile shape that makes WSL blocks
        // work at all: `wsl.exe -d Ubuntu -- bash`.
        for line in [
            "wsl.exe -d Ubuntu -- bash",
            "wsl -d Ubuntu bash",
            "wsl.exe --distribution Ubuntu -u andy -- bash -i",
            "WSL.EXE -d Ubuntu -- /usr/bin/bash",
            // `--` means everything after is the command, a login-shell
            // spelling included.
            "wsl.exe -d Ubuntu -- -bash",
        ] {
            assert_eq!(Shell::detect(line), Some(Shell::Bash), "{line} launches a bash");
        }

        // A bare launcher names no shell, and the distro's default is not
        // knowable from here. None, never a guess -- the log tells the user
        // to name it.
        assert_eq!(Shell::detect("wsl.exe -d Ubuntu"), None);
        assert_eq!(Shell::detect("wsl.exe"), None);
        // An inner zsh is real but unhookable today: its ZDOTDIR would need
        // WSLENV plumbing of its own, and injecting env that never crosses is
        // a hook that silently does nothing.
        assert_eq!(Shell::detect("wsl.exe -d Ubuntu -- zsh"), None);
        // An unknown flag may take a value; walking past it would misread the
        // value as the command.
        assert_eq!(Shell::detect("wsl.exe --mystery bash"), None);
        // A flag with its value missing is a broken line, not a bash.
        assert_eq!(Shell::detect("wsl.exe -d"), None);
    }

    #[test]
    fn a_bash_that_already_runs_something_is_left_alone() {
        // `-c` and a script path make bash non-interactive, where an init file
        // is never read; `--rcfile`/`--norc` are the user turning our knob
        // themselves; a login bash ignores `--init-file` outright, so hooking
        // it would report success and do nothing.
        for args in [
            &["-c", "ls"][..],
            &["script.sh"][..],
            &["--rcfile", "/tmp/rc"][..],
            &["--init-file", "/tmp/rc"][..],
            &["--norc"][..],
            &["--posix"][..],
            &["-l"][..],
            &["--login"][..],
            &["-lic", "ls"][..],
            &["-s"][..],
            &["--", "script.sh"][..],
        ] {
            assert!(!bash_accepts_appended_args(args), "{args:?} cannot take an init file");
        }

        for args in [&[][..], &["-i"][..], &["--noprofile"][..], &["--noediting", "-i"][..]] {
            assert!(bash_accepts_appended_args(args), "{args:?} drops into an interactive shell");
        }
    }

    #[test]
    fn bash_is_hooked_through_an_init_file() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-bash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let injection = install(Shell::Bash, "/bin/bash", &dir).expect("install");
        let shim = dir.join("bash").join(SHIM_BASH);
        assert!(shim.exists(), "the shim was not written");
        assert!(dir.join("bash").join("zesterm.bash").exists(), "the hook was not written");

        assert!(
            injection.env.is_empty(),
            "a native bash needs no environment: the init file is named on the command line"
        );
        assert!(
            injection.args.contains("--init-file"),
            "bash has no ZDOTDIR; the init file is its one per-invocation knob: {:?}",
            injection.args
        );
        assert!(
            injection.args.contains(shim.to_str().expect("utf-8 shim path")),
            "the amendment does not name the file it was just written to: {:?}",
            injection.args
        );

        let amended = format!("/bin/bash{}", injection.args);
        assert!(
            already_injected(&amended),
            "an amended command line must be recognisable, or the daemon injects a second time"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wsl_bash_is_hooked_through_wslenv_and_the_command_line() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-wsl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let line = "wsl.exe -d Ubuntu -- bash";
        let injection = install(Shell::Bash, line, &dir).expect("install");
        let shim = dir.join("bash").join(SHIM_BASH);
        assert!(shim.exists(), "the shim was not written");

        // The env half: the Windows path, and the WSLENV entry that makes WSL
        // itself translate it into the distro's view -- which is what makes a
        // custom automount root work without us knowing about it.
        let vars: std::collections::HashMap<_, _> = injection.env.iter().cloned().collect();
        assert_eq!(
            vars.get(WSL_SHIM_VAR).map(String::as_str),
            shim.to_str(),
            "the variable does not carry the shim"
        );
        let wslenv = vars.get("WSLENV").expect("without WSLENV nothing crosses into the distro");
        assert!(
            wslenv.ends_with(&format!("{WSL_SHIM_VAR}/p")),
            "/p is the flag that translates the path: {wslenv:?}"
        );

        // The args half: the variable's name, unquoted -- WSL escapes a `"`
        // into a literal character of the filename, so the quoted spelling
        // opens a file that does not exist.
        assert_eq!(injection.args, format!(" --init-file ${WSL_SHIM_VAR}"));
        assert!(
            already_injected(&format!("{line}{}", injection.args)),
            "an amended command line must be recognisable, or the daemon injects a second time"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wslenv_appends_rather_than_clobbers() {
        // WSLENV is inherited and shared: Windows Terminal, VS Code and the
        // user's own setup all put entries there, and replacing it strips
        // theirs from every shell zesterm spawns.
        assert_eq!(wslenv_value(None), "ZESTERM_BASH_INIT/p");
        assert_eq!(wslenv_value(Some("")), "ZESTERM_BASH_INIT/p");
        assert_eq!(
            wslenv_value(Some("WT_SESSION:WT_PROFILE_ID")),
            "WT_SESSION:WT_PROFILE_ID:ZESTERM_BASH_INIT/p"
        );
        // Already present -- a respawn under an injected daemon -- stays put
        // rather than growing forever.
        assert_eq!(
            wslenv_value(Some("ZESTERM_BASH_INIT/p")),
            "ZESTERM_BASH_INIT/p",
            "a second spawn must not append a second entry"
        );
    }

    #[test]
    fn wsl_exec_mode_and_spaced_paths_are_declined() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-decl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // `--exec` launches the command with no shell in between, so the
        // variable never expands -- bash would source a file literally named
        // `$ZESTERM_BASH_INIT` *instead of* the user's bashrc. Declining
        // loses blocks; injecting loses their configuration.
        for line in
            ["wsl.exe --exec bash", "wsl.exe -e bash", "wsl.exe --shell-type none -- bash"]
        {
            let injection = install(Shell::Bash, line, &dir).expect("install");
            assert_eq!(injection, Injection::default(), "{line} has no shell to expand $VAR");
        }

        // The WSL spelling rides unquoted, so a space in the shim path would
        // word-split into arguments after expansion.
        let spaced = std::env::temp_dir().join(format!("zesterm si {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&spaced);
        let injection = install(Shell::Bash, "wsl.exe -d Ubuntu -- bash", &spaced).expect("install");
        assert_eq!(
            injection,
            Injection::default(),
            "a spaced path cannot cross WSL unquoted; declining beats a broken rc"
        );
        // A *native* bash quotes the path, so the same directory is fine there.
        let native = install(Shell::Bash, "/bin/bash", &spaced).expect("install");
        assert!(
            native.args.contains("--init-file \""),
            "natively the path is quoted and a space is no obstacle: {:?}",
            native.args
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&spaced).ok();
    }

    #[test]
    fn the_bash_shim_hands_control_back_to_the_users_own_dotfiles() {
        // Same property as the zsh shim set: `--init-file` *replaces* the
        // interactive startup files, so the shim must run them itself or the
        // user's shell silently loses its configuration -- and it must run
        // them *before* the hook, or a prompt framework's rc clobbers
        // PROMPT_COMMAND a line after we set it.
        let etc = BASH_SHIM.find("&& . /etc/bash.bashrc").expect("sources the system rc");
        let user = BASH_SHIM.find("&& . ~/.bashrc").expect("sources the user's rc");
        let ours = BASH_SHIM.find(". \"${BASH_SOURCE[0]%/*}/zesterm.bash\"").expect("loads the hook");
        assert!(etc < user, "bash runs the system rc before the user's; so must the shim");
        assert!(user < ours, "the hook is loaded before the user's rc");
    }

    #[test]
    fn the_bash_hook_chains_prompt_command_and_the_debug_trap() {
        // The property that makes injection safe to do without asking: a
        // prompt that silently loses starship or oh-my-bash to a terminal is
        // a failure nobody suspects the terminal for -- and a DEBUG trap the
        // user installed is their tooling, not ours to discard.
        let hook = hook(Shell::Bash);
        assert!(
            hook.contains("__zesterm_user_prompt_command=") && hook.contains("(exit \"$ret\")"),
            "the user's PROMPT_COMMAND must run, and with the exit status it expects to read"
        );
        assert!(
            hook.contains("trap -p DEBUG"),
            "a pre-existing DEBUG trap must be chained, not replaced"
        );
        // And the modern path must be PS0, not the trap: a DEBUG trap never
        // fires for a top-level compound, so `(exit 3)` would run, finish and
        // leave no marker at all -- seen live before this line existed.
        assert!(
            hook.contains("PS0="),
            "without PS0 a compound command produces no block on any modern bash"
        );
    }

    #[test]
    fn the_bash_hook_is_safe_to_source_twice() {
        // Someone with `eval "$(zesterm --shell-integration bash)"` in their
        // rc *and* injection active sources it twice.
        assert!(hook(Shell::Bash).contains("__zesterm_loaded"));
    }

    #[test]
    fn the_bash_hook_reports_the_cwd_before_it_opens_the_block() {
        // Ordering, not decoration: `133;A` opens a block and stamps it with
        // the working directory known at that moment. A cwd emitted afterwards
        // lands on the *next* block, so every path shown is one command stale.
        // In bash the guarantee is structural: OSC 7 is emitted from
        // PROMPT_COMMAND, `133;A` lives in PS1, and bash prints PS1 only after
        // PROMPT_COMMAND has finished.
        let hook = hook(Shell::Bash);
        let precmd = &hook[hook.find("__zesterm_precmd() {").expect("has a precmd")..];
        let precmd = &precmd[..precmd.find("\n}").expect("the function ends")];
        assert!(precmd.contains("7;file://"), "the cwd is not reported from precmd");
        assert!(
            !precmd.contains("133;A"),
            "A emitted from precmd would land on the line before the prompt"
        );
        assert!(
            hook.contains(r"PS1='\[\e]133;A\a\]'"),
            "A must open the prompt from inside PS1"
        );
    }

    #[test]
    fn the_posix_hooks_ship_with_unix_line_endings() {
        // include_str! embeds checkout bytes, and the shim is written into a
        // file a *Linux* bash sources -- from a Windows-built daemon, across
        // the WSL boundary. A CRLF checkout fails every line of it with
        // `$'\r': command not found`, which is why .gitattributes pins this
        // tree to LF; this test is what notices that rule being lost.
        for (name, body) in [
            ("zesterm.bash", hook(Shell::Bash)),
            ("zesterm-shim.bash", BASH_SHIM),
            ("zesterm.zsh", hook(Shell::Zsh)),
            (".zshenv", ZSH_ZSHENV),
            (".zshrc", ZSH_ZSHRC),
        ] {
            assert!(!body.contains('\r'), "{name} carries CR bytes and would fail in a POSIX shell");
        }
    }

    #[test]
    fn a_quoted_executable_path_survives_the_space_in_program_files() {
        // The bug behind #83, and the reason a `Pwsh` variant alone would have
        // changed nothing: Windows' own default shell is quoted because its path
        // has a space, and splitting the line on whitespace made the executable
        // `C:\Program`. Every Windows shell went unhooked, and the only trace was
        // a status bar reading `0 blocks`.
        //
        // Asserted on `tokens` rather than through `detect`, so it runs on
        // every platform: `detect` finishes the job with `Path::file_name`, which
        // only treats `\` as a separator on Windows, and the quoting half of the
        // bug is not Windows-specific.
        let first = |line| tokens(line).next();
        assert_eq!(
            first(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#),
            Some(r"C:\Program Files\PowerShell\7\pwsh.exe")
        );
        assert_eq!(first("  /bin/zsh -l"), Some("/bin/zsh"));
        assert_eq!(first(r#""/opt/my shells/zsh""#), Some("/opt/my shells/zsh"));
        // An unterminated quote takes the rest, which is what `CreateProcessW`
        // does with it too.
        assert_eq!(first(r#""C:\Program Files\pwsh.exe"#), Some(r"C:\Program Files\pwsh.exe"));
        assert_eq!(first(""), None);
        assert_eq!(first(r#""""#), None);

        // The walk keeps the same rules past the first token: a quoted launcher
        // path must not smear into the flags after it.
        assert_eq!(
            tokens(r#""C:\WINDOWS\system32\wsl.exe" -d Ubuntu -- bash"#).collect::<Vec<_>>(),
            vec![r"C:\WINDOWS\system32\wsl.exe", "-d", "Ubuntu", "--", "bash"]
        );
    }

    #[test]
    fn every_spelling_of_powershell_is_recognised() {
        // One variant, four executables. Windows paths are case-insensitive, so
        // the case a launcher happens to use is not a signal.
        for line in ["pwsh", "pwsh.exe", "PWSH.EXE", "powershell", "powershell.exe"] {
            assert_eq!(Shell::detect(line), Some(Shell::Pwsh), "{line} is a PowerShell");
        }
        // pwsh runs on macOS and Linux too, so this one is not Windows-only.
        assert_eq!(Shell::detect("/usr/local/bin/pwsh -Login"), Some(Shell::Pwsh));

        // The argument names a PowerShell; the program is Notepad.
        assert_eq!(Shell::detect("notepad pwsh.ps1"), None);
        assert_eq!(Shell::detect("powershell-notes"), None);
    }

    /// The whole command line, on the platform whose paths these are.
    ///
    /// Windows-only because `detect` ends in `Path::file_name`, which treats `\`
    /// as a separator only on Windows — deliberately, since a command line is
    /// interpreted by the machine that will run it, and `\` is a legal character
    /// in a unix file name rather than a separator.
    #[cfg(windows)]
    #[test]
    fn a_windows_command_line_is_detected_whole() {
        for line in [
            r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#,
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo",
            r"C:\Users\someone\scoop\shims\pwsh.exe -NoLogo",
        ] {
            assert_eq!(Shell::detect(line), Some(Shell::Pwsh), "{line} is a PowerShell");
        }

        // `zsh.exe` under Git for Windows is not zsh: the variant is matched
        // exactly, and `.exe` is part of the name.
        assert_eq!(Shell::detect(r#""C:\Program Files\Git\bin\zsh.exe""#), None);
        assert_eq!(Shell::detect(r"C:\Windows\System32\cmd.exe"), None);

        // The launcher walk, from the fully-qualified spelling a Windows
        // profile actually stores. Here rather than in the walk's own test,
        // for the same reason as the paths above: only Windows'
        // `Path::file_name` sees `wsl.exe` inside `C:\WINDOWS\system32\`.
        assert_eq!(
            Shell::detect(r#""C:\WINDOWS\system32\wsl.exe" --cd C:\dev -d Ubuntu -- bash"#),
            Some(Shell::Bash),
            "the profile-shaped WSL line launches a bash"
        );
    }

    #[test]
    fn a_powershell_that_already_runs_something_is_left_alone() {
        // `-Command` swallows the rest of the line, so appending after one does
        // not produce a second `-Command` -- it produces text inside the user's
        // command. That breaks their shell outright, which is strictly worse
        // than the missing command blocks it was meant to fix.
        for line in [
            "pwsh -Command Get-Date",
            "pwsh -c Get-Date",
            "pwsh -NoLogo -File C:\\bootstrap.ps1",
            "pwsh -f C:\\bootstrap.ps1",
            "pwsh -EncodedCommand RwBlAHQA",
        ] {
            assert!(!accepts_appended_args(line), "{line} already runs a command of its own");
        }

        for line in ["pwsh", "pwsh -NoLogo", "pwsh -NoLogo -NoProfile", "pwsh -Login"] {
            assert!(accepts_appended_args(line), "{line} drops into an interactive shell");
        }
    }

    #[test]
    fn powershell_is_hooked_through_the_command_line_because_it_has_no_zdotdir() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-pwsh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let injection = install(Shell::Pwsh, "pwsh -NoLogo", &dir).expect("install");
        let shim = dir.join("pwsh").join(SHIM_PWSH);
        assert!(shim.exists(), "the hook was not written");

        assert!(injection.env.is_empty(), "PowerShell needs no environment to find the hook");
        assert!(
            injection.args.contains("-NoExit") && injection.args.contains("-Command"),
            "the hook has to be dot-sourced from the command line: {:?}",
            injection.args
        );
        assert!(
            injection.args.contains(shim.to_str().expect("utf-8 shim path")),
            "the amendment does not name the file it was just written to: {:?}",
            injection.args
        );

        // The whole amendment is one `CreateProcessW` argument, so the shim path
        // must sit inside the quotes rather than end them early.
        let amended = format!("pwsh -NoLogo{}", injection.args);
        assert!(
            already_injected(&amended),
            "an amended command line must be recognisable, or the daemon injects a second time"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_already_injected_command_line_is_recognised() {
        // The app hands its command line to the daemon, which detects a
        // PowerShell and would inject again. Two hooks emit every marker twice,
        // which the parser reads as an empty block between each real one.
        assert!(already_injected(r#"pwsh -NoExit -Command ". 'C:\x\pwsh\zesterm.ps1'""#));
        assert!(!already_injected("pwsh -NoLogo"));
        assert!(!already_injected("/bin/zsh"));

        // Windows paths are case-insensitive, and a command line written by
        // hand is where the other spellings come from. Matching case-sensitively
        // would hand a shell that already has the hook a second one.
        assert!(already_injected(r#"pwsh -NoExit -Command ". 'C:\X\PWSH\ZESTERM.PS1'""#));
        assert!(already_injected(r#"pwsh -NoExit -Command ". 'C:\x\pwsh\Zesterm.Ps1'""#));
    }

    #[test]
    fn the_powershell_hook_is_safe_to_load_twice() {
        // Someone with the `Invoke-Expression` line in their $PROFILE *and*
        // injection active loads it twice.
        assert!(hook(Shell::Pwsh).contains("Test-Path variable:global:__zesterm"));
    }

    #[test]
    fn the_powershell_hook_chains_the_users_prompt_rather_than_replacing_it() {
        // The property that makes injection safe to do without asking. A prompt
        // that silently loses oh-my-posh or starship to a terminal is a failure
        // nobody suspects the terminal for.
        let hook = hook(Shell::Pwsh);
        assert!(hook.contains("OriginalPrompt   = $function:prompt"), "the prompt is not saved");
        assert!(
            hook.contains("$Global:__zesterm.OriginalPrompt.Invoke()"),
            "the saved prompt is never called"
        );
    }

    #[test]
    fn the_powershell_hook_reports_the_cwd_before_it_opens_the_block() {
        // Ordering, not decoration: `133;A` opens a block and stamps it with the
        // working directory known at that moment. A `Cwd` emitted afterwards
        // lands on the *next* block, so every path shown is one command stale --
        // a wrong answer that looks like a plausible one.
        let hook = hook(Shell::Pwsh);
        let cwd = hook.find("633;P;Cwd=").expect("reports a working directory");
        let open = hook.find("'133;A'").expect("opens a block");
        assert!(cwd < open, "the working directory is reported after the block opens");
    }

    #[test]
    fn the_shim_hands_control_back_to_the_users_own_dotfiles() {
        // The property that makes injection safe to do without asking. If a
        // shim ever stops sourcing the user's file, their shell silently loses
        // its configuration and the cause is a terminal they did not suspect.
        for (shim, sourced) in [
            (ZSH_ZSHENV, ".zshenv"),
            (ZSH_ZPROFILE, ".zprofile"),
            (ZSH_ZSHRC, ".zshrc"),
            (ZSH_ZLOGIN, ".zlogin"),
        ] {
            assert!(
                shim.contains(&format!("source $ZDOTDIR/{sourced}")),
                "the {sourced} shim does not source the user's own {sourced}"
            );
        }
    }

    #[test]
    fn the_hook_loads_after_the_users_rc() {
        // `add-zsh-hook` appends, so loading before the user's rc puts our
        // precmd ahead of a prompt framework's -- and our PS1 wrapper is then
        // overwritten on every single prompt, producing no blocks at all.
        let rc = ZSH_ZSHRC;
        let user = rc.find("source $ZDOTDIR/.zshrc").expect("sources the user's rc");
        let ours = rc.find("source $ZESTERM_ZDOTDIR/zesterm.zsh").expect("loads the hook");
        assert!(ours > user, "the hook is loaded before the user's rc");
    }

    #[test]
    fn the_hook_is_safe_to_source_twice() {
        // Someone with `eval "$(zesterm --shell-integration zsh)"` in their rc
        // *and* injection active sources it twice. Without the guard every
        // marker is emitted twice, which the parser reads as an empty block
        // between each real one.
        assert!(hook(Shell::Zsh).contains("__zesterm_loaded"));
    }

    #[test]
    fn installing_writes_a_complete_shim_and_names_it() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let injection = install(Shell::Zsh, "/bin/zsh", &dir).expect("install");
        let zdotdir = dir.join("zsh");
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin", "zesterm.zsh"] {
            assert!(zdotdir.join(name).exists(), "{name} was not written");
        }

        assert!(
            injection.args.is_empty(),
            "zsh is hooked through the environment; amending its command line would be a bug"
        );
        let vars: std::collections::HashMap<_, _> = injection.env.into_iter().collect();
        assert_eq!(vars.get("ZDOTDIR").map(String::as_str), zdotdir.to_str());
        assert!(
            vars.contains_key("ZESTERM_USER_ZDOTDIR"),
            "without this the shim has nowhere to hand control back to"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
