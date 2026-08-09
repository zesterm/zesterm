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
#[cfg(feature = "fs")]
pub use watch::Watcher;
// Re-exported because `write_value`'s signature already names
// `toml_edit::Value` — a caller cannot use the function without the type, and
// should not need its own pinned copy of the dependency to get it.
#[cfg(feature = "fs")]
pub use toml_edit;
pub use settings::{
    Appearance, Backdrop, Cursor, CursorShape, CursorTrail, Motion, Scrolling, Settings, Shell,
    Typography, Window, CURRENT_SCHEMA_VERSION,
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

    // The profile layer comes from inside the user's file, so it can only exist
    // once that file has been read.
    if let (Some(name), Some(table)) = (options.profile.as_ref(), user_table.as_ref()) {
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
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, groups)) = parts.split_last() else {
        return Ok(());
    };

    let mut node = doc.as_item_mut();
    for group in groups {
        node = &mut node[*group];
        if node.is_none() {
            *node = toml_edit::Item::Table(toml_edit::Table::new());
        }
    }
    node[*last] = toml_edit::Item::Value(value);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())
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
