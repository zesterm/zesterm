-- Bearer tokens for machines: the credential /api/enroll/claim hands back.
--
-- A separate table from `sessions` on purpose. A session is a cookie keyed to
-- a person; a machine token is keyed to a *principal* — a host or device row
-- whose primary key is its public key — and must die the moment that principal
-- is revoked. Overloading `sessions` would mean nullable principal columns and
-- a resolver that branches on row shape, and the resolver is security code.
--
-- `id` is hex(sha256(token)); the token itself is returned once and never
-- stored, for the reason `sessions.id` works the same way (a dump of this
-- table must be a list of hashes, not a bag of usable credentials).
--
-- There is deliberately no ON DELETE CASCADE to hosts/devices: those rows are
-- never deleted (revocation is an UPDATE), and liveness is enforced by the
-- resolver joining against them — a revoked principal fails the join, so
-- revocation kills its token with no second write anywhere.
CREATE TABLE machine_tokens (
  id             TEXT    PRIMARY KEY,
  user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  principal_kind TEXT    NOT NULL CHECK (principal_kind IN ('host', 'device')),
  principal_id   TEXT    NOT NULL
                         CHECK (length(principal_id) = 64 AND principal_id NOT GLOB '*[^0-9a-f]*'),
  created_at     INTEGER NOT NULL,
  last_seen_at   INTEGER NOT NULL,
  expires_at     INTEGER NOT NULL,
  revoked_at     INTEGER
);

-- For "revoke this principal's older tokens" at mint time. Partial, because
-- only live tokens are ever looked up this way.
CREATE INDEX machine_tokens_principal
  ON machine_tokens(principal_kind, principal_id) WHERE revoked_at IS NULL;
