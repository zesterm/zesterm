import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  builtinThemes,
  resolveTerminalPalette,
  DEFAULT_DARK,
  DEFAULT_LIGHT,
} from '@zesterm/theme';

import { themeCards } from '../src/theme-gallery-model.ts';

const cards = themeCards(builtinThemes);

test('one card per built-in, nothing more — the import card is #147', () => {
  assert.equal(cards.length, builtinThemes.length, 'five themes, five cards');
  assert.deepEqual(
    cards.map((c) => c.id),
    builtinThemes.map((t) => t.id),
    "builtin.rs's IDS order is the gallery's order",
  );
});

test('every swatch strip is that theme’s normal ANSI row from resolveTerminalPalette', () => {
  for (const [i, card] of cards.entries()) {
    const theme = builtinThemes[i]!;
    assert.deepEqual(
      card.swatches,
      resolveTerminalPalette(theme.ui).ansi.slice(0, 8),
      `${card.id}: the strip is read from the palette derivation in index order, never re-typed`,
    );
    assert.equal(card.swatches.length, 8, 'brights are derived and never shown (§8)');
  }
});

test('the qualifier comes from the theme’s mode, with the defaults saying so', () => {
  for (const card of cards) {
    const theme = builtinThemes.find((t) => t.id === card.id)!;
    if (card.id === DEFAULT_DARK || card.id === DEFAULT_LIGHT) {
      assert.equal(
        card.qualifier,
        `${theme.mode} · default`,
        'the two defaults are named — the footer is where a user learns which theme is the fallback',
      );
    } else {
      assert.equal(card.qualifier, theme.mode, 'plain mode for everything else');
    }
  }
  assert.ok(
    cards.some((c) => c.qualifier.startsWith('light')),
    'paper keeps the gallery honest that light themes exist',
  );
});

test('the preview carries the theme’s OWN tokens, not the page theme’s', () => {
  for (const [i, card] of cards.entries()) {
    assert.equal(
      card.ui,
      builtinThemes[i]!.ui,
      'inline preview colours must come from the previewed theme object — a preview painted in the page theme previews nothing',
    );
  }
});
