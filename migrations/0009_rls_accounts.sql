-- ---------------------------------------------------------------------------
-- Row-level security on accounts too. Reverses a decision made in 0007.
--
-- 0007 excluded this table because `email-archiver ingest <address>` starts
-- from an address and must discover which user owns it before it can declare an
-- identity, and RLS-PLAN.md section 4 judged the cost of leaving it open small:
-- "addresses, labels, hierarchy delimiters and encrypted source credentials.
-- No message bodies, no subjects, no senders."
--
-- That judgement was wrong, and wrong in the worst direction. `accounts` holds
-- `imap_password_enc`, and the application holds the key that decrypts it (see
-- db::source_for). A code path reading another user's account row does not
-- learn some metadata -- it recovers their LIVE SOURCE MAILBOX PASSWORD and can
-- log into their real mail server. That is a bigger prize than the archive it
-- was being weighed against.
--
-- The bootstrap problem is real but is a lookup problem, not a reason to leave
-- credentials unprotected. `db::owner_of_address` now resolves an address to
-- its owner by walking `users` -- which has no policy, because authentication
-- must read it before any identity exists -- and trying each in a scope. The
-- table has a handful of rows and this runs once per CLI invocation.
--
-- After this migration the only unprotected table is `users`: logins, Argon2id
-- hashes and bucket names. It cannot be protected, because `authenticate` has
-- to find a row before there is anyone to be.
-- ---------------------------------------------------------------------------

ALTER TABLE accounts ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounts FORCE ROW LEVEL SECURITY;
CREATE POLICY accounts_own_rows ON accounts
    USING (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint)
    WITH CHECK (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint);
