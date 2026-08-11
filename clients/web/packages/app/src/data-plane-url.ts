/**
 * The one place a `DataPlane` becomes a URL.
 *
 * It lives beside the session list rather than inside it so it can be tested
 * without a DOM — the exhaustiveness guard below is the kind of thing that is
 * only ever exercised by the next person adding a variant, and a test is what
 * proves the `relay` arm still disables a row rather than dialling nonsense.
 */

import type { DataPlane } from '@zesterm/control';

/**
 * `null` means "not dialable from here", which every caller already handles:
 * it is the state a browser is in before the directory has learned the data
 * plane at all, and rows are disabled for it today.
 */
export function dataPlaneUrl(dataPlane: DataPlane | null): string | null {
  if (dataPlane === null) return null;
  switch (dataPlane.kind) {
    case 'ws':
      return `ws://${dataPlane.host}:${dataPlane.port}`;
    case 'relay':
      // Declared, not yet dialable: the relay `Dial` mints a ticket first, so
      // a relay plane never becomes a plain URL. Until that lands the row is
      // disabled, exactly as it is when no plane is known.
      return null;
    default: {
      // A third kind must not fall through to a default that silently dials
      // the wrong thing — this makes adding one a compile error here, which
      // is the only place that can answer the question.
      //
      // The binding is the guard; the *return* is deliberately `null` and not
      // `unreachable`. Returning the binding would hand a `DataPlane` object
      // back through a `string | null` signature, and every caller tests
      // `url === null` — an object passes that test, so the row would be
      // enabled and `wsDial` would get `new WebSocket("[object Object]")`.
      // A kind the compiler never saw can still arrive at runtime:
      // `SessionDirectory` is `allowAnonymous: true` and its write methods are
      // wire-callable, and a stale bundle can meet a newer sidecar. Unknown
      // means not dialable, which is what `null` already says.
      const unreachable: never = dataPlane;
      void unreachable;
      return null;
    }
  }
}
