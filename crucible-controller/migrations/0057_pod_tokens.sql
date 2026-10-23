-- 0057_pod_tokens.sql — a scope turn's pod holds a token too, for its pack and transcript, so run
-- tokens become pod tokens (RFC-0004:C-RUN-EVIDENCE). A token names exactly one run or turn pod.
ALTER TABLE run_tokens RENAME TO pod_tokens;
ALTER INDEX run_tokens_run RENAME TO pod_tokens_run;
ALTER TABLE pod_tokens ALTER COLUMN run_id DROP NOT NULL;
ALTER TABLE pod_tokens ADD COLUMN turn_pod TEXT;
ALTER TABLE pod_tokens ADD CONSTRAINT pod_tokens_one_subject
    CHECK ((run_id IS NULL) <> (turn_pod IS NULL));
CREATE INDEX pod_tokens_turn ON pod_tokens (turn_pod);
