/**
 * The sign-in screen.
 *
 * A link, not a `fetch`. `/auth/login` answers with a 302 to GitHub, and the
 * whole flow depends on the browser *navigating* — an XHR would follow the
 * redirect itself, land on github.com, and fail CORS having achieved nothing.
 */

import { component } from 'sigx';

/**
 * `next` is where the OAuth round trip should land — already vetted by the
 * route's `safeNextPath`, encoded here because it rides inside a query value
 * of the login URL. The Worker's `safeNext` vets it once more on the far
 * side; neither trusts the other to have done it.
 */
export const Login = component<{ failed: boolean; next: string }>((ctx) => () => (
  <div class="shell centered">
    <div class="card">
      <h1>zesterm</h1>
      <p class="muted">Sign in to reach your machines from anywhere.</p>

      {ctx.props.failed ? (
        <p class="error" role="alert">
          That sign-in did not complete. It usually means the attempt took too long — starting
          again is the fix.
        </p>
      ) : null}

      <a
        class="button primary"
        href={`/auth/login?provider=github&next=${encodeURIComponent(ctx.props.next)}`}
      >
        Continue with GitHub
      </a>

      <p class="fineprint">
        zesterm reads your name, avatar and verified email address, and nothing else. It never
        asks for repository access.
      </p>
    </div>
  </div>
));
