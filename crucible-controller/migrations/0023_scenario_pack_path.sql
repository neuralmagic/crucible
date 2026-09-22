-- A scenario may name a pack the repo already carries, instead of asking an agent to draft one.
-- Relative to the checkout root; NULL keeps the propose path every scenario took before this.
ALTER TABLE scenarios ADD COLUMN pack_path TEXT;
