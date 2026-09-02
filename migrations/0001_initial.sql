-- ----------------------------------------------------------------------------
-- email-archiver: initial schema
-- ----------------------------------------------------------------------------
-- Lives in its own DATABASE (`archive`) on the shared OVH Managed Postgres
-- cluster, alongside `defaultdb` (game services) and `backroom`. A separate
-- database is stronger isolation than the shared-schema approach originally
-- planned: the game services' tables are not merely in a different namespace,
-- they are in a different database entirely.
--
-- Connects as `gern`, which cannot CREATE on the database itself but owns
-- enough to create tables in `public` — verified, so no privileged bootstrap
-- step is required.
--
-- Objects are therefore unqualified and land in `public` within `archive`.
-- The binary asserts current_database() = 'archive' before migrating, so a
-- misconfigured DATABASE_URL cannot write into defaultdb.
-- ----------------------------------------------------------------------------

-- ----------------------------------------------------------------------------
-- users: one person. Owns exactly one S3 bucket.
-- ----------------------------------------------------------------------------
CREATE TABLE users (
    id            bigserial PRIMARY KEY,
    login         text        NOT NULL UNIQUE,
    password_hash text        NOT NULL,
    bucket        text        NOT NULL UNIQUE,
    display_name  text,
    created_at    timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE  users IS 'One person. Authenticates over IMAP; owns one S3 bucket.';
COMMENT ON COLUMN users.bucket IS
    'S3 bucket name, derived in Terraform from the primary address (ken@twoducks.ca -> ken-twoducks-ca). Must match terraform output user_buckets.';
COMMENT ON COLUMN users.password_hash IS
    'Argon2 hash of the IMAP password. This is an archive credential, unrelated to the source mail account passwords.';

-- ----------------------------------------------------------------------------
-- accounts: a source mailbox feeding a user's archive. A user may have several.
-- ----------------------------------------------------------------------------
CREATE TABLE accounts (
    id         bigserial PRIMARY KEY,
    user_id    bigint      NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    address    text        NOT NULL UNIQUE,
    label      text        NOT NULL,
    provider   text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (user_id, label),
    CONSTRAINT accounts_provider_known CHECK (provider IN ('gmail', 'imap')),
    -- The label becomes an IMAP namespace prefix, so it must not contain the
    -- hierarchy delimiter or it would forge folder structure.
    CONSTRAINT accounts_label_simple CHECK (label ~ '^[a-z0-9][a-z0-9_-]*$')
);

COMMENT ON COLUMN accounts.label IS
    'IMAP namespace prefix for this account''s folders, e.g. "work" -> work/INBOX. Unique per user.';
COMMENT ON COLUMN accounts.provider IS
    'gmail = XOAUTH2 ingest; imap = password ingest. Drives auth only, not storage.';

-- ON DELETE RESTRICT throughout, deliberately: nothing in this schema should
-- ever cascade-delete archived mail as a side effect of removing a parent row.

-- ----------------------------------------------------------------------------
-- folders: an IMAP mailbox within an account. Also carries ingest resume state.
-- ----------------------------------------------------------------------------
CREATE TABLE folders (
    id          bigserial PRIMARY KEY,
    account_id  bigint NOT NULL REFERENCES accounts (id) ON DELETE RESTRICT,
    name        text   NOT NULL,

    -- UIDs we serve to Thunderbird. Assigned once, never reissued: if these
    -- shifted, every client would silently re-download everything.
    uidvalidity bigint NOT NULL,
    uidnext     bigint NOT NULL DEFAULT 1,

    -- Ingest resume state, from the SOURCE server. Distinct from the UIDs above.
    -- If the source's uidvalidity changes, its UIDs are meaningless and the
    -- folder must be re-scanned from zero.
    source_uidvalidity bigint,
    last_source_uid    bigint NOT NULL DEFAULT 0,

    UNIQUE (account_id, name),
    CONSTRAINT folders_uidnext_positive     CHECK (uidnext >= 1),
    CONSTRAINT folders_uidvalidity_positive CHECK (uidvalidity >= 1)
);

COMMENT ON COLUMN folders.last_source_uid IS
    'Highest UID already ingested from the source server. Makes a multi-day pull resumable after interruption.';

-- ----------------------------------------------------------------------------
-- messages: one row per distinct message per user.
-- ----------------------------------------------------------------------------
CREATE TABLE messages (
    id            bigserial PRIMARY KEY,
    user_id       bigint      NOT NULL REFERENCES users (id) ON DELETE RESTRICT,

    -- Content address. The raw RFC 5322 bytes live at messages/<blake3> in the
    -- user's bucket. Unique PER USER, not globally: cross-user dedup would mean
    -- one person's storage backing another's mail. See ARCHIVE-PLAN.md 2.3.
    blake3        text        NOT NULL,

    size          bigint      NOT NULL,
    internaldate  timestamptz NOT NULL,

    -- Denormalised for IMAP SEARCH. The authoritative copy is in `envelope`.
    subject       text,
    from_addr     text,

    envelope      jsonb       NOT NULL,
    bodystructure jsonb       NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),

    UNIQUE (user_id, blake3),
    CONSTRAINT messages_blake3_hex   CHECK (blake3 ~ '^[0-9a-f]{64}$'),
    CONSTRAINT messages_size_sane    CHECK (size >= 0)
);

COMMENT ON TABLE messages IS
    'Immutable. Rows are inserted by the indexer and never updated. Rebuildable in full from the S3 manifest objects.';

-- ----------------------------------------------------------------------------
-- placements: where a message appears. One message may sit in several folders.
-- ----------------------------------------------------------------------------
CREATE TABLE placements (
    folder_id  bigint  NOT NULL REFERENCES folders (id)  ON DELETE RESTRICT,
    uid        bigint  NOT NULL,
    message_id bigint  NOT NULL REFERENCES messages (id) ON DELETE RESTRICT,

    -- The ONLY mutable column in this schema. Thunderbird is permitted to set
    -- \Seen; no other IMAP flag is accepted and no other STORE is honoured.
    -- Modelled as a boolean rather than a flag array so that "read state is the
    -- only thing a client can change" is enforced by the schema, not by code.
    seen       boolean NOT NULL DEFAULT false,

    PRIMARY KEY (folder_id, uid),
    CONSTRAINT placements_uid_positive CHECK (uid >= 1)
);

CREATE INDEX placements_message_idx ON placements (message_id);

-- ----------------------------------------------------------------------------
-- Indexes supporting the IMAP SEARCH criteria we intend to serve.
-- Substring criteria (SUBJECT, FROM) are left to sequential scan for now;
-- revisit with pg_trgm in Phase 5 if they prove slow at real volume.
-- ----------------------------------------------------------------------------
CREATE INDEX messages_user_date_idx ON messages (user_id, internaldate);
CREATE INDEX messages_user_size_idx ON messages (user_id, size);
