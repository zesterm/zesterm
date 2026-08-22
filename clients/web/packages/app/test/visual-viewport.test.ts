/**
 * The soft keyboard on iOS Safari overlays the page and pushes it up instead
 * of shrinking the layout viewport, so a `100%`-tall shell keeps its prompt
 * line under the keys (#421). These pin the arithmetic that sizes the shell
 * to the visual viewport instead — and, as importantly, when it must NOT.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { NO_SHELL_VARS, shellVarsFor, watchVisualViewport } from '../src/visual-viewport.ts';

test('with no keyboard the variables are cleared, not set to the same height', () => {
  assert.deepEqual(
    shellVarsFor({ height: 1024, offsetTop: 0 }, 1024),
    NO_SHELL_VARS,
    'the stylesheet’s own 100% must take over — a pinned pixel height would freeze the shell across a rotation',
  );
});

test('a keyboard shrinks the shell to the visible height and follows the push-up', () => {
  assert.deepEqual(
    shellVarsFor({ height: 612, offsetTop: 0 }, 1024),
    { '--zt-vv-height': '612px', '--zt-vv-top': '0px' },
    'the shell is exactly what is on screen, so the prompt line sits above the keys',
  );
  // iOS then scrolls the page so the focused element is visible: same
  // height, new offset — the shell has to travel with it or its chrome is
  // off the top of the screen.
  assert.deepEqual(
    shellVarsFor({ height: 612, offsetTop: 140 }, 1024),
    { '--zt-vv-height': '612px', '--zt-vv-top': '140px' },
  );
});

test('sub-pixel disagreement between the viewports is not a keyboard', () => {
  assert.deepEqual(
    shellVarsFor({ height: 1023.5, offsetTop: 0.25 }, 1024),
    NO_SHELL_VARS,
    'scroll-bar and rubber-band jitter would otherwise resize every pty on every scroll',
  );
});

test('fractional visual heights are floored so the shell never overhangs', () => {
  assert.equal(shellVarsFor({ height: 611.6, offsetTop: 0 }, 1024)['--zt-vv-height'], '611px');
});

test('an unmeasurable viewport clears rather than writing 0px', () => {
  assert.deepEqual(shellVarsFor({ height: 0, offsetTop: 0 }, 1024), NO_SHELL_VARS);
  assert.deepEqual(shellVarsFor({ height: 600, offsetTop: 0 }, 0), NO_SHELL_VARS);
});

test('the watcher mirrors the viewport onto the root and removes what it cleared', () => {
  const props = new Map<string, string>();
  const root = {
    style: {
      setProperty: (n: string, v: string) => void props.set(n, v),
      removeProperty: (n: string) => void props.delete(n),
    },
  } as unknown as HTMLElement;
  const listeners = new Map<string, () => void>();
  const vv = {
    height: 1024,
    offsetTop: 0,
    addEventListener: (type: string, fn: () => void) => void listeners.set(type, fn),
    removeEventListener: (type: string) => void listeners.delete(type),
  };
  const win = { visualViewport: vv as never, innerHeight: 1024 };

  const stop = watchVisualViewport(win, root);
  assert.equal(props.size, 0, 'no keyboard at mount: nothing written');

  vv.height = 612;
  listeners.get('resize')?.();
  assert.equal(props.get('--zt-vv-height'), '612px');

  vv.height = 1024;
  listeners.get('resize')?.();
  assert.equal(props.size, 0, 'keyboard dismissed: both properties removed, so 100% applies again');

  stop();
  assert.equal(listeners.size, 0);
});

test('a browser without visualViewport is left alone', () => {
  const root = { style: { setProperty: () => assert.fail('must not write') } } as unknown as HTMLElement;
  watchVisualViewport({ visualViewport: null, innerHeight: 800 }, root)();
});
