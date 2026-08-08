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
//! # zsh only, for now
//!
//! Not an oversight. bash, fish and PowerShell each have a documented injection
//! mechanism, and none of the three can be *seen working* on the machine this
//! was written on: there is no fish and no pwsh here, and `/bin/bash` is
//! Apple's patched 3.2.57, where the `ENV`-based startup path the technique
//! depends on is disabled — which is why Ghostty excludes `/bin/bash` on Darwin
//! outright rather than shipping something that silently does nothing.

use std::io;
use std::path::{Path, PathBuf};

/// A shell zesterm knows how to hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Zsh,
}

impl Shell {
    /// Recognise the shell from a command line, if it is one we can hook.
    ///
    /// Matches the *executable* only. A trailing argument that happens to
    /// contain `zsh` — `vim zsh-notes.md` — is not a zsh, and neither is a
    /// login shell spelled `-zsh`, which is why the leading `-` is stripped.
    #[must_use]
    pub fn detect(command_line: &str) -> Option<Self> {
        let exe = command_line.split_whitespace().next()?;
        let exe = exe.trim_matches('"');
        let name = Path::new(exe).file_name()?.to_str()?;
        match name.strip_prefix('-').unwrap_or(name) {
            "zsh" => Some(Self::Zsh),
            _ => None,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
        }
    }
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
pub fn install(shell: Shell, dir: &Path) -> io::Result<Vec<(String, String)>> {
    match shell {
        Shell::Zsh => install_zsh(dir),
    }
}

fn install_zsh(dir: &Path) -> io::Result<Vec<(String, String)>> {
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

    Ok(vec![
        ("ZDOTDIR".to_string(), zdotdir.to_string()),
        ("ZESTERM_USER_ZDOTDIR".to_string(), user_zdotdir),
    ])
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

        let env = install(Shell::Zsh, &dir).expect("install");
        let zdotdir = dir.join("zsh");
        for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin", "zesterm.zsh"] {
            assert!(zdotdir.join(name).exists(), "{name} was not written");
        }

        let vars: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(vars.get("ZDOTDIR").map(String::as_str), zdotdir.to_str());
        assert!(
            vars.contains_key("ZESTERM_USER_ZDOTDIR"),
            "without this the shim has nowhere to hand control back to"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
