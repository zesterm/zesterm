/**
 * Composed text: what an IME commit, an emoji picker, or a dictation engine
 * hands over — whole strings that never went through per-key events.
 *
 * Encoded as plain UTF-8, deliberately **not** bracketed like a paste: a
 * composition commit is typing, and an app in bracketed-paste mode that
 * received `ESC[200~` around a single kanji would treat keystrokes as a
 * paste. Nothing is stripped either — the host emulator is the truth, the
 * same rule `encodePaste` follows.
 *
 * On a phone this is the *primary* input path, not the fallback: the phone
 * design's `❯ run…` field and every soft-keyboard commit arrive here.
 */

const UTF8 = new TextEncoder();

export function encodeComposedText(text: string): Uint8Array | null {
  if (text.length === 0) {
    return null;
  }
  return UTF8.encode(text);
}
