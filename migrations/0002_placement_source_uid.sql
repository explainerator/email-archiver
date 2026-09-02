-- ----------------------------------------------------------------------------
-- Record the source server's UID on each placement.
-- ----------------------------------------------------------------------------
-- The manifest in S3 already carries source_uid, but Postgres did not, which
-- made the two representations asymmetric: manifests could rebuild the index,
-- but the index could not rebuild the manifests without losing a field.
--
-- Symmetry matters here. Manifests are how the database is reconstructed after
-- a disaster; the database is how manifests are repaired when they drift. Each
-- has to be able to produce the other, or one direction quietly degrades.
--
-- Nullable because rows ingested before this migration have no value to
-- backfill — the source UID was not retained anywhere recoverable.
-- ----------------------------------------------------------------------------

ALTER TABLE placements ADD COLUMN source_uid bigint;

COMMENT ON COLUMN placements.source_uid IS
    'UID on the source IMAP server. Distinct from placements.uid, which is the UID we serve to clients. NULL for rows ingested before this column existed.';
