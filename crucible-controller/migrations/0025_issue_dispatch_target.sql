-- 0025_issue_dispatch_target.sql — which cluster this issue's work dispatches onto.
--
-- The target is chosen once, at launch, from the set the launcher was authorized for, and every pod
-- the issue's lifecycle dispatches afterwards (scope turn, grounded rank, loop run) reads it. It
-- lives on `issues` rather than on `playbook_launches` because a playbook launch and an
-- autoresearch scenario are both an issue row, so one column serves both paths.
--
-- NULL is "the controller's configured default" (`CONTROLLER_DISPATCH_CLUSTER`), which is what
-- every row written before targets existed means and what a launch that names no target still
-- sends. Resolution stays late on purpose: an operator who repoints the default moves the work that
-- never asked for a specific cluster, and leaves the work that did where it was put.
ALTER TABLE issues ADD COLUMN dispatch_target TEXT;

-- A schedule fires long after its author is gone, so it carries the target they were authorized
-- for at create time rather than resolving one at fire time against nobody. NULL is the same
-- "controller default" every other row means.
ALTER TABLE playbook_schedules ADD COLUMN dispatch_target TEXT;
