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

test('a storage layer that refuses still yields a usable device', async () => {
  // Private windows have historically had IndexedDB present and throwing.
  // There is no identity to preserve in that case — there is nothing stored —
  // so the honest outcome is a working seed-backed device.
  const seeds = fakeSeeds();
  const store: DeviceKeyStore = {
    load: () => Promise.reject(new Error('IndexedDB is not available')),
    save: () => Promise.reject(new Error('IndexedDB is not available')),
  };

  const device = await deviceKey({ seeds, store });
  assert.equal(device.kind, 'seed');
  assert.notEqual(seeds.value, null);
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
