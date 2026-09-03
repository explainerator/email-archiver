-- ---------------------------------------------------------------------------
-- Treat an EMPTY session setting as "no identity", not as a cast error.
--
-- 0007 used:
--
--     user_id = current_setting('archive.user_id', true)::bigint
--
-- which is right only while the variable has never been set on that connection.
-- `current_setting(..., true)` returns NULL when the setting does not exist, and
-- `user_id = NULL` is never true, so an unscoped query correctly sees nothing.
--
-- But once ANY transaction on a pooled connection has called
-- `set_config('archive.user_id', ..., true)`, the variable exists. Rolling back
-- restores its previous value, which is the EMPTY STRING rather than absent. The
-- next unscoped query on that connection then evaluates `''::bigint` and raises
--
--     invalid input syntax for type bigint: ""
--
-- So the behaviour depended on whether the pooled connection had been used
-- before: the first query after a restart returned zero rows, and the same query
-- on a recycled connection blew up. Found by the ingest write-path test, whose
-- cleanup runs unscoped on purpose.
--
-- NULLIF collapses both cases to NULL, so "no identity" means "no rows"
-- consistently, whatever the connection has done previously.
-- ---------------------------------------------------------------------------

DROP POLICY messages_own_rows ON messages;
CREATE POLICY messages_own_rows ON messages
    USING (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint)
    WITH CHECK (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint);

DROP POLICY folders_own_rows ON folders;
CREATE POLICY folders_own_rows ON folders
    USING (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint)
    WITH CHECK (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint);

DROP POLICY placements_own_rows ON placements;
CREATE POLICY placements_own_rows ON placements
    USING (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint)
    WITH CHECK (user_id = NULLIF(current_setting('archive.user_id', true), '')::bigint);
