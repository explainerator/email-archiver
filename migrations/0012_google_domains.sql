-- ---------------------------------------------------------------------------
-- Google Workspace service account keys, in the database rather than on disk.
--
-- Source credentials for generic IMAP have lived in `accounts` since migration
-- 0005; this brings the last one in. A path in `config.toml` meant the key had
-- to exist on whichever machine ran ingest, and the config is Terraform-
-- rendered, so the path had to be right for both a workstation and the
-- instance. Encrypted in the database it follows the archive instead.
--
-- Keyed by DOMAIN because that is what delegation covers: one service account
-- is authorised for a whole Workspace domain and can mint a token for any
-- mailbox in it. Per-account rows would be the same secret copied N times.
--
-- ROW-LEVEL SECURITY, but not the per-user kind.
--
-- This key is not user data. It is an operator credential that can read EVERY
-- mailbox in the domain, which makes it the most dangerous row in the database
-- and means no `user_id` predicate makes sense -- the three mailboxes it
-- unlocks may belong to three different archive users.
--
-- So the policy is break-glass instead: readable only by a transaction that has
-- explicitly asked for it by setting `archive.google_access`. Ingest asks; the
-- IMAP server and the web server never do, and have no code that would. A bug
-- in either therefore cannot reach this table even though both connect as the
-- same role with the same encryption key in memory.
--
-- That is a weaker guarantee than the per-user policies -- it is opt-in rather
-- than derived from who is asking -- and it is the strongest one available for
-- a secret that genuinely is not scoped to a user.
-- ---------------------------------------------------------------------------

CREATE TABLE google_domains (
    domain        text        PRIMARY KEY,
    -- The service account's own address, kept in the clear so `email sources`
    -- can show which account is configured without decrypting anything.
    client_email  text        NOT NULL,
    -- The whole JSON key, encrypted with config.encryption_key exactly as
    -- source mailbox passwords are.
    key_enc       text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE google_domains IS
    'Service account keys for Google Workspace domains, encrypted. One per domain: delegation is domain-wide. Readable only under archive.google_access -- see migration 0012.';

ALTER TABLE google_domains ENABLE ROW LEVEL SECURITY;
ALTER TABLE google_domains FORCE ROW LEVEL SECURITY;

CREATE POLICY google_domains_break_glass ON google_domains
    USING (current_setting('archive.google_access', true) = 'yes')
    WITH CHECK (current_setting('archive.google_access', true) = 'yes');
