/**
 * The sign-in screen.
 *
 * A link, not a `fetch`. `/auth/login` answers with a 302 to GitHub, and the
 * whole flow depends on the browser *navigating* — an XHR would follow the
 * redirect itself, land on github.com, and fail CORS having achieved nothing.
 */

import { component } from 'sigx';

export const Login = component<{ failed: boolean }>((ctx) => () => (
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

      <a class="button primary" href="/auth/login?provider=github&amp;next=/hosts">
        Continue with GitHub
      </a>

      <p class="fineprint">
        zesterm reads your name, avatar and verified email address, and nothing else. It never
        asks for repository access.
      </p>
    </div>
  </div>
));
