-- Accounts: who you are, and how this browser proves it.
--
-- An account owns device public keys; it does not replace them (ADR-006 --
-- identities are public keys, not random ids). Those tables arrive with the
-- device registry; this file is only the person and the session.

PRAGMA foreign_keys = ON;

CREATE TABLE users (
  id            TEXT    PRIMARY KEY,          -- 128-bit hex, ours
  -- Display only. Identity lives in oauth_identities: an email can change, be
  -- reassigned, or be unverified, and none of those may move an account.
  email         TEXT,
  display_name  TEXT    NOT NULL DEFAULT '',
  avatar_url    TEXT,
  created_at    INTEGER NOT NULL,             -- epoch ms
  updated_at    INTEGER NOT NULL,
  disabled_at   INTEGER
);

-- The (provider, subject) pair IS the identity.
--
-- `subject` is the provider's immutable id -- GitHub's numeric user id, never
-- the login. Logins are renameable and reusable, so an account keyed on one
-- can be taken over by whoever claims the name next.
CREATE TABLE oauth_identities (
  provider       TEXT    NOT NULL,
  subject        TEXT    NOT NULL,
  user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  email          TEXT,
  -- Load-bearing, not informational: two identities are only ever linked into
  -- one account when both sides assert a verified address. Without that,
  -- signing up elsewhere while claiming someone's email is account takeover.
  email_verified INTEGER NOT NULL DEFAULT 0,
  created_at     INTEGER NOT NULL,
  last_login_at  INTEGER NOT NULL,
  PRIMARY KEY (provider, subject)
);
CREATE INDEX oauth_identities_user ON oauth_identities(user_id);

-- `id` is sha256(cookie token), hex -- the token itself is never stored.
--
-- A dump of this table must not be a set of usable session cookies. It costs
-- one hash per request and removes the whole class of "the database leaked and
-- so did everyone's logins".
CREATE TABLE sessions (
  id           TEXT    PRIMARY KEY,
  user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at   INTEGER NOT NULL,
  -- Sliding, but written at most hourly: a write per request would turn every
  -- page load into a D1 round trip that nothing reads.
  last_seen_at INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  user_agent   TEXT,
  -- request.cf.country, not the address. Enough to recognise a session you do
  -- not remember; not a location log.
  country      TEXT,
  revoked_at   INTEGER
);
CREATE INDEX sessions_user ON sessions(user_id, expires_at);
