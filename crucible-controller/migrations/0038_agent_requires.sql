-- 0038_agent_requires.sql — what a pack's agent requires of its sandbox image, and what image a
-- run actually got. `agent_requirements` is the rest of the manifest's `[agent]` table the
-- capability preflight reads: the declared harness, `[agent.requires]`, `[agent.prefers]`, and
-- the `allow_unverified_image` override, as one JSON document beside the backend and image
-- columns 0014 added. NULL is "not recorded", as for those; the startup backfill fills it.
ALTER TABLE playbooks ADD COLUMN agent_requirements JSONB;
ALTER TABLE pack_imports ADD COLUMN agent_requirements JSONB;
ALTER TABLE playbook_draft_versions ADD COLUMN agent_requirements JSONB;

-- The image provenance of a run (C-IDENTITY): the reference the manifest named, the digest the
-- catalog resolved it to and the capability document's digest at dispatch, and whether the launch
-- went through on the unverified-image override rather than a capability match.
ALTER TABLE runs ADD COLUMN image_ref TEXT;
ALTER TABLE runs ADD COLUMN image_digest TEXT;
ALTER TABLE runs ADD COLUMN capability_digest TEXT;
ALTER TABLE runs ADD COLUMN image_override BOOLEAN NOT NULL DEFAULT FALSE;
