/**
 * Turning browser keyboard events into terminal byte sequences.
 *
 * A faithful port of `crates/zest-input` for the web client. In the fleet,
 * encoding happens **at the keyboard** — the protocol carries already-encoded
 * bytes — because modifier and keymap conventions belong to the platform that
 * produced the keystroke, not to the platform the shell happens to run on. So
 * this must agree byte-for-byte with the Rust encoder, or the same session
 * types differently depending on which window it is viewed from.
 *
 * # The thing that is easy to get backwards
 *
 * **Modes change the encoding.** Arrow keys are `ESC [ A` normally and
 * `ESC O A` under DECCKM. Getting that wrong breaks vim and readline in a way
 * that reads as the terminal randomly ignoring the keyboard, which sends people
 * looking at the wrong layer entirely.
 *
 * # Deferred, on purpose
 *
 * Mouse encoding and IME composition are not ported yet, and there are no stub
 * functions to reach for by accident — a stub that returns bytes is worse than
 * an import error. They land with the app shell, which is where pointer
 * geometry and composition events actually live.
 */

export { type Mods, NO_MODS, mods, modsOf } from './mods.ts';
export { type KeyLike, encodeKey } from './key.ts';
export { belongsToDesktop, isClipboardChord, belongsToBrowser } from './desktop.ts';
export { encodePaste } from './paste.ts';
export { encodeFocus } from './focus.ts';
