/**
 * The command palette modal (design §6) — thin over the tested pieces: the
 * store's reducers own the cursor, `rank.ts` owns the order, `sources.ts`
 * owns what a row says and what ⏎ does. This component owns only the DOM:
 * the scrim, the query row, the rows, the footer, and focus.
 *
 * Focus discipline: the real focus target is a visually hidden input (the
 * same IME-safe trick as the terminal's textarea — the drawn query line and
 * its block caret are presentation). Opening remembers the element that had
 * focus and moves it here, which is also what keeps every keystroke out of
 * the shell behind the scrim; dismissal restores it — normally the terminal
 * textarea. Row mousedown is cancelled like every chrome click, so clicking
 * a row never parks focus on <body> first, and Tab is trapped (keys.ts says
 * why) — with both routes closed, focus cannot leave the input while open.
 * The key handler sits on the scrim, not the input, so a key struck while
 * focus is somewhere unexpected inside the dialog still reaches it.
 *
 * The footer advertises ↑↓ ⏎ esc and deliberately NOT ⇧⏎ run-on-host: the
 * launcher exposes no host-chooser hook yet, and a dead advertised chord
 * reads as broken (the launcher menu's own rule).
 */

import { component, onMounted, onUnmounted, onUpdated } from 'sigx';

import { handlePaletteKey } from '../palette/keys.ts';
import { flattenResults, type PaletteGroup } from '../palette/rank.ts';
import type { PaletteItem } from '../palette/sources.ts';

const GLYPHS: Record<PaletteItem['kind'], string> = {
  block: '●',
  session: '›',
  host: '⬢',
  action: '⌘',
};

export const Palette = component<{
  query: string;
  selection: number;
  groups: readonly PaletteGroup[];
  hostsSearched: number;
  onQuery: (query: string) => void;
  onMove: (delta: number) => void;
  onRun: (item: PaletteItem) => void;
  onDismiss: () => void;
}>((ctx) => {
  let inputEl: HTMLInputElement | null = null;
  let selectedEl: HTMLElement | null = null;
  let restoreEl: HTMLElement | null = null;

  onMounted(() => {
    restoreEl = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    inputEl?.focus();
  });
  onUnmounted(() => {
    // Restore focus on ANY dismissal path — Esc, scrim, ⏎ — so typing after
    // the palette lands back in the terminal instead of on <body>, which
    // silently eats it. A remounting TerminalView refocuses itself after this.
    if (restoreEl !== null && restoreEl.isConnected) restoreEl.focus();
  });
  // 'nearest' no-ops when the row is already visible, so this cannot fight a
  // scroll the user made — it only chases the selection past the fold.
  onUpdated(() => selectedEl?.scrollIntoView({ block: 'nearest' }));

  const selectedItem = (): PaletteItem | undefined => {
    const flat = flattenResults(ctx.props.groups);
    return flat[Math.min(ctx.props.selection, flat.length - 1)];
  };

  const onKeyDown = (e: KeyboardEvent): void =>
    handlePaletteKey(e, {
      move: (delta) => ctx.props.onMove(delta),
      run: () => {
        const item = selectedItem();
        if (item !== undefined) ctx.props.onRun(item);
      },
      dismiss: () => ctx.props.onDismiss(),
    });

  return () => {
    const flat = flattenResults(ctx.props.groups);
    // Results can shrink under a stale index (blocks arriving mid-keystroke);
    // clamping keeps the highlight and ⏎ on the same, existing row.
    const sel = Math.min(ctx.props.selection, flat.length - 1);
    selectedEl = null;
    let flatIndex = -1;
    const n = ctx.props.hostsSearched;

    return (
      <div
        class="palette-scrim"
        onKeyDown={onKeyDown}
        onPointerDown={(e: PointerEvent) => {
          // Scrim click dismisses; a click inside the panel must not — the
          // guard is target identity, not bubbling, so nothing in the panel
          // needs to stop propagation.
          if (e.target === e.currentTarget) {
            e.preventDefault();
            ctx.props.onDismiss();
          }
        }}
      >
        <div class="palette" role="dialog" aria-modal="true" aria-label="command palette">
          <div
            class="palette-query"
            onPointerDown={(e: PointerEvent) => {
              e.preventDefault();
              inputEl?.focus();
            }}
          >
            <span class="p-prompt">❯</span>
            <span class="p-qtext">
              {ctx.props.query}
              <span class="caret" />
            </span>
            <input
              class="palette-input"
              aria-label="Search sessions, blocks, hosts"
              ref={(el: HTMLInputElement | null) => {
                inputEl = el;
              }}
              value={ctx.props.query}
              autoComplete="off"
              spellCheck={false}
              onInput={() => ctx.props.onQuery(inputEl?.value ?? '')}
            />
            <span class="p-count">
              {n} host{n === 1 ? '' : 's'} searched
            </span>
          </div>
          <div class="palette-results">
            {flat.length === 0 ? <div class="palette-empty">no matches</div> : null}
            {ctx.props.groups.map((g) => (
              <div key={g.label} class="palette-group">
                <div class="p-label">{g.label}</div>
                {g.items.map((item) => {
                  flatIndex += 1;
                  const i = flatIndex;
                  const isSel = i === sel;
                  return (
                    <button
                      key={`${item.kind}-${i}`}
                      class={`palette-row${isSel ? ' selected' : ''}`}
                      ref={(el: HTMLElement | null) => {
                        if (isSel) selectedEl = el;
                      }}
                      onPointerDown={(e: PointerEvent) => e.preventDefault() /* the chip focus rule */}
                      onClick={() => ctx.props.onRun(item)}
                    >
                      <span class={`p-glyph${item.kind === 'block' ? ` ${item.tone}` : ''}`}>
                        {GLYPHS[item.kind]}
                      </span>
                      <span class={`p-text${item.kind === 'block' ? ' mono' : ''}`}>
                        {item.text}
                      </span>
                      {item.provenance !== '' ? (
                        <span class="p-prov">{item.provenance}</span>
                      ) : null}
                      {isSel ? <span class="p-hint">⏎ run</span> : null}
                    </button>
                  );
                })}
              </div>
            ))}
          </div>
          <div class="palette-footer">
            <span>
              <span class="p-key">↑↓</span> navigate
            </span>
            <span>
              <span class="p-key">⏎</span> run here
            </span>
            <span class="p-esc">
              <span class="p-key">esc</span> dismiss
            </span>
          </div>
        </div>
      </div>
    );
  };
});
