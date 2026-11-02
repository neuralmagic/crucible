-- 0071_draft_rename.sql — a draft's versions follow its id when the draft is renamed.
ALTER TABLE playbook_draft_versions
    DROP CONSTRAINT playbook_draft_versions_draft_id_fkey,
    ADD CONSTRAINT playbook_draft_versions_draft_id_fkey
        FOREIGN KEY (draft_id) REFERENCES playbook_drafts(id) ON DELETE CASCADE ON UPDATE CASCADE;
