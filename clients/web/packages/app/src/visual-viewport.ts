/**
 * Keeping the shell inside the *visual* viewport when a soft keyboard is up.
 *
 * Two viewports exist on a touch browser. The layout viewport is what `100%`
 * and `100vh` measure; the visual viewport is what is actually on screen. iOS
 * Safari does not shrink the layout viewport for the soft keyboard — it
 * overlays the keys, then pushes the page up so the focused element is in
 * view. For a terminal whose height is `100%` of a layout viewport that never
 * changed, that means the prompt line, the caret and the bottom of the alt
 * screen are under the keyboard, and the chrome is scrolled off the top.
 * (Chrome on Android resizes the layout viewport instead when the viewport
 * meta carries `interactive-widget=resizes-content`, and then the two agree
 * and this file has nothing to do.)
 *
 * The fix is to size the shell to the visual viewport while the two disagree
 * and put it back the moment they agree again. The decision is pure
 * (`shellVarsFor`) so the arithmetic is tested; `watchVisualViewport` is the
 * DOM plumbing around it.
 */

/** The slice of `VisualViewport` this reads — structural, so tests hand-build it. */
export interface VisualViewportLike {
  readonly height: number;
  /** Distance from the top of the layout viewport to the top of the visual one. */
  readonly offsetTop: number;
}

export interface ShellVars {
  readonly '--zt-vv-height': string | null;
  readonly '--zt-vv-top': string | null;
}

/** Both cleared: the stylesheet's own `100%` takes over. */
export const NO_SHELL_VARS: ShellVars = { '--zt-vv-height': null, '--zt-vv-top': null };

/**
 * Sub-pixel jitter: a visual viewport that is 0.5px shorter than the layout
 * one is not a keyboard, and a rule that fires on it would re-lay out the
 * shell (and resize every pty) on scroll-bar and rubber-band noise.
 */
const KEYBOARD_SLACK_PX = 2;

/**
 * What to write on the root element for this visual viewport.
 *
 * `null` values mean "remove the property" rather than "set to nothing": the
 * shell's height must fall back to the stylesheet's `100%`, and an empty
 * custom property would make `var(--zt-vv-height, 100%)` resolve to nothing
 * at all.
 */
export function shellVarsFor(vv: VisualViewportLike, layoutHeight: number): ShellVars {
  if (!(vv.height > 0) || !(layoutHeight > 0)) return NO_SHELL_VARS;
  if (layoutHeight - vv.height <= KEYBOARD_SLACK_PX && vv.offsetTop <= KEYBOARD_SLACK_PX) {
    return NO_SHELL_VARS;
  }
  // Floored, so the shell never overhangs the visible area by a fraction of
  // a pixel and scrolls the document by one line.
  return {
    '--zt-vv-height': `${Math.floor(vv.height)}px`,
    '--zt-vv-top': `${Math.floor(vv.offsetTop)}px`,
  };
}

export interface WindowLike {
  readonly visualViewport: (VisualViewportLike & EventTarget) | null;
  readonly innerHeight: number;
}

/**
 * Subscribe to the visual viewport and mirror it onto `root`'s style.
 *
 * Returns the unsubscribe, which the app never calls (the shell lives as long
 * as the page) but a test can. A browser with no `visualViewport` gets the
 * stylesheet's behaviour and nothing else.
 */
export function watchVisualViewport(win: WindowLike, root: HTMLElement): () => void {
  const vv = win.visualViewport;
  if (vv === null) return () => {};
  // What was last written. `scroll` fires on every frame of a pan while
  // the keyboard is up, and a style write per frame — even of the same
  // value — is a style invalidation per frame; only a change reaches the DOM.
  let applied: ShellVars = NO_SHELL_VARS;
  const apply = (): void => {
    const vars = shellVarsFor(vv, win.innerHeight);
    for (const name of Object.keys(vars) as (keyof ShellVars)[]) {
      const value = vars[name];
      if (value === applied[name]) continue;
      if (value === null) root.style.removeProperty(name);
      else root.style.setProperty(name, value);
    }
    applied = vars;
  };
  // Both: the keyboard opening fires `resize`, and iOS's push-up of the page
  // afterwards fires `scroll` with a new offsetTop and the same height.
  vv.addEventListener('resize', apply);
  vv.addEventListener('scroll', apply);
  apply();
  return () => {
    vv.removeEventListener('resize', apply);
    vv.removeEventListener('scroll', apply);
  };
}
