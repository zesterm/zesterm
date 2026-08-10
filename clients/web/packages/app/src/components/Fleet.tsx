/**
 * Your machines, and what can reach them.
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
 * thing to show, and it comes from the enrolment record.
 */

import { component, signal } from 'sigx';
import type { Theme } from '@zesterm/theme';

import type { Bootstrap, User } from '../bootstrap.ts';
import type { DeviceKey } from '../device-key.ts';
import { fetchRegistry, revoke, type Device, type Host } from '../registry.ts';
import { AccountMenu } from './AccountMenu.tsx';
import { Shell } from './Shell.tsx';

/** Rough, and deliberately so — an exact age is not a thing anyone reads. */
function ago(at: number | null, now: number): string {
  if (at === null) return 'never';
  const mins = Math.floor((now - at) / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

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

  const now = Date.now();
  const user = bootstrap.user as User;

  return () => (
    <div class="shell">
      <header class="topbar">
        <span class="brand">zesterm</span>
        <AccountMenu user={user} />
      </header>

      <div class="fleet">
        {state.load.phase === 'loading' ? <p class="muted">Loading…</p> : null}

        {state.load.phase === 'failed' ? (
          <p class="error" role="alert">
            Could not read your account: {state.load.error}
          </p>
        ) : null}

        {state.load.phase === 'ready' ? (
          <>
            <section>
              <h2>Machines</h2>
              {state.load.hosts.length === 0 ? (
                <p class="muted">
                  No machines yet. Run <code>zest-daemon --enroll &lt;code&gt;</code> on one to add
                  it.
                </p>
              ) : (
                <ul class="rows">
                  {state.load.hosts.map((h) => (
                    <li class="row">
                      <span class="row-name">{h.label}</span>
                      <span class="row-meta">
                        {h.platform !== '' ? `${h.platform} · ` : ''}last seen {ago(h.lastSeenAt, now)}
                      </span>
                      <button
                        class="button subtle"
                        disabled={state.busy === h.id}
                        onClick={() => drop('hosts', h.id, h.label)}
                      >
                        revoke
                      </button>
                    </li>
                  ))}
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
                        {d.kind} · last seen {ago(d.lastSeenAt, now)}
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
