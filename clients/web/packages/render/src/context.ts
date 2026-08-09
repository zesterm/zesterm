/**
 * The slice of `CanvasRenderingContext2D` the painter actually uses.
 *
 * Structural on purpose: the suite runs under `node --test` with no DOM, so the
 * tests drive a recording fake through this type and assert on the command
 * stream. Keeping the surface to six members is also what keeps the painter
 * honest — everything it draws is `fillRect` and `fillText`, so underline and
 * strikethrough are thin rects rather than stroked paths, and there is no
 * save/restore stack to leak state through.
 */
export interface Canvas2DLike {
  /**
   * `string | object`, not `string`: the DOM declares
   * `string | CanvasGradient | CanvasPattern`, and TypeScript relates mutable
   * properties covariantly, so a plain `string` here would make the real
   * context unassignable to this type. The painter only ever writes strings.
   */
  fillStyle: string | object;
  font: string;
  /**
   * `string` where the DOM has a literal union — the union is assignable to
   * `string`, so the real context still satisfies this. The painter sets it to
   * `'alphabetic'` once per paint and positions glyphs by `Metrics.baseline`.
   */
  textBaseline: string;
  globalAlpha: number;
  fillRect(x: number, y: number, w: number, h: number): void;
  fillText(text: string, x: number, y: number): void;
}
