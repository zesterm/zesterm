/**
 * Pure chrome logic — everything the tab strip, sidebar and launcher decide
 * that is not pixels: chip labels, launcher rows, the scroll-into-view
 * predicate, the icon-rail breakpoint. Plain data in, plain data out, so all
 * of it runs under `node --test` with no renderer in the room; the components
 * stay thin over these functions.
 */

import type { HostFacts } from '@zesterm/control';

import type { Tab } from './state/tabs.ts';

/**
 * The one mono family for the grid AND the chrome's mono text (cwd lines, key
 * caps, sub-labels). The handoff's typography table is explicit that chrome
 * mono reuses the grid font rather than shipping a second one; sharing the
 * constant is what keeps a drift from being possible. `entry-client` mirrors
 * it into `--zt-mono` for the stylesheet, and `TerminalView` measures glyphs
 * with it directly.
 */
export const MONO_FAMILY = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';

/**
 * The grid's font size in CSS pixels. Shared for the same reason as the
 * family: `TerminalView` sizes the pty off these metrics and `GridPane` sizes
 * its canvas off them — two literals would let the pty and the painted grid
 * disagree by a row.
 */
export const GRID_FONT_SIZE = 13;

/**
 * A tab chip shows its title ONLY — host and cwd live in the tooltip and the
 * vertical sidebar/header, which have room for them (design §1). An untitled
 * session reads as `shell`, never as a blank chip.
 */
export function chipTitle(tab: Pick<Tab, 'title'>): string {
  return tab.title === '' ? 'shell' : tab.title;
}

/** A host id is 64 hex chars; twelve are enough to tell machines apart on screen. */
export function shortHostId(id: string): string {
  return id.length <= 12 ? id : id.slice(0, 12);
}

/**
 * The chip's tooltip — where host and cwd live, since the 34px chip has no
 * room for a second line. The label falls back to the shortened host id so a
 * host the directory has not named yet is still identifiable.
 */
export function chipTooltip(
  tab: Pick<Tab, 'title' | 'hostId' | 'cwd'>,
  hostLabel?: string,
): string {
  const host = hostLabel ?? shortHostId(tab.hostId);
  return tab.cwd === ''
    ? `${chipTitle(tab)} — ${host}`
    : `${chipTitle(tab)} — ${host} · ${tab.cwd}`;
}

/**
 * One tab per (host, session) pair — the FULL pair, like folds: session ids
 * are per daemon, so `studio`'s session 3 and `forge`'s session 3 are
 * different terminals. ':' cannot appear in a hex host id or a decimal
 * session id, so no pair collides with another by shuffling characters
 * across the join.
 */
export function tabIdOf(hostId: string, sessionId: string): string {
  return `${hostId}:${sessionId}`;
}

/** A machine a session can be started on, as the launcher needs it. */
export interface HostChoice {
  readonly id: string;
  readonly label: string;
}

/**
 * A machine's header in the launcher menu.
 *
 * Grouping is by **the machine that will run the row**, which is what makes
 * "which computer" structural rather than a chip you have to notice — design
 * §2's argument for the vertical sidebar, applied to the menu. In the browser
 * that is also the only grouping available: every launch target it can see was
 * published by a machine and is pinned to it by construction (ADR-014), so
 * there is no `any machine` bucket and no local-definition bucket here, unlike
 * the native launcher which reads a config file as well.
 */
export interface LauncherGroupRow {
  readonly kind: 'group';
  readonly hostId: string;
  readonly label: string;
  /** `windows · x86_64`, or empty when the machine has not said. */
  readonly sub: string;
}

/**
 * One thing the menu can launch. The shape carries everything the row markup
 * needs — name over a mono sub-line, host chip, ⌘N.
 */
export interface LauncherTargetRow {
  readonly kind: 'target';
  readonly hostId: string;
  readonly hostLabel: string;
  /**
   * Which published profile this row runs, or null for the machine's plain
   * shell. Identity and display only — what is actually sent is `command`.
   */
  readonly profile: string | null;
  /**
   * What to run, already resolved by the machine that published it through
   * *its own* `profiles.defaults`. Empty means that machine's default shell,
   * which is what `create_session` has always meant by an empty command.
   *
   * The command and not the profile name, because `CreateSession` carries no
   * profile field — the native app sends the published command for a remote
   * launch for the same reason (ADR-014: the resolution happened on the
   * machine that owns the profile, and re-resolving it here would need a
   * config this client does not have).
   */
  readonly command: string;
  /** Where to start it. Empty means the machine's own default. */
  readonly cwd: string;
  readonly name: string;
  /** Mono sub-line — real data, never a fabricated command. */
  readonly sub: string;
  readonly isDefault: boolean;
  /** `⌘1`…`⌘9` by launchable position; null past the ninth — there is no ⌘10. */
  readonly chord: string | null;
}

export type LauncherRow = LauncherGroupRow | LauncherTargetRow;

/** The rows `⌘N` and `⏎` can actually run, in menu order. */
export function launchableRows(rows: readonly LauncherRow[]): readonly LauncherTargetRow[] {
  return rows.filter((r): r is LauncherTargetRow => r.kind === 'target');
}

/**
 * Hosts → launcher rows, grouped by the machine that will run them (#352).
 *
 * The default machine comes first and its shell row is tagged `default` — it
 * is what `⏎` runs — and the rest keep their given order, so the menu is
 * stable across opens. An unknown or null default tags nothing; `⏎` then falls
 * to the first row, which the menu resolves, not this builder.
 *
 * `factsOf` answers what each machine published. **Null is not an empty
 * list**: a machine that has said nothing (an older daemon, one nothing can
 * reach, one whose connection has not landed yet) contributes no targets, and
 * a machine with an empty profile table contributes none either — but both
 * still get their "New session on…" row, because that row is how you get a
 * shell at all and it does not depend on the offer.
 *
 * **Headers appear only once there is more than one machine.** A one-machine
 * setup — every loopback shell, and most accounts — must not grow chrome for a
 * fleet it does not have; the native launcher draws none there for the same
 * reason.
 */
export function launcherRows(
  hosts: readonly HostChoice[],
  defaultHostId: string | null,
  factsOf: (hostId: string) => HostFacts | null = () => null,
): readonly LauncherRow[] {
  const def = defaultHostId === null ? undefined : hosts.find((h) => h.id === defaultHostId);
  const ordered = def === undefined ? hosts : [def, ...hosts.filter((h) => h.id !== def.id)];
  const grouped = ordered.length > 1;
  const rows: LauncherRow[] = [];
  // Counted across groups rather than within one, so ⌘2 is the second row you
  // can see wherever it sits — the digits number the menu, not a section of
  // it. `launchableRows` is the same walk, and `Shell` indexes with it.
  let nth = 0;
  const chordOf = (): string | null => {
    nth += 1;
    return nth <= 9 ? `⌘${nth}` : null;
  };
  for (const h of ordered) {
    const facts = factsOf(h.id);
    if (grouped) {
      rows.push({
        kind: 'group',
        hostId: h.id,
        label: h.label,
        // Only what it actually said. "An os row we cannot fill would be a
        // dash pretending to be a fact" is the native fleet card's rule, and
        // a header is the same promise in a smaller space.
        sub: [facts?.os ?? '', facts?.arch ?? ''].filter((p) => p !== '').join(' · '),
      });
    }
    rows.push({
      kind: 'target',
      hostId: h.id,
      hostLabel: h.label,
      profile: null,
      // Empty is "that machine's default shell" — the meaning
      // `create_session` has always given an empty command, so the row that
      // means "just a shell" says nothing rather than guessing one.
      command: '',
      cwd: '',
      name: `New session on ${h.label}`,
      // The machine's own default shell once it has said, and the shortened
      // id until then — never a guess about what it runs.
      sub: facts?.defaultShell !== undefined && facts.defaultShell !== ''
        ? facts.defaultShell
        : shortHostId(h.id),
      isDefault: def !== undefined && h.id === def.id,
      chord: chordOf(),
    });
    for (const target of facts?.launchTargets ?? []) {
      rows.push({
        kind: 'target',
        hostId: h.id,
        hostLabel: h.label,
        profile: target.name,
        command: target.command,
        cwd: target.startingDirectory,
        name: target.name,
        // What that machine resolved it to. Empty means "its default shell",
        // which the machine has already told us the name of.
        sub: target.command === '' ? (facts?.defaultShell ?? '') : target.command,
        isDefault: false,
        chord: chordOf(),
      });
    }
  }
  return rows;
}

/** What a document-level keydown means to the open launcher menu. */
export type LauncherKey = 'dismiss' | 'run-default' | 'focus-rows' | 'none';

/**
 * The open menu's key policy, pure so every chord the menu ADVERTISES is
 * pinned to an action: `esc` dismisses, `⏎` runs the default, `⇧⏎` moves
 * focus into the host rows — the "Run on another host…" row's own action
 * (design §1: both act; a dead chord feeds a newline to the shell behind the
 * menu instead). `⏎` yields when focus already sits inside the menu, because
 * the focused row activates itself and claiming the key would run the
 * default over the row the user chose.
 */
export function launcherKeyOf(key: string, shift: boolean, focusInMenu: boolean): LauncherKey {
  if (key === 'Escape') return 'dismiss';
  if (key !== 'Enter') return 'none';
  if (shift) return 'focus-rows';
  return focusInMenu ? 'none' : 'run-default';
}

/**
 * The launcher menu's width. `style.css`'s `.launcher { width: 318px }` is
 * the same number, and a test pins the two against drifting apart — the
 * alignment predicate below is only correct while they agree.
 */
export const LAUNCHER_WIDTH = 318;

/** Which edge of its `+` button the open launcher menu hangs from. */
export type LauncherAlign = 'left' | 'right';

/**
 * The strip's menu hangs right-under its `+` by design (§1) — but with zero
 * or one tabs that button sits within 318px of the window's LEFT edge, so a
 * right-anchored menu runs past the viewport and is clipped: the defect
 * class invariant 5 names for the sidebar's anchor, on the other edge. So
 * the menu right-anchors only when it fits between the viewport's left edge
 * and the button, and opens rightwards otherwise. `anchorRight` is the `+`
 * anchor's right edge in viewport coordinates.
 */
export function launcherAlign(anchorRight: number): LauncherAlign {
  return anchorRight >= LAUNCHER_WIDTH ? 'right' : 'left';
}

/** A horizontal extent — a chip's or its scroll viewport's, in any shared coordinates. */
export interface Extent {
  readonly left: number;
  readonly right: number;
}

/**
 * Whether the active chip needs scrolling into view: any part of it outside
 * the viewport qualifies. Pure so the edge cases are testable; the component
 * only measures rects and calls `scrollIntoView` when this says so — calling
 * it unconditionally would fight the user's own scroll on every re-render.
 */
export function shouldScrollIntoView(chip: Extent, viewport: Extent): boolean {
  return chip.left < viewport.left || chip.right > viewport.right;
}

/**
 * The sidebar's icon-rail breakpoint. The stylesheet's
 * `@media (max-width: 900px)` rule is what actually collapses the sidebar;
 * this is the same number for logic-side callers, and the test pins the two
 * against drifting apart. `<=` because `max-width` is inclusive.
 */
export const ICON_RAIL_MAX_WIDTH = 900;

export function isIconRail(width: number): boolean {
  return width <= ICON_RAIL_MAX_WIDTH;
}

/** What the shell's pane shows right now. */
export type PaneChoice =
  | { readonly kind: 'terminal'; readonly tabId: string }
  | { readonly kind: 'landing' }
  | { readonly kind: 'list'; readonly hostId: string };

/**
 * Which of the three the pane is showing (#344).
 *
 * Pure, because this is the rule the hosted shell turns on and it has three
 * cases that a component render cannot be asked about. The shell used to have
 * two — terminal or list — and the hosted path had its own three *screens*
 * instead of tabs.
 */
export function paneFor(args: {
  /** The active tab, and whether its dial target is still held. */
  readonly activeTabId: string | null;
  readonly activeHasTarget: boolean;
  readonly routeHost: string | undefined;
  readonly routeSession: string | undefined;
  /** The caller supplied something to show when no machine is named. */
  readonly hasLanding: boolean;
  /** The machine to list when the URL names none. */
  readonly defaultHostId: string | null;
}): PaneChoice {
  const { activeTabId, activeHasTarget, hasLanding } = args;
  // `''` is not a machine. The router can yield an empty param for an
  // unmatched or partial match (`/h//s/7`), and `Shell.syncRoute` already
  // treats that as "no machine named" — so this has to as well, or the two
  // disagree: the route watcher would open nothing while the pane rendered a
  // session list for a host id that is the empty string.
  const routeHost = args.routeHost === '' ? undefined : args.routeHost;
  const routeSession = args.routeSession === '' ? undefined : args.routeSession;
  // The terminal only when the URL names *the active tab's* session. Matching
  // on the params alone would show whichever tab happened to be active while
  // the URL described another — the two move in separate updates, and a render
  // between them must not settle on the stale one.
  if (
    activeTabId !== null &&
    activeHasTarget &&
    routeHost !== undefined &&
    routeSession !== undefined &&
    activeTabId === tabIdOf(routeHost, routeSession)
  ) {
    return { kind: 'terminal', tabId: activeTabId };
  }
  // A machine named in the URL is that machine's list, even when a landing
  // exists: `/h/:hostId` is how the fleet grid's own "open" button gets you to
  // a machine, so answering it with the grid would make that button a no-op.
  if (routeHost !== undefined) return { kind: 'list', hostId: routeHost };
  if (hasLanding) return { kind: 'landing' };
  // Loopback: one machine, and `''` until the directory says who it is.
  // Listing anyway is the point — the list is what shows "reaching its
  // sidecar…", and a blank pane would make a slow start look like a broken one.
  return { kind: 'list', hostId: args.defaultHostId ?? '' };
}
