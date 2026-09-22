-- Drop-box payloads live in artifacts/artifact_chunks now; the pointer row keeps
-- only the content address.
ALTER TABLE pod_artifacts DROP COLUMN path;
