/**
 * An unknown path.
 *
 * Worth a real screen rather than a redirect: the Worker's asset fallback
 * serves `index.html` for every path, so a mistyped or stale link arrives at
 * the app looking exactly like a valid route. Silently showing the fleet would
 * make a broken link indistinguishable from a working one.
 */

import { component } from 'sigx';
import { Link } from '@sigx/router';

export const NotFound = component(() => () => (
  <div class="shell centered">
    <div class="card">
      <h1>Not found</h1>
      <p class="muted">There is nothing at this address.</p>
      <Link class="button primary" to="/hosts">
        Back to your machines
      </Link>
    </div>
  </div>
));
