-- 0057_run_sandbox_names.sql — each bound sandbox's OpenShell name, read from the cluster's Sandbox
-- resource by id (never taken from the binder), and the workspace path inside it. Both reach agent
-- tokens as the `sandbox` and `workdir` claims. A name belongs to one bound sandbox at a time.
ALTER TABLE run_sandboxes ADD COLUMN sandbox_name TEXT;
ALTER TABLE run_sandboxes ADD COLUMN workdir TEXT;
CREATE UNIQUE INDEX run_sandboxes_name ON run_sandboxes (sandbox_name);
