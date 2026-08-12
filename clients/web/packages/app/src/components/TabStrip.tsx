/**
 * The horizontal tab strip (design §1): the 46px title bar with 34px chips
 * hanging 1px below its floor. Structure is the three-rule shape the handoff
 * paid for: an `overflow-x:auto` inner row of ALL tabs, then the `+` as a
 * `flex:none` SIBLING outside it (inside, its menu is clipped by the scroll),
 * then a `flex:1` spacer. Nothing is right-aligned except the `⌘K` pill.
 */

import { component, onMounted, onUpdated } from 'sigx';

import {
  chipTitle,
  chipTooltip,
  shouldScrollIntoView,
  type LauncherRow,
} from '../chrome-model.ts';
import type { Tab } from '../state/tabs.ts';
import { LauncherMenu } from './LauncherMenu.tsx';

export const TabStrip = component<{
  tabs: readonly Tab[];
  activeId: string | null;
  hostLabels: Readonly<Record<string, string>>;
  launcherOpen: boolean;
  launcherRows: readonly LauncherRow[];
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onLauncherToggle: () => void;
  onLaunch: (hostId: string) => void;
  onLauncherDismiss: () => void;
  onPalette: () => void;
}>((ctx) => {
  let scrollEl: HTMLElement | null = null;
  const chipEls = new Map<string, HTMLElement>();

  // Keep the active chip in view — the pure predicate decides, the DOM call
  // acts. Guarded rather than unconditional: scrollIntoView on every render
  // would fight the user's own scroll through the strip.
  const keepActiveVisible = (): void => {
    // Prune closed tabs' entries first — the ref callback only ever adds, so
    // without this the map holds every chip element a long session has seen.
    for (const [id, el] of chipEls) {
      if (!el.isConnected) chipEls.delete(id);
    }
    const id = ctx.props.activeId;
    if (id === null || scrollEl === null) return;
    const chip = chipEls.get(id);
    if (chip === undefined || !chip.isConnected) return;
    const c = chip.getBoundingClientRect();
    const v = scrollEl.getBoundingClientRect();
    if (shouldScrollIntoView({ left: c.left, right: c.right }, { left: v.left, right: v.right })) {
      chip.scrollIntoView({ inline: 'nearest', block: 'nearest' });
    }
  };
  onMounted(keepActiveVisible);
  onUpdated(keepActiveVisible);

  return () => (
    <header class="titlebar">
      <div
        class="tabs-scroll"
        role="tablist"
        ref={(el: HTMLElement) => {
          scrollEl = el;
        }}
      >
        {ctx.props.tabs.map((t) => (
          <div
            key={t.id}
            class={`tab-chip${t.id === ctx.props.activeId ? ' active' : ''}`}
            role="tab"
            aria-selected={t.id === ctx.props.activeId}
            // Roving tabindex: one Tab stop for the whole strip. Mouse focus
            // is cancelled below, so this is the keyboard's path in.
            tabIndex={t.id === ctx.props.activeId ? 0 : -1}
            title={chipTooltip(t, ctx.props.hostLabels[t.hostId])}
            style={`--tab-color:${t.color ?? 'var(--zt-accent)'}`}
            onMouseDown={(e: MouseEvent) => {
              // Chrome never steals the terminal's focus. Clicking the
              // ALREADY-active chip changes no key, so no remount refocuses
              // the textarea — the blur to <body> would silently eat all
              // typing. Cancelling mousedown's default stops the focus move;
              // click still fires. (Covers the close button via bubbling.)
              e.preventDefault();
            }}
            onClick={() => ctx.props.onActivate(t.id)}
            onKeyDown={(e: KeyboardEvent) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                ctx.props.onActivate(t.id);
              }
            }}
            ref={(el: HTMLElement) => chipEls.set(t.id, el)}
          >
            <span class="tab-glyph">❯</span>
            <span class="tab-title">{chipTitle(t)}</span>
            <button
              class="tab-close"
              title="close"
              onClick={(e: MouseEvent) => {
                // The chip underneath activates on click; closing must not
                // first yank focus onto the tab being destroyed.
                e.stopPropagation();
                ctx.props.onClose(t.id);
              }}
            >
              ×
            </button>
          </div>
        ))}
      </div>
      <div class="launcher-anchor">
        <button
          class={`tab-new${ctx.props.launcherOpen ? ' open' : ''}`}
          title="new session"
          onMouseDown={(e: MouseEvent) => e.preventDefault() /* same focus rule as the chips */}
          onClick={() => ctx.props.onLauncherToggle()}
        >
          +
        </button>
        {ctx.props.launcherOpen ? (
          <LauncherMenu
            rows={ctx.props.launcherRows}
            anchor="strip"
            onRun={ctx.props.onLaunch}
            onDismiss={ctx.props.onLauncherDismiss}
          />
        ) : null}
      </div>
      <div class="strip-spacer" />
      <button
        class="kbd-pill"
        onMouseDown={(e: MouseEvent) => e.preventDefault() /* same focus rule as the chips */}
        onClick={() => ctx.props.onPalette()}
      >
        ⌘K
      </button>
    </header>
  );
});
