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
    fn a_missing_workspace_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("zesterm-no-such-dir-12345");
        assert!(workspace_file(&dir).is_none());
    }
}
