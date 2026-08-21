//! The theme roster: the built-ins plus the user's imported themes.
//!
//! Imported themes live as one TOML file each in
//! `zest_config::paths::themes_dir()` — written by the gallery's import card,
//! read back at startup, and re-read on config reload so a hand-edited file
//! is picked up the same way an edited `config.toml` is. The roster is
//! process-global because scheme resolution happens in free functions
//! (`tabs::resolve_scheme`) as well as on `App`, and threading a handle into
//! every palette lookup would touch far more seams than the data earns.
//!
//! The built-ins always win a lookup: a user file that claims a built-in's id
//! is skipped with a warning rather than silently restyling "obsidian"
//! everywhere a default is resolved.

use std::path::Path;
use std::sync::RwLock;

use zest_theme::Theme;

static USER: RwLock<Vec<Theme>> = RwLock::new(Vec::new());

/// Read the user's theme directory into the roster. Called at startup and on
/// config reload; loading is a *replace*, so a deleted file really is gone.
pub(crate) fn reload() {
    if let Some(dir) = zest_config::paths::themes_dir() {
        *USER.write().unwrap() = load_dir(&dir);
    }
}

/// A theme by id — built-ins first, then the user's imports.
pub(crate) fn get(id: &str) -> Option<Theme> {
    zest_theme::builtin::get(id)
        .or_else(|| USER.read().unwrap().iter().find(|t| t.id == id).cloned())
}

/// Every theme, built-ins in their canonical order and then the user's
/// imports — the gallery order, so an index into this list is a card.
pub(crate) fn all() -> Vec<Theme> {
    let mut out = zest_theme::builtin::all();
    out.extend(USER.read().unwrap().iter().cloned());
    out
}

/// The ids of [`all`], in the same order.
pub(crate) fn ids() -> Vec<String> {
    all().into_iter().map(|t| t.id).collect()
}

/// Import a pasted scheme: parse, write it into the theme directory, and add
/// it to the roster. Returns the imported theme so the caller can apply it.
pub(crate) fn import_pasted(source: &str) -> Result<Theme, String> {
    let dir = zest_config::paths::themes_dir()
        .ok_or_else(|| "no config directory to keep imported themes in".to_string())?;
    install(&mut USER.write().unwrap(), &dir, source)
}

/// The testable core of [`import_pasted`]: everything but the process-global
/// state and the real config directory.
fn install(user: &mut Vec<Theme>, dir: &Path, source: &str) -> Result<Theme, String> {
    // No filename to go on — a paste is detected from content alone, and the
    // name is a placeholder the richer formats (Windows Terminal, base16)
    // override with the name the file itself carries.
    let mut theme =
        zest_theme::import::import("Imported scheme", "", source).map_err(|e| e.to_string())?;

    // A name of punctuation slugs to nothing, and an empty id can never be
    // chosen again once written to config.
    if theme.id.is_empty() {
        theme.id = "imported".to_string();
    }
    // A built-in's id must stay the built-in's: suffix rather than shadow, so
    // importing a "Nord" variant does not restyle every `theme = "nord"`.
    // Re-importing over an earlier *import* replaces it instead — updating a
    // scheme must not accumulate copies.
    let base_id = theme.id.clone();
    let base_name = theme.name.clone();
    let mut n = 2;
    while zest_theme::builtin::get(&theme.id).is_some() {
        theme.id = format!("{base_id}-{n}");
        theme.name = format!("{base_name} {n}");
        n += 1;
    }

    let text = toml::to_string_pretty(&theme).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{}.toml", theme.id)), text).map_err(|e| e.to_string())?;

    match user.iter_mut().find(|t| t.id == theme.id) {
        Some(slot) => *slot = theme.clone(),
        None => user.push(theme.clone()),
    }
    Ok(theme)
}

/// Every parseable theme in `dir`, sorted by name for a stable gallery.
fn load_dir(dir: &Path) -> Vec<Theme> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    // Sorted by file name: read_dir order is unspecified, and "the later
    // file wins" for a duplicate id is only an actionable rule when "later"
    // means the same file on every platform and every run.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut out: Vec<Theme> = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        // One bad file must not poison the rest: themes are user-authored
        // input, and the never-crash rule for a deleted scheme (see
        // `tabs::resolve_scheme`) applies just as much to a mistyped one.
        let theme: Theme = match toml::from_str(&text) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "unreadable theme file skipped");
                continue;
            }
        };
        if zest_theme::builtin::get(&theme.id).is_some() {
            tracing::warn!(path = %path.display(), id = %theme.id, "theme file shadows a built-in; skipped");
            continue;
        }
        if let Some(prev) = out.iter().position(|t| t.id == theme.id) {
            tracing::warn!(path = %path.display(), id = %theme.id, "duplicate theme id; the later file wins");
            out.remove(prev);
        }
        out.push(theme);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory per test — the process-global roster is
    /// deliberately not touched here, so tests cannot race each other.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("zesterm-themes-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    const WT: &str = r##"{
      "name": "Campbell",
      "background": "#0C0C0C", "foreground": "#CCCCCC",
      "black": "#0C0C0C", "red": "#C50F1F", "green": "#13A10E", "yellow": "#C19C00",
      "blue": "#0037DA", "purple": "#881798", "cyan": "#3A96DD", "white": "#CCCCCC"
    }"##;

    #[test]
    fn an_imported_scheme_survives_a_reload() {
        // The whole point of writing the file: the theme must still exist
        // after a restart, which load_dir stands in for here.
        let dir = scratch("reload");
        let mut user = Vec::new();
        let theme = install(&mut user, &dir, WT).expect("import");
        assert_eq!(theme.id, "campbell");
        assert_eq!(user.len(), 1, "the roster gains the theme immediately");

        let loaded = load_dir(&dir);
        assert_eq!(loaded.len(), 1, "the written file reads back");
        assert_eq!(loaded[0], theme, "byte-for-byte the same theme, or a restart restyles it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reimporting_updates_rather_than_duplicates() {
        // Importing a tweaked copy of the same scheme is the editing loop;
        // a gallery that grows a card per attempt punishes it.
        let dir = scratch("reimport");
        let mut user = Vec::new();
        install(&mut user, &dir, WT).expect("first");
        let tweaked = WT.replace("#C50F1F", "#FF0000");
        let theme = install(&mut user, &dir, &tweaked).expect("second");
        assert_eq!(user.len(), 1, "same id replaces in place");
        assert_eq!(user[0], theme);
        assert_eq!(load_dir(&dir).len(), 1, "one file on disk too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_may_not_shadow_a_builtin() {
        // `theme = "nord"` in someone's config must keep meaning the
        // built-in; the import gets a suffixed id beside it instead.
        let dir = scratch("shadow");
        let mut user = Vec::new();
        let theme = install(&mut user, &dir, &WT.replace("Campbell", "Nord")).expect("import");
        assert_eq!(theme.id, "nord-2");
        assert_eq!(theme.name, "Nord 2", "the visible name moves with the id");
        assert!(zest_theme::builtin::get("nord").is_some(), "the built-in is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_builtin_shadow_on_disk_is_skipped() {
        // A hand-placed obsidian.toml must not silently restyle the default
        // theme everywhere a lookup falls back to it.
        let dir = scratch("disk-shadow");
        let mut fake = zest_theme::builtin::paper();
        fake.id = "obsidian".into();
        std::fs::write(dir.join("obsidian.toml"), toml::to_string_pretty(&fake).unwrap())
            .unwrap();
        assert!(load_dir(&dir).is_empty(), "a built-in id on disk is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_file_does_not_poison_the_rest() {
        let dir = scratch("malformed");
        let mut user = Vec::new();
        install(&mut user, &dir, WT).expect("import");
        std::fs::write(dir.join("broken.toml"), "not = a theme").unwrap();
        let loaded = load_dir(&dir);
        assert_eq!(loaded.len(), 1, "the good theme still loads");
        assert_eq!(loaded[0].id, "campbell");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicate_id_resolves_by_file_name_order() {
        // read_dir order is unspecified, so the tiebreak has to be one we
        // impose: last file name in sort order wins, on every platform.
        let dir = scratch("dup");
        let mut user = Vec::new();
        install(&mut user, &dir, WT).expect("import");
        let red = std::fs::read_to_string(dir.join("campbell.toml")).unwrap();
        let blue = red.replace("#c50f1f", "#0000ff");
        std::fs::write(dir.join("a-first.toml"), &blue).unwrap();
        let loaded = load_dir(&dir);
        assert_eq!(loaded.len(), 1, "one id, one theme");
        assert_eq!(
            loaded[0].ansi.normal.unwrap()[1],
            zest_theme::Rgba8::rgb(0xc5, 0x0f, 0x1f),
            "campbell.toml sorts after a-first.toml, so its colours win"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_is_an_error_not_a_theme() {
        let dir = scratch("garbage");
        let mut user = Vec::new();
        assert!(install(&mut user, &dir, "definitely not a colour scheme").is_err());
        assert!(user.is_empty(), "nothing joins the roster on failure");
        assert!(load_dir(&dir).is_empty(), "nothing is written on failure");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
