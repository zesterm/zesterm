/**
 * The `+` launcher menu (design §1/§12): the ONE way to start a session —
 * there is no separate default-only new-tab control, and `⏎` runs the
 * default row, so the default stays one keystroke away.
 *
 * Rows carry the profile-row shape (icon tile, name over a mono sub-line,
 * host chip, ⌘N) so profile rows drop straight in when profiles land; today
 * they are "New session on <host>" per known host, built by `launcherRows`.
 *
 * The mock's *Manage profiles* row is deliberately absent: the browser has
 * no profiles yet, and a dead row in a short menu reads as a broken feature.
 * *Run on another host…* stays and acts — it moves focus into the host rows.
 */

import { component, onMounted, onUnmounted } from 'sigx';

import { launcherKeyOf, type LauncherAlign, type LauncherRow } from '../chrome-model.ts';

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
  onRun: (hostId: string) => void;
  onDismiss: () => void;
}>((ctx) => {
  let rootEl: HTMLElement | null = null;
  let firstHostRow: HTMLButtonElement | null = null;

  /** `⏎ runs the default` — and with nothing tagged, the first row stands in. */
  const defaultRow = (): LauncherRow | undefined =>
    ctx.props.rows.find((r) => r.isDefault) ?? ctx.props.rows[0];

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
          ctx.props.onRun(row.hostId);
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

  return () => (
    <div class={`launcher align-${ctx.props.align}`} role="menu">
      <div class="launcher-head">⏎ runs the default</div>
      {ctx.props.rows.map((r, i) => (
        <button
          key={r.hostId}
          class="launcher-row"
          role="menuitem"
          ref={(el: HTMLButtonElement) => {
            if (i === 0) firstHostRow = el;
          }}
          onClick={() => ctx.props.onRun(r.hostId)}
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
      ))}
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
});
