-- 0064_kube_identity_links.sql — the Kubernetes username each person acts as on each cluster,
-- minted into people's access tokens as `kube_user` for MCP servers on that cluster. `kube_user` is
-- only ever the cluster's own TokenReview answer for a token the person presented; the token itself
-- is never stored.
CREATE TABLE kube_identity_links (
    sub        TEXT NOT NULL,
    cluster    TEXT NOT NULL,
    kube_user  TEXT NOT NULL,
    source     TEXT NOT NULL,
    linked_at  TEXT NOT NULL,
    PRIMARY KEY (sub, cluster)
);
