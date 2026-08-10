/**
 * Which key backs the device, and — the part with teeth — when it may change.
 *
 * The storage seams are injected so this runs under `node --test`: Node has
 * no IndexedDB and no `localStorage`, but it *does* have `crypto.subtle`
 * Ed25519, so the WebCrypto path exercised here is the real one — real
 * non-extractable keys, real signatures, verified by `@noble`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { generateIdentity, verifyClientSignature } from '@zesterm/auth';

import {
  deviceKey,
  describeDeviceKey,
  type DeviceKeyStore,
  type StoredDeviceKey,
} from '../src/device-key.ts';

const SEED_KEY = 'zesterm.device-seed.v1';

/** `localStorage`, as much of it as this module touches. */
function fakeSeeds(initial?: string): Pick<Storage, 'getItem' | 'setItem'> & { value: string | null } {
  return {
    value: initial ?? null,
    getItem(key: string): string | null {
      return key === SEED_KEY ? this.value : null;
    },
    setItem(key: string, value: string): void {
      if (key === SEED_KEY) this.value = value;
    },
  };
}

/** IndexedDB, reduced to the one record this module keeps in it. */
function fakeStore(): DeviceKeyStore & { record: StoredDeviceKey | null; saves: number } {
  return {
    record: null,
    saves: 0,
    load(): Promise<StoredDeviceKey | null> {
      return Promise.resolve(this.record);
    },
    save(key: StoredDeviceKey): Promise<void> {
      this.record = key;
      this.saves += 1;
      return Promise.resolve();
    },
  };
}

test('a device that already has a seed keeps it, and gets no WebCrypto key', async () => {
  // The whole reason this file is careful. Rotating silently changes the
  // ClientId, and every daemon in the fleet then shows its pairing prompt to
  // a device the person already approved — which is how people learn to
  // approve prompts without reading them.
  const existing = generateIdentity();
  const seeds = fakeSeeds(existing.seed);
  const store = fakeStore();

  const device = await deviceKey({ seeds, store, ed25519Available: () => Promise.resolve(true) });

  assert.equal(device.kind, 'seed', 'an established device is not upgraded behind its back');
  assert.equal(device.signer.clientId, existing.clientId, 'the id must survive the visit');
  assert.equal(seeds.value, existing.seed, 'the stored seed is untouched');
  assert.equal(store.saves, 0, 'no key was minted that would have replaced the old one');
});

test('a fresh device on a capable browser gets a non-extractable key, once', async () => {
  const seeds = fakeSeeds();
  const store = fakeStore();

  const first = await deviceKey({ seeds, store });
  assert.equal(first.kind, 'webcrypto');
  assert.equal(seeds.value, null, 'the WebCrypto path must never write a seed to localStorage');
  assert.equal(store.record?.privateKey.extractable, false, 'a readable device key is the bug');

  // The second visit is the one that matters: the same id, and nothing new
  // stored. A reload that mints a key is a pairing prompt per reload.
  const second = await deviceKey({ seeds, store });
  assert.equal(second.kind, 'webcrypto');
  assert.equal(second.signer.clientId, first.signer.clientId, 'a reload is the same device');
  assert.equal(store.saves, 1, 'exactly one key was ever generated');
});

test('the id a stored key claims is the key that actually signs', async () => {
  // A signer built from the wrong half of a pair, or from a mismatched id,
  // fails as `BadSignature` at the host — a refusal that names nothing. This
  // is the assertion that would have caught it here instead.
  const device = await deviceKey({ seeds: fakeSeeds(), store: fakeStore() });
  const message = Uint8Array.of(1, 2, 3, 4);
  const signature = await device.signer.sign('auth', message);
  assert.ok(
    verifyClientSignature(device.signer.clientId, 'auth', message, signature),
    'the stored id must verify what the stored key signs',
  );
});

test('a browser without Ed25519 falls back to the seed rather than to nothing', async () => {
  // Not detecting is worse than falling back: the failure mode is a device
  // that cannot sign at all, which looks exactly like a daemon refusing it.
  const seeds = fakeSeeds();
  const store = fakeStore();

  const device = await deviceKey({ seeds, store, ed25519Available: () => Promise.resolve(false) });

  assert.equal(device.kind, 'seed');
  assert.notEqual(seeds.value, null, 'the fallback persists, or every reload is a new device');
  assert.equal(store.saves, 0);

  const again = await deviceKey({ seeds, store, ed25519Available: () => Promise.resolve(false) });
  assert.equal(again.signer.clientId, device.signer.clientId, 'the fallback is stable too');
});

test('a storage layer that refuses still yields a usable device, without persisting it', async () => {
  // Private windows have historically had IndexedDB present and throwing, so a
  // working device still has to come out of this.
  //
  // What changed is the last assertion. This used to require that a seed was
  // *written*, on the reasoning that "there is nothing stored" — but a
  // rejected read cannot tell "nothing is stored" from "cannot read what is",
  // and persisting made the wrong guess permanent. So the device is usable and
  // deliberately ephemeral: it pairs again, and the real key stays findable.
  const seeds = fakeSeeds();
  const store: DeviceKeyStore = {
    load: () => Promise.reject(new Error('IndexedDB is not available')),
    save: () => Promise.reject(new Error('IndexedDB is not available')),
  };

  const device = await deviceKey({ seeds, store });
  assert.equal(device.kind, 'seed', 'the seed path is the one that always works');
  assert.equal(device.ephemeral, true, 'and it says so');
  assert.equal(seeds.value, null, 'nothing is written over a key that might exist');
});

test('a corrupt seed is a fresh device, not a broken app', async () => {
  const seeds = fakeSeeds('not hex at all');
  const device = await deviceKey({ seeds, store: fakeStore() });
  assert.equal(device.kind, 'webcrypto', 'a seed that cannot parse names no identity to keep');
});

test('the two kinds are described differently', () => {
  // The point of returning the kind at all: a seed-backed device must not be
  // shown whatever reassurance a WebCrypto-backed one is shown.
  assert.notEqual(describeDeviceKey('webcrypto'), describeDeviceKey('seed'));
});

test('a transient read failure does not permanently downgrade a real key', async () => {
  // The bug: one rejected `load()` minted a seed AND persisted it, after which
  // the seed check at the top of deviceKey() won forever. A single IndexedDB
  // hiccup gave the device a new ClientId -- every daemon in the fleet
  // re-prompts -- and silently downgraded it from a non-extractable key to a
  // page-readable one. Exactly what the module doc exists to prevent.
  const seeds = fakeSeeds();
  let record: unknown = null;
  let failNextLoad = false;
  const store = {
    load: () => (failNextLoad ? Promise.reject(new Error('hiccup')) : Promise.resolve(record)),
    save: (k: unknown) => {
      record = k;
      return Promise.resolve();
    },
  } as never;

  const first = await deviceKey({ seeds, store });
  assert.equal(first.kind, 'webcrypto', 'a healthy first visit stores a real key');

  failNextLoad = true;
  const during = await deviceKey({ seeds, store });
  assert.equal(during.kind, 'seed', 'an unreadable store still yields a usable device');
  assert.equal(during.ephemeral, true, 'and says the identity was not persisted');
  assert.equal(seeds.getItem('zesterm.device-seed.v1'), null, 'nothing was written over the key');

  failNextLoad = false;
  const after = await deviceKey({ seeds, store });
  assert.equal(after.kind, 'webcrypto', 'the real key is found again once the store recovers');
  assert.equal(after.signer.clientId, first.signer.clientId, 'and it is the same device');
});
