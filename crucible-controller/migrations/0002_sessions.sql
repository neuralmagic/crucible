-- HTTP session rows for tower-sessions-sqlx-store's PostgresStore. Columns mirror the store's
-- internal queries exactly (its own migrate() is never called); src/session.rs's roundtrip test
-- pins this DDL to the crate. `data` is an rmp-serde blob — sessions hold scratch state only.
create table sessions (
    id text primary key,
    data bytea not null,
    expiry_date timestamptz not null
);
create index sessions_expiry_date_idx on sessions (expiry_date);
