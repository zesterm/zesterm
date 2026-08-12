/**
 * The daemon's outbound leg, in Node — so the whole relay path can be run
 * before `zest-daemon` learns to dial a relay.
 *
 * ```
 * fake-host  --(1) GET /v1/control?host=…-->  relay Worker
 *            <--   {"t":"challenge",…}
 *            --    {"t":"hello",…,"sig":…}-->
 *            <--   {"t":"ready"}                     the link is parked
 *
 *            <--   {"t":"open","pipe":…}             a browser attached
 *            --(2) GET /v1/pipe?host=…&pipe=…-->     the dial-back
 *            --(3) ws://127.0.0.1:7718 ------->      a real zest-daemon
 *            ====  bytes, both ways, for ever
 * ```
 *
 * Everything else in a run of this is real: a real ticket, a real Durable
 * Object under workerd, a real `zest-daemon --listen-ws`, real `zest-proto`
 * framing inside a real ADR-008 sealed channel. This script is the one
 * simulated part, and it is simulated *narrowly* — it is a byte pump and an
 * Ed25519 signature, and it decides nothing.
 *
 * # It is a diagnostic, in the tradition of `examples/attach.rs`
 *
 * The job is to answer "which layer is wrong" without the layers above it.
 * So every stage prints where it got to, and every failure names the stage it
 * failed at rather than the exception it failed with: "the control link was
 * refused: bad-signature" is an answer, `Error: unexpected server response` is
 * a starting point for an afternoon.
 *
 * # Its key is its own, and that is not a shortcut
 *
 * The signature this sends is over the relay's nonce with **this script's**
 * key, so the `hosts` row it needs in D1 names this script's public key and
 * not the daemon's. That is not a compromise made to avoid reading the
 * daemon's secret: the relay's `hello` proves *which machine parked the link*,
 * while the `zest-proto` handshake inside the pipe separately proves which
 * machine is on the other end of the shell — and the second one really is the
 * daemon here, with its own key, through this pump. The two proofs are
 * deliberately independent (ADR-008: the relay is a dumb pipe, and nothing it
 * verifies substitutes for the host's own handshake), which is exactly what
 * makes standing in for one of them a fair test of the other.
 *
 * The seed defaults to a fixed, published value so the D1 row and the script
 * agree without anyone copying hex between two terminals. A random default
 * would mean editing the database every run. **It is a development key in a
 * committed file**: never point this at a relay whose `hosts` table matters.
 *
 * # Not a test, and deliberately not wired into one
 *
 * `pnpm -C cloud -r test` must stay a `node --test` run that needs no workerd,
 * no daemon and no network — that property is why the room is written against
 * `src/room/state.ts`'s narrow interfaces at all. This script needs all three,
 * so it is a thing a person runs and reads. `cloud/README.md` says how.
 */

import { getPublicKeyAsync, signAsync } from '@noble/ed25519';
import { fromHex, hex, signingPreimage } from '@zesterm/cloud-shared';

/** `zest_mesh::identity::NONCE_LEN`, and `room/control.ts`'s `NONCE_BYTES`. */
const NONCE_BYTES = 32;

/** An Ed25519 seed. `HostId` is the public half (ADR-006). */
const SEED_BYTES = 32;

/**
 * `sha256("zesterm fake-host dev seed")`.
 *
 * A constant rather than a fresh key per run, because the host id it derives
 * has to already be a row in D1 — see the header. Written out rather than
 * computed from the phrase so that grepping for either the seed or the host id
 * finds this line.
 *
 * host id: b417fae55b7a18917bf0e3760eb421e8acca7797e32e4d3f1735ac90fde20c31
 */
const DEV_SEED = '642d6c57266ed50639cfbf2565e93ec97e2efc0dbfd2f8fd5a96964befe42602';

/**
 * `room/control.ts`'s `KEEPALIVE_REQUEST` and `KEEPALIVE_RESPONSE`.
 *
 * The relay answers these **without waking the Durable Object** — it is the
 * whole of ADR-009's "an idle host costs nothing", and the one line of the
 * design that no unit test can check, because what is being asserted is about
 * the runtime rather than about the room. Sending them here is how a person
 * sees it happen: the pong comes back and `wrangler dev` logs no request.
 */
const KEEPALIVE_REQUEST = 'ping';
const KEEPALIVE_RESPONSE = 'pong';

const DEFAULTS = {
  relay: 'http://127.0.0.1:8787',
  daemon: 'ws://127.0.0.1:7718',
  seed: DEV_SEED,
  label: 'fake-host',
  /**
   * Far below anything an idle TCP path needs, because this is a demonstration
   * rather than a keepalive: at 30s a person watching the log learns nothing
   * before they give up.
   */
  ping: '5000',
};

const USAGE = `fake-host — stand in for the daemon's relay leg

  node tools/fake-host.mjs [options]

  --relay <origin>   the relay Worker            (default ${DEFAULTS.relay})
  --daemon <ws url>  a zest-daemon --listen-ws   (default ${DEFAULTS.daemon})
  --seed <64 hex>    this host's Ed25519 seed    (default: the dev seed)
  --label <text>     the unsigned label in hello (default ${DEFAULTS.label})
  --ping <ms>        keepalive interval, 0 off   (default ${DEFAULTS.ping})
  --host-id          print the host id and exit
`;

/**
 * Every line is stamped with milliseconds since the process started.
 *
 * The stages this prints are minutes apart in a real run and milliseconds apart
 * when something is wrong, and which of those happened is the answer more often
 * than the text of the line is.
 */
const startedAt = Date.now();

function say(...parts) {
  console.error(`[fake-host +${Date.now() - startedAt}ms]`, ...parts);
}

/** Print where it broke, then stop. Every exit from a failure comes through here. */
function die(stage, detail) {
  say(`FAILED at ${stage}: ${detail}`);
  process.exit(1);
}

function parseArgs(argv) {
  const options = { ...DEFAULTS, printHostId: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      process.stdout.write(USAGE);
      process.exit(0);
    } else if (arg === '--host-id') {
      options.printHostId = true;
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
 * `http(s)://` → `ws(s)://`, keeping everything else.
 *
 * The same rewrite `clients/web/packages/app/src/relay-dial.ts` does, and for
 * the same reason: an origin is what a person has in hand (it is what they
 * typed at `wrangler dev`), and a scheme mismatch is a `SyntaxError` from the
 * WebSocket constructor rather than anything about the relay.
 */
function wsUrl(origin, path, params) {
  const url = new URL(path, origin);
  for (const [key, value] of Object.entries(params)) url.searchParams.set(key, value);
  if (url.protocol === 'https:') url.protocol = 'wss:';
  else if (url.protocol === 'http:') url.protocol = 'ws:';
  return url.toString();
}

/**
 * An open WebSocket, or a rejection that says which endpoint refused.
 *
 * Node's `WebSocket` reports a failed upgrade as an `error` event whose
 * message is the same for a 404, a 426 and a connection refused, so the URL is
 * folded into the rejection here — with three endpoints in play, "it failed"
 * without one is the least useful thing this script could print.
 */
function open(url, protocols) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, protocols);
    ws.binaryType = 'arraybuffer';
    ws.addEventListener('open', () => resolve(ws), { once: true });
    ws.addEventListener(
      'error',
      () => reject(new Error(`could not open ${url}`)),
      { once: true },
    );
  });
}

/**
 * Pump one socket into the other until either goes away.
 *
 * Bytes that arrive while the far socket is not `OPEN` are held and reported
 * rather than sent, because `send` throws on a socket that is connecting or
 * already closing, and a throw in a message listener is an unhandled rejection
 * that kills the process mid-session. Nothing in today's path reaches that
 * branch — `openPipe` has both `open` events before either direction is
 * pumped — so it is a line that says so out loud if that ever stops being
 * true, rather than one that quietly drops a `Hello`.
 */
function pipe(from, to, onGone) {
  const queued = [];
  const flush = () => {
    while (queued.length > 0 && to.readyState === WebSocket.OPEN) to.send(queued.shift());
  };
  from.addEventListener('message', (event) => {
    // Binary only. The pipes carry `zest-proto` frames; a text frame is
    // something no end of this path sends, and forwarding it would launder a
    // protocol error into a decode failure two layers away.
    if (typeof event.data === 'string') {
      say(`ignoring a text frame on the pipe (${event.data.length} chars)`);
      return;
    }
    queued.push(event.data);
    flush();
    if (queued.length > 0) say(`holding ${queued.length} message(s): the far socket is not open`);
  });
  from.addEventListener('close', (event) => onGone(event.code, event.reason));
  from.addEventListener('error', () => onGone(null, 'transport error'));
}

/**
 * One pipe: dial the daemon, dial the relay, join them.
 *
 * **The daemon first.** The relay holds the browser's `101` until this dial
 * back lands (`room.ts`'s `openAttach` awaits it), so a browser released
 * before the daemon socket exists sends its `Hello` into a pump with one end.
 * Dialling the daemon first makes the ordering structural rather than a race
 * the queue above has to win.
 */
async function openPipe(options, hostId, pipeId) {
  const short = `${pipeId.slice(0, 8)}…`;
  say(`open ${short}: dialling the daemon at ${options.daemon}`);

  let daemon;
  try {
    daemon = await open(options.daemon);
  } catch (error) {
    // Not fatal to the process: the control link is still parked and the next
    // attach deserves its turn. The browser sees `CLOSE_PIPE_DIAL_TIMEOUT`
    // once the relay's deadline passes, which is the truthful answer — the
    // host was asked and did not come back.
    say(`open ${short}: the daemon refused the dial (${error.message})`);
    say(`open ${short}: is zest-daemon running with --listen-ws?`);
    return;
  }

  let relay;
  try {
    relay = await open(wsUrl(options.relay, '/v1/pipe', { host: hostId, pipe: pipeId }));
  } catch (error) {
    daemon.close();
    // A 404 here is the relay saying this id names no pending pipe: the dial
    // missed `PIPE_DIAL_TIMEOUT_MS`, or the browser hung up while we were
    // opening the daemon socket.
    say(`open ${short}: the relay refused the dial-back (${error.message})`);
    return;
  }

  say(`open ${short}: both legs up, pumping`);
  let bytesToDaemon = 0;
  let bytesToRelay = 0;
  const end = (side) => (code, reason) => {
    say(
      `open ${short}: ${side} closed (${code ?? 'error'}${reason ? ` ${reason}` : ''}); ` +
        `${bytesToDaemon} B to the daemon, ${bytesToRelay} B to the browser`,
    );
    relay.close();
    daemon.close();
  };
  relay.addEventListener('message', (e) => {
    if (typeof e.data !== 'string') bytesToDaemon += e.data.byteLength;
  });
  daemon.addEventListener('message', (e) => {
    if (typeof e.data !== 'string') bytesToRelay += e.data.byteLength;
  });
  pipe(relay, daemon, end('the browser leg'));
  pipe(daemon, relay, end('the daemon'));
}

/**
 * Start pinging, once the link is parked and not before.
 *
 * A `ping` sent during the handshake is not free — the auto-response pair is
 * installed by then, but a daemon pinging while it still owes a `hello` is not
 * a thing to model, and modelling it would put a message on the wire that the
 * handshake's own timeout is being measured against.
 */
function startKeepalive(control, intervalMs) {
  if (!Number.isFinite(intervalMs) || intervalMs <= 0) {
    say('keepalive off (--ping 0)');
    return;
  }
  say(`pinging every ${intervalMs} ms`);
  // `unref` so the interval is not what keeps this process alive: the control
  // link is, and a script that outlived its own socket would sit there pinging
  // a closed one.
  setInterval(() => {
    if (control.readyState === WebSocket.OPEN) control.send(KEEPALIVE_REQUEST);
  }, intervalMs).unref();
}

/** The `hello` this script answers a `challenge` with. */
async function helloFor(challenge, seed, hostId, label) {
  const nonce = fromHex(challenge.nonce, NONCE_BYTES);
  if (nonce === null) return null;
  // `Role::Host` + `Purpose::Auth` over the nonce bytes, through the one copy
  // of the preimage both Workers use — the same construction
  // `test/control-golden.test.ts` pins against a signature Rust produced. A
  // second copy here is exactly the drift that file exists to catch.
  const sig = await signAsync(signingPreimage('host', 'auth', nonce), seed);
  return JSON.stringify({ t: 'hello', v: 1, host: hostId, label, sig: hex(sig) });
}

async function main() {
  const options = parseArgs(process.argv.slice(2));

  const seed = fromHex(options.seed, SEED_BYTES);
  if (seed === null) die('the command line', '--seed must be 64 lowercase hex characters');
  const hostId = hex(await getPublicKeyAsync(seed));

  if (options.printHostId) {
    process.stdout.write(`${hostId}\n`);
    return;
  }

  const url = wsUrl(options.relay, '/v1/control', { host: hostId });
  say(`host ${hostId}`);
  say(`dialling ${url}`);

  let control;
  try {
    control = await open(url);
  } catch (error) {
    die(
      'the control dial',
      `${error.message} — is \`wrangler dev\` running in cloud/packages/relay?`,
    );
  }
  say('control link open; waiting for the challenge');

  let parked = false;
  let pongs = 0;
  control.addEventListener('message', (event) => {
    if (typeof event.data !== 'string') {
      die('the control link', 'the relay sent a binary frame on a text channel');
    }
    // Before `JSON.parse`, and it has to be: the keepalive response is a bare
    // `pong`, which is not JSON, so a script that parsed first would answer the
    // relay's cheapest correct behaviour with `FAILED: not JSON`.
    if (event.data === KEEPALIVE_RESPONSE) {
      pongs += 1;
      // Only the first, because the interesting event is that the pair works at
      // all — after that it is one line every `--ping` milliseconds for as long
      // as this runs, which buries everything else.
      if (pongs === 1) {
        say(`pong — the relay answers the keepalive. \`wrangler dev\` should log no request for it.`);
      }
      return;
    }
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      die('the control link', `the relay sent something that is not JSON: ${event.data}`);
      return;
    }

    // `void`: the listener is sync, and everything below either finishes the
    // process or is a fire-and-forget dial whose own failures it reports.
    void (async () => {
      switch (message.t) {
        case 'challenge': {
          say(`challenge: nonce ${message.nonce?.slice(0, 8)}…, relay_key ${message.relay_key?.slice(0, 8)}…`);
          const hello = await helloFor(message, seed, hostId, options.label);
          if (hello === null) {
            die('the challenge', `nonce is not ${NONCE_BYTES} bytes of hex: ${message.nonce}`);
            return;
          }
          control.send(hello);
          say('hello sent; waiting for ready');
          return;
        }
        case 'ready': {
          parked = true;
          say('READY — the control link is parked. Waiting for a browser to attach.');
          startKeepalive(control, Number(options.ping));
          return;
        }
        case 'open': {
          if (!parked) die('the control link', 'an `open` arrived before `ready`');
          await openPipe(options, hostId, message.pipe);
          return;
        }
        case 'error': {
          // The relay names its refusals on this link precisely so a machine
          // knows whether to re-enrol, fix its clock or upgrade — see
          // `room/control.ts`. Passing the code straight through is the whole
          // value of that decision.
          die('the control handshake', `the relay refused: ${message.code}`);
          return;
        }
        default:
          say(`ignoring an unknown control message: ${event.data}`);
      }
    })();
  });

  control.addEventListener('close', (event) => {
    // 4401 refused, 4409 replaced, 4503 the relay has no key of its own —
    // `room/control.ts` is the list, and the `error` frame above usually
    // arrives first with the detail.
    die('the control link', `closed by the relay: ${event.code} ${event.reason}`);
  });
  control.addEventListener('error', () => {
    die('the control link', 'the transport failed');
  });
}

await main();
