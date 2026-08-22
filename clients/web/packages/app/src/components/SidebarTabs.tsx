/**
 * The vertical layout's chrome (design §2): a full-width 46px header reading
 * from the ACTIVE tab, and the 262px sidebar whose host groups are derived —
 * via `groupByHost` — from the SAME tab list the horizontal strip renders.
 * Never a second array: a hardcoded row cannot be selected, and a separate
 * list cannot show a session the user just started (invariant 6).
 *
 * Below 900px the stylesheet collapses the sidebar to a 48px icon rail (host
 * dots only); the `title` tooltips carried here are what still name things
 * there. `ICON_RAIL_MAX_WIDTH` in chrome-model.ts mirrors the breakpoint.
 */

import { component } from 'sigx';

import {
  chipTitle,
  chipTooltip,
  shortHostId,
  type LauncherRow,
  type LauncherTargetRow,
} from '../chrome-model.ts';
import { groupByHost, type Tab } from '../state/tabs.ts';
import { LauncherMenu } from './LauncherMenu.tsx';

/**
 * The header spans the window edge to edge on purpose (it is the sibling of
 * the sidebar+pane row, not part of the sidebar), which is why it is its own
 * component. Its identity reads from the active tab — literal text here
 * would contradict the pane the moment the active tab is not the first one.
 */
export const VerticalHeader = component<{
  active: Tab | null;
  hostLabels: Readonly<Record<string, string>>;
  onPalette: () => void;
}>((ctx) => () => {
  const t = ctx.props.active;
  return (
    <header class="v-header">
      {t !== null ? (
        <>
          <span class="v-title">{chipTitle(t)}</span>
          {t.cwd !== '' ? <span class="v-cwd">{t.cwd}</span> : null}
          <span class="host-chip">
            <span class="host-dot" />
            {ctx.props.hostLabels[t.hostId] ?? shortHostId(t.hostId)}
          </span>
        </>
      ) : (
        <span class="v-title">zesterm</span>
      )}
      <span class="grow" />
      {/* The visual affordance the spec draws. It dispatches the palette
          action, which is a no-op until the palette work item lands. */}
      <button
        class="kbd-pill"
        onPointerDown={(e: PointerEvent) => e.preventDefault() /* chrome never steals the terminal's focus (see TabStrip) */}
        onClick={() => ctx.props.onPalette()}
      >
        ⌘K
      </button>
    </header>
  );
});

export const SidebarTabs = component<{
  tabs: readonly Tab[];
  activeId: string | null;
  hostLabels: Readonly<Record<string, string>>;
  launcherOpen: boolean;
  launcherRows: readonly LauncherRow[];
  onActivate: (id: string) => void;
  onLauncherToggle: () => void;
  onLaunch: (row: LauncherTargetRow) => void;
  onLauncherDismiss: () => void;
  onPalette: () => void;
  onHosts: () => void;
}>((ctx) => () => {
  const label = (hostId: string): string =>
    ctx.props.hostLabels[hostId] ?? shortHostId(hostId);
  const groups = groupByHost(ctx.props.tabs.filter((t) => t.kind === 'session'));

  return (
    <aside class="sidebar">
      <div class="search-row">
        <button
          class="search-pill"
          onPointerDown={(e: PointerEvent) => e.preventDefault() /* chrome never steals the terminal's focus (see TabStrip) */}
          onClick={() => ctx.props.onPalette()}
        >
          <span class="search-key">⌘K</span>
          <span class="search-hint">Search sessions, blocks, hosts</span>
        </button>
        {/* The SAME `+` as the strip's, right of the search pill; its menu
            opens rightwards so it stays inside the window (invariant 5). */}
        <div class="launcher-anchor">
          <button
            class={`tab-new in-sidebar${ctx.props.launcherOpen ? ' open' : ''}`}
            title="new session"
            onPointerDown={(e: PointerEvent) => e.preventDefault()}
            onClick={() => ctx.props.onLauncherToggle()}
          >
            +
          </button>
          {ctx.props.launcherOpen ? (
            <LauncherMenu
              rows={ctx.props.launcherRows}
              align="left"
              onRun={ctx.props.onLaunch}
              onDismiss={ctx.props.onLauncherDismiss}
            />
          ) : null}
        </div>
      </div>

      <div class="host-groups">
        {groups.map((g) => (
          <section class="host-group" key={g.hostId}>
            <div class="host-head" title={label(g.hostId)}>
              <span class="host-dot" />
              <span class="host-label">{label(g.hostId)}</span>
              {/* The mock's sub-line (`macOS · LAN 0.3ms`) is data this client
                  does not have, and a made-up number is worse than none — so
                  there is no sub-line at all until the control plane carries
                  OS and latency. The .host-sub style stays for that day. */}
            </div>
            {g.tabs.map((t) => (
              <button
                key={t.id}
                class={`side-row${t.id === ctx.props.activeId ? ' selected' : ''}`}
                title={chipTooltip(t, ctx.props.hostLabels[t.hostId])}
                onPointerDown={(e: PointerEvent) => {
                  // A click on the ALREADY-selected row must not blur the
                  // terminal textarea — no remount would refocus it, and
                  // typing silently stops reaching the shell (see TabStrip).
                  e.preventDefault();
                }}
                onClick={() => ctx.props.onActivate(t.id)}
              >
                <span class={`link-dot ${t.link}`} />
                <span class="side-text">
                  <span class="side-title">{chipTitle(t)}</span>
                  {t.cwd !== '' ? <span class="side-cwd">{t.cwd}</span> : null}
                </span>
                {/* Age is right-aligned here when known; the directory does
                    not carry it yet, so the column is omitted rather than
                    faked (the spec's own rule). */}
              </button>
            ))}
          </section>
        ))}
      </div>

      <footer class="sidebar-footer">
        {/* Dot and label are separate spans so the 48px icon rail can keep
            the dot and drop the sentence — text wraps out of the rail. The
            tooltip carries the count there, like every other rail name. */}
        <button
          class="hosts-link"
          title={`${groups.length} host${groups.length === 1 ? '' : 's'}`}
          onPointerDown={(e: PointerEvent) => e.preventDefault()}
          onClick={() => ctx.props.onHosts()}
        >
          <span class="hosts-dot">●</span>
          <span class="hosts-label">
            {groups.length} host{groups.length === 1 ? '' : 's'}
          </span>
        </button>
      </footer>
    </aside>
  );
});
