/**
 * What a keystroke is to the echo predictor, read off the key the keyboard
 * reported — never off the encoded bytes (the port of `predict_key` in
 * `zest-app`, with the same answers).
 *
 * A printable is a single code point with no Ctrl, Alt or Meta held (Shift is
 * part of the character). Everything else is `other`: the predictor flushes
 * on it, because what Enter, an arrow or a chord does is the shell's
 * business. The predictor applies its own width rule on top.
 */

import type { PredictKey } from '@zesterm/proto';

export interface PredictKeyLike {
  readonly key: string;
  readonly ctrlKey: boolean;
  readonly altKey: boolean;
  readonly metaKey: boolean;
}

export function predictKeyOf(e: PredictKeyLike): PredictKey {
  if (e.ctrlKey || e.altKey || e.metaKey) return { key: 'other' };
  if (e.key === 'Backspace') return { key: 'backspace' };
  // `KeyboardEvent.key` is the character itself for a printable and a name
  // ("Enter", "ArrowLeft", "Dead") for everything else; one code point is
  // what separates the two, and a name is never one.
  const cps = [...e.key];
  if (cps.length === 1) return { key: 'printable', ch: e.key };
  return { key: 'other' };
}

/** A composed or dictated string: each code point is typed in turn. */
export function predictKeysOfText(text: string): PredictKey[] {
  return [...text].map((ch) => ({ key: 'printable', ch }));
}
