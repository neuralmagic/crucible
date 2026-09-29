-- 0046_secret_audit_subject.sql — the subject a secret action was decided for, when it differs from
-- the actor: the team a caller acted as.
ALTER TABLE secret_audit ADD COLUMN subject TEXT;
