/**
 * The blocks pane's render model — the design's §3 (command blocks) and §4
 * (degraded states) as data, computed pure so `node --test` can prove every
 * header field, the fold behaviour and the interrupted predicate without a
 * DOM, a canvas or a connection.
 *
 * The split with `BlocksPane.tsx` is deliberate: everything a reviewer would
 * want to assert — "exit ? never renders a green tick", "folding drops the
 * output and adds the count", "a block whose host went away mid-run reads
 * interrupted" — is decided here; the component only turns items into
 * elements. The block outcomes themselves come from the wire verbatim
 * (`BlockPayload.state`); this file formats, it never judges — the mockup's
 * rule that the UI must never compute a block's outcome locally.
 */

import {
  expandRow,
  outputLineCount,
  rowSpans,
  rowText,
  sliceBlocks,
  type AttrDef,
  type BlockPayload,
  type BlockSlice,
  type BlocksLayout,
  type CursorState,
  type RowPayload,
  type Span,
} from '@zesterm/proto';
import type { ConnectionState } from '@zesterm/client';

/**
 * The transport's health, as the pane cares about it. `stalled` has no
 * producer yet — `ConnectionState` carries no such phase — but §4 specifies
 * its rendering ('buffering'), so the model accepts it now and the mapping in
 * `TerminalView` grows the detection later without this file changing.
 */
export type LinkHealth = 'live' | 'stalled' | 'reconnecting';

/**
 * ConnectionState → link health, one mapping feeding two consumers: the pane
 * model (§4's degraded states) and the tab chip. connecting / awaiting-approval
 * read as stalled — a tab must not claim live before the link ever existed.
 * 'failed' — the client gave up redialling for good — is *more* degraded than
 * reconnecting, not less: anything short of 'reconnecting' lifts §4's
 * rendering at exactly the moment the link died for good, flipping an
 * interrupted open block back to a 'running Ns' counter that ticks forever
 * ('stalled' is late data, not a lost host — it interrupts nothing). The
 * connection banner names the refusal precisely; the pane and the tab only
 * have to stay degraded. Delta-silence stall detection slots in here when it
 * exists.
 */
export function linkOf(state: ConnectionState): LinkHealth {
  switch (state.phase) {
    case 'connected':
      return 'live';
    case 'reconnecting':
    case 'failed':
      return 'reconnecting';
    case 'connecting':
    case 'awaiting-approval':
      return 'stalled';
  }
}

export type RailToken = 'success' | 'danger' | 'warn' | 'faint';

export type Outcome =
  /** `code: null` renders 'exit ?' — a shell that omitted the status is not a shell that succeeded. */
  | { readonly kind: 'exit'; readonly code: number | null }
  /** `seconds: null` when the host never stamped `started_ms`; the ring still spins. */
  | { readonly kind: 'running'; readonly seconds: number | null }
  | { readonly kind: 'interrupted' };

/** One grid row as styled runs, ready for a text renderer. */
export interface PaneRow {
  readonly line: number;
  readonly spans: readonly Span[];
  /**
   * The digit a tap on this row should type, when the row is a numbered
   * option of the RUNNING command — an agent CLI's question, a menu. Null
   * everywhere else: a finished block's "1. foo" is history, and a prompt
   * is never running. `optionOf` decides the shape; `paneModel` decides
   * which rows are eligible. (#421)
   */
  readonly option: string | null;
}

/**
 * The option number a row of output offers, or null.
 *
 * A heuristic, so it is narrow on purpose: an optional selection marker
 * (`❯`, `>`, `›`), one or two digits, a `.` or `)`, whitespace, then
 * text. That is how Claude Code's questions and permission prompts, and
 * most numbered menus, render their choices — and a tap types exactly the
 * digit, never an Enter: the program decides what a digit means, and
 * guessing a CR into a running command's stdin is the one thing this
 * must not do. Rows that merely start with a number ("1 file changed",
 * "2024-01-01") do not match.
 */
export function optionOf(text: string): string | null {
  const m = /^\s*(?:[❯>›]\s*)?(\d{1,2})[.)]\s+\S/.exec(text);
  return m === null ? null : (m[1] as string);
}

export interface HeaderItem {
  readonly kind: 'header';
  readonly blockId: number;
  readonly railToken: RailToken;
  /** Only a block with output folds — a chevron on nothing is a lie. */
  readonly foldable: boolean;
  readonly folded: boolean;
  /** `null` when not foldable; the view renders no chevron at all. */
  readonly chevron: '▾' | '▸' | null;
  readonly command: string;
  readonly cwd: string;
  /** Empty while running, or when the host never stamped the timestamps. */
  readonly durationText: string;
  readonly outcome: Outcome;
  /** What the folded header's 'N lines' says — grid rows, matching the unfold. */
  readonly foldedLineCount: number;
}

export type RenderItem =
  /** Preamble or tail rows — history outside any block. */
  | { readonly kind: 'rows'; readonly key: string; readonly rows: readonly PaneRow[] }
  | HeaderItem
  | { readonly kind: 'output'; readonly blockId: number; readonly rows: readonly PaneRow[] }
  /** The live shell prompt awaiting input; the view appends the blinking caret. */
  | { readonly kind: 'prompt'; readonly rows: readonly PaneRow[]; readonly chips: readonly PromptChip[] }
  | { readonly kind: 'overlay'; readonly link: 'stalled' | 'reconnecting'; readonly text: string };

/**
 * One context chip on the live prompt (#420): where the session stands, by
 * its own daemon's word, so the phone shows the same chips the desktop does.
 * Display only — the facts behind these carry a trust label and nothing may
 * gate behavior on one; a chip is a thing to read and copy.
 */
export interface PromptChip {
  /** Identity and css-class suffix: `cwd`, `git`, `venv`, … */
  readonly key: string;
  readonly label: string;
  /** What a click copies — the whole path, the bare branch. */
  readonly value: string;
}

/** Fact keys worth a chip, and the shorter name the chip wears. */
const CHIP_NAMES: ReadonlyMap<string, string> = new Map([
  ['venv', 'venv'],
  ['conda', 'conda'],
  ['kube', 'kube'],
  ['aws_profile', 'aws'],
  ['node', 'node'],
  ['ssh_host', 'ssh'],
]);

/**
 * Chips for a session's context, in a fixed order (cwd, git, then facts as
 * the daemon sent them). Structural parameter for the `sessionEntryOf`
 * reason: the shape is `SessionEntry.context`'s, and a drift fails at the
 * call site rather than here. An unknown fact key is one chip fewer, never
 * an error — the `HostOffer` rule.
 */
export function promptChips(
  cwd: string,
  context: {
    readonly git: { readonly branch: string; readonly dirty: boolean | null } | null;
    readonly facts: readonly { readonly key: string; readonly value: string }[];
  } | null,
): PromptChip[] {
  const chips: PromptChip[] = [];
  if (cwd !== '') {
    chips.push({ key: 'cwd', label: shortenHome(cwd), value: cwd });
  }
  if (context !== null) {
    if (context.git !== null) {
      const label = context.git.dirty === true ? `${context.git.branch}*` : context.git.branch;
      chips.push({ key: 'git', label, value: context.git.branch });
    }
    for (const f of context.facts) {
      const name = CHIP_NAMES.get(f.key);
      if (name === undefined) continue;
      chips.push({ key: name, label: `${name} ${f.value}`, value: f.value });
    }
  }
  return chips;
}

/**
 * `~` for the *viewer-agnostic* prefixes only. The web client cannot know
 * another machine's home, so it shortens just the two spellings the daemons
 * themselves report under, and a path that matches neither passes through —
 * a wrong `~` is worse than a long path.
 */
function shortenHome(path: string): string {
  const m = /^(\/(?:Users|home)\/[^/]+)(\/.*)?$/.exec(path);
  if (m === null) return path;
  return `~${m[2] ?? ''}`;
}

/** The slice of `GridView` the model reads — structural, so tests hand-build it. */
export interface PaneView {
  readonly rows: readonly RowPayload[];
  readonly scrollback: readonly RowPayload[];
  readonly blocks: readonly BlockPayload[];
  readonly cursor: CursorState;
  readonly attrs: ReadonlyMap<number, AttrDef>;
}

/**
 * §4's "block whose host went away mid-run": the command was still producing
 * output (open and running) when the link dropped. A trailing *prompt*-state
 * block is open too and is deliberately not interrupted — nothing was running
 * in it, and 'interrupted' on an idle prompt would report a failure that
 * never happened.
 */
export function isInterrupted(block: BlockPayload, link: LinkHealth): boolean {
  return block.end_line === null && block.state.state === 'running' && link === 'reconnecting';
}

/**
 * The design's three shapes — `0.41s`, `51.2s`, `4m 12s` — with the
 * boundaries where a digit would otherwise vanish: two decimals under 10s,
 * one under a minute, whole seconds beyond.
 */
export function formatDuration(ms: number): string {
  const s = ms / 1000;
  if (s < 10) return `${s.toFixed(2)}s`;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s - m * 60);
  // Rounding can carry to a whole minute — '4m 60s' reads as a bug.
  return rem === 60 ? `${m + 1}m 0s` : `${m}m ${rem}s`;
}

/**
 * The one selector behind BOTH ⌘⇧O (copy output) and ⌘⇧R (re-run) — §3:
 * "Both target the most recent block **with output**, not the block the
 * cursor is in." One function so the two chords can never disagree about
 * which block they mean.
 */
export function mostRecentBlockWithOutput(layout: BlocksLayout): BlockSlice | null {
  for (let i = layout.slices.length - 1; i >= 0; i--) {
    const s = layout.slices[i] as BlockSlice;
    if (s.outputRows.length > 0) return s;
  }
  return null;
}

/**
 * A block's output as clipboard text. `wrapped` is load-bearing: it marks a
 * grid row whose line continues onto the next row (soft-wrapped at the
 * terminal's width), and the wire carries it precisely so a copy can rejoin
 * the line — `delta.rs` on `RowPayload.wrapped`: "so a client can copy a
 * wrapped line without a spurious newline". Joining every row with '\n'
 * would put newlines into text the command never printed.
 */
export function copyOutputText(
  rows: readonly RowPayload[],
  attrs: ReadonlyMap<number, AttrDef>,
): string {
  let text = '';
  let prevWrapped = true; // nothing precedes the first row, so no separator
  for (const r of rows) {
    if (!prevWrapped) text += '\n';
    text += rowText(expandRow(r, 0, attrs));
    prevWrapped = r.wrapped;
  }
  return text;
}

/**
 * Whether the shell itself is reading — the trailing block open in prompt
 * state, the same shape `paneModel` renders as the prompt line. ⌘⇧R *types*,
 * so this is its gate: during a running command the "most recent block with
 * output" is that very command, and the replayed text plus CR would land in
 * its stdin rather than re-run anything; with no blocks at all there is no
 * prompt to trust. Copy needs no such gate — a clipboard write disturbs
 * nothing.
 */
export function atShellPrompt(blocks: readonly BlockPayload[]): boolean {
  const last = blocks[blocks.length - 1];
  return last !== undefined && last.end_line === null && last.state.state === 'prompt';
}

/**
 * Browser zoom and DPR scaling leave fractional-pixel gaps between
 * `scrollTop + clientHeight` and `scrollHeight` even when the user never
 * scrolled, so "at the bottom" needs slack — but well under one row (~21px),
 * or a deliberate one-row nudge upward would be overridden.
 */
const FOLLOW_SLACK_PX = 8;

/**
 * Whether the pane should keep pinning its scroll to the bottom as output
 * appends. The canvas mode always showed the live screen; a DOM scroller
 * does not — browser scroll anchoring *holds* position, it never follows
 * appended content — so the pane must follow explicitly, and must stop the
 * moment the user scrolls up to read (being yanked to the bottom mid-read is
 * the opposite bug). Pure so the decision is provable without a DOM.
 */
export function followsOutput(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
): boolean {
  return scrollHeight - scrollTop - clientHeight <= FOLLOW_SLACK_PX;
}

/**
 * Everything the blocks pane draws, in document order.
 *
 * `nowMs` exists for the running counter ('running 4.2s' needs a clock) and
 * defaults to the real one; tests pass a fixed value so the header fields are
 * exact rather than approximately-now.
 */
export function paneModel(
  view: PaneView,
  folds: ReadonlySet<number>,
  link: LinkHealth,
  nowMs: number = Date.now(),
  chips: readonly PromptChip[] = [],
): RenderItem[] {
  const layout = sliceBlocks(view);
  const items: RenderItem[] = [];

  if (layout.preamble.length > 0) {
    items.push({ kind: 'rows', key: 'preamble', rows: toPaneRows(layout.preamble, view.attrs) });
  }

  for (let i = 0; i < layout.slices.length; i++) {
    const slice = layout.slices[i] as BlockSlice;
    const b = slice.block;

    // The trailing prompt-state block IS the prompt line (§3): the shell's
    // own prompt text, drawn as text with the caret, never as a header — a
    // header for a command that does not exist yet would say '·' forever.
    const isTrailingPrompt =
      i === layout.slices.length - 1 && slice.open && b.state.state === 'prompt';
    if (isTrailingPrompt) {
      items.push({
        kind: 'prompt',
        rows: toPaneRows(upToCaret([...slice.promptRows, ...slice.outputRows], view), view.attrs),
        chips,
      });
      continue;
    }

    // A prompt block that is NOT the trailing one is a prompt that was
    // abandoned — an empty Enter, a ^C, a resize, all of which make zsh
    // re-emit `133;A`. It is not a command, and a header for it invents one:
    // `headerOf` splits on `finished ? exit : running`, so it would draw a '·'
    // with a warn rail and a running counter for something that never ran.
    // Its rows are the prompt's own text and are drawn as exactly that. Hosts
    // stopped producing these in #193; one already running still can.
    if (b.state.state === 'prompt') {
      const rows = toPaneRows([...slice.promptRows, ...slice.outputRows], view.attrs);
      if (rows.length > 0) items.push({ kind: 'rows', key: `prompt-${b.id}`, rows });
      continue;
    }

    const header = headerOf(slice, folds, link, nowMs);
    items.push(header);
    if (!header.folded) {
      // A block with a command subsumes its prompt rows into the header —
      // rendering both would show the command twice. A block *without* one
      // (integration edge) keeps its prompt rows as body, so nothing the
      // host sent silently vanishes.
      const rows =
        b.command === '' ? [...slice.promptRows, ...slice.outputRows] : slice.outputRows;
      if (rows.length > 0) {
        // Only the open, running block offers options: its output is what
        // the command is showing right now, and a tap on a numbered row
        // answers it. Everything finished is history.
        const live = slice.open && b.state.state === 'running';
        items.push({ kind: 'output', blockId: b.id, rows: toPaneRows(rows, view.attrs, live) });
      }
    }
  }

  if (layout.tail.length > 0) {
    items.push({ kind: 'rows', key: 'tail', rows: toPaneRows(layout.tail, view.attrs) });
  }

  if (link === 'reconnecting') {
    items.push({ kind: 'overlay', link, text: 'connection lost — reconnecting' });
  } else if (link === 'stalled') {
    items.push({ kind: 'overlay', link, text: 'buffering' });
  }

  return items;
}

/**
 * The prompt's rows, stopping at the row the caret is on.
 *
 * A terminal is a fixed number of rows and a shell has usually printed a
 * handful, so the trailing prompt slice runs on into the viewport's blank
 * tail — 34 empty rows after one prompt line, measured. Those rows are
 * viewport rather than content: the canvas grid pays nothing for them, and a
 * DOM pane makes the reader scroll past them. Worse, the view appends the
 * caret to the LAST row it is handed, so it was drawn dozens of lines below
 * the prompt it belongs to. (#202)
 *
 * The caret's row, not the last non-blank one: a caret can legitimately sit on
 * a blank continuation row of a multi-line prompt, and trimming to the text
 * would swallow it.
 *
 * Everything is kept when the caret is not on one of this slice's own rows — a
 * cursor on the alternate screen, or rows the client never cached. Membership
 * is checked rather than inferred from the id being in range: a slice holding
 * lines 10 and 12 while the caret sits on 11 would otherwise drop line 12 on
 * the strength of a caret that is not in the slice at all. Rendering the whole
 * prompt is the harmless answer; dropping a row of it is not.
 */
function upToCaret(rows: readonly RowPayload[], view: PaneView): readonly RowPayload[] {
  const caretLine = view.rows[view.cursor.row]?.line;
  if (caretLine === undefined) return rows;
  if (!rows.some((r) => r.line === caretLine)) return rows;
  return rows.filter((r) => r.line <= caretLine);
}

function headerOf(
  slice: BlockSlice,
  folds: ReadonlySet<number>,
  link: LinkHealth,
  nowMs: number,
): HeaderItem {
  const b = slice.block;
  const foldable = slice.outputRows.length > 0;
  const folded = foldable && folds.has(b.id);

  const interrupted = isInterrupted(b, link);
  const outcome: Outcome = interrupted
    ? { kind: 'interrupted' }
    : b.state.state === 'finished'
      ? { kind: 'exit', code: b.state.exit_code }
      : {
          kind: 'running',
          seconds: b.started_ms === undefined ? null : Math.max(0, (nowMs - b.started_ms) / 1000),
        };

  return {
    kind: 'header',
    blockId: b.id,
    railToken: railOf(outcome),
    foldable,
    folded,
    chevron: foldable ? (folded ? '▸' : '▾') : null,
    command: b.command,
    cwd: b.cwd,
    durationText:
      b.state.state === 'finished' && b.started_ms !== undefined && b.ended_ms !== undefined
        ? formatDuration(b.ended_ms - b.started_ms)
        : '',
    outcome,
    foldedLineCount: outputLineCount(slice),
  };
}

/**
 * §3's rail colours off the *outcome*, so the rail can never disagree with
 * the metadata beside it: success for exit 0, danger for non-zero, warn while
 * running — and faint for both honest unknowns, a null exit code (never a
 * green tick) and an interrupted block (§4's "rail → ui.faint").
 */
function railOf(outcome: Outcome): RailToken {
  switch (outcome.kind) {
    case 'exit':
      if (outcome.code === null) return 'faint';
      return outcome.code === 0 ? 'success' : 'danger';
    case 'running':
      return 'warn';
    case 'interrupted':
      return 'faint';
  }
}

/**
 * Spans per row, memoized on the row *object's* identity. `GridView` replaces
 * a row's payload when a delta touches it and keeps the reference otherwise,
 * so identity is exactly "unchanged" — which makes the per-frame recompute
 * O(rows that changed), not O(session): during output only the open block's
 * tail and the prompt pay for span coalescing again. Sound because attr ids
 * are append-only (the encoder's interning table never redefines one), so a
 * cached row cannot go stale through the attrs map growing in place.
 */
const SPAN_CACHE = new WeakMap<RowPayload, readonly Span[]>();

function spansOf(row: RowPayload, attrs: ReadonlyMap<number, AttrDef>): readonly Span[] {
  let spans = SPAN_CACHE.get(row);
  if (spans === undefined) {
    spans = rowSpans(row, attrs);
    SPAN_CACHE.set(row, spans);
  }
  return spans;
}

function toPaneRows(
  rows: readonly RowPayload[],
  attrs: ReadonlyMap<number, AttrDef>,
  options = false,
): PaneRow[] {
  return rows.map((r) => {
    const spans = spansOf(r, attrs);
    return {
      line: r.line,
      spans,
      option: options ? optionOf(spans.map((s) => s.text).join('')) : null,
    };
  });
}
