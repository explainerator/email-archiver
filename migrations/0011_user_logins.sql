-- ---------------------------------------------------------------------------
-- Several logins per user.
--
-- `kduck@twoducks.ca` and `ken@twoducks.ca` are the same person, and either
-- should get them into their archive. Migration 0010 made the login an email
-- address; this makes it *addresses*, plural.
--
-- WHY A TABLE RATHER THAN AN ALIAS COLUMN OR A SECOND TABLE.
--
-- Every login now lives in exactly one place, so the primary key is the
-- uniqueness rule -- there is no way for an alias to collide with somebody
-- else's login, because they are rows in the same index. The obvious cheaper
-- design, keeping `users.login` and adding a separate `user_aliases`, puts
-- logins in two uniqueness domains: "is this alias already a login?" becomes a
-- SELECT-then-INSERT in application code, which is a race and a rule someone
-- has to remember. This has neither.
--
-- `is_primary` marks the canonical address -- the one shown in the UI and the
-- one `rename-user` changes. The partial unique index below permits exactly one
-- per user, so "which address is this person's real one" is answered by the
-- schema rather than by convention.
--
-- NO ROW-LEVEL SECURITY here, deliberately, for the same reason `users` has
-- none: `authenticate` has to resolve a login before there is any identity to
-- scope by. The table maps addresses to user ids and holds nothing else.
-- ---------------------------------------------------------------------------

CREATE TABLE user_logins (
    login      text        PRIMARY KEY,
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    -- The canonical address. Exactly one per user; see the index below.
    is_primary boolean     NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX user_logins_user_idx ON user_logins (user_id);

-- One primary per user. A partial unique index rather than a constraint,
-- because the rule is "at most one row where is_primary", not "unique across
-- all rows".
CREATE UNIQUE INDEX user_logins_one_primary ON user_logins (user_id) WHERE is_primary;

-- Carry the existing logins over as primaries before dropping the column, so
-- nobody is locked out by this migration.
INSERT INTO user_logins (login, user_id, is_primary)
SELECT login, id, true FROM users;

-- users.login is gone: one source of truth. Anything that resolved a login now
-- joins through user_logins.
ALTER TABLE users DROP COLUMN login;

COMMENT ON TABLE user_logins IS
    'Email addresses a user may log in with, for both IMAP and the web client. The primary key makes every login unique across all users; is_primary marks the canonical one.';
COMMENT ON COLUMN user_logins.is_primary IS
    'The canonical address, shown in the UI and changed by `rename-user`. Exactly one per user, enforced by user_logins_one_primary.';
