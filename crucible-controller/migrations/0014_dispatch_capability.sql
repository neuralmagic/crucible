-- 0014_dispatch_capability.sql — what substrate a pack's agent needs, and how a run was
-- dispatched. The `[agent]` backend and sandbox image are read off the manifest wherever a pack is
-- compiled (registration, import, draft save) and stored beside the params schema, so the preview
-- gate, the launch endpoints and the registry list all read one derived fact instead of unpacking
-- a tarball per request.
--
-- NULL is "not recorded": a manifest that did not parse, or a row stored before this column
-- existed. The startup backfill reads the stored tarballs and fills in what it can, so a NULL that
-- survives it is a pack whose manifest really is unreadable.
ALTER TABLE playbooks ADD COLUMN agent_backend TEXT;
ALTER TABLE playbooks ADD COLUMN agent_sandbox_image TEXT;

ALTER TABLE pack_imports ADD COLUMN agent_backend TEXT;
ALTER TABLE pack_imports ADD COLUMN agent_sandbox_image TEXT;

ALTER TABLE playbook_draft_versions ADD COLUMN agent_backend TEXT;
ALTER TABLE playbook_draft_versions ADD COLUMN agent_sandbox_image TEXT;

-- How the run's engine was dispatched: a controller-owned work pod, or a supervised subprocess on
-- the controller's own machine (the config-gated local mode). Every pre-existing run was a pod.
ALTER TABLE runs ADD COLUMN dispatch TEXT NOT NULL DEFAULT 'pod'
    CHECK (dispatch IN ('pod', 'local'));
