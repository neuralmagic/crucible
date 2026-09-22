-- The ranker's affinity verdict (perf | perf-adjacent | unrelated), persisted at rank time so
-- the UI can hint/filter on it. NULL = ranked before this column existed (or never ranked).
ALTER TABLE issues ADD COLUMN affinity TEXT
  CHECK (affinity IN ('perf', 'perf-adjacent', 'unrelated'));
