-- ---------------------------------------------------------------------------
-- Which source mailboxes are still worth checking for new mail.
--
-- Importing a mailbox once is not the end of it. While an account is live its
-- archive falls behind the moment the import finishes, so the useful default is
-- to keep going back for whatever has arrived since -- which resume state
-- already makes cheap, since a re-run fetches only what is new.
--
-- But these accounts do not live forever. A domain lapses, a job ends, a
-- provider shuts a mailbox down; the archive of it stays valuable long after
-- the mailbox itself is gone. At that point continuing to reach for it is at
-- best a guaranteed error on every sweep and at worst a slow timeout holding up
-- every other account behind it.
--
-- Hence a flag rather than deletion. Unfollowing keeps every message, every
-- folder and every credential exactly where they are, and stops only the
-- reaching out. It is reversible, which deleting the account would not be.
--
-- DEFAULT true, because an account someone has just gone to the trouble of
-- registering is one they want kept current. The flag exists to be turned OFF
-- later, once that stops being so.
-- ---------------------------------------------------------------------------

ALTER TABLE accounts ADD COLUMN follow boolean NOT NULL DEFAULT true;

COMMENT ON COLUMN accounts.follow IS
    'Whether `refresh` should check this mailbox for new mail. Turn off when the source is shut down; the archive of it is unaffected. See migration 0013.';
