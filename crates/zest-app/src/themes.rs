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
//!
//! **Reading and writing the directory is [`zest_theme::dir`]'s job**, not this
//! module's. It moved there so the daemon can answer what themes a machine has
//! for a client on the other side of a relay; what is left here is the part
//! that is genuinely about a running window — the process-global cache, and the
//! rule that a built-in outranks an import.

use std::sync::RwLock;

use zest_theme::Theme;

static USER: RwLock<Vec<Theme>> = RwLock::new(Vec::new());

/// Read the user's theme directory into the roster. Called at startup and on
/// config reload; loading is a *replace*, so a deleted file really is gone.
pub(crate) fn reload() {
    if let Some(dir) = zest_config::paths::themes_dir() {
        *USER.write().unwrap() = zest_theme::dir::load_dir(&dir);
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
    let theme = zest_theme::dir::install(&dir, source)?;
    merge(&mut USER.write().unwrap(), theme.clone());
    Ok(theme)
}

/// Put `theme` into the roster, replacing any entry with the same id.
///
/// Separate from the write so it is testable without the process-global, and
/// matched to what [`zest_theme::dir::install`] does on disk: that function
/// overwrites `<id>.toml`, so a roster that *pushed* would show two cards for
/// one file until the next reload silently dropped one of them. It takes the
/// theme `install` returned rather than the one that was parsed, because a
/// built-in collision suffixes the id and only the returned value knows.
fn merge(user: &mut Vec<Theme>, theme: Theme) {
    match user.iter_mut().find(|t| t.id == theme.id) {
        Some(slot) => *slot = theme,
        None => user.push(theme),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disk rules are [`zest_theme::dir`]'s and are tested there. What is
    /// left here is the roster, so these do not touch a filesystem at all.
    fn theme(id: &str) -> Theme {
        let mut t = zest_theme::builtin::paper();
        t.id = id.into();
        t.name = id.to_uppercase();
        t
    }

    #[test]
    fn reimporting_updates_the_roster_rather_than_duplicating_it() {
        // Importing a tweaked copy of the same scheme is the editing loop, and
        // `install` overwrites one file — so a roster that pushed would show
        // two cards for one file until a reload dropped one at random.
        let mut user = vec![theme("campbell")];
        let mut tweaked = theme("campbell");
        tweaked.name = "Campbell Red".into();
        merge(&mut user, tweaked.clone());
        assert_eq!(user.len(), 1, "same id replaces in place");
        assert_eq!(user[0], tweaked, "and it is the new one that survives");
    }

    #[test]
    fn a_new_id_joins_the_roster() {
        let mut user = vec![theme("campbell")];
        merge(&mut user, theme("solarized"));
        assert_eq!(user.len(), 2);
    }

    /// Swap the process-global roster for the duration of a test and put it
    /// back on the way out.
    ///
    /// A guard rather than a `clear()` at the end of the test, because the
    /// interesting case is the one where the assertion *fails*: a panic skips
    /// the cleanup line, and every test that runs afterwards in the same
    /// process then sees a roster somebody else stuffed. That turns one real
    /// failure into a spray of unrelated ones, which is the shape #403's umask
    /// leak had -- process-global state whose victims are nowhere near the
    /// culprit. `Drop` runs during unwind; the line after the assert does not.
    struct Roster(Vec<Theme>);
    impl Roster {
        fn set(themes: Vec<Theme>) -> Self {
            Self(std::mem::replace(&mut *USER.write().unwrap(), themes))
        }
    }
    impl Drop for Roster {
        fn drop(&mut self) {
            // `PoisonError::into_inner`: another test panicking while holding
            // this lock must not turn into a second panic here, which would
            // abort the whole run and bury the first failure's message.
            let mut slot = USER.write().unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = std::mem::take(&mut self.0);
        }
    }

    #[test]
    fn a_builtin_outranks_an_import_of_the_same_id() {
        // The rule the module exists to hold: `zest_theme::dir::load_dir`
        // refuses to load a shadowing file, and `get` refuses to serve one
        // even if some other path put it in the roster. Two guards, because
        // this one decides what every `theme = "obsidian"` in every config
        // resolves to.
        let _roster = Roster::set(vec![theme("obsidian")]);
        let got = get("obsidian").expect("a built-in is always resolvable");
        assert_eq!(
            got,
            zest_theme::builtin::obsidian(),
            "an import shadowed a built-in, restyling every config that names it"
        );
    }
}
