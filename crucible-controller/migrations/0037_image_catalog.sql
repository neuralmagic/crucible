-- 0037_image_catalog.sql — the sandbox images a pack may pick, read off the registries the
-- controller is configured to watch. One row per (repository, manifest digest); `tags` is the set
-- of channel tags currently pointing at that digest, `capabilities` the io.crucible.capabilities.v1
-- document off the image config, NULL when the image carries none (catalogued, unverified).
-- `catalog_repositories` records each watched repository's last poll so a registry outage shows
-- as a stale-with-error repository beside the cached rows rather than an empty catalog.
CREATE TABLE catalog_images (
    repository        TEXT NOT NULL,
    digest            TEXT NOT NULL,
    tags              JSONB NOT NULL,
    arches            JSONB NOT NULL,
    created_at        TEXT,
    capabilities      JSONB,
    capability_digest TEXT,
    intro_digest      TEXT,
    first_seen        TEXT NOT NULL,
    last_seen         TEXT NOT NULL,
    PRIMARY KEY (repository, digest)
);

CREATE TABLE catalog_repositories (
    repository  TEXT PRIMARY KEY,
    last_polled TEXT NOT NULL,
    last_ok     TEXT,
    last_error  TEXT
);
