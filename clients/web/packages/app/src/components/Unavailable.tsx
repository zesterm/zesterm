/**
 * What the hosted app shows until it can reach a machine.
 *
 * Deployed, this build has no route to any daemon. Two independent reasons,
 * and it is worth stating both because fixing either alone changes nothing:
 *
 * 1. There is no sidecar at the edge, so nothing hosts the `SessionDirectory`
 *    actor the session list reads (ADR-005 — actors run on the user's machine,
 *    never at the edge).
 * 2. Mixed content: an `https://` page cannot open `ws://<lan-ip>:7718`. That
 *    is not a configuration problem, and it holds even for the machine the
 *    browser is sitting on.
 *
 * So the hosted client needs the relay before it can show anything real. Saying
 * that plainly beats rendering a session list that spins forever on a socket
 * which cannot connect — that failure looks like a broken daemon, and someone
 * would go and debug the daemon.
 */

import { component } from 'sigx';
import type { Theme } from '@zesterm/theme';

export const Unavailable = component<{ theme: Theme }>(() => () => (
  <div class="shell unavailable">
    <div class="unavailable-card">
      <h1>zesterm</h1>
      <p>
        This is the hosted client. It cannot reach any of your machines yet — the relay that
        carries terminal traffic between them is not built.
      </p>
      <p class="unavailable-note">
        Running a daemon on this machine? Use the local client at{' '}
        <code>http://127.0.0.1:7350</code>, which talks to it directly.
      </p>
    </div>
  </div>
));
