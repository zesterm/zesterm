//! Where the config lives.
//!
//! Portable mode is checked first and deliberately: a `zesterm.toml` sitting
//! beside the executable means someone put it there, most likely on a USB stick
//! or in a locked-down environment where `%APPDATA%` is not theirs. Preferring
//! the roaming profile in that case would silently ignore the only config they
//! could write.

use std::path::{Path, PathBuf};

/// Config file name in the user's config directory.
pub const CONFIG_FILE: &str = "config.toml";

/// Config file name for portable mode, beside the executable.
pub const PORTABLE_FILE: &str = "zesterm.toml";

/// Config file name for a directory-local override.
pub const WORKSPACE_FILE: &str = ".zesterm.toml";

/// The directory holding config, themes, and the trust list.
///
/// Portable mode wins when a `zesterm.toml` sits next to the binary.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = portable_dir() {
        return Some(dir);
    }
    directories::ProjectDirs::from("dev", "zesterm", "zesterm")
        .map(|d| d.config_dir().to_path_buf())
}

/// The executable's directory, when it holds a portable config.
#[must_use]
pub fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join(PORTABLE_FILE).is_file().then(|| dir.to_path_buf())
}

/// The config file to read, if one exists.
#[must_use]
pub fn config_file() -> Option<PathBuf> {
    if let Some(dir) = portable_dir() {
        return Some(dir.join(PORTABLE_FILE));
    }
    let path = config_dir()?.join(CONFIG_FILE);
    path.is_file().then_some(path)
}

/// Where an edit lands: the config file to read if there is one, else the path
/// a first write should create.
///
/// The `config_file()`-first ordering is the portable-mode rule, and it is why
/// this is a function rather than a `config_dir().join(CONFIG_FILE)` at each
/// call site: portable mode's file is `zesterm.toml` beside the binary, not
/// `config.toml` under it, so a caller that composes the fallback itself writes
/// to a file the loader will never read — on the one setup where that file is
/// the only one the person can write.
#[must_use]
pub fn config_write_target() -> Option<PathBuf> {
    config_file().or_else(|| config_dir().map(|d| d.join(CONFIG_FILE)))
}

/// Where user themes are read from.
#[must_use]
pub fn themes_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("themes"))
}

/// The trust list for workspace configs.
#[must_use]
pub fn trust_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("trusted.toml"))
}

/// Where mutable app state lives — the remembered tab set, and whatever
/// joins it.
///
/// **Not the config directory**, for two live reasons: state is not settings
/// (nothing here belongs in a generated settings UI or a dotfiles repo), and
/// the config watcher fires on writes to its directory — an app that saves
/// state on every tab change into a watched directory reloads its own config
/// in a loop. Portable mode keeps state beside the binary, matching where
/// its config lives.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = portable_dir() {
        return Some(dir.join("state"));
    }
    directories::ProjectDirs::from("dev", "zesterm", "zesterm")
        .map(|d| d.data_local_dir().join("state"))
}

/// A `.zesterm.toml` in `dir`, if there is one.
#[must_use]
pub fn workspace_file(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(WORKSPACE_FILE);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_dir_is_always_available() {
        // Every supported platform has one; if this ever returns None the
        // caller silently runs on defaults forever.
        assert!(config_dir().is_some());
    }

    #[test]
    fn a_write_target_exists_even_before_the_file_does() {
        // The point of the fallback: a machine that has never been configured
        // must still have somewhere for a first write to go, or every settings
        // edit on a fresh install silently does nothing.
        let target = config_write_target().expect("a config dir is always available");
        assert!(
            target.is_absolute(),
            "a write target must be absolute; a relative one resolves against \
             whatever directory the process happens to be in: {}",
            target.display()
        );
    }

    #[test]
    fn a_write_target_agrees_with_what_the_loader_reads() {
        // The two must not be allowed to drift: a target that is not the file
        // `config_file` returns is a write nothing ever loads.
        if let Some(existing) = config_file() {
            assert_eq!(
                config_write_target(),
                Some(existing),
                "the write target left the file the loader actually reads"
            );
        }
    }

    #[test]
    fn a_missing_workspace_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("zesterm-no-such-dir-12345");
        assert!(workspace_file(&dir).is_none());
    }
}
