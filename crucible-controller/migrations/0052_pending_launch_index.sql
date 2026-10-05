-- The launch loop probes for undispatched playbook launches every few seconds.
CREATE INDEX issues_pending_launch ON issues (updated_at)
    WHERE status = 'new' AND input_kind = 'playbook';
