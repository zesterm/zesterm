//! Reading and writing a machine's imported themes.
//!
//! The only part of this crate that touches a filesystem, and it is behind the
//! `fs` feature for that reason: everything else compiles to
//! `wasm32-unknown-unknown` so the browser client runs *this* [`resolve`] rather
//! than a JavaScript port that drifts, and a default-on `std::fs` module would
//! put a `read_dir` in that build's way.
//!
//! It lives here rather than in `zest-app` — where it was written — because
//! `zest-app` is a `[[bin]]`, so nothing outside it can call in. The daemon has
//! to answer "what themes does this machine have" for a client on the other side
//! of a relay, and the code that knew was in a binary the daemon does not link.
//!
//! The roster itself stays an application concern: this module reads and writes
//! a directory and returns owned [`Theme`]s. Who caches them, and whether the
//! built-ins win a lookup, is a question about a running window.

use std::path::Path;

use crate::Theme;

/// How many theme files one directory may be read out of.
///
/// A bound rather than tidiness: [`load_dir`] runs on the daemon's serve loop,
/// which holds a connection's lock across the message it is answering, so an
/// unbounded scan of a directory somebody dropped ten thousand files into
/// stalls that session's own input and output. It lives beside the reader
/// rather than at a call site because a second caller is how a bound gets
/// forgotten.
///
/// It counts files **examined**, not themes returned, and the difference is
/// the whole of the protection. Capping the output bounds nothing: a
/// directory of ten thousand malformed `.toml` files yields no themes at all,
/// so an output cap never trips, while every one of them still costs a
/// `read_to_string` and a parse attempt — which is exactly the work the serve
/// loop cannot afford. A file that fails to parse, shadows a built-in, or
/// duplicates an id has already been paid for by the time we know.
pub const MAX_THEMES: usize = 256;

/// Every parseable theme in `dir`, sorted by name for a stable gallery.
///
/// A missing or unreadable directory is an empty roster, not an error: a
/// machine that has never imported a theme is the common case.
#[must_use]
pub fn load_dir(dir: &Path) -> Vec<Theme> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    // Sorted by file name: read_dir order is unspecified, and "the later
    // file wins" for a duplicate id is only an actionable rule when "later"
    // means the same file on every platform and every run.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut out: Vec<Theme> = Vec::new();
    // Counts candidates opened, not themes kept -- see `MAX_THEMES`. The
    // increment therefore sits above every `continue` that can reject a file,
    // because each of those has already spent the read this bound exists to
    // limit.
    let mut examined = 0usize;
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if examined >= MAX_THEMES {
            tracing::warn!(
                dir = %dir.display(),
                max = MAX_THEMES,
                "too many theme files; the rest of the directory is ignored"
            );
            break;
        }
        examined += 1;
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
        if crate::builtin::get(&theme.id).is_some() {
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

/// Parse a pasted scheme and write it into `dir` as one TOML file.
///
/// Returns the theme as it was stored — which is not always the theme that was
/// parsed, because an id colliding with a built-in is suffixed rather than
/// allowed to shadow it. A caller adding this to a roster must use the returned
/// value, or its cache and the disk disagree about the id.
///
/// Deliberately does **not** take the roster: who holds the imported themes in
/// memory is an application question, and threading a `&mut Vec` through here
/// is what kept this function inside a binary crate.
pub fn install(dir: &Path, source: &str) -> Result<Theme, String> {
    // No filename to go on — a paste is detected from content alone, and the
    // name is a placeholder the richer formats (Windows Terminal, base16)
    // override with the name the file itself carries.
    let mut theme =
        crate::import::import("Imported scheme", "", source).map_err(|e| e.to_string())?;

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
    while crate::builtin::get(&theme.id).is_some() {
        theme.id = format!("{base_id}-{n}");
        theme.name = format!("{base_name} {n}");
        n += 1;
    }

    let text = toml::to_string_pretty(&theme).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{}.toml", theme.id)), text).map_err(|e| e.to_string())?;

    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory per test, so tests cannot race each other.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("zesterm-themedir-test-{tag}-{}", std::process::id()));
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
        let theme = install(&dir, WT).expect("import");
        assert_eq!(theme.id, "campbell");

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
        install(&dir, WT).expect("first");
        let tweaked = WT.replace("#C50F1F", "#FF0000");
        install(&dir, &tweaked).expect("second");
        assert_eq!(load_dir(&dir).len(), 1, "one file on disk, not two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_may_not_shadow_a_builtin() {
        // `theme = "nord"` in someone's config must keep meaning the
        // built-in; the import gets a suffixed id beside it instead.
        let dir = scratch("shadow");
        let theme = install(&dir, &WT.replace("Campbell", "Nord")).expect("import");
        assert_eq!(theme.id, "nord-2");
        assert_eq!(theme.name, "Nord 2", "the visible name moves with the id");
        assert!(crate::builtin::get("nord").is_some(), "the built-in is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_builtin_shadow_on_disk_is_skipped() {
        // A hand-placed obsidian.toml must not silently restyle the default
        // theme everywhere a lookup falls back to it.
        let dir = scratch("disk-shadow");
        let mut fake = crate::builtin::paper();
        fake.id = "obsidian".into();
        std::fs::write(dir.join("obsidian.toml"), toml::to_string_pretty(&fake).unwrap())
            .unwrap();
        assert!(load_dir(&dir).is_empty(), "a built-in id on disk is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_file_does_not_poison_the_rest() {
        let dir = scratch("malformed");
        install(&dir, WT).expect("import");
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
        install(&dir, WT).expect("import");
        let red = std::fs::read_to_string(dir.join("campbell.toml")).unwrap();
        let blue = red.replace("#c50f1f", "#0000ff");
        std::fs::write(dir.join("a-first.toml"), &blue).unwrap();
        let loaded = load_dir(&dir);
        assert_eq!(loaded.len(), 1, "one id, one theme");
        assert_eq!(
            loaded[0].ansi.normal.unwrap()[1],
            crate::Rgba8::rgb(0xc5, 0x0f, 0x1f),
            "campbell.toml sorts after a-first.toml, so its colours win"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_is_an_error_not_a_theme() {
        let dir = scratch("garbage");
        assert!(install(&dir, "definitely not a colour scheme").is_err());
        assert!(load_dir(&dir).is_empty(), "nothing is written on failure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_of_unreadable_files_stops_at_the_cap_too() {
        // The cap has to count files *examined*, and this is the test that can
        // tell the two implementations apart: capping the output would let a
        // directory of malformed files run to the end, because no output is
        // ever produced to trip it -- while every file still costs a read and
        // a parse on the serve loop.
        //
        // Observable because the good theme sorts last: if the scan stopped at
        // the cap, it never reached it.
        let dir = scratch("cap-malformed");
        for i in 0..(MAX_THEMES + 20) {
            std::fs::write(dir.join(format!("{i:04}-bad.toml")), "not = a theme").unwrap();
        }
        let mut good = crate::builtin::paper();
        good.id = "zzz-good".into();
        good.name = "ZZZ Good".into();
        std::fs::write(dir.join("zzz-good.toml"), toml::to_string_pretty(&good).unwrap())
            .unwrap();

        assert!(
            load_dir(&dir).is_empty(),
            "the scan read past {MAX_THEMES} unparseable files to reach the last one: \
             the cap is counting themes returned rather than files opened, which bounds \
             nothing when nothing parses"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_of_too_many_themes_is_capped() {
        // The bound is why this runs on the daemon's serve loop at all, so the
        // assertion is that it *reached* the cap — a test that only checked
        // "some themes loaded" would pass just as well with no cap in place,
        // which is the state this exists to rule out.
        let dir = scratch("cap");
        let base = crate::builtin::paper();
        for i in 0..(MAX_THEMES + 20) {
            let mut t = base.clone();
            t.id = format!("t{i:04}");
            t.name = format!("Theme {i:04}");
            std::fs::write(dir.join(format!("{}.toml", t.id)), toml::to_string_pretty(&t).unwrap())
                .unwrap();
        }
        let loaded = load_dir(&dir);
        assert_eq!(
            loaded.len(),
            MAX_THEMES,
            "the scan must stop at the cap: it runs on the serve loop, where an \
             unbounded read_dir stalls the session's own I/O"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
