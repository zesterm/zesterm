/**
 * The browser's device key: what this device signs with, and where it lives.
 *
 * Two shapes, and which one a device has is *not* a detail:
 *
 * - **`webcrypto`** — a non-extractable Ed25519 `CryptoKey` in IndexedDB. A
 *   script on this origin can ask it to sign and can never read it, so an XSS
 *   is bounded by how long the page is open rather than being a key theft that
 *   outlives it. A `CryptoKey` is structured-cloneable, which is exactly why
 *   IndexedDB and not `localStorage`: the latter stores strings, and a key
 *   that can be turned into a string is a key that can be stolen.
 * - **`seed`** — the v1 stopgap this file replaced: 32 bytes of hex in
 *   `localStorage`, readable by anything on the origin. Still the fallback,
 *   because a device that cannot use WebCrypto Ed25519 must still be able to
 *   reach its terminals.
 *
 * **A device already on the seed path stays on it.** Rotating to a WebCrypto
 * key would change the `ClientId`, and a new id means every daemon in the
 * fleet shows its pairing prompt again — for a device the person has already
 * approved, with no explanation. That is precisely how people are taught to
 * approve prompts without reading them — which is the very failure the
 * persisted seed was introduced to avoid. Migration needs enrolment (#41) to
 * carry the old key's blessing to the new one; until then, no rotation.
 *
 * The kind is returned rather than hidden because the UI has to be able to say
 * which one it is. A device on the seed path is not "secure, green tick" — it
 * is working, and readable.
 */

import { generateIdentity, seedSigner, type ClientSigner } from '@zesterm/auth';
import {
  generateWebCryptoKey,
  webCryptoEd25519Available,
  webCryptoSigner,
} from '@zesterm/auth/webcrypto';

/** The v1 seed's key, unchanged — this is the string that must keep meaning what it meant. */
const SEED_KEY = 'zesterm.device-seed.v1';

const DB_NAME = 'zesterm';
const DB_VERSION = 1;
const STORE = 'device-key';
const RECORD = 'v1';

export type DeviceKeyKind = 'webcrypto' | 'seed';

export interface DeviceKey {
  /**
   * True when this identity was minted because the store could not be *read*,
   * and so was deliberately not persisted. The device pairs again next visit,
   * and the real key — if there is one — is still recoverable.
   */
  readonly ephemeral?: boolean;
  readonly signer: ClientSigner;
  readonly kind: DeviceKeyKind;
}

/** What the WebCrypto path persists: the private key, and the id it proves. */
export interface StoredDeviceKey {
  readonly clientId: string;
  /** Non-extractable. Stored by structured clone; never serialised. */
  readonly privateKey: CryptoKey;
}

export interface DeviceKeyStore {
  load(): Promise<StoredDeviceKey | null>;
  save(key: StoredDeviceKey): Promise<void>;
}

/** The seams a test replaces; production takes all three defaults. */
export interface DeviceKeyEnv {
  readonly store: DeviceKeyStore;
  readonly seeds: Pick<Storage, 'getItem' | 'setItem'>;
  readonly ed25519Available: () => Promise<boolean>;
}

/**
 * The device key this browser should use, creating one on first visit.
 *
 * Async because both storage and key generation are: IndexedDB has no
 * synchronous read, and neither does `crypto.subtle.generateKey`.
 */
export async function deviceKey(env: Partial<DeviceKeyEnv> = {}): Promise<DeviceKey> {
  const seeds = env.seeds ?? localStorage;
  const store = env.store ?? indexedDbStore();
  const available = env.ed25519Available ?? webCryptoEd25519Available;

  // Checked first, and it wins: see the module doc on why an existing device
  // must not be quietly given a new identity.
  const seed = seeds.getItem(SEED_KEY);
  if (seed !== null) {
    try {
      return { signer: seedSigner(generateIdentity(seed)), kind: 'seed' };
    } catch {
      // A corrupt seed is a fresh device, not a broken app — and a device
      // whose stored seed does not parse has no identity to preserve.
    }
  }

  // Whether the read *failed*, as opposed to succeeding and finding nothing.
  //
  // The distinction is the whole of this function's correctness. A rejected
  // read means we do not know whether a key exists — and one bad read used to
  // mint a seed and persist it, after which the seed check at the top of this
  // function wins forever. A single transient IndexedDB hiccup therefore gave
  // the device a new ClientId (every daemon in the fleet re-prompts) *and*
  // silently downgraded it from a non-extractable key to a page-readable one.
  // That is exactly what the module doc exists to prevent, arriving through
  // the back door.
  let unreadable = false;
  let stored: StoredDeviceKey | null = null;
  try {
    stored = await store.load();
  } catch {
    unreadable = true;
  }

  if (stored !== null) {
    try {
      return { signer: webCryptoSigner(stored.clientId, stored.privateKey), kind: 'webcrypto' };
    } catch {
      // A record that will not build a signer is not a key we can use, but it
      // is also not nothing: writing over it is the same mistake as above.
      unreadable = true;
    }
  }

  if (!unreadable) {
    try {
      if (await available()) {
        const { clientId, keyPair } = await generateWebCryptoKey();
        await store.save({ clientId, privateKey: keyPair.privateKey });
        return { signer: webCryptoSigner(clientId, keyPair.privateKey), kind: 'webcrypto' };
      }
    } catch {
      // Generation or the write failed. The seed path below still works, and
      // the read succeeded, so persisting a seed overwrites nothing.
    }
  }

  const identity = generateIdentity();
  if (!unreadable) {
    // Persisted only when the read succeeded and genuinely found nothing.
    // After an unreadable store this device is deliberately EPHEMERAL: it
    // pairs again this session, which is a nuisance, but the next load can
    // still find the real key. Writing here would make the loss permanent.
    seeds.setItem(SEED_KEY, identity.seed);
  }
  return { signer: seedSigner(identity), kind: 'seed', ephemeral: unreadable };
}

/**
 * IndexedDB, wrapped in promises.
 *
 * Hand-rolled rather than reached for a library: this is one object store with
 * one record, and `packages/app`'s dependency list is somewhere the whole
 * workspace has been deliberate.
 */
export function indexedDbStore(): DeviceKeyStore {
  const open = (): Promise<IDBDatabase> =>
    new Promise((resolve, reject) => {
      const request = indexedDB.open(DB_NAME, DB_VERSION);
      request.onupgradeneeded = () => {
        if (!request.result.objectStoreNames.contains(STORE)) {
          request.result.createObjectStore(STORE);
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error ?? new Error('indexedDB.open failed'));
      // A blocked upgrade means another tab holds an older version open. It
      // resolves when that tab goes away, which may be never — failing here
      // beats an app that hangs before its first paint.
      request.onblocked = () => reject(new Error('another tab is holding the device key database'));
    });

  const transact = async <T>(
    mode: IDBTransactionMode,
    run: (store: IDBObjectStore) => IDBRequest<T>,
  ): Promise<T> => {
    const db = await open();
    try {
      return await new Promise<T>((resolve, reject) => {
        const tx = db.transaction(STORE, mode);
        const request = run(tx.objectStore(STORE));
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error ?? new Error('the device key store failed'));
      });
    } finally {
      db.close();
    }
  };

  return {
    async load(): Promise<StoredDeviceKey | null> {
      const found = await transact<StoredDeviceKey | undefined>('readonly', (s) => s.get(RECORD));
      return found ?? null;
    },
    save(key: StoredDeviceKey): Promise<void> {
      return transact('readwrite', (s) => s.put(key, RECORD)).then(() => undefined);
    },
  };
}

/** How to describe this device's key to a person. */
export function describeDeviceKey(kind: DeviceKeyKind): string {
  return kind === 'webcrypto'
    ? 'device key held by the browser, not readable by this page'
    : 'device key stored in this browser and readable by it';
}
