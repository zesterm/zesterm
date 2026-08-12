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
      const dirtyNow = pendingDirty;
      pendingDirty = new Set();
      if (painter) painter.paint(client.grid, dirtyNow);
    });
  };

  // Canvas bitmap only — the pty resize lives in TerminalView (one authority
  // for both modes). Same family/size/formula, so the bitmap and the pty
  // agree on cols/rows without this ever telling the client anything.
  const sizeToWrapper = (): void => {
    const m = metrics;
    const el = wrapper;
    const c = canvas;
    if (!m || !el || !c) return;
    const cols = Math.max(2, Math.floor((el.clientWidth * m.dpr) / m.cellW));
    const rows = Math.max(1, Math.floor((el.clientHeight * m.dpr) / m.cellH));
    c.width = cols * m.cellW;
    c.height = rows * m.cellH;
    c.style.width = `${c.width / m.dpr}px`;
    c.style.height = `${c.height / m.dpr}px`;
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

    sizeToWrapper();
    ctx.props.register({ schedulePaint });

    observer = new ResizeObserver(() => {
      sizeToWrapper();
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
      ref={(el: HTMLElement) => {
        wrapper = el;
      }}
    >
      <canvas
        ref={(el: HTMLCanvasElement) => {
          canvas = el;
        }}
      />
    </div>
  );
});
