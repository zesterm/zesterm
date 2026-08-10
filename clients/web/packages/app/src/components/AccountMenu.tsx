/**
 * Who you are, and the way out.
 *
 * Sign-out is a **POST with `content-type: application/json`**, because that is
 * exactly what the Worker's CSRF rule requires: a form or a link would be
 * refused 403, and that refusal is the rule working rather than a bug. See
 * `cloud/packages/web/src/http.ts`.
 */

import { component, signal } from 'sigx';

import type { User } from '../bootstrap.ts';

export const AccountMenu = component<{ user: User }>((ctx) => {
  const state = signal({ busy: false, error: null as string | null });

  const signOut = (): void => {
    if (state.busy) return;
    state.busy = true;
    state.error = null;
    void fetch('/auth/logout', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      // The cookie is what is being destroyed, so it has to be sent.
      credentials: 'same-origin',
    })
      .then((res) => {
        if (!res.ok) throw new Error(`sign out failed (${res.status})`);
        // A full navigation rather than re-rendering: signing out should leave
        // nothing of the session behind in memory, and the cheapest way to be
        // sure of that is to start the app again with no cookie.
        location.assign('/login');
      })
      .catch((e: unknown) => {
        state.busy = false;
        state.error = e instanceof Error ? e.message : String(e);
      });
  };

  return () => (
    <div class="account">
      {ctx.props.user.avatarUrl !== null ? (
        <img class="avatar" src={ctx.props.user.avatarUrl} alt="" width="20" height="20" />
      ) : null}
      <span class="account-name">{ctx.props.user.displayName}</span>
      <button class="button subtle" onClick={signOut} disabled={state.busy}>
        {state.busy ? 'signing out…' : 'sign out'}
      </button>
      {state.error !== null ? <span class="error inline">{state.error}</span> : null}
    </div>
  );
});
