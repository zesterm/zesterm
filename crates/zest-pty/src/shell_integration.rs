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
//! zsh and PowerShell. The absences are not an oversight:
//!
//! - **bash and fish** each have a documented mechanism, and neither can be
//!   *seen working* on the machines this has been written on. `/bin/bash` on
//!   macOS is Apple's patched 3.2.57, where the `ENV`-based startup path the
//!   technique depends on is disabled — which is why Ghostty excludes
//!   `/bin/bash` on Darwin outright rather than shipping something that silently
//!   does nothing. Writing them blind is how the attach path nearly shipped
//!   compiled and unseen.
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
}

impl Shell {
    /// Recognise the shell from a command line, if it is one we can hook.
    ///
    /// Matches the *executable* only. A trailing argument that happens to
    /// contain `zsh` — `vim zsh-notes.md` — is not a zsh, and neither is a
    /// login shell spelled `-zsh`, which is why the leading `-` is stripped.
    ///
    /// The first token is parsed quote-aware rather than split on whitespace:
    /// Windows' default shell lives at `"C:\Program Files\PowerShell\7\pwsh.exe"`,
    /// and splitting on whitespace makes its executable `C:\Program` — a name
    /// that matches nothing, so *every* Windows shell silently went unhooked
    /// (#83). It is also the seam a launcher-aware pass hangs off later:
    /// `wsl.exe -d Ubuntu -- bash` needs the shell named *after* the first
    /// token, which a one-line match has nowhere to grow into.
    #[must_use]
    pub fn detect(command_line: &str) -> Option<Self> {
        let name = Path::new(first_token(command_line)?).file_name()?.to_str()?;
        let name = name.strip_prefix('-').unwrap_or(name);
        // zsh is matched exactly and the PowerShells case-insensitively, which
        // is not inconsistency but the two filesystems: a `ZSH` next to a `zsh`
        // on unix is a different file, and `PWSH.EXE` on Windows is not.
        if name == "zsh" {
            return Some(Self::Zsh);
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
        }
    }
}

/// The executable a command line runs, with a quoted path kept whole.
///
/// Not a shell-grade parser — no escapes, no single quotes, because neither
/// `CreateProcessW` nor [`crate::CommandSpec`] promises them for the program
/// name. It only has to keep `"C:\Program Files\..."` in one piece.
fn first_token(command_line: &str) -> Option<&str> {
    let line = command_line.trim_start();
    if let Some(rest) = line.strip_prefix('"') {
        // An unterminated quote takes the rest of the line, which is what
        // `CreateProcessW` does with it too.
        return Some(rest.split('"').next().unwrap_or(rest)).filter(|s| !s.is_empty());
    }
    line.split_whitespace().next()
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
    // Both sides lowered, rather than relying on `SHIM_PWSH` happening to be
    // lowercase already -- renaming the file should not silently disarm this.
    command_line.to_ascii_lowercase().contains(&SHIM_PWSH.to_ascii_lowercase())
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
        assert_eq!(Shell::detect("/bin/bash"), None);
        assert_eq!(Shell::detect(""), None);
    }

    #[test]
    fn a_quoted_executable_path_survives_the_space_in_program_files() {
        // The bug behind #83, and the reason a `Pwsh` variant alone would have
        // changed nothing: Windows' own default shell is quoted because its
        // path has a space, and splitting the line on whitespace made the
        // executable `C:\Program`. Every Windows shell went unhooked, and the
        // only trace was a status bar reading `0 blocks`.
        assert_eq!(
            Shell::detect(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#),
            Some(Shell::Pwsh)
        );
        assert_eq!(Shell::detect(r#""C:\Program Files\Git\bin\zsh.exe""#), None);
    }

    #[test]
    fn every_spelling_of_powershell_is_recognised() {
        // One variant, four executables. Windows paths are case-insensitive, so
        // the case a launcher happens to use is not a signal.
        for line in [
            "pwsh",
            "pwsh.exe",
            "PWSH.EXE",
            "powershell",
            "powershell.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo",
            r"C:\Users\andy\scoop\shims\pwsh.exe -NoLogo",
            "/usr/local/bin/pwsh -Login",
        ] {
            assert_eq!(Shell::detect(line), Some(Shell::Pwsh), "{line} is a PowerShell");
        }

        // The argument names a PowerShell; the program is Notepad.
        assert_eq!(Shell::detect("notepad pwsh.ps1"), None);
        assert_eq!(Shell::detect("powershell-notes"), None);
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
