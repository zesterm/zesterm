-- Attestations: signed statements that one trusted device vouches for another
-- (crates/zest-mesh/src/attest.rs; #184). The account distributes them and the
-- revocation list to daemons, which verify the signature themselves and
-- additionally require `by_device` in their own trust store — the table is a
-- distribution channel, never the authority.
--
-- PRIMARY KEY (device_id, by_device) means re-approval by the same approver
-- REPLACES its earlier statement rather than accumulating: renewal is simply
-- approving again, and "how many vouchers does this pair have" stays one.
-- Different approvers vouching for one device are different rows on purpose --
-- revoking the approver must not take down a voucher somebody else made.
--
-- `blob` is the attestation verbatim as it was verified --
-- base64url(message).base64url(signature) -- because verification is always
-- over arrived bytes: re-encoding parsed fields would make every later
-- verification depend on two encoders agreeing, and a disagreement surfaces
-- as "bad signature" naming nothing. The iat/exp columns are copies *for
-- querying* (serving only unexpired rows); the signed truth stays inside the
-- blob.
--
-- `revoked_at` is revocation Model B: attestations are long-lived (a year),
-- so what undoes one is this flag -- set when the subject OR the approver is
-- revoked -- not the clock. Rows are kept, as everywhere in this schema, so
-- "why can this device not attach" has an answer.

PRAGMA foreign_keys = ON;

CREATE TABLE device_attestations (
  -- Both 64-hex ClientIds, the CHECKs for the reason hosts.id has one: a
  -- malformed id would otherwise surface much later as a signature that never
  -- verifies, pointing at the crypto rather than at whatever wrote the row.
  device_id  TEXT    NOT NULL
                     CHECK (length(device_id) = 64 AND device_id NOT GLOB '*[^0-9a-f]*'),
  by_device  TEXT    NOT NULL
                     CHECK (length(by_device) = 64 AND by_device NOT GLOB '*[^0-9a-f]*'),
  user_id    TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  blob       TEXT    NOT NULL,
  iat        INTEGER NOT NULL,
  exp        INTEGER NOT NULL,
  revoked_at INTEGER,
  PRIMARY KEY (device_id, by_device)
);

-- For "serve this account's live attestations". Partial, because revoked rows
-- are only ever read by someone debugging, not by the serving query.
CREATE INDEX device_attestations_user
  ON device_attestations(user_id) WHERE revoked_at IS NULL;
