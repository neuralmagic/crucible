-- 0060_user_kube_user.sql — the Kubernetes username each person acts as on the controller's own
-- cluster, minted into agent tokens as `kube_user` so crucible MCP servers check RBAC as the run's
-- launcher. `kube_user` is learned from the API server (`users/~`) when the person presents a
-- cluster token; `kube_user_override` is set by a platform administrator and wins.
ALTER TABLE users ADD COLUMN kube_user TEXT;
ALTER TABLE users ADD COLUMN kube_user_override TEXT;
