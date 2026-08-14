/**
 * The host offer (#262): what a machine says it is and what it can launch.
 *
 * The type-level half lives in `bindings-match.test.ts`, which holds these
 * shapes to the generated Rust ones. This is the runtime half, and it is about
 * one property: **every field but `name` is `#[serde(default)]` on the host**,
 * so a peer may legitimately omit any of them, and the parser has to produce a
 * decoded value rather than throw. A launcher that crashed on a daemon which
 * happened to leave `os_version` empty would be worse than one that showed no
 * version at all.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { isSessions, parseHostMessage, parseHostOffer } from '../src/wire.ts';

test('an offer with only the required fields parses to empties, not to a throw', () => {
  // What a host with no profiles and no readable OS version sends. Every
  // absent key has a decoded form; none of them is a reason to fail a message
  // that also carries the session list.
  const offer = parseHostOffer({ os: 'windows', arch: 'x86_64' });
  assert.equal(offer.os, 'windows');
  assert.equal(offer.arch, 'x86_64');
  assert.equal(offer.os_version, '', 'unknown is empty, never a placeholder');
  assert.equal(offer.default_shell, '');
  assert.deepEqual(offer.profiles, []);
});

test('a profile with only a name parses, and its command means "the far default shell"', () => {
  // Empty `command` is the wire's word for "this host's own shell" — the same
  // convention `create_session.command` uses, so a launch passes it straight
  // through instead of substituting the *client's* shell and asking a Mac to
  // run pwsh.
  const offer = parseHostOffer({ os: 'linux', arch: 'aarch64', profiles: [{ name: 'plain' }] });
  const [profile] = offer.profiles;
  assert.equal(profile?.name, 'plain');
  assert.equal(profile?.command, '');
  assert.equal(profile?.tab_color, null, 'no colour chosen is null, not 0 — 0 is a real accent');
});

test('a full offer round-trips every field a launcher renders', () => {
  const offer = parseHostOffer({
    os: 'macos',
    arch: 'aarch64',
    os_version: '24.5.0',
    default_shell: 'zsh -l',
    profiles: [
      {
        name: 'ubuntu',
        command: 'wsl.exe -d Ubuntu-24.04',
        starting_directory: '\\\\wsl$\\Ubuntu-24.04\\home\\andy',
        icon: 'star',
        color_scheme: 'nord',
        tab_color: 3,
      },
    ],
  });
  assert.equal(offer.os_version, '24.5.0');
  assert.equal(offer.default_shell, 'zsh -l');
  assert.deepEqual(offer.profiles[0], {
    name: 'ubuntu',
    command: 'wsl.exe -d Ubuntu-24.04',
    // A path this machine has never heard of, and that is expected — it names
    // a filesystem on the far host.
    starting_directory: '\\\\wsl$\\Ubuntu-24.04\\home\\andy',
    icon: 'star',
    color_scheme: 'nord',
    tab_color: 3,
  });
});

test('a sessions message without an offer parses, exactly as a pre-#262 daemon sends it', () => {
  // The compatibility this whole design rests on. A new `HostMessage` tag
  // would not merely go unread on an older client — the Rust client maps an
  // undecodable frame to a transport error and drops the connection — so the
  // field had to be additive, and this is the assertion that says it is.
  const msg = parseHostMessage({ t: 'sessions', sessions: [] });
  assert.ok(isSessions(msg));
  assert.equal(
    msg.offer,
    null,
    'no offer means "nothing new to say", which is the same branch a current ' +
      'daemon takes on an ordinary session push',
  );
});

test('a sessions message carrying an offer exposes it alongside the listing', () => {
  const msg = parseHostMessage({
    t: 'sessions',
    sessions: [],
    offer: { os: 'linux', arch: 'x86_64', profiles: [{ name: 'build' }] },
  });
  assert.ok(isSessions(msg));
  assert.equal(msg.offer?.os, 'linux');
  assert.equal(msg.offer?.profiles[0]?.name, 'build');
});

test('an explicit null offer reads as absent rather than as a parse failure', () => {
  // MessagePack and JSON disagree about how an absent optional looks once it
  // has been through a re-encoder, so both spellings have to mean the same
  // thing here.
  const msg = parseHostMessage({ t: 'sessions', sessions: [], offer: null });
  assert.ok(isSessions(msg));
  assert.equal(msg.offer, null);
});
