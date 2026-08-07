//! Whether a directory-local config may be loaded.
//!
//! `.zesterm.toml` can set `shell.command`. Auto-loading one found beside a
//! checkout is therefore remote code execution: clone a repository, open a
//! terminal in it, and the repository chooses what runs. VS Code learned this the
//! hard way and now prompts per folder; there is no reason to learn it again.
//!
//! The decision is remembered per directory *and* fingerprinted, so editing a
//! trusted file re-prompts. Trusting a file once must not be trusting whatever it
//! becomes after the next `git pull`.

use std::collections::BTreeMap;
use std::path::Path;

/// A cheap content fingerprint.
///
/// FNV-1a: not cryptographic, and it does not need to be. This detects *change*,
/// not forgery — an attacker who can rewrite the file can also rewrite the trust
/// list next to it, so a stronger hash would buy nothing.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn load_list() -> BTreeMap<String, String> {
    let Some(path) = crate::paths::trust_file() else {
        return BTreeMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    text.parse::<toml::Table>()
        .map(|t| {
            t.into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this exact file content has been trusted.
#[must_use]
pub fn is_trusted(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let key = path.to_string_lossy().to_string();
    load_list().get(&key).is_some_and(|fp| fp == &format!("{:016x}", fingerprint(&bytes)))
}

/// Record that the user trusted this file, as it is right now.
pub fn trust(path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let mut list = load_list();
    list.insert(path.to_string_lossy().to_string(), format!("{:016x}", fingerprint(&bytes)));

    let Some(out) = crate::paths::trust_file() else {
        return Ok(());
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut table = toml::Table::new();
    for (k, v) in list {
        table.insert(k, toml::Value::String(v));
    }
    std::fs::write(out, toml::to_string(&table).unwrap_or_default())
}

/// Withdraw trust from a file.
pub fn revoke(path: &Path) -> std::io::Result<()> {
    let mut list = load_list();
    list.remove(&path.to_string_lossy().to_string());

    let Some(out) = crate::paths::trust_file() else {
        return Ok(());
    };
    let mut table = toml::Table::new();
    for (k, v) in list {
        table.insert(k, toml::Value::String(v));
    }
    std::fs::write(out, toml::to_string(&table).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untrusted_file_is_not_trusted() {
        let path = std::env::temp_dir().join("zesterm-untrusted-.zesterm.toml");
        std::fs::write(&path, "[shell]\ncommand = \"evil\"\n").expect("write");
        assert!(!is_trusted(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_not_trusted() {
        assert!(!is_trusted(Path::new("C:/no/such/.zesterm.toml")));
    }

    #[test]
    fn editing_a_trusted_file_withdraws_trust() {
        // The property that matters: trusting a file is not trusting whatever
        // the next `git pull` turns it into.
        let path = std::env::temp_dir().join("zesterm-trust-edit-.zesterm.toml");
        std::fs::write(&path, "[shell]\ncommand = \"pwsh\"\n").expect("write");

        trust(&path).expect("trust");
        assert!(is_trusted(&path));

        std::fs::write(&path, "[shell]\ncommand = \"rm -rf /\"\n").expect("rewrite");
        assert!(!is_trusted(&path), "an edited file stayed trusted");

        let _ = revoke(&path);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_fingerprint_distinguishes_content() {
        assert_ne!(fingerprint(b"a"), fingerprint(b"b"));
        assert_eq!(fingerprint(b"same"), fingerprint(b"same"));
    }
}
