-- 0073_blueprints.sql — registered playbooks offered as starting points (RFC-0006), the bindings
-- a draft started from one carried, and the placeholders it still has to fill.
ALTER TABLE playbooks
    ADD COLUMN blueprint BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN public    BOOLEAN NOT NULL DEFAULT false,
    ADD CONSTRAINT playbooks_public_is_a_blueprint CHECK (blueprint OR NOT public);

ALTER TABLE secret_bindings
    ADD COLUMN origin         TEXT,
    ADD COLUMN origin_binding TEXT;
CREATE INDEX secret_bindings_origin ON secret_bindings (origin)
    WHERE origin IS NOT NULL;

CREATE TABLE draft_placeholders (
    draft_id        TEXT NOT NULL
        REFERENCES playbook_drafts(id) ON DELETE CASCADE ON UPDATE CASCADE,
    declared_name   TEXT NOT NULL,
    projection_kind TEXT,
    projection      TEXT,
    reason          TEXT NOT NULL,
    PRIMARY KEY (draft_id, declared_name),
    CHECK ((projection_kind IS NULL) = (projection IS NULL))
);
