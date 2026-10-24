-- 0068_people_dynamic_clients.sql — public clients that registered themselves (RFC 7591), limited
-- to loopback redirect URIs. A registration unused for the controller's idle limit is deleted with
-- the approvals issued to it.
CREATE TABLE people_dynamic_clients (
    client_id     TEXT PRIMARY KEY,
    client_name   TEXT NOT NULL,
    redirect_uris JSONB NOT NULL,
    created_at    BIGINT NOT NULL,
    last_used_at  BIGINT NOT NULL
);
CREATE INDEX people_dynamic_clients_last_used ON people_dynamic_clients (last_used_at);
