-- ---------------------------------------------------------------------------
-- Denormalise user_id onto folders and placements.
--
-- Groundwork for row-level security (RLS-PLAN.md). Policies need a cheap
-- predicate, and these two tables only reach a user through a join:
--
--     placements -> folders -> accounts -> user_id
--
-- A per-row subquery on placements -- 152,741 rows and the hottest table in the
-- system -- is not acceptable, and policies whose subqueries are themselves
-- RLS-filtered are very hard to reason about when something comes back empty.
-- With this column every policy is one indexable column comparison.
--
-- NO POLICY IS CREATED HERE. This migration only adds and fills the columns, so
-- it can go in and be verified while the system behaves exactly as before.
-- Enabling RLS before every read sets an identity would return an EMPTY ARCHIVE
-- rather than an error -- see RLS-PLAN.md section 6.
--
-- The redundancy is real: placements.user_id must always agree with
-- folders -> accounts -> users. That is enforced below by a composite foreign
-- key rather than by trusting ingest to get it right.
-- ---------------------------------------------------------------------------

-- --- folders ---------------------------------------------------------------

ALTER TABLE folders ADD COLUMN user_id bigint REFERENCES users (id) ON DELETE RESTRICT;

-- Derived from the existing relationship, never guessed, so the backfill cannot
-- disagree with what was already true.
UPDATE folders f
   SET user_id = a.user_id
  FROM accounts a
 WHERE a.id = f.account_id;

ALTER TABLE folders ALTER COLUMN user_id SET NOT NULL;

-- The target of the composite foreign key below. `id` is already the primary
-- key, so this pair is trivially unique; it exists only because a foreign key
-- must reference a declared unique constraint.
ALTER TABLE folders ADD CONSTRAINT folders_id_user_key UNIQUE (id, user_id);

COMMENT ON COLUMN folders.user_id IS
    'Denormalised from accounts.user_id so RLS policies need no join. Kept honest by folders_id_user_key and the composite FK on placements.';

-- --- placements ------------------------------------------------------------

ALTER TABLE placements ADD COLUMN user_id bigint REFERENCES users (id) ON DELETE RESTRICT;

UPDATE placements p
   SET user_id = f.user_id
  FROM folders f
 WHERE f.id = p.folder_id;

ALTER TABLE placements ALTER COLUMN user_id SET NOT NULL;

-- THE POINT OF THE WHOLE EXERCISE. A placement whose user_id disagrees with its
-- folder's is rejected by the database, so the denormalised copy cannot drift
-- away from real ownership no matter what application code does.
ALTER TABLE placements ADD CONSTRAINT placements_folder_user_fk
    FOREIGN KEY (folder_id, user_id) REFERENCES folders (id, user_id) ON DELETE RESTRICT;

COMMENT ON COLUMN placements.user_id IS
    'Denormalised from folders.user_id so RLS policies need no join. The composite FK to folders (id, user_id) makes a mismatch impossible.';

-- No index on either column: with a handful of users the selectivity is close
-- to nil, and an RLS predicate is ANDed onto queries that are already reaching
-- rows through folder_id or the messages user/date index. Add one with evidence
-- if a plan ever shows it wanted.
