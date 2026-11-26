ALTER TABLE call_target_runs DROP CONSTRAINT call_target_runs_kind_check;
ALTER TABLE call_target_runs ADD CONSTRAINT call_target_runs_kind_check
    CHECK (kind IN ('playbook', 'github_workflow', 'gitlab_pipeline', 'tekton'));
