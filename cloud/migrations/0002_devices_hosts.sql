-- What an account owns: the public keys of the machines and browsers that are
-- yours.
--
-- ADR-006: identities are public keys, not random ids. "Is this really my Mac"
-- has to be answerable by asking it to sign a nonce, and a UUID is equally
-- unique while proving nothing. So `id` here IS the 64-hex Ed25519 public key
-- for both tables -- there is no separate opaque id that can drift out of sync
-- with the key, and enrolment is a signature rather than a claim.
--
-- The account therefore does not *name* devices. It makes signed statements
-- about them, and this is where those statements are kept.

PRAGMA foreign_keys = ON;

-- A machine running a daemon.
CREATE TABLE hosts (
  id           TEXT    PRIMARY KEY,        -- HostId: 64 hex, the Ed25519 public key
  user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  label        TEXT    NOT NULL,           -- what a person calls it; the only mutable identity
  platform     TEXT    NOT NULL DEFAULT '',
  -- Reserved for the relay: a Durable Object's placement is decided by the id
  -- it is first addressed with, and per-host naming should bias toward the
  -- machine rather than the roaming client. Added now because adding a column
  -- later means a migration during an outage.
  do_id        TEXT,
  enrolled_at  INTEGER NOT NULL,
  last_seen_at INTEGER,
  -- Revocation is a positive statement, not a deletion: the row stays so a
  -- later "why can this not connect" has an answer, and so a revoked key
  -- cannot be silently re-enrolled as though it were new.
  revoked_at   INTEGER
);
CREATE INDEX hosts_user ON hosts(user_id) WHERE revoked_at IS NULL;

-- A browser, phone or desktop app that attaches to one.
CREATE TABLE devices (
  id           TEXT    PRIMARY KEY,        -- ClientId: 64 hex, the Ed25519 public key
  user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  label        TEXT    NOT NULL,
  kind         TEXT    NOT NULL,           -- 'browser' | 'phone' | 'desktop'
  -- 1 while the private key is a localStorage seed any script on the origin
  -- can read; 0 once a non-extractable WebCrypto key backs it. Recorded rather
  -- than assumed so the devices screen can say which, instead of showing a
  -- green tick that is not true.
  extractable  INTEGER NOT NULL DEFAULT 1,
  enrolled_at  INTEGER NOT NULL,
  last_seen_at INTEGER,
  revoked_at   INTEGER
);
CREATE INDEX devices_user ON devices(user_id) WHERE revoked_at IS NULL;

-- One-shot codes, carried from a signed-in browser to `zest-daemon --enroll`.
--
-- The code is what a *person* moves between two places they control, and the
-- signature the daemon returns over it is what proves the far end holds the
-- key it claims. Neither alone is enough: a code without a signature enrols
-- whoever intercepted it, and a signature without a code enrols a key nobody
-- approved.
CREATE TABLE enroll_codes (
  code       TEXT    PRIMARY KEY,          -- short, unambiguous alphabet, single use
  user_id    TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  kind       TEXT    NOT NULL,             -- 'host' | 'device'
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  -- Set the moment it is spent. Checked rather than deleted, so a replay is
  -- distinguishable from a code that never existed -- which is the difference
  -- between "you already used this" and "that is not a code".
  used_at    INTEGER,
  used_by    TEXT
);
CREATE INDEX enroll_codes_user ON enroll_codes(user_id, expires_at);
