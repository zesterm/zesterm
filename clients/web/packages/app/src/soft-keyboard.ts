/**
 * The soft keyboard: what a tap should do to the hidden textarea, and
 * whether the keyboard is up at all (#428).
 *
 * iOS opens the keyboard for one thing only: a focus *change* caused inside
 * a user gesture's own task. `focus()` on the element that already holds
 * focus is a no-op, and the terminal's textarea holds focus from mount on —
 * so the synchronous `focus()` a tap runs (#421) opened nothing, ever. The
 * re-open is `blur()` then `focus()` in the same task, which is a focus
 * change by the letter of the rule. That is `'refocus'`.
 *
 * And `document.activeElement` does not say whether the keyboard is up:
 * iOS's own dismiss key hides it without blurring. What does say is the
 * visual viewport — `visual-viewport.ts` already decides "the keyboard is
 * taking space" to size the shell, and that decision is mirrored here so
 * the ⌨ cap can read it. A floating or split iPad keyboard shrinks nothing,
 * so it reads as down and ⌨ always re-opens; its own dismiss key closes it.
 *
 * Everything here is pure or a plain variable so `node --test` proves it.
 */

export type FocusAction = 'focus' | 'refocus' | 'blur' | 'none';

/**
 * A tap on the terminal. A mouse click on a focused terminal must do
 * nothing: a blur would send focus-out/focus-in (mode 1004) to vim on
 * every click, and a mouse has no keyboard to open.
 */
export function tapTerminalAction(env: { readonly active: boolean; readonly touch: boolean }): FocusAction {
  if (!env.active) return 'focus';
  return env.touch ? 'refocus' : 'none';
}

/** The ⌨ cap: dismiss when the keyboard is up, otherwise bring it up. */
export function kbdCapAction(env: { readonly keyboardUp: boolean; readonly active: boolean }): FocusAction {
  if (env.keyboardUp) return 'blur';
  return env.active ? 'refocus' : 'focus';
}

export function applyFocusAction(el: HTMLElement | null, action: FocusAction): void {
  if (el === null) return;
  switch (action) {
    case 'focus':
      el.focus();
      break;
    case 'refocus':
      el.blur();
      el.focus();
      break;
    case 'blur':
      el.blur();
      break;
    case 'none':
      break;
  }
}

let up = false;

/** Whether a soft keyboard is taking screen space now, per the visual viewport. */
export function keyboardUp(): boolean {
  return up;
}

/** Written by `watchVisualViewport`'s subscriber (entry-client.tsx). */
export function setKeyboardUp(v: boolean): void {
  up = v;
}
