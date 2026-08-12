/**
 * The palette's keyboard contract, as a pure function so node can test it
 * without a DOM. While the palette is open it owns the keyboard — the part
 * the component cannot prove in a test is *where* this is attached (the
 * scrim container, so it hears keys whatever descendant holds focus); the
 * part it can prove is here: which keys act, which are trapped, and which
 * pass through to the query input.
 */

/** The slice of `KeyboardEvent` the decision needs — tests fake this. */
export type PaletteKeyEvent = {
  readonly key: string;
  readonly shiftKey: boolean;
  readonly isComposing: boolean;
  preventDefault(): void;
};

export type PaletteKeyActions = {
  move(delta: number): void;
  run(): void;
  dismiss(): void;
};

export const handlePaletteKey = (e: PaletteKeyEvent, actions: PaletteKeyActions): void => {
  if (e.isComposing) return;
  switch (e.key) {
    case 'ArrowDown':
      e.preventDefault();
      actions.move(1);
      return;
    case 'ArrowUp':
      e.preventDefault();
      actions.move(-1);
      return;
    case 'Enter':
      e.preventDefault();
      actions.run();
      return;
    case 'Escape':
      e.preventDefault();
      actions.dismiss();
      return;
    case 'Tab':
      // Trapped, both directions, and deliberately a no-op: the hidden input
      // is the dialog's only intended tab stop. Left alone, Tab walks focus
      // through the row buttons and out to the chrome behind the scrim —
      // where printable keys reach the live pty and Esc stops dismissing.
      // Row mousedown is already cancelled and a scrim click dismisses, so
      // Tab was the one remaining way for focus to leave the input. It does
      // not move the selection either: the footer advertises ↑↓ ⏎ esc, and
      // an unadvertised chord is the launcher menu's dead-chord rule inverted.
      e.preventDefault();
      return;
  }
};
