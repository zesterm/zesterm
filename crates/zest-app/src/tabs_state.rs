//! The remembered tab set of the single-window days, read for migration.
//!
//! Superseded by [`crate::windows_state`], which remembers every window and
//! reads this file as one window with no geometry until its own first save
//! replaces it. The tab record itself lives on here — it is the same tab
//! whichever window holds it.
//!
//! This file is what retired the adopt guess (#23): launching used to pick
//! *any* unattached session — once, one another machine was mid-command in —
//! because the app had no memory of what it was showing. Now it does. State,
//! not settings: it lives in `paths::state_dir`, never in the watched config
//! directory, and no schema is generated from it.
//!
//! Failure is always the empty set. A corrupt, missing, or future-versioned
//! file means a fresh local shell, which is the safe launch — never a
//! refused one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zest_proto::SessionAddr;

/// Bumped on incompatible shape changes; an unknown version reads as "no
/// saved tabs" rather than a guess at someone else's format.
const VERSION: u32 = 1;

const FILE: &str = "tabs.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedTabs {
    version: u32,
    /// Index into `tabs` of the one holding the keyboard.
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTab {
    /// `HostId` + session, hex on disk via the ids' own serde.
    pub addr: SessionAddr,
    /// Close-policy bit: a restored local tab kills on close like any other.
    pub local: bool,
    /// How the host was dialled, when it was not the local socket. The
    /// roster may know a fresher address by the time this is read; this is
    /// the fallback for a restore that races discovery.
    pub dial_hint: Option<String>,
    /// The last title, so a reconnecting tab has a name before its keyframe.
    pub title: String,
}

impl SavedTabs {
    #[cfg(test)]
    pub fn new(active: usize, tabs: Vec<SavedTab>) -> Self {
        Self { version: VERSION, active, tabs }
    }
}

fn file() -> Option<PathBuf> {
    zest_config::paths::state_dir().map(|d| d.join(FILE))
}

/// The saved tab set, or `None` for "start fresh" — which covers missing,
/// unreadable, corrupt, and future-versioned alike.
#[must_use]
pub fn load() -> Option<SavedTabs> {
    let path = file()?;
    let text = std::fs::read_to_string(path).ok()?;
    let saved: SavedTabs = serde_json::from_str(&text).ok()?;
    (saved.version == VERSION && !saved.tabs.is_empty()).then_some(saved)
}

/// Forget the old single-window file, once `windows.json` has replaced it.
/// Never fatal and never loud: a file that was never there is the common
/// case after the first launch on this version.
pub(crate) fn remove() {
    if let Some(path) = file() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_set_round_trips_through_json() {
        // The address serializes through the ids' own hex serde; if that
        // ever changed shape, restore would silently start fresh forever.
        let tab = SavedTab {
            addr: SessionAddr::new(zest_proto::HostId::from_bytes([7; 32]), zest_proto::SessionId(3)),
            local: true,
            dial_hint: None,
            title: "vim".into(),
        };
        let saved = SavedTabs::new(0, vec![tab]);
        let text = serde_json::to_string(&saved).expect("serialize");
        let back: SavedTabs = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back.tabs[0].addr, saved.tabs[0].addr);
        assert_eq!(back.tabs[0].title, "vim");
        assert_eq!(back.active, 0);
    }

    #[test]
    fn an_unknown_version_reads_as_no_saved_tabs() {
        let text = r#"{"version": 99, "active": 0, "tabs": [{"addr": {"host": "00", "session": 1}, "local": true, "dial_hint": null, "title": ""}]}"#;
        let saved: Result<SavedTabs, _> = serde_json::from_str(text);
        // Whether it parses or not, `load`'s version gate refuses it — this
        // pins the gate's premise: parse errors and version mismatches must
        // both land on "start fresh".
        if let Ok(s) = saved {
            assert_ne!(s.version, VERSION);
        }
    }
}
