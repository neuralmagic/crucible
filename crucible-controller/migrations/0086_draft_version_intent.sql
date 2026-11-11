-- 0085_draft_version_intent.sql — the intent a generate was handed, stored on the version it
-- wrote. Only a generate sets it; an editor save or a seeded first version carries none, so the
-- column is nullable and a launch's frozen version reads back what the operator asked for.
ALTER TABLE playbook_draft_versions ADD COLUMN intent TEXT;
