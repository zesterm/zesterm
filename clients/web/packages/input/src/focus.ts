/**
 * Focus reporting (`CSI ? 1004 h`): `ESC [ I` on focus in, `ESC [ O` on focus
 * out, and *nothing at all* unless the program opted in — an unsolicited
 * report is bytes a shell will happily echo into the command line.
 */

import { Modes } from '@zesterm/proto';

const UTF8 = new TextEncoder();

export function encodeFocus(focused: boolean, modes: number): Uint8Array | null {
  if ((modes & Modes.FOCUS_REPORTING) === 0) {
    return null;
  }
  return UTF8.encode(focused ? '\x1b[I' : '\x1b[O');
}
