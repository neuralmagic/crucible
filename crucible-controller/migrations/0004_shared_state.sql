-- 0004_shared_state.sql — the state volume's contents get tables: the controller event log,
-- artifact payloads (chunked bytea), pack tarballs + steering appends, and the autopilot flag.
-- Same conventions as the baseline: TEXT RFC3339 UTC timestamps, BIGINT integers.

-- The frozen NDJSON line shape, one row per line. `from`/`to` are reserved words, so the
-- columns are from_status/to_status; the export path re-emits them as `from`/`to`.
CREATE TABLE events (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    v           BIGINT NOT NULL,
    ts          TEXT NOT NULL,
    key         TEXT NOT NULL,
    from_status TEXT NOT NULL,
    to_status   TEXT NOT NULL,
    reason      TEXT,
    evidence    TEXT,
    actor       TEXT
);
CREATE INDEX events_key ON events (key);
CREATE INDEX events_ts ON events (ts);

-- Artifact payloads: pod evidence (the drop-box bodies pod_artifacts points at) and run session
-- logs (dispatched and adopted alike). Bytes stay gzipped exactly as ingested, split into
-- fixed-size chunks (blob_store::ARTIFACT_CHUNK_BYTES).
CREATE TABLE artifacts (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('pod-evidence', 'run-session')),
    owner_id   TEXT NOT NULL,
    kind       TEXT NOT NULL,
    digest     TEXT NOT NULL,
    bytes      BIGINT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (owner_kind, owner_id, kind)
);
CREATE INDEX artifacts_digest ON artifacts (digest);

CREATE TABLE artifact_chunks (
    artifact_id BIGINT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    seq         BIGINT NOT NULL,
    data        BYTEA NOT NULL,
    PRIMARY KEY (artifact_id, seq)
);

-- The frozen pack, as the gzipped tarball the pod executor ships. Keyed by the sanitized issue
-- key (reconcile::sanitize_key, `owner/repo#7` -> `owner_repo_7`). Packs cap at 16 MiB, so a
-- single bytea holds one.
CREATE TABLE pack_tarballs (
    issue_slug TEXT PRIMARY KEY,
    tar_gz     BYTEA NOT NULL,
    digest     TEXT NOT NULL,
    bytes      BIGINT NOT NULL,
    created_at TEXT NOT NULL
);

-- Human STEER.md appends, one row each, injected at pack materialization so the tarball stays
-- frozen.
CREATE TABLE pack_steering (
    issue_slug TEXT NOT NULL,
    seq        BIGINT NOT NULL,
    body_md    TEXT NOT NULL,
    author     TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (issue_slug, seq)
);

CREATE TABLE autopilot (
    singleton  BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    enabled    BOOLEAN NOT NULL,
    changed_by TEXT,
    reason     TEXT,
    updated_at TEXT NOT NULL
);
