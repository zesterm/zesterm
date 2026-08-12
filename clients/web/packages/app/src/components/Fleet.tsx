/**
 * Your machines, and what can reach them — as the design §7 card grid.
 *
 * On the **local** path this is the real session list, unchanged: the sidecar
 * hosts the `SessionDirectory` actor and the daemon is a `ws://` away.
 *
 * On the **hosted** path it is the account's registry. Two lists, because they
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
import type { Theme } from '@zesterm/theme';

import type { Bootstrap, User } from '../bootstrap.ts';
import type { DeviceKey } from '../device-key.ts';
import { ago, hostCard } from '../fleet-model.ts';
import { liveDirectory, relayLinks } from '../live-directory.ts';
import { relayAccess } from '../relay-access.ts';
import { fetchRegistry, revoke, type Device, type Host } from '../registry.ts';
import { AccountMenu } from './AccountMenu.tsx';
import { SessionList, type OpenTarget } from './SessionList.tsx';
import { TerminalView } from './TerminalView.tsx';
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

  // The local path never touches any of this, so nothing is fetched there.
  if (bootstrap.mode === 'local') {
    return () => <Shell device={device} theme={theme} />;
  }

  const state = signal<{
    load: Load;
    busy: string | null;
    /** Which machine's sessions are showing, if any. */
    open: { readonly id: string; readonly label: string } | null;
    /** Which session is attached, if any. */
    session: OpenTarget | null;
  }>({
    load: { phase: 'loading' },
    busy: null,
    open: null,
    session: null,
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

  const load = (): void => {
    fetchRegistry()
      .then((r) => {
        state.load = { phase: 'ready', hosts: r.hosts, devices: r.devices };
        // Only the machines still in the account: a revoked host's connection
        // is closed by the same call that stops listing it.
        live?.setHosts(r.hosts.map((h) => ({ id: h.id, label: h.label })));
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

  const user = bootstrap.user as User;

  /**
   * Three screens rather than three routes.
   *
   * The route table already points `/hosts`, `/h/:id` and `/h/:id/s/:id` at
   * this component, so switching here is what exists today; making them real
   * routes is a separate change with its own argument about deep links, and
   * doing it inside this one would bury the part that matters.
   */

  return () => {
    if (state.session !== null) {
      const target = state.session;
      // The terminal keeps the chrome, and the way back is the point: a
      // full-bleed pane with no header is a screen the user can only leave by
      // reloading the page, which drops every pipe this tab is holding.
      return (
        <div class="shell">
          <header class="topbar">
            <span class="brand">zesterm</span>
            <button class="button subtle" onClick={() => (state.session = null)}>
              ← sessions
            </button>
            <span class="grow" />
            <AccountMenu user={user} />
          </header>
          <TerminalView
            entry={target.entry}
            dial={target.dial}
            signer={device.signer}
            theme={theme}
          />
        </div>
      );
    }
    if (state.open !== null && live !== null) {
      const machine = state.open;
      return (
        <div class="shell">
          <header class="topbar">
            <span class="brand">zesterm</span>
            <button class="button subtle" onClick={() => (state.open = null)}>
              ← fleet
            </button>
            <span class="grow" />
            <AccountMenu user={user} />
          </header>
          <div class="fleet-page">
            <SessionList
              source={live.sourceFor(machine.id)}
              relay={relay}
              title={machine.label}
              connectingLabel="reaching this machine…"
              deviceKind={device.kind}
              onOpen={(t: OpenTarget) => (state.session = t)}
              onCreate={() =>
                live
                  .createSession(machine.id, { cols: 120, rows: 32 })
                  .catch((e: unknown) => {
                    state.load = {
                      phase: 'failed',
                      error: e instanceof Error ? e.message : String(e),
                    };
                  })
              }
            />
          </div>
        </div>
      );
    }
    return (
    <div class="shell">
      <header class="topbar">
        <span class="brand">zesterm</span>
        <AccountMenu user={user} />
      </header>

      <div class="fleet-page">
        <header class="page-head">
          <h1>
            Your fleet
            <span class="page-tagline">
              every machine is a host · every window, tab and phone is a client
            </span>
          </h1>
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
                            <button
                              class="button"
                              onClick={() => (state.open = { id: h.id, label: h.label })}
                            >
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
              <h2>Browsers and phones</h2>
              <p class="fineprint">
                These hold keys that can attach to your machines. They never appear as machines
                themselves, because they serve no sessions.
              </p>
              {state.load.devices.length === 0 ? (
                <p class="muted">Nothing enrolled yet.</p>
              ) : (
                <ul class="rows">
                  {state.load.devices.map((d) => (
                    <li key={d.id} class="row">
                      <span class="row-name">{d.label}</span>
                      <span class="row-meta">
                        {d.kind} · last seen {ago(d.lastSeenAt, Date.now())}
                        {d.extractable ? (
                          // Said out loud rather than shown as a tick: this key
                          // is readable by any script on the origin, which is
                          // working but not secure.
                          <span class="warn"> · key readable by scripts on this origin</span>
                        ) : null}
                      </span>
                      <button
                        class="button subtle"
                        disabled={state.busy === d.id}
                        onClick={() => drop('devices', d.id, d.label)}
                      >
                        revoke
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </section>
          </>
        ) : null}
      </div>
    </div>
    );
  };
});
