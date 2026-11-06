-- 0081_artifact_object_key.sql — an artifact's payload can live in an S3-compatible bucket
-- instead of artifact_chunks. object_key names the object; NULL means the chunks hold the bytes.
ALTER TABLE artifacts ADD COLUMN object_key TEXT;
