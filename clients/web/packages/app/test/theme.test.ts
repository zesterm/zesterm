import { test } from 'node:test';
import assert from 'node:assert/strict';
import { themeById, DEFAULT_DARK, type Theme } from '@zesterm/theme';

import {
  createThemeStore,
  initThemeStore,
  themeStore,
  type StorageLike,
} from '../src/state/theme.ts';

// Map-backed fakes: the store's interfaces are structural exactly so these
// tests run under node with no DOM.
function fakeStorage(seed: Record<string, string> = {}): StorageLike & {
  data: Map<string, string>;
} {
  const data = new Map(Object.entries(seed));
  return {
    data,
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
  };
}

function fakeTarget(): { props: Map<string, string>; style: { setProperty(k: string, v: string): void } } {
  const props = new Map<string, string>();
  return { props, style: { setProperty: (k, v) => void props.set(k, v) } };
}

test('construction reads the stored id and applies the full theme CSS', () => {
  const el = fakeTarget();
  const storage = fakeStorage({ 'zesterm.theme': 'paper' });
  const store = createThemeStore(el, storage);

  assert.equal(store.theme.id, 'paper', 'the stored choice must win over the default');
  assert.equal(
    el.props.size,
    27,
    'boot paints tokens AND derived surfaces — applyCssVars alone leaves the chrome unstyled',
  );
  assert.equal(el.props.get('--zt-bg'), store.theme.ui.bg);
  assert.equal(
    storage.data.get('zesterm.boot-bg'),
    store.theme.ui.bg,
    'index.html replays boot-bg before the bundle loads; boot must keep it current',
  );
});

test('an unknown or missing stored id falls back to the default dark theme', () => {
  const missing = createThemeStore(fakeTarget(), fakeStorage());
  assert.equal(missing.theme.id, DEFAULT_DARK, 'no choice yet means the default');

  const unknown = createThemeStore(fakeTarget(), fakeStorage({ 'zesterm.theme': 'no-such' }));
  assert.equal(
    unknown.theme.id,
    DEFAULT_DARK,
    'a stale id (a theme removed in an update) must not leave the app unstyled',
  );
});

test('setTheme applies vars, persists both keys, and notifies subscribers', () => {
  const el = fakeTarget();
  const storage = fakeStorage();
  const store = createThemeStore(el, storage);
  const paper = themeById('paper')!;

  const heard: Theme[] = [];
  store.onThemeChange((t) => heard.push(t));

  store.setTheme('paper');

  assert.equal(store.theme.id, 'paper');
  assert.equal(
    el.props.get('--zt-bg'),
    paper.ui.bg,
    'the switch must repaint the CSS vars, not just record the choice',
  );
  assert.equal(
    el.props.get('--zt-titlebar') !== undefined && el.props.get('--zt-titlebar') !== '#0d121f',
    true,
    "the derived surfaces must re-derive — obsidian's dark titlebar on paper is the bug this exists to prevent",
  );
  assert.equal(storage.data.get('zesterm.theme'), 'paper', 'the choice survives a reload');
  assert.equal(
    storage.data.get('zesterm.boot-bg'),
    paper.ui.bg,
    'the next first paint must be the NEW background',
  );
  assert.deepEqual(
    heard.map((t) => t.id),
    ['paper'],
    'live components repaint off this notification',
  );
});

test('setTheme with an unknown id falls back to the default dark theme', () => {
  const storage = fakeStorage({ 'zesterm.theme': 'paper' });
  const store = createThemeStore(fakeTarget(), storage);

  store.setTheme('no-such-theme');

  assert.equal(store.theme.id, DEFAULT_DARK, 'an unknown id must not strand the previous theme');
  assert.equal(
    storage.data.get('zesterm.theme'),
    DEFAULT_DARK,
    'the resolved id is persisted, so the next boot agrees with what is on screen',
  );
});

test('unsubscribing stops notifications without disturbing other subscribers', () => {
  const store = createThemeStore(fakeTarget(), fakeStorage());
  const first: string[] = [];
  const second: string[] = [];
  const unsub = store.onThemeChange((t) => first.push(t.id));
  store.onThemeChange((t) => second.push(t.id));

  store.setTheme('paper');
  unsub();
  store.setTheme('nord');

  assert.deepEqual(first, ['paper'], 'an unmounted component must not repaint a dead painter');
  assert.deepEqual(second, ['paper', 'nord'], 'unsubscribing one listener must not silence the rest');
});

test('a storage that throws on write (private mode) does not break the switch', () => {
  const el = fakeTarget();
  const storage: StorageLike = {
    getItem: () => null,
    setItem: () => {
      throw new Error('QuotaExceededError');
    },
  };
  const store = createThemeStore(el, storage);
  const heard: string[] = [];
  store.onThemeChange((t) => heard.push(t.id));

  store.setTheme('paper');

  assert.equal(store.theme.id, 'paper', 'the theme still switches for this session');
  assert.equal(el.props.get('--zt-bg'), themeById('paper')!.ui.bg, 'and still paints');
  assert.deepEqual(heard, ['paper'], 'and still notifies — persistence is best-effort');
});

test('initThemeStore registers the app-wide store themeStore() hands out', () => {
  assert.equal(themeStore(), null, 'before boot there is no store, and callers must tolerate that');
  const store = initThemeStore(fakeTarget(), fakeStorage());
  assert.equal(
    themeStore(),
    store,
    'components without props access (TerminalView) must find the same instance boot made',
  );
});
