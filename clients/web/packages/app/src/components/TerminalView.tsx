/**
 * One attached session: the mode switch, its input, and the chrome around it.
 *
 * The primary screen renders as DOM command blocks (`BlocksPane`, design §3);
 * the alt screen keeps the canvas painter (`GridPane`) — a full-screen app is
 * a grid and blocks would misrender it, while a shell session is blocks and a
 * grid loses the block semantics. The switch rides `client.grid.altScreen`,
 * which the wire flips.
 *
 * The wiring stays deliberately thin — every behaviour lives in a tested
 * package or the pure `blocks-pane-model`. `SessionClient` owns the protocol,
 * `@zesterm/input` owns key bytes; this component owns only the DOM seams
 * (rAF batching of the model, event plumbing) and the status chrome. The
 * hidden textarea (the IME path), focus handling and the connection banner
 * live HERE, shared by both modes, so switching screens never drops focus or
 * a mid-flight composition.
 */

import { component, onMounted, onUnmounted, signal } from 'sigx';
import type { ClientSigner } from '@zesterm/auth';
import { SessionClient, type ConnectionState, type Dial } from '@zesterm/client';
import {
  belongsToBrowser,
  encodeComposedText,
  encodeFocus,
  encodeKey,
  encodePaste,
  modsOf,
  shellChord,
} from '@zesterm/input';
import { sliceBlocks, type BlockPayload } from '@zesterm/proto';
import { measureMetrics } from '@zesterm/render';
import { resolveTerminalPalette, type Theme } from '@zesterm/theme';
import type { SessionEntry } from '@zesterm/control';

import { GRID_FONT_SIZE, MONO_FAMILY } from '../chrome-model.ts';
import { currentTheme, themeStore } from '../state/theme.ts';
import { NO_FOLDS, foldedFor, toggle, type FoldsState } from '../state/folds.ts';
import {
  atShellPrompt,
  copyOutputText,
  linkOf,
  mostRecentBlockWithOutput,
  paneModel,
  type LinkHealth,
  type RenderItem,
} from '../blocks-pane-model.ts';
import type { LinkState } from '../state/tabs.ts';
import { GridPane, type GridPaneHooks } from './GridPane.tsx';
import { BlocksPane } from './BlocksPane.tsx';

/**
 * Module-scoped so folds survive leaving a session and coming back within one
 * page life — the design keys them by the full (host, session) pair precisely
 * so one shared map serves every session without cross-talk.
 */
let folds: FoldsState = NO_FOLDS;

/**
 * The shell's reach into a mounted session — what the command palette needs
 * and no more. Registered on mount and revoked on unmount, so the shell's map
 * of these IS the set of grids the browser actually holds: the palette's
 * "N hosts searched" honesty rests on nothing surviving here past close().
 */
export interface TerminalHooks {
  /** The live grid's block index, read at call time. */
  blocks(): readonly BlockPayload[];
  /** Type `command` + CR, under the ⌘⇧R gate; declines rather than risking stdin. */
  runCommand(command: string): void;
  /** Refocus the hidden textarea — where dismissed overlays send focus home. */
  focus(): void;
}

export const TerminalView = component<{
  entry: SessionEntry;
  /**
   * How to open the socket, rather than where it goes.
   *
   * A relay plane never becomes a URL: a ticket has to be minted first, and it
   * rides the subprotocol of a socket this component does not construct. So
   * the caller hands over a `Dial` — reusable, because `SessionClient` calls it
   * again on every redial — and this file stops knowing which world it is in.
   */
  dial: Dial;
  signer: ClientSigner;
  theme: Theme;
  /** The tab chip owns the visible title now; this is how it learns it. */
  onTitle?: (title: string) => void;
  /** Link health surfaces on the tab, not on a status bar the design removed. */
  onLink?: (link: LinkState) => void;
  /** The palette's seam; called with null on unmount so the shell's map stays honest. */
  register?: (hooks: TerminalHooks | null) => void;
}>((ctx) => {
  const { entry, dial, signer, theme } = ctx.props;

  const status = signal<{ state: ConnectionState; exited: number | null | false }>({
    state: { phase: 'connecting' },
    exited: false,
  });
  const pane = signal<{
    alt: boolean;
    items: readonly RenderItem[];
    link: LinkHealth;
    /** Bumped on theme switches so the blocks mode re-resolves its palette. */
    themeEpoch: number;
  }>({ alt: entry.altScreen, items: [], link: 'live', themeEpoch: 0 });

  /**
   * The real focus target — a hidden textarea, because composition events
   * only exist for editable elements. An IME commit, the emoji picker and
   * dictation all arrive as composed *text*, not keydowns; a bare div sees
   * none of it, which is exactly how the first live run typed an emoji and
   * the shell received nothing.
   */
  let inputEl: HTMLTextAreaElement | null = null;
  let wrapEl: HTMLElement | null = null;
  let gridHooks: GridPaneHooks | null = null;
  let sizeObserver: ResizeObserver | null = null;
  let unsubTheme: (() => void) | null = null;
  let ticker: ReturnType<typeof setInterval> | null = null;

  // Model recomputation is rAF-batched exactly like the canvas paints were:
  // one recompute per frame no matter how many deltas landed inside it.
  let modelQueued = false;

  const foldedIds = (): ReadonlySet<number> =>
    new Set([...foldedFor(folds, entry.host, entry.session)].map(Number));

  const recomputeModel = (): void => {
    pane.items = paneModel(client.grid, foldedIds(), pane.link);
  };

  const scheduleModel = (): void => {
    if (modelQueued) return;
    modelQueued = true;
    requestAnimationFrame(() => {
      modelQueued = false;
      recomputeModel();
    });
  };

  // Constructed in setup, not onMounted: both panes take it as a prop, and
  // the first render happens before mount. Nothing here touches the DOM —
  // the socket opens in connect(), below.
  const client = new SessionClient({
    dial,
    signer,
    label: 'zesterm-web',
    session: { host: entry.host, session: BigInt(entry.session) },
    cols: entry.cols,
    rows: entry.rows,
    events: {
      onChange: (dirty) => {
        const alt = client.grid.altScreen;
        if (alt !== pane.alt) pane.alt = alt;
        if (alt) gridHooks?.schedulePaint(dirty);
        else scheduleModel();
      },
      onBlocksChanged: () => {
        if (!client.grid.altScreen) scheduleModel();
      },
      onTitle: (title) => {
        // The tab chip owns the visible title now (the in-pane header is
        // gone); the document title keeps naming the browser tab.
        document.title = title === '' ? 'zesterm' : `${title} — zesterm`;
        ctx.props.onTitle?.(title);
      },
      onConnection: (state) => {
        status.state = state;
        const link = linkOf(state);
        if (link !== pane.link) {
          // The link is a model input (§4's degraded states), not only chrome.
          pane.link = link;
          scheduleModel();
        }
        // The same value feeds the tab chip — link health surfaces there,
        // not on a status bar the design removed.
        ctx.props.onLink?.(link);
        if (state.phase === 'connected') gridHooks?.schedulePaint('all');
      },
      onExited: (code) => {
        status.exited = code;
      },
    },
  });

  onMounted(() => {
    // The pty's size belongs HERE, not to a pane: the blocks mode is the
    // default screen, and a session that never entered the alt screen would
    // otherwise keep the directory's dims forever — COLUMNS frozen at a width
    // unrelated to the visible pane, every command's output formatted for it.
    // Grid metrics on purpose, in both modes: the alt screen must fit its
    // canvas exactly, while the blocks body (same family, 12.5px) only gets
    // narrower and wraps per invariant 11. Sized before connect() so the
    // attach itself carries the measured dims rather than entry's.
    const meas = document.createElement('canvas').getContext('2d');
    if (meas !== null && wrapEl !== null) {
      const m = measureMetrics(meas, MONO_FAMILY, GRID_FONT_SIZE, window.devicePixelRatio || 1);
      const fit = (): void => {
        if (wrapEl === null) return;
        client.resize(
          Math.max(2, Math.floor((wrapEl.clientWidth * m.dpr) / m.cellW)),
          Math.max(1, Math.floor((wrapEl.clientHeight * m.dpr) / m.cellH)),
        );
      };
      fit();
      sizeObserver = new ResizeObserver(fit);
      sizeObserver.observe(wrapEl);
    }
    client.connect();
    // The blocks palette resolves at render time; a theme switch only needs
    // a re-render. GridPane rebuilds its own painter on the same event.
    unsubTheme = themeStore()?.onThemeChange(() => {
      pane.themeEpoch += 1;
    }) ?? null;
    // 'running 4.2s' must count while a silent command runs — deltas are the
    // usual clock, but a `sleep 30` sends none for 30 seconds.
    ticker = setInterval(() => {
      const g = client.grid;
      if (g.altScreen) return;
      const last = g.blocks[g.blocks.length - 1];
      if (last !== undefined && last.end_line === null && last.state.state === 'running') {
        scheduleModel();
      }
    }, 1000);
    ctx.props.register?.({
      blocks: () => client.grid.blocks,
      runCommand,
      focus: () => inputEl?.focus(),
    });
    inputEl?.focus();
  });

  onUnmounted(() => {
    ctx.props.register?.(null);
    if (ticker !== null) clearInterval(ticker);
    sizeObserver?.disconnect();
    unsubTheme?.();
    client.close();
    document.title = 'zesterm';
  });

  const platform: 'mac' | 'other' = navigator.platform.toLowerCase().includes('mac')
    ? 'mac'
    : 'other';

  const copyOutput = (): void => {
    const target = mostRecentBlockWithOutput(sliceBlocks(client.grid));
    if (target === null) return;
    // copyOutputText, not a '\n' join: `wrapped` rejoins soft-wrapped lines.
    const text = copyOutputText(target.outputRows, client.grid.attrs);
    // writeText rejects in insecure contexts and on denied permission; an
    // unhandled rejection in the console for a copy that simply didn't take
    // helps nobody, and there is no toast surface yet to say more.
    navigator.clipboard.writeText(text).catch(() => {});
  };

  const runCommand = (command: string): void => {
    // Running a command TYPES, so it may fire only when the shell is the
    // thing reading: primary screen, trailing block an open prompt
    // (atShellPrompt, tested). During a running command the replay would land
    // in that command's stdin; in the alt screen it would land in the
    // full-screen app's document. Every caller — the ⌘⇧R chord, the header
    // chip, and the palette's ⏎ on a block row — passes through here, so the
    // gate covers all three or none.
    if (client.grid.altScreen || !atShellPrompt(client.grid.blocks) || command === '') return;
    const bytes = encodeComposedText(command);
    if (bytes !== null) client.input(bytes);
    // Enter exactly as the key path sends it: a bare CR (key.ts's 'Enter' arm).
    client.input(Uint8Array.of(0x0d));
  };

  const reRun = (): void => {
    const target = mostRecentBlockWithOutput(sliceBlocks(client.grid));
    if (target === null) return;
    runCommand(target.block.command);
  };

  const onToggleFold = (blockId: number): void => {
    folds = toggle(folds, entry.host, entry.session, String(blockId));
    // Immediate, not rAF-batched: a click deserves a same-tick response.
    recomputeModel();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    // Mid-composition keydowns are the IME's business ('Process' keys and
    // the like); encoding them would type fragments the commit then repeats.
    if (e.isComposing) return;
    const mods = modsOf(e);
    // The shell's chords run BEFORE the browser split — chords.ts pins that
    // ordering. Only the two block actions are claimed here; the rest of the
    // table (palette, split, tabs) is the app shell's to wire when the
    // tab-strip work lands, and claiming them from inside one session's view
    // would strand them when a second view exists.
    // Claimed only on the primary screen: the block actions are meaningless
    // over a full-screen app, and a claimed-but-swallowed chord would eat a
    // keystroke the app (kitty-protocol aware or not) was entitled to see.
    const action = client.grid.altScreen ? null : shellChord(e, mods, platform);
    if (action !== null && (action.kind === 'copy-output' || action.kind === 're-run')) {
      e.preventDefault();
      if (action.kind === 'copy-output') copyOutput();
      else reRun();
      return;
    }
    if (belongsToBrowser(e, mods, platform)) return;
    const bytes = encodeKey(e, mods, client.grid.modes);
    if (bytes === null) return;
    // preventDefault on a handled key also suppresses the textarea's own
    // `input` event, which is the coordination that keeps ordinary typing
    // from arriving twice — once encoded, once as composed text.
    e.preventDefault();
    client.input(bytes);
  };

  /**
   * A key coming back up.
   *
   * Silent for every program that has not turned on kitty event types, which is
   * almost all of them: the encoder returns null and nothing is written. Bound
   * unconditionally anyway, because the flags are the host's to change at any
   * moment and a listener attached only on demand would miss the first release
   * after a program asked.
   */
  const onKeyUp = (e: KeyboardEvent): void => {
    if (e.isComposing) return;
    const mods = modsOf(e);
    if (belongsToBrowser(e, mods, platform)) return;
    const bytes = encodeKey(e, mods, client.grid.modes);
    if (bytes === null) return;
    e.preventDefault();
    client.input(bytes);
  };

  const clearInput = (): void => {
    if (inputEl) inputEl.value = '';
  };

  const sendComposed = (text: string): void => {
    const bytes = encodeComposedText(text);
    if (bytes !== null) client.input(bytes);
    clearInput();
  };

  // Chrome's order is beforeinput → input → compositionend, so the commit is
  // read HERE, once; the in-flight `insertCompositionText` events are skipped
  // below rather than sent as they mutate.
  const onCompositionEnd = (e: CompositionEvent): void => {
    sendComposed(e.data);
  };

  const onInput = (e: Event): void => {
    const ev = e as InputEvent;
    if (ev.isComposing || ev.inputType === 'insertCompositionText') return;
    // The non-composition insertions: the emoji picker, dictation, autofill —
    // anything that writes text without keydowns the encoder handled.
    const text = inputEl?.value ?? '';
    if (text !== '') sendComposed(text);
  };

  const onPaste = (e: ClipboardEvent): void => {
    const text = e.clipboardData?.getData('text') ?? '';
    if (text === '') return;
    e.preventDefault();
    client.input(encodePaste(text, client.grid.modes));
  };

  const sendFocus = (focused: boolean): void => {
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

  return () => {
    // Read so a theme switch re-renders this fn; the epoch value is unused.
    void pane.themeEpoch;
    const palette = resolveTerminalPalette(currentTheme(theme).ui);
    return (
      <div class="terminal-view">
        <div
          class="term-wrap"
          ref={(el: HTMLElement) => {
            wrapEl = el;
          }}
          onMouseDown={() => {
            // Clicking anywhere in the terminal focuses the hidden input —
            // preventDefault would break future selection, so focus rides the
            // next tick instead of fighting the browser's own focus handling.
            setTimeout(() => inputEl?.focus(), 0);
          }}
        >
          {/* The header row this used to carry belongs to the tab chrome now
              (the chip owns the title via onTitle); connection state keeps a
              small overlay so a reconnect is never silent inside a pane. */}
          {banner(status.state, status.exited) !== null ? (
            <div class="term-banner">{banner(status.state, status.exited)}</div>
          ) : null}
          {pane.alt ? (
            <GridPane
              client={client}
              theme={theme}
              register={(hooks) => {
                gridHooks = hooks;
              }}
            />
          ) : (
            <BlocksPane
              items={pane.items}
              link={pane.link}
              palette={palette}
              onToggleFold={onToggleFold}
              onCopyOutput={copyOutput}
              onReRun={reRun}
            />
          )}
          <textarea
            class="term-input"
            ref={(el: HTMLTextAreaElement) => {
              inputEl = el;
            }}
            autoComplete="off"
            autocapitalize="off"
            spellCheck={false}
            onKeyDown={onKeyDown}
            onKeyUp={onKeyUp}
            onCompositionEnd={onCompositionEnd}
            onInput={onInput}
            onPaste={onPaste}
            onFocus={() => sendFocus(true)}
            onBlur={() => sendFocus(false)}
          />
        </div>
      </div>
    );
  };
});
