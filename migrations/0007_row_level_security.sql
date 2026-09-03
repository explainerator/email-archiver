-- ---------------------------------------------------------------------------
-- Row-level security on the tables that hold mail.
--
-- Postgres appends `user_id = current_setting('archive.user_id')` to every
-- query against these three tables. A query that reaches them without going
-- through `db::Scope` therefore sees nothing.
--
-- READ THAT FAILURE MODE CAREFULLY, because it is about OUR code, not an
-- attacker's. An attacker shown zero rows is the feature working. But if some
-- path in this application reaches these tables without setting an identity,
-- the policy hides the rows belonging to the person legitimately logged in --
-- the owner clicks a folder and sees an empty one. That is why RLS-PLAN.md
-- phase 2 routed every path through a Scope BEFORE this migration exists: if a
-- path had been missed, it would have surfaced as an ordinary bug while nothing
-- was yet filtering.
--
-- Reversible in one statement per table if a missed path ever does surface:
--
--     ALTER TABLE messages DISABLE ROW LEVEL SECURITY;
--
-- `gern` owns these tables, so that needs no console action and no data
-- migration. It is an emergency lever, not a plan; the real fix is always to
-- scope the path that was missed.
--
-- WARNING FOR FUTURE MIGRATIONS. Migrations run through `connect_db`, which is
-- outside any Scope, so `archive.user_id` is unset while they execute. DDL is
-- unaffected, but any DML -- an UPDATE, INSERT or DELETE against these three
-- tables in a later migration -- will match ZERO rows and report success. A
-- backfill would silently do nothing.
--
-- Migration 0006 got away with it only because it ran before this one existed.
-- Any future migration touching these tables must bracket its DML:
--
--     ALTER TABLE placements DISABLE ROW LEVEL SECURITY;
--     UPDATE placements SET ...;
--     ALTER TABLE placements ENABLE ROW LEVEL SECURITY;
--
-- Both statements are transactional, so an interrupted migration leaves the
-- policy on rather than off.
--
-- Only these three tables. `users` and `accounts` are deliberately excluded --
-- authentication and `ingest <address>` both have to read them BEFORE an
-- identity exists, and neither holds mail. See RLS-PLAN.md section 4.
-- ---------------------------------------------------------------------------

-- FORCE as well as ENABLE. `gern` owns these tables and owners bypass RLS
-- otherwise, which would make the whole thing decorative for the one role that
-- actually connects. FORCE applies the policy to the owner too.
--
-- `current_setting(..., true)` -- the `true` is missing_ok. Without it an unset
-- variable raises an error instead of returning NULL, and every query outside a
-- Scope would fail loudly rather than returning nothing. Tempting, and
-- deliberately not chosen: migrations themselves run outside any Scope, and so
-- do the `users` and `accounts` lookups that must happen before an identity
-- exists. A policy that throws would take those down too.
--
-- WITH CHECK as well as USING: USING governs what a query can see, WITH CHECK
-- governs what it may write. Without the latter, ingest could insert a row
-- owned by someone else and then be unable to read it back.

ALTER TABLE messages ENABLE ROW LEVEL SECURITY;
ALTER TABLE messages FORCE ROW LEVEL SECURITY;
CREATE POLICY messages_own_rows ON messages
    USING (user_id = current_setting('archive.user_id', true)::bigint)
    WITH CHECK (user_id = current_setting('archive.user_id', true)::bigint);

ALTER TABLE folders ENABLE ROW LEVEL SECURITY;
ALTER TABLE folders FORCE ROW LEVEL SECURITY;
CREATE POLICY folders_own_rows ON folders
    USING (user_id = current_setting('archive.user_id', true)::bigint)
    WITH CHECK (user_id = current_setting('archive.user_id', true)::bigint);

ALTER TABLE placements ENABLE ROW LEVEL SECURITY;
ALTER TABLE placements FORCE ROW LEVEL SECURITY;
CREATE POLICY placements_own_rows ON placements
    USING (user_id = current_setting('archive.user_id', true)::bigint)
    WITH CHECK (user_id = current_setting('archive.user_id', true)::bigint);
