/**
 * The `/link` page: the desktop app opened this URL, and the person decides.
 *
 * Small on purpose — every decision that can be pure lives in `link.ts` with
 * tests, and this renders one `LinkPhase` at a time. The fingerprint line is
 * the anti-phishing half of the flow: the app is showing the same eight hex
 * characters while it polls, and the instruction to compare them is on the
 * page because a check nobody is told about is a check nobody makes.
 */

import { component, signal } from 'sigx';
import { useQuery } from '@sigx/router';

import type { Bootstrap } from '../bootstrap.ts';
import {
  approveLinkGrant,
  denyLinkGrant,
  fetchLinkGrant,
  fingerprintGroups,
  grantFromQuery,
  type LinkPhase,
} from '../link.ts';

export const LinkApprove = component<{ bootstrap: Bootstrap }>((ctx) => {
  const state = signal<{ at: LinkPhase }>({ at: { phase: 'loading' } });

  // Read once at setup: the grant id arrives in the opening URL and never
  // changes under a mounted page — a *different* grant is a fresh navigation.
  const grant = grantFromQuery(useQuery()['grant']);

  if (ctx.props.bootstrap.mode === 'local' || grant === null) {
    // The local client has no account to link into, and a mangled id has no
    // request worth making — both are "this link is not valid", now.
    state.at = { phase: 'invalid' };
  } else {
    fetchLinkGrant(grant)
      .then((details) => {
        state.at =
          details === null
            ? { phase: 'invalid' }
            : details.approved
              ? // Re-opened after approving: say so instead of offering a
                // button that would be a no-op.
                { phase: 'approved', grant: details }
              : { phase: 'ready', grant: details, busy: false };
      })
      .catch(() => {
        state.at = { phase: 'invalid' };
      });
  }

  // Callable only from the `ready` phase, which the invalid branch above can
  // never reach — so `grant` is non-null whenever this runs.
  const answer = (verb: 'approve' | 'deny'): void => {
    const at = state.at;
    if (at.phase !== 'ready' || at.busy || grant === null) return;
    state.at = { phase: 'ready', grant: at.grant, busy: true };
    (verb === 'approve' ? approveLinkGrant(grant) : denyLinkGrant(grant))
      .then(() => {
        state.at = verb === 'approve' ? { phase: 'approved', grant: at.grant } : { phase: 'denied' };
      })
      .catch(() => {
        // The grant died under the page — expired, or answered elsewhere.
        // "No longer valid" is the honest summary of every such failure.
        state.at = { phase: 'invalid' };
      });
  };

  return () => {
    const at = state.at;
    return (
      <div class="shell centered">
        <div class="card">
          <h1>Link a device</h1>

          {at.phase === 'loading' ? <p class="muted">Loading…</p> : null}

          {at.phase === 'invalid' ? (
            <p class="error" role="alert">
              This link is not valid any more. Links expire after ten minutes — ask the device to
              sign in again for a fresh one.
            </p>
          ) : null}

          {at.phase === 'ready' || at.phase === 'approved' ? (
            <>
              <p>
                <strong>{at.grant.label}</strong> ({at.grant.kind}
                {at.grant.platform !== '' ? ` · ${at.grant.platform}` : ''}) wants to sign in to
                your account.
              </p>
              <p class="mono">key {fingerprintGroups(at.grant.fingerprint)}</p>
              <p class="fineprint">
                The device is showing the same key. If the two do not match, deny — someone else is
                asking.
              </p>
            </>
          ) : null}

          {at.phase === 'ready' ? (
            <p>
              <button class="button primary" disabled={at.busy} onClick={() => answer('approve')}>
                Approve
              </button>{' '}
              <button class="button subtle" disabled={at.busy} onClick={() => answer('deny')}>
                Deny
              </button>
            </p>
          ) : null}

          {at.phase === 'approved' ? (
            <p class="muted" role="status">
              Approved. You can return to the device — it signs itself in from here.
            </p>
          ) : null}

          {at.phase === 'denied' ? (
            <p class="muted" role="status">
              Denied. The device was not signed in, and that link is now dead.
            </p>
          ) : null}
        </div>
      </div>
    );
  };
});
