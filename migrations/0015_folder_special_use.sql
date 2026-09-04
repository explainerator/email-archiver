-- ---------------------------------------------------------------------------
-- Which folders are Trash, Junk or Sent, so search can leave them out.
--
-- Searching an archive turns up the same message three times: once where it
-- lives, once in Sent, once in Trash where an earlier copy was deleted. Junk is
-- worse -- tens of thousands of messages nobody wants to read, matching on the
-- same words as everything else, crowding out the result the search was for.
-- So the default is to search neither Trash nor Junk, with the choice offered
-- rather than imposed.
--
-- Deciding which folder is which happens ONCE, here and at ingest, rather than
-- in the search query. The query runs on every keystroke-submitted search over
-- 150,000 messages; classification does not belong in its inner loop, and a
-- rule spread across a WHERE clause is a rule nobody can find later.
--
-- Only trash, junk and sent. Drafts and Archive are equally identifiable and
-- deliberately not recorded: nothing consumes them, and a column carrying
-- categories no code reads is a category error waiting to be miscounted.
--
-- The backfill below classifies by NAME because that is all the database has.
-- Ingest does better -- it reads the IMAP special-use attributes the server
-- sends with LIST, which are authoritative and language-independent -- and
-- corrects these values on its next full sweep. This exists so the feature
-- works immediately rather than after a sweep, and so a server that predates
-- RFC 6154 still gets sensible answers.
-- ---------------------------------------------------------------------------

ALTER TABLE folders ADD COLUMN special_use text;

COMMENT ON COLUMN folders.special_use IS
    'trash, junk, sent, or NULL for an ordinary folder. Set from the IMAP special-use attribute at ingest; seeded by name in migration 0015. Read by search to exclude folders. See migration 0015.';

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
