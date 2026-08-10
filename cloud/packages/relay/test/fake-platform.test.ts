/**
 * The fake platform's own tests.
 *
 * A fake nobody exercises is a fake that quietly stops resembling the thing it
 * stands in for, and this one exists precisely to fail where the platform
 * fails. So each cap is asserted from both sides — accepted just under,
 * refused just over — and the snapshot semantics are asserted by the mutation
 * that would expose a shared reference.
 *
 * The boundary values are not arithmetic on `MAX_ATTACHMENT_BYTES`; they are
 * the payloads that were run against real workerd. See `src/room/limits.ts`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { MAX_ATTACHMENT_BYTES, MAX_TAGS_PER_SOCKET, MAX_TAG_LENGTH } from '../src/room/limits.ts';
import { FakePlatform, FakeSock } from './fake-platform.ts';

/** A string that serializes to exactly `MAX_ATTACHMENT_BYTES`. Measured, not derived. */
const LARGEST_ACCEPTED_STRING = 'a'.repeat(16_379);
/** One character more, which is 16,385 bytes and is refused by workerd. */
const SMALLEST_REFUSED_STRING = 'a'.repeat(16_380);

test('the tag caps are the platform’s, from both sides', () => {
  const platform = new FakePlatform();
  const state = platform.state();

  const ten = Array.from({ length: MAX_TAGS_PER_SOCKET }, (_, i) => `t${i}`);
  state.acceptWebSocket(new FakeSock('ten'), ten);

  assert.throws(
    () => state.acceptWebSocket(new FakeSock('eleven'), [...ten, 'one-too-many']),
    /cannot have more than 10 tags/,
    'a fake that accepts 11 tags reports success on a room that cannot accept a socket at the edge',
  );

  state.acceptWebSocket(new FakeSock('long'), ['x'.repeat(MAX_TAG_LENGTH)]);
  assert.throws(
    () => state.acceptWebSocket(new FakeSock('longer'), ['x'.repeat(MAX_TAG_LENGTH + 1)]),
    /longer than the max tag length/,
    'tags are how a pipe is addressed, so a silently-truncated one pairs the wrong two sockets',
  );
});

test('duplicate tags count once, as workerd counts them', () => {
  const platform = new FakePlatform();
  const ws = new FakeSock();
  const tags = Array.from({ length: MAX_TAGS_PER_SOCKET + 4 }, () => 'pipe:same');

  platform.state().acceptWebSocket(ws, tags);
  assert.deepEqual(
    platform.state().getTags(ws),
    ['pipe:same'],
    'the cap is on distinct tags — counting repeats would refuse a socket the platform accepts',
  );
});

test('the attachment cap is on the serialized size, not the value’s length', () => {
  const ws = new FakeSock();

  ws.serializeAttachment(LARGEST_ACCEPTED_STRING);
  assert.equal(ws.deserializeAttachment(), LARGEST_ACCEPTED_STRING);

  assert.throws(
    () => ws.serializeAttachment(SMALLEST_REFUSED_STRING),
    /cannot be larger than 16384 bytes/,
    'sizing by string length instead of by serialization would accept ~6 bytes more than workerd does',
  );

  // The other half of the same point: the refused string is *shorter* than the
  // cap in characters, which is why the naive check passes it.
  assert.ok(
    SMALLEST_REFUSED_STRING.length < MAX_ATTACHMENT_BYTES,
    'the refused payload must be under the cap in characters, or it proves nothing about serialization overhead',
  );
});

test('an attachment is a snapshot, and every read is a fresh one', () => {
  const ws = new FakeSock();
  const value = { role: 'host', pipes: ['a'] };

  ws.serializeAttachment(value);
  value.pipes.push('b');

  assert.deepEqual(
    ws.deserializeAttachment(),
    { role: 'host', pipes: ['a'] },
    'the platform stores serialized bytes, so a room that mutates the value it attached loses the mutation in production',
  );

  const first = ws.deserializeAttachment() as { pipes: string[] };
  first.pipes.push('c');
  assert.deepEqual(
    ws.deserializeAttachment(),
    { role: 'host', pipes: ['a'] },
    'each deserialize is a fresh object, so mutating one read must not be visible to the next',
  );
});

test('writes are counted, on storage and on attachments', async () => {
  const platform = new FakePlatform();
  const ws = new FakeSock();
  platform.state().acceptWebSocket(ws, ['pipe:1']);

  assert.equal(platform.storage.writes, 0);
  assert.equal(platform.attachmentWrites, 0);

  await platform.storage.put('ticket:abc', { seenAt: 1 });
  ws.serializeAttachment({ role: 'host' });
  ws.serializeAttachment({ role: 'host', pipe: '1' });

  assert.equal(platform.storage.writes, 1, 'the data path must never write storage — ADR-009');
  assert.equal(platform.attachmentWrites, 2);

  await platform.storage.delete('ticket:abc');
  assert.equal(
    platform.storage.writes,
    2,
    'a delete is a write — it wakes the object and bills the same as a put, so an expiry sweep on the data path is the same mistake',
  );
});

test('storage hands back copies', async () => {
  const platform = new FakePlatform();
  const stored = { tickets: ['a'] };

  await platform.storage.put('k', stored);
  stored.tickets.push('b');
  assert.deepEqual(
    await platform.storage.get('k'),
    { tickets: ['a'] },
    'D1-backed object storage serializes on put; sharing the reference would hide a room that mutates what it stored',
  );

  const read = (await platform.storage.get<{ tickets: string[] }>('k'))!;
  read.tickets.push('c');
  assert.deepEqual(await platform.storage.get('k'), { tickets: ['a'] });
});

test('eviction discards the instance and keeps the sockets, tags and storage', async () => {
  const platform = new FakePlatform();
  const host = new FakeSock('host');
  const browser = new FakeSock('browser');

  platform.state().acceptWebSocket(host, ['pipe:1', 'role:host']);
  platform.state().acceptWebSocket(browser, ['pipe:1', 'role:browser']);
  host.serializeAttachment({ pipe: '1' });
  await platform.storage.put('ticket:abc', { seenAt: 1 });

  // A brand-new state, as the runtime builds after an idle gap. Nothing was
  // carried over except what the platform itself holds — which is exactly the
  // property a room keeping its pairing in an instance field would fail.
  const afterEviction = platform.state();

  assert.deepEqual(
    afterEviction.getWebSockets('pipe:1'),
    [host, browser],
    'the pairing must be re-derivable from tags alone after the object is thrown away',
  );
  assert.deepEqual(afterEviction.getTags(host), ['pipe:1', 'role:host']);
  assert.deepEqual(host.deserializeAttachment(), { pipe: '1' });
  assert.deepEqual(await afterEviction.storage.get('ticket:abc'), { seenAt: 1 });
  assert.equal(platform.states, 3, 'each state() is one simulated eviction');
});

test('tags cannot be read from a socket that was never accepted', () => {
  const platform = new FakePlatform();
  assert.throws(
    () => platform.state().getTags(new FakeSock()),
    /must call 'acceptWebSocket\(\)'/,
    'only hibernatable sockets have tags, and reaching for them on a plain one is a real mistake',
  );
});

test('a closed socket refuses further sends', () => {
  const ws = new FakeSock('gone');
  ws.close(1000, 'done');
  assert.throws(() => ws.send('hello'), /send after close/);
  assert.deepEqual(ws.closed, { code: 1000, reason: 'done' });
});
