-- ----------------------------------------------------------------------------
-- Cache each message's raw header block.
-- ----------------------------------------------------------------------------
-- Thunderbird builds its message list with BODY.PEEK[HEADER.FIELDS (...)], one
-- request per message. Serving that from S3 costs a round trip each: ~7000
-- round trips to open a large folder, which is ~30s of pure latency before any
-- data moves. A single query returns them all.
--
-- Round trips are the cost, not bytes — a ranged GET would halve the transfer
-- and change nothing about the latency.
--
-- Stored as the RAW block, everything up to and including the terminating blank
-- line, rather than parsed fields. Any HEADER.FIELDS combination is then
-- answerable from it, including ones we have not seen a client ask for, and it
-- stays byte-identical to what S3 holds.
--
-- Derived data: recomputable from S3 at any time, exactly like manifests.
-- Nullable so a partial backfill is never wrong, only slower — rows without it
-- fall back to fetching the message.
-- ----------------------------------------------------------------------------

ALTER TABLE messages ADD COLUMN headers bytea;

COMMENT ON COLUMN messages.headers IS
    'Raw RFC 5322 header block including the terminating blank line. A cache of the first bytes of the S3 object, not a second source of truth. NULL means fall back to S3.';
