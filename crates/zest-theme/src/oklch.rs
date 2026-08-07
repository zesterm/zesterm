//! Perceptual colour operations, in OKLCH.
//!
//! # Why not RGB
//!
//! Every derivation here — lightening a background into a panel, mixing a
//! border, lifting a normal ANSI colour into its bright counterpart — is a
//! *perceptual* request. Naive RGB arithmetic does not honour that: lightening
//! `#804000` by adding to each channel desaturates it toward grey, and
//! lightening a saturated blue barely changes how light it looks.
//!
//! The visible consequence in a terminal is bright ANSI colours that look
//! washed out and no longer belong to the theme's family. OKLCH keeps lightness
//! and chroma independent, so "10% lighter, slightly more saturated" means what
//! it says.

use palette::{FromColor, IntoColor, Oklch, Srgb};

use crate::tokens::Rgba8;

fn to_oklch(c: Rgba8) -> Oklch {
    let srgb = Srgb::new(
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
    );
    Oklch::from_color(srgb.into_linear())
}

fn from_oklch(c: Oklch, alpha: u8) -> Rgba8 {
    let linear: palette::LinSrgb = c.into_color();
    let srgb: Srgb<f32> = Srgb::from_linear(linear);
    Rgba8::rgba(
        (srgb.red.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.green.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.blue.clamp(0.0, 1.0) * 255.0).round() as u8,
        alpha,
    )
}

/// Shift lightness by `delta` (roughly -1.0 to 1.0), preserving hue and chroma.
#[must_use]
pub fn lighten(c: Rgba8, delta: f32) -> Rgba8 {
    let mut lch = to_oklch(c);
    lch.l = (lch.l + delta).clamp(0.0, 1.0);
    from_oklch(lch, c.a)
}

/// Lighten toward white or darken toward black, whichever moves *away* from the
/// colour's own lightness.
///
/// This is what "make a panel that stands out from the background" means: on a
/// dark theme it must lighten, on a light theme it must darken, and a token
/// derivation should not need to know which it is looking at.
///
/// # The pure-black case
///
/// OKLCH lightness is perceptual, but 8-bit sRGB is coarsely quantized at the
/// extremes. Near `#000000` a lightness delta of 0.03 lands *below one sRGB
/// step* and rounds straight back to black — so a classic xterm-style theme
/// with a pure black background would derive a panel identical to its
/// background, making the whole tab bar invisible.
///
/// So when the perceptual shift quantizes to nothing, fall back to a minimum
/// step in plain sRGB. It is less principled, but "visible" beats "perceptually
/// exact" when the alternative is an invisible UI.
#[must_use]
pub fn contrast_shift(c: Rgba8, amount: f32) -> Rgba8 {
    let lch = to_oklch(c);
    let up = lch.l < 0.5;
    let shifted = lighten(c, if up { amount } else { -amount });

    if shifted.r != c.r || shifted.g != c.g || shifted.b != c.b {
        return shifted;
    }

    let step = (amount * 300.0).round().max(1.0) as i16;
    let nudge = |v: u8| -> u8 {
        let signed = i16::from(v) + if up { step } else { -step };
        signed.clamp(0, 255) as u8
    };
    Rgba8::rgba(nudge(c.r), nudge(c.g), nudge(c.b), c.a)
}

/// Perceptual mix. `t = 0` returns `a`, `t = 1` returns `b`.
///
/// Hue is interpolated the short way around the wheel, so mixing red and
/// magenta does not detour through green.
#[must_use]
pub fn mix(a: Rgba8, b: Rgba8, t: f32) -> Rgba8 {
    let t = t.clamp(0.0, 1.0);
    let (x, y) = (to_oklch(a), to_oklch(b));

    let hx = x.hue.into_degrees();
    let hy = y.hue.into_degrees();
    let mut dh = (hy - hx) % 360.0;
    if dh > 180.0 {
        dh -= 360.0;
    } else if dh < -180.0 {
        dh += 360.0;
    }

    let mixed = Oklch::new(
        x.l + (y.l - x.l) * t,
        x.chroma + (y.chroma - x.chroma) * t,
        hx + dh * t,
    );
    let alpha = (f32::from(a.a) + (f32::from(b.a) - f32::from(a.a)) * t).round() as u8;
    from_oklch(mixed, alpha)
}

/// Flatten a translucent colour onto an opaque backdrop.
///
/// Themes express soft accents as "the accent at 15%", but a terminal palette
/// entry has to be a concrete opaque colour. This does that compositing once,
/// at theme-resolution time, instead of per frame.
#[must_use]
pub fn flatten(fg: Rgba8, alpha: f32, bg: Rgba8) -> Rgba8 {
    mix(bg, Rgba8::rgb(fg.r, fg.g, fg.b), alpha.clamp(0.0, 1.0))
}

/// The bright counterpart of a normal ANSI colour.
///
/// Lifts lightness and nudges chroma up, rather than blending toward white.
/// Blending toward white is what produces the pale, washed-out brights that no
/// longer look like they belong to the theme.
#[must_use]
pub fn brighten_ansi(c: Rgba8) -> Rgba8 {
    let mut lch = to_oklch(c);
    lch.l = (lch.l + 0.10).min(0.95);
    lch.chroma *= 1.06;
    from_oklch(lch, c.a)
}

/// Relative luminance, per WCAG.
#[must_use]
pub fn luminance(c: Rgba8) -> f32 {
    let ch = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
}

/// WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).
#[must_use]
pub fn contrast_ratio(a: Rgba8, b: Rgba8) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Mix `base` toward `toward` until the result is at least `min_ratio` against
/// `base`.
///
/// Derived chrome has a *requirement* — a border you cannot see is not a border
/// — and a fixed mix ratio does not deliver it uniformly. On a mid-dark
/// background 14% toward the foreground is a perfectly visible line; on pure
/// black the same 14% lands around `#090909` and vanishes. Rather than tuning a
/// constant that is wrong at one end or the other, mix until the requirement is
/// actually met.
///
/// Themes with normal backgrounds are unaffected: the starting mix already
/// clears the threshold and the loop never runs.
#[must_use]
pub fn mix_to_min_contrast(base: Rgba8, toward: Rgba8, start_t: f32, min_ratio: f32) -> Rgba8 {
    let mut t = start_t.clamp(0.0, 1.0);
    let mut out = mix(base, toward, t);
    while t < 1.0 && contrast_ratio(out, base) < min_ratio {
        t = (t + 0.02).min(1.0);
        out = mix(base, toward, t);
    }
    out
}

/// Whichever of two candidates is more readable on `bg`.
///
/// Used to pick `accentText`: a theme whose accent is pale needs dark text on
/// it, and one whose accent is deep needs light text. Choosing wrong makes
/// button labels vanish.
#[must_use]
pub fn best_contrast(bg: Rgba8, a: Rgba8, b: Rgba8) -> Rgba8 {
    if contrast_ratio(bg, a) >= contrast_ratio(bg, b) {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_BG: Rgba8 = Rgba8::rgb(0x0b, 0x0f, 0x1a);
    const LIGHT_BG: Rgba8 = Rgba8::rgb(0xf5, 0xf5, 0xf0);

    #[test]
    fn lighten_raises_perceived_lightness() {
        let out = lighten(DARK_BG, 0.2);
        assert!(luminance(out) > luminance(DARK_BG));
    }

    #[test]
    fn contrast_shift_goes_the_right_way_for_both_modes() {
        // The point of this helper: a token derivation should not have to know
        // whether it is looking at a light or dark theme.
        assert!(luminance(contrast_shift(DARK_BG, 0.05)) > luminance(DARK_BG));
        assert!(luminance(contrast_shift(LIGHT_BG, 0.05)) < luminance(LIGHT_BG));
    }

    #[test]
    fn contrast_shift_is_visible_even_at_pure_black_and_white() {
        // The bug this guards: a perceptual delta of 0.03 near #000000 is below
        // one 8-bit sRGB step and rounds back to black, so a classic
        // xterm-style theme derives a panel identical to its background and the
        // tab bar disappears.
        let black = Rgba8::rgb(0, 0, 0);
        let panel = contrast_shift(black, 0.03);
        assert_ne!(panel, black, "a panel on pure black must be visible");
        assert!(luminance(panel) > luminance(black));

        let white = Rgba8::rgb(255, 255, 255);
        let panel = contrast_shift(white, 0.03);
        assert_ne!(panel, white, "and on pure white");
        assert!(luminance(panel) < luminance(white));
    }

    #[test]
    fn mix_endpoints_are_exact() {
        let a = Rgba8::rgb(0xff, 0x00, 0x00);
        let b = Rgba8::rgb(0x00, 0x00, 0xff);
        // Round-tripping through OKLCH is lossy by a unit or so; endpoints must
        // still land essentially on the inputs.
        let near = |x: Rgba8, y: Rgba8| {
            x.r.abs_diff(y.r) <= 2 && x.g.abs_diff(y.g) <= 2 && x.b.abs_diff(y.b) <= 2
        };
        assert!(near(mix(a, b, 0.0), a), "t=0 should be a");
        assert!(near(mix(a, b, 1.0), b), "t=1 should be b");
    }

    #[test]
    fn mix_takes_the_short_way_around_the_hue_wheel() {
        // Red (~29 deg) to magenta (~328 deg). Going the long way would pass
        // through green and yellow, which looks obviously wrong.
        let red = Rgba8::rgb(0xff, 0x00, 0x00);
        let magenta = Rgba8::rgb(0xff, 0x00, 0xff);
        let mid = mix(red, magenta, 0.5);
        assert!(mid.r > mid.g, "the midpoint should stay red-ish, not go green");
        assert!(mid.b > mid.g, "and gain blue on the way to magenta");
    }

    #[test]
    fn brightening_lifts_without_washing_out() {
        let red = Rgba8::rgb(0xcc, 0x33, 0x33);
        let bright = brighten_ansi(red);

        assert!(luminance(bright) > luminance(red), "brights are lighter");

        // The failure this guards: blending toward white raises luminance but
        // collapses the colour toward grey. Chroma must not shrink.
        let before = to_oklch(red).chroma;
        let after = to_oklch(bright).chroma;
        assert!(after >= before * 0.95, "bright variants must stay in the theme's colour family");
    }

    #[test]
    fn flatten_composites_onto_the_backdrop() {
        let accent = Rgba8::rgb(0x6e, 0xa8, 0xff);
        let soft = flatten(accent, 0.15, DARK_BG);
        // A 15% accent over a dark background should be only slightly lifted,
        // and must be fully opaque -- palette entries cannot carry alpha.
        assert!(soft.is_opaque());
        assert!(luminance(soft) > luminance(DARK_BG));
        assert!(luminance(soft) < luminance(accent));
    }

    #[test]
    fn contrast_ratio_matches_known_values() {
        let black = Rgba8::rgb(0, 0, 0);
        let white = Rgba8::rgb(255, 255, 255);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.1);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
    }

    #[test]
    fn best_contrast_picks_the_readable_option() {
        let black = Rgba8::rgb(0, 0, 0);
        let white = Rgba8::rgb(255, 255, 255);
        // A pale accent needs dark text; a deep one needs light text. Getting
        // this backwards makes labels disappear.
        assert_eq!(best_contrast(Rgba8::rgb(0xff, 0xe0, 0x80), black, white), black);
        assert_eq!(best_contrast(Rgba8::rgb(0x1a, 0x2b, 0x6e), black, white), white);
    }
}
