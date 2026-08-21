/**
 * The theme gallery (design §8): the five built-ins as cards, each carrying a
 * live preview in that theme's OWN colours, its normal-ANSI swatch strip, and
 * a footer naming it. Clicking a card applies the theme through the #133
 * store — CSS variables re-apply and painters rebuild — and the active ring
 * follows the store's answer, so a click that fell back (unknown id) rings
 * the theme actually applied rather than the one clicked.
 *
 * Deliberately absent: the mock's dashed "Import a scheme" card is #147's
 * open web half. The parsers exist in Rust (`zest-theme::import`) and the
 * native gallery's card is live, but this client has no TypeScript port of
 * them and — theme choice being client-kept — nowhere durable to put an
 * imported theme's *content* yet. Rendering the card before both exist
 * would be a dead target. A nav affordance to reach `/themes` (a palette
 * action, a settings row) is likewise still open; the route is this work
 * item's whole surface.
 */

import { component, onUnmounted, signal } from 'sigx';
import { builtinThemes, type Theme } from '@zesterm/theme';

import { themeCards } from '../theme-gallery-model.ts';
import { currentTheme, themeStore } from '../state/theme.ts';

export const ThemeGallery = component<{ theme: Theme }>((ctx) => {
  // The built-ins never change within a session, so the cards are computed
  // once; only the active ring is reactive.
  const cards = themeCards(builtinThemes);
  const store = themeStore();
  const state = signal({ activeId: currentTheme(ctx.props.theme).id });

  const off = store?.onThemeChange((t) => (state.activeId = t.id));
  onUnmounted(() => off?.());

  const pick = (id: string): void => {
    // No store only before boot wiring, where there is nothing to switch.
    if (store === null) return;
    store.setTheme(id);
    // onThemeChange updates activeId; nothing else to do here.
  };

  return () => (
    <div class="fleet-page">
      <header class="page-head">
        <h1>
          Themes
          <span class="page-tagline">one file styles the chrome and any TUI running inside it</span>
        </h1>
        <p class="page-lede">
          A theme is two things in one file: the chrome tokens, which are the window's alone, and
          the ANSI palette, which is the default grid scheme and the part a profile may override.
          Brights are derived in OKLCH, never hand-authored, so no bright drifts out of its family.
        </p>
      </header>

      <div class="card-grid themes">
        {cards.map((c) => (
          <button
            key={c.id}
            class={`theme-card${c.id === state.activeId ? ' active' : ''}`}
            aria-pressed={c.id === state.activeId}
            onClick={() => pick(c.id)}
          >
            {/* Inline styles are correct here and only here: these are the
                previewed theme's own colours from the theme object, not the
                page theme's — a preview painted in --zt-* previews nothing. */}
            <div class="theme-preview" style={`background:${c.ui.bg};color:${c.ui.fg}`}>
              <div>❯ zesterm --theme {c.id}</div>
              <div>
                <span style={`color:${c.ui.green}`}>ok</span> · schema 1 · 24 tokens
              </div>
              <div>
                <span style={`color:${c.ui.blue}`}>accent</span> {c.ui.accent}{'  '}
                <span style={`color:${c.ui.red}`}>danger</span> {c.ui.danger}
              </div>
            </div>
            <div class="theme-swatches">
              {c.swatches.map((s) => (
                <span style={`background:${s}`} />
              ))}
            </div>
            <div class="theme-foot">
              <span class="theme-name">{c.name}</span>
              <span class="theme-mode">{c.qualifier}</span>
              {c.id === state.activeId ? <span class="theme-active">active</span> : null}
            </div>
          </button>
        ))}
      </div>
    </div>
  );
});
