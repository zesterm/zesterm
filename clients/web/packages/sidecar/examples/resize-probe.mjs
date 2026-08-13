/**
 * What the daemon actually sends across a resize, at the wire.
 *
 * `pnpm -C clients/web/packages/sidecar probe:resize`
 *
 * The counterpart to `zest-pty`'s `pty_dump --resize`, one layer up: that one
 * answers "what does ConPTY emit", this one answers "what does the host's block
 * index look like afterwards". Between them a resize bug can be placed without
 * guessing from a rendered pane.
 *
 * Drives one session over the loopback pipe -- which skips the trust store
 * (`Auth::Transport`), so no pairing -- runs commands, resizes the way a
 * browser pane does, and dumps each block's line range against the rows it is
 * holding, before and after.
 *
 * The question it answers: after a resize, do the blocks the host sends still
 * name rows that exist and hold their own output? A block with `rows=0`, or one
 * whose `nonBlank` count has halved, is #200 -- the anchors and the content
 * having diverged. Needs a daemon running for this user.
 */

import { generateIdentity, seedSigner } from '@zesterm/auth';
import { ConnectionClient, SessionClient } from '@zesterm/client';
import { sliceBlocks, expandRow, rowText } from '@zesterm/proto';
import { socketDial, defaultSocketPath } from '../src/net-link.ts';

const dial = socketDial(defaultSocketPath());
const signer = seedSigner(generateIdentity());
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

function dump(label, client) {
  const g = client.grid;
  const { preamble, slices, tail } = sliceBlocks(g);
  const text = (r) => rowText(expandRow(r, g.cols, g.attrs)).trimEnd();
  console.log(`\n=== ${label} ===  cols=${g.cols} viewport=${g.rows.length} scrollback=${g.scrollback.length}`);
  console.log(`preamble=${preamble.length} tail=${tail.length} blocks=${g.blocks.length}`);
  for (const s of slices) {
    const b = s.block;
    const rows = [...s.promptRows, ...s.outputRows];
    const nb = rows.filter((r) => text(r) !== '');
    console.log(
      `  block ${b.id} ${JSON.stringify(b.command)} ${b.state.state} ` +
        `lines[${b.prompt_line}..${b.output_line ?? '-'}..${b.end_line ?? 'open'}] ` +
        `rows=${rows.length} nonBlank=${nb.length}` +
        (nb.length ? ` first=${JSON.stringify(text(nb[0]).slice(0, 26))}` : ''),
    );
  }
  const ids = [...g.scrollback, ...g.rows].map((r) => r.line);
  console.log(`  rows span lines ${ids[0]}..${ids[ids.length - 1]}`);
}

// 1. Create a session of our own so nothing else is disturbed.
const addr = await new Promise((resolve, reject) => {
  const conn = new ConnectionClient({
    dial,
    signer,
    label: 'resize-probe',
    events: {
      onConnection: (s) => {
        if (s.phase === 'connected') conn.createSession({ command: '', cwd: '', cols: 80, rows: 24 });
        if (s.phase === 'failed') reject(new Error(s.message));
      },
      onSessions: (sessions, created) => {
        if (created === null) return;
        const mine = sessions.find((s) => String(s.addr.session) === String(created)) ?? sessions[sessions.length - 1];
        conn.close();
        mine ? resolve({ host: mine.addr.host, session: BigInt(String(mine.addr.session)) }) : reject(new Error('not listed'));
      },
    },
  });
  conn.connect();
});
console.log(`session ${addr.host.slice(0, 8)}:${addr.session}`);

// 2. Attach and drive it.
const client = new SessionClient({
  dial,
  signer,
  label: 'resize-probe',
  session: addr,
  cols: 80,
  rows: 24,
});
client.connect();
await wait(1500);

const type = async (s) => {
  client.input(new TextEncoder().encode(s));
  await wait(1200);
};

await type('ls\r');
await type('ls\r');
dump('before the resize', client);

// 3. A drag: many resizes in quick succession, both dimensions moving.
//    `ResizeObserver` fires throughout a real drag, so this is the gesture and
//    not an exaggeration of it.
for (const [c, r] of [[60, 20], [40, 18], [30, 14], [40, 18], [60, 20], [80, 24]]) {
  client.resize(c, r);
  await wait(120);
}
await wait(3000);
dump('after a drag back to 80x24', client);

const g = client.grid;
const blank = [...g.scrollback, ...g.rows].filter(
  (row) => rowText(expandRow(row, g.cols, g.attrs)).trim() === '',
).length;
console.log(`
blank rows held: ${blank} of ${g.scrollback.length + g.rows.length}`);
console.log('--- the viewport, as the client holds it ---');
for (const row of g.rows) {
  const t = rowText(expandRow(row, g.cols, g.attrs)).trimEnd();
  console.log(`  ${String(row.line).padStart(3)} |${t}|`);
}

client.close();
process.exit(0);
