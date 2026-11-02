-- 0073_run_created_at.sql — when a run row was recorded. Existing rows are dated from their id:
-- an engine-minted id leads with `YYYYMMDDTHHMMSSZ`, a controller-minted one ends in `-<unix secs>`.
ALTER TABLE runs ADD COLUMN created_at TEXT;

UPDATE runs SET created_at = CASE
    WHEN run_id ~ '^[0-9]{4}(0[1-9]|1[0-2])(0[1-9]|[12][0-9]|3[01])T([01][0-9]|2[0-3])[0-5][0-9][0-5][0-9]Z' THEN
        substr(run_id, 1, 4) || '-' || substr(run_id, 5, 2) || '-' || substr(run_id, 7, 2) || 'T' ||
        substr(run_id, 10, 2) || ':' || substr(run_id, 12, 2) || ':' || substr(run_id, 14, 2) || 'Z'
    WHEN run_id ~ '-[0-9]{10}$' THEN
        to_char(to_timestamp(substring(run_id FROM '-([0-9]{10})$')::bigint) AT TIME ZONE 'UTC',
                'YYYY-MM-DD"T"HH24:MI:SS"Z"')
END;

-- An id that carries no time takes the newest dated row inserted before it (`seq` is insertion
-- order), else the oldest dated row inserted after it.
UPDATE runs r SET created_at = COALESCE(
    (SELECT max(p.created_at) FROM runs p WHERE p.seq < r.seq AND p.created_at IS NOT NULL),
    (SELECT min(n.created_at) FROM runs n WHERE n.seq > r.seq AND n.created_at IS NOT NULL),
    to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
WHERE r.created_at IS NULL;

ALTER TABLE runs
    ALTER COLUMN created_at SET DEFAULT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
    ALTER COLUMN created_at SET NOT NULL;

CREATE INDEX runs_created_at ON runs (created_at DESC, seq DESC);
