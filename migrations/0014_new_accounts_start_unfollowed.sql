-- ---------------------------------------------------------------------------
-- New accounts start UNFOLLOWED. Existing ones are left exactly as they are.
--
-- Migration 0013 defaulted this to true, on the reasoning that an account
-- someone had just registered was one they wanted kept current. True as far as
-- it goes, and wrong about the order things happen in.
--
-- A newly registered account has not been imported yet, so its first sweep is
-- not a catch-up of a few seconds but a full import of the entire mailbox --
-- tens of thousands of messages, hours of work. The sweep is sequential, so for
-- that whole time every other account waits behind it. The one moment an
-- account most needs to be out of the rotation is the moment it is created.
--
-- Off by default inverts that. Register the account, import it by hand, and
-- turn following on once it is loaded and its sweeps are the cheap kind:
--
--     email add-account ken@twoducks.ca new@example.com work imap
--     email set-source  new@example.com mail.example.com new@example.com
--     email ingest      new@example.com
--     email follow      new@example.com on
--
-- SET DEFAULT rather than an UPDATE: every account already in the table has
-- been imported and should keep being followed. Changing the default changes
-- what happens next, and nothing about what is already true.
-- ---------------------------------------------------------------------------

ALTER TABLE accounts ALTER COLUMN follow SET DEFAULT false;

COMMENT ON COLUMN accounts.follow IS
    'Whether the scheduler and `refresh` check this mailbox for new mail. New accounts start false so a first import does not block the sweep; turn on once loaded. Turning off when a source shuts down leaves the archive untouched. See migrations 0013 and 0014.';
