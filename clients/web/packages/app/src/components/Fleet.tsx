/**
 * Your machines, and what can reach them — as the design §7 card grid.
 *
 * **Both paths are the tabbed shell now** (#344). This mounts it and hands it
 * what differs: the machines it can launch on, where each machine's session
 * list comes from, and this grid to show when the URL names no machine.
 *
 * The hosted path used to be three screens *inside this component* — grid,
 * then a machine's session list, then a bare terminal with a `← back` — with
 * `state.open` and `state.session` switching between them. Which meant the one
 * path that actually has a fleet had no tabs, no launcher and no palette, and
 * crossing between machines threw away whatever you had open. The note that
 * used to sit here said making them real routes was a separate change; the
 * routes already existed (`route-table.ts`), and what was missing was a shell
 * that could hold more than one machine (#332, #338, #340).
 *
 * On the **local** path the shell is unchanged: the sidecar hosts the
 * `SessionDirectory` actor and the daemon is a `ws://` away.
 *
 * On the **hosted** path this grid is the account's registry. Two lists, because they
 * answer different questions — `hosts` is your machines, `devices` is the
 * browsers and phones holding keys that can attach to them. A browser never
 * appears in a fleet listing because it serves no sessions, so this is the only
 * place one can be seen or revoked.
 *
 * **Enrolment is still the spine, and presence now decorates it.** The registry
 * is durable and account-scoped and survives a machine being asleep;
 * `liveDirectory` dials each enrolled machine through the relay and says which
 * of them are actually there. `last seen` stays the registry's answer and is
 * the only thing worth showing for a machine that is not.
 *
 * The same honesty shapes what each card OMITS — see `fleet-model.ts` — and it
 * is why `asleep` is not painted as a fault: over the relay most of a fleet is
 * asleep most of the time, and a screen that shows the ordinary case in red is
 * one people stop reading, taking the real faults with it.
 */

import { component, onUnmounted, signal } from 'sigx';
import { useNavigate } from '@sigx/router';
import type { Theme } from '@zesterm/theme';

import type { Bootstrap, User } from '../bootstrap.ts';
import type { DeviceKey } from '../device-key.ts';
import {
  browserLabel,
  deviceRow,
  deviceVouchAction,
  hostCard,
  mintPanelOnStart,
  ownDeviceAction,
  ownDeviceApproved,
} from '../fleet-model.ts';
import { liveHostSource } from '../host-source.ts';
import { liveDirectory, relayLinks } from '../live-directory.ts';
import { relayAccess } from '../relay-access.ts';
import {
  approveDevice,
  fetchRegistry,
  mintEnrollCode,
  registerDevice,
  revoke,
  type Device,
  type Host,
} from '../registry.ts';
import { AccountMenu } from './AccountMenu.tsx';
import { EnrollCode } from './EnrollCode.tsx';
import { Shell } from './Shell.tsx';

type Load =
  | { readonly phase: 'loading' }
  | { readonly phase: 'ready'; readonly hosts: readonly Host[]; readonly devices: readonly Device[] }
  | { readonly phase: 'failed'; readonly error: string };

export const Fleet = component<{
  bootstrap: Bootstrap;
  device: DeviceKey;
  theme: Theme;
}>((ctx) => {
  const { bootstrap, device, theme } = ctx.props;
  const navigate = useNavigate();

  // The local path never touches any of this, so nothing is fetched there.
  if (bootstrap.mode === 'local') {
    return () => <Shell device={device} theme={theme} />;
  }

  const state = signal<{
    load: Load;
    busy: string | null;
    /** The one open enrolment panel, if any — minting for either kind replaces it. */
    mint: { readonly kind: 'host' | 'device'; readonly code: string; readonly expiresAt: number } | null;
    /** Which kind a mint is in flight for, so the buttons can say so. */
    minting: 'host' | 'device' | null;
    mintError: { readonly kind: 'host' | 'device'; readonly message: string } | null;
  }>({
    load: { phase: 'loading' },
    busy: null,
    mint: null,
    minting: null,
    mintError: null,
  });

  /**
   * One connection per enrolled machine, held for as long as this screen is.
   *
   * Built once rather than per load: `setHosts` is idempotent for a machine
   * already watched, and the fleet refetches `/api/hosts` after every revoke —
   * rebuilding here would drop and redial every pipe in the account because
   * one browser key was removed.
   */
  const relay = relayAccess(bootstrap);
  /**
   * `null` when this deployment has no relay, and then nothing is watched.
   *
   * Driving the directory anyway would put every machine into `failed` with
   * `NO_RELAY` — collapsing "nobody asked" into "we asked and it went wrong",
   * which is precisely the distinction `presenceOf` exists to keep. A card
   * whose deployment cannot reach any machine should say nothing, not accuse
   * each one in turn.
   */
  const live = relay === null ? null : liveDirectory({ openLink: relayLinks(device.signer, relay) });
  onUnmounted(() => live?.close());

  // Once per mount, not once per load: `load()` runs again after every revoke,
  // and a registration the Worker keeps refusing must not be retried on each —
  // that is a request loop wearing a refresh's clothes.
  let registerTried = false;

  const load = (): void => {
    fetchRegistry()
      .then((r) => {
        state.load = { phase: 'ready', hosts: r.hosts, devices: r.devices };
        // Only the machines still in the account: a revoked host's connection
        // is closed by the same call that stops listing it.
        live?.setHosts(r.hosts.map((h) => ({ id: h.id, label: h.label })));

        // This browser's own key, silently, when the account does not list it
        // yet. Silent in both directions — no button and no error: the row
        // (and the pending banner, when the bootstrap rule does not apply)
        // appearing on the refetch is the whole surface, and a failure leaves
        // a working screen that simply is not listed yet. Ephemeral keys are
        // excluded in `ownDeviceAction`: they are gone next load, and a
        // pending row per visit helps nobody.
        const account = bootstrap.user?.id;
        if (
          account !== undefined &&
          !registerTried &&
          ownDeviceAction(r.devices, device.signer.clientId, device.ephemeral === true) === 'register'
        ) {
          registerTried = true;
          registerDevice({
            signer: device.signer,
            account,
            label: browserLabel(navigator.userAgent),
            // Reported honestly from the key's own kind: a seed is readable
            // by any script on the origin, a WebCrypto key is not.
            extractable: device.kind === 'seed',
          })
            .then(() => load())
            .catch(() => {
              // Nothing to show: the screen is already rendering the account
              // as it stands, and the next mount will try again.
            });
        }
      })
      .catch((e: unknown) => {
        state.load = { phase: 'failed', error: e instanceof Error ? e.message : String(e) };
      });
  };
  load();

  const drop = (table: 'hosts' | 'devices', id: string, label: string): void => {
    // Revocation is a positive statement and cannot be undone from here — the
    // row stays revoked so a key cannot quietly re-enrol as though it were new.
    // That is worth one confirm.
    if (!confirm(`Revoke ${label}? It will have to enrol again to reach your machines.`)) return;
    state.busy = id;
    revoke(table, id)
      .then(() => {
        state.busy = null;
        load();
      })
      .catch((e: unknown) => {
        state.busy = null;
        state.load = { phase: 'failed', error: e instanceof Error ? e.message : String(e) };
      });
  };

  /**
   * Vouch for a device as this browser: sign an attestation over its id and
   * label and hand it to the approve route. Approving a pending row is what
   * the button is for; on an approved row the same act is a renewal — the
   * Worker replaces this approver's earlier statement.
   *
   * No confirm, unlike `drop`: approval is reversible (revoke it), and the
   * person just read the row they are approving.
   */
  const approve = (d: Device): void => {
    const account = bootstrap.user?.id;
    if (account === undefined || state.busy !== null) return;
    state.busy = d.id;
    approveDevice({ signer: device.signer, account, device: d, now: Date.now() })
      .then(() => {
        state.busy = null;
        load();
      })
      .catch((e: unknown) => {
        state.busy = null;
        state.load = { phase: 'failed', error: e instanceof Error ? e.message : String(e) };
      });
  };

  const mint = (kind: 'host' | 'device'): void => {
    if (state.minting !== null) return;
    // A cross-kind mint clears the visible panel NOW, not when the new code
    // lands — see `mintPanelOnStart` for why the stale panel is the bug.
    state.mint = mintPanelOnStart(state.mint, kind);
    state.minting = kind;
    state.mintError = null;
    mintEnrollCode(kind)
      .then((minted) => {
        state.minting = null;
        state.mint = { kind, code: minted.code, expiresAt: minted.expiresAt };
      })
      .catch((e: unknown) => {
        // A failed mint is the button's problem, not the screen's — putting
        // `load` into `failed` here would tear down two healthy lists over a
        // request the user can simply retry.
        state.minting = null;
        state.mintError = { kind, message: e instanceof Error ? e.message : String(e) };
      });
  };

  const user = bootstrap.user as User;

  /**
   * The fleet grid, as the shell's landing pane (#344).
   *
   * A closure rather than an element: it is called inside `Shell`'s render, so
   * the signal reads inside it register there and the grid updates when the
   * registry does — without `Fleet` re-rendering a shell that is holding open
   * tabs and live pipes.
   */
  const grid = (): unknown => {
    // Local consts rather than `state.mint?.…` in the JSX: narrowing does not
    // survive into the nested closures, and each render reads one snapshot.
    const minted = state.mint;
    // Whether this browser may sign vouchers, computed once per render from
    // the same snapshot the rows render from — so the buttons and the banner
    // can never disagree about it.
    const canVouch =
      state.load.phase === 'ready' &&
      ownDeviceApproved(state.load.devices, device.signer.clientId, device.ephemeral === true);
    const mintErr = state.mintError;
    return (
      <div class="fleet-page">
        <header class="page-head">
          <h1>
            Your fleet
            <span class="page-tagline">
              every machine is a host · every window, tab and phone is a client
            </span>
          </h1>
          {/* The account menu rides the page head now that the shell owns the
              chrome: sign-out is account UI, and the fleet screen is where the
              account is. A second topbar inside a pane would be one bar too
              many. */}
          <span class="grow" />
          <AccountMenu user={user} />
          <p class="page-lede">
            The directory knows which machines exist and how to reach them. Sessions never leave
            the machine they run on.
          </p>
        </header>

        {state.load.phase === 'loading' ? <p class="muted">Loading…</p> : null}

        {state.load.phase === 'failed' ? (
          <p class="error" role="alert">
            Could not read your account: {state.load.error}
          </p>
        ) : null}

        {state.load.phase === 'ready' ? (
          <>
            <section>
              <div class="section-head">
                <h2>Machines</h2>
                <span class="grow" />
                <button
                  class="button subtle"
                  disabled={state.minting !== null}
                  onClick={() => mint('host')}
                >
                  {state.minting === 'host' ? 'minting…' : 'Add a machine'}
                </button>
              </div>
              {mintErr !== null && mintErr.kind === 'host' ? (
                <p class="error" role="alert">
                  Could not mint a code: {mintErr.message}
                </p>
              ) : null}
              {minted !== null && minted.kind === 'host' ? (
                <EnrollCode
                  kind="host"
                  code={minted.code}
                  expiresAt={minted.expiresAt}
                  // Per-kind, not `!== null`: this panel must never claim a
                  // mint the other section started.
                  busy={state.minting === 'host'}
                  onRemint={() => mint('host')}
                  onClose={() => (state.mint = null)}
                />
              ) : null}
              {state.load.hosts.length === 0 ? (
                <p class="muted">
                  No machines yet. Run <code>zest-daemon --enroll &lt;code&gt;</code> on one to add
                  it.
                </p>
              ) : (
                <ul class="card-grid">
                  {/* Deliberately absent from these cards, each tracked rather
                      than rendered dead: path/latency rows and the tunnel pill
                      (#148), wake-over-LAN (#146). Session counts appear only
                      when something real supplies one — the registry does not.
                      localHostId is null: on the hosted path the browser is a
                      DEVICE, so no host is identifiably this machine. */}
                  {state.load.hosts.map((h) => {
                    const card = hostCard(h, {
                      localHostId: null,
                      now: Date.now(),
                      // `undefined` where nothing is watching, which is what
                      // makes the dot read `unknown` rather than `asleep`.
                      status: live?.statusFor(h.id),
                    });
                    return (
                      <li
                        key={h.id}
                        class={`host-card${card.local ? ' local' : ''}${
                          card.presence.reachable ? ' reachable' : ''
                        }`}
                      >
                        <div class="card-head">
                          {/* The dot states what the socket says and nothing
                              more. `unknown` keeps the faint dot it always had:
                              a deployment with no relay has not learned that a
                              machine is absent, it has not asked. */}
                          <span class={`card-dot ${card.presence.kind}`} />
                          <span class="card-name">{card.name}</span>
                          {card.local ? <span class="card-note">this machine</span> : null}
                          <span class="grow" />
                          <button
                            class="button subtle"
                            disabled={state.busy === h.id}
                            onClick={() => drop('hosts', h.id, h.label)}
                          >
                            revoke
                          </button>
                        </div>
                        <div class="card-rows">
                          {card.rows.map((r) => (
                            <div class="card-row">
                              <span class="card-label">{r.label}</span>
                              <span class={`card-value${r.mono ? ' mono' : ''}`}>{r.value}</span>
                            </div>
                          ))}
                        </div>
                        <div class="card-foot">
                          <span class={`presence ${card.presence.kind}`}>{card.presence.text}</span>
                          <span class="grow" />
                          {/* Only a machine that answered offers a way in. A
                              button that opens a screen saying "asleep" is a
                              worse answer than no button. */}
                          {card.presence.reachable ? (
                            <button class="button" onClick={() => void navigate(`/h/${h.id}`)}>
                              open
                            </button>
                          ) : null}
                        </div>
                      </li>
                    );
                  })}
                </ul>
              )}
            </section>

            <section>
              <div class="section-head">
                <h2>Browsers and phones</h2>
                <span class="grow" />
                <button
                  class="button subtle"
                  disabled={state.minting !== null}
                  onClick={() => mint('device')}
                >
                  {state.minting === 'device' ? 'minting…' : 'Add a device'}
                </button>
              </div>
              <p class="fineprint">
                These hold keys that can attach to your machines. They never appear as machines
                themselves, because they serve no sessions.
              </p>
              {mintErr !== null && mintErr.kind === 'device' ? (
                <p class="error" role="alert">
                  Could not mint a code: {mintErr.message}
                </p>
              ) : null}
              {minted !== null && minted.kind === 'device' ? (
                <EnrollCode
                  kind="device"
                  code={minted.code}
                  expiresAt={minted.expiresAt}
                  busy={state.minting === 'device'}
                  onRemint={() => mint('device')}
                  onClose={() => (state.mint = null)}
                />
              ) : null}
              {ownDeviceAction(
                state.load.devices,
                device.signer.clientId,
                device.ephemeral === true,
              ) === 'awaiting-approval' ? (
                // The one fact worth a banner: this row is *this browser*, and
                // pending means it cannot reach any machine yet. The list
                // below marks the row too, but nothing there says which
                // device the reader is sitting at.
                <p class="fineprint" role="status">
                  <span class="warn">This browser is awaiting approval</span> — it is listed below
                  and can be revoked, but it cannot reach your machines until an approved device
                  vouches for it.
                </p>
              ) : null}
              {state.load.devices.length === 0 ? (
                <p class="muted">Nothing enrolled yet.</p>
              ) : (
                <ul class="rows">
                  {state.load.devices.map((d) => {
                    const row = deviceRow(d, Date.now());
                    const action = deviceVouchAction(d, device.signer.clientId, canVouch);
                    return (
                      <li key={row.id} class="row">
                        <span class="row-name">{row.name}</span>
                        <span class="row-meta">
                          {row.meta}
                          {row.pending ? (
                            // Pending is a state, not a fault — but it is the
                            // warn colour because an unexpected pending row is
                            // a key somebody registered with this account's
                            // session, and the owner should look at it.
                            <span class="warn"> · pending approval</span>
                          ) : null}
                          {row.keyReadable ? (
                            // Said out loud rather than shown as a tick: this key
                            // is readable by any script on the origin, which is
                            // working but not secure.
                            <span class="warn"> · key readable by scripts on this origin</span>
                          ) : null}
                        </span>
                        {action !== null ? (
                          // `approve` turns a waiting device into a working
                          // one; `vouch` renews or adds this browser's own
                          // attestation for an already-approved device — see
                          // `deviceVouchAction` for why every approved row
                          // gets the offer.
                          <button
                            class="button"
                            disabled={state.busy !== null}
                            onClick={() => approve(d)}
                          >
                            {action}
                          </button>
                        ) : null}
                        <button
                          class="button subtle"
                          disabled={state.busy === row.id}
                          onClick={() => drop('devices', row.id, row.name)}
                        >
                          {row.removeLabel}
                        </button>
                      </li>
                    );
                  })}
                </ul>
              )}
            </section>
          </>
        ) : null}
      </div>
    );
  };

  // The hosted path is the tabbed shell too (#344). It used to be three
  // screens inside this component — grid, then a machine's session list, then
  // a bare terminal with a `← back` — which meant no tabs, no launcher and no
  // palette on the one path that actually has a fleet.
  //
  // `Shell` holds the tabs; this hands it the account's machines and the grid
  // to show when no machine is named. Built once, outside the render, so a
  // registry refetch does not rebuild the seam under a shell that is holding
  // live pipes.
  const hosts = live === null ? null : liveHostSource(live, relay);
  return () => {
    // Nothing to watch and nothing to reach: a deployment with no relay. The
    // grid says so per card; a shell whose every launcher row was dead would
    // not.
    if (live === null || hosts === null) return grid();
    return (
      <Shell
        device={device}
        theme={theme}
        hosts={() => hosts}
        listSourceFor={(hostId) => live.sourceFor(hostId)}
        landing={grid}
      />
    );
  };
});
