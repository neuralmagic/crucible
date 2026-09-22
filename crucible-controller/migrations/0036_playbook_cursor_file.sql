-- 0036_playbook_cursor_file.sql — a schedule cursor that carries a file. `cursor_from` grows a
-- second grammar: a run-file key (`rollup/STATE.json`, the key `GET /api/runs/{id}/files/{key}`
-- serves) beside the `$.task.field` result path. A file cursor is delivered as a pack file at
-- `cursor_path` instead of as a param; `cursor_value` holds the file body, bounded by the advance's
-- size cap rather than the column. Exactly one delivery target is set, and which one follows from
-- the grammar of `cursor_from`.
ALTER TABLE playbook_schedules
    ADD COLUMN cursor_path TEXT,
    DROP CONSTRAINT playbook_schedules_cursor_pair,
    ADD CONSTRAINT playbook_schedules_cursor_shape CHECK (
        (cursor_from IS NULL AND cursor_param IS NULL AND cursor_path IS NULL)
        OR (cursor_from LIKE '$.%' AND cursor_param IS NOT NULL AND cursor_path IS NULL)
        OR (cursor_from NOT LIKE '$.%' AND cursor_path IS NOT NULL AND cursor_param IS NULL)
    );
