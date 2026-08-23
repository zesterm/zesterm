/**
 * The `+` launcher menu (design §1/§12): the ONE way to start a session —
 * there is no separate default-only new-tab control, and `⏎` runs the
 * default row, so the default stays one keystroke away.
 *
 * Rows are a machine's shell plus whatever that machine published (#352),
 * grouped under its name once there is more than one machine to tell apart.
 *
 * The mock's *Manage profiles* row is deliberately absent: every profile the
 * browser can see belongs to a host and is read-only from here, so the row
 * would open an editor that cannot write. *Run on another host…* stays and
 * acts — it moves focus into the host rows.
 */

import { component, onMounted, onUnmounted } from 'sigx';

import {
  launchableRows,
  launcherKeyOf,
  type LauncherAlign,
  type LauncherRow,
  type LauncherTargetRow,
} from '../chrome-model.ts';

export const LauncherMenu = component<{
  rows: readonly LauncherRow[];
  /**
   * Which edge of the `+` the menu hangs from, so it stays inside the window
   * instead of running off either edge (invariant 5's defect class): the
   * sidebar's always opens rightwards, the strip's is measured per open by
   * `launcherAlign` — right-anchored as designed once the tabs push the `+`
   * far enough from the window's left edge for 318px to fit, rightwards
   * before that.
   */
  align: LauncherAlign;
  /**
   * The row, not its host id: a machine now has more than one row and they
   * launch different things. Passing the id alone was enough while every row
   * meant "a shell on that machine", and would now silently run the shell for
   * every profile.
   */
  onRun: (row: LauncherTargetRow) => void;
  onDismiss: () => void;
}>((ctx) => {
  let rootEl: HTMLElement | null = null;
  let firstHostRow: HTMLButtonElement | null = null;

  /** `⏎ runs the default` — and with nothing tagged, the first row stands in. */
  const defaultRow = (): LauncherTargetRow | undefined => {
    const launchable = launchableRows(ctx.props.rows);
    return launchable.find((r) => r.isDefault) ?? launchable[0];
  };

  // Every advertised chord — ⏎, ⇧⏎, Esc — is claimed at document capture
  // while the menu is open: focus usually still sits in the terminal's
  // textarea, and an Enter that both acted on the menu AND fed a newline to
  // the shell would be the worst of both. The policy itself lives in
  // `launcherKeyOf`, where it is node-tested.
  const onDocKeyDown = (e: KeyboardEvent): void => {
    const focusInMenu = rootEl !== null && e.target instanceof Node && rootEl.contains(e.target);
    switch (launcherKeyOf(e.key, e.shiftKey, focusInMenu)) {
      case 'dismiss':
        e.preventDefault();
        e.stopPropagation();
        ctx.props.onDismiss();
        return;
      case 'focus-rows':
        // The "Run on another host…" row's own action, by its ⇧⏎ chord.
        e.preventDefault();
        e.stopPropagation();
        firstHostRow?.focus();
        return;
      case 'run-default': {
        const row = defaultRow();
        if (row !== undefined) {
          e.preventDefault();
          e.stopPropagation();
          ctx.props.onRun(row);
        }
        return;
      }
      case 'none':
        return;
    }
  };

  // Outside-click dismisses. Containment is tested against the anchor wrapper
  // (the `+` button and this menu share it), not the menu alone — otherwise a
  // click on the `+` while open dismisses on mousedown and reopens on click,
  // and the button appears to do nothing.
  const onDocMouseDown = (e: MouseEvent): void => {
    const within = rootEl?.parentElement ?? rootEl;
    if (within !== null && e.target instanceof Node && within.contains(e.target)) return;
    ctx.props.onDismiss();
  };

  onMounted(({ el }) => {
    rootEl = el instanceof HTMLElement ? el : null;
    document.addEventListener('keydown', onDocKeyDown, true);
    document.addEventListener('mousedown', onDocMouseDown, true);
  });
  onUnmounted(() => {
    document.removeEventListener('keydown', onDocKeyDown, true);
    document.removeEventListener('mousedown', onDocMouseDown, true);
  });

  return () => {
    // The first *launchable* row, not the first row: once machines have
    // headers, index 0 is a header, and ⇧⏎ — the "Run on another host…"
    // row's own action — would focus nothing and silently do nothing.
    const first = launchableRows(ctx.props.rows)[0];
    return (
    <div class={`launcher align-${ctx.props.align}`} role="menu">
      <div class="launcher-head">⏎ runs the default</div>
      {ctx.props.rows.map((r) =>
        r.kind === 'group' ? (
          // Not a `menuitem`: a header is not selectable, and announcing it as
          // one would put a dead stop in every screen reader's row count.
          <div key={`g:${r.hostId}`} class="launcher-group" role="presentation">
            <span class="group-label">{r.label}</span>
            {r.sub === '' ? null : <span class="group-sub">{r.sub}</span>}
          </div>
        ) : (
          <button
            // The machine AND the profile: a host id alone stopped being
            // unique the moment a machine could contribute more than one row,
            // and a duplicate key is a row the framework may reuse for its
            // neighbour.
            key={`${r.hostId}:${r.profile ?? ''}`}
            class="launcher-row"
            role="menuitem"
            ref={(el: HTMLButtonElement | null) => {
              if (r === first) firstHostRow = el;
            }}
            onClick={() => ctx.props.onRun(r)}
          >
            <span class="row-tile">❯</span>
            <span class="row-main">
              <span class="row-name">{r.name}</span>
              <span class="row-sub">{r.sub}</span>
            </span>
            {r.isDefault ? <span class="default-tag">default</span> : null}
            <span class="host-chip">{r.hostLabel}</span>
            {r.chord !== null ? <span class="row-chord">{r.chord}</span> : null}
          </button>
        ),
      )}
      <div class="launcher-divider" />
      <button class="launcher-row" role="menuitem" onClick={() => firstHostRow?.focus()}>
        <span class="row-tile">›</span>
        <span class="row-main">
          <span class="row-name">Run on another host…</span>
        </span>
        <span class="row-chord">⇧⏎</span>
      </button>
    </div>
    );
  };
});
