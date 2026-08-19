/**
 * The launch targets this account's machines publish (design §12), read-only.
 *
 * **Read-only, deliberately, and it says so on the screen.** A profile lives
 * in the config file of the machine that publishes it (ADR-014). The browser
 * has no config file, and writing one would mean a config-write path over the
 * wire that does not exist — inventing that inside a UI screen is how a seam
 * gets designed by accident. So this screen shows what each machine says it
 * can run, and names the machine you would edit it on.
 *
 * Strictly better than what `⌘⇧,` did before, which was to be claimed by the
 * chord table and then discarded (#352): the terminal never saw the keystroke
 * and nothing happened, which reads as a broken app rather than an unbuilt
 * feature.
 *
 * The design's editor — inheritance chips, the swatch picker, the live
 * preview — is the *native* screen's, and none of it is reachable without
 * writes. What is honest here is the rail: every target, under the machine
 * that owns it, with what it will actually run.
 */

import { component } from 'sigx';
import type { HostFacts } from '@zesterm/control';

import type { HostChoice } from '../chrome-model.ts';

export const ProfilesPane = component<{
  hosts: () => readonly HostChoice[];
  factsOf: (hostId: string) => HostFacts | null;
  /** Launch one. The same verb the launcher menu has — §12's two verbs, and
      this screen only ever gets the harmless one wrong way round. */
  onLaunch?: (target: { hostId: string; command: string; cwd: string }) => void;
  onClose?: () => void;
}>((ctx) => {
  return () => {
    const hosts = ctx.props.hosts();
    return (
      <div class="fleet-page profiles-page">
        <header class="page-head">
          <h1>
            Launch targets
            <span class="page-tagline">
              what each machine says it can run · edit them on the machine
            </span>
          </h1>
          {ctx.props.onClose === undefined ? null : (
            <button class="button subtle" onClick={() => ctx.props.onClose?.()}>
              Done
            </button>
          )}
        </header>
        {hosts.length === 0 ? (
          <p class="page-lede muted">No machines yet.</p>
        ) : (
          hosts.map((h) => {
            const facts = ctx.props.factsOf(h.id);
            return (
              <section key={h.id} class="profiles-host">
                <h2>
                  {h.label}
                  {/* Only what it said. A machine that has not answered gets
                      the sentence below instead of a row of blanks — an os we
                      cannot fill would be a dash pretending to be a fact. */}
                  {facts === null || facts.os === '' ? null : (
                    <span class="host-facts">
                      {[facts.os, facts.arch, facts.osVersion].filter((p) => p !== '').join(' · ')}
                    </span>
                  )}
                </h2>
                {facts === null ? (
                  // Not "no profiles": this machine has told us nothing at
                  // all, which is an older daemon or one nothing can reach,
                  // and those two readings are the user's to make.
                  <p class="page-lede muted">This machine has not said what it can run.</p>
                ) : facts.launchTargets.length === 0 ? (
                  <p class="page-lede muted">
                    No profiles defined on this machine — it will open its default shell
                    {facts.defaultShell === '' ? '' : `, ${facts.defaultShell}`}.
                  </p>
                ) : (
                  <ul class="target-list">
                    {facts.launchTargets.map((t) => (
                      <li key={t.name} class="target-row">
                        <span class="row-tile">{t.icon === '' ? '❯' : t.icon}</span>
                        <span class="row-main">
                          <span class="row-name">{t.name}</span>
                          {/* What will actually run, resolved by the machine
                              that owns it. Empty means its default shell,
                              which it has already told us the name of. */}
                          <span class="row-sub">
                            {t.command === '' ? facts.defaultShell : t.command}
                            {t.startingDirectory === '' ? '' : ` · ${t.startingDirectory}`}
                          </span>
                        </span>
                        <button
                          class="button subtle"
                          onClick={() =>
                            ctx.props.onLaunch?.({
                              hostId: h.id,
                              command: t.command,
                              cwd: t.startingDirectory,
                            })
                          }
                        >
                          Run
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </section>
            );
          })
        )}
      </div>
    );
  };
});
