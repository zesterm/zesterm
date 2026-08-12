/**
 * The command palette's client-side state: open, query, selection. The result
 * list itself is server state (blocks and sessions arrive over the control
 * plane); only the cursor over it lives here.
 */

export interface PaletteState {
  readonly open: boolean;
  readonly query: string;
  readonly selection: number;
}

export const PALETTE_CLOSED: PaletteState = { open: false, query: '', selection: 0 };

/**
 * Opening starts fresh — a palette that reopens onto last week's query makes
 * ⌘K + type land the keystrokes in the middle of stale text.
 */
export function openPalette(): PaletteState {
  return { open: true, query: '', selection: 0 };
}

export function closePalette(state: PaletteState): PaletteState {
  return { ...state, open: false };
}

/** Typing resets the selection: the old index points into results that no longer exist. */
export function setQuery(state: PaletteState, query: string): PaletteState {
  return { ...state, query, selection: 0 };
}

/**
 * Move the selection with wrap-around: ↓ past the last result lands on the
 * first and ↑ from the first lands on the last, which is what makes the last
 * result one keystroke away instead of a whole list of them.
 */
export function moveSelection(
  state: PaletteState,
  delta: number,
  resultCount: number,
): PaletteState {
  if (resultCount <= 0) {
    return state.selection === 0 ? state : { ...state, selection: 0 };
  }
  const selection = (((state.selection + delta) % resultCount) + resultCount) % resultCount;
  return { ...state, selection };
}
