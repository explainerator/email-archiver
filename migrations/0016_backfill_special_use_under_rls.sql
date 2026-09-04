-- ---------------------------------------------------------------------------
-- Redo migration 0015's backfill, this time with the policy out of the way.
--
-- 0015 added folders.special_use and seeded it by name. The UPDATE matched zero
-- rows and reported success, so every folder stayed NULL and search had nothing
-- to exclude. The feature looked finished and did nothing.
--
-- Migrations run through connect_db, outside any Scope, so archive.user_id is
-- unset while they execute. `folders` is FORCE ROW LEVEL SECURITY, which
-- applies to the table owner too, and the policy's USING clause compares
-- user_id against a setting that is not there. No rows are visible, so no rows
-- are updated -- and an UPDATE matching nothing is not an error.
--
-- Migration 0007 wrote this warning down, naming the three tables and giving
-- the bracket below as the required pattern. 0015 was written without reading
-- it. The lesson is not that the policy is inconvenient: it is that a silent
-- zero-row DML is indistinguishable from success, which is why the assertion
-- belongs in a test that reads back what was written.
--
-- 0015 is left exactly as it is. It has been applied, and sqlx checksums
-- applied migrations, so editing it would break startup rather than fix
-- history.
--
-- Both ALTERs are transactional: an interrupted migration leaves the policy on
-- rather than off.
-- ---------------------------------------------------------------------------

ALTER TABLE folders DISABLE ROW LEVEL SECURITY;

-- The last path segment: Gmail sends "[Gmail]/Sent Mail", Dovecot often
-- "INBOX.Trash", so both separators are stripped.
UPDATE folders
   SET special_use = CASE lower(regexp_replace(name, '^.*[/.]', ''))
       WHEN 'trash'            THEN 'trash'
       WHEN 'bin'              THEN 'trash'
       WHEN 'deleted'          THEN 'trash'
       WHEN 'deleted items'    THEN 'trash'
       WHEN 'deleted messages' THEN 'trash'
       WHEN 'junk'             THEN 'junk'
       WHEN 'junk e-mail'      THEN 'junk'
       WHEN 'junk email'       THEN 'junk'
       WHEN 'spam'             THEN 'junk'
       WHEN 'bulk mail'        THEN 'junk'
       WHEN 'sent'             THEN 'sent'
       WHEN 'sent mail'        THEN 'sent'
       WHEN 'sent items'       THEN 'sent'
       WHEN 'sent messages'    THEN 'sent'
       ELSE NULL
   END;

ALTER TABLE folders ENABLE ROW LEVEL SECURITY;
ALTER TABLE folders FORCE ROW LEVEL SECURITY;
