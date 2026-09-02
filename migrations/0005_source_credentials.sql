-- ----------------------------------------------------------------------------
-- Move source mailbox credentials out of config.toml and into the database.
-- ----------------------------------------------------------------------------
-- Source passwords were rendered into config.toml by Terraform, which put them
-- in terraform.tfstate in plaintext and on the instance's disk. They are
-- hand-entered credentials for other people's servers; the database is a better
-- home than a generated file.
--
-- imap_password is ENCRYPTED, not hashed: it must be presented to the source
-- server, so it has to be recoverable. The key lives in config.toml, never
-- here — that is what stops this being obfuscation. See src/secrets.rs for what
-- it does and does not protect against.
--
-- Nullable so accounts can exist before their credentials are set. An account
-- without them simply cannot be ingested, and says so.
-- ----------------------------------------------------------------------------

ALTER TABLE accounts
    ADD COLUMN imap_host           text,
    ADD COLUMN imap_port           integer NOT NULL DEFAULT 993,
    ADD COLUMN imap_username       text,
    ADD COLUMN imap_password_enc   text,
    -- Per-source, because one server with a stale certificate must not weaken
    -- verification for every other account.
    ADD COLUMN allow_invalid_certs boolean NOT NULL DEFAULT false;

ALTER TABLE accounts ADD CONSTRAINT accounts_imap_port_valid
    CHECK (imap_port > 0 AND imap_port <= 65535);

COMMENT ON COLUMN accounts.imap_password_enc IS
    'Source mailbox password, encrypted with the key from config.toml (XChaCha20-Poly1305, base64 of nonce||ciphertext). Encrypted rather than hashed because it must be presented to the source server.';

-- users.password_hash exists already but held a deliberate non-hash ('!') so
-- nothing could authenticate. Now that the IMAP server verifies properly, say
-- what it is for.
COMMENT ON COLUMN users.password_hash IS
    'Argon2id hash of the IMAP password for this archive user. The literal ''!'' means no password has been set and login is impossible; set one with: email-archiver set-password <login>.';
