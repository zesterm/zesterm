//! Where a pointer event lands in the chrome.
//!
//! The hit map is produced by the same layout pass that produces the rects,
//! which is the whole design: a region and the rectangle it was drawn as come
//! from one computation, so visuals and hit targets physically cannot drift.
//! (`docs/ROADMAP.md`, WS-A.)

use zest_proto::SessionAddr;

/// What a point in the chrome means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitRegion {
    /// A tab; clicking activates it.
    Tab(SessionAddr),
    /// A tab's close button.
    TabClose(SessionAddr),
    /// The new-tab button.
    NewTab,
    /// Empty chrome that moves the window when dragged.
    Drag,
    /// The strip itself — catches wheel scrolling and swallows clicks that
    /// would otherwise fall through to the grid beneath.
    Strip,
    /// One row of the open picker, by index into its row list.
    PickerRow(usize),
    /// The dimmed backdrop behind the picker; clicking it dismisses.
    PickerScrim,
}

/// Rectangles to meanings, in draw order.
#[derive(Debug, Default)]
pub struct ChromeHitMap {
    /// `[x, y, w, h]` in physical pixels, paired with what lives there.
    /// Push order is draw order, so lookups walk it backwards: the thing
    /// drawn last sits on top and wins.
    regions: Vec<([f32; 4], HitRegion)>,
}

impl ChromeHitMap {
    pub fn push(&mut self, rect: [f32; 4], region: HitRegion) {
        self.regions.push((rect, region));
    }

    /// The topmost region containing the point, if any.
    #[must_use]
    pub fn hit(&self, x: f32, y: f32) -> Option<HitRegion> {
        self.regions
            .iter()
            .rev()
            .find(|(r, _)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
            .map(|(_, region)| *region)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_pushed_region_wins_where_they_overlap() {
        // A close button sits inside its tab. If the tab won, the close
        // button would be decorative.
        let mut map = ChromeHitMap::default();
        map.push([0.0, 0.0, 100.0, 30.0], HitRegion::Strip);
        map.push([80.0, 8.0, 16.0, 16.0], HitRegion::NewTab);
        assert_eq!(map.hit(85.0, 10.0), Some(HitRegion::NewTab));
        assert_eq!(map.hit(10.0, 10.0), Some(HitRegion::Strip));
        assert_eq!(map.hit(10.0, 40.0), None, "below the strip is the grid's problem");
    }

    #[test]
    fn edges_are_half_open() {
        // [x, x+w): a point exactly on the right edge belongs to whatever is
        // next, or two adjacent tabs would both claim their shared border.
        let mut map = ChromeHitMap::default();
        map.push([0.0, 0.0, 10.0, 10.0], HitRegion::Drag);
        assert_eq!(map.hit(0.0, 0.0), Some(HitRegion::Drag));
        assert_eq!(map.hit(10.0, 5.0), None);
    }
}
