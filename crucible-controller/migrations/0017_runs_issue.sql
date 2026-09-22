-- A run belongs to an issue directly. Autoresearch runs reached theirs through `scope`;
-- a playbook launch has no scope row, so its runs were only findable by string-matching
-- the run id against the sanitized launch key.
ALTER TABLE runs ADD COLUMN issue TEXT REFERENCES issues(key);

UPDATE runs r SET issue = s.issue FROM scopes s WHERE r.scope = s.id;

UPDATE runs r SET issue = l.key
FROM playbook_launches l
WHERE r.issue IS NULL
  AND starts_with(r.run_id, replace(replace(replace(l.key, '/', '_'), '#', '_'), ':', '_') || '-');

CREATE INDEX runs_issue_seq ON runs (issue, seq DESC);

-- Launches the scope-only lookup wedged: the self-heal saw no live run and bounced the issue to
-- awaiting-approval while its run row stayed running. Now that the row links back, return them
-- to running so the completion edge folds the run its pod already finished.
WITH wedged AS (
    SELECT i.key
    FROM issues i
    JOIN playbook_launches l ON l.key = i.key
    WHERE i.status = 'awaiting-approval'
      AND EXISTS (SELECT 1 FROM runs r WHERE r.issue = i.key AND r.status = 'running')
)
INSERT INTO events (v, ts, key, from_status, to_status, reason, evidence, actor)
SELECT 1, to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'), key,
       'awaiting-approval', 'running',
       'live playbook run relinked to its issue; back to running for the completion edge',
       NULL, NULL
FROM wedged;

UPDATE issues SET status = 'running'
WHERE status = 'awaiting-approval'
  AND key IN (SELECT l.key FROM playbook_launches l)
  AND EXISTS (SELECT 1 FROM runs r WHERE r.issue = issues.key AND r.status = 'running');
