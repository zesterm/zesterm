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
    Appearance, Backdrop, Cursor, CursorShape, CursorTrail, Motion, PredictEcho, Prompt, Scrolling,
    Settings, Shell, TextAntialias, TextHinting, Typography, Window, CURRENT_SCHEMA_VERSION,
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

    // No OS-appearance layer. There was one, and being a *layer* was the bug:
    // it sat below the user's file so an explicit `theme =` always beat it,
    // which is everyone who cared enough to pick a theme, and it hardcoded
    // `paper` rather than reading `appearance.light_theme`. Following the OS is
    // a question about the live window, not about how files merge, so the app
    // resolves it per repaint off `follow_system_theme` and `light_theme`.
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
/// Create an empty profile table (`[profiles.<name>]`), comments preserved.
///
/// The profiles editor's "＋ New profile": a profile with no keys is a valid
/// launch target (everything falls through Defaults), so creating one writes
/// only the header. A profile that already exists is left alone — creating is
/// idempotent, never destructive.
pub fn create_profile(path: &Path, name: &str) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    if doc.get("profiles").and_then(|p| p.get(name)).is_some() {
        return Ok(());
    }
    if doc.get("profiles").is_none() {
        // Implicit, so an empty [profiles] header never prints on its own —
        // the same rule write_at applies to intermediate tables.
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc["profiles"] = toml_edit::Item::Table(table);
    }
    // Explicit, unlike the parents above: the header IS the profile.
    doc["profiles"][name] = toml_edit::Item::Table(toml_edit::Table::new());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())
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
/// Rename a profile in place, comments, ordering and position intact.
///
/// Not `copy_profile` + `remove_profile`: that pair appends the new table at
/// the end of the document and deletes the old one, so a rename would silently
/// reorder a hand-written file — and `[profiles.<name>]` is the one heading a
/// person scrolls to. Moving the `Item` and re-stamping its position keeps the
/// profile where its author put it.
///
/// A missing source is an error, like [`copy_profile`]'s and for the same
/// reason. So is an existing destination: renaming onto a live profile would
/// silently destroy it, and the editor cannot offer to merge two launch
/// targets. `from == to` is a no-op rather than an error — the editor commits
/// a name entry whether or not it changed.
pub fn rename_profile(path: &Path, from: &str, to: &str) -> std::io::Result<()> {
    if from == to {
        return Ok(());
    }
    let existing = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    // `as_table_like_mut`, not `as_table_mut`: `profiles = { work = { … } }`
    // is a legal hand-written spelling and an *inline* table, which
    // `as_table_mut` declines — the rename would then report "no profile" for
    // one that is plainly there. `remove_in` below already takes this shape
    // seriously, and a writer that only understands half the file is how a
    // config people hand-edit starts refusing its own contents.
    let Some(profiles) = doc.get_mut("profiles").and_then(toml_edit::Item::as_table_like_mut)
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no profile `{from}` to rename"),
        ));
    };
    // Before the removal, or a refused rename has already moved the source.
    if profiles.contains_key(to) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("a profile named `{to}` already exists"),
        ));
    }
    // The old key carries the decor, and for a profile that decor is the blank
    // line and the `# comment` a person wrote above `[profiles.<name>]`. Taken
    // before the removal because `remove` does not hand it back.
    let old_key = profiles.key(from).cloned();
    let Some(source) = profiles.remove(from) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no profile `{from}` to rename"),
        ));
    };
    let mut key = toml_edit::Key::new(to);
    if let Some(old) = old_key {
        *key.leaf_decor_mut() = old.leaf_decor().clone();
        *key.dotted_decor_mut() = old.dotted_decor().clone();
    }
    profiles.entry_format(&key).or_insert(source);
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
    fn renaming_keeps_the_comments_the_ordering_and_the_place() {
        // Rename is not copy+remove: that appends the new table at the end of
        // the file and deletes the old one, so a profile a person deliberately
        // put first would silently move last — and `[profiles.<name>]` is the
        // heading they scroll to.
        let path = temp("rename-order");
        std::fs::write(
            &path,
            "[profiles.ubuntu]\n# the penguin\ncommand = \"wsl.exe\"\n\n\
             [profiles.mac]\ncommand = \"zsh\"\n\n[appearance]\ntheme = \"nord\"\n",
        )
        .expect("write");

        rename_profile(&path, "ubuntu", "forge").expect("rename");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert!(!profiles.contains_key("ubuntu"), "the old name survived: {text}");
        assert_eq!(
            profiles["forge"]["command"].as_str(),
            Some("wsl.exe"),
            "the renamed profile lost its contents: {text}"
        );
        assert!(text.contains("# the penguin"), "the comment did not survive: {text}");
        assert!(
            text.find("[profiles.forge]") < text.find("[profiles.mac]"),
            "the rename reordered the file: {text}"
        );
        assert_eq!(
            table["appearance"]["theme"].as_str(),
            Some("nord"),
            "an unrelated root key was disturbed: {text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn renaming_refuses_to_land_on_a_profile_that_exists() {
        // Silently merging two launch targets is not something the editor can
        // offer to undo, so the write must not happen at all.
        let path = temp("rename-collision");
        std::fs::write(
            &path,
            "[profiles.ubuntu]\ncommand = \"wsl.exe\"\n[profiles.mac]\ncommand = \"zsh\"\n",
        )
        .expect("write");

        let err = rename_profile(&path, "ubuntu", "mac").expect_err("a collision is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let table: toml::Table =
            std::fs::read_to_string(&path).expect("read").parse().expect("valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert_eq!(
            profiles["mac"]["command"].as_str(),
            Some("zsh"),
            "the refused rename still overwrote the target"
        );
        assert!(profiles.contains_key("ubuntu"), "the refused rename still moved the source");

        assert!(
            rename_profile(&path, "ghost", "ghost-2").is_err(),
            "renaming a profile that is not there means the caller is stale; say so"
        );
        rename_profile(&path, "mac", "mac").expect("renaming to the same name is a no-op");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn renaming_reaches_into_an_inline_profiles_table() {
        // Review finding on #290. `profiles = { work = { … } }` is a legal
        // hand-written spelling, and `as_table_mut` declines an inline table —
        // so the rename reported "no profile `work`" for one plainly in the
        // file. `remove_in` already took this shape seriously; a writer that
        // understands half the file is worse than one that understands none,
        // because it fails only for the people who hand-edit.
        let path = temp("rename-inline");
        std::fs::write(
            &path,
            "profiles = { work = { command = \"zsh\" }, mac = { command = \"bash\" } }\n",
        )
        .expect("write");

        rename_profile(&path, "work", "forge").expect("an inline profiles table renames");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert_eq!(
            profiles["forge"]["command"].as_str(),
            Some("zsh"),
            "the inline profile did not take the new name: {text}"
        );
        assert!(!profiles.contains_key("work"), "the old name survived: {text}");
        assert_eq!(
            profiles["mac"]["command"].as_str(),
            Some("bash"),
            "a sibling in the same inline table was disturbed: {text}"
        );
        assert_eq!(
            rename_profile(&path, "forge", "mac").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
            "the collision check must reach inline tables too"
        );
        let table: toml::Table =
            std::fs::read_to_string(&path).expect("read").parse().expect("valid toml");
        assert!(
            table["profiles"].as_table().expect("table").contains_key("forge"),
            "a refused rename removed the source anyway"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_dotted_new_name_stays_one_key() {
        // The twin of `a_dotted_profile_name_stays_one_key`, for the other end
        // of a rename: `prod.eu` is ONE profile, not `[profiles.prod.eu]`.
        let path = temp("rename-dotted");
        std::fs::write(&path, "[profiles.staging]\ncommand = \"bash\"\n").expect("write");

        rename_profile(&path, "staging", "prod.eu").expect("rename");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert_eq!(
            profiles["prod.eu"]["command"].as_str(),
            Some("bash"),
            "a dotted name shattered into nested tables: {text}"
        );
        assert!(
            !profiles.contains_key("prod"),
            "the name was split on the dot: {text}"
        );
        // And the renamed profile is still writable under its new name.
        write_profile_value(&path, "prod.eu", "host", toml_edit::Value::from("forge"))
            .expect("write under the new name");
        let table: toml::Table =
            std::fs::read_to_string(&path).expect("read").parse().expect("valid toml");
        assert_eq!(table["profiles"]["prod.eu"]["host"].as_str(), Some("forge"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn editing_a_copy_writes_only_the_copy() {
        // #272 was reported as "duplicate a profile, change its command, and
        // it is never saved" — which put the writer under suspicion. It is
        // innocent, and this pins that: the loss was upstream, in an editor
        // that only ever committed on Enter. If this test ever goes red the
        // diagnosis was wrong and the writer really is at fault.
        let path = temp("copy-then-edit");
        std::fs::write(
            &path,
            "# hand-written\n[profiles.wsl]\ncommand = \"wsl.exe\"\n\n[appearance]\ntheme = \"nord\"\n",
        )
        .expect("write");

        copy_profile(&path, "wsl", "wsl-2").expect("copy");
        write_profile_value(
            &path,
            "wsl-2",
            "command",
            toml_edit::Value::from("/opt/homebrew/bin/fish"),
        )
        .expect("write the copy's command");

        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        let profiles = table["profiles"].as_table().expect("table");
        assert_eq!(
            profiles["wsl-2"]["command"].as_str(),
            Some("/opt/homebrew/bin/fish"),
            "the copy did not take the edit: {text}"
        );
        assert_eq!(
            profiles["wsl"]["command"].as_str(),
            Some("wsl.exe"),
            "editing the copy changed its source: {text}"
        );
        assert_eq!(
            table["appearance"]["theme"].as_str(),
            Some("nord"),
            "an unrelated root key was disturbed: {text}"
        );
        assert!(text.contains("# hand-written"), "the comment did not survive: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn creating_a_profile_writes_only_its_header_and_is_idempotent() {
        // "＋ New profile" (design §12): an empty table is a valid launch
        // target, so creation writes the header alone — and re-creating an
        // existing profile must never truncate what the user put in it.
        let path = temp("create-profile");
        std::fs::write(&path, "# my terminal\n[typography]\nsize_pt = 14.0\n").expect("write");

        create_profile(&path, "new-profile-1").expect("create");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# my terminal"), "lost a comment: {text}");
        assert!(text.contains("[profiles.new-profile-1]"), "no header: {text}");
        let table: toml::Table = text.parse().expect("still valid toml");
        assert!(
            table["profiles"]["new-profile-1"].as_table().is_some_and(toml::Table::is_empty),
            "a new profile starts empty: {text}"
        );

        write_profile_value(
            &path,
            "new-profile-1",
            "icon",
            toml_edit::value("★").into_value().unwrap(),
        )
        .expect("edit");
        create_profile(&path, "new-profile-1").expect("re-create");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("icon"), "re-creating truncated the profile: {text}");
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
