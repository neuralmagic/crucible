-- 0087_launch_test_cursor.sql — what a schedule's test firing was given as its cursor
-- (`{"from": "stored" | "none" | "value", "value"?}`). NULL on every other launch. A test firing
-- never moves the schedule's cursor, which the constraint holds the row to.
ALTER TABLE playbook_launches ADD COLUMN test_cursor JSONB;
ALTER TABLE playbook_launches ADD CONSTRAINT playbook_launches_test_cursor_inert
    CHECK (test_cursor IS NULL OR NOT advance_dedupe);
