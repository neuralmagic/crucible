-- 0010_playbook_cursor.sql — v1 recurring-input dedupe: the cursor. A schedule maps one field of a
-- successful run's result to a param of its next firing; the controller stores the value between
-- firings and passes it as an ordinary param, and the pack subtracts. Same conventions as the
-- baseline: TEXT RFC3339 UTC timestamps, and a DEFAULT on every added NOT NULL column.
ALTER TABLE playbook_schedules
    -- Where in the run result the next value is read from: `$.task.field`, the first segment being
    -- the task whose `output` carries it.
    ADD COLUMN cursor_from       TEXT,
    -- The param the next firing overlays the stored value onto.
    ADD COLUMN cursor_param      TEXT,
    -- The last successful run's value. NULL until one lands, and the firing then omits the param
    -- entirely so the pack's own declared default stands.
    ADD COLUMN cursor_value      TEXT,
    ADD COLUMN cursor_updated_at TEXT,
    ADD CONSTRAINT playbook_schedules_cursor_pair
        CHECK ((cursor_from IS NULL) = (cursor_param IS NULL));

-- Which schedule's cursor this run may advance, recorded at adopt time because a dequeued key
-- carries no payload. NULL is the default and means none: a launch naming no schedule moves no
-- dedupe state, whatever it was opted into.
ALTER TABLE playbook_launches
    ADD COLUMN dedupe_schedule TEXT REFERENCES playbook_schedules(id) ON DELETE SET NULL;

ALTER TABLE playbook_one_shots
    ADD COLUMN dedupe_schedule TEXT REFERENCES playbook_schedules(id) ON DELETE SET NULL;
