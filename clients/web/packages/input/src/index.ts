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
 * Mouse encoding is not ported yet, and there is no stub function to reach
 * for by accident — a stub that returns bytes is worse than an import error.
 * It lands when the app grows selection, which is where pointer geometry
 * lives. Composition is split in two: the *events* belong to the app (only a
 * DOM sees `compositionend`), and the *bytes* are `encodeComposedText` here —
 * found the hard way, when the first live browser run typed an emoji and the
 * shell received nothing.
 */

export { type Mods, NO_MODS, mods, modsOf } from './mods.ts';
export { type KeyLike, encodeKey } from './key.ts';
export { belongsToDesktop, isClipboardChord, belongsToBrowser } from './desktop.ts';
export { encodePaste } from './paste.ts';
export { encodeComposedText } from './text.ts';
export { encodeFocus } from './focus.ts';
