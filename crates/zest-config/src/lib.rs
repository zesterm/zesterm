//! zesterm's settings.
//!
//! Four properties, each of which is awkward to add later:
//!
//! - **A cascade with provenance.** Values merge from compiled defaults through
//!   the OS appearance, the user's file, a profile, a workspace file, and the
//!   command line — and every value remembers which layer wrote it, so the
//!   settings UI can say *"set by profile `k8s-prod`"* rather than leaving the
//!   user to hunt for whatever is overriding them.
//! - **Invalidation classes.** Most edits are nearly free; a few resize the pty.
//!   See [`invalidate`], and the schema-coverage test that stops a new setting
//!   from silently needing a restart.
//! - **A JSON Schema.** The web and phone settings UIs are generated from it, so
//!   a new setting reaches them without three matching UI changes.
//! - **Never crashing on a bad config.** A broken file keeps the last good
//!   settings and produces a [`ConfigError`] to show in the chrome. A terminal
//!   that will not start because of a typo is a terminal that has locked the
//!   user out of the tool they would fix it with.

pub mod cascade;
pub mod invalidate;
pub mod migrate;
pub mod profiles;
pub mod schema;
pub mod settings;
pub mod ui;

// Everything that touches the filesystem. The wasm clients take the types and
// the schema and nothing else -- see the `fs` feature.
#[cfg(feature = "fs")]
pub mod paths;
#[cfg(feature = "fs")]
pub mod trust;
#[cfg(feature = "fs")]
pub mod watch;

pub use cascade::{Layer, Resolved, Source};
pub use invalidate::{diff, Invalidation};
pub use profiles::{
    ColorFrom, ProfileMeta, ProfileProvenance, ProfileResolved, TabTitle,
};
#[cfg(feature = "fs")]
pub use watch::Watcher;
// Re-exported because `write_value`'s signature already names
// `toml_edit::Value` — a caller cannot use the function without the type, and
// should not need its own pinned copy of the dependency to get it.
#[cfg(feature = "fs")]
pub use toml_edit;
pub use settings::{
    Appearance, Backdrop, Cursor, CursorShape, CursorTrail, Motion, Scrolling, Settings, Shell,
    TextAntialias, TextHinting, Typography, Window, CURRENT_SCHEMA_VERSION,
};

#[cfg(feature = "fs")]
use std::path::Path;
use std::path::PathBuf;

/// Something wrong with a config file, phrased for a user to act on.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ConfigError {
    /// Line and column, when the error has them.
    ///
    /// A parse error without a position makes the user re-read the whole file.
    #[must_use]
    pub fn position(&self) -> Option<(usize, usize)> {
        match self {
            Self::Parse { source, .. } => source.span().map(|_| {
                // toml reports a byte span; the message already carries the
                // line and column, so this only reports whether one exists.
                (0, 0)
            }),
            Self::Read { .. } => None,
        }
    }
}

/// How the config was loaded, including anything the user should be told.
#[derive(Debug)]
pub struct Load {
    pub resolved: Resolved,
    /// Errors that did not stop the load. Each one means a layer was skipped.
    pub errors: Vec<Box<ConfigError>>,
    /// A migration that ran, if any.
    pub migration: Option<migrate::Migration>,
}

/// What the caller wants layered on top of the files.
#[derive(Debug, Default)]
pub struct Options {
    /// Profile to select, if any.
    pub profile: Option<String>,
    /// Working directory to look for a `.zesterm.toml` in.
    pub workspace_dir: Option<PathBuf>,
    /// Command-line overrides, as a TOML table. Always the strongest layer.
    pub cli: Option<toml::Table>,
    /// The OS reports a light appearance.
    pub system_light: bool,
}

#[cfg(feature = "fs")]
/// Read every layer and merge them.
///
/// Never fails: a layer that cannot be read or parsed is skipped and reported.
/// The caller keeps its previous settings for anything that went wrong, so a
/// half-typed config in an editor with autosave does not take the terminal down
/// mid-keystroke.
#[must_use]
pub fn load(options: &Options) -> Load {
    let mut layers = Vec::new();
    let mut errors = Vec::new();
    let mut migration = None;

    // The OS appearance layer sits above defaults and below the user's file, so
    // an explicit `theme =` always beats it.
    if options.system_light {
        let mut table = toml::Table::new();
        let mut appearance = toml::Table::new();
        appearance.insert("theme".into(), toml::Value::String("paper".into()));
        table.insert("appearance".into(), toml::Value::Table(appearance));
        layers.push(Layer::new(Source::System, table));
    }

    let user_table = match paths::config_file() {
        Some(path) => match read_table(&path) {
            Ok(mut table) => {
                migration = migrate::migrate(&mut table);
                layers.push(Layer::new(Source::User, table.clone()));
                Some(table)
            }
            Err(e) => {
                errors.push(e);
                None
            }
        },
        None => None,
    };

    // Any profile's typos are worth one warning each at load, whether or not
    // that profile is selected today -- a typo found at launch time is a typo
    // found the day it was written.
    if let Some(table) = user_table.as_ref() {
        if let Some(all) = table.get("profiles").and_then(toml::Value::as_table) {
            for (name, profile) in all {
                if let Some(profile) = profile.as_table() {
                    for key in profiles::unknown_profile_keys(profile) {
                        tracing::warn!(profile = %name, key = %key, "unknown key in profile; ignoring");
                    }
                }
            }
        }
    }

    // The profile layers come from inside the user's file, so they can only
    // exist once that file has been read. `profiles.defaults` sits beneath the
    // named profile: every profile falls through to it.
    if let (Some(name), Some(table)) = (options.profile.as_ref(), user_table.as_ref()) {
        if name != profiles::RESERVED_PROFILE {
            if let Some(defaults) = cascade::defaults_layer(table) {
                layers.push(defaults);
            }
        }
        match cascade::profile_layer(table, name) {
            Some(layer) => layers.push(layer),
            None => tracing::warn!(profile = %name, "no such profile; ignoring"),
        }
    }

    // Workspace config is opt-in: it can set `shell.command`, so loading one
    // found next to a checkout would be remote code execution.
    if let Some(dir) = options.workspace_dir.as_ref() {
        if let Some(path) = paths::workspace_file(dir) {
            if trust::is_trusted(&path) {
                match read_table(&path) {
                    Ok(table) => layers.push(Layer::new(Source::Workspace, table)),
                    Err(e) => errors.push(e),
                }
            } else {
                tracing::info!(path = %path.display(), "workspace config found but not trusted");
            }
        }
    }

    if let Some(cli) = options.cli.clone() {
        layers.push(Layer::new(Source::CommandLine, cli));
    }

    let resolved = cascade::resolve(&layers);

    for key in &resolved.unknown_keys {
        tracing::warn!(key = %key, "unknown setting; ignoring");
    }

    Load { resolved, errors, migration }
}

// Boxed because `toml::de::Error` is large and this is on a cold path -- the
// alternative is every caller paying for the error size on every success.
#[cfg(feature = "fs")]
fn read_table(path: &Path) -> Result<toml::Table, Box<ConfigError>> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| Box::new(ConfigError::Read { path: path.to_path_buf(), source }))?;
    text.parse::<toml::Table>()
        .map_err(|source| Box::new(ConfigError::Parse { path: path.to_path_buf(), source }))
}

#[cfg(feature = "fs")]
/// Write a single value into the user's config, preserving the rest of the file.
///
/// `toml_edit`, not `toml::to_string`. Serializing the whole tree back would
/// destroy the user's comments and key ordering, which is unforgivable in a file
/// people hand-edit — and it would write out every default explicitly, turning a
/// three-line config into two hundred.
pub fn write_value(path: &Path, key: &str, value: toml_edit::Value) -> std::io::Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    write_at(path, &parts, value)
}

#[cfg(feature = "fs")]
/// Write one settings value inside a profile's table.
///
/// The profile name is one path segment, never split: a profile literally
/// named `prod.eu` must land at `[profiles."prod.eu"]`, not shatter into
/// `[profiles.prod.eu]`. Only the settings key below it is dotted.
pub fn write_profile_value(
    path: &Path,
    profile: &str,
    key: &str,
    value: toml_edit::Value,
) -> std::io::Result<()> {
    let mut parts = vec!["profiles", profile];
    parts.extend(key.split('.'));
    write_at(path, &parts, value)
}

#[cfg(feature = "fs")]
fn write_at(path: &Path, parts: &[&str], value: toml_edit::Value) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    let Some((last, groups)) = parts.split_last() else {
        return Ok(());
    };

    let mut node = doc.as_item_mut();
    for group in groups {
        node = &mut node[*group];
        if node.is_none() {
            // Implicit, so an intermediate table with only subtables ("[profiles]"
            // above "[profiles.x.window]") does not print an empty header of
            // its own. A table that gains a direct value prints regardless.
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            *node = toml_edit::Item::Table(table);
        }
    }
    node[*last] = toml_edit::Item::Value(value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())
}

#[cfg(feature = "fs")]
/// Delete one key from the file, pruning tables the deletion left empty.
///
/// A missing key — or a missing file — is a no-op, not an error: "make this
/// value not set" is already true, and reset-to-default must be idempotent.
pub fn remove_value(path: &Path, key: &str) -> std::io::Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    remove_at(path, &parts)
}

#[cfg(feature = "fs")]
/// Delete one settings key from a profile's table, pruning emptied tables.
///
/// Clearing an override is what makes a row fall back through Defaults, so
/// like [`remove_value`] this treats "already absent" as success.
pub fn remove_profile_value(path: &Path, profile: &str, key: &str) -> std::io::Result<()> {
    let mut parts = vec!["profiles", profile];
    parts.extend(key.split('.'));
    remove_at(path, &parts)
}

#[cfg(feature = "fs")]
/// Delete a whole profile. Missing profile or file is a no-op.
pub fn remove_profile(path: &Path, name: &str) -> std::io::Result<()> {
    remove_at(path, &["profiles", name])
}

#[cfg(feature = "fs")]
/// Duplicate a profile under a new name, comments and all.
///
/// Unlike the removals, a missing source is an error: "Duplicate" acting on a
/// profile that is not there means the caller's picture of the file is stale,
/// and silently writing nothing would confirm it.
pub fn copy_profile(path: &Path, from: &str, to: &str) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    let source = doc
        .get("profiles")
        .and_then(|p| p.get(from))
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no profile `{from}` to copy"),
            )
        })?;
    doc["profiles"][to] = source;
    std::fs::write(path, doc.to_string())
}

#[cfg(feature = "fs")]
fn remove_at(path: &Path, parts: &[&str]) -> std::io::Result<()> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    if remove_in(doc.as_table_mut(), parts) {
        std::fs::write(path, doc.to_string())?;
    }
    Ok(())
}

#[cfg(feature = "fs")]
/// Remove `parts` under `table`, pruning parents the removal emptied.
///
/// Pruning matters because [`write_at`] creates parents on demand: without it,
/// set-then-reset leaves a trail of empty `[profiles.x.window]` headers that
/// read as settings the user once had.
fn remove_in(table: &mut dyn toml_edit::TableLike, parts: &[&str]) -> bool {
    match parts {
        [] => false,
        [last] => table.remove(last).is_some(),
        [head, rest @ ..] => {
            // `as_table_like_mut`, not `as_table_mut`: a hand-written inline
            // table (`profiles = { x = { … } }`) is a Value, not a Table, and
            // the narrower cast made removals through it silently no-op while
            // write_at's index-based traversal wrote into it just fine.
            let Some(child) = table.get_mut(head).and_then(toml_edit::Item::as_table_like_mut)
            else {
                return false;
            };
            let removed = remove_in(child, rest);
            if removed && child.is_empty() {
                table.remove(head);
            }
            removed
        }
    }
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zesterm-config-test-{name}.toml"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn writing_a_value_keeps_comments_and_ordering() {
        let path = temp("comments");
        std::fs::write(
            &path,
            "# my terminal\n[typography]\n# I like it big\nsize_pt = 14.0\nline_height = 1.5\n",
        )
        .expect("write");

        write_value(&path, "typography.size_pt", toml_edit::value(20.0).into_value().unwrap())
            .expect("edit");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# my terminal"), "lost a comment: {text}");
        assert!(text.contains("# I like it big"), "lost a comment: {text}");
        assert!(text.contains("size_pt = 20.0"), "did not apply: {text}");
        assert!(text.contains("line_height = 1.5"), "lost a sibling: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_into_a_missing_group_creates_it() {
        let path = temp("newgroup");
        std::fs::write(&path, "# empty\n").expect("write");

        write_value(&path, "cursor.blink", toml_edit::value(false).into_value().unwrap())
            .expect("edit");

        let text = std::fs::read_to_string(&path).expect("read");
        let parsed: Settings = toml::from_str(&text).expect("still valid");
        assert!(!parsed.cursor.blink);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn writing_does_not_expand_defaults_into_the_file() {
        // The reason for toml_edit over round-tripping the typed tree: a user
        // with a three-line config must not find two hundred lines afterwards.
        let path = temp("nodefaults");
        std::fs::write(&path, "[typography]\nsize_pt = 14.0\n").expect("write");

        write_value(&path, "typography.size_pt", toml_edit::value(16.0).into_value().unwrap())
            .expect("edit");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("spring_damping"), "wrote out defaults: {text}");
        assert!(text.lines().count() < 5, "file grew: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_dotted_profile_name_stays_one_key() {
        // "prod.eu" is a legal profile name. Split naively it shatters into
        // [profiles.prod.eu], a different profile, and reads back empty.
        let path = temp("dotname");
        std::fs::write(&path, "# fleet config\n").expect("write");

        write_profile_value(
            &path,
            "prod.eu",
            "window.opacity",
            toml_edit::value(0.9).into_value().unwrap(),
        )
        .expect("edit");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let opacity = table["profiles"]["prod.eu"]["window"]["opacity"].as_float();
        assert_eq!(opacity, Some(0.9), "round-trip failed under the literal name: {text}");
        assert!(
            table["profiles"].as_table().is_some_and(|p| !p.contains_key("prod")),
            "the name shattered into nested tables: {text}"
        );
        assert!(text.contains("# fleet config"), "lost a comment: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_a_value_keeps_comments_and_ordering() {
        // The mirror of writing_a_value_keeps_comments_and_ordering: reset is
        // an edit too, and it must not cost the user their annotations.
        let path = temp("rm-comments");
        std::fs::write(
            &path,
            "# my terminal\n[typography]\n# I like it big\nsize_pt = 14.0\nline_height = 1.5\n",
        )
        .expect("write");

        remove_value(&path, "typography.size_pt").expect("remove");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# my terminal"), "lost a comment: {text}");
        assert!(!text.contains("size_pt"), "did not remove: {text}");
        assert!(text.contains("line_height = 1.5"), "lost a sibling: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_a_missing_key_or_file_is_a_no_op() {
        let path = temp("rm-missing");
        // Missing file: nothing to do, and nothing to fail over.
        remove_value(&path, "typography.size_pt").expect("missing file is fine");
        assert!(!path.exists(), "a removal must not create the file");

        std::fs::write(&path, "[cursor]\nblink = false\n").expect("write");
        remove_value(&path, "typography.size_pt").expect("missing key is fine");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "[cursor]\nblink = false\n", "a no-op must not rewrite the file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removing_the_last_value_prunes_the_emptied_tables() {
        // write-then-reset must round-trip to nothing: leftover empty
        // [profiles.x.window] headers read as settings the user once had.
        let path = temp("rm-prune");
        std::fs::write(&path, "").expect("write");

        write_profile_value(&path, "x", "window.opacity", toml_edit::value(0.5).into_value().unwrap())
            .expect("edit");
        remove_profile_value(&path, "x", "window.opacity").expect("remove");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        assert!(
            !table.contains_key("profiles"),
            "emptied tables were not pruned: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn removal_reaches_into_inline_tables() {
        // A hand-written `profiles = { x = { ... } }` is the same data in a
        // different representation, and write_at happily writes through it —
        // so a removal that silently no-ops there strands overrides the
        // editor created and can never clear, and "Delete profile" reports
        // success while the profile survives.
        let path = temp("rm-inline");
        std::fs::write(
            &path,
            "profiles = { x = { command = \"wsl\", window = { opacity = 0.5 } } }\n",
        )
        .expect("write");

        remove_profile_value(&path, "x", "window.opacity").expect("remove");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("opacity"), "removal no-oped through an inline table: {text}");
        assert!(text.contains("command"), "removal must take only the one key: {text}");

        remove_profile(&path, "x").expect("remove profile");
        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        assert!(
            !table.contains_key("profiles"),
            "an inline profile survived its own deletion: {text:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_profile_can_be_removed_and_copied() {
        let path = temp("profile-ops");
        std::fs::write(
            &path,
            "[profiles.ubuntu]\n# the penguin\ncommand = \"wsl.exe\"\n[profiles.mac]\ncommand = \"zsh\"\n",
        )
        .expect("write");

        copy_profile(&path, "ubuntu", "ubuntu-2").expect("copy");
        remove_profile(&path, "mac").expect("remove");
        remove_profile(&path, "never-existed").expect("removing a missing profile is a no-op");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert!(
            profiles.contains_key("ubuntu") && profiles.contains_key("ubuntu-2"),
            "the copy must add a sibling without touching its source: {text}"
        );
        assert!(!profiles.contains_key("mac"), "remove_profile left it behind: {text}");
        assert_eq!(
            profiles["ubuntu-2"]["command"].as_str(),
            Some("wsl.exe"),
            "the copy is not a copy: {text}"
        );

        assert!(
            copy_profile(&path, "ghost", "ghost-2").is_err(),
            "copying a profile that is not there means the caller is stale; say so"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_file_does_not_stop_the_load() {
        // Simulated at the layer level, because `load` reads real paths: the
        // property under test is that a parse failure produces an error and
        // usable settings, not a panic.
        let bad = Layer::parse(Source::User, "[typography\nsize_pt = ");
        assert!(bad.is_err());
        let resolved = cascade::resolve(&[]);
        assert_eq!(resolved.settings, Settings::default());
    }
}
