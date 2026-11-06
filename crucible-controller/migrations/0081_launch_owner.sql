-- 0081_launch_owner.sql — the principal a launch's runs belong to (RFC-0003 C-HIERARCHY): the
-- team a standing launch is owned by, or the team the launcher acted as, else the launching user.
-- Rows written before this column existed have none; attribution falls back to the login-shaped
-- created_by, then to the platform.
ALTER TABLE playbook_launches ADD COLUMN owner TEXT;
