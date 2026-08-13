//! Full-pane screens: the fleet directory and the theme gallery
//! (design screens 7 and 8).
//!
//! Both share one page frame — heading, tagline, lede, then a card grid —
//! drawn opaquely over the grid area. They live in the cached chrome layout
//! (event-driven, never per-frame): a fleet card changes when the fleet
//! does, and the fleet already posts a wakeup for that.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::HitRegion;
use super::layout::{ChromeLayout, TextRun};
use super::model::{
    FleetAccountAction, FleetAccountModel, FleetCard, ScreenModel, SettingsValueCell, ThemeCard,
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
    // Opaque ground — deliberately not the chrome-opacity fill: the grid is
    // still underneath, and a screen is a screen, not a scrim.
    out.rects.push(RectInstance::filled(area, colors.bg_opaque, area));
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
        ScreenModel::Fleet { account, cards } => {
            fleet(account, cards, area, colors, s, measure, out);
            None
        }
        ScreenModel::Themes { cards } => {
            themes(cards, area, colors, s, measure, out);
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

    if let Some(SettingsValueCell::Editing { buffer, error }) = &model.entry {
        // §11's editing input, sized for an 8-character code; the caret is a
        // character, exactly as the settings tab draws it.
        let boxr = [x, top + (h - ACCOUNT_BTN_H * s) / 2.0, CODE_INPUT_W * s, ACCOUNT_BTN_H * s];
        out.rects.push(RectInstance {
            radii: [8.0 * s; 4],
            border: if *error { colors.pill_warn_text } else { colors.accent },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(boxr, colors.panel_bg, area)
        });
        let text = format!("{buffer}\u{258f}");
        let vw = measure(&text, px, false, 0.0);
        out.texts.push(TextRun {
            text,
            pos: [boxr[0] + 10.0 * s, baseline_in(boxr[1], boxr[3], px)],
            max_width: vw.max(2.0),
            color: if *error { colors.pill_warn_text } else { colors.text_active },
            clip: boxr,
            px,
            bold: false,
            tracking: 0.0,
        });
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
        let button = match model.action {
            FleetAccountAction::None => None,
            FleetAccountAction::SignIn => Some(("Sign in with a code", HitRegion::FleetSignIn)),
            FleetAccountAction::SignOut => Some(("Sign out", HitRegion::FleetSignOut)),
        };
        if let Some((label, region)) = button {
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
            x = rect[0] + rect[2] + 14.0 * s;
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

fn fleet(
    account: &FleetAccountModel,
    cards: &[FleetCard],
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

    for (i, card) in cards.iter().enumerate() {
        let col = i % ncols;
        let row = i / ncols;
        // Row height is uniform: enough for a header and four rows. Uneven
        // heights would need a measure pass nothing else wants yet.
        let card_h = (46.0 + 4.0 * 18.0 + 14.0) * s;
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
    }
}

fn themes(
    cards: &[ThemeCard],
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
            // Import target: dashed, and honest about the formats.
            out.rects.push(RectInstance::rounded(
                rect,
                CARD_RADIUS * s,
                LinearRgba::TRANSPARENT,
                area,
            ));
            dashed_border(&mut out.rects, rect, s, colors.line, area);
            let line1 = "Import a scheme";
            let w1 = measure(line1, 12.5 * s, false, 0.0);
            out.texts.push(TextRun {
                text: line1.into(),
                pos: [cx + (card_w - w1) / 2.0, cy + card_h / 2.0 - 6.0 * s],
                max_width: w1 + 2.0,
                color: colors.text_inactive,
                clip: area,
                px: 12.5 * s,
                bold: false,
                tracking: 0.0,
            });
            let line2 = ".itermcolors · Windows Terminal · base16 · Alacritty TOML";
            let w2 = measure(line2, 11.0 * s, false, 0.0).min(card_w - 16.0 * s);
            out.texts.push(TextRun {
                text: line2.into(),
                pos: [cx + ((card_w - w2) / 2.0).max(8.0 * s), cy + card_h / 2.0 + 12.0 * s],
                max_width: w2,
                color: colors.text_faint,
                clip: area,
                px: 11.0 * s,
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
    use crate::chrome::model::ThemeCard;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0)
    }

    fn measure(s: &str, px: f32, _b: bool, _t: f32) -> f32 {
        s.chars().count() as f32 * px * 0.6
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
            &ScreenModel::Themes { cards },
            area,
            &colors(),
            None,
            1.0,
            &mut measure,
            &mut out,
        );

        let mut seen = std::collections::HashSet::new();
        for x in (0..1200).step_by(4) {
            for y in (46..746).step_by(4) {
                match out.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::ThemeCard(i)) => {
                        seen.insert(i);
                    }
                    Some(HitRegion::ScreenPanel) => {}
                    other => panic!("({x},{y}) escaped the screen: {other:?}"),
                }
            }
        }
        assert_eq!(seen.len(), n, "every theme card must answer as itself");
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
