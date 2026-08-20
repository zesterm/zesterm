-- Who did that to the registry, and when? (#373)
--
-- A production machine was locked out for ten days by a Revoke click nobody
-- remembered making (#365's motivating incident): revoked rows vanished from
-- every listing, and the system could not answer "who revoked this, or when".
-- The listing half is fixed (the fleet screen's Revoked section); this table
-- is the memory half -- an append-only log of every act that changes what an
-- account trusts, written by the handler that performs the act.
--
-- `subject_label` is denormalized ON PURPOSE. The label at the moment of the
-- act is the honest historical record -- the row it names may be relabeled or
-- gone by the time anyone reads the event, and an audit line that says
-- "revoked <a key hash>" answers nobody.
--
-- `actor` is the *kind* of authority behind the act, not an identity: 'owner'
-- is the account's session in a browser, 'device' a bearer device token (the
-- desktop app approving), 'machine' the key itself proving possession in a
-- signed claim. The account (`user_id`) plus the kind is what "what happened"
-- questions actually need; storing more (IPs, user agents) would make an
-- audit table into a tracking table.
--
-- No updates, no deletes, no revoked_at: an event that could be edited is not
-- an audit log. Rows are scoped to the account and are the owner's to read
-- (cookie-only route), like the revoked view they explain.
CREATE TABLE registry_events (
  id            INTEGER PRIMARY KEY,
  user_id       TEXT NOT NULL REFERENCES users(id),
  actor         TEXT NOT NULL CHECK (actor IN ('owner', 'device', 'machine')),
  action        TEXT NOT NULL CHECK (action IN ('revoke', 'restore', 'approve', 'enroll', 'register', 'claim')),
  subject_kind  TEXT NOT NULL CHECK (subject_kind IN ('host', 'device')),
  subject_id    TEXT NOT NULL CHECK (length(subject_id) = 64),
  subject_label TEXT NOT NULL,
  at            INTEGER NOT NULL
);

CREATE INDEX registry_events_user ON registry_events (user_id, at);
