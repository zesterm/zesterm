/**
 * The command-blocks pane (design §3–§4), the primary screen's renderer.
 *
 * Thin on purpose: every decision — which blocks fold, what the rail colour
 * is, what 'exit ?' means — was made by `paneModel`, and this component only
 * turns `RenderItem`s into DOM. Header + body pair up into one `<section>`
 * keyed by block id, so the diff never rebuilds a finished block's rows when
 * the open block grows; rows key by their absolute line id for the same
 * reason. SGR colours resolve against the theme's terminal palette here and
 * only here, mirroring the canvas painter (ADR-002): the spans carry `Color`
 * values unresolved.
 */

import { component, onMounted, onUpdated } from 'sigx';
import { CellFlags, hasFlag, type Color, type Span } from '@zesterm/proto';
import type { TerminalPalette } from '@zesterm/theme';

import {
  followsOutput,
  type HeaderItem,
  type LinkHealth,
  type Outcome,
  type PaneRow,
  type RenderItem,
} from '../blocks-pane-model.ts';

const UNDERLINE_FLAGS =
  CellFlags.UNDERLINE | CellFlags.DOUBLE_UNDERLINE | CellFlags.UNDERCURL;

function hexByte(v: number): string {
  return v.toString(16).padStart(2, '0');
}

function resolveColor(c: Color, palette: TerminalPalette): string | null {
  if (c === 'Default') return null;
  if ('Indexed' in c) return palette.indexed(c.Indexed);
  const [r, g, b] = c.Rgb;
  return `#${hexByte(r)}${hexByte(g)}${hexByte(b)}`;
}

/**
 * A span's inline style, or undefined for plain text (the common case — an
 * attribute-free span must not carry an empty style attribute per span).
 */
function spanStyle(s: Span, palette: TerminalPalette): string | undefined {
  let fg = resolveColor(s.fg, palette);
  let bg = resolveColor(s.bg, palette);
  if (hasFlag(s.flags, CellFlags.INVERSE)) {
    // Same swap as the painter: reverse video over defaults shows bg-on-fg.
    const f = fg ?? palette.defaultFg;
    const b = bg ?? palette.defaultBg;
    fg = b;
    bg = f;
  }
  const parts: string[] = [];
  if (fg !== null) parts.push(`color:${fg}`);
  if (bg !== null) parts.push(`background:${bg}`);
  if (hasFlag(s.flags, CellFlags.BOLD)) parts.push('font-weight:600');
  if (hasFlag(s.flags, CellFlags.ITALIC)) parts.push('font-style:italic');
  const deco: string[] = [];
  if ((s.flags & UNDERLINE_FLAGS) !== 0) deco.push('underline');
  if (hasFlag(s.flags, CellFlags.STRIKEOUT)) deco.push('line-through');
  if (deco.length > 0) parts.push(`text-decoration:${deco.join(' ')}`);
  // Alpha rather than a blend, matching the painter's DIM rule.
  if (hasFlag(s.flags, CellFlags.DIM)) parts.push('opacity:.6');
  if (hasFlag(s.flags, CellFlags.HIDDEN)) parts.push('visibility:hidden');
  return parts.length > 0 ? parts.join(';') : undefined;
}

export const BlocksPane = component<{
  items: readonly RenderItem[];
  link: LinkHealth;
  palette: TerminalPalette;
  onToggleFold: (blockId: number) => void;
  onCopyOutput: () => void;
  onReRun: () => void;
}>((ctx) => {
  let paneEl: HTMLElement | null = null;

  // The pane follows output: pinned to the bottom so the live prompt stays
  // in view as history grows — a DOM scroller left alone holds position and
  // new content lands below the fold, which for a terminal means typing into
  // an input you cannot see. The user scrolling up releases the pin
  // (followsOutput decides, and is tested); returning to the bottom
  // re-engages it. Programmatic pinning lands *at* the bottom, so the scroll
  // event it fires re-derives `follow` as true rather than fighting this.
  let follow = true;

  const onScroll = (): void => {
    const el = paneEl;
    if (el !== null) follow = followsOutput(el.scrollTop, el.clientHeight, el.scrollHeight);
  };

  const pin = (): void => {
    const el = paneEl;
    if (el !== null && follow) el.scrollTop = el.scrollHeight;
  };

  // Both hooks: onUpdated covers every re-render (new output, folds, link),
  // onMounted covers the first paint and each alt→primary mode switch, which
  // remounts the pane with history already long.
  onMounted(pin);
  onUpdated(pin);

  // Two shapes rather than style={maybeUndefined}: exactOptionalPropertyTypes
  // is right that an explicit undefined is not the same as no attribute.
  const spanEl = (s: Span, palette: TerminalPalette) => {
    const style = spanStyle(s, palette);
    return style === undefined ? <span>{s.text}</span> : <span style={style}>{s.text}</span>;
  };

  const rowEl = (row: PaneRow, palette: TerminalPalette) => (
    <div class="pane-row" key={String(row.line)}>
      {row.spans.length === 0
        ? ' ' // an empty row still occupies its line
        : row.spans.map((s) => spanEl(s, palette))}
    </div>
  );

  const outcomeEl = (o: Outcome) => {
    switch (o.kind) {
      case 'exit':
        // The daemon's verdict verbatim — a null exit code renders as a
        // question, never a green tick.
        return o.code === null ? (
          <span class="outcome unknown">exit ?</span>
        ) : (
          <span class={`outcome ${o.code === 0 ? 'ok' : 'bad'}`}>exit {o.code}</span>
        );
      case 'running':
        return (
          <span class="outcome running">
            <span class="spinner" />
            {o.seconds === null ? 'running' : `running ${o.seconds.toFixed(1)}s`}
          </span>
        );
      case 'interrupted':
        return <span class="outcome interrupted">interrupted</span>;
    }
  };

  const headerEl = (h: HeaderItem) => (
    <div
      class={`block-header ${h.railToken}${h.outcome.kind === 'running' ? ' is-running' : ''}`}
      tabIndex={0}
    >
      {h.chevron !== null ? (
        <button
          class="fold"
          aria-expanded={!h.folded}
          onClick={() => ctx.props.onToggleFold(h.blockId)}
        >
          {h.chevron}
        </button>
      ) : null}
      <span class="cmd">{h.command === '' ? '·' : h.command}</span>
      <span class="chips">
        <button class="chip" onClick={() => ctx.props.onCopyOutput()}>
          copy output ⌘⇧O
        </button>
        <button class="chip" onClick={() => ctx.props.onReRun()}>
          re-run ⌘⇧R
        </button>
      </span>
      <span class="meta">
        {h.folded ? (
          <span class="lines">
            {h.foldedLineCount} {h.foldedLineCount === 1 ? 'line' : 'lines'}
          </span>
        ) : null}
        {h.cwd !== '' ? <span class="cwd">{h.cwd}</span> : null}
        {h.durationText !== '' ? <span class="dur">{h.durationText}</span> : null}
        {outcomeEl(h.outcome)}
      </span>
    </div>
  );

  return () => {
    const { items, link, palette } = ctx.props;

    // Pair each header with its output item into one keyed <section>, so a
    // block is one DOM subtree the differ can skip whole while another grows.
    const out: unknown[] = [];
    for (let i = 0; i < items.length; i++) {
      const item = items[i] as RenderItem;
      switch (item.kind) {
        case 'rows':
          out.push(
            <div class="pane-rows" key={item.key}>
              {item.rows.map((r) => rowEl(r, palette))}
            </div>,
          );
          break;
        case 'header': {
          const next = items[i + 1];
          const body =
            next !== undefined && next.kind === 'output' && next.blockId === item.blockId
              ? next
              : null;
          if (body !== null) i += 1;
          out.push(
            <section class="block" key={`b${item.blockId}`}>
              {headerEl(item)}
              {body !== null ? (
                <div class="block-body">{body.rows.map((r) => rowEl(r, palette))}</div>
              ) : null}
            </section>,
          );
          break;
        }
        case 'output':
          // Unreachable when items came from paneModel (an output always
          // follows its header); rendered anyway rather than dropped.
          out.push(
            <div class="block-body" key={`o${item.blockId}`}>
              {item.rows.map((r) => rowEl(r, palette))}
            </div>,
          );
          break;
        case 'prompt':
          out.push(
            <div class="prompt-line" key="prompt">
              {item.rows.map((r, idx) => (
                <span class="prompt-row" key={String(r.line)}>
                  {r.spans.map((s) => spanEl(s, palette))}
                  {idx === item.rows.length - 1 ? <span class="caret" /> : null}
                </span>
              ))}
            </div>,
          );
          break;
        case 'overlay':
          out.push(
            <div class={`pane-overlay ${item.link}`} key="overlay">
              {item.text}
            </div>,
          );
          break;
      }
    }

    return (
      <div
        class={`blocks-pane${link === 'reconnecting' ? ' degraded' : ''}`}
        ref={(el: HTMLElement) => {
          paneEl = el;
        }}
        onScroll={onScroll}
      >
        {out}
      </div>
    );
  };
});
