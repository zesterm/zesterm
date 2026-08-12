/**
 * The browser's leg, in Node — a ticket and an attach, so the relay path can be
 * exercised without a signed-in browser.
 *
 * ```
 * fake-browser  --  mint a ticket with the web Worker's signing key
 *               --(1) GET /v1/attach?host=…  Sec-WebSocket-Protocol: ticket.…
 *               <--   101, or a close code that says which refusal it was
 *               ====  bytes, if --send was given
 * ```
 *
 * It is the companion to `fake-host.mjs` and stops where that one does not:
 * **it speaks no `zest-proto`.** The frames a real client sends are
 * length-prefixed MessagePack built by `clients/web/packages/proto`, in the
 * other project and with its own conformance fixtures, and a third hand-rolled
 * encoder here would be a wire format in three places — the thing
 * `@zesterm/cloud-shared`'s ticket layout is centralised to avoid. So this tool
 * answers "does the pipe exist and carry bytes", and the questions past that
 * one belong to a real client.
 *
 * What it therefore proves, and it is the half nothing else covers: that a
 * ticket signed by the account service's key is accepted at the edge, that the
 * room woke, that the parked host was told to dial back, and that whatever the
 * daemon does with the bytes comes back as a close code with a name.
 *
 * # The ticket key is yours to supply, unlike fake-host's host key
 *
 * `fake-host.mjs` carries a fixed dev seed because the `hosts` row in D1 has to
 * name it and a random one would mean editing the database every run. This tool
 * has no such pull in either direction: the relay verifies against whatever is
 * in its `TICKET_PUBLIC_KEYS`, so the seed and the binding have to be generated
 * as a pair regardless — and a signing key that mints admission to any room, in
 * a committed file, is worth strictly less than the step it would save.
 *
 * ```sh
 * node tools/fake-browser.mjs --ticket-seed <hex> --public   # → TICKET_PUBLIC_KEYS
 * ```
 *
 * # Not a test, deliberately
 *
 * Same as `fake-host.mjs`: `pnpm -C cloud -r test` needs no workerd, no daemon
 * and no network, and that property is why the room is written against
 * `src/room/state.ts`'s narrow interfaces at all. `cloud/README.md` says how to
 * run this.
 */

import { getPublicKeyAsync, signAsync } from '@noble/ed25519';
import {
  attachTicketPayload,
  attachTicketPreimage,
  encodeAttachTicket,
  fromHex,
  hex,
  randomBytes,
  toBase64Url,
} from '@zesterm/cloud-shared';

import { ATTACH_PATH } from '../src/routes.ts';
import { CLOSE_TICKET_REFUSED, RELAY_SUBPROTOCOL } from '../src/ticket.ts';
import { CLOSE_HOST_ABSENT } from '../src/room/control.ts';
import {
  CLOSE_PIPE_DIAL_TIMEOUT,
  CLOSE_PIPE_FLOOD,
  CLOSE_PIPE_PEER_GONE,
} from '../src/room/pipe.ts';

/** An Ed25519 seed, and a `HostId`, which is a public key (ADR-006). */
const KEY_BYTES = 32;

/** `relay/src/ticket.ts`'s `JTI_BYTES`, which is the web Worker's mint's. */
const JTI_BYTES = 16;

/** `@zesterm/cloud-shared`'s `ATTACH_TICKET_TTL_MS`, and the mint's. */
const TICKET_TTL_MS = 30_000;

/** How long to wait for the answer to our own close frame before giving up. */
const CLOSE_GRACE_MS = 2000;

const DEFAULTS = {
  relay: 'http://127.0.0.1:8787',
  host: '',
  'ticket-seed': '',
  send: '',
  wait: '3000',
};

const USAGE = `fake-browser — stand in for a signed-in browser's attach

  node tools/fake-browser.mjs --ticket-seed <64 hex> --host <64 hex> [options]

  --ticket-seed <hex>  the key attach tickets are signed with — the web
                       Worker's TICKET_SIGNING_KEY
  --host <64 hex>      whose room to attach to. \`node tools/fake-host.mjs
                       --host-id\` prints the one fake-host parks
  --relay <origin>     the relay Worker            (default ${DEFAULTS.relay})
  --send <hex>         bytes to push once the pipe is up
  --wait <ms>          how long to hold the socket  (default ${DEFAULTS.wait})
  --public             print the ticket key's public half and exit
`;

/**
 * Every close code a browser can be given, named.
 *
 * Built from the modules that declare them rather than from literals, so a code
 * that moves cannot leave this tool confidently printing the old name. That
 * mattering at all is the point of the exercise: the whole reason every refusal
 * is a close code is that a browser cannot read an HTTP status, and a tool that
 * printed `4404` alone would throw away the distinction the design paid for.
 */
const CLOSE_NAMES = new Map([
  [CLOSE_TICKET_REFUSED, 'the ticket was refused — expired, or not signed by a key this relay holds'],
  [CLOSE_HOST_ABSENT, 'no host is parked in this room — is fake-host.mjs running?'],
  [CLOSE_PIPE_DIAL_TIMEOUT, 'the host was told to dial back and did not'],
  [CLOSE_PIPE_PEER_GONE, 'the other end of the pipe went away'],
  [CLOSE_PIPE_FLOOD, 'the relay cut the pipe for flooding'],
  [1000, 'a normal close'],
  [1006, 'the socket died without a close frame'],
]);

/**
 * Every line is stamped with milliseconds since the process started.
 *
 * Not decoration: the one thing this tool measures that no unit test can is
 * *when* a refusal arrives, and the two refusals it can be given arrive an
 * order of magnitude apart — the edge's is immediate, the room's waits on a
 * wake-up. A log without the elapsed time makes those look identical.
 */
const startedAt = Date.now();

function say(...parts) {
  console.error(`[fake-browser +${Date.now() - startedAt}ms]`, ...parts);
}

/** Print where it broke, then stop. Every exit from a failure comes through here. */
function die(stage, detail) {
  say(`FAILED at ${stage}: ${detail}`);
  process.exit(1);
}

function parseArgs(argv) {
  const options = { ...DEFAULTS, printPublic: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      process.stdout.write(USAGE);
      process.exit(0);
    } else if (arg === '--public') {
      options.printPublic = true;
    } else if (arg.startsWith('--') && i + 1 < argv.length) {
      const key = arg.slice(2);
      if (!(key in DEFAULTS)) die('the command line', `unknown option ${arg}`);
      options[key] = argv[(i += 1)];
    } else {
      die('the command line', `unexpected argument ${arg}`);
    }
  }
  return options;
}

/**
 * A ticket, by the layout in `@zesterm/cloud-shared` and no other.
 *
 * The claims are assembled here rather than by calling the web Worker's
 * `mintAttachTicket`, which would reach across a package boundary this project
 * keeps — but every byte that is *signed* comes from the shared package, so
 * there is no second copy of the layout. `user` and `dev` are attribution the
 * relay never authorizes on (see `AttachTicket`), so a marker is honest here
 * where a plausible-looking id would only invite someone to trust it.
 */
async function mint(seed, host, now) {
  const payload = attachTicketPayload({
    v: 1,
    jti: toBase64Url(randomBytes(JTI_BYTES)),
    aud: 'relay',
    host,
    user: 'fake-browser',
    dev: 'fake-browser',
    iat: now,
    exp: now + TICKET_TTL_MS,
  });
  return encodeAttachTicket(payload, await signAsync(attachTicketPreimage(payload), seed));
}

async function main() {
  const options = parseArgs(process.argv.slice(2));

  const seed = fromHex(options['ticket-seed'], KEY_BYTES);
  if (seed === null) {
    die('the command line', '--ticket-seed must be 64 lowercase hex characters');
  }
  if (options.printPublic) {
    process.stdout.write(`${hex(await getPublicKeyAsync(seed))}\n`);
    return;
  }
  if (fromHex(options.host, KEY_BYTES) === null) {
    die('the command line', '--host must be 64 lowercase hex characters');
  }

  const ticket = await mint(seed, options.host, Date.now());
  say(`ticket minted, ${ticket.length} characters, good for ${TICKET_TTL_MS / 1000}s`);

  const url = new URL(ATTACH_PATH, options.relay);
  url.searchParams.set('host', options.host);
  // The same rewrite `clients/web/packages/app/src/relay-dial.ts` does, and for
  // the same reason: an origin is what a person has in hand, and a scheme
  // mismatch is a `SyntaxError` from the constructor rather than a refusal.
  if (url.protocol === 'https:') url.protocol = 'wss:';
  else if (url.protocol === 'http:') url.protocol = 'ws:';

  say(`dialling ${url}`);
  // Two tokens, not one joined string: a ticket may contain characters a
  // subprotocol token may not. `ticketFromSubprotocols` is the other end.
  const ws = new WebSocket(url, [RELAY_SUBPROTOCOL, `ticket.${ticket}`]);
  ws.binaryType = 'arraybuffer';

  let bytesIn = 0;
  let bytesOut = 0;
  let opened = false;

  ws.addEventListener('open', () => {
    opened = true;
    // **Not success, and saying so is the point.** A refused ticket also
    // arrives here: the Worker answers 101 and closes the socket immediately
    // afterwards, because a browser cannot read an HTTP status and a close code
    // is the only refusal it can be told apart from the wifi dropping
    // (`index.ts`'s `closeAtTheEdge`). So a tool that printed "attached" on
    // `open` would report a working relay to anyone whose key rotation had
    // gone wrong. The verdict is at the close, below.
    say('101 — the socket is open. A refusal arrives as a close code, so wait for it.');
    const payload = fromHex(options.send, options.send.length / 2);
    if (options.send.length === 0) {
      say('nothing to send (--send <hex> pushes bytes down the pipe)');
    } else if (payload === null) {
      say('--send must be an even number of lowercase hex characters; sending nothing');
    } else {
      ws.send(payload);
      bytesOut = payload.length;
      say(`sent ${payload.length} B`);
    }
    setTimeout(() => {
      say(`held for ${options.wait} ms; hanging up`);
      ws.close();
      // The closing handshake is a round trip, and a peer that never answers it
      // leaves this process waiting for ever with nothing on screen. Reported
      // rather than merely escaped, because "the relay took my bytes and then
      // did not finish the close" is a fact about the relay and not about this
      // tool.
      setTimeout(() => {
        say('the peer never answered the close frame — hanging up regardless');
        report(null);
      }, CLOSE_GRACE_MS).unref();
    }, Number(options.wait));
  });

  ws.addEventListener('message', (event) => {
    if (typeof event.data === 'string') {
      say(`a text frame came back, which no end of this path sends: ${event.data}`);
      return;
    }
    bytesIn += event.data.byteLength;
    say(`${event.data.byteLength} B back: ${hex(new Uint8Array(event.data)).slice(0, 64)}…`);
  });

  /**
   * The one verdict, and the process's exit code.
   *
   * `code` is the close code, or `null` for a close that never completed. The
   * distinction a caller needs is not "did it open" but "did the pipe carry
   * anything", so that is what the exit status reports: a refusal and a host
   * that never dialled back are both failures, and both open first.
   */
  function report(code) {
    if (code !== null) {
      const named = CLOSE_NAMES.get(code) ?? 'an unrecognised close code';
      say(`closed: ${code} — ${named}`);
    }
    say(`${bytesOut} B sent, ${bytesIn} B back`);
    if (code === CLOSE_TICKET_REFUSED || code === CLOSE_HOST_ABSENT || code === CLOSE_PIPE_DIAL_TIMEOUT) {
      die('the attach', 'the relay refused it — the line above says which');
    }
    if (bytesOut > 0 && bytesIn === 0) {
      die('the pipe', 'bytes went out and nothing came back — is the daemon answering?');
    }
    say('OK');
    process.exit(0);
  }

  ws.addEventListener('close', (event) => {
    if (event.reason) say(`reason: ${event.reason}`);
    report(event.code);
  });

  ws.addEventListener('error', () => {
    // A failed upgrade and a refused connection are the same event here, so the
    // URL is the useful half — with three endpoints in play, "it failed"
    // without one is the least useful thing this tool could print.
    if (!opened) die('the attach dial', `could not open ${url} — is \`wrangler dev\` running?`);
  });
}

await main();
