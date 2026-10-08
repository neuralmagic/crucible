# MCP auth on pirate

Two authorization servers issue tokens for crucible's MCP servers; each MCP server is an OAuth
resource server built on the `crucible-mcp-auth` crate and trusts both, each against its own JWKS.

- **Sandboxed agents**: the crucible controller. The OpenShell supervisor's provider `token_grant`
  posts the sandbox's SPIRE JWT-SVID as a `client_credentials` `client_assertion` to the
  controller's `/oauth/token`; the controller maps the sandbox to the run whose pod bound it and
  returns a 5-minute ES256 token. The supervisor injects it only on egress to the bound MCP
  endpoint. See [the controller deploy guide](../../docs/controller-deploy.md#agent-tokens).
- **People**: Keycloak (realm `crucible`), logging in through OpenShift with client `claude-code`.
  A separate crucible-kueue behind a Route serves them, and Kubernetes RBAC decides every call as
  the person (`kube_user`, their OpenShift username).

```text
 run pod ──POST /api/pods/{pod}/sandboxes {sandbox_id, sandbox_name, workdir} (pod token)──▶ controller
 supervisor ──JWT-SVID (aud = controller issuer)──▶ controller /oauth/token ──access token──▶ supervisor
   sub=spiffe://pirate.crucible/openshell/sandbox/<id>     sub=<launcher>, act.sub=<spiffe id>,
                                                            aud=<MCP URL>, run_id, namespace,
                                                            queues, role, sandbox, workdir,
                                                            scope, jti
 supervisor ──Authorization: Bearer──▶ crucible-kueue ──▶ JWKS check, tool policy, Kueue
```

| File | Scope | What |
| --- | --- | --- |
| `operators.yaml` | cluster | RHBK 26.6 + Zero Trust Workload Identity Manager operators |
| `spire.yaml` | cluster | SPIRE server/agent DaemonSet/CSI driver/OIDC discovery, trust domain `pirate.crucible` |
| `clusterspiffeid.yaml` | cluster | SPIFFE IDs for supervisors in crucible-system and the spike pod in rhbk |
| `openshift-oauth-client.yaml` | cluster | OAuthClient for the openshift-v4 IdP |
| `keycloak.yaml` | rhbk | Postgres, Keycloak, edge Route |
| `realm.yaml` | rhbk | Realm import (IdPs, scopes, `claude-code`, the earlier `run-auth-spike` client) |
| `spike.yaml` | rhbk | Pod that fetches one JWT-SVID shaped like a supervisor's |
| `realm-dev.sh` | rhbk | Live realm changes an import cannot make (scopes, audiences, `kube_user`, login hardening); `grant` sets optional tenant attributes |
| `kueue-people.yaml`, `kueue-people.sh` | crucible-system, cluster | The person-facing crucible-kueue, its RBAC, and the Route `crucible-kueue.apps…` |
| `sar-e2e-rbac.yaml` | crucible-kueue-e2e | The e2e test user's grants: Jobs on `crucible-e2e`, no Deployments |
| `apply.sh` | - | Applies all of the above in order; safe to re-run |

## Apply

```bash
crucible-mcp-auth/deploy/apply.sh
```

`realm.yaml` is a `KeycloakRealmImport`, which only creates a realm that does not exist; changes
to it after the first import go through the admin console or API.

## Realm

People log in through the `openshift-v4` IdP with the `claude-code` client. The `spire` IdP and
the `run-auth-spike` federated-jwt client in `realm.yaml` date from the per-run-client design and
no longer take part in agent auth: Keycloak's federated-jwt matches `jwt.credential.sub` exactly,
so it needed a client per sandbox, which the controller issuer replaces.

## crucible-kueue

```bash
crucible-kueue \
  --issuer http://crucible-controller.tms.svc.cluster.local:8080=http://crucible-controller.tms.svc.cluster.local:8080/.well-known/jwks.json \
  --issuer https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible=http://keycloak-service.rhbk.svc.cluster.local:8080/realms/crucible/protocol/openid-connect/certs \
  --resource http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp \
  --tool-policy /etc/crucible-kueue/kueue-policy.toml \
  --tenant-namespace crucible-kueue-tenants
```

The policy file ships as `crucible-kueue/deploy/kueue-policy.toml`.

Tenant tokens keep working alongside access tokens; each request logs `auth=oidc` or
`auth=tenant-token`. A run's token carries `run_id`, `namespace`, `role` and maybe `queues`, and is
pinned to that namespace. Add `--kube-authz openshift` and the policy's authorize rules run as
SubjectAccessReviews for any token with `kube_user`, and people's tokens (no `run_id`) are
accepted: they name a namespace per call and RBAC decides. The launcher is stamped on created
workloads as `crucible.dev/launched-by`, and the shipped policy records `act.sub` as the
`crucible.dev/actor` annotation.

Unauthenticated requests get `401` with
`WWW-Authenticate: Bearer resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`;
the metadata document (RFC 9728) names both issuers as authorization servers.

## OpenShell provider

The sandbox side needs the embedded gateway to have `providers_v2_enabled` set and
`provider_spiffe_workload_api_socket_path = "/spiffe-workload-api/spire-agent.sock"` in its
`[openshell.drivers.kubernetes]` block (the driver then mounts `csi.spiffe.io` into the supervisor
pod). The provider profile's credential:

```toml
[token_grant]
token_endpoint = "http://crucible-controller.tms.svc.cluster.local:8080/oauth/token"
jwt_svid_audience = "http://crucible-controller.tms.svc.cluster.local:8080"
audience = "http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp"
scopes = ["kueue.read", "kueue.submit"]
```

`jwt_svid_audience` must equal the controller's `CONTROLLER_AGENT_ISSUER` exactly; left empty,
OpenShell would request the token endpoint URL. Pod networks cannot resolve `*.apps` hosts on
pirate, so in-cluster Service URLs are the only ones a supervisor can reach.

## People and Claude Code

`kueue-people.sh` runs a second crucible-kueue for people beside the sandbox-facing one:

```bash
IMAGE=quay.io/wseaton/crucible-kueue@sha256:<digest> crucible-mcp-auth/deploy/kueue-people.sh
```

It takes realm tokens only, with keys from the in-cluster Keycloak Service. It runs the shipped
policy with `--kube-authz openshift` and serves `https://crucible-kueue.apps.openshift.nm.rdu4.corp.redhat.com/mcp`
through an edge Route. Its service account may create SubjectAccessReviews and list OpenShift
groups, and gets workload verbs in every namespace holding a LocalQueue (`PEOPLE_NAMESPACES`
overrides). Rerun it after adding a queue.

No realm attributes or `grant` are needed: RBAC on the person's OpenShift user decides. `claude-code`
gets `kueue.read` and `kueue.submit` by default, because Claude Code requests only the scope a 401
names. Usernames are trustworthy because only the OpenShift login creates users, it re-syncs the
name every time, and registration, username edits and the first-login review are off.

From a laptop (pirate's `*.apps` has no DNS and its ingress CA is self-signed):

```bash
echo "10.14.217.8 keycloak.apps.openshift.nm.rdu4.corp.redhat.com crucible-kueue.apps.openshift.nm.rdu4.corp.redhat.com" | sudo tee -a /etc/hosts
oc -n openshift-config-managed get configmap default-ingress-cert -o jsonpath='{.data.ca-bundle\.crt}' > ~/.config/crucible/pirate-ingress-ca.pem
claude mcp add --transport http kueue https://crucible-kueue.apps.openshift.nm.rdu4.corp.redhat.com/mcp --client-id claude-code --callback-port 8765
env NODE_EXTRA_CA_CERTS=$HOME/.config/crucible/pirate-ingress-ca.pem claude
```

In Claude Code, `/mcp`, select `kueue`, then Authenticate. Pick openshift-v4 in the browser and log
in. Name the namespace in each request: "plan and submit a Job named smoketest on queue
crucible-e2e in namespace crucible-kueue-e2e". Every call logs `subject access review
user=<you> … allowed=…` in `oc -n crucible-system logs deploy/crucible-kueue-people`.

## Spike and e2e

```bash
CRUCIBLE_KUEUE_E2E_CONTEXT=crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton \
CRUCIBLE_KUEUE_E2E_NAMESPACE=crucible-kueue-e2e CRUCIBLE_KUEUE_E2E_QUEUE=crucible-e2e \
CRUCIBLE_E2E_INGRESS_IP=10.14.217.8 \
CRUCIBLE_KUEUE_E2E_DATABASE_URL=postgres://postgres@localhost:55432/crucible_oidc_e2e \
cargo test -p crucible-kueue --test oidc_e2e -- --ignored --nocapture --test-threads=1
```

The crucible-kueue e2e recreates `spike.yaml` with the controller's issuer as the SVID audience,
binds the spike's sandbox to a seeded run in a local controller ledger, exchanges the SVID at the
controller's token endpoint (served on loopback, verifying against SPIRE's JWKS through the
discovery Route) exactly as the supervisor does, and drives crucible-kueue on loopback with the
resulting token and its shipped policy. It then collects the run and checks the same SVID is
refused. `kubernetes_rbac_decides_what_the_launcher_may_create` needs `sar-e2e-rbac.yaml` applied:
the realm's `run-auth-spike` token carries `kube_user crucible-e2e-sar`, and a Deployment plan must
be refused by RBAC. buildit's e2e still exchanges at the realm's `run-auth-spike` client.
