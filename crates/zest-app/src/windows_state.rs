//! The remembered window set: what this process reopens as.
//!
//! [`crate::tabs_state`] remembered one window's tabs, because there was one
//! window. This remembers every window — its tabs, and where it stood — and
//! reads the old file as a single window with no geometry, so nobody loses
//! their tabs to the upgrade. The same rules as before: state, not settings
//! (it lives in `paths::state_dir`, never the watched config directory), and
//! failure is always the empty set — a corrupt, missing or future-versioned
//! file means a fresh local shell, never a refused launch.
//!
//! Geometry is in **physical** pixels because that is what
//! `Window::inner_size` and `Window::outer_position` report and what
//! `with_inner_size` / `with_position` take back; converting through logical
//! units would round twice for nothing. A position is `None` where the
//! platform will not say (Wayland has no global coordinates), and a restore
//! then degrades to size only.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zest_proto::SessionAddr;

use crate::tabs_state::{SavedTab, SavedTabs};

/// Bumped on incompatible shape changes; an unknown version reads as "no
/// saved windows" rather than a guess at someone else's format.
const VERSION: u32 = 1;

const FILE: &str = "windows.json";

/// How far a window has to reach onto some monitor to keep its saved
/// position, in physical pixels each way. Smaller than any titlebar's grab
/// area is the wrong answer — a window whose only visible sliver cannot be
/// dragged is a window the user cannot recover.
const MIN_VISIBLE: i32 = 64;

/// How far a new window sits from the one it was opened from.
const CASCADE: i32 = 32;

/// Where a window stood, as the OS reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Geometry {
    /// Client-area size. `None` means "whatever the config says".
    #[serde(default)]
    pub inner_size: Option<[u32; 2]>,
    /// Outer top-left corner. `None` where the platform cannot say.
    #[serde(default)]
    pub position: Option<[i32; 2]>,
    #[serde(default)]
    pub maximized: bool,
}

/// A monitor's rectangle in the same physical coordinate space, for
/// deciding whether a saved position is still somewhere a person can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Index into `tabs` of the one holding the keyboard.
    pub active: usize,
    pub tabs: Vec<SavedTab>,
    #[serde(flatten)]
    pub geometry: Geometry,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedWindows {
    version: u32,
    pub windows: Vec<SavedWindow>,
}

impl SavedWindows {
    /// A set to save. Windows with nothing worth reopening — every tab an
    /// in-process pty that dies with the window — are dropped here, so the
    /// file never carries an entry that would reopen as an empty strip.
    pub fn new(windows: Vec<SavedWindow>) -> Self {
        let windows = windows.into_iter().filter(|w| !w.tabs.is_empty()).collect();
        Self { version: VERSION, windows }
    }

    /// The old single-window file, read as one window that remembers no
    /// geometry — the config's size rule applies, exactly as it did before.
    pub fn from_tabs_v1(saved: SavedTabs) -> Self {
        Self::new(vec![SavedWindow {
            active: saved.active,
            tabs: saved.tabs,
            geometry: Geometry::default(),
        }])
    }
}

fn file() -> Option<PathBuf> {
    zest_config::paths::state_dir().map(|d| d.join(FILE))
}

/// Parse a saved set, or `None` for "start fresh" — which covers corrupt,
/// future-versioned and empty alike. Separate from [`load`] so the rule is
/// testable without a state directory.
fn parse(text: &str) -> Option<SavedWindows> {
    let saved: SavedWindows = serde_json::from_str(text).ok()?;
    if saved.version != VERSION {
        return None;
    }
    let saved = SavedWindows::new(saved.windows);
    (!saved.windows.is_empty()).then_some(saved)
}

/// The saved window set, or `None` for "start fresh". Falls back to the
/// pre-multi-window `tabs.json`, which is left in place until the first
/// successful save of this file replaces it — a launch that crashes before
/// then must not have eaten the only memory of the tabs.
#[must_use]
pub fn load() -> Option<SavedWindows> {
    let from_file = || {
        let text = std::fs::read_to_string(file()?).ok()?;
        parse(&text)
    };
    from_file().or_else(|| crate::tabs_state::load().map(SavedWindows::from_tabs_v1))
}

/// Persist the window set. Atomic (temp + `sync_all` + rename) so a crash
/// mid-write leaves the previous set rather than half a file — the rename
/// alone is not enough, since it can land before the bytes do; never fatal,
/// because losing the memory of windows must not take the windows with it.
pub fn save(saved: &SavedWindows) {
    let Some(path) = file() else { return };
    let Some(dir) = path.parent() else { return };
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        // A serialization failure is an error, not an empty file: an empty
        // file reads as "nothing remembered" and would quietly erase the set.
        let bytes = serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    match write() {
        // This file is now the memory; the old one would only shadow it on
        // a downgrade, and a downgrade reading a stale tab set is worse than
        // one starting fresh.
        Ok(()) => crate::tabs_state::remove(),
        Err(e) => tracing::warn!(error = %e, "could not save the window set"),
    }
}

/// Which remembered tab leads a window's restore, and which follow.
///
/// The synchronous startup slot fits exactly one attach, and only a *local*
/// one keeps the startup budget honest — a remote active tab would put a
/// network dial on the startup path. So the active tab leads when it is
/// local, else the first local one does, else nothing leads and everything
/// arrives in the background behind a fresh shell.
pub fn split_lead(mut tabs: Vec<SavedTab>, active: usize) -> (Option<SessionAddr>, Vec<SavedTab>) {
    let lead = if tabs.get(active).is_some_and(|t| t.local) {
        Some(active)
    } else {
        tabs.iter().position(|t| t.local)
    };
    match lead {
        Some(i) => {
            let lead = tabs.remove(i);
            (Some(lead.addr), tabs)
        }
        None => (None, tabs),
    }
}

/// Fit a remembered geometry to the monitors that exist now.
///
/// A position is kept only where the window would still show at least
/// [`MIN_VISIBLE`] pixels each way on some monitor — the monitor it was on
/// may be unplugged, or the desktop rearranged, and a window restored
/// off-screen is a window the user cannot find. The size is clamped to the
/// largest monitor so a window saved on a 4K panel does not come back larger
/// than the laptop it is reopened on. With no monitors to judge by, nothing
/// is changed: an empty list means the platform would not say, not that
/// there is nowhere to put a window.
#[must_use]
pub fn place(saved: Geometry, monitors: &[Rect]) -> Geometry {
    if monitors.is_empty() {
        return saved;
    }
    let largest = monitors.iter().max_by_key(|m| u64::from(m.w) * u64::from(m.h)).copied();
    let inner_size = match (saved.inner_size, largest) {
        (Some([w, h]), Some(m)) => Some([w.min(m.w), h.min(m.h)]),
        (size, _) => size,
    };
    let position = saved.position.filter(|&[x, y]| {
        let [w, h] = inner_size.unwrap_or([MIN_VISIBLE as u32, MIN_VISIBLE as u32]);
        monitors.iter().any(|m| {
            let vis_w = (x + w as i32).min(m.x + m.w as i32) - x.max(m.x);
            let vis_h = (y + h as i32).min(m.y + m.h as i32) - y.max(m.y);
            vis_w >= MIN_VISIBLE && vis_h >= MIN_VISIBLE
        })
    });
    Geometry { inner_size, position, maximized: saved.maximized }
}

/// Where a window opened *from* another one goes: the same size, offset a
/// little down and right so the two are visibly two, and never maximized —
/// a new window over a maximized one would hide the one it came from. Then
/// fitted like a restore, so a cascade off the bottom-right of the screen
/// falls back to the platform's own placement rather than off it.
#[must_use]
pub fn cascade(from: Geometry, monitors: &[Rect]) -> Geometry {
    let position = from.position.map(|[x, y]| [x + CASCADE, y + CASCADE]);
    place(Geometry { inner_size: from.inner_size, position, maximized: false }, monitors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_proto::{HostId, SessionId};

    fn tab(n: u8, local: bool) -> SavedTab {
        SavedTab {
            addr: SessionAddr { host: HostId::from_bytes([n; 32]), session: SessionId(u64::from(n)) },
            local,
            dial_hint: (!local).then(|| "10.0.0.2:7717".to_string()),
            title: format!("tab {n}"),
        }
    }

    fn window(active: usize, tabs: Vec<SavedTab>, geometry: Geometry) -> SavedWindow {
        SavedWindow { active, tabs, geometry }
    }

    #[test]
    fn a_saved_set_round_trips_through_json_with_and_without_geometry() {
        let placed = Geometry { inner_size: Some([1200, 800]), position: Some([40, 60]), maximized: true };
        let set = SavedWindows::new(vec![
            window(1, vec![tab(1, true), tab(2, false)], placed),
            window(0, vec![tab(3, true)], Geometry::default()),
        ]);
        let text = serde_json::to_string(&set).unwrap();
        let back = parse(&text).expect("a set this crate wrote must read back");
        assert_eq!(back.windows.len(), 2, "both windows survive the trip");
        assert_eq!(back.windows[0].geometry, placed, "geometry is part of the memory");
        assert_eq!(back.windows[0].active, 1);
        assert_eq!(back.windows[0].tabs[1].dial_hint.as_deref(), Some("10.0.0.2:7717"));
        assert_eq!(
            back.windows[1].geometry,
            Geometry::default(),
            "a window that remembered no geometry reads back as remembering none"
        );
    }

    #[test]
    fn the_old_single_window_file_reads_as_one_window_with_no_geometry() {
        let v1 = SavedTabs::new(1, vec![tab(1, true), tab(2, true)]);
        let set = SavedWindows::from_tabs_v1(v1);
        assert_eq!(set.windows.len(), 1, "one file, one window");
        assert_eq!(set.windows[0].active, 1, "the active index carries over");
        assert_eq!(set.windows[0].tabs.len(), 2);
        assert_eq!(
            set.windows[0].geometry,
            Geometry::default(),
            "the config's size rule applies, exactly as before the upgrade"
        );
    }

    #[test]
    fn an_unknown_version_or_an_empty_set_starts_fresh() {
        let future = r#"{"version": 99, "windows": [{"active": 0, "tabs": []}]}"#;
        assert!(parse(future).is_none(), "a future format is not guessed at");
        let empty = serde_json::to_string(&SavedWindows::new(vec![])).unwrap();
        assert!(parse(&empty).is_none(), "nothing remembered means a fresh shell");
        let all_placeholders =
            serde_json::to_string(&SavedWindows::new(vec![window(0, vec![], Geometry::default())]))
                .unwrap();
        assert!(
            parse(&all_placeholders).is_none(),
            "a window with nothing to reopen is not a window to reopen"
        );
        assert!(parse("not json").is_none());
    }

    #[test]
    fn a_geometry_field_missing_from_the_file_reads_as_unknown() {
        // An older file of this same version may lack a field a later build
        // added; the shape must degrade to "not remembered", never refuse.
        let text = r#"{"version": 1, "windows": [{"active": 0, "tabs": [
            {"addr": {"host": "0101010101010101010101010101010101010101010101010101010101010101", "session": 1},
             "local": true, "dial_hint": null, "title": "t"}]}]}"#;
        let set = parse(text).expect("missing geometry is not an error");
        assert_eq!(set.windows[0].geometry, Geometry::default());
    }

    #[test]
    fn the_active_local_tab_leads_the_restore() {
        let (lead, rest) = split_lead(vec![tab(1, true), tab(2, true)], 1);
        assert_eq!(lead, Some(tab(2, true).addr), "the tab holding the keyboard is the one attached inline");
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].addr, tab(1, true).addr);
    }

    #[test]
    fn a_remote_active_tab_yields_the_lead_to_the_first_local_one() {
        // A remote dial on the startup path would break the budget; the
        // remote tab arrives in the background behind a local lead.
        let (lead, rest) = split_lead(vec![tab(1, false), tab(2, true), tab(3, true)], 0);
        assert_eq!(lead, Some(tab(2, true).addr));
        assert_eq!(rest.iter().map(|t| t.addr.session).collect::<Vec<_>>(), vec![SessionId(1), SessionId(3)]);
    }

    #[test]
    fn with_no_local_tab_nothing_leads() {
        let (lead, rest) = split_lead(vec![tab(1, false), tab(2, false)], 1);
        assert_eq!(lead, None, "a fresh local shell leads and every remote tab follows");
        assert_eq!(rest.len(), 2);
    }

    const LAPTOP: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };
    const RIGHT: Rect = Rect { x: 1920, y: 0, w: 2560, h: 1440 };

    #[test]
    fn a_position_on_a_live_monitor_is_kept() {
        let g = Geometry { inner_size: Some([800, 600]), position: Some([2000, 100]), maximized: false };
        assert_eq!(place(g, &[LAPTOP, RIGHT]), g, "still where the user left it");
    }

    #[test]
    fn a_position_on_an_unplugged_monitor_is_dropped_and_the_size_kept() {
        let g = Geometry { inner_size: Some([800, 600]), position: Some([2000, 100]), maximized: true };
        let placed = place(g, &[LAPTOP]);
        assert_eq!(placed.position, None, "off every monitor: let the platform place it");
        assert_eq!(placed.inner_size, Some([800, 600]), "the size still fits and is kept");
        assert!(placed.maximized, "maximized is a state, not a position, and survives");
    }

    #[test]
    fn a_sliver_off_the_edge_is_not_visible_enough_to_keep() {
        // 40px of the window inside the monitor is less than a titlebar's
        // grab; a person could see it and still not be able to drag it back.
        let g = Geometry { inner_size: Some([800, 600]), position: Some([1880, 100]), maximized: false };
        assert_eq!(place(g, &[LAPTOP]).position, None);
        let g = Geometry { inner_size: Some([800, 600]), position: Some([1800, 100]), maximized: false };
        assert_eq!(place(g, &[LAPTOP]).position, Some([1800, 100]), "120px showing is enough to grab");
    }

    #[test]
    fn a_size_from_a_bigger_screen_is_clamped_to_the_largest_present() {
        let g = Geometry { inner_size: Some([3800, 2000]), position: Some([10, 10]), maximized: false };
        assert_eq!(place(g, &[LAPTOP]).inner_size, Some([1920, 1080]));
        assert_eq!(place(g, &[LAPTOP, RIGHT]).inner_size, Some([2560, 1440]), "the largest monitor bounds it, not the first");
    }

    #[test]
    fn with_no_monitors_to_judge_by_nothing_changes() {
        let g = Geometry { inner_size: Some([3800, 2000]), position: Some([-5000, 10]), maximized: false };
        assert_eq!(place(g, &[]), g, "an empty list means the platform would not say, not that nothing exists");
    }

    #[test]
    fn a_cascade_offsets_from_its_parent_and_is_never_maximized() {
        let from = Geometry { inner_size: Some([800, 600]), position: Some([100, 100]), maximized: true };
        let next = cascade(from, &[LAPTOP]);
        assert_eq!(next.position, Some([132, 132]));
        assert_eq!(next.inner_size, Some([800, 600]), "same size as the window it came from");
        assert!(!next.maximized, "a new window over a maximized one would hide the one it came from");
    }

    #[test]
    fn a_cascade_off_the_screen_falls_back_to_platform_placement() {
        let from = Geometry { inner_size: Some([800, 600]), position: Some([1870, 1050]), maximized: false };
        assert_eq!(cascade(from, &[LAPTOP]).position, None);
        let unknown = Geometry { inner_size: Some([800, 600]), position: None, maximized: false };
        assert_eq!(cascade(unknown, &[LAPTOP]).position, None, "nothing to offset from");
    }
}
