-- 0067_people_grant_clients.sql — the client each approval was issued to, so a refresh token
-- redeems only for that client and a person can sign one client out. Approvals granted before
-- this were all issued to the one client there was, claude-code.
ALTER TABLE people_grants ADD COLUMN client_id TEXT NOT NULL DEFAULT 'claude-code';
ALTER TABLE people_grants ADD COLUMN client_name TEXT NOT NULL DEFAULT 'Claude Code';
ALTER TABLE people_grants ALTER COLUMN client_id DROP DEFAULT;
ALTER TABLE people_grants ALTER COLUMN client_name DROP DEFAULT;
CREATE INDEX people_grants_sub_client ON people_grants (sub, client_id);
