/**
 * The app shell: the tabbed chrome around the terminal panes (design §1–§2).
 *
 * State here is exactly the client-side list the handoff enumerates — one
 * `TabsState` signal driven by the pure reducers in `state/tabs.ts`, the
 * layout, the launcher, and the palette cursor. Everything else is server
 * state arriving over one of the two planes.
 *
 * Two orderings are load-bearing:
 *
 * - **Keydown pipeline.** `shellChord()` runs FIRST, on a window *capture*
 *   listener, and a hit is dispatched and `preventDefault`ed before the
 *   terminal's own `belongsToBrowser` → encode path (in `TerminalView`) can
 *   see the event. That is the contract `chords.ts` documents.
 * - **Navigation is an EFFECT of activation — one direction only.** Clicking
 *   a tab activates it and then navigates; the route watcher below only ever
 *   *activates* (or opens) and never navigates, so there is no feedback loop.
 */

import { component, onMounted, onUnmounted, signal, watch } from 'sigx';
import { useNavigate, useRoute } from '@sigx/router';
import type { Theme } from '@zesterm/theme';
import { modsOf, shellChord, type ShellAction } from '@zesterm/input';
import type { Dial } from '@zesterm/client';
import type { DirectoryView } from '@zesterm/control';

import {
  launcherRows,
  shortHostId,
  tabIdOf,
  type HostChoice,
} from '../chrome-model.ts';
import { createSessionOverDataPlane } from '../create-session.ts';
import { dialFor } from '../dial-for.ts';
import type { DeviceKey } from '../device-key.ts';
import { actorDirectorySource } from '../directory-source.ts';
import { localHostSource, type HostSource } from '../host-source.ts';
import {
  activate,
  closeTab,
  NO_TABS,
  openTab,
  setLink,
  setTitle,
  type Tab,
  type TabsState,
} from '../state/tabs.ts';
import { loadLayout, saveLayout, toggleLayout, type Layout } from '../state/layout.ts';
import {
  closePalette,
  moveSelection,
  openPalette,
  PALETTE_CLOSED,
  setQuery,
  type PaletteState,
} from '../state/palette.ts';
import { themeStore } from '../state/theme.ts';
import { flattenResults, rankResults, type PaletteSources } from '../palette/rank.ts';
import {
  actionItems,
  blockItems,
  hostItems,
  hostsSearchedCount,
  runTargetOf,
  sessionItems,
  type AttachedTabBlocks,
  type PaletteItem,
} from '../palette/sources.ts';
import { Palette } from './Palette.tsx';
import { SessionList, type OpenTarget } from './SessionList.tsx';
import { SidebarTabs, VerticalHeader } from './SidebarTabs.tsx';
import { TabStrip } from './TabStrip.tsx';
import { TerminalView, type TerminalHooks } from './TerminalView.tsx';

export const Shell = component<{ device: DeviceKey; theme: Theme }>((ctx) => {
  const { device, theme } = ctx.props;
  // `useRoute()`, not `useParams()`, and the params are read off it at every
  // use rather than captured here. `useParams()` IS `useRoute().params`, and
  // the router replaces that record wholesale on each navigation — so a
  // reference taken at setup is frozen for the life of the component and
  // registers no dependency on the route. The pane then kept rendering the
  // session list while the URL said a session, and only a full reload showed
  // the terminal. `routes.tsx` records the same trap for `useQuery()`; this is
  // it one hook over. (#196)
  const route = useRoute();
  const navigate = useNavigate();
  // The shell's own directory read, beside the one SessionList makes: the
  // launcher needs the host and the route watcher needs to resolve a session
  // id to an entry, and neither may borrow a reader whose lifetime belongs to
  // the list (see directory-source.ts on why sources are per-component).
  const directory = actorDirectorySource();
  // What this shell can launch on, and how to reach each (#332). Built here,
  // in setup, because it closes over `directory` — a live actor read whose
  // subscription belongs to this instance (see `directory-source.ts`).
  //
  // Loopback answers with the directory's own machine, which is exactly what
  // the three inlined lookups below used to do. The hosted path answers with
  // the account's machines, and `Shell` does not have to know which it got.
  const hostSource: HostSource = localHostSource(directory);

  const store = signal<{
    tabs: TabsState;
    layout: Layout;
    launcherOpen: boolean;
    palette: PaletteState;
    error: string | null;
  }>({
    tabs: NO_TABS,
    layout: loadLayout(window.localStorage),
    launcherOpen: false,
    palette: PALETTE_CLOSED,
    error: null,
  });

  // How to mount each tab's TerminalView. Beside the tabs rather than inside
  // them: `Tab` is pure data shared with node tests, and a dial target is
  // this component's wiring, not state the reducers should know about.
  const targets = new Map<string, OpenTarget>();

  // The palette's reach: each mounted TerminalView registers its hooks here
  // and revokes them on unmount, so this map IS the set of grids the browser
  // holds — the "N hosts searched" count states exactly its hosts. Today the
  // shell mounts one view (the active tab); a future multi-pane shell widens
  // the map without the palette changing.
  const termHooks = new Map<string, TerminalHooks>();

  const tabFor = (target: OpenTarget): Tab => {
    const e = target.entry;
    const id = tabIdOf(e.host, e.session);
    return {
      id,
      kind: 'session',
      title: e.title,
      hostId: e.host,
      cwd: e.cwd,
      color: null,
      panes: [{ id: `${id}-p0`, hostId: e.host, sessionId: e.session, focused: true }],
      // The SessionClient starts in 'connecting'; TerminalView flips this to
      // 'live' when the connection actually exists. Starting live would show
      // a healthy tab for a session that may never connect.
      link: 'stalled',
    };
  };

  const urlOfTab = (t: Tab): string | null => {
    const pane = t.panes[0];
    return pane === undefined ? null : `/h/${t.hostId}/s/${pane.sessionId}`;
  };

  const openTarget = (target: OpenTarget, navigateTo: boolean): void => {
    const id = tabIdOf(target.entry.host, target.entry.session);
    if (store.tabs.tabs.some((t) => t.id === id)) {
      store.tabs = activate(store.tabs, id);
    } else {
      targets.set(id, target);
      store.tabs = openTab(store.tabs, tabFor(target));
    }
    if (navigateTo) void navigate(`/h/${target.entry.host}/s/${target.entry.session}`);
  };

  const activateAndNavigate = (id: string): void => {
    store.tabs = activate(store.tabs, id);
    const t = store.tabs.tabs.find((x) => x.id === id);
    const url = t === undefined ? null : urlOfTab(t);
    if (url !== null) void navigate(url);
  };

  const close = (id: string): void => {
    const wasActive = store.tabs.activeId === id;
    store.tabs = closeTab(store.tabs, id);
    targets.delete(id);
    if (!wasActive) return;
    // Focus fell to the neighbour (the reducer's rule); the URL follows it.
    const next = store.tabs.tabs.find((t) => t.id === store.tabs.activeId);
    const url = next === undefined ? null : urlOfTab(next);
    void navigate(url ?? '/hosts');
  };

  // All three read the seam now, so the shell is host-plural by construction
  // rather than by a list that happens to hold one (#332). `launcherRows` was
  // already pure over any host list; this is what finally hands it one.
  const hostChoices = (): readonly HostChoice[] => hostSource.hosts();

  // The first, which on loopback is the only one. A hosted shell will want a
  // remembered choice here rather than positional luck — its own item.
  const defaultHostId = (): string | null => hostChoices()[0]?.id ?? null;

  const hostLabelsOf = (): Readonly<Record<string, string>> =>
    Object.fromEntries(hostChoices().map((h) => [h.id, h.label]));

  // The existing create path, unchanged underneath: a one-shot data-plane
  // connection creates the session, then the new tab opens on the address the
  // daemon confirmed.
  // Returns the chain so `SessionList` can hold its button for the whole round
  // trip rather than for the synchronous call — a create is asynchronous in
  // both worlds, so a guard that clears on return guards nothing.
  const createAt = (dial: Dial): Promise<void> => {
    store.launcherOpen = false;
    return createSessionOverDataPlane({ dial, signer: device.signer, cols: 120, rows: 32 })
      .then((addr) => {
        store.error = null;
        openTarget(
          {
            dial,
            entry: {
              host: addr.host,
              session: addr.session.toString(),
              title: '',
              cwd: '',
              cols: 120,
              rows: 32,
              altScreen: false,
              attached: false,
            },
          },
          true,
        );
      })
      .catch((e: unknown) => {
        store.error = e instanceof Error ? e.message : String(e);
      });
  };

  const createOn = (hostId: string): void => {
    const dial = hostSource.dialFor(hostId);
    if (dial === null) {
      store.error = 'that machine is not dialable from here';
      return;
    }
    void createAt(dial);
  };

  /**
   * Everything the palette searches, gathered fresh at call time: blocks from
   * the registered grids, sessions from the open tabs plus the directory,
   * hosts from the launcher's list, and the static actions. A function, not
   * cached state — the handlers re-ask so a wrap or a ⏎ is decided against
   * the results that exist at the keystroke, not at the last render.
   */
  const paletteData = (): { sources: PaletteSources; hostsSearched: number } => {
    const labels = hostLabelsOf();
    const attached: AttachedTabBlocks[] = [];
    for (const [tabId, hooks] of termHooks) {
      const tab = store.tabs.tabs.find((t) => t.id === tabId);
      if (tab === undefined) continue;
      attached.push({
        tabId,
        hostId: tab.hostId,
        hostLabel: labels[tab.hostId] ?? shortHostId(tab.hostId),
        blocks: hooks.blocks(),
      });
    }
    const dir = directory();
    return {
      sources: {
        blocks: blockItems(attached, Date.now()),
        sessions: sessionItems(store.tabs.tabs, dir.kind === 'ready' ? dir.view.sessions : [], labels),
        hosts: hostItems(hostChoices()),
        actions: actionItems(),
      },
      hostsSearched: hostsSearchedCount(attached),
    };
  };

  const movePaletteSelection = (delta: number): void => {
    const groups = rankResults(store.palette.query, paletteData().sources);
    store.palette = moveSelection(store.palette, delta, flattenResults(groups).length);
  };

  const runPaletteItem = (item: PaletteItem): void => {
    // Close FIRST: unmounting the palette restores focus to the terminal
    // textarea, so a run-block's typed bytes follow a focused terminal.
    store.palette = closePalette(store.palette);
    const target = runTargetOf(item, store.tabs.activeId);
    switch (target.kind) {
      case 'run-block':
        // The terminal's own gate (runCommand = the ⌘⇧R rule) decides whether
        // typing is safe; at a running command or in the alt screen it
        // declines and nothing destructive happens.
        termHooks.get(target.tabId)?.runCommand(target.command);
        break;
      case 'activate-tab':
        // A block on a background tab lands here (runTargetOf): activate so
        // the user can SEE the prompt state, and do nothing destructive.
        activateAndNavigate(target.tabId);
        break;
      case 'open-session': {
        const dir = directory();
        if (dir.kind !== 'ready') break;
        const entry = dir.view.sessions.find(
          (e) => e.host === target.hostId && e.session === target.sessionId,
        );
        // Dialled by the *entry's* host, not by "the directory's plane" —
        // the same question `createOn` asks, and on a shell that holds
        // several machines they are different answers (#332).
        const dial = hostSource.dialFor(target.hostId);
        if (entry !== undefined && dial !== null) openTarget({ entry, dial }, true);
        break;
      }
      case 'create-session':
        createOn(target.hostId);
        break;
      case 'layout-toggle':
        dispatch({ kind: 'layout-toggle' });
        break;
      case 'set-theme':
        themeStore()?.setTheme(target.themeId);
        break;
    }
  };

  /**
   * URL → tabs, the one allowed direction of that arrow. Activates the named
   * tab if it is open; opens it from the directory once the directory is
   * ready; never navigates. Idempotent, because the watch below re-runs on
   * every directory turn.
   */
  const syncRoute = (): void => {
    const h = route.params['hostId'];
    const s = route.params['sessionId'];
    if (h === undefined || s === undefined || h === '' || s === '') return;
    const id = tabIdOf(h, s);
    if (store.tabs.activeId === id) return;
    if (store.tabs.tabs.some((t) => t.id === id)) {
      store.tabs = activate(store.tabs, id);
      return;
    }
    const dir = directory();
    if (dir.kind !== 'ready') return;
    const entry = dir.view.sessions.find((e) => e.host === h && e.session === s);
    // By the host the URL names, for `open-session`'s reason: a shell holding
    // several machines cannot dial "the" plane.
    const dial = hostSource.dialFor(h);
    if (entry === undefined || dial === null) return;
    openTarget({ entry, dial }, false);
  };

  const routeWatch = watch(
    () => [route.params['hostId'], route.params['sessionId'], directory()] as const,
    syncRoute,
    { immediate: true },
  );
  onUnmounted(() => routeWatch.stop());

  const platform: 'mac' | 'other' = navigator.platform.toLowerCase().includes('mac')
    ? 'mac'
    : 'other';

  const dispatch = (action: ShellAction): void => {
    switch (action.kind) {
      case 'palette':
        store.palette = store.palette.open ? closePalette(store.palette) : openPalette();
        break;
      case 'layout-toggle': {
        const next = toggleLayout(store.layout);
        store.layout = next;
        saveLayout(window.localStorage, next);
        break;
      }
      case 'tab-n': {
        // ⌘N launches the Nth launcher row — the browser's stand-in for "the
        // Nth profile" until profiles exist here.
        const row = launcherRows(hostChoices(), defaultHostId())[action.n - 1];
        if (row !== undefined) createOn(row.hostId);
        break;
      }
      // split / settings / profiles / copy-output / re-run: claimed so the
      // terminal never types them, acted on by their own work items.
      default:
        break;
    }
  };

  // Window CAPTURE so a claimed chord never reaches the terminal textarea's
  // own keydown handler — stopPropagation in the capture phase halts the
  // event before the target phase.
  const onWindowKeyDown = (e: KeyboardEvent): void => {
    const action = shellChord(e, modsOf(e), platform);
    if (action === null) return;
    e.preventDefault();
    e.stopPropagation();
    // While the palette is open it owns the keyboard: every chord is still
    // claimed (the terminal must never see one), but only the palette toggle
    // acts — a layout flip behind a modal is a surprise, not a feature.
    if (store.palette.open && action.kind !== 'palette') return;
    dispatch(action);
  };
  onMounted(() => window.addEventListener('keydown', onWindowKeyDown, true));
  onUnmounted(() => window.removeEventListener('keydown', onWindowKeyDown, true));

  return () => {
    const tabs = store.tabs;
    const active = tabs.tabs.find((t) => t.id === tabs.activeId) ?? null;
    const rows = launcherRows(hostChoices(), defaultHostId());
    const labels = hostLabelsOf();
    // Read inside the render, so the pane tracks the URL: activation and
    // navigation are two separate updates and the render triggered by the
    // first one must not settle on the route as it was before the second.
    const routeH = route.params['hostId'];
    const routeS = route.params['sessionId'];

    // The pane shows the active tab's terminal when the route names it, and
    // the session directory otherwise (`/hosts` — where the sidebar footer
    // lands). `key` forces a remount per tab: a TerminalView's client is
    // created in setup, so reusing the instance across tabs would leave it
    // attached to the previous session.
    const pane = ((): unknown => {
      const target = active === null ? undefined : targets.get(active.id);
      if (
        active !== null &&
        target !== undefined &&
        routeH !== undefined &&
        routeS !== undefined &&
        active.id === tabIdOf(routeH, routeS)
      ) {
        const id = active.id;
        return (
          <TerminalView
            key={id}
            entry={target.entry}
            dial={target.dial}
            signer={device.signer}
            theme={theme}
            onTitle={(title: string) => (store.tabs = setTitle(store.tabs, id, title))}
            onLink={(link) => (store.tabs = setLink(store.tabs, id, link))}
            register={(hooks: TerminalHooks | null) => {
              if (hooks === null) termHooks.delete(id);
              else termHooks.set(id, hooks);
            }}
          />
        );
      }
      return (
        <SessionList
          source={actorDirectorySource}
          deviceKind={device.kind}
          onOpen={(t: OpenTarget) => openTarget(t, true)}
          // The seam hands over the whole view rather than a URL, because the
          // hosted path has no URL to hand: it creates on the connection it is
          // already holding. Loopback answers it the way it always has.
          onCreate={(view: DirectoryView) => {
            const dial = dialFor(view.dataPlane, null);
            return dial === null ? undefined : createAt(dial);
          }}
        />
      );
    })();

    // Recomputed by this render whenever the query or the selection moves —
    // signal writes re-run the render fn, which re-asks paletteData(), so the
    // list tracks the keystrokes without its own store.
    const paletteEl = ((): unknown => {
      if (!store.palette.open) return null;
      const { sources, hostsSearched } = paletteData();
      return (
        <Palette
          query={store.palette.query}
          selection={store.palette.selection}
          groups={rankResults(store.palette.query, sources)}
          hostsSearched={hostsSearched}
          onQuery={(q: string) => (store.palette = setQuery(store.palette, q))}
          onMove={movePaletteSelection}
          onRun={runPaletteItem}
          onDismiss={() => (store.palette = closePalette(store.palette))}
        />
      );
    })();

    if (store.layout === 'vertical') {
      return (
        <div class="shell vertical">
          {store.error !== null ? <div class="shell-error">{store.error}</div> : null}
          {paletteEl}
          <VerticalHeader
            active={active}
            hostLabels={labels}
            onPalette={() => dispatch({ kind: 'palette' })}
          />
          <div class="v-body">
            <SidebarTabs
              tabs={tabs.tabs}
              activeId={tabs.activeId}
              hostLabels={labels}
              launcherOpen={store.launcherOpen}
              launcherRows={rows}
              onActivate={activateAndNavigate}
              onLauncherToggle={() => (store.launcherOpen = !store.launcherOpen)}
              onLaunch={createOn}
              onLauncherDismiss={() => (store.launcherOpen = false)}
              onPalette={() => dispatch({ kind: 'palette' })}
              onHosts={() => void navigate('/hosts')}
            />
            <section class="pane">{pane}</section>
          </div>
        </div>
      );
    }

    return (
      <div class="shell horizontal">
        {store.error !== null ? <div class="shell-error">{store.error}</div> : null}
        {paletteEl}
        <TabStrip
          tabs={tabs.tabs}
          activeId={tabs.activeId}
          hostLabels={labels}
          launcherOpen={store.launcherOpen}
          launcherRows={rows}
          onActivate={activateAndNavigate}
          onClose={close}
          onLauncherToggle={() => (store.launcherOpen = !store.launcherOpen)}
          onLaunch={createOn}
          onLauncherDismiss={() => (store.launcherOpen = false)}
          onPalette={() => dispatch({ kind: 'palette' })}
        />
        <section class="pane">{pane}</section>
      </div>
    );
  };
});
