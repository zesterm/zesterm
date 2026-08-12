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
 * Enrolment is the spine here, not discovery. It is durable and account-scoped
 * and survives a machine being asleep; presence will decorate it once there is
 * a relay to learn presence from. Until then `last seen` is the only honest
 * thing to show, and it comes from the enrolment record. The same honesty
 * shapes what each card OMITS — see `fleet-model.ts` and the note below.
 */

import { component, signal } from 'sigx';
import type { Theme } from '@zesterm/theme';

import type { Bootstrap, User } from '../bootstrap.ts';
import type { DeviceKey } from '../device-key.ts';
import { ago, hostCard } from '../fleet-model.ts';
import { fetchRegistry, revoke, type Device, type Host } from '../registry.ts';
import { AccountMenu } from './AccountMenu.tsx';
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

  const state = signal<{ load: Load; busy: string | null }>({
    load: { phase: 'loading' },
    busy: null,
  });

  const load = (): void => {
    fetchRegistry()
      .then((r) => (state.load = { phase: 'ready', hosts: r.hosts, devices: r.devices }))
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

  return () => (
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
                    const card = hostCard(h, { localHostId: null, now: Date.now() });
                    return (
                      <li class={`host-card${card.local ? ' local' : ''}`}>
                        <div class="card-head">
                          {/* Faint on purpose: presence is unknown until the
                              relay exists — a green dot would claim liveness
                              the directory cannot know. */}
                          <span class="card-dot" />
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
                      </li>
                    );
                  })}
                </ul>
              )}
              {/* Honest about the gap: enrolled is not the same as reachable. */}
              {state.load.hosts.length > 0 ? (
                <p class="fineprint">
                  Enrolled, but not yet reachable from here — the relay that carries terminal
                  traffic between your machines is not built.
                </p>
              ) : null}
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
                    <li class="row">
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
});
