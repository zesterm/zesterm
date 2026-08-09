/**
 * The browser's device identity.
 *
 * The seed persists in localStorage so a reload is the same device — without
 * this, every refresh is a brand-new client id and a LAN host would show the
 * approval prompt on every F5, which teaches people to approve without
 * reading. Stated honestly: localStorage is readable by any script on this
 * origin. The real answer is M4's non-extractable WebCrypto key with
 * enrollment; this is the v1 stopgap the phone design doc names as such.
 */

import { generateIdentity, type ClientIdentity } from '@zesterm/auth';

const KEY = 'zesterm.device-seed.v1';

export function deviceIdentity(): ClientIdentity {
  const stored = localStorage.getItem(KEY);
  if (stored !== null) {
    try {
      return generateIdentity(stored);
    } catch {
      // A corrupt seed is a fresh device, not a broken app.
    }
  }
  const identity = generateIdentity();
  localStorage.setItem(KEY, identity.seed);
  return identity;
}
