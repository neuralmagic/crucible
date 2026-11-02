-- 0062_dispatch_principal.sql — who each run was dispatched for. `issues.dispatch_requested_by` is
-- the principal whose action last asked for the issue's next dispatch (an unpark, redispatch, bump,
-- or scope-now); `runs.dispatched_by` is stamped at run creation from it, else from the launch's
-- owner. Agent tokens name `runs.dispatched_by` as `sub`.
ALTER TABLE issues ADD COLUMN dispatch_requested_by TEXT;
ALTER TABLE runs ADD COLUMN dispatched_by TEXT;
