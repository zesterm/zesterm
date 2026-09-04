//! The Profiles tab's screen (design §12): a 248px profile rail and an
//! editor column, drawn over the grid area while the Profiles pane is up.
//!
//! Sibling of `settings_screen`, same discipline — pure, event-driven; rects,
//! text runs and hit regions out of one pass — and it *borrows* that module's
//! widget vocabulary (`draw_control`, `row_extent`, the dropdown) rather than
//! redrawing it: the §11 controls and the §12 controls are one vocabulary.
//! What is new here is §12's own chrome: the rail of launch targets, the
//! editor header, the live preview, and the inheritance chips.

use zest_render_wgpu::{LinearRgba, RectInstance};

use super::hit::HitRegion;
use super::layout::{accent_color, washed};
use super::layout::{ChromeLayout, TextRun};
use super::model::{InheritChip, ProfilePreviewModel, ProfilesScreenModel, SettingsRowModel};
use super::settings_screen as ss;
use super::theme::ChromeColors;

// §12 geometry, logical px (docs/design/client-ui/README.md §12) — change
// them there first or not at all.
pub const RAIL_W: f32 = 248.0;
const RAIL_PAD: f32 = 12.0;
const RAIL_ROW_H: f32 = 44.0;
const RAIL_TILE: f32 = 24.0;
/// The 6px gap after Defaults, so it reads as the parent, not a sibling.
const DEFAULTS_GAP: f32 = 6.0;
const HEAD_TILE: f32 = 34.0;
const BTN_H: f32 = 26.0;
/// The header name's click target and its rename entry share a height, so the
/// box a click opens is exactly the box the pointer was over (#283).
const NAME_INPUT_H: f32 = 26.0;
const HAIRLINE: f32 = 1.0;
const SECTION_PX: f32 = 10.5;

fn srgb(c: [u8; 3]) -> LinearRgba {
    LinearRgba::opaque(c[0], c[1], c[2])
}

/// A profile's glyph tile: the icon in its accent on a 12%-alpha wash, or
/// the placeholder dot the tab chips draw while a profile has no icon.
#[allow(clippy::too_many_arguments)]
fn glyph_tile(
    out: &mut ChromeLayout,
    rect: [f32; 4],
    radius: f32,
    icon: Option<&str>,
    ink: LinearRgba,
    px: f32,
    clip: [f32; 4],
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
) {
    out.rects.push(RectInstance::rounded(rect, radius, washed(ink, 0.12), clip));
    match icon {
        Some(glyph) if !glyph.is_empty() => {
            let gw = measure(glyph, px, false, 0.0);
            out.texts.push(TextRun {
                text: glyph.to_string(),
                pos: [rect[0] + (rect[2] - gw) / 2.0, ss::baseline_in(rect[1], rect[3], px)],
                max_width: rect[2] + 2.0,
                color: ink,
                clip,
                px,
                bold: false,
                tracking: 0.0,
            });
        }
        _ => {
            let d = rect[2] * 0.33;
            out.rects.push(RectInstance::rounded(
                [
                    rect[0] + (rect[2] - d) / 2.0,
                    rect[1] + (rect[3] - d) / 2.0,
                    d,
                    d,
                ],
                d / 2.0,
                ink,
                clip,
            ));
        }
    }
}

/// Draws the editor's base layer and returns the open dropdown's anchor —
/// the same #182 contract as the settings screen: a floating panel emitted
/// in the base pass has every base text under it painted over its fill, so
/// the caller draws the menu past the overlay markers.
pub fn profiles_screen(
    model: &ProfilesScreenModel,
    area: [f32; 4],
    colors: &ChromeColors,
    hover: Option<HitRegion>,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) -> Option<[f32; 4]> {
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

    rail(model, area, colors, s, measure, out);

    let rail_w = (RAIL_W * s).min(area[2] * 0.4);
    let content = [area[0] + rail_w, area[1], (area[2] - rail_w).max(0.0), area[3]];
    let cx = content[0] + ss::CONTENT_X * s;
    let cw = (content[2] - 2.0 * ss::CONTENT_X * s).max(0.0);

    let head_bottom = header(model, content, cx, cw, colors, hover, s, measure, out);
    let preview_bottom =
        preview(&model.preview, cx, cw, head_bottom, content, colors, s, measure, out);

    out.rects.push(RectInstance::filled(
        [content[0], preview_bottom, content[2], HAIRLINE * s],
        colors.hairline_soft,
        content,
    ));
    let rows_top = preview_bottom + HAIRLINE * s;

    // Footer fixed at the bottom; the field rows scroll between.
    let footer_y = area[1] + area[3] - ss::FOOTER_H * s;
    footer(model, content, footer_y, colors, s, measure, out);

    let rows_clip = [content[0], rows_top, content[2], (footer_y - rows_top).max(0.0)];
    // The rows pane answers the wheel as settings content does — same
    // region, same meaning: scrollable generated-form ground.
    out.hit.push(rows_clip, HitRegion::SettingsPanel);

    // §11's responsive wrap, inherited whole: under ~400 logical px the
    // control drops to its own line instead of crushing the label column.
    let narrow = cw / s < ss::WRAP_AT;
    let desc_w = if narrow {
        cw
    } else {
        (cw - ss::CONTROL_W * s - 20.0 * s).max(60.0 * s)
    }
    .min(420.0 * s);

    // Extents first: ensure-visible needs the selected row's offset before
    // anything draws (the settings screen's exact discipline).
    let mut tops = Vec::with_capacity(model.rows.len());
    let mut descs = Vec::with_capacity(model.rows.len());
    let mut content_h = 0.0f32;
    for row in &model.rows {
        let (h, lines) = ss::row_extent(row, narrow, desc_w, s, measure);
        tops.push(content_h);
        descs.push(lines);
        content_h += h;
    }
    content_h += 12.0 * s;
    let max_scroll = (content_h - rows_clip[3]).max(0.0);
    let mut scroll = model.scroll.clamp(0.0, max_scroll);
    if model.ensure_visible {
        if let (Some(top), Some(row)) = (tops.get(model.selected), model.rows.get(model.selected))
        {
            let bottom = top + ss::row_extent(row, narrow, desc_w, s, measure).0;
            if *top < scroll {
                scroll = *top;
            } else if bottom > scroll + rows_clip[3] {
                scroll = bottom - rows_clip[3];
            }
            scroll = scroll.clamp(0.0, max_scroll);
        }
    }
    out.profiles_scroll = scroll;

    if model.rows.is_empty() {
        if let Some(empty) = &model.empty {
            out.texts.push(TextRun {
                text: empty.clone(),
                pos: [cx, rows_top + 40.0 * s],
                max_width: cw,
                color: colors.text_faint,
                clip: rows_clip,
                px: 12.0 * s,
                bold: false,
                tracking: 0.0,
            });
        }
    }

    let mut menu_anchor: Option<[f32; 4]> = None;

    for (i, row) in model.rows.iter().enumerate() {
        let y = rows_top + tops[i] - scroll;
        let h = model.rows.get(i + 1).map_or(content_h - 12.0 * s, |_| tops[i + 1]) - tops[i];
        if y + h < rows_clip[1] || y > rows_clip[1] + rows_clip[3] {
            continue;
        }
        let band = [content[0], y, content[2], h];
        let Some(visible) = ss::intersect(band, rows_clip) else { continue };

        match row {
            SettingsRowModel::Group { title } => {
                // The §12 section rule: uppercase label, hairline running to
                // the column's right edge.
                let px = SECTION_PX * s;
                let base = ss::baseline_in(y, h, px);
                let label = title.to_uppercase();
                let tw = measure(&label, px, true, 0.09 * px);
                out.texts.push(TextRun {
                    text: label,
                    pos: [cx, base],
                    max_width: cw,
                    color: colors.text_inactive,
                    clip: rows_clip,
                    px,
                    bold: true,
                    tracking: 0.09 * px,
                });
                out.rects.push(RectInstance::filled(
                    [cx + tw + 12.0 * s, base - 3.0 * s, (cw - tw - 12.0 * s).max(0.0), HAIRLINE * s],
                    colors.hairline_soft,
                    rows_clip,
                ));
            }
            SettingsRowModel::Notice { .. } => {
                let band_rect = [cx, y + 6.0 * s, cw, h - 12.0 * s];
                out.rects.push(RectInstance {
                    radii: [9.0 * s; 4],
                    border: colors.pill_warn_text,
                    border_width: HAIRLINE * s,
                    ..RectInstance::filled(band_rect, colors.pill_warn_bg, rows_clip)
                });
                let mut ty = y + 6.0 * s;
                for line in &descs[i] {
                    ty += 17.0 * s;
                    out.texts.push(TextRun {
                        text: line.clone(),
                        pos: [cx + 14.0 * s, ty],
                        max_width: cw - 28.0 * s,
                        color: colors.pill_warn_text,
                        clip: rows_clip,
                        px: ss::DESC_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
            }
            SettingsRowModel::Unknown { .. } => {
                // The profiles builder never produces these; drawing nothing
                // is honest if one ever arrives.
            }
            SettingsRowModel::Setting { label, key, value, modified, .. } => {
                if i == model.selected {
                    out.rects.push(RectInstance::rounded(
                        [content[0] + 8.0 * s, y + 4.0 * s, content[2] - 16.0 * s, h - 8.0 * s],
                        8.0 * s,
                        colors.accent_soft,
                        rows_clip,
                    ));
                }
                out.hit.push(visible, HitRegion::SettingsRow(i));
                if i + 1 < model.rows.len()
                    && !matches!(model.rows.get(i + 1), Some(SettingsRowModel::Group { .. }))
                {
                    out.rects.push(RectInstance::filled(
                        [cx, y + h - HAIRLINE * s, cw, HAIRLINE * s],
                        colors.hairline_soft,
                        rows_clip,
                    ));
                }

                let top = y + ss::ROW_VPAD * s;
                // The modified dot IS the reset (§12: back through Defaults):
                // only an override draws one, and only a drawn one is a
                // click target — remove_profile_value, never the root file.
                let dot = [cx, top + 6.0 * s, 5.0 * s, 5.0 * s];
                if *modified {
                    out.rects.push(RectInstance::rounded(dot, 2.5 * s, colors.accent, rows_clip));
                    let grab = [dot[0] - 5.0 * s, dot[1] - 5.0 * s, 16.0 * s, 16.0 * s];
                    if let Some(hit) = ss::intersect(grab, rows_clip) {
                        out.hit.push(hit, HitRegion::SettingsReset(i));
                    }
                }
                let text_x = cx + 14.0 * s;
                let mut ty = top + 13.0 * s;
                out.texts.push(TextRun {
                    text: label.clone(),
                    pos: [text_x, ty],
                    max_width: desc_w,
                    color: colors.text_active,
                    clip: rows_clip,
                    px: ss::LABEL_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                ty += 5.0 * s;
                for line in &descs[i] {
                    ty += 17.0 * s;
                    out.texts.push(TextRun {
                        text: line.clone(),
                        pos: [text_x, ty],
                        max_width: desc_w,
                        color: colors.text_inactive,
                        clip: rows_clip,
                        px: ss::DESC_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }
                ty += 16.0 * s;
                let kw = measure(key, ss::KEY_PX * s, false, 0.0);
                out.texts.push(TextRun {
                    text: key.clone(),
                    pos: [text_x, ty],
                    max_width: desc_w,
                    color: colors.text_faint,
                    clip: rows_clip,
                    px: ss::KEY_PX * s,
                    bold: false,
                    tracking: 0.0,
                });
                // The §12 inheritance chip, riding the key line: accent on
                // accentSoft for an override, faint on the header fill for a
                // value that fell through Defaults.
                if let Some(chip) = model.chips.get(i).copied().flatten() {
                    let (text, fg, bg, border) = match chip {
                        InheritChip::Overrides => (
                            "overrides Defaults",
                            colors.accent,
                            colors.accent_soft,
                            LinearRgba::TRANSPARENT,
                        ),
                        InheritChip::Inherited => (
                            "inherited from Defaults",
                            colors.text_faint,
                            colors.block_header_bg,
                            colors.hairline_soft,
                        ),
                        // Faint like `Inherited`, never the warning colours the
                        // Settings screen's restart chip wears: this is a fact
                        // about when the row takes effect, not a problem.
                        InheritChip::NewSessions => (
                            "applies to new sessions",
                            colors.text_faint,
                            colors.block_header_bg,
                            colors.hairline_soft,
                        ),
                    };
                    let tw = measure(text, ss::CHIP_PX * s, false, 0.0);
                    let pad = 6.0 * s;
                    let chip_x = text_x + kw + 10.0 * s;
                    out.rects.push(RectInstance {
                        radii: [5.0 * s; 4],
                        border,
                        border_width: HAIRLINE * s,
                        ..RectInstance::filled(
                            [chip_x, ty - 10.0 * s, tw + 2.0 * pad, 15.0 * s],
                            bg,
                            rows_clip,
                        )
                    });
                    out.texts.push(TextRun {
                        text: text.to_string(),
                        pos: [chip_x + pad, ty + 1.0 * s],
                        max_width: tw + 2.0,
                        color: fg,
                        clip: rows_clip,
                        px: ss::CHIP_PX * s,
                        bold: false,
                        tracking: 0.0,
                    });
                }

                let control_h = ss::control_height(value) * s;
                let control_right = cx + cw;
                let control_top =
                    if narrow { y + h - ss::ROW_VPAD * s - control_h } else { top };
                let anchor = ss::draw_control(
                    out,
                    value,
                    i,
                    model.menu.as_ref().map(|m| m.row),
                    colors,
                    s,
                    rows_clip,
                    control_right,
                    control_top,
                    measure,
                );
                if let Some(a) = anchor {
                    menu_anchor = Some(a);
                }
            }
        }
    }

    // Handed back rather than drawn: the menu belongs to the overlay layer.
    menu_anchor.filter(|_| model.menu.is_some())
}

/// The profile rail (§12): header, Defaults pinned first, a row per profile,
/// dashed "＋ New profile" footer. NO discovery line — #145 tracks it, and a
/// dead row reads as broken.
fn rail(
    model: &ProfilesScreenModel,
    area: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let w = (RAIL_W * s).min(area[2] * 0.4);
    // A surface, like the ground it continues (#538) — and pushed after it, so
    // the replace pipeline simply hands this strip over. Safe in either order
    // regardless: the content column starts right of the rail and the rows are
    // clipped to stop at the footer, so nothing in `rects` overlaps either one.
    out.surface_rects.push(RectInstance::filled([area[0], area[1], w, area[3]], colors.screen_rail_bg, area));
    out.rects.push(RectInstance::filled(
        [area[0] + w - HAIRLINE * s, area[1], HAIRLINE * s, area[3]],
        colors.hairline_soft,
        area,
    ));

    let x = area[0] + RAIL_PAD * s;
    let inner_w = w - 2.0 * RAIL_PAD * s;
    let mut y = area[1] + 16.0 * s;

    let head_px = SECTION_PX * s;
    y += head_px;
    out.texts.push(TextRun {
        text: "LAUNCH TARGETS".into(),
        pos: [x, y],
        max_width: inner_w,
        color: colors.text_inactive,
        clip: area,
        px: head_px,
        bold: true,
        tracking: 0.09 * head_px,
    });
    y += 15.0 * s;
    out.texts.push(TextRun {
        text: "what to run, which machine runs it, how it looks".into(),
        pos: [x, y],
        max_width: inner_w,
        color: colors.text_faint,
        clip: area,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });
    y += 14.0 * s;

    // The dashed footer first, so the rows know where to stop.
    let add = [x, area[1] + area[3] - (30.0 + RAIL_PAD) * s, inner_w, 30.0 * s];
    ss::dashed_border(&mut out.rects, add, s, colors.line, area);
    out.hit.push(add, HitRegion::ProfilesNew);
    let add_label = "+ New profile";
    let aw = measure(add_label, 11.5 * s, false, 0.0);
    out.texts.push(TextRun {
        text: add_label.into(),
        pos: [add[0] + (add[2] - aw) / 2.0, ss::baseline_in(add[1], add[3], 11.5 * s)],
        max_width: add[2],
        color: colors.text_faint,
        clip: area,
        px: 11.5 * s,
        bold: false,
        tracking: 0.0,
    });

    let rows_clip = [area[0], y, w, (add[1] - 8.0 * s - y).max(0.0)];
    for (i, row) in model.rail.iter().enumerate() {
        let rect = [x, y, inner_w, RAIL_ROW_H * s];
        if i == model.selected_rail {
            out.rects.push(RectInstance::rounded(rect, 8.0 * s, colors.accent_soft, rows_clip));
        }
        if let Some(hit) = ss::intersect(rect, rows_clip) {
            out.hit.push(hit, HitRegion::ProfilesRailRow(i));
        }

        let ink = accent_color(colors, row.accent);
        let tile = [
            rect[0] + 6.0 * s,
            rect[1] + (rect[3] - RAIL_TILE * s) / 2.0,
            RAIL_TILE * s,
            RAIL_TILE * s,
        ];
        glyph_tile(out, tile, 6.0 * s, row.icon.as_deref(), ink, 12.0 * s, rows_clip, measure);

        let tx = tile[0] + tile[2] + 9.0 * s;
        let mut right = rect[0] + rect[2] - 10.0 * s;
        if let Some(d) = row.digit {
            let hint = d.to_string();
            let hw = measure(&hint, 10.0 * s, false, 0.0);
            out.texts.push(TextRun {
                text: hint,
                pos: [right - hw, ss::baseline_in(rect[1], rect[3], 10.0 * s)],
                max_width: hw + 2.0,
                color: colors.text_faint,
                clip: rows_clip,
                px: 10.0 * s,
                bold: false,
                tracking: 0.0,
            });
            right -= hw + 8.0 * s;
        }
        out.texts.push(TextRun {
            text: row.name.clone(),
            pos: [tx, rect[1] + 18.0 * s],
            max_width: (right - tx).max(0.0),
            color: if i == model.selected_rail { colors.text_active } else { colors.text_inactive },
            clip: rows_clip,
            px: 12.5 * s,
            bold: false,
            tracking: 0.0,
        });
        out.texts.push(TextRun {
            text: row.sub.clone(),
            pos: [tx, rect[1] + 33.0 * s],
            max_width: (right - tx).max(0.0),
            color: colors.text_faint,
            clip: rows_clip,
            px: 10.0 * s,
            bold: false,
            tracking: 0.0,
        });

        y += RAIL_ROW_H * s + 2.0 * s;
        if i == 0 {
            // Defaults reads as the parent, not a sibling (§12).
            y += DEFAULTS_GAP * s;
        }
    }
}

/// The editor header (§12): 34px glyph tile, name, host chip, command line,
/// Duplicate / Delete. Returns the y below its hairline.
#[allow(clippy::too_many_arguments)]
fn header(
    model: &ProfilesScreenModel,
    content: [f32; 4],
    cx: f32,
    cw: f32,
    colors: &ChromeColors,
    hover: Option<HitRegion>,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) -> f32 {
    let top = content[1] + 18.0 * s;
    let ink = accent_color(colors, model.accent);
    let tile = [cx, top, HEAD_TILE * s, HEAD_TILE * s];
    // 12%-alpha fill, 33%-alpha border — the §12 numbers.
    out.rects.push(RectInstance {
        radii: [9.0 * s; 4],
        border: washed(ink, 0.33),
        border_width: HAIRLINE * s,
        ..RectInstance::filled(tile, washed(ink, 0.12), content)
    });
    glyph_tile(
        out,
        [tile[0] + 1.0, tile[1] + 1.0, tile[2] - 2.0, tile[3] - 2.0],
        8.0 * s,
        model.icon.as_deref(),
        ink,
        16.0 * s,
        content,
        measure,
    );

    // Buttons from the right: Delete (when allowed), then Duplicate.
    let mut right = cx + cw;
    let button = |right: f32,
                      label: &str,
                      region: HitRegion,
                      danger: bool,
                      measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
                      out: &mut ChromeLayout|
     -> f32 {
        let px = 11.0 * s;
        let tw = measure(label, px, false, 0.0);
        let rect = [right - tw - 20.0 * s, top + (HEAD_TILE * s - BTN_H * s) / 2.0, tw + 20.0 * s, BTN_H * s];
        let hovered = hover == Some(region);
        out.rects.push(RectInstance {
            radii: [7.0 * s; 4],
            border: if hovered && danger {
                colors.danger
            } else if hovered {
                colors.accent
            } else {
                colors.line
            },
            border_width: HAIRLINE * s,
            ..RectInstance::filled(rect, colors.panel_bg, content)
        });
        out.hit.push(rect, region);
        out.texts.push(TextRun {
            text: label.to_string(),
            pos: [rect[0] + 10.0 * s, ss::baseline_in(rect[1], rect[3], px)],
            max_width: tw + 2.0,
            // Delete hovers danger (§12); resting it is an ordinary button.
            color: if hovered && danger { colors.danger } else { colors.text_inactive },
            clip: content,
            px,
            bold: false,
            tracking: 0.0,
        });
        rect[0]
    };
    if model.can_delete {
        right = button(right, "Delete", HitRegion::ProfilesDelete, true, measure, out) - 8.0 * s;
    }
    right = button(right, "Duplicate", HitRegion::ProfilesDuplicate, false, measure, out) - 14.0 * s;

    let name_px = 17.0 * s;
    let nx = tile[0] + tile[2] + 12.0 * s;
    let name_w = measure(&model.name, name_px, true, -0.01 * name_px).min((right - nx).max(0.0));

    if let Some(edit) = &model.renaming {
        // The settings row's editing recipe, in the header: 8px radii, accent
        // border turning warn on a name that cannot be used, panel fill.
        let ink = if edit.error.is_some() { colors.pill_warn_text } else { colors.accent };
        let boxr = [
            nx - 8.0 * s,
            top + 1.0 * s,
            (right - nx + 8.0 * s).max(80.0 * s),
            NAME_INPUT_H * s,
        ];
        out.rects.push(RectInstance {
            radii: [8.0 * s; 4],
            border: ink,
            border_width: HAIRLINE * s,
            ..RectInstance::filled(boxr, colors.panel_bg, content)
        });
        ss::text_entry(
            &ss::TextEntry {
                text: &edit.buffer,
                caret: edit.caret,
                selection: edit.selection,
                color: if edit.error.is_some() { colors.pill_warn_text } else { colors.text_active },
                selection_bg: colors.accent_soft,
                px: name_px,
            },
            boxr,
            content,
            s,
            measure,
            out,
        );
        if let Some(why) = &edit.error {
            out.texts.push(TextRun {
                text: why.clone(),
                pos: [nx, boxr[1] + boxr[3] + 13.0 * s],
                max_width: (right - nx).max(0.0),
                color: colors.pill_warn_text,
                clip: content,
                px: 10.5 * s,
                bold: false,
                tracking: 0.0,
            });
        }
        // The host chip and command line are suppressed while renaming: the
        // entry spans the header's width and the error line sits where the
        // command line would be. The tile and buttons stay — they are what
        // says which profile is being renamed.
        return top + HEAD_TILE * s + 14.0 * s;
    }

    // Defaults is the reserved parent and cannot be renamed, so it gets no
    // region at all rather than one that refuses — an affordance that does
    // nothing reads as broken (the §12 rule Delete already follows).
    let renameable = model.can_delete;
    if renameable && hover == Some(HitRegion::ProfilesName) {
        // Before the text, so it sits behind it rather than over it.
        out.rects.push(RectInstance {
            radii: [6.0 * s; 4],
            border: colors.line,
            border_width: HAIRLINE * s,
            ..RectInstance::filled(
                [nx - 6.0 * s, top + 1.0 * s, name_w + 12.0 * s, NAME_INPUT_H * s],
                colors.panel_bg,
                content,
            )
        });
    }
    out.texts.push(TextRun {
        text: model.name.clone(),
        pos: [nx, top + 15.0 * s],
        max_width: name_w,
        color: colors.text_active,
        clip: content,
        px: name_px,
        bold: true,
        tracking: -0.01 * name_px,
    });
    if renameable {
        out.hit.push(
            [nx - 6.0 * s, top + 1.0 * s, name_w + 12.0 * s, NAME_INPUT_H * s],
            HitRegion::ProfilesName,
        );
    }
    if let Some(host) = &model.host_chip {
        let px = 10.0 * s;
        let tw = measure(host, px, false, 0.0);
        let chip = [nx + name_w + 10.0 * s, top + 2.0 * s, tw + 14.0 * s, 16.0 * s];
        if chip[0] + chip[2] < right {
            out.rects.push(RectInstance {
                radii: [5.0 * s; 4],
                border: colors.line,
                border_width: HAIRLINE * s,
                ..RectInstance::filled(chip, colors.panel_bg, content)
            });
            out.texts.push(TextRun {
                text: host.clone(),
                pos: [chip[0] + 7.0 * s, ss::baseline_in(chip[1], chip[3], px)],
                max_width: tw + 2.0,
                color: colors.text_inactive,
                clip: content,
                px,
                bold: false,
                tracking: 0.0,
            });
        }
    }
    out.texts.push(TextRun {
        text: model.command.clone(),
        pos: [nx, top + 31.0 * s],
        max_width: (right - nx).max(0.0),
        color: colors.text_faint,
        clip: content,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });

    top + HEAD_TILE * s + 14.0 * s
}

/// The §12 live preview: a mini tab-chip in the WINDOW's chrome colours
/// carrying only the profile's 2px rule and glyph, over a body block in the
/// profile's scheme, with the caption saying exactly that. Returns the y
/// where the rows begin.
#[allow(clippy::too_many_arguments)]
fn preview(
    p: &ProfilePreviewModel,
    cx: f32,
    cw: f32,
    top: f32,
    content: [f32; 4],
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) -> f32 {
    let ink = accent_color(colors, p.accent);
    let bw = cw.min(520.0 * s);

    // The chip: the window's panel fill and line border — deliberately NOT
    // the scheme's colours; that restraint is what the caption points at.
    let title_px = 11.5 * s;
    let chip_w = (measure(&p.title, title_px, false, 0.0) + 52.0 * s).min(bw * 0.6);
    let chip = [cx, top, chip_w, 26.0 * s];
    out.rects.push(RectInstance {
        radii: [7.0 * s, 7.0 * s, 0.0, 0.0],
        border: colors.line,
        border_width: HAIRLINE * s,
        ..RectInstance::filled(chip, colors.panel_bg, content)
    });
    // The one per-tab concession: the 2px rule in the profile's accent.
    out.rects.push(RectInstance::filled(
        [chip[0] + HAIRLINE * s, chip[1] + HAIRLINE * s, chip[2] - 2.0 * HAIRLINE * s, 2.0 * s],
        ink,
        content,
    ));
    let tile = [chip[0] + 6.0 * s, chip[1] + 5.0 * s, 16.0 * s, 16.0 * s];
    glyph_tile(out, tile, 4.0 * s, p.icon.as_deref(), ink, 10.0 * s, content, measure);
    out.texts.push(TextRun {
        text: p.title.clone(),
        pos: [tile[0] + tile[2] + 7.0 * s, ss::baseline_in(chip[1], chip[3], title_px)],
        max_width: chip[2] - 34.0 * s,
        color: colors.text_active,
        clip: content,
        px: title_px,
        bold: false,
        tracking: 0.0,
    });

    // The body block: the profile's scheme, and only here.
    let mono = 11.5 * s;
    let lh = 17.0 * s;
    let bh = p.lines.len() as f32 * lh + 18.0 * s;
    let body = [cx, top + 26.0 * s, bw, bh];
    out.rects.push(RectInstance {
        radii: [0.0, 7.0 * s, 7.0 * s, 7.0 * s],
        border: colors.line,
        border_width: HAIRLINE * s,
        ..RectInstance::filled(body, srgb(p.scheme_bg), content)
    });
    let mut ly = body[1] + 6.0 * s;
    for (i, line) in p.lines.iter().enumerate() {
        ly += lh;
        // The prompt line leads with the scheme's accent — the third colour
        // §12 asks the fragment to prove.
        let (head, rest) = match (i, line.split_once(' ')) {
            (0, Some((h, r))) => (Some(h.to_string()), format!(" {r}")),
            _ => (None, line.clone()),
        };
        let mut lx = body[0] + 12.0 * s;
        if let Some(head) = head {
            let hw = measure(&head, mono, false, 0.0);
            out.texts.push(TextRun {
                text: head,
                pos: [lx, ly],
                max_width: hw + 2.0,
                color: srgb(p.scheme_accent),
                clip: content,
                px: mono,
                bold: false,
                tracking: 0.0,
            });
            lx += hw;
        }
        out.texts.push(TextRun {
            text: rest,
            pos: [lx, ly],
            max_width: (body[0] + body[2] - lx - 12.0 * s).max(0.0),
            color: srgb(p.scheme_fg),
            clip: content,
            px: mono,
            bold: false,
            tracking: 0.0,
        });
    }

    // The caption, verbatim from the model (§12's exact sentence).
    let mut cy = body[1] + body[3] + 6.0 * s;
    for line in ss::wrap_text(&p.caption, 10.5 * s, cw, measure) {
        cy += 14.0 * s;
        out.texts.push(TextRun {
            text: line,
            pos: [cx, cy],
            max_width: cw,
            color: colors.text_faint,
            clip: content,
            px: 10.5 * s,
            bold: false,
            tracking: 0.0,
        });
    }
    cy + 12.0 * s
}

/// The footer bar (§12): a dot in the profile's colour, the override-count
/// sentence, the TOML table name, and `Edit as TOML`.
fn footer(
    model: &ProfilesScreenModel,
    content: [f32; 4],
    footer_y: f32,
    colors: &ChromeColors,
    s: f32,
    measure: &mut dyn FnMut(&str, f32, bool, f32) -> f32,
    out: &mut ChromeLayout,
) {
    let bar = [content[0], footer_y, content[2], ss::FOOTER_H * s];
    // A surface, like the ground it continues (#538) — and pushed after it, so
    // the replace pipeline simply hands this strip over. Safe in either order
    // regardless: the content column starts right of the rail and the rows are
    // clipped to stop at the footer, so nothing in `rects` overlaps either one.
    out.surface_rects.push(RectInstance::filled(bar, colors.screen_rail_bg, content));
    out.rects.push(RectInstance::filled(
        [bar[0], bar[1], bar[2], HAIRLINE * s],
        colors.hairline_soft,
        content,
    ));

    // Right side first, so the sentence budgets against it.
    let mut right = bar[0] + bar[2] - ss::CONTENT_X * s;
    let edit = "Edit as TOML";
    let ew = measure(edit, ss::DESC_PX * s, false, 0.0);
    right -= ew;
    out.hit.push([right - 6.0 * s, bar[1], ew + 12.0 * s, bar[3]], HitRegion::SettingsEditToml);
    out.texts.push(TextRun {
        text: edit.into(),
        pos: [right, ss::baseline_in(bar[1], bar[3], ss::DESC_PX * s)],
        max_width: ew + 2.0,
        color: colors.text_inactive,
        clip: content,
        px: ss::DESC_PX * s,
        bold: false,
        tracking: 0.0,
    });
    right -= 14.0 * s;
    let tw = measure(&model.table_name, ss::KEY_PX * s, false, 0.0).min(bar[2] * 0.35);
    right -= tw;
    out.texts.push(TextRun {
        text: model.table_name.clone(),
        pos: [right, ss::baseline_in(bar[1], bar[3], ss::KEY_PX * s)],
        max_width: tw + 2.0,
        color: colors.text_faint,
        clip: content,
        px: ss::KEY_PX * s,
        bold: false,
        tracking: 0.0,
    });

    let x = bar[0] + ss::CONTENT_X * s;
    out.rects.push(RectInstance::rounded(
        [x, bar[1] + (bar[3] - 5.0 * s) / 2.0, 5.0 * s, 5.0 * s],
        2.5 * s,
        accent_color(colors, model.accent),
        content,
    ));
    out.texts.push(TextRun {
        text: model.footer_sentence.clone(),
        pos: [x + 13.0 * s, ss::baseline_in(bar[1], bar[3], 11.0 * s)],
        max_width: (right - x - 27.0 * s).max(0.0),
        color: colors.text_inactive,
        clip: content,
        px: 11.0 * s,
        bold: false,
        tracking: 0.0,
    });
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        AccentChoice, ProfileRailRow, SettingsValueCell,
    };
    use super::*;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0, 1.0)
    }

    fn measure(text: &str, px: f32, _b: bool, _t: f32) -> f32 {
        text.chars().count() as f32 * px * 0.6
    }

    fn rail_row(name: &str, digit: Option<u8>) -> ProfileRailRow {
        ProfileRailRow {
            name: name.into(),
            sub: "pwsh \u{b7} studio".into(),
            icon: None,
            accent: AccentChoice::Host(0),
            digit,
        }
    }

    fn setting_row(key: &str, value: SettingsValueCell, modified: bool) -> SettingsRowModel {
        SettingsRowModel::Setting {
            label: key.to_string(),
            key: key.to_string(),
            description: "Words about the field.".into(),
            value,
            provenance: None,
            restart: false,
            inert: false,
            modified,
        }
    }

    fn model(can_delete: bool) -> ProfilesScreenModel {
        ProfilesScreenModel {
            rail: vec![rail_row("Defaults", None), rail_row("ubuntu", Some(1))],
            selected_rail: 1,
            name: "ubuntu".into(),
            command: "wsl.exe -d Ubuntu".into(),
            host_chip: Some("forge".into()),
            icon: None,
            accent: AccentChoice::Profile(2),
            can_delete,
            preview: ProfilePreviewModel {
                title: "ubuntu".into(),
                icon: None,
                accent: AccentChoice::Profile(2),
                scheme_bg: [10, 15, 26],
                scheme_fg: [215, 220, 234],
                scheme_accent: [110, 168, 255],
                caption: "Chrome is the window's theme (obsidian). Only the grid follows \
                          this profile's scheme."
                    .into(),
                lines: vec!["\u{276f} uname -sr".into(), "Linux 6.8.0-31-generic".into()],
            },
            rows: vec![
                SettingsRowModel::Group { title: "Appearance".into() },
                setting_row(
                    "color_scheme",
                    SettingsValueCell::SchemeSwatches {
                        options: vec![
                            super::super::model::SchemeSwatch {
                                id: "obsidian".into(),
                                ansi: [[0; 3]; 8],
                            },
                            super::super::model::SchemeSwatch {
                                id: "nord".into(),
                                ansi: [[10; 3]; 8],
                            },
                        ],
                        selected: Some(1),
                    },
                    true,
                ),
                setting_row(
                    "tab_color",
                    SettingsValueCell::AccentSwatches { selected: Some(2), inert: false },
                    false,
                ),
                setting_row(
                    "icon",
                    SettingsValueCell::Glyphs {
                        options: vec!["\u{2605}".into(), "\u{25cf}".into()],
                        selected: None,
                    },
                    false,
                ),
                setting_row(
                    "host",
                    SettingsValueCell::HostPill { name: "forge".into(), online: true },
                    false,
                ),
            ],
            chips: vec![
                None,
                Some(InheritChip::Overrides),
                Some(InheritChip::Inherited),
                None,
                None,
            ],
            selected: 1,
            filter: String::new(),
            filter_caret: Default::default(),
            renaming: None,
            scroll: 0.0,
            ensure_visible: false,
            empty: None,
            footer_sentence: "1 setting overrides Defaults".into(),
            table_name: "[profiles.ubuntu]".into(),
            menu: None,
        }
    }

    #[test]
    fn the_base_pass_floats_nothing_and_hands_the_menu_back() {
        // #182's profiles half: the editor's dropdown had the same
        // base-pass draw-ordering hole as the settings tab's. The base pass
        // now returns the anchor instead of drawing — no SettingsMenuRow
        // can exist until the caller draws the menu in the overlay layer.
        let mut m = model(false);
        // Menus only ever open from Select pills (window.backdrop is the
        // §12 case); the fixture has none, so give it one.
        m.rows.push(setting_row(
            "window.backdrop",
            SettingsValueCell::Select { value: "mica".into() },
            false,
        ));
        m.chips.push(None);
        let select_row = m.rows.len() - 1;
        m.menu = Some(super::super::model::SettingsMenuModel {
            row: select_row,
            options: vec![super::super::model::SettingsMenuOption {
                label: "Mica".into(),
                value: "mica".into(),
                doc: String::new(),
            }],
            current: Some(0),
            selected: 0,
            searchable: false,
            filter: String::new(),
            filter_caret: Default::default(),
            scroll: 0.0,
            ensure_visible: false,
            footer: None,
        });
        let mut out = ChromeLayout::default();
        let anchor =
            profiles_screen(&m, [0.0, 46.0, 1100.0, 720.0], &colors(), None, 1.0, &mut measure, &mut out);
        assert!(anchor.is_some(), "an open menu must hand its anchor to the caller");
        let leaked = (0..1100).step_by(4).any(|x| {
            (46..766).step_by(4).any(|y| {
                matches!(out.hit.hit(x as f32, y as f32), Some(HitRegion::SettingsMenuRow(_)))
            })
        });
        assert!(!leaked, "the base pass must not draw the menu — base texts would paint over it");
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
        let area = [0.0, 46.0, 1100.0, 720.0];
        profiles_screen(&model(true), area, &colors_at(0.4), None, 1.0, &mut measure, &mut out);

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
        // The rail is the ground continuing one tone down, so it is a surface
        // too — and at the same alpha, or it reads as a solid column between a
        // glass sidebar and glass content.
        assert!(
            out.surface_rects.len() >= 2,
            "the ground and the rail are both surfaces, got {}",
            out.surface_rects.len()
        );
        assert!(
            out.surface_rects.iter().all(|r| (r.fill.0[3] - 0.4).abs() < 1e-6),
            "every surface a screen pushes carries the one chrome alpha"
        );
        assert_eq!(
            out.hit.hit(area[0] + 2.0, area[1] + 2.0),
            Some(HitRegion::ScreenPanel),
            "moving the fill must not move the swallow region"
        );
    }

    fn lay(model: &ProfilesScreenModel, w: f32, h: f32) -> ChromeLayout {
        let mut out = ChromeLayout::default();
        let area = [0.0, 46.0, w, h];
        let anchor = profiles_screen(model, area, &colors(), None, 1.0, &mut measure, &mut out);
        // The caller contract since #182: the base pass hands the anchor
        // back and the menu draws in the overlay layer, like layout() does.
        if let (Some(menu), Some(anchor)) = (&model.menu, anchor) {
            ss::dropdown_menu(menu, anchor, area, &colors(), 1.0, &mut measure, &mut out);
        }
        out
    }

    #[test]
    fn the_rail_the_header_the_choices_and_the_footer_all_answer() {
        let l = lay(&model(true), 1100.0, 720.0);
        let mut rail_rows = std::collections::HashSet::new();
        let mut choices = std::collections::HashSet::new();
        let mut seen_new = false;
        let mut seen_name = false;
        let mut seen_dup = false;
        let mut seen_del = false;
        let mut seen_row = false;
        let mut seen_reset = false;
        let mut seen_toml = false;
        let mut seen_host_pill = false;
        for x in (0..1100).step_by(2) {
            for y in (46..766).step_by(2) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::ProfilesRailRow(i)) => {
                        rail_rows.insert(i);
                    }
                    Some(HitRegion::ProfilesChoice(row, opt)) => {
                        choices.insert((row, opt));
                    }
                    Some(HitRegion::ProfilesNew) => seen_new = true,
                    Some(HitRegion::ProfilesName) => seen_name = true,
                    Some(HitRegion::ProfilesDuplicate) => seen_dup = true,
                    Some(HitRegion::ProfilesDelete) => seen_del = true,
                    Some(HitRegion::SettingsRow(_)) => seen_row = true,
                    Some(HitRegion::SettingsReset(1)) => seen_reset = true,
                    Some(HitRegion::SettingsEditToml) => seen_toml = true,
                    Some(HitRegion::SettingsSelect(4)) => seen_host_pill = true,
                    Some(HitRegion::SettingsReset(i)) => {
                        panic!("row {i} is not an override; its dot must not take clicks")
                    }
                    Some(HitRegion::ScreenPanel | HitRegion::SettingsPanel) | None => {}
                    other => panic!("({x},{y}) escaped the profiles screen: {other:?}"),
                }
            }
        }
        assert_eq!(rail_rows.len(), 2, "every rail row answers as itself");
        assert!(seen_new, "the dashed + New profile is clickable");
        assert!(seen_name, "the header name answers, so it can be renamed (#283)");
        assert!(seen_dup, "Duplicate answers");
        assert!(seen_del, "Delete answers when allowed");
        assert!(seen_row, "field rows select on click");
        assert!(seen_reset, "the override's dot is the reset");
        assert!(seen_toml, "'Edit as TOML' answers");
        assert!(seen_host_pill, "the host pill routes through the select dispatch");
        // Scheme swatches (row 1: two options), accent swatches (row 2: six),
        // icon tiles (row 3: two) all answer with their option index.
        assert!(
            choices.contains(&(1, 0)) && choices.contains(&(1, 1)),
            "scheme chips answer: {choices:?}"
        );
        assert_eq!(
            (0..6).filter(|o| choices.contains(&(2, *o))).count(),
            6,
            "all six accent swatches answer: {choices:?}"
        );
        assert!(choices.contains(&(3, 0)) && choices.contains(&(3, 1)), "icon tiles answer");
    }

    #[test]
    fn a_text_row_takes_a_click_to_begin_editing() {
        // #276 was filed on the belief that the profiles screen pushes no hit
        // region for a value cell, making `command` keyboard-only. It does —
        // through the SHARED `ss::draw_control`, which this screen borrows
        // whole; the earlier survey only enumerated the regions pushed
        // literally in this file and missed it. Pinned here rather than
        // closed silently, so the claim cannot be made a third time.
        let mut m = model(true);
        m.rows.push(setting_row(
            "command",
            SettingsValueCell::Text {
                text: "wsl.exe -d Ubuntu".into(),
                placeholder: false,
            },
            false,
        ));
        m.chips.push(None);
        let row = m.rows.len() - 1;
        let l = lay(&m, 1100.0, 720.0);
        // `any` rather than the neighbouring full scans: those collect several
        // regions at once and have to see the whole screen, this one asks a
        // single yes/no and short-circuits on the first hit.
        let seen = (0..1100).step_by(2).any(|x| {
            (46..766)
                .step_by(2)
                .any(|y| l.hit.hit(x as f32, y as f32) == Some(HitRegion::SettingsSelect(row)))
        });
        assert!(
            seen,
            "a text row must answer a click; the dispatch routes SettingsSelect \
             to profiles_activate_selected, which opens the edit"
        );
    }

    #[test]
    fn defaults_draws_no_delete_button() {
        // §12: Defaults has no Delete — the parent every profile falls
        // through to must not be one misclick from gone.
        let l = lay(&model(false), 1100.0, 720.0);
        let mut seen_dup = false;
        for x in (0..1100).step_by(2) {
            for y in (46..766).step_by(2) {
                match l.hit.hit(x as f32, y as f32) {
                    Some(HitRegion::ProfilesDelete) => {
                        panic!("({x},{y}): Delete must not exist on Defaults")
                    }
                    // Same rule, same reason (#283): `defaults` is the name
                    // the whole cascade resolves through, so it is not
                    // renameable — and an affordance that refuses reads as
                    // broken, so it is absent rather than inert.
                    Some(HitRegion::ProfilesName) => {
                        panic!("({x},{y}): Defaults' name must not be renameable")
                    }
                    Some(HitRegion::ProfilesDuplicate) => seen_dup = true,
                    _ => {}
                }
            }
        }
        assert!(seen_dup, "Duplicate stays — copying Defaults into a profile is legal");
    }

    #[test]
    fn a_rename_in_flight_replaces_the_name_with_an_entry() {
        // The entry has to actually reach the layout: a model field the
        // drawing ignores is the shape where the caret exists in state and
        // never on screen.
        let mut m = model(true);
        m.renaming = Some(super::super::model::ProfileNameEdit {
            buffer: "forge".into(),
            caret: 5,
            selection: None,
            error: Some("a profile with that name already exists".into()),
        });
        let l = lay(&m, 1100.0, 720.0);
        assert!(
            l.texts.iter().any(|t| t.text == "forge"),
            "the buffer is drawn, not the profile's stored name"
        );
        assert!(
            l.texts.iter().any(|t| t.text == "a profile with that name already exists"),
            "the reason a name is refused is on screen, not only in state"
        );
        // The name region is the resting affordance; while the entry is open
        // the box IS the entry, so a click target over it would reopen what
        // is already open.
        let mut seen_name = false;
        for x in (0..1100).step_by(2) {
            for y in (46..766).step_by(2) {
                if l.hit.hit(x as f32, y as f32) == Some(HitRegion::ProfilesName) {
                    seen_name = true;
                }
            }
        }
        assert!(!seen_name, "the click-to-rename target is gone while renaming");
    }

    #[test]
    fn inert_accent_swatches_take_no_clicks() {
        // §12: dimmed AND inert when the host decides — a swatch that acts
        // while looking disabled would betray `color_from = "host"`.
        let mut m = model(true);
        m.rows[2] = setting_row(
            "tab_color",
            SettingsValueCell::AccentSwatches { selected: Some(2), inert: true },
            false,
        );
        let l = lay(&m, 1100.0, 720.0);
        for x in (0..1100).step_by(2) {
            for y in (46..766).step_by(2) {
                if let Some(HitRegion::ProfilesChoice(2, opt)) = l.hit.hit(x as f32, y as f32) {
                    panic!("({x},{y}): inert swatch {opt} answered a click");
                }
            }
        }
    }
}
