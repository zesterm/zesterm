/**
 * `@zesterm/render` — the Canvas 2D grid painter.
 *
 * The seam is `(grid, dirtyRows) → paint`: deltas name dirty rows, repaints
 * are row-scoped, and colors resolve against the theme at paint time only.
 */

export type { Canvas2DLike } from './context.ts';
export {
  fontString,
  measureMetrics,
  type FontStyle,
  type MeasureContext,
  type Metrics,
} from './metrics.ts';
export {
  GridPainter,
  type GridViewLike,
  type PaintOptions,
  type PainterDeps,
} from './painter.ts';
