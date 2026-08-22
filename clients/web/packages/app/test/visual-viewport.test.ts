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
  let writes = 0;
  const classes = new Set<string>();
  const upSeen: boolean[] = [];
  const root = {
    classList: {
      toggle: (c: string, on: boolean) => {
        if (on) classes.add(c);
        else classes.delete(c);
      },
    },
    style: {
      setProperty: (n: string, v: string) => {
        writes++;
        props.set(n, v);
      },
      removeProperty: (n: string) => {
        writes++;
        props.delete(n);
      },
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

  const stop = watchVisualViewport(win, root, (up) => upSeen.push(up));
  assert.equal(props.size, 0, 'no keyboard at mount: nothing written');
  assert.deepEqual(upSeen, [], 'keyboard down at mount is not a change to report');
  assert.equal(writes, 0, 'and no removeProperty of something never set — a style write per event is a style invalidation per event');

  vv.height = 612;
  listeners.get('resize')?.();
  assert.equal(props.get('--zt-vv-height'), '612px');
  assert.equal(writes, 2);
  assert.ok(classes.has('kbd-up'), 'the stylesheet reads keyboard-up as a root class (#428)');
  assert.deepEqual(upSeen, [true], 'the ⌨ cap reads keyboard-up from here, not from activeElement');

  // A pan while the keyboard is up fires `scroll` on every frame with the
  // same geometry; none of those may reach the DOM.
  listeners.get('scroll')?.();
  listeners.get('scroll')?.();
  assert.equal(writes, 2, 'unchanged values are not rewritten');

  vv.offsetTop = 40;
  listeners.get('scroll')?.();
  assert.equal(props.get('--zt-vv-top'), '40px');
  assert.equal(writes, 3, 'only the property that changed is written');
  assert.deepEqual(upSeen, [true], 'a pan is not a keyboard change');

  vv.height = 1024;
  vv.offsetTop = 0;
  listeners.get('resize')?.();
  assert.equal(props.size, 0, 'keyboard dismissed: both properties removed, so 100% applies again');
  assert.equal(writes, 5);
  assert.ok(!classes.has('kbd-up'));
  assert.deepEqual(upSeen, [true, false]);

  stop();
  assert.equal(listeners.size, 0);
});

test('a browser without visualViewport is left alone', () => {
  const root = { style: { setProperty: () => assert.fail('must not write') } } as unknown as HTMLElement;
  watchVisualViewport({ visualViewport: null, innerHeight: 800 }, root)();
});
