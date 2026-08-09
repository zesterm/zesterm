/**
 * Paste as bytes. Wrapped in the bracketed-paste markers only when the program
 * asked for them (`CSI ? 2004 h`), so a shell that knows the protocol can tell
 * a paste from typing and, e.g., not run each pasted line.
 *
 * Nothing is stripped or normalized here — the host emulator is the truth
 * about what the bytes mean, and a client that "helpfully" rewrites newlines
 * or filters control characters is a client that disagrees with the native
 * window over the same session.
 */

import { Modes } from '@zesterm/proto';

const UTF8 = new TextEncoder();
const OPEN = UTF8.encode('\x1b[200~');
const CLOSE = UTF8.encode('\x1b[201~');

export function encodePaste(text: string, modes: number): Uint8Array {
  const body = UTF8.encode(text);
  if ((modes & Modes.BRACKETED_PASTE) === 0) {
    return body;
  }
  const out = new Uint8Array(OPEN.length + body.length + CLOSE.length);
  out.set(OPEN, 0);
  out.set(body, OPEN.length);
  out.set(CLOSE, OPEN.length + body.length);
  return out;
}
