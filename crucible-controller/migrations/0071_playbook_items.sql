-- 0071_playbook_items.sql — labels a playbook's runs keep on the items they process, and the run
-- holding each item. A hold counts only while that run has a live pod token.
CREATE TABLE playbook_items (
    -- A registered playbook or a draft id, as `playbook_launches.playbook` names it.
    playbook   TEXT NOT NULL,
    store      TEXT NOT NULL,
    key        TEXT NOT NULL,
    labels     JSONB NOT NULL DEFAULT '{}'::jsonb,
    held_by    TEXT,
    updated_by TEXT,
    updated_at TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (playbook, store, key)
);
