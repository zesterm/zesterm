/**
 * Perceptual color operations, in OKLCH — `crates/zest-theme/src/oklch.rs`.
 *
 * Why not RGB: every derivation here is a *perceptual* request, and naive RGB
 * arithmetic does not honour it — lightening by adding to each channel
 * desaturates toward grey. The visible consequence in a terminal is bright
 * ANSI colors that look washed out and no longer belong to the theme's family.
 *
 * The Rust side uses the `palette` crate; this file implements Björn
 * Ottosson's reference OKLab conversion directly (the same math `palette`
 * encodes), so the parity gap with Rust is float rounding only — tests hold
 * results to within ±2/255 per channel of values computed by the Rust crate.
 */

interface Rgba {
  r: number;
  g: number;
  b: number;
  a: number;
}

interface Oklch {
  l: number;
  c: number;
  /** Degrees. */
  h: number;
}

/**
 * Parse `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa` — the forms `Rgba8::from_str`
 * accepts — so a token that ever arrives in short form does not silently
 * become NaN-driven black.
 */
export function parseHex(hex: string): Rgba {
  const s = hex.trim();
  if (!s.startsWith('#')) throw new Error(`expected a leading '#' in ${JSON.stringify(hex)}`);
  const h = s.slice(1);
  if (!/^[0-9a-fA-F]+$/.test(h)) throw new Error(`invalid hex digit in ${JSON.stringify(hex)}`);
  const nib = (i: number): number => parseInt(h[i]!, 16) * 17;
  const byte = (i: number): number => parseInt(h.slice(i, i + 2), 16);
  switch (h.length) {
    case 3:
      return { r: nib(0), g: nib(1), b: nib(2), a: 255 };
    case 4:
      return { r: nib(0), g: nib(1), b: nib(2), a: nib(3) };
    case 6:
      return { r: byte(0), g: byte(2), b: byte(4), a: 255 };
    case 8:
      return { r: byte(0), g: byte(2), b: byte(4), a: byte(6) };
    default:
      throw new Error(`expected 3, 4, 6 or 8 hex digits, found ${h.length}`);
  }
}

/** Mirror of `Rgba8`'s `Display`: alpha is omitted when opaque. */
export function formatHex(c: Rgba): string {
  const two = (v: number): string => v.toString(16).padStart(2, '0');
  const rgb = `#${two(c.r)}${two(c.g)}${two(c.b)}`;
  return c.a === 255 ? rgb : `${rgb}${two(c.a)}`;
}

function srgbToLinear(v: number): number {
  const c = v / 255;
  return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

function linearToSrgb(v: number): number {
  const c = v <= 0.0031308 ? v * 12.92 : 1.055 * Math.max(v, 0) ** (1 / 2.4) - 0.055;
  return Math.round(Math.min(Math.max(c, 0), 1) * 255);
}

// Ottosson's reference matrices (https://bottosson.github.io/posts/oklab/).
function toOklch(c: Rgba): Oklch {
  const r = srgbToLinear(c.r);
  const g = srgbToLinear(c.g);
  const b = srgbToLinear(c.b);

  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);

  const L = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s;
  const A = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
  const B = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;

  return {
    l: L,
    c: Math.hypot(A, B),
    h: (Math.atan2(B, A) * 180) / Math.PI,
  };
}

function fromOklch(c: Oklch, alpha: number): Rgba {
  const hRad = (c.h * Math.PI) / 180;
  const A = c.c * Math.cos(hRad);
  const B = c.c * Math.sin(hRad);

  const l = (c.l + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const m = (c.l - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const s = (c.l - 0.0894841775 * A - 1.291485548 * B) ** 3;

  return {
    r: linearToSrgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
    g: linearToSrgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
    b: linearToSrgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    a: alpha,
  };
}

/**
 * Perceptual mix, hex in / hex out. `t = 0` returns `a`, `t = 1` returns `b`.
 * Hue interpolates the short way around the wheel, so mixing red and magenta
 * does not detour through green. Port of `oklch::mix`.
 */
export function mix(a: string, b: string, t: number): string {
  const tt = Math.min(Math.max(t, 0), 1);
  const ca = parseHex(a);
  const cb = parseHex(b);
  const x = toOklch(ca);
  const y = toOklch(cb);

  let dh = (y.h - x.h) % 360;
  if (dh > 180) dh -= 360;
  else if (dh < -180) dh += 360;

  return formatHex(
    fromOklch(
      {
        l: x.l + (y.l - x.l) * tt,
        c: x.c + (y.c - x.c) * tt,
        h: x.h + dh * tt,
      },
      Math.round(ca.a + (cb.a - ca.a) * tt),
    ),
  );
}

/**
 * The bright counterpart of a normal ANSI color: lift lightness by 0.10
 * (capped at 0.95) and nudge chroma up 6%, rather than blending toward white —
 * blending toward white is what produces pale brights that leave the theme's
 * color family. Port of `oklch::brighten_ansi`.
 */
export function brightenAnsi(hex: string): string {
  const c = parseHex(hex);
  const lch = toOklch(c);
  return formatHex(
    fromOklch({ l: Math.min(lch.l + 0.1, 0.95), c: lch.c * 1.06, h: lch.h }, c.a),
  );
}

/** Relative luminance per WCAG. Port of `oklch::luminance` (same threshold). */
export function luminance(hex: string): number {
  const c = parseHex(hex);
  const ch = (v: number): number => {
    const x = v / 255;
    return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b);
}

/** WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white). */
export function contrastRatio(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  const hi = Math.max(la, lb);
  const lo = Math.min(la, lb);
  return (hi + 0.05) / (lo + 0.05);
}
