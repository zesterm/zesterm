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
import { useNavigate, useParams } from '@sigx/router';
import type { Theme } from '@zesterm/theme';
import { modsOf, shellChord, type ShellAction } from '@zesterm/input';

import {
  launcherRows,
  tabIdOf,
  type HostChoice,
} from '../chrome-model.ts';
import { createSessionOverDataPlane } from '../create-session.ts';
import { dataPlaneUrl } from '../data-plane-url.ts';
import type { DeviceKey } from '../device-key.ts';
import { actorDirectorySource } from '../directory-source.ts';
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
import { closePalette, openPalette, PALETTE_CLOSED, type PaletteState } from '../state/palette.ts';
import { SessionList, type OpenTarget } from './SessionList.tsx';
import { SidebarTabs, VerticalHeader } from './SidebarTabs.tsx';
import { TabStrip } from './TabStrip.tsx';
import { TerminalView } from './TerminalView.tsx';

export const Shell = component<{ device: DeviceKey; theme: Theme }>((ctx) => {
  const { device, theme } = ctx.props;
  const params = useParams();
  const navigate = useNavigate();
  // The shell's own directory read, beside the one SessionList makes: the
  // launcher needs the host and the route watcher needs to resolve a session
  // id to an entry, and neither may borrow a reader whose lifetime belongs to
  // the list (see directory-source.ts on why sources are per-component).
  const directory = actorDirectorySource();

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

  // Local mode has exactly one launchable host: the directory's own. The
  // hosted path (fetchRegistry hosts) plugs in here when the hosted shell
  // exists — `launcherRows` is already pure over any host list.
  const hostChoices = (): readonly HostChoice[] => {
    const dir = directory();
    if (dir.kind === 'ready' && dir.view.host !== null) {
      return [{ id: dir.view.host.id, label: dir.view.host.label }];
    }
    return [];
  };

  const defaultHostId = (): string | null => {
    const dir = directory();
    return dir.kind === 'ready' && dir.view.host !== null ? dir.view.host.id : null;
  };

  const hostLabelsOf = (): Readonly<Record<string, string>> => {
    const dir = directory();
    if (dir.kind === 'ready' && dir.view.host !== null) {
      return { [dir.view.host.id]: dir.view.host.label };
    }
    return {};
  };

  // The existing create path, unchanged underneath: a one-shot data-plane
  // connection creates the session, then the new tab opens on the address the
  // daemon confirmed.
  const createAt = (url: string): void => {
    store.launcherOpen = false;
    createSessionOverDataPlane({ url, signer: device.signer, cols: 120, rows: 32 })
      .then((addr) => {
        store.error = null;
        openTarget(
          {
            dataPlaneUrl: url,
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
    const dir = directory();
    if (dir.kind !== 'ready' || dir.view.host === null || dir.view.host.id !== hostId) return;
    const url = dataPlaneUrl(dir.view.dataPlane);
    if (url === null) {
      store.error = 'the daemon is not dialable from here';
      return;
    }
    createAt(url);
  };

  /**
   * URL → tabs, the one allowed direction of that arrow. Activates the named
   * tab if it is open; opens it from the directory once the directory is
   * ready; never navigates. Idempotent, because the watch below re-runs on
   * every directory turn.
   */
  const syncRoute = (): void => {
    const h = params['hostId'];
    const s = params['sessionId'];
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
    const url = dataPlaneUrl(dir.view.dataPlane);
    if (entry === undefined || url === null) return;
    openTarget({ entry, dataPlaneUrl: url }, false);
  };

  const routeWatch = watch(
    () => [params['hostId'], params['sessionId'], directory()] as const,
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
        // Toggled but not yet rendered — the palette is its own work item;
        // claiming the chord now is what keeps it out of the terminal.
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
    dispatch(action);
  };
  onMounted(() => window.addEventListener('keydown', onWindowKeyDown, true));
  onUnmounted(() => window.removeEventListener('keydown', onWindowKeyDown, true));

  return () => {
    const tabs = store.tabs;
    const active = tabs.tabs.find((t) => t.id === tabs.activeId) ?? null;
    const rows = launcherRows(hostChoices(), defaultHostId());
    const labels = hostLabelsOf();
    const routeH = params['hostId'];
    const routeS = params['sessionId'];

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
            dataPlaneUrl={target.dataPlaneUrl}
            signer={device.signer}
            theme={theme}
            onTitle={(title: string) => (store.tabs = setTitle(store.tabs, id, title))}
            onLink={(link) => (store.tabs = setLink(store.tabs, id, link))}
          />
        );
      }
      return (
        <SessionList
          source={actorDirectorySource}
          deviceKind={device.kind}
          onOpen={(t: OpenTarget) => openTarget(t, true)}
          onCreate={createAt}
        />
      );
    })();

    if (store.layout === 'vertical') {
      return (
        <div class="shell vertical">
          {store.error !== null ? <div class="shell-error">{store.error}</div> : null}
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
