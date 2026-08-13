/**
 * The canvas bitmap follows the GRID, not the wrapper.
 *
 * Under size arbitration (#215) the session is the smallest attached client's
 * size, so the grid this pane paints can be a different shape than the pane
 * itself. Sizing the bitmap from the wrapper left stale pixels standing after
 * a foreign shrink: a canvas is only cleared by a bitmap-dims write, and a
 * wrapper that never moved never wrote them. Before the first keyframe the
 * grid is empty; the wrapper supplies the fallback so the canvas has a sane
 * size from the first frame.
 */

export interface CanvasMetrics {
  cellW: number;
  cellH: number;
  dpr: number;
}

export function canvasSizeFor(
  grid: { cols: number; rows: { length: number } },
  wrapper: { clientWidth: number; clientHeight: number },
  m: CanvasMetrics,
): { width: number; height: number } {
  const cols =
    grid.cols > 0
      ? grid.cols
      : Math.max(2, Math.floor((wrapper.clientWidth * m.dpr) / m.cellW));
  const rows =
    grid.rows.length > 0
      ? grid.rows.length
      : Math.max(1, Math.floor((wrapper.clientHeight * m.dpr) / m.cellH));
  return { width: cols * m.cellW, height: rows * m.cellH };
}
