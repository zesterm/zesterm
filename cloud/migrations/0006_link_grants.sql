-- Link grants: the browser hand-off sign-in (#226). The app proves it holds a
-- key, gets a grant id, opens the account's browser at /link?grant=<id>; the
-- signed-in person approves; the app claims the grant with a second signature
-- and is enrolled. RFC 8628's shape, minus the user-typed code.
--
-- `id` is base64url(32 random bytes) -- 43 characters. base64url rather than
-- hex because the id travels in a browser URL (`/link?grant=`), where hex is
-- half again as long for the same entropy; 32 bytes because the id is briefly
-- a capability (whoever opens the URL signed-in gets to approve), so it gets
-- the machine-token entropy rather than the eight-character typed-code kind,
-- which leans on rate and TTL a URL never pays.
--
-- The row is PRE-ACCOUNT: there is no user_id at creation, because the whole
-- point is that the account is decided by whoever approves. `approved_by_user`
-- is that decision. This is also the honest DoS surface of the one
-- unauthenticated insert this schema has: bounded by UNIQUE(device_id) -- one
-- live row per *proven* key, replaced on every fresh start (the machine-token
-- discipline: asking again rotates, never accumulates) -- plus the TTL. An
-- attacker minting keys can mint rows; each costs an Ed25519 keypair and dies
-- in ten minutes.
--
-- `claimed_at` is the single-use latch, spent by compare-and-set exactly as
-- enroll_codes.used_at is. Denial is deletion, not a status: a denied grant
-- has no history worth keeping -- the device it names was never enrolled, so
-- "why can this not attach" has nothing to answer -- and the deleted row frees
-- the key to ask again.

PRAGMA foreign_keys = ON;

CREATE TABLE link_grants (
  id               TEXT    PRIMARY KEY,
  -- ClientId: 64 hex, the key that asked. See hosts.id for the CHECK's why.
  device_id        TEXT    NOT NULL UNIQUE
                           CHECK (length(device_id) = 64 AND device_id NOT GLOB '*[^0-9a-f]*'),
  label            TEXT    NOT NULL,
  kind             TEXT    NOT NULL CHECK (kind IN ('browser', 'phone', 'desktop')),
  platform         TEXT    NOT NULL DEFAULT '',   -- shown on the approval page; never stored on the device row
  created_at       INTEGER NOT NULL,
  expires_at       INTEGER NOT NULL,
  approved_at      INTEGER,
  -- The account the approver signed the grant into. ON DELETE CASCADE: a
  -- deleted account must not leave grants that would enrol devices into it.
  approved_by_user TEXT    REFERENCES users(id) ON DELETE CASCADE,
  claimed_at       INTEGER
);
