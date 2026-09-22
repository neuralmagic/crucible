-- The credentials a proposed pack declares, frozen with the rest of the preview. The compile
-- endpoint recomputed them from the tarball, so the review page had nothing to show until an
-- operator edited a parameter; the declarations belong to the bytes, not to the values.
ALTER TABLE pack_imports ADD COLUMN declared_secrets JSONB NOT NULL DEFAULT '[]'::jsonb;
