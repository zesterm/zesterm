/**
 * How many cells fit in a box.
 *
 * One function, two callers, and the reason they must be one: a new session is
 * created *before* its pane exists, so the size has to be worked out from the
 * box the pane is about to fill — and if that arithmetic disagreed with the
 * arithmetic the pane itself uses a moment later, every new session would be
 * spawned at one size and immediately resized to another. The daemon lays the
 * shell's first prompt out at the size it was told, so the visible cost is the
 * first screen of output formatted for a width it never had.
 *
 * `Shell` hard-coded `120×32` here, which is that failure with the numbers
 * written down (#352).
 */

import { measureMetrics } from '@zesterm/render';

import { GRID_FONT_SIZE, MONO_FAMILY } from './chrome-model.ts';

export interface GridSize {
  readonly cols: number;
  readonly rows: number;
}

/**
 * What to ask for when nothing can be measured — no pane on screen yet, or a
 * browser that gave us no 2D context.
 *
 * 80×24 rather than something roomier on purpose: it is the size every shell,
 * every `less` and every curses program has assumed since VT100, so a session
 * that starts here and is corrected a frame later reflows from a layout
 * nothing renders badly. A too-*wide* guess is the one that hurts — output
 * already emitted at 120 columns cannot be rewrapped narrower without losing
 * the original line breaks.
 */
export const DEFAULT_GRID: GridSize = { cols: 80, rows: 24 };

/**
 * The arithmetic, with no DOM in it.
 *
 * Separate from `fitGrid` so it can be tested at all: the app's suite is
 * `node --test` with no document, and a cell size is a *number* rather than
 * something to eyeball through a rendered pane.
 */
export function gridFor(
  box: { readonly width: number; readonly height: number },
  cell: { readonly cellW: number; readonly cellH: number; readonly dpr: number },
): GridSize {
  // A pane that is not laid out yet measures zero, and the `Math.max` floors
  // below would turn that into a 2x1 terminal — a real size, silently absurd,
  // and one the daemon would lay a prompt out for. Fall back instead: "we
  // could not measure" and "it is two cells wide" are different answers and
  // only one of them is ever true. A non-finite cell size (a font that failed
  // to load measures NaN) is the same case wearing a different hat, which is
  // why these are written as `> 0` rather than as `=== 0`.
  if (box.width <= 0 || box.height <= 0) return DEFAULT_GRID;
  if (!(cell.cellW > 0) || !(cell.cellH > 0)) return DEFAULT_GRID;
  return {
    cols: Math.max(2, Math.floor((box.width * cell.dpr) / cell.cellW)),
    rows: Math.max(1, Math.floor((box.height * cell.dpr) / cell.cellH)),
  };
}

/**
 * Fit a grid to `el`'s content box.
 *
 * Grid metrics on purpose, in both modes, for the reason `TerminalView` gives:
 * the alt screen must fit its canvas exactly, while the blocks body only ever
 * gets narrower and wraps.
 */
export function fitGrid(el: Element | null, dpr: number): GridSize {
  if (el === null) return DEFAULT_GRID;
  const meas = document.createElement('canvas').getContext('2d');
  if (meas === null) return DEFAULT_GRID;
  return gridFor(
    { width: el.clientWidth, height: el.clientHeight },
    measureMetrics(meas, MONO_FAMILY, GRID_FONT_SIZE, dpr || 1),
  );
}
