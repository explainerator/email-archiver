-- ----------------------------------------------------------------------------
-- Record each source server's IMAP hierarchy delimiter.
-- ----------------------------------------------------------------------------
-- The delimiter is a property of the source server, not a universal. Dovecot
-- with Maildir++ uses '.', so Archives.qra.2014.Sent is a tree; Gmail and
-- Exchange use '/'. Assuming one would mangle folder names on the other.
--
-- What makes translating safe is that a server cannot have a folder whose name
-- contains its own delimiter as data — it would be read as hierarchy. So
-- normalising with the SERVER'S OWN delimiter is lossless, while guessing is
-- not.
--
-- The server reports it in the LIST response; ingest previously discarded it.
--
-- folders.name continues to hold the source name verbatim. This only affects
-- how folders are presented over IMAP, so nothing already archived moves and
-- re-ingest still matches by source name.
--
-- NULL means unknown (rows ingested before this existed), in which case names
-- are presented unchanged rather than guessed at.
-- ----------------------------------------------------------------------------

ALTER TABLE accounts ADD COLUMN hierarchy_delimiter text;

ALTER TABLE accounts ADD CONSTRAINT accounts_delimiter_single_char
    CHECK (hierarchy_delimiter IS NULL OR length(hierarchy_delimiter) = 1);

COMMENT ON COLUMN accounts.hierarchy_delimiter IS
    'Hierarchy separator reported by the source server in its LIST response, e.g. "." for Dovecot Maildir++ or "/" for Gmail. Used only to present folder names; folders.name stays as the source named it. NULL means present names unchanged.';
