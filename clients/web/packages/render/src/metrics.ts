/**
 * The cell geometry the painter draws against.
 *
 * All lengths are in **device pixels**: the app sizes the canvas backing store
 * to `css * devicePixelRatio` and never calls `ctx.scale`, so a rect painted at
 * `col * cellW` lands on a physical pixel edge and a 1px underline is one
 * device pixel, not a blurry two. `dpr` is carried so line thicknesses can
 * scale with it.
 */
export interface Metrics {
  cellW: number;
  cellH: number;
  /** Alphabetic-baseline offset from the top of the row, in device pixels. */
  baseline: number;
  dpr: number;
  family: string;
  /** Font size in CSS pixels; the canvas font uses `size * dpr`. */
  size: number;
}

export interface FontStyle {
  bold?: boolean;
  italic?: boolean;
}

/**
 * The canvas font shorthand for a metrics + style pair.
 *
 * Only bold and italic vary per cell; family and size are fixed for the
 * grid — a terminal where BOLD changed the advance width would not be a grid.
 */
export function fontString(m: Metrics, style?: FontStyle): string {
  const italic = style?.italic === true ? 'italic ' : '';
  const bold = style?.bold === true ? 'bold ' : '';
  return `${italic}${bold}${m.size * m.dpr}px ${m.family}`;
}

/** What `measureMetrics` needs from a context — settable font, measurable text. */
export interface MeasureContext {
  font: string;
  measureText(text: string): { width: number };
}

/**
 * Derive cell geometry the way terminals conventionally do.
 *
 * - `cellW` is the advance of 'M' — in a monospaced font every advance is the
 *   same, and 'M' is the traditional probe.
 * - `cellH` is `1.4 * em`: tighter than that and descenders touch the next
 *   row's ascenders; looser and a TUI's box-drawing rows visibly gap. 1.4 is a
 *   choice, not a law — the app can hand the painter its own `Metrics`.
 * - `baseline` places the em box centered in the leading: half the extra
 *   height above, half below, with ascent taken as the conventional 0.8em.
 *   That works out to exactly `(1.4 - 1.0) / 2 * em + 0.8 * em = 1.0 * em`.
 *
 * `actualBoundingBox*` metrics would be more precise and are deliberately not
 * used: they vary per glyph and per engine, and a grid needs one answer.
 */
export function measureMetrics(
  ctx: MeasureContext,
  family: string,
  size: number,
  dpr: number,
): Metrics {
  const em = size * dpr;
  ctx.font = `${em}px ${family}`;
  return {
    cellW: Math.ceil(ctx.measureText('M').width),
    cellH: Math.ceil(em * 1.4),
    baseline: Math.round(em),
    dpr,
    family,
    size,
  };
}
