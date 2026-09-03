//! Full-pane screens: the fleet directory and the theme gallery
//! (design screens 7 and 8).
//!
//! Both share one page frame — heading, tagline, lede, then a card grid —
//! drawn opaquely over the grid area. They live in the cached chrome layout
//! (event-driven, never per-frame): a fleet card changes when the fleet
//! does, and the fleet already posts a wakeup for that.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::HitRegion;
use super::layout::{intersect, ChromeLayout, TextRun};
use super::model::{
    FleetAccountAction, FleetAccountModel, FleetCard, FleetDeviceAction, FleetDevicesModel,
    ScreenModel, SettingsValueCell, ThemeCard,
};
use super::theme::ChromeColors;

// Page frame, logical px (design screens 7–8).
const PAD_X: f32 = 38.0;
const PAD_Y: f32 = 34.0;
const HEADING_PX: f32 = 19.0;
const TAGLINE_PX: f32 = 12.0;
const LEDE_PX: f32 = 12.0;
const CARD_RADIUS: f32 = 12.0;
const CARD_PAD: f32 = 18.0;
const FLEET_CARD_MIN: f32 = 300.0;
const FLEET_GAP: f32 = 16.0;
const THEME_CARD_MIN: f32 = 268.0;
const THEME_GAP: f32 = 18.0;
const HAIRLINE: f32 = 1.0;

fn baseline_in(y: f32, h: f32, px: f32) -> f32 {
    y + (h + px * 0.72) / 2.0
}

fn srgb(c: [u8; 3]) -> LinearRgba {
    LinearRgba::opaque(c[0], c[1], c[2])
}

/// A dashed 1px border, as the SDF pipeline cannot draw one: straight-edge
/// segments, 4 on / 4 off, corners left open. Placeholder-grade by design —
/// the handoff's dashed cards are empty/asleep states, not jewellery.
fn dashed_border(
    rects: &mut Vec<RectInstance>,
    rect: [f32; 4],
    s: f32,
    color: LinearRgba,
    clip: [f32; 4],
) {
    let dash = 4.0 * s;
    let t = HAIRLINE * s;
    let mut x = rect[0] + dash;
    while x + dash < rect[0] + rect[2] - dash {
        rects.push(RectInstance::filled([x, rect[1], dash, t], color, clip));
        rects.push(RectInstance::filled([x, rect[1] + rect[3] - t, dash, t], color, clip));
        x += 2.0 * dash;
    }
    let mut y = rect[1] + dash;
    while y + dash < rect[1] + rect[3] - dash {
        rects.push(RectInstance::filled([rect[0], y, t, dash], color, clip));
        rects.push(RectInstance::filled([rect[0] + rect[2] - t, y, t, dash], color, clip));
        y += 2.0 * dash;
    }
}

/// Column geometry for a `repeat(auto-fill, minmax(min, 1fr))` grid.
fn grid_columns(avail: f32, min: f32, gap: f32) -> (usize, f32) {
    let n = (((avail + gap) / (min + gap)).floor() as usize).max(1);
    let w = (avail - (n as f32 - 1.0) * gap) / n as f32;
    (n, w)
}

/// Draw the page frame; returns the y where content starts.
#[allow(clippy::too_many_arguments)]
fn page_frame(
    out: &mut ChromeLayout,
    area: [f32; 4],
    s: f32,
    colors: &ChromeColors,
    heading: &str,
    tagline: &str,
    lede: &str,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) -> f32 {
    // The ground *is* the window surface where it sits, exactly as the tab
    // strip is (#522): written rather than blended, so `window.chrome_opacity`
    // is an alpha onto whatever is behind the window instead of a tint toward
    // the window background. Blended it could only ever be the latter — the
    // clear is already `window.opacity`, and compositing onto it is ADR-017's
    // `1-(1-o)²`. Safe to *replace* because `pane_is_covered` builds no
    // viewport under a screen; a surface rect erases what it overlaps, so one
    // over a live grid would cut a hole in it.
    //
    // The swallow region below is unchanged and still earns its place: it stops
    // a click falling through to a region an earlier pass pushed. Which vec the
    // fill went into is not a fact `ChromeHitMap` knows — it is its own list,
    // ordered by call, and `hit` walks it in reverse.
    out.surface_rects.push(RectInstance::filled(area, colors.screen_bg, area));
    out.hit.push(area, HitRegion::ScreenPanel);

    let x = area[0] + PAD_X * s;
    let mut y = area[1] + PAD_Y * s;

    let hw = measure(heading, HEADING_PX * s, true, 0.0);
    y += HEADING_PX * s;
    out.texts.push(TextRun {
        text: heading.into(),
        pos: [x, y],
        max_width: hw + 2.0,
        color: colors.text_active,
        clip: area,
        px: HEADING_PX * s,
        bold: true,
        tracking: 0.0,
    });
    out.texts.push(TextRun {
        text: tagline.into(),
        pos: [x + hw + 14.0 * s, y],
        max_width: (area[0] + area[2] - x - hw - 14.0 * s - PAD_X * s).max(0.0),
        color: colors.text_faint,
        clip: area,
        px: TAGLINE_PX * s,
        bold: false,
        tracking: 0.0,
    });
    y += 20.0 * s;
    out.texts.push(TextRun {
        text: lede.into(),
        pos: [x, y],
        max_width: (640.0 * s).min(area[2] - 2.0 * PAD_X * s),
        color: colors.text_inactive,
        clip: area,
        px: LEDE_PX * s,
        bold: false,
        tracking: 0.0,
    });
    y + 26.0 * s
}

/// Returns the profiles editor's open-dropdown anchor, when that screen has
/// one — the #182 contract; the card screens float nothing.
pub fn screen_overlay(
    screen: &ScreenModel,
    area: [f32; 4],
    colors: &ChromeColors,
    hover: Option<HitRegion>,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) -> Option<[f32; 4]> {
    match screen {
        ScreenModel::Fleet { account, cards, devices } => {
            fleet(account, cards, devices.as_ref(), area, colors, s, measure, out);
            None
        }
        ScreenModel::Themes { cards, import_error } => {
            themes(cards, import_error.as_deref(), area, colors, s, measure, out);
            None
        }
        // Hover matters only here (the Delete button's danger tint); the
        // card screens keep the design's instant, hoverless surfaces.
        ScreenModel::Profiles(model) => super::profiles_screen::profiles_screen(
            model, area, colors, hover, s, measure, out,
        ),
    }
}

// Account header (issue #190), logical px.
const ACCOUNT_H: f32 = 32.0;
const ACCOUNT_BTN_H: f32 = 26.0;
const CODE_INPUT_W: f32 = 150.0;

/// The account header between the page frame and the cards: one line of
/// fact, at most one affordance, the code entry while one is open. Returns
/// the y where the cards start.
fn account_header(
    model: &FleetAccountModel,
    area: [f32; 4],
    top: f32,
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) -> f32 {
    // Nothing to say (the Unknown state): the cards keep the whole page
    // rather than leaving a blank band that reads as a broken header.
    if model.line.is_empty() && model.entry.is_none() && model.error.is_none() {
        return top;
    }
    let h = ACCOUNT_H * s;
    let x0 = area[0] + PAD_X * s;
    let px = 12.0 * s;
    let base = baseline_in(top, h, px);
    let lw = measure(&model.line, px, false, 0.0);
    out.texts.push(TextRun {
        text: model.line.clone(),
        pos: [x0, base],
        max_width: lw + 2.0,
        color: colors.text_active,
        clip: area,
        px,
        bold: false,
        tracking: 0.0,
    });
    let mut x = x0 + lw + 14.0 * s;

    if let Some(SettingsValueCell::Editing { buffer, caret, selection, error }) = &model.entry {
        // §11's editing input, sized for an 8-character code — drawn through
        // the settings tab's own entry, so the caret and selection are the
        // same ones everywhere.
        let boxr = [x, top + (h - ACCOUNT_BTN_H * s) / 2.0, CODE_INPUT_W * s, ACCOUNT_BTN_H * s];
        out.rects.push(RectInstance {
            radii: [8.0 * s; 4],
            border: if *error { colors.pill_warn_text } else { colors.accent },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(boxr, colors.panel_bg, area)
        });
        super::settings_screen::text_entry(
            &super::settings_screen::TextEntry {
                text: buffer,
                caret: *caret,
                selection: *selection,
                color: if *error { colors.pill_warn_text } else { colors.text_active },
                selection_bg: colors.accent_soft,
                px,
            },
            boxr,
            area,
            s,
            measure,
            out,
        );
        let hint = "Enter to sign in · Esc to cancel";
        out.texts.push(TextRun {
            text: hint.into(),
            pos: [boxr[0] + boxr[2] + 12.0 * s, base],
            max_width: (area[0] + area[2] - boxr[0] - boxr[2] - 12.0 * s - PAD_X * s).max(0.0),
            color: colors.text_faint,
            clip: area,
            px: 10.5 * s,
            bold: false,
            tracking: 0.0,
        });
    } else {
        // One verb per action, in the header's own button treatment. The
        // signed-out header draws two doors side by side (#226).
        let verb = |action: FleetAccountAction| match action {
            FleetAccountAction::None => None,
            FleetAccountAction::SignIn => Some(("Sign in with a code", HitRegion::FleetSignIn)),
            FleetAccountAction::SignInBrowser => {
                Some(("Sign in with browser", HitRegion::FleetLinkStart))
            }
            FleetAccountAction::CancelLink => Some(("Cancel", HitRegion::FleetLinkCancel)),
            FleetAccountAction::SignOut => Some(("Sign out", HitRegion::FleetSignOut)),
        };
        for (label, region) in [verb(model.action), verb(model.second)].into_iter().flatten() {
            let bpx = 11.0 * s;
            let tw = measure(label, bpx, false, 0.0);
            let rect = [x, top + (h - ACCOUNT_BTN_H * s) / 2.0, tw + 20.0 * s, ACCOUNT_BTN_H * s];
            out.rects.push(RectInstance {
                radii: [7.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(rect, colors.panel_bg, area)
            });
            out.hit.push(rect, region);
            out.texts.push(TextRun {
                text: label.to_string(),
                pos: [rect[0] + 10.0 * s, baseline_in(rect[1], rect[3], bpx)],
                max_width: tw + 2.0,
                color: colors.text_inactive,
                clip: area,
                px: bpx,
                bold: false,
                tracking: 0.0,
            });
            x = rect[0] + rect[2] + 10.0 * s;
        }
        if let Some(error) = &model.error {
            out.texts.push(TextRun {
                text: error.clone(),
                pos: [x, base],
                max_width: (area[0] + area[2] - x - PAD_X * s).max(0.0),
                color: colors.warn,
                clip: area,
                px: 11.0 * s,
                bold: false,
                tracking: 0.0,
            });
        }
    }
    top + h + 12.0 * s
}

// Devices section, logical px.
const DEVICE_ROW_H: f32 = 30.0;

/// The devices section (issue #190: the app as approver): hosted account
/// data under the host cards — a title, one row per key, a button carrying
/// whichever verb the row's state earns, and the last failure in warn ink.
fn devices_section(
    model: &FleetDevicesModel,
    area: [f32; 4],
    top: f32,
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let x0 = area[0] + PAD_X * s;
    let title_px = 13.0 * s;
    let mut y = top + title_px;
    let tw = measure("Devices", title_px, true, 0.0);
    out.texts.push(TextRun {
        text: "Devices".into(),
        pos: [x0, y],
        max_width: tw + 2.0,
        color: colors.text_active,
        clip: area,
        px: title_px,
        bold: true,
        tracking: 0.0,
    });
    out.texts.push(TextRun {
        text: "browsers and apps holding a key to this account".into(),
        pos: [x0 + tw + 12.0 * s, y],
        max_width: (area[0] + area[2] - x0 - tw - 12.0 * s - PAD_X * s).max(0.0),
        color: colors.text_faint,
        clip: area,
        px: 10.5 * s,
        bold: false,
        tracking: 0.0,
    });
    y += 8.0 * s;

    if let Some(error) = &model.error {
        let px = 11.0 * s;
        y += px + 4.0 * s;
        out.texts.push(TextRun {
            text: error.clone(),
            pos: [x0, y],
            max_width: (area[2] - 2.0 * PAD_X * s).max(0.0),
            color: colors.warn,
            clip: area,
            px,
            bold: false,
            tracking: 0.0,
        });
        y += 4.0 * s;
    }

    for (i, row) in model.rows.iter().enumerate() {
        let h = DEVICE_ROW_H * s;
        let px = 12.0 * s;
        let base = baseline_in(y, h, px);
        let lw = measure(&row.label, px, false, 0.0);
        out.texts.push(TextRun {
            text: row.label.clone(),
            pos: [x0, base],
            max_width: lw + 2.0,
            color: colors.text_active,
            clip: area,
            px,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: row.detail.clone(),
            pos: [x0 + lw + 12.0 * s, base],
            max_width: (area[2] * 0.5).max(0.0),
            color: colors.text_faint,
            clip: area,
            px: 11.0 * s,
            bold: false,
            tracking: 0.0,
        });

        // The account header's button treatment, verb from the row's state.
        let label = match row.action {
            FleetDeviceAction::None => None,
            FleetDeviceAction::Approve => Some("Approve"),
            FleetDeviceAction::Vouch => Some("Vouch"),
        };
        if let Some(label) = label {
            let bpx = 11.0 * s;
            let bw = measure(label, bpx, false, 0.0);
            let rect = [
                area[0] + area[2] - PAD_X * s - bw - 20.0 * s,
                y + (h - ACCOUNT_BTN_H * s) / 2.0,
                bw + 20.0 * s,
                ACCOUNT_BTN_H * s,
            ];
            out.rects.push(RectInstance {
                radii: [7.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(rect, colors.panel_bg, area)
            });
            out.hit.push(rect, HitRegion::FleetApproveDevice(i));
            out.texts.push(TextRun {
                text: label.to_string(),
                pos: [rect[0] + 10.0 * s, baseline_in(rect[1], rect[3], bpx)],
                max_width: bw + 2.0,
                color: colors.text_inactive,
                clip: area,
                px: bpx,
                bold: false,
                tracking: 0.0,
            });
        }
        y += h;
    }
}

#[allow(clippy::too_many_arguments, reason = "one screen's worth of model, not a seam")]
fn fleet(
    account: &FleetAccountModel,
    cards: &[FleetCard],
    devices: Option<&FleetDevicesModel>,
    area: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let top = page_frame(
        out,
        area,
        s,
        colors,
        "Your fleet",
        "every machine is a host · every window, tab and phone is a client",
        "The directory knows which machines exist and how to reach them. \
         Sessions never leave the machine they run on.",
        measure,
    );
    let top = account_header(account, area, top, colors, s, measure, out);

    let x0 = area[0] + PAD_X * s;
    let avail = area[2] - 2.0 * PAD_X * s;
    let (ncols, card_w) = grid_columns(avail, FLEET_CARD_MIN * s, FLEET_GAP * s);

    // Card height is uniform, and now sized to the *tallest* card rather than
    // to a constant four rows (#287): session rows made the old constant clip.
    // Uniform rather than masonry because the grid is a plain row/column
    // walk and the devices section below needs to know where it ends — and
    // because a grid of ragged cards reads worse than a grid of even ones.
    let tallest = cards
        .iter()
        .map(|c| c.rows.len() + c.sessions.len() + usize::from(c.sessions_hidden > 0))
        .max()
        .unwrap_or(0)
        .max(4);
    let card_h = (46.0 + tallest as f32 * 18.0 + 14.0) * s;
    let card_rows = cards.len().div_ceil(ncols.max(1));
    let cards_end = top + card_rows as f32 * (card_h + FLEET_GAP * s);
    if let Some(devices) = devices {
        devices_section(devices, area, cards_end + 10.0 * s, colors, s, measure, out);
    }

    for (i, card) in cards.iter().enumerate() {
        let col = i % ncols;
        let row = i / ncols;
        let cx = x0 + col as f32 * (card_w + FLEET_GAP * s);
        let cy = top + row as f32 * (card_h + FLEET_GAP * s);
        let rect = [cx, cy, card_w, card_h];

        if card.online {
            out.rects.push(RectInstance {
                radii: [CARD_RADIUS * s; 4],
                border: if card.local { colors.accent } else { colors.line },
                border_width: HAIRLINE * s,
                ..RectInstance::filled(rect, colors.panel_bg, area)
            });
        } else {
            out.rects.push(RectInstance::rounded(
                rect,
                CARD_RADIUS * s,
                colors.block_header_bg,
                area,
            ));
            dashed_border(&mut out.rects, rect, s, colors.line, area);
        }
        // Only routable cards answer to the pointer (the ThemeCard shape):
        // a card with no way to open a shell must not offer to.
        if card.open {
            out.hit.push(rect, HitRegion::FleetCard(i));
        }

        // Header: dot, name, note or pill.
        let hx = cx + CARD_PAD * s;
        let hy = cy + CARD_PAD * s;
        let dot_d = 8.0 * s;
        let dot_color = if card.online { colors.success } else { colors.text_faint };
        out.rects.push(RectInstance::rounded(
            [hx, hy + 3.0 * s, dot_d, dot_d],
            dot_d / 2.0,
            dot_color,
            area,
        ));
        let name_px = 15.0 * s;
        let name_color = if card.online { colors.text_active } else { colors.text_inactive };
        let nw = measure(&card.name, name_px, true, 0.0);
        out.texts.push(TextRun {
            text: card.name.clone(),
            pos: [hx + dot_d + 9.0 * s, hy + 11.0 * s],
            max_width: nw + 2.0,
            color: name_color,
            clip: area,
            px: name_px,
            bold: true,
            tracking: 0.0,
        });
        let note = if card.local {
            Some(("this machine".to_string(), colors.text_faint, false))
        } else if !card.online {
            Some(("asleep".to_string(), colors.text_faint, false))
        } else {
            card.pill.clone().map(|p| (p, colors.pill_warn_text, true))
        };
        if let Some((text, color, pill)) = note {
            let px = 10.5 * s;
            let tx = hx + dot_d + 9.0 * s + nw + 9.0 * s;
            if pill {
                let tw = measure(&text, px, false, 0.0);
                out.rects.push(RectInstance::rounded(
                    [tx - 7.0 * s + 0.0, hy + 2.0 * s, tw + 14.0 * s, 16.0 * s],
                    5.0 * s,
                    colors.pill_warn_bg,
                    area,
                ));
            }
            out.texts.push(TextRun {
                text,
                pos: [tx, hy + 11.0 * s],
                max_width: card_w,
                color,
                clip: area,
                px,
                bold: false,
                tracking: 0.0,
            });
        }

        // The enroll button (issue #227), right-aligned in the header band —
        // the devices rows' button treatment. Pushed after the card's own
        // region, so it outranks the card underneath it; not pushed at all
        // while the worker is in flight, because a button that answers and
        // does nothing teaches double-clicking.
        if let Some(enroll) = &card.enroll {
            let bpx = 11.0 * s;
            let bw = measure(&enroll.label, bpx, false, 0.0);
            let brect = [
                cx + card_w - CARD_PAD * s - bw - 20.0 * s,
                hy,
                bw + 20.0 * s,
                ACCOUNT_BTN_H * s,
            ];
            out.rects.push(RectInstance {
                radii: [7.0 * s; 4],
                border: if enroll.clickable { colors.accent } else { colors.line },
                border_width: HAIRLINE * s,
                ..RectInstance::filled(brect, colors.panel_bg, area)
            });
            if enroll.clickable {
                out.hit.push(brect, HitRegion::FleetEnrollLocal);
            }
            out.texts.push(TextRun {
                text: enroll.label.clone(),
                pos: [brect[0] + 10.0 * s, baseline_in(brect[1], brect[3], bpx)],
                max_width: bw + 2.0,
                color: if enroll.clickable { colors.accent } else { colors.text_faint },
                clip: area,
                px: bpx,
                bold: false,
                tracking: 0.0,
            });
        }

        // Label/value rows.
        let mut ry = hy + 32.0 * s;
        for (label, value, role) in &card.rows {
            let px = 11.5 * s;
            out.texts.push(TextRun {
                text: label.clone(),
                pos: [hx, ry + px],
                max_width: card_w * 0.4,
                color: colors.text_faint,
                clip: area,
                px,
                bold: false,
                tracking: 0.0,
            });
            let value_color = match role {
                1 => colors.success,
                2 => colors.warn,
                _ if !card.online => colors.text_faint,
                _ => colors.text_inactive,
            };
            let vw = measure(value, px, false, 0.0);
            out.texts.push(TextRun {
                text: value.clone(),
                pos: [cx + card_w - CARD_PAD * s - vw, ry + px],
                max_width: vw + 2.0,
                color: value_color,
                clip: area,
                px,
                bold: false,
                tracking: 0.0,
            });
            ry += 18.0 * s;
        }

        // What is running there (#287). The ⌘K picker could attach to a
        // remote session before the screen that exists to show you the fleet
        // could; these rows close that.
        for (j, session) in card.sessions.iter().enumerate() {
            let row = [hx, ry, card_w - 2.0 * CARD_PAD * s, 18.0 * s];
            // No hover wash: this screen draws none on the cards either, and
            // adding one here alone would make a session row look like the
            // only live thing on a card that is entirely clickable.
            //
            // Same gate as the card itself: no route, no click. A session row
            // that must fail to dial is the affordance rule inverted.
            if card.open {
                if let Some(hit) = intersect(row, area) {
                    out.hit.push(hit, HitRegion::FleetSession(i, j));
                }
            }

            let px = 11.5 * s;
            // A dot that says whether anyone is looking at it: `here` is this
            // window, `attached` is somebody. Colour is the *glance*, never the
            // fact — the title carries "this window" in words, exactly as the
            // ⌘K picker's session rows do, so the state survives a reader who
            // cannot tell the accent from the success colour.
            let dot_d = 5.0 * s;
            let ink = if session.here {
                colors.accent
            } else if session.attached {
                colors.success
            } else {
                colors.text_faint
            };
            out.rects.push(RectInstance::rounded(
                [hx, ry + px / 2.0, dot_d, dot_d],
                dot_d / 2.0,
                ink,
                area,
            ));

            let tx = hx + dot_d + 7.0 * s;
            let title = if session.here {
                format!("{} \u{b7} this window", session.title)
            } else {
                session.title.clone()
            };
            let tw = measure(&title, px, false, 0.0);
            out.texts.push(TextRun {
                text: title,
                pos: [tx, ry + px],
                max_width: (card_w * 0.45).min(tw + 2.0),
                color: if card.online { colors.text_inactive } else { colors.text_faint },
                clip: area,
                px,
                bold: false,
                tracking: 0.0,
            });
            if !session.detail.is_empty() {
                let dw = measure(&session.detail, px, false, 0.0);
                out.texts.push(TextRun {
                    text: session.detail.clone(),
                    pos: [cx + card_w - CARD_PAD * s - dw, ry + px],
                    max_width: dw + 2.0,
                    color: colors.text_faint,
                    clip: area,
                    px,
                    bold: false,
                    tracking: 0.0,
                });
            }
            ry += 18.0 * s;
        }
        if card.sessions_hidden > 0 {
            // The cap, said out loud, pointing at the surface that holds them
            // all. A card is a summary; ⌘K is the inventory.
            let px = 11.0 * s;
            out.texts.push(TextRun {
                text: format!("+{} more \u{b7} \u{2318}K", card.sessions_hidden),
                pos: [hx, ry + px],
                max_width: card_w - 2.0 * CARD_PAD * s,
                color: colors.text_faint,
                clip: area,
                px,
                bold: false,
                tracking: 0.0,
            });
        }
    }
}

fn themes(
    cards: &[ThemeCard],
    import_error: Option<&str>,
    area: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let top = page_frame(
        out,
        area,
        s,
        colors,
        "Themes",
        "one file styles the chrome and any TUI running inside it",
        "Brights are derived in OKLCH, never hand-authored, so no bright \
         drifts out of its theme's colour family.",
        measure,
    );

    let x0 = area[0] + PAD_X * s;
    let avail = area[2] - 2.0 * PAD_X * s;
    let (ncols, card_w) = grid_columns(avail, THEME_CARD_MIN * s, THEME_GAP * s);

    let preview_h = (14.0 * 2.0 + 3.0 * 11.0 * 1.6) * s;
    let swatch_h = 10.0 * s;
    let footer_h = 34.0 * s;
    let card_h = preview_h + swatch_h + footer_h;

    // The trailing "+1" is the import card.
    for i in 0..=cards.len() {
        let col = i % ncols;
        let row = i / ncols;
        let cx = x0 + col as f32 * (card_w + THEME_GAP * s);
        let cy = top + row as f32 * (card_h + THEME_GAP * s);
        let rect = [cx, cy, card_w, card_h];

        let Some(card) = cards.get(i) else {
            // Import target: dashed, and honest about the formats. Live
            // since #147 landed its parsers — clicking imports the scheme
            // the clipboard holds, so the whole card is one hit region.
            out.rects.push(RectInstance::rounded(
                rect,
                CARD_RADIUS * s,
                LinearRgba::TRANSPARENT,
                area,
            ));
            dashed_border(&mut out.rects, rect, s, colors.line, area);
            out.hit.push(rect, HitRegion::ThemeImport);
            let line1 = "Import a scheme";
            let w1 = measure(line1, 12.5 * s, false, 0.0);
            out.texts.push(TextRun {
                text: line1.into(),
                pos: [cx + (card_w - w1) / 2.0, cy + card_h / 2.0 - 26.0 * s],
                max_width: w1 + 2.0,
                color: colors.text_inactive,
                clip: area,
                px: 12.5 * s,
                bold: false,
                tracking: 0.0,
            });
            // The design's two-line format list — split, not truncated: a
            // card advertising "bas…" advertises nothing.
            for (row, line) in
                [".itermcolors · Windows Terminal", "base16 / base24 · Alacritty TOML"]
                    .into_iter()
                    .enumerate()
            {
                let w = measure(line, 11.0 * s, false, 0.0).min(card_w - 16.0 * s);
                out.texts.push(TextRun {
                    text: line.into(),
                    pos: [
                        cx + ((card_w - w) / 2.0).max(8.0 * s),
                        cy + card_h / 2.0 + (-6.0 + 16.0 * row as f32) * s,
                    ],
                    max_width: w,
                    color: colors.text_faint,
                    clip: area,
                    px: 11.0 * s,
                    bold: false,
                    tracking: 0.0,
                });
            }
            // The last line teaches the gesture — or says why the last
            // attempt was refused, in its place: two messages at once would
            // fight for a card that only has room to say one thing.
            let (last, color) = match import_error {
                Some(e) => (e, colors.danger),
                None => ("copy a scheme file, then click here", colors.text_faint),
            };
            let w3 = measure(last, 10.5 * s, false, 0.0).min(card_w - 16.0 * s);
            out.texts.push(TextRun {
                text: last.into(),
                pos: [cx + ((card_w - w3) / 2.0).max(8.0 * s), cy + card_h / 2.0 + 30.0 * s],
                max_width: w3,
                color,
                clip: area,
                px: 10.5 * s,
                bold: false,
                tracking: 0.0,
            });
            break;
        };

        // Card border; the active theme gets the accent.
        out.rects.push(RectInstance {
            radii: [CARD_RADIUS * s; 4],
            border: if card.active { colors.accent } else { colors.line },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(rect, srgb(card.bg), area)
        });
        out.hit.push(rect, HitRegion::ThemeCard(i));

        // The live preview, in that theme's own colours.
        let mono = 11.0 * s;
        let lh = mono * 1.6;
        let px0 = cx + 14.0 * s;
        let mut py = cy + 14.0 * s + mono;
        out.texts.push(TextRun {
            text: format!("\u{276f} zesterm --theme {}", card.id),
            pos: [px0, py],
            max_width: card_w - 28.0 * s,
            color: srgb(card.fg),
            clip: area,
            px: mono,
            bold: false,
            tracking: 0.0,
        });
        py += lh;
        let okw = measure("ok", mono, false, 0.0);
        out.texts.push(TextRun {
            text: "ok".into(),
            pos: [px0, py],
            max_width: okw + 2.0,
            color: srgb(card.green),
            clip: area,
            px: mono,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: " · schema 1 · 24 tokens".into(),
            pos: [px0 + okw, py],
            max_width: card_w - 28.0 * s - okw,
            color: srgb(card.fg),
            clip: area,
            px: mono,
            bold: false,
            tracking: 0.0,
        });
        py += lh;
        let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
        let aw = measure("accent ", mono, false, 0.0);
        let ahw = measure(&format!("{}  ", hex(card.accent)), mono, false, 0.0);
        let dw = measure("danger ", mono, false, 0.0);
        for (text, color, xoff) in [
            ("accent".to_string(), srgb(card.accent), 0.0),
            (format!(" {}  ", hex(card.accent)), srgb(card.fg), aw),
            ("danger".to_string(), srgb(card.danger), aw + ahw),
            (format!(" {}", hex(card.danger)), srgb(card.fg), aw + ahw + dw),
        ] {
            out.texts.push(TextRun {
                text,
                pos: [px0 + xoff, py],
                max_width: card_w - 28.0 * s - xoff,
                color,
                clip: area,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
        }

        // The swatch strip: the normal ANSI row, index order, no gaps.
        let sy = cy + preview_h;
        let sw = card_w / 8.0;
        for (j, c) in card.ansi.iter().enumerate() {
            out.rects.push(RectInstance::filled(
                [cx + j as f32 * sw, sy, sw + 0.5, swatch_h],
                srgb(*c),
                area,
            ));
        }

        // Footer band, in the UI's own colours (it is chrome, not preview).
        let fy = sy + swatch_h;
        out.rects.push(RectInstance {
            radii: [0.0, 0.0, CARD_RADIUS * s, CARD_RADIUS * s],
            ..RectInstance::filled([cx, fy, card_w, footer_h], colors.panel_bg, area)
        });
        let name_px = 12.5 * s;
        let base = baseline_in(fy, footer_h, name_px);
        let nw = measure(&card.name, name_px, false, 0.0);
        out.texts.push(TextRun {
            text: card.name.clone(),
            pos: [cx + 12.0 * s, base],
            max_width: nw + 2.0,
            color: colors.text_active,
            clip: area,
            px: name_px,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: card.qualifier.clone(),
            pos: [cx + 12.0 * s + nw + 8.0 * s, base],
            max_width: card_w * 0.5,
            color: colors.text_faint,
            clip: area,
            px: 10.5 * s,
            bold: false,
            tracking: 0.0,
        });
        if card.active {
            let active_w = measure("active", 11.0 * s, false, 0.0);
            out.texts.push(TextRun {
                text: "active".into(),
                pos: [cx + card_w - 12.0 * s - active_w, base],
                max_width: active_w + 2.0,
                color: colors.accent,
                clip: area,
                px: 11.0 * s,
                bold: false,
                tracking: 0.0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::FleetSessionRow;
    use crate::chrome::model::ThemeCard;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0, 1.0)
    }

    fn measure(s: &str, px: f32, _b: bool, _t: f32) -> f32 {
        s.chars().count() as f32 * px * 0.6
    }

    /// The chrome at `chrome_opacity`, the window opaque — the pair that tells
    /// a ground following the *chrome* from one following the window.
    fn colors_at(chrome_opacity: f32) -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, chrome_opacity, 1.0)
    }

    #[test]
    fn the_ground_is_the_window_surface_at_chrome_opacity() {
        // #538, per screen: each of the three grounds is its own copy of this
        // line, so a test through one of them proves nothing about the others.
        // Both halves matter and neither is enough — a translucent fill left in
        // `rects` can only tint toward the window background (ADR-017), and a
        // surface rect at alpha 1 is the opaque slab this replaced.
        let mut out = ChromeLayout::default();
        let area = [8.0, 46.0, 1200.0, 700.0];
        screen_overlay(
            &ScreenModel::Themes { cards: Vec::new(), import_error: None },
            area,
            &colors_at(0.4),
            None,
            1.0,
            &mut measure,
            &mut out,
        );

        let ground = out
            .surface_rects
            .iter()
            .find(|r| r.rect == area)
            .expect("the screen grounds the whole pane, and does it as a surface");
        assert!(
            (ground.fill.0[3] - 0.4).abs() < 1e-6,
            "the ground carries chrome_opacity verbatim, got {:?}",
            ground.fill
        );
        assert!(
            !out.rects.iter().any(|r| r.rect == area),
            "a whole-pane fill left in the blended layer paints the glass back to opaque"
        );
        assert_eq!(
            out.hit.hit(area[0] + 2.0, area[1] + 2.0),
            Some(HitRegion::ScreenPanel),
            "moving the fill must not move the swallow region"
        );
    }

    #[test]
    fn theme_cards_are_clickable_and_the_screen_swallows_the_rest() {
        let cards: Vec<ThemeCard> = zest_theme::builtin::all()
            .into_iter()
            .map(|t| ThemeCard {
                id: t.id.clone(),
                name: t.name.clone(),
                qualifier: "dark".into(),
                bg: [t.ui.bg.r, t.ui.bg.g, t.ui.bg.b],
                fg: [t.ui.fg.r, t.ui.fg.g, t.ui.fg.b],
                accent: [t.ui.accent.r, t.ui.accent.g, t.ui.accent.b],
                danger: [t.ui.danger.r, t.ui.danger.g, t.ui.danger.b],
                green: [t.ui.green.r, t.ui.green.g, t.ui.green.b],
                ansi: [[0, 0, 0]; 8],
                active: t.id == "obsidian",
            })
            .collect();
        let n = cards.len();
        let mut out = ChromeLayout::default();
        let area = [0.0, 46.0, 1200.0, 700.0];
        screen_overlay(
            &ScreenModel::Themes { cards, import_error: None },
            area,
            &colors(),
            None,
            1.0,
            &mut measure,
            &mut out,
        );

        let mut seen = std::collections::HashSet::new();
        let mut seen_import = false;
        for x in (0..1200).step_by(4) {
            for y in (46..746).step_by(4) {
                match out.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::ThemeCard(i)) => {
                        seen.insert(i);
                    }
                    Some(HitRegion::ThemeImport) => seen_import = true,
                    Some(HitRegion::ScreenPanel) => {}
                    other => panic!("({x},{y}) escaped the screen: {other:?}"),
                }
            }
        }
        assert_eq!(seen.len(), n, "every theme card must answer as itself");
        // The dashed card shipped drawn-but-dead for a while; a hit region
        // is what separates an import target from decoration (#147).
        assert!(seen_import, "the import card must be clickable");
    }

    /// A card carrying `sessions` sessions and hiding `hidden` more.
    fn card_with(open: bool, sessions: usize, hidden: usize) -> FleetCard {
        named_card("forge", open, sessions, hidden)
    }

    fn named_card(name: &str, open: bool, sessions: usize, hidden: usize) -> FleetCard {
        FleetCard {
            name: name.into(),
            local: false,
            online: true,
            pill: None,
            open,
            rows: vec![
                ("os".into(), "Linux 6.8.0-31-generic".into(), 0),
                ("key".into(), "1f2a3b4c".into(), 0),
            ],
            enroll: None,
            sessions: (0..sessions)
                .map(|i| FleetSessionRow {
                    title: format!("shell{i}"),
                    detail: "/src".into(),
                    attached: i == 0,
                    here: false,
                })
                .collect(),
            sessions_hidden: hidden,
        }
    }

    fn fleet_layout(cards: Vec<FleetCard>) -> ([f32; 4], ChromeLayout) {
        let area = [0.0, 46.0, 1200.0, 700.0];
        let mut out = ChromeLayout::default();
        screen_overlay(
            &ScreenModel::Fleet {
                account: FleetAccountModel {
                    line: "signed in as andy".into(),
                    action: FleetAccountAction::SignOut,
                    second: FleetAccountAction::None,
                    entry: None,
                    error: None,
                },
                cards,
                devices: None,
            },
            area,
            &colors(),
            None,
            1.0,
            &mut measure,
            &mut out,
        );
        (area, out)
    }

    #[test]
    fn a_cards_sessions_are_drawn_and_answer_as_themselves() {
        // #287: the ⌘K picker could attach to a remote session before the
        // screen that exists to *show you the fleet* could.
        let (_, out) = fleet_layout(vec![card_with(true, 3, 0)]);

        for i in 0..3 {
            assert!(
                out.texts.iter().any(|t| t.text == format!("shell{i}")),
                "session {i} is drawn"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for x in (0..1200).step_by(2) {
            for y in (46..746).step_by(2) {
                if let Some(HitRegion::FleetSession(c, j)) = out.hit.hit(x as f32, y as f32) {
                    assert_eq!(c, 0, "one card");
                    seen.insert(j);
                }
            }
        }
        assert_eq!(seen.len(), 3, "each session answers as itself: {seen:?}");
    }

    #[test]
    fn a_session_this_window_holds_says_so_in_words() {
        // The dot is the glance; the words are the fact. Colour alone would
        // put "is this the tab I already have open" out of reach of a reader
        // who cannot tell the accent from the success colour — and the ⌘K
        // picker already spells it out, so two surfaces showing one state must
        // not disagree about how.
        let mut card = card_with(true, 2, 0);
        card.sessions[0].here = true;
        let (_, out) = fleet_layout(vec![card]);
        assert!(
            out.texts.iter().any(|t| t.text == "shell0 · this window"),
            "the held session names itself in words: {:?}",
            out.texts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        assert!(
            out.texts.iter().any(|t| t.text == "shell1"),
            "and one nobody here holds is just its title"
        );
    }

    #[test]
    fn sessions_on_an_unroutable_card_are_drawn_and_take_no_hit_region() {
        // The affordance rule the card itself already obeys: no route, no
        // click. Drawn anyway, because "what is running over there" is worth
        // knowing even when we cannot reach it right now — it is a fact, not
        // a button.
        let (_, out) = fleet_layout(vec![card_with(false, 2, 0)]);
        assert!(
            out.texts.iter().any(|t| t.text == "shell0"),
            "the sessions are still drawn — they are facts about the machine"
        );
        for x in (0..1200).step_by(2) {
            for y in (46..746).step_by(2) {
                assert!(
                    !matches!(out.hit.hit(x as f32, y as f32), Some(HitRegion::FleetSession(..))),
                    "an unroutable card must offer no session to click"
                );
            }
        }
    }

    #[test]
    fn a_capped_session_list_says_how_many_it_left_out() {
        // The grid is uniform-height, so a machine running thirty shells
        // would make every card thirty rows tall. A cap nobody is told about
        // is a card that quietly lies about what is running.
        let (_, out) = fleet_layout(vec![card_with(true, 4, 26)]);
        assert!(
            out.texts.iter().any(|t| t.text.starts_with("+26 more")),
            "the overflow is stated, and points at ⌘K: {:?}",
            out.texts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_card_grid_grows_to_fit_its_tallest_card() {
        // Card height was a constant four rows and session rows made it clip.
        //
        // **Asserting "more text is drawn lower down" does not catch that**,
        // which is how the first version of this test passed against the very
        // constant it was meant to fail: rows clip to the *page*, not to the
        // card, so a card too short for its contents does not truncate — it
        // spills over whatever the grid puts underneath it. Everything is
        // still drawn and still findable.
        //
        // So the assertion has to be about the grid: with enough cards to wrap
        // to a second row, a card on that second row must begin *below* the
        // last session of the card above it. That is false exactly when the
        // height is a constant.
        let mut cards = vec![named_card("tall", true, 6, 0)];
        for i in 1..6 {
            cards.push(named_card(&format!("m{i}"), true, 0, 0));
        }
        let (area, out) = fleet_layout(cards);
        let (ncols, _) = grid_columns(area[2] - 2.0 * PAD_X, FLEET_CARD_MIN, FLEET_GAP);
        assert!(ncols < 6, "precondition: six cards must wrap, got {ncols} columns");

        let y_of = |text: &str| {
            out.texts
                .iter()
                .find(|t| t.text == text)
                .unwrap_or_else(|| panic!("{text} is drawn"))
                .pos[1]
        };
        let last_session = y_of("shell5");
        let second_row = y_of(&format!("m{ncols}"));
        assert!(
            second_row > last_session,
            "a second-row card must start below the first row's deepest content — \
             card at m{ncols} is at {second_row}, the tall card's last session at {last_session}"
        );
    }

    #[test]
    fn the_enroll_button_answers_only_while_clickable() {
        // Issue #227: the local card offers "Enroll this machine". Clickable,
        // it must answer as itself and outrank the card underneath (a click
        // that opened a shell instead of enrolling would be a bad surprise);
        // in flight, it must not answer at all — a button that answers and
        // does nothing teaches double-clicking.
        use crate::chrome::model::{FleetAccountAction, FleetAccountModel, FleetEnroll};
        let account = FleetAccountModel {
            line: "signed in as andy".into(),
            action: FleetAccountAction::SignOut,
            second: FleetAccountAction::None,
            entry: None,
            error: None,
        };
        let card = |clickable: bool| FleetCard {
            name: "studio".into(),
            local: true,
            online: true,
            pill: None,
            open: true,
            rows: vec![("key".into(), "1f2a3b4c".into(), 0)],
            enroll: Some(FleetEnroll {
                label: if clickable { "Enroll this machine" } else { "enrolling…" }.into(),
                clickable,
            }),
            sessions: Vec::new(),
            sessions_hidden: 0,
        };
        let area = [0.0, 46.0, 1200.0, 700.0];

        let hits = |clickable: bool| {
            let mut out = ChromeLayout::default();
            screen_overlay(
                &ScreenModel::Fleet {
                    account: account.clone(),
                    cards: vec![card(clickable)],
                    devices: None,
                },
                area,
                &colors(),
                None,
                1.0,
                &mut measure,
                &mut out,
            );
            let mut found = None;
            for x in (0..1200).step_by(2) {
                for y in (46..746).step_by(2) {
                    if out.hit.hit(x as f32, y as f32) == Some(HitRegion::FleetEnrollLocal) {
                        found = Some((x as f32, y as f32));
                    }
                }
            }
            (found, out)
        };

        let (found, out) = hits(true);
        let (bx, by) = found.expect("the clickable button must answer somewhere");
        assert!(
            out.texts.iter().any(|t| t.text == "Enroll this machine"),
            "…and carry its caption"
        );
        // The card is `open: true`, so everywhere else on it still opens a
        // shell — the button is an island, not a takeover.
        assert_eq!(
            out.hit.hit(bx, by + 60.0),
            Some(HitRegion::FleetCard(0)),
            "below the button the card still answers as the card"
        );

        let (found, _) = hits(false);
        assert!(
            found.is_none(),
            "an in-flight button must not answer — the worker is already going"
        );
    }

    #[test]
    fn the_swatch_strip_is_the_ansi_row_in_index_order() {
        // The design reads the strip from builtin.rs; re-typing it is the
        // drift this test forbids. Obsidian's row is asserted exactly.
        let t = zest_theme::builtin::obsidian();
        let normal = t.ansi.normal.expect("builtins always carry the row");
        assert_eq!(
            normal.map(|c| (c.r, c.g, c.b))[1],
            (0xe0, 0x60, 0x6a),
            "index 1 is red, per the gallery strip"
        );
    }
}
