-- 0070_people_code_reuse.sql — a redeemed authorization code is kept, not deleted, so presenting it
-- again is recognised (OAuth 2.1 §4.1.3, RFC 9700 §4.5.3): `redeemed_at` marks it spent,
-- `grant_id` names the approval its redemption started, and `reused_at` marks a replay, which
-- revokes that approval and stops one from starting.
ALTER TABLE people_codes ADD COLUMN redeemed_at BIGINT;
ALTER TABLE people_codes ADD COLUMN grant_id TEXT;
ALTER TABLE people_codes ADD COLUMN reused_at BIGINT;
