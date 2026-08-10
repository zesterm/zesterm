//! Getting the OSC 133 hook into the shell, without touching the user's files.
//!
//! Command blocks need the shell to say where a command starts and ends. The
//! shell will not do that on its own, so something has to load a hook into it.
//!
//! # Injection, not installation
//!
//! zesterm sets environment variables at spawn so the shell loads the hook
//! itself. **No file of the user's is read for modification, written, or
//! appended to.** This is what kitty, Ghostty and VS Code all default to, and
//! kitty states the property directly: *"No files are added or modified."*
//! Appending `eval "$(...)"` to `~/.zshrc` — iTerm2's older model — is the
//! alternative, and it is the one that needs a consent prompt, a fingerprint of
//! the file, and a story for what happens when the user edits it afterwards.
//!
//! # Why it can only reach the first shell
//!
//! Environment manipulation applies to the process zesterm starts. A shell
//! started *inside* that one — `ssh`, `tmux`, `nix-shell`, a container, a bare
//! `zsh` — is not touched, and no amount of cleverness changes that. The escape
//! hatch is [`hook`], which prints the same script for a person to `eval` by
//! hand on the far side.
//!
//! # zsh and PowerShell; not bash or fish
//!
//! Not an oversight either way. The rule is that a shell is not hooked until it
//! can be *seen working* on a machine someone has. zsh was written on the Mac
//! and PowerShell on the Windows box, where it is the default shell and where
//! command blocks were inert until it existed.
//!
//! `/bin/bash` on that Mac is Apple's patched 3.2.57, where the `ENV`-based
//! startup path the technique depends on is disabled -- which is why Ghostty
//! excludes `/bin/bash` on Darwin outright rather than shipping something that
//! silently does nothing. There is still no fish on either machine.
//!
//! `cmd.exe` is refused on purpose rather than for want of a machine: `PROMPT`
//! can carry the `A` and `B` markers, but cmd has no preexec hook, so `C` and
//! `D` are unreachable and every block would begin and never end. Blocks that
//! never finish render worse than no blocks at all.

use std::io;
use std::path::{Path, PathBuf};

/// A shell zesterm knows how to hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    /// PowerShell — `pwsh` 7 and Windows PowerShell 5.1 alike. One script and
    /// one injection covers both; where they differ is noted in the script.
    Pwsh,
}

impl Shell {
    /// Recognise the shell from a command line, if it is one we can hook.
    ///
    /// Matches the *executable* only. A trailing argument that happens to
    /// contain `zsh` — `vim zsh-notes.md` — is not a zsh, and neither is a
    /// login shell spelled `-zsh`, which is why the leading `-` is stripped.
    #[must_use]
    pub fn detect(command_line: &str) -> Option<Self> {
        let exe = first_word(command_line)?;
        let exe = exe.trim_matches('"');
        let name = Path::new(exe).file_name()?.to_str()?;
        let name = name.strip_prefix('-').unwrap_or(name);
        // `.exe` is stripped case-insensitively, because the default shell is
        // built as a quoted absolute path and Windows does not care about case.
        let stem = name
            .rfind('.')
            .filter(|_| name.to_ascii_lowercase().ends_with(".exe"))
            .map_or(name, |at| &name[..at]);
        match stem.to_ascii_lowercase().as_str() {
            "zsh" => Some(Self::Zsh),
            "pwsh" | "powershell" => Some(Self::Pwsh),
            _ => None,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Pwsh => "pwsh",
        }
    }
}

/// The first word of a command line, honouring one level of quoting.
///
/// `CommandSpec::default_shell` produces `"C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo`
/// on Windows, and splitting that on whitespace yields `"C:\Program`, whose
/// file name is not a shell anyone has heard of. This is why PowerShell was not
/// detected even once the variant existed.
fn first_word(command_line: &str) -> Option<&str> {
    let s = command_line.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        return rest.find('"').map(|end| &rest[..end]);
    }
    s.split_whitespace().next()
}

/// How a shell is made to load the hook.
///
/// Two mechanisms, because the shells offer two. zsh is reached by environment
/// alone — `ZDOTDIR` points at a shim that sources the user's own files — and
/// nothing about its command line changes. PowerShell has no such variable:
/// `$PROFILE` resolves to four fixed paths that no environment redirects, and
/// `PSModulePath` affects module *resolution*, never startup. Its only seam is
/// the command line.
///
/// The property that made injection safe to do without asking survives either
/// way: **no file of the user's is read for modification, written, or appended
/// to.**
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Injection {
    /// Environment entries layered over the parent's.
    pub env: Vec<(String, String)>,
    /// Appended verbatim to the command line, when a shell needs that instead.
    pub args: Option<String>,
}

/// The hook script, for a person to `eval` by hand.
///
/// This is the documented path for everything injection cannot reach — ssh,
/// tmux, subshells — and it is the *same* script the shim loads, so the two
/// cannot drift.
///
/// ```text
/// # ~/.zshrc, on a machine you ssh into
/// eval "$(zesterm --shell-integration zsh)"
/// ```
#[must_use]
pub fn hook(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("../shell-integration/zsh/zesterm.zsh"),
        Shell::Pwsh => include_str!("../shell-integration/pwsh/zesterm.ps1"),
    }
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
pub fn install(shell: Shell, dir: &Path) -> io::Result<Injection> {
    match shell {
        Shell::Zsh => install_zsh(dir),
        Shell::Pwsh => install_pwsh(dir),
    }
}

fn install_pwsh(dir: &Path) -> io::Result<Injection> {
    let pwshdir = dir.join("pwsh");
    std::fs::create_dir_all(&pwshdir)?;
    let script = pwshdir.join("zesterm.ps1");
    std::fs::write(&script, hook(Shell::Pwsh))?;

    let Some(script) = script.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shell integration directory is not valid UTF-8",
        ));
    };

    // `-NoExit -Command . '<path>'`, which is what VS Code does and the only
    // seam PowerShell has.
    //
    // Wrapped in `try {} catch {}` for one reason: dot-sourcing a `.ps1` obeys
    // execution policy. The default here and on most machines is
    // `RemoteSigned`, which permits a file we wrote ourselves (it carries no
    // Mark-of-the-Web) — but a machine under an `AllSigned` or `Restricted`
    // group policy would greet every single shell with a red error. Swallowing
    // it means such a user gets a working shell with no blocks, which is what
    // they had before.
    //
    // `-ExecutionPolicy Bypass` was the obvious alternative and is worse twice
    // over: it is *ignored* when the policy comes from machine or user policy
    // scope, which is the only case that matters, and a terminal that launches
    // shells with it on the command line looks exactly like something an EDR
    // should quarantine.
    //
    // Single quotes around the path, doubled inside: PowerShell's literal
    // string, so a `$` in a user's profile path cannot expand.
    let escaped = script.replace('\'', "''");
    Ok(Injection {
        env: Vec::new(),
        args: Some(format!("-NoExit -Command \"try {{ . '{escaped}' }} catch {{ }}\"")),
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
        args: None,
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

        let injection = install(Shell::Zsh, &dir).expect("install");
        let zdotdir = dir.join("zsh");
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin", "zesterm.zsh"] {
            assert!(zdotdir.join(name).exists(), "{name} was not written");
        }

        let vars: std::collections::HashMap<_, _> = injection.env.into_iter().collect();
        assert_eq!(vars.get("ZDOTDIR").map(String::as_str), zdotdir.to_str());
        assert!(
            vars.contains_key("ZESTERM_USER_ZDOTDIR"),
            "without this the shim has nowhere to hand control back to"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn powershell_is_recognised_however_it_is_spelled() {
        // The quoted absolute path is not a corner case — it is exactly what
        // `CommandSpec::default_shell` builds on Windows, and splitting it on
        // whitespace yields `"C:\Program`, which is why detection has to
        // understand one level of quoting to work at all.
        assert_eq!(
            Shell::detect(r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#),
            Some(Shell::Pwsh),
            "the default Windows shell must be detected, or blocks never work there"
        );
        assert_eq!(Shell::detect("pwsh"), Some(Shell::Pwsh));
        assert_eq!(Shell::detect("pwsh.exe -NoLogo"), Some(Shell::Pwsh));
        assert_eq!(Shell::detect(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"), Some(Shell::Pwsh));
        assert_eq!(Shell::detect("POWERSHELL.EXE"), Some(Shell::Pwsh), "Windows does not care about case");

        // Still the executable only, exactly as for zsh.
        assert_eq!(Shell::detect("vim pwsh-notes.md"), None);
        assert_eq!(Shell::detect("cmd.exe"), None, "cmd has no preexec, so it gets no half-working blocks");
    }

    #[test]
    fn the_powershell_hook_rides_the_command_line_and_writes_no_env() {
        let dir = std::env::temp_dir().join(format!("zesterm-si-pwsh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let injection = install(Shell::Pwsh, &dir).expect("install");
        assert!(dir.join("pwsh").join("zesterm.ps1").exists(), "the hook was not written");
        assert!(
            injection.env.is_empty(),
            "PowerShell has no environment seam; anything here is a mistaken port of the zsh path"
        );
        let args = injection.args.expect("the hook must reach the shell somehow");
        assert!(args.contains("-NoExit"), "without -NoExit the shell runs the dot-source and leaves");
        assert!(
            args.contains("try {") && args.contains("catch {"),
            "an execution policy that refuses the script must cost blocks, not every launch"
        );
        assert!(
            args.contains(dir.join("pwsh").join("zesterm.ps1").to_str().expect("utf-8")),
            "the injected line must name the script actually written: {args}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The `A`/`B`/`C`/`D` set is the whole contract with `zest-core`'s parser;
    /// a hook missing one of them produces blocks that never start or never
    /// end, which renders worse than no blocks at all.
    #[test]
    fn both_hooks_emit_every_osc_133_marker() {
        for shell in [Shell::Zsh, Shell::Pwsh] {
            let script = hook(shell);
            for marker in ["133;A", "133;B", "133;C", "133;D"] {
                assert!(
                    script.contains(marker),
                    "the {} hook never emits OSC {marker}",
                    shell.name()
                );
            }
        }
    }

    #[test]
    fn the_powershell_hook_is_safe_to_load_twice() {
        // Same property as the zsh guard and for the same reason: doubled hooks
        // emit every marker twice, which the parser reads as an empty block
        // between each real one.
        let script = hook(Shell::Pwsh);
        assert!(
            script.contains("__zesterm_loaded"),
            "the pwsh hook has no double-load guard"
        );
    }

    #[test]
    fn the_pwsh_hook_reads_the_exit_status_before_anything_can_clobber_it() {
        // `$?` is the success of the *last statement*, and reading it is itself
        // a statement. The zsh hook has the identical hazard with `local ret=$?`
        // and the identical answer: capture first, do everything else after.
        let script = hook(Shell::Pwsh);
        let body = script.split("function global:prompt").nth(1).expect("the prompt wrapper");
        let ok_at = body.find("$ok = $?").expect("the status must be captured");
        let history_at = body.find("Get-History").expect("the history probe");
        assert!(
            ok_at < history_at,
            "anything before `$ok = $?` clobbers the status the block is about to report"
        );
    }
}
