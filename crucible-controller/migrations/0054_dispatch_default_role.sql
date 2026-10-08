-- 0054_dispatch_default_role.sql — a default per inference role.
--
-- A run reaches up to two models: the one its agent turns think with, and the one its route tasks
-- ask. Both are chosen the same way, scope then class, so a default row names the role it answers
-- for. Every row written before this one was an agent default.
ALTER TABLE dispatch_defaults ADD COLUMN role TEXT NOT NULL DEFAULT 'agent'
    CHECK (role IN ('agent', 'decision'));
ALTER TABLE dispatch_defaults ALTER COLUMN role DROP DEFAULT;
ALTER TABLE dispatch_defaults DROP CONSTRAINT dispatch_defaults_pkey;
ALTER TABLE dispatch_defaults ADD PRIMARY KEY (scope_kind, scope_ref, workload_class, role);
