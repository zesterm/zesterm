/**
 * The blocks pane's render model, proven on the corpus and on a synthetic
 * three-state session.
 *
 * `blocks-zsh` is the real recording — finished blocks (exit 0 and exit 1), a
 * zero-output block and an open running block — so the header fields are
 * checked against what a real shell produced, not against hand-built rows.
 * The synthetic scenario adds what the recording cannot: timestamps (the
 * corpus has none, so duration formatting would otherwise go untested) and a
 * pinned `nowMs` for the running counter.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  FrameReader,
  GridView,
  decode,
  hexToBytes,
  isKeyframe,
  isUpdate,
  parseHostMessage,
  sliceBlocks,
  type BlockPayload,
  type BlockState,
  type RowPayload,
} from '@zesterm/proto';

import {
  atShellPrompt,
  copyOutputText,
  followsOutput,
  formatDuration,
  isInterrupted,
  linkOf,
  mostRecentBlockWithOutput,
  optionOf,
  paneModel,
  promptChips,
  type HeaderItem,
  type RenderItem,
} from '../src/blocks-pane-model.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURES_DIR = join(HERE, '../../../../../crates/zest-proto/fixtures');

/** Replay a recording exactly as `@zesterm/proto`'s own suite does. */
function replay(name: string): GridView {
  const fixture = JSON.parse(readFileSync(join(FIXTURES_DIR, `${name}.json`), 'utf8')) as {
    frames: Array<{ wire: string }>;
  };
  const view = new GridView();
  for (const frame of fixture.frames) {
    const reader = new FrameReader();
    reader.feed(hexToBytes(frame.wire));
    const body = reader.next();
    assert.ok(body, 'every fixture frame is a complete frame');
    const msg = parseHostMessage(decode(body));
    if (isKeyframe(msg)) view.applyKeyframe(msg);
    else if (isUpdate(msg)) view.applyDelta(msg.delta);
    else assert.fail(`unexpected message ${msg.t} in a recording`);
  }
  return view;
}

function headers(items: readonly RenderItem[]): HeaderItem[] {
  return items.filter((i): i is HeaderItem => i.kind === 'header');
}

function headerOf(items: readonly RenderItem[], blockId: number): HeaderItem {
  const found = headers(items).find((h) => h.blockId === blockId);
  assert.ok(found, `block ${blockId} must render a header`);
  return found;
}

// --- synthetic three-state session -----------------------------------------

function synthRow(line: number, text: string): RowPayload {
  return {
    line,
    runs: [{ attr: 0, cells: [...text].length, text, marks: [] }],
    wrapped: false,
  };
}

function synthBlock(
  id: number,
  prompt: number,
  output: number | null,
  end: number | null,
  state: BlockState,
  command: string,
  times?: { started: number; ended?: number },
): BlockPayload {
  return {
    id,
    prompt_line: prompt,
    output_line: output,
    end_line: end,
    state,
    command,
    cwd: '~/dev',
    ...(times === undefined ? {} : { started_ms: times.started }),
    ...(times?.ended === undefined ? {} : { ended_ms: times.ended }),
  };
}

const CURSOR = { row: 0, col: 0, visible: true, shape: 0 } as const;
const NOW = 100_000;

/**
 * exit 0 one-liner, a foldable three-line block, and a running command:
 * the mock's three states, with timestamps the recording lacks.
 */
function threeStateView() {
  return {
    scrollback: [],
    rows: [
      synthRow(0, '❯ echo hi'),
      synthRow(1, 'hi'),
      synthRow(2, '❯ ls -la'),
      synthRow(3, 'total 3'),
      synthRow(4, 'a.txt'),
      synthRow(5, 'b.txt'),
      synthRow(6, '❯ sleep 99'),
      synthRow(7, 'tick'),
      synthRow(8, 'tock'),
    ],
    blocks: [
      synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, 'echo hi', {
        started: 1000,
        ended: 1410,
      }),
      synthBlock(1, 2, 3, 5, { state: 'finished', exit_code: 0 }, 'ls -la', {
        started: 0,
        ended: 51_200,
      }),
      synthBlock(2, 6, 7, null, { state: 'running' }, 'sleep 99', { started: NOW - 4200 }),
    ],
    cursor: CURSOR,
    attrs: new Map(),
  };
}

test('the three-state scenario renders exact header fields per the spec', () => {
  const items = paneModel(threeStateView(), new Set(), 'live', NOW);

  const h0 = headerOf(items, 0);
  assert.deepEqual(
    {
      railToken: h0.railToken,
      foldable: h0.foldable,
      folded: h0.folded,
      chevron: h0.chevron,
      command: h0.command,
      cwd: h0.cwd,
      durationText: h0.durationText,
      outcome: h0.outcome,
      foldedLineCount: h0.foldedLineCount,
    },
    {
      railToken: 'success',
      foldable: true,
      folded: false,
      chevron: '▾',
      command: 'echo hi',
      cwd: '~/dev',
      durationText: '0.41s',
      outcome: { kind: 'exit', code: 0 },
      foldedLineCount: 1,
    },
    'a finished exit-0 block is the design\'s success header, field for field',
  );

  const h1 = headerOf(items, 1);
  assert.equal(h1.durationText, '51.2s', 'sub-minute durations carry one decimal');
  assert.equal(h1.foldedLineCount, 3, 'the fold count is the rows the unfold will reveal');
  assert.ok(h1.foldable, 'a block with output must offer its chevron');

  const h2 = headerOf(items, 2);
  assert.equal(h2.railToken, 'warn', 'a running block rails warn');
  assert.deepEqual(
    h2.outcome,
    { kind: 'running', seconds: 4.2 },
    'the running counter is (now - started_ms), exact under a pinned clock',
  );
  assert.equal(h2.durationText, '', 'a running block has no finished duration');
  assert.equal(h2.command, 'sleep 99');

  // Output follows each unfolded header, and the open block's reaches bottom.
  const out2 = items.find(
    (i): i is Extract<RenderItem, { kind: 'output' }> => i.kind === 'output' && i.blockId === 2,
  );
  assert.ok(out2, 'the running block renders its output');
  assert.deepEqual(
    out2.rows.map((r) => r.line),
    [7, 8],
    'the open block renders to the bottom while the command runs',
  );
});

test('folding drops the output item and the header gains the count', () => {
  const items = paneModel(threeStateView(), new Set([1]), 'live', NOW);

  const h1 = headerOf(items, 1);
  assert.ok(h1.folded, 'the folded set folds its block');
  assert.equal(h1.chevron, '▸', 'a folded header points its chevron right');
  assert.equal(h1.foldedLineCount, 3, "the folded header says '3 lines'");
  assert.ok(
    !items.some((i) => i.kind === 'output' && i.blockId === 1),
    'a folded block renders header-only — its rows must not take layout space',
  );
  assert.ok(
    items.some((i) => i.kind === 'output' && i.blockId === 0),
    'folding one block must not fold its neighbours',
  );
});

test('a block with no output is not foldable, folded set or not', () => {
  const view = {
    scrollback: [],
    rows: [synthRow(0, '❯ true')],
    blocks: [synthBlock(0, 0, 1, 0, { state: 'finished', exit_code: 0 }, 'true')],
    cursor: CURSOR,
    attrs: new Map(),
  };
  // Folded by id anyway — a stale fold surviving from before a clear must not
  // conjure a chevron onto a silent command.
  const h = headerOf(paneModel(view, new Set([0]), 'live', NOW), 0);
  assert.ok(!h.foldable, 'nothing to fold when the command printed nothing');
  assert.ok(!h.folded, 'a non-foldable block can never report itself folded');
  assert.equal(h.chevron, null, 'no chevron means no fold affordance is drawn');
});

test("exit_code null renders 'exit ?' semantics: unknown outcome, never a green rail", () => {
  const view = {
    scrollback: [],
    rows: [synthRow(0, '❯ mystery'), synthRow(1, 'out')],
    blocks: [synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: null }, 'mystery')],
    cursor: CURSOR,
    attrs: new Map(),
  };
  const h = headerOf(paneModel(view, new Set(), 'live', NOW), 0);
  assert.deepEqual(h.outcome, { kind: 'exit', code: null }, 'the missing status stays missing');
  assert.equal(
    h.railToken,
    'faint',
    'a shell that omitted the status is not a shell that succeeded — success here lies',
  );
});

test('the interrupted predicate: open and running while the link reconnects', () => {
  const running = synthBlock(2, 6, 7, null, { state: 'running' }, 'sleep 99');
  const finished = synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, 'echo hi');
  const prompt = synthBlock(3, 9, null, null, { state: 'prompt' }, '');

  assert.ok(isInterrupted(running, 'reconnecting'), 'the host went away mid-command');
  assert.ok(!isInterrupted(running, 'live'), 'a healthy link interrupts nothing');
  assert.ok(!isInterrupted(finished, 'reconnecting'), 'a finished block kept its real outcome');
  assert.ok(
    !isInterrupted(prompt, 'reconnecting'),
    'an idle prompt has no command to interrupt — reporting one invents a failure',
  );
});

test('reconnecting turns the running header interrupted and appends the overlay', () => {
  const items = paneModel(threeStateView(), new Set(), 'reconnecting', NOW);

  const h2 = headerOf(items, 2);
  assert.deepEqual(h2.outcome, { kind: 'interrupted' });
  assert.equal(h2.railToken, 'faint', '§4: an interrupted block rails faint, not warn');
  assert.deepEqual(
    headerOf(items, 0).outcome,
    { kind: 'exit', code: 0 },
    'finished blocks keep their verdicts — the link cannot rewrite history',
  );

  const last = items[items.length - 1];
  assert.deepEqual(
    last,
    { kind: 'overlay', link: 'reconnecting', text: 'connection lost — reconnecting' },
    'the degraded overlay is the last thing in the pane',
  );
});

test("a stalled link says 'buffering' and interrupts nothing", () => {
  const items = paneModel(threeStateView(), new Set(), 'stalled', NOW);
  const last = items[items.length - 1];
  assert.deepEqual(last, { kind: 'overlay', link: 'stalled', text: 'buffering' });
  assert.equal(
    headerOf(items, 2).outcome.kind,
    'running',
    'a stall is late data, not a lost host — the block is still running',
  );
});

test('a trailing prompt-state block renders as the prompt line, not a header', () => {
  const view = {
    scrollback: [],
    rows: [synthRow(0, '❯ echo hi'), synthRow(1, 'hi'), synthRow(2, '❯ ')],
    blocks: [
      synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, 'echo hi'),
      synthBlock(1, 2, null, null, { state: 'prompt' }, ''),
    ],
    cursor: CURSOR,
    attrs: new Map(),
  };
  const items = paneModel(view, new Set(), 'live', NOW);
  assert.ok(
    !headers(items).some((h) => h.blockId === 1),
    "the shell's live prompt is not a command — a header would say '·' forever",
  );
  const prompt = items.find(
    (i): i is Extract<RenderItem, { kind: 'prompt' }> => i.kind === 'prompt',
  );
  assert.ok(prompt, 'the prompt line renders as its own item');
  assert.deepEqual(
    prompt.rows.map((r) => r.line),
    [2],
    "the prompt item carries the shell's own prompt row, where the caret goes",
  );
});

test('mostRecentBlockWithOutput skips a last block that printed nothing', () => {
  const view = {
    scrollback: [],
    rows: [
      synthRow(0, '❯ echo hi'),
      synthRow(1, 'hi'),
      synthRow(2, '❯ true'),
    ],
    blocks: [
      synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, 'echo hi'),
      // `true` prints nothing: end_line before output_line, the wire's shape
      // for a silent command.
      synthBlock(1, 2, 3, 2, { state: 'finished', exit_code: 0 }, 'true'),
    ],
    cursor: CURSOR,
    attrs: new Map(),
  };
  const target = mostRecentBlockWithOutput(sliceBlocks(view));
  assert.ok(target, 'a session with output has a target');
  assert.equal(
    target.block.id,
    0,
    '⌘⇧O/⌘⇧R target the most recent block WITH output — copying the silent one copies nothing',
  );

  assert.equal(
    mostRecentBlockWithOutput(sliceBlocks({ ...view, blocks: [] })),
    null,
    'no blocks, no target — the chords must be able to decline',
  );
});

test('atShellPrompt: re-run may type only into a shell that is reading', () => {
  const finished = synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, 'echo hi');
  const running = synthBlock(1, 2, 3, null, { state: 'running' }, 'sleep 99');
  const prompt = synthBlock(2, 4, null, null, { state: 'prompt' }, '');

  assert.ok(
    atShellPrompt([finished, prompt]),
    'a trailing open prompt is the one state where typed bytes reach the shell',
  );
  assert.ok(
    !atShellPrompt([finished, running]),
    "⌘⇧R during a running command would land in that command's stdin, not the shell's",
  );
  assert.ok(!atShellPrompt([]), 'no shell integration means no prompt to trust — decline');
  assert.ok(
    !atShellPrompt([finished]),
    'a finished trailing block (D received, next A not yet) is mid-transition, not a prompt',
  );
});

test('copyOutputText rejoins soft-wrapped rows instead of inserting newlines', () => {
  // One printed line that wrapped across two grid rows, then a real second
  // line. `wrapped` exists on the wire precisely so this copy has no
  // spurious newline (delta.rs, RowPayload.wrapped).
  const rows: RowPayload[] = [
    { ...synthRow(0, 'a long line that '), wrapped: true },
    synthRow(1, 'kept going'),
    synthRow(2, 'second line'),
  ];
  assert.equal(
    copyOutputText(rows, new Map()),
    'a long line that kept going\nsecond line',
    'a wrapped row continues onto the next — a newline there is text the command never printed',
  );
  assert.equal(
    copyOutputText([synthRow(0, 'only')], new Map()),
    'only',
    'a single unwrapped row copies as itself, with no trailing newline',
  );
  assert.equal(
    copyOutputText(
      [{ ...synthRow(0, 'cut '), wrapped: true }, synthRow(1, 'short')],
      new Map(),
    ),
    'cut short',
    'the join must preserve the wrapped row verbatim — trimming would eat a real space',
  );
});

test('linkOf: a link the client gave up on stays degraded, never live', () => {
  assert.equal(
    linkOf({ phase: 'failed', reason: 'denied', message: 'no' }),
    'reconnecting',
    "§4's degraded rendering must persist at the moment the client stops redialling — " +
      "anything less flips the interrupted block back to a 'running' counter on a dead link",
  );
  assert.equal(
    linkOf({ phase: 'reconnecting', attempt: 3 }),
    'reconnecting',
    'a redialling link is the degraded state §4 draws',
  );
  assert.equal(linkOf({ phase: 'connected' }), 'live', 'a healthy link degrades nothing');
  assert.equal(
    linkOf({ phase: 'connecting' }),
    'stalled',
    'the same mapping feeds the tab chip, which must not claim live before the link exists',
  );
  assert.equal(
    linkOf({ phase: 'awaiting-approval', code: '12-34' }),
    'stalled',
    'waiting on the host to approve is not a live link either',
  );
});

test('followsOutput: at (or near) the bottom follows, scrolled away does not', () => {
  assert.ok(followsOutput(900, 100, 1000), 'exactly at the bottom keeps following new output');
  assert.ok(
    followsOutput(897, 100, 1000),
    'sub-row rounding slack still follows — browser zoom leaves fractional-pixel gaps',
  );
  assert.ok(
    !followsOutput(500, 100, 1000),
    'reading history must not be yanked back down by every delta',
  );
  assert.ok(
    followsOutput(0, 200, 150),
    'content shorter than the viewport always follows — there is nowhere to scroll away to',
  );
});

test('duration formatting: the three design shapes and the carry at a whole minute', () => {
  const table: Array<[number, string]> = [
    [410, '0.41s'], // the mock's fast block
    [0, '0.00s'],
    [9_990, '9.99s'], // two decimals up to 10s
    [10_000, '10.0s'], // one decimal from there
    [51_200, '51.2s'], // the mock's build
    [59_940, '59.9s'],
    [60_000, '1m 0s'],
    [252_000, '4m 12s'], // the mock's long form
    [299_500, '5m 0s'], // 4m 59.5s must not read '4m 60s'
  ];
  for (const [ms, want] of table) {
    assert.equal(formatDuration(ms), want, `${ms}ms formats as ${want}`);
  }
});

test('recomputation reuses span arrays for rows the delta never touched', () => {
  // The pane recomputes its model per animation frame while a command prints;
  // if every row re-coalesced its spans each frame, a long session would pay
  // O(scrollback) per output line. Row payloads are replaced-on-change by
  // GridView, so identical references across recomputes must yield identical
  // span arrays — that is what keeps the recompute O(changed rows).
  const view = threeStateView();
  const first = paneModel(view, new Set(), 'live', NOW);
  const second = paneModel(view, new Set(), 'live', NOW);
  const outOf = (items: readonly RenderItem[], id: number) =>
    items.find(
      (i): i is Extract<RenderItem, { kind: 'output' }> => i.kind === 'output' && i.blockId === id,
    );
  const a = outOf(first, 0);
  const b = outOf(second, 0);
  assert.ok(a && b, 'block 0 renders output in both recomputes');
  assert.equal(
    a.rows[0]?.spans,
    b.rows[0]?.spans,
    'an untouched row must hand back the same span array, or every frame repays the whole session',
  );
});

// --- the corpus ------------------------------------------------------------

test('paneModel over blocks-zsh: real headers for a real session', () => {
  const view = replay('blocks-zsh');
  const items = paneModel(view, new Set(), 'live', NOW);
  const layout = sliceBlocks(view);

  const echo = layout.slices.find((s) => s.block.command === 'echo hello');
  assert.ok(echo, 'the recording ran an echo');
  const hEcho = headerOf(items, echo.block.id);
  assert.equal(hEcho.railToken, 'success', 'exit 0 rails success');
  assert.deepEqual(hEcho.outcome, { kind: 'exit', code: 0 });
  assert.equal(hEcho.cwd, '/tmp/zestdemo', "the header shows the block's own cwd");
  assert.ok(hEcho.foldable, 'echo printed, so it folds');
  assert.equal(hEcho.foldedLineCount, 1);
  assert.equal(
    hEcho.durationText,
    '',
    'the recording carries no timestamps — inventing a duration would be a lie',
  );

  const silent = layout.slices.find((s) => s.block.command === 'false');
  assert.ok(silent, 'the recording ran false');
  const hSilent = headerOf(items, silent.block.id);
  assert.equal(hSilent.railToken, 'danger', 'a non-zero exit rails danger');
  assert.deepEqual(hSilent.outcome, { kind: 'exit', code: 1 });
  assert.ok(!hSilent.foldable, 'false printed nothing, so no chevron');
  assert.ok(
    !items.some((i) => i.kind === 'output' && i.blockId === silent.block.id),
    'a zero-output block renders no body at all',
  );

  const open = layout.slices.find((s) => s.open);
  assert.ok(open, 'the recording ends mid-command');
  const hOpen = headerOf(items, open.block.id);
  assert.equal(hOpen.railToken, 'warn', 'the open block is still running');
  assert.deepEqual(
    hOpen.outcome,
    { kind: 'running', seconds: null },
    'no started_ms means no counter — the ring alone says running',
  );

  // Fold echo against the real rows: the count must match what unfolding shows.
  const folded = paneModel(view, new Set([echo.block.id]), 'live', NOW);
  const hFolded = headerOf(folded, echo.block.id);
  assert.ok(hFolded.folded);
  assert.equal(hFolded.foldedLineCount, 1, "the folded echo says '1 lines'");
  assert.ok(!folded.some((i) => i.kind === 'output' && i.blockId === echo.block.id));
});

test('paneModel over blocks-zsh while reconnecting: the open block reads interrupted', () => {
  const view = replay('blocks-zsh');
  const items = paneModel(view, new Set(), 'reconnecting', NOW);
  const open = sliceBlocks(view).slices.find((s) => s.open);
  assert.ok(open, 'the recording ends mid-command');
  const h = headerOf(items, open.block.id);
  assert.deepEqual(
    h.outcome,
    { kind: 'interrupted' },
    '§4: a block whose host went away mid-run says so instead of running forever',
  );
  assert.equal(items[items.length - 1]?.kind, 'overlay', 'and the pane carries the overlay');
});

test('an abandoned prompt renders as rows, never as a card that claims to be running', () => {
  // A prompt that ran nothing is not a command, and `headerOf` splits on
  // `finished ? exit : running` — so one rendered as a header got a '·' for
  // the command it never had, a warn rail and a running counter. On a zsh
  // session idle for an hour that read as six commands in flight. (#193)
  //
  // `zest-core` no longer produces these, and a client still meets them from
  // any host that has not been restarted. The honest rendering is the prompt's
  // own text: nothing the host sent may vanish, and nothing may be invented.
  const view = {
    scrollback: [],
    rows: [
      synthRow(0, '❯ '),
      synthRow(1, '❯ echo hi'),
      synthRow(2, 'hi'),
      synthRow(3, '❯ '),
    ],
    blocks: [
      synthBlock(0, 0, null, null, { state: 'prompt' }, ''),
      synthBlock(1, 1, 2, 2, { state: 'finished', exit_code: 0 }, 'echo hi'),
      synthBlock(2, 3, null, null, { state: 'prompt' }, ''),
    ],
    cursor: CURSOR,
    attrs: new Map(),
  };

  const items = paneModel(view, new Set(), 'live', NOW);
  assert.deepEqual(
    headers(items).map((h) => h.blockId),
    [1],
    'only the command that actually ran gets a card',
  );

  const rendered = items
    .flatMap((i) => (i.kind === 'rows' || i.kind === 'prompt' || i.kind === 'output' ? i.rows : []))
    .map((r) => r.line);
  assert.ok(rendered.includes(0), "the abandoned prompt's row is still drawn, as text");

  const trailing = items[items.length - 1];
  assert.equal(trailing?.kind, 'prompt', 'the live prompt is still the prompt line');
  assert.deepEqual(
    trailing?.kind === 'prompt' ? trailing.rows.map((r) => r.line) : null,
    [3],
    'and it holds the row the caret is on',
  );
});

test('the prompt line stops at the caret, not at the bottom of the grid', () => {
  // A terminal is a fixed number of rows and a shell has usually printed a
  // handful, so the trailing prompt slice runs on into the viewport's blank
  // tail. The view appends the caret to the LAST row it is given
  // (`BlocksPane.tsx`), so on a 60-row window the caret was drawn dozens of
  // empty lines below the prompt it belongs to — reported as "the cursor is at
  // the bottom and the actual terminal input sits higher up". Measured live at
  // 35 rows for one non-blank prompt. (#202)
  //
  // Those rows are viewport, not content. The canvas grid pays nothing for
  // them; a DOM pane makes the user scroll past them.
  const view = {
    scrollback: [],
    rows: [
      synthRow(0, '❯ '),
      synthRow(1, ''),
      synthRow(2, ''),
      synthRow(3, ''),
      synthRow(4, ''),
    ],
    blocks: [synthBlock(0, 0, null, null, { state: 'prompt' }, '')],
    // Where the caret actually is: on the prompt row, at the end of '❯ '.
    cursor: { row: 0, col: 2, visible: true, shape: 0 },
    attrs: new Map(),
  };

  const items = paneModel(view, new Set(), 'live', NOW);
  const prompt = items[items.length - 1];
  assert.equal(prompt?.kind, 'prompt');
  assert.deepEqual(
    prompt?.kind === 'prompt' ? prompt.rows.map((r) => r.line) : null,
    [0],
    'the four blank rows below the caret are viewport, not prompt',
  );
});

test('a prompt spanning several rows keeps every row up to the caret', () => {
  // Trimming at the caret must not truncate a multi-line prompt: a two-line
  // prompt with the caret on the second row keeps both, and only the blank
  // tail below goes. Trimming to the last *non-blank* row instead would be
  // wrong the moment a caret sits on a blank continuation row.
  const view = {
    scrollback: [],
    rows: [
      synthRow(0, 'user@host ~/dev'),
      synthRow(1, '❯ '),
      synthRow(2, ''),
      synthRow(3, ''),
    ],
    blocks: [synthBlock(0, 0, null, null, { state: 'prompt' }, '')],
    cursor: { row: 1, col: 2, visible: true, shape: 0 },
    attrs: new Map(),
  };

  const items = paneModel(view, new Set(), 'live', NOW);
  const prompt = items[items.length - 1];
  assert.deepEqual(
    prompt?.kind === 'prompt' ? prompt.rows.map((r) => r.line) : null,
    [0, 1],
    'both prompt rows survive; only the tail below the caret is dropped',
  );
});

test('a caret that is not on one of the prompt rows trims nothing', () => {
  // Membership, not range. This slice holds 10 and 12 while the caret sits on
  // 11 — a row the client never cached — so the caret is not in the prompt at
  // all and trimming on `line <= 11` would drop line 12 on its say-so. The
  // rule is that a caret elsewhere means "leave the prompt alone", and an id
  // that merely falls between two of our rows is exactly that case.
  const view = {
    scrollback: [],
    rows: [synthRow(10, '❯ one'), synthRow(11, 'uncached'), synthRow(12, '❯ two')],
    blocks: [synthBlock(0, 10, null, null, { state: 'prompt' }, '')],
    cursor: { row: 1, col: 0, visible: true, shape: 0 },
    attrs: new Map(),
  };

  // The slice the model sees holds 10 and 12 only — 11 is not the prompt's.
  const trimmed = paneModel(
    { ...view, rows: [synthRow(10, '❯ one'), synthRow(12, '❯ two')] },
    new Set(),
    'live',
    NOW,
  );
  const prompt = trimmed[trimmed.length - 1];
  assert.deepEqual(
    prompt?.kind === 'prompt' ? prompt.rows.map((r) => r.line) : null,
    [10, 12],
    'the caret names no row here, so nothing is dropped',
  );
});

// --- the live prompt's context chips (#420) -------------------------------

test('promptChips orders cwd, git, then facts, and skips what it does not know', () => {
  const chips = promptChips('/Users/andy/dev/zesterm', {
    git: { branch: 'main', dirty: null },
    facts: [
      { key: 'venv', value: 'ml' },
      { key: 'kube', value: 'prod-eu' },
      { key: 'telemetry_blob', value: 'x' },
    ],
  });
  assert.deepEqual(
    chips.map((c) => [c.key, c.label]),
    [
      ['cwd', '~/dev/zesterm'],
      ['git', 'main'],
      ['venv', 'venv ml'],
      ['kube', 'kube prod-eu'],
    ],
    'an unknown fact key is one chip fewer, never an error',
  );
  assert.equal(chips[0]?.value, '/Users/andy/dev/zesterm', 'a click copies the whole path');
});

test('promptChips marks dirty with a star and leaves unknown-dirty unmarked', () => {
  const dirty = promptChips('', {
    git: { branch: 'main', dirty: true },
    facts: [],
  });
  assert.equal(dirty[0]?.label, 'main*');
  const unknown = promptChips('', {
    git: { branch: 'main', dirty: null },
    facts: [],
  });
  assert.equal(
    unknown[0]?.label,
    'main',
    'no subprocess ran, so nothing may claim clean or dirty — the star only means known-dirty',
  );
});

test('promptChips shortens only the home spellings it can be sure of', () => {
  assert.equal(promptChips('/home/andy/dev', null)[0]?.label, '~/dev');
  assert.equal(
    promptChips('C:\\Users\\andy\\dev', null)[0]?.label,
    'C:\\Users\\andy\\dev',
    'another platform\'s home is unknowable from here; a wrong ~ is worse than a long path',
  );
  assert.equal(promptChips('', null).length, 0, 'no cwd, no chip — never an empty chip');
});

// --- tap to answer (#421) ----------------------------------------------------

test('optionOf reads a numbered option and nothing that merely starts with a number', () => {
  // Claude Code's question and permission prompts, as they render.
  assert.equal(optionOf('❯ 1. Yes'), '1');
  assert.equal(optionOf('  2. No, and tell Claude what to do differently'), '2');
  assert.equal(optionOf('> 3) Something else'), '3');
  assert.equal(optionOf('12. twelfth'), '12');
  // Not options: a diffstat, a date, a bare marker, a version, a long id.
  assert.equal(optionOf('1 file changed, 2 insertions(+)'), null);
  assert.equal(optionOf('2024-01-01 build ok'), null);
  assert.equal(optionOf('1.'), null, 'a marker with no text after it');
  assert.equal(optionOf('1.2.3'), null);
  assert.equal(optionOf('123. too many digits for a menu'), null);
  assert.equal(optionOf(''), null);
});

test('only the running block’s rows carry an option; finished output is history', () => {
  const view = {
    scrollback: [],
    rows: [
      synthRow(0, '❯ ./menu'),
      synthRow(1, '1. old choice'),
      synthRow(2, '❯ claude'),
      synthRow(3, 'Do you want to proceed?'),
      synthRow(4, '❯ 1. Yes'),
      synthRow(5, '  2. No'),
    ],
    blocks: [
      synthBlock(0, 0, 1, 1, { state: 'finished', exit_code: 0 }, './menu'),
      synthBlock(1, 2, 3, null, { state: 'running' }, 'claude'),
    ],
    cursor: CURSOR,
    attrs: new Map(),
  };
  const items = paneModel(view, new Set(), 'live', NOW);
  const outputs = items.filter(
    (i): i is Extract<RenderItem, { kind: 'output' }> => i.kind === 'output',
  );
  assert.equal(outputs.length, 2);
  const [old, live] = outputs as [(typeof outputs)[number], (typeof outputs)[number]];
  assert.deepEqual(
    old.rows.map((r) => r.option),
    [null],
    'a finished block’s "1. …" is output that was, and a tap there must type nothing',
  );
  assert.deepEqual(
    live.rows.map((r) => r.option),
    [null, '1', '2'],
    'the running command is showing a question; its numbered rows answer it',
  );
});

test('the prompt line and a prompt-state block never offer options', () => {
  const view = {
    scrollback: [],
    rows: [synthRow(0, '1. looks like an option but is the prompt')],
    blocks: [synthBlock(0, 0, null, null, { state: 'prompt' }, '')],
    cursor: CURSOR,
    attrs: new Map(),
  };
  const items = paneModel(view, new Set(), 'live', NOW);
  for (const item of items) {
    if (item.kind === 'prompt' || item.kind === 'rows') {
      assert.ok(
        item.rows.every((r) => r.option === null),
        'nothing is running, so nothing can be answered',
      );
    }
  }
});

// --- per-block context (#429) ---------------------------------------------

test('a block with a stamped context shows its branch, and one without shows nothing', () => {
  const view = threeStateView();
  const stamped = {
    ...view,
    blocks: [
      { ...view.blocks[0]!, context: { branch: 'release/1.2', venv: 'ml', kube: '' } },
      view.blocks[1]!,
      view.blocks[2]!,
    ],
  };
  const items = paneModel(stamped, new Set(), 'live', NOW);
  const headers = items.filter((i): i is HeaderItem => i.kind === 'header');
  assert.equal(
    headers[0]?.branch,
    'release/1.2',
    'the branch the command ran on survives into the header — that is what decides whether an old failure still matters',
  );
  assert.equal(headers[1]?.branch, '', 'no stamp is no branch, never a dash pretending');
});

test('the git chip label grows a star and a count in the order the probe answers', () => {
  const git = (dirty: boolean | null, changed: number | null) =>
    promptChips('', { git: { branch: 'main', dirty, changed }, facts: [] })[0]?.label;
  assert.equal(git(null, null), 'main', 'before the probe: the branch alone');
  assert.equal(git(true, null), 'main*', 'dirty known, count not yet');
  assert.equal(git(true, 3), 'main* ±3', 'the count joins the star');
  assert.equal(git(false, null), 'main', 'clean wears no star and no count');
});
