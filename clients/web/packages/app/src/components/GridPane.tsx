/**
 * The alt-screen pane: the canvas and its painter, extracted verbatim from
 * `TerminalView` when the primary screen became DOM blocks (#151). This
 * component owns pixels only — the `SessionClient`, the hidden input
 * textarea, the connection chrome AND the pty's size stay in `TerminalView`,
 * shared with the blocks mode: a resize owned here would never fire on the
 * primary screen, freezing COLUMNS at whatever the directory reported.
 *
 * Dirty rows arrive by registration rather than by prop: the client's
 * `onChange` fires per delta, and threading that through reactive props would
 * re-render the component to deliver what is only a repaint. The parent holds
 * the hook and forwards; unmounting registers `null` so a delta landing
 * mid-switch paints nowhere rather than onto a detached canvas.
 */

import { component, onMounted, onUnmounted } from 'sigx';
import type { SessionClient, DirtyRows } from '@zesterm/client';
import { GridPainter, measureMetrics, type Metrics } from '@zesterm/render';
import { resolveTerminalPalette, type Theme } from '@zesterm/theme';

import { currentTheme, themeStore } from '../state/theme.ts';
import { GRID_FONT_SIZE, MONO_FAMILY } from '../chrome-model.ts';
import { canvasSizeFor } from '../grid-canvas.ts';

export interface GridPaneHooks {
  schedulePaint(dirty: DirtyRows): void;
}

export const GridPane = component<{
  client: SessionClient;
  theme: Theme;
  register: (hooks: GridPaneHooks | null) => void;
}>((ctx) => {
  const { client, theme } = ctx.props;

  let canvas: HTMLCanvasElement | null = null;
  let wrapper: HTMLElement | null = null;
  let painter: GridPainter | null = null;
  let metrics: Metrics | null = null;
  let observer: ResizeObserver | null = null;
  let unsubTheme: (() => void) | null = null;

  // Dirty rows accumulate between animation frames; one paint per frame no
  // matter how many deltas landed inside it.
  let pendingDirty: Set<number> | 'all' = 'all';
  let frameQueued = false;

  const schedulePaint = (dirty: DirtyRows): void => {
    if (pendingDirty !== 'all') {
      if (dirty === 'all') pendingDirty = 'all';
      else for (const r of dirty) pendingDirty.add(r);
    }
    if (frameQueued) return;
    frameQueued = true;
    requestAnimationFrame(() => {
      frameQueued = false;
      let dirtyNow = pendingDirty;
      pendingDirty = new Set();
      // A reshaped bitmap starts blank, so partial dirt would paint stripes
      // over emptiness.
      if (sizeToGrid()) dirtyNow = 'all';
      if (painter) painter.paint(client.grid, dirtyNow);
    });
  };

  // Canvas bitmap only — the pty resize lives in TerminalView (one authority
  // for both modes), and this never tells the client anything. The bitmap
  // follows the GRID rather than the wrapper: under size arbitration (#215)
  // another attached client can hold the session smaller than this pane, and
  // a wrapper-sized canvas kept the stale pixels below a foreign shrink
  // forever — a canvas is only cleared by a bitmap-dims write, and a wrapper
  // that never moved never wrote them.
  const sizeToGrid = (): boolean => {
    const m = metrics;
    const el = wrapper;
    const c = canvas;
    if (!m || !el || !c) return false;
    const { width, height } = canvasSizeFor(client.grid, el, m);
    if (c.width === width && c.height === height) return false;
    c.width = width;
    c.height = height;
    c.style.width = `${width / m.dpr}px`;
    c.style.height = `${height / m.dpr}px`;
    return true;
  };

  onMounted(() => {
    const c = canvas;
    const el = wrapper;
    if (!c || !el) return;
    const ctx2d = c.getContext('2d');
    if (!ctx2d) return;

    const dpr = window.devicePixelRatio || 1;
    const m = measureMetrics(ctx2d, MONO_FAMILY, GRID_FONT_SIZE, dpr);
    metrics = m;
    // currentTheme, not the prop: the prop is `store.theme` captured once at
    // boot, so a view mounted after a setTheme would paint the grid in the
    // stale theme until the NEXT switch (#133).
    painter = new GridPainter({
      ctx: ctx2d,
      metrics: m,
      palette: resolveTerminalPalette(currentTheme(theme).ui),
    });

    // A theme switch REBUILDS the painter rather than mutating it: its palette
    // is readonly, and its only other state is a cached font/fill string — a
    // setter would add mutable state for one call site. The rAF in
    // schedulePaint re-reads `painter`, so the next frame paints the new one.
    unsubTheme =
      themeStore()?.onThemeChange((next) => {
        painter = new GridPainter({
          ctx: ctx2d,
          metrics: m,
          palette: resolveTerminalPalette(next.ui),
        });
        schedulePaint('all');
      }) ?? null;

    sizeToGrid();
    ctx.props.register({ schedulePaint });

    // The wrapper only matters before the first keyframe (the fallback in
    // canvasSizeFor); after that a pane resize reaches this canvas as a
    // keyframe with a new shape, via TerminalView's resize and the daemon's
    // arbitration. The rAF re-runs sizeToGrid either way.
    observer = new ResizeObserver(() => {
      schedulePaint('all');
    });
    observer.observe(el);
    schedulePaint('all');
  });

  onUnmounted(() => {
    ctx.props.register(null);
    unsubTheme?.();
    observer?.disconnect();
  });

  return () => (
    <div
      class="grid-pane"
      ref={(el: HTMLElement | null) => {
        wrapper = el;
      }}
    >
      <canvas
        ref={(el: HTMLCanvasElement | null) => {
          canvas = el;
        }}
      />
    </div>
  );
});
