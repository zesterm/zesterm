/**
 * The account's registry, as the browser sees it.
 *
 * Two lists, because they answer different questions. `hosts` is *your
 * machines* — the thing the fleet screen is about. `devices` is *what can
 * reach them*: browsers, phones, desktop clients, none of which serve sessions
 * and none of which will ever appear in a fleet listing. A browser holds a key
 * that can attach to every machine you own, so it needs somewhere to be seen
 * and somewhere to be revoked; that is the only reason the second list exists.
 *
 * Shapes mirror the Worker's `PublicHost` / `PublicDevice`. Parsed field by
 * field rather than cast: this is a server response about to be rendered, and
 * a missing `label` should be a row that does not appear rather than a row
 * reading `undefined`.
 */

import { ATTESTATION_TTL_MS, attestDevice, signRegistration, type ClientSigner } from '@zesterm/auth';

export interface Host {
  readonly id: string;
  readonly label: string;
  readonly platform: string;
  readonly enrolledAt: number;
  readonly lastSeenAt: number | null;
}

export type DeviceKind = 'browser' | 'phone' | 'desktop';

export type DeviceStatus = 'pending' | 'approved';

export interface Device {
  readonly id: string;
  readonly label: string;
  readonly kind: DeviceKind;
  /**
   * True while the private key is a seed any script on the origin can read.
   *
   * Surfaced rather than hidden: a browser on the fallback path is *working*,
   * not secure, and a screen that shows every device alike is telling a
   * comfortable lie about the one that matters.
   */
  readonly extractable: boolean;
  /**
   * `pending` is a registered key awaiting approval — listed and revocable,
   * but not yet a credential anywhere. Shown, because a pending device that
   * looks approved is one whose owner never learns it is waiting.
   */
  readonly status: DeviceStatus;
  readonly enrolledAt: number;
  readonly lastSeenAt: number | null;
}

const KINDS: readonly string[] = ['browser', 'phone', 'desktop'];

function str(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null;
}

function millis(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

export function parseHost(value: unknown): Host | null {
  if (typeof value !== 'object' || value === null) return null;
  const h = value as Record<string, unknown>;
  const id = str(h['id']);
  const label = str(h['label']);
  const enrolledAt = millis(h['enrolledAt']);
  if (id === null || label === null || enrolledAt === null) return null;
  return {
    id,
    label,
    platform: typeof h['platform'] === 'string' ? h['platform'] : '',
    enrolledAt,
    lastSeenAt: millis(h['lastSeenAt']),
  };
}

export function parseDevice(value: unknown): Device | null {
  if (typeof value !== 'object' || value === null) return null;
  const d = value as Record<string, unknown>;
  const id = str(d['id']);
  const label = str(d['label']);
  const kind = str(d['kind']);
  const enrolledAt = millis(d['enrolledAt']);
  if (id === null || label === null || enrolledAt === null) return null;
  if (kind === null || !KINDS.includes(kind)) return null;
  return {
    id,
    label,
    kind: kind as DeviceKind,
    // Absent reads as extractable, which is the cautious direction: the screen
    // then over-warns rather than quietly promising a key is safe.
    extractable: d['extractable'] !== false,
    // Absent reads as approved — backward tolerance for a Worker deployed
    // before the column existed, whose rows are all grandfathered approved by
    // the migration anyway. The cautious direction is inverted from
    // `extractable` on purpose: a false "pending" banner tells the owner
    // their working browser is locked out, which is the scarier lie here.
    status: d['status'] === 'pending' ? 'pending' : 'approved',
    enrolledAt,
    lastSeenAt: millis(d['lastSeenAt']),
  };
}

async function getJson(path: string, fetchImpl: typeof fetch): Promise<unknown> {
  const res = await fetchImpl(path, {
    headers: { accept: 'application/json' },
    credentials: 'same-origin',
  });
  if (!res.ok) throw new Error(`${path} answered ${res.status}`);
  return res.json();
}

export interface Registry {
  readonly hosts: readonly Host[];
  readonly devices: readonly Device[];
}

/** Both lists, in parallel. Rejects if either does, so the screen can say so. */
export async function fetchRegistry(fetchImpl: typeof fetch = fetch): Promise<Registry> {
  const [h, d] = await Promise.all([
    getJson('/api/hosts', fetchImpl),
    getJson('/api/devices', fetchImpl),
  ]);
  return {
    hosts: ((h as { hosts?: unknown[] }).hosts ?? []).map(parseHost).filter((x): x is Host => x !== null),
    devices: ((d as { devices?: unknown[] }).devices ?? [])
      .map(parseDevice)
      .filter((x): x is Device => x !== null),
  };
}

/**
 * Revoke a key.
 *
 * `POST` with a JSON content-type because that is what the Worker's CSRF rule
 * requires — a link or a form is refused 403, and that refusal is the rule
 * working.
 */
export async function revoke(
  table: 'hosts' | 'devices',
  id: string,
  fetchImpl: typeof fetch = fetch,
): Promise<void> {
  const res = await fetchImpl(`/api/${table}/${encodeURIComponent(id)}/revoke`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    credentials: 'same-origin',
  });
  if (!res.ok) throw new Error(`revoke answered ${res.status}`);
}

/**
 * Mint an enrolment code (10-minute TTL) for a new machine or device.
 *
 * Same posture as `revoke`: a JSON `POST` with the cookie, because the
 * Worker's CSRF rule refuses anything else 403. Parsed field by field for the
 * same reason the registry is — this answer is about to be shown to a person
 * who will type it somewhere, and a panel reading `undefined` teaches them to
 * type `undefined`.
 */
export async function mintEnrollCode(
  kind: 'host' | 'device',
  fetchImpl: typeof fetch = fetch,
): Promise<{ code: string; expiresAt: number }> {
  const res = await fetchImpl('/api/enroll/code', {
    method: 'POST',
    // `accept` as well as the CSRF content-type: this call reads a JSON body
    // back, so it states what it can take, exactly as `getJson` does.
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({ kind }),
  });
  if (!res.ok) throw new Error(`enroll code answered ${res.status}`);
  const body = (await res.json()) as Record<string, unknown> | null;
  const code = str(body?.['code']);
  const expiresAt = millis(body?.['expiresAt']);
  if (code === null || expiresAt === null) {
    throw new Error('enroll code answered the wrong shape');
  }
  return { code, expiresAt };
}

/**
 * Register this browser's own key with the account, as a `pending` device
 * (an account's first device is born approved — the Worker's bootstrap rule).
 *
 * Same CSRF posture as `revoke` and `mintEnrollCode`. The body carries an
 * Ed25519 signature over `(account, key, label)` — see `@zesterm/auth`'s
 * `register.ts` — so the Worker learns the caller *holds* the key it names,
 * and a captured request replays under no other account.
 *
 * `account` is the bootstrap answer's `user.id`, passed in rather than
 * re-fetched: the caller already holds the bootstrap, and a second fetch here
 * would be a second chance for the two to name different users.
 */
export async function registerDevice(
  args: {
    readonly signer: ClientSigner;
    readonly account: string;
    readonly label: string;
    /** From the device key's kind: a seed is readable by scripts, WebCrypto is not. */
    readonly extractable: boolean;
  },
  fetchImpl: typeof fetch = fetch,
): Promise<Device> {
  const { signer, account, label, extractable } = args;
  const sig = await signRegistration(signer, account, label);
  const res = await fetchImpl('/api/devices/register', {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({ deviceId: signer.clientId, label, kind: 'browser', extractable, sig }),
  });
  if (!res.ok) throw new Error(`device register answered ${res.status}`);
  const device = parseDevice(((await res.json()) as { device?: unknown }).device);
  if (device === null) throw new Error('device register answered the wrong shape');
  return device;
}

/**
 * Vouch for a device: sign an attestation as this browser and hand it to the
 * approve route. Approves a `pending` row; on an already-approved one it is
 * an idempotent renewal (the Worker replaces this approver's earlier
 * statement rather than accumulating).
 *
 * Same CSRF posture as every other cookie write here. The window starts at
 * `now` and runs the auth package's `ATTESTATION_TTL_MS` — the caller
 * supplies the clock so the function stays testable against a fixed one.
 */
export async function approveDevice(
  args: {
    readonly signer: ClientSigner;
    readonly account: string;
    /** The device being vouched for — its id and its current label, both signed. */
    readonly device: Pick<Device, 'id' | 'label'>;
    readonly now: number;
  },
  fetchImpl: typeof fetch = fetch,
): Promise<Device> {
  const { signer, account, device, now } = args;
  const blob = await attestDevice(signer, {
    account,
    device: device.id,
    label: device.label,
    iat: now,
    exp: now + ATTESTATION_TTL_MS,
  });
  const res = await fetchImpl(`/api/devices/${encodeURIComponent(device.id)}/approve`, {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({ attestation: blob }),
  });
  if (!res.ok) throw new Error(`device approve answered ${res.status}`);
  const approved = parseDevice(((await res.json()) as { device?: unknown }).device);
  if (approved === null) throw new Error('device approve answered the wrong shape');
  return approved;
}
