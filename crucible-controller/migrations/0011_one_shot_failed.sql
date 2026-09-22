-- 0011_one_shot_failed.sql — a fourth, terminal state for a deferred one-shot: the sweep claimed
-- it and could not mint its launch. The claim always takes the earliest pending fire_at, so a row
-- that fails deterministically (a stored ceiling that no longer parses, a deregistered playbook)
-- has to leave the pending set or it is re-picked ahead of every later one-shot forever.
ALTER TABLE playbook_one_shots
    DROP CONSTRAINT playbook_one_shots_status_check,
    ADD CONSTRAINT playbook_one_shots_status_check
        CHECK (status IN ('pending', 'fired', 'canceled', 'failed'));
