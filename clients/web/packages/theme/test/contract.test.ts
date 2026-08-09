/**
 * The contract with `@sigx/terminal-zero`: `{...theme.ui, name, mode}` must be
 * a valid argument to sigx's `registerTheme()`. tokens.rs pins the same thing
 * on the Rust side with `deny_unknown_fields`; this file pins it here.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TOKEN_KEYS, cssVarsOf, obsidian, toSigxTheme } from '../src/index.ts';
import type { SigxTheme } from '../src/index.ts';

// Hand-copied from @sigx/terminal-zero's Theme interface
// (~/dev/sigx/terminal/main/packages/terminal-zero/src/contract.ts), in its
// declaration order. If sigx adds or renames a token, updating this list makes
// the assertions below fail loudly instead of the two vocabularies silently
// diverging.
const SIGX_CONTRACT_KEYS = [
  'name',
  'mode',
  // structure
  'bg',
  'panel',
  'chrome',
  'line',
  'fg',
  'dim',
  'faint',
  'shadow',
  // accent
  'accent',
  'accentSoft',
  'accentText',
  'selSoft',
  // status
  'success',
  'warn',
  'danger',
  'info',
  // ANSI core
  'black',
  'red',
  'green',
  'yellow',
  'blue',
  'magenta',
  'cyan',
  'white',
] as const;

const SIGX_TOKEN_KEYS = SIGX_CONTRACT_KEYS.filter((k) => k !== 'name' && k !== 'mode');

// The type-level half of the contract: a flat replica of terminal-zero's
// `Theme` (string-valued color tokens, name, mode) must be mutually
// assignable with `SigxTheme`, with identical key sets. A missing, extra, or
// renamed key on either side fails `tsc` before any test runs.
type ContractTheme = { [K in (typeof SIGX_TOKEN_KEYS)[number]]: string } & {
  name: string;
  mode: 'dark' | 'light';
};
type Equals<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
const _valuesMatch: Equals<SigxTheme, ContractTheme> = true;
const _keysMatch: Equals<keyof SigxTheme, keyof ContractTheme> = true;
void _valuesMatch;
void _keysMatch;

test('UiTokens carries exactly terminal-zero\'s 24 tokens, key for key', () => {
  assert.equal(
    TOKEN_KEYS.length,
    24,
    'the sigx vocabulary is 24 tokens; a different count means the contract moved',
  );
  assert.deepEqual(
    [...TOKEN_KEYS].sort(),
    [...SIGX_TOKEN_KEYS].sort(),
    'zest-theme and terminal-zero must agree on the token names, or a spread into registerTheme() silently drops or invents colors',
  );
});

test('cssVarsOf emits one variable per token, no more, no fewer', () => {
  const vars = cssVarsOf(obsidian.ui);
  assert.equal(
    Object.keys(vars).length,
    24,
    'a missing var is an unstyled surface; an extra one is a token that does not exist',
  );
});

test('toSigxTheme produces name, mode, and the 24 tokens — nothing else', () => {
  const sigx = toSigxTheme(obsidian);
  assert.deepEqual(
    Object.keys(sigx).sort(),
    [...SIGX_CONTRACT_KEYS].sort(),
    'registerTheme() must receive exactly the contract record; extra keys would rely on sigx ignoring them',
  );
  assert.equal(sigx.name, 'Obsidian');
  assert.equal(sigx.mode, 'dark');
  assert.equal(
    sigx.accentSoft,
    obsidian.ui.accentSoft,
    'token values must pass through untouched',
  );
});
