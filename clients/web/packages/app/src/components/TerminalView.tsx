/**
 * One attached session: the canvas, its input, and the chrome around it.
 *
 * The wiring is deliberately thin — every behaviour lives in a tested
 * package. `SessionClient` owns the protocol, `GridPainter` owns pixels,
 * `@zesterm/input` owns key bytes; this component owns only the DOM seams
 * between them (rAF batching, ResizeObserver sizing, event plumbing) and the
 * status chrome.
 */

import { component, onMounted, onUnmounted, signal } from 'sigx';
import type { ClientIdentity } from '@zesterm/auth';
import {
  SessionClient,
  type ConnectionState,
  type DirtyRows,
} from '@zesterm/client';
import { belongsToBrowser, encodeFocus, encodeKey, encodePaste, modsOf } from '@zesterm/input';
import type { BlockPayload } from '@zesterm/proto';
import { GridPainter, measureMetrics, type Metrics } from '@zesterm/render';
import { resolveTerminalPalette, type Theme } from '@zesterm/theme';
import type { SessionEntry } from '@zesterm/control';

import { wsDial } from '../ws-dial.ts';

const FONT_FAMILY = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';
const FONT_SIZE = 13;

export const TerminalView = component<{
  entry: SessionEntry;
  dataPlaneUrl: string;
  identity: ClientIdentity;
  theme: Theme;
  onBack?: () => void;
}>((ctx) => {
  const { entry, dataPlaneUrl, identity, theme } = ctx.props;

  const status = signal<{ state: ConnectionState; exited: number | null | false; title: string }>({
    state: { phase: 'connecting' },
    exited: false,
    title: entry.title,
  });
  const blocks = signal<{ list: readonly BlockPayload[] }>({ list: [] });

  let canvas: HTMLCanvasElement | null = null;
  let wrapper: HTMLElement | null = null;
  let painter: GridPainter | null = null;
  let metrics: Metrics | null = null;
  let client: SessionClient | null = null;
  let observer: ResizeObserver | null = null;

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
      if (painter && client) painter.paint(client.grid, dirtyNow);
    });
  };

  const sizeToWrapper = (): { cols: number; rows: number } => {
    const m = metrics;
    const el = wrapper;
    const c = canvas;
    if (!m || !el || !c) return { cols: entry.cols, rows: entry.rows };
    const cols = Math.max(2, Math.floor((el.clientWidth * m.dpr) / m.cellW));
    const rows = Math.max(1, Math.floor((el.clientHeight * m.dpr) / m.cellH));
    c.width = cols * m.cellW;
    c.height = rows * m.cellH;
    c.style.width = `${c.width / m.dpr}px`;
    c.style.height = `${c.height / m.dpr}px`;
    return { cols, rows };
  };

  onMounted(() => {
    const c = canvas;
    const el = wrapper;
    if (!c || !el) return;
    const ctx2d = c.getContext('2d');
    if (!ctx2d) return;

    const dpr = window.devicePixelRatio || 1;
    metrics = measureMetrics(ctx2d, FONT_FAMILY, FONT_SIZE, dpr);
    painter = new GridPainter({
      ctx: ctx2d,
      metrics,
      palette: resolveTerminalPalette(theme.ui),
    });

    const { cols, rows } = sizeToWrapper();
    client = new SessionClient({
      dial: wsDial(dataPlaneUrl),
      identity,
      label: 'zesterm-web',
      session: { host: entry.host, session: BigInt(entry.session) },
      cols,
      rows,
      events: {
        onChange: schedulePaint,
        onBlocksChanged: () => {
          blocks.list = client ? [...client.grid.blocks] : [];
        },
        onTitle: (title) => {
          status.title = title;
          document.title = title === '' ? 'zesterm' : `${title} — zesterm`;
        },
        onConnection: (state) => {
          status.state = state;
          if (state.phase === 'connected') schedulePaint('all');
        },
        onExited: (code) => {
          status.exited = code;
        },
      },
    });
    client.connect();

    observer = new ResizeObserver(() => {
      const size = sizeToWrapper();
      client?.resize(size.cols, size.rows);
      schedulePaint('all');
    });
    observer.observe(el);
    el.focus();
  });

  onUnmounted(() => {
    observer?.disconnect();
    client?.close();
    document.title = 'zesterm';
  });

  const platform: 'mac' | 'other' = navigator.platform.toLowerCase().includes('mac')
    ? 'mac'
    : 'other';

  const onKeyDown = (e: KeyboardEvent): void => {
    if (!client) return;
    const mods = modsOf(e);
    if (belongsToBrowser(e, mods, platform)) return;
    const bytes = encodeKey(e, mods, client.grid.modes);
    if (bytes === null) return;
    e.preventDefault();
    client.input(bytes);
  };

  const onPaste = (e: ClipboardEvent): void => {
    if (!client) return;
    const text = e.clipboardData?.getData('text') ?? '';
    if (text === '') return;
    e.preventDefault();
    client.input(encodePaste(text, client.grid.modes));
  };

  const sendFocus = (focused: boolean): void => {
    if (!client) return;
    const bytes = encodeFocus(focused, client.grid.modes);
    if (bytes !== null) client.input(bytes);
  };

  const banner = (state: ConnectionState, exited: number | null | false): string | null => {
    if (exited !== false) {
      return exited === null ? 'session ended' : `session ended (exit ${exited})`;
    }
    switch (state.phase) {
      case 'connecting':
        return 'connecting…';
      case 'awaiting-approval':
        return `waiting for approval on the host — compare code ${state.code}`;
      case 'reconnecting':
        return 'connection lost — reconnecting';
      case 'failed':
        return `refused: ${state.message}`;
      case 'connected':
        return null;
    }
  };

  const duration = (b: BlockPayload): string => {
    if (b.started_ms === undefined || b.ended_ms === undefined) return '';
    return ` · ${((b.ended_ms - b.started_ms) / 1000).toFixed(1)}s`;
  };

  return () => (
    <div class="terminal-view">
      <header class="term-header">
        <button class="back" onClick={() => ctx.props.onBack?.()}>
          ← sessions
        </button>
        <span class="term-title">{status.title === '' ? 'shell' : status.title}</span>
        {banner(status.state, status.exited) !== null ? (
          <span class="term-banner">{banner(status.state, status.exited)}</span>
        ) : null}
      </header>

      <div
        class="term-wrap"
        tabIndex={0}
        ref={(el: HTMLElement) => {
          wrapper = el;
        }}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        onFocus={() => sendFocus(true)}
        onBlur={() => sendFocus(false)}
      >
        <canvas
          ref={(el: HTMLCanvasElement) => {
            canvas = el;
          }}
        />
      </div>

      {blocks.list.length > 0 ? (
        <aside class="block-rail">
          {/* The daemon's verdicts verbatim — a null exit code renders as a
              question, never a green tick. The UI computes nothing. */}
          {[...blocks.list].slice(-8).map((b) => (
            <div class={`block-chip ${b.state.state}`}>
              <span class="cmd">{b.command === '' ? '·' : b.command}</span>
              <span class="verdict">
                {b.state.state === 'finished'
                  ? b.state.exit_code === null
                    ? 'exit ?'
                    : `exit ${b.state.exit_code}`
                  : b.state.state}
                {duration(b)}
              </span>
            </div>
          ))}
        </aside>
      ) : null}
    </div>
  );
});
