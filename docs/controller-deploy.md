# Deploying the controller

The controller is the control plane around the engine: a Postgres ledger of issues, scopes,
runs and launches, a daemon that reconciles them into loop pods, and an HTTP API that serves
the embedded UI, `/api`, and the hosted MCP surface at `/mcp`. One replica per cluster. It is
the binary `crucible-controller`, shipped as `ghcr.io/neuralmagic/crucible-controller` on top
of the runtime image, so the engine binaries it launches are in the same image.

This page covers what the controller needs from you and how it is wired. Identity is on
[Authentication](./controller-auth.md); the secrets registry is on [Vault](./controller-vault.md).
To try it on a laptop first, see [Running the controller locally](./controller-local.md).

## What it needs

| Need | How it is given | Notes |
| --- | --- | --- |
| Postgres | `DATABASE_URL` | The daemon migrates the schema on open; there is no separate migrate step. `crucible-controller db verify` proves the migration set against a scratch database. `embedded` runs a Postgres of its own, for [local use](./controller-local.md#the-embedded-database). |
| A bind address | `CONTROLLER_API_ADDR` | Without `CONTROLLER_API_TOKEN` the API binds loopback only, whatever address you give it. |
| A static bearer | `CONTROLLER_API_TOKEN` | The deployment's machine credential, for CD and the cluster-internal Service. Required for any routable bind. |
| A GitHub token | `GITHUB_TOKEN` | Discovery and triage read the watched repos with it; public repos need only public read. |
| Cluster access | in-cluster ServiceAccount | Pod `get/list/watch/create` in the loop namespace (`CONTROLLER_POD_NAMESPACE`). Spokes are separate, below. |
| Scratch space | `CONTROLLER_SCRATCH_DIR` | Repo checkouts and the flow cache, all regenerable. An `emptyDir` is enough. |
| A public URL | `CONTROLLER_PUBLIC_URL` | What the UI, the MCP handoff and emitted links are built against. |

Everything durable is in Postgres. The controller's own pod can be replaced at will.

## Running it

```sh
export DATABASE_URL=postgres://crucible:...@postgres:5432/crucible
export CONTROLLER_API_ADDR=0.0.0.0:8080
export CONTROLLER_API_TOKEN=$(openssl rand -hex 32)
export CONTROLLER_PUBLIC_URL=https://crucible.example.com
export CONTROLLER_WATCHED_REPOS=your-org/your-repo
export CONTROLLER_POD_NAMESPACE=crucible-loops
export GITHUB_TOKEN=ghp_...
crucible-controller autopilot
```

`autopilot` is the resident daemon: discovery, ranking, scoping, launching and recording,
plus the API. `autopilot --once` drains the queue and exits, for a one-shot reconcile.
Every flag has a `CONTROLLER_*` environment form; `crucible-controller autopilot --help`
lists them all with their defaults.

The admission caps bound what the daemon may spend on its own:

| Variable | Default | Bounds |
| --- | --- | --- |
| `CONTROLLER_MAX_CONCURRENT_PODS` | 2 | loop pods running at once |
| `CONTROLLER_MAX_SCOPES_PER_DAY` | 20 | scopes admitted per UTC day |
| `CONTROLLER_DAILY_COST_CEILING_USD` | 50 | agent spend per UTC day |
| `CONTROLLER_PER_RECONCILE_COST_USD` | 10 | spend one reconcile may commit |

Autopilot can be switched off at runtime from the admin page; that pauses machine-initiated
spend and nothing else.

## The autoresearch lane

Playbooks are the controller's core: the registry, drafts, launches, schedules, watches and
their runs. The autoresearch lane is separate and off by default. It covers GitHub discovery
and triage, ranking, scope proposal and its approval gate, image builds, and scored loop runs.
It takes two switches:

- the `autoresearch` cargo feature, which compiles it in
  (`cargo build -p crucible-controller --features autoresearch`; the published image and
  release binaries carry it), and
- `CONTROLLER_AUTORESEARCH=true`, which turns it on in a build that has it. The controller
  refuses to start with the variable set on a build without the feature.

With the lane off, its routes are not served, its discovery polls do not run, and a row that is
not a playbook launch is left where it is. `GET /api/version` reports `autoresearch`, and the UI
hides the lane's pages. The engine has the matching `autoresearch` feature for the scored loop
and `crucible scope`; a deployment that turns the lane on needs both.

## On Kubernetes

The controller runs as a Deployment. In a cluster, replicas contend on a Lease
(`CONTROLLER_LEASE_NAME`) and only the holder reconciles, so a rolling update never has two
daemons acting on the check-then-act caps at once; `CONTROLLER_LEADER_ELECTION=off` disables
that for a single-replica deployment that would rather not hold a Lease.

A minimal set of objects:

- a **ServiceAccount** for the controller, with a **Role** in the loop namespace bound to it:
  `pods` get/list/watch/create/delete and `pods/log` get (dispatch, the watch, and the log
  relay), `configmaps` and `secrets` create/delete (the pack ConfigMap and the delivered
  run secrets, both owner-referenced to the pod), and `leases` get/create/update/patch for
  leader election;
- a **Secret** for `CONTROLLER_API_TOKEN` and one for `GITHUB_TOKEN`, referenced by env;
- a **Service** on the API port;
- the **Deployment**, env from the table above, `emptyDir` at `CONTROLLER_SCRATCH_DIR`.

Put an OIDC login in front of it before exposing the Service outside the cluster; see
[Authentication](./controller-auth.md). The machine paths (`/api`, `/mcp`) can be exposed on
their own hostname since they authenticate by bearer.

The webhook delivery listener is a third surface, on its own port, meant to be the one thing
published to the internet. Its variables and the chart's `webhooks` values are in
[Launching from webhooks](./webhooks.md#deploying).

The image's labels carry the contract version the engine was built with
(`io.crucible.contract-version`); the controller reads the same label off every dispatch
image before it launches a pod, and refuses a mismatch.

## Where loops run

By default loop pods run in the controller's own cluster, in `CONTROLLER_POD_NAMESPACE`. A
deploy profile (`CONTROLLER_DEPLOY_PROFILE`, a TOML file) fixes the per-cluster facts a
launch needs: the loop image, pull secrets, the service account, the results bucket, and the
environment handed to the engine. The engine renders the loop pod from the profile in the
controller's own process, so a profile is validated when the controller starts, not when a
launch fails.

Spoke clusters, when the controller runs loops elsewhere:

- `CONTROLLER_CLUSTERS_DIR` holds one kubeconfig per spoke; `CONTROLLER_DISPATCH_CLUSTER`
  names the default target and `CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT` routes by the pack's
  broker contract.
- `CONTROLLER_CLUSTER_POLICY` restricts who may target which cluster
  (`name=group:/path;user:login`, comma-separated).
- A spoke the hub cannot reach reports inbound instead: `crucible-controller cluster-snapshot`
  runs there as a CronJob and posts to the hub's push endpoint under a bearer the hub lists in
  `CONTROLLER_PUSH_TOKENS`.

## Agent tokens

The controller can act as the OAuth authorization server for sandboxed agents. An OpenShell
sandbox supervisor posts its SPIRE JWT-SVID to `POST /oauth/token` (client credentials with a
`jwt-bearer` client assertion, no `client_id`); the controller verifies it against SPIRE's JWKS,
maps the sandbox to the run whose pod bound it, and returns an ES256 access token living 15 to 30 minutes. MCP
servers verify it against `/.well-known/jwks.json`; `/.well-known/oauth-authorization-server`
describes the endpoints. All three are served outside the human auth stack.

The token's `sub` is the person the run was dispatched for: whoever unparked, scoped,
redispatched, or approved the issue for that dispatch, else the launch's creator (the owner, for
a schedule, watch, or webhook). A GitHub review approval counts only when an operator paired the
reviewer's GitHub login with a crucible login in `CONTROLLER_GITHUB_LOGINS`
(`<github login>=<crucible login>`, comma-separated); equal names are never assumed to be the same
person. The request is consumed by the run it gets, so later
runs act for the owner again. An override by an actor that is not a login records nobody.
`act.sub` is the sandbox's SPIFFE ID. It also carries `run_id` (the run pod), `sandbox`
(the sandbox's name), `workdir` (its workspace path), `kube_user` (below) and, for runs on the
`[kueue]` cluster, `namespace` (crucible-kueue's default namespace for the run's workloads). Claims
name who is calling and for which run; what that identity may do is the resource server's to
decide (crucible-kueue runs SubjectAccessReview as `kube_user`). Refusals return only the OAuth
error code
and a fixed description; the reason is logged.

**Sandbox placement.** A sandbox's SPIFFE ID is only as trustworthy as whoever can create pods
where its supervisor runs, so the controller issues nothing until the deploy profile runs each
run's gateway in its own pod: `sandbox_driver = "kubernetes"` with `[cluster] sandbox_namespace`,
which profile load refuses to share with the loop, rig, or crucible-kueue namespaces. The
controller refuses to start the issuer when its own namespace (`POD_NAMESPACE`), where the signing
key Secret is mounted from, is any of those namespaces; a controller rendered by
`crucible deploy render` runs in the loop namespace and so cannot hold the key.

**Gateway identity.** Each run's gateway has its own OpenShell gateway id, the run pod's name. The
gateway stamps it as `openshell.ai/gateway-id` on every supervisor pod and Sandbox it creates, from
its own config, and `driver_config` cannot set labels or annotations. Scope the sandbox
ClusterSPIFFEID to the sandbox namespace and build the ID from that label and the supervisor's
`openshell.ai/sandbox-id` annotation:

```yaml
spiffeIDTemplate: 'spiffe://{{ .TrustDomain }}/openshell/gateway/{{ index .PodMeta.Labels "openshell.ai/gateway-id" }}/sandbox/{{ index .PodMeta.Annotations "openshell.ai/sandbox-id" }}'
```

`CONTROLLER_AGENT_GATEWAY_PREFIX` is everything before the gateway id
(`spiffe://<trust domain>/openshell/gateway/`).

The engine binds each sandbox it creates by posting its id and workdir to
`/api/pods/{pod}/sandboxes` with the pod's ingest token, so the drop-box env
(`CRUCIBLE_INGEST_URL`) must reach the loop pod. The controller binds only a `Sandbox` resource
(`agents.x-k8s.io`) in the sandbox namespace carrying that id and the run's gateway id, reads the
sandbox's name from it, and owner-references it to the gateway pod. At token issue it refuses an
SVID whose gateway id is not the bound run's. Name the controller's identity on each cluster in
`[cluster] sandbox_readers` (`namespace/name`) and the loop render grants what it needs (below). A
sandbox that was never bound, whose run pod is no longer running, or whose SVID names another
run's gateway gets `invalid_grant`.

**Isolated gateways.** A run's OpenShell gateway does not run in the loop pod. The loop render
creates the `crucible-openshell-gateway` ServiceAccount in the sandbox namespace and binds the
sandbox rights (Sandboxes, pods, secrets) to it alone; the loop service account gets no verbs
there. For each run the controller first creates the gateway Service in the sandbox namespace and
mints a CA, a server certificate for that Service's ClusterIP, and client certificates for the loop
pod and the supervisors, valid for the run's deadline plus a day (30 days without one); it keeps
no copy of the CA key. The loop pod's client material and the gateway URL arrive through the run
Secret. Before the loop pod it creates a loop Service (the sandboxes' `host.openshell.internal`),
a NetworkPolicy admitting only that run's loop pod and its own supervisors to the gateway port, the
gateway pod, and the server and supervisor TLS Secrets owned by that pod. Collection deletes the
gateway's Sandboxes and supervisors, the loop Service, then the gateway's own objects; a sweep on
the discovery tick does the same for any gateway whose run pod is no longer running. Dispatch to a
cluster the hub only receives pushes from is refused, since the hub cannot create the gateway
there.

For the controller's identity, `sandbox_readers` is granted: `sandboxes` get/list/patch/delete;
`pods` get/list/create/delete; `services`, `secrets` and `networkpolicies` list/create/delete in
the sandbox namespace; and `services` list/create/delete in the loop namespace.

Grounded-rank and scope turns get a gateway of their own the same way, with certificates valid for
the turn's deadline; a failed launch tears it down, and the gateway sweep removes it once the
turn's row closes. Turn sandboxes are never bound, so they get no agent tokens.

Supervisors get the SPIFFE Workload API (the `csi.spiffe.io` driver) only when the profile names
`[cluster] sandbox_spiffe_socket`, for example `/spiffe-workload-api/spire-agent.sock`.

With `[cluster] aws_sandbox_role_arn`, the gateway pod assumes that role with an STS-audienced
token for its own service account, as the in-pod gateway does with the loop pod's, so the role's
trust policy must allow `system:serviceaccount:<sandbox namespace>:crucible-openshell-gateway`.

Pack code holds its
run's gateway client certificate, so it can still create sandboxes for its own run with the
`driver_config` pod fields the gateway accepts (`node_selector`, `tolerations`, `host_aliases`,
`runtime_class_name`, `priority_class_name`, PVC volumes); resource admission keeps PVCs and
priority classes to those labelled `openshell.ai/sandbox-attachable=true`, and those sandboxes
carry its own gateway id. The pinned OpenShell (3441f91) predates v0.1.2: supervisors are separate
pods, and nothing here depends on the sidecar supervisor topology or other v0.1.2 features.

**`kube_user`.** Tokens carry the dispatcher's Kubernetes username when the controller knows it;
a server that sees it runs SubjectAccessReview as that user. It is learned from the API server
(`users/~`): when someone calls the API with a cluster token whose username equals their login
exactly (filling an empty value only), or when a signed-in person links one with
`PUT /api/me/kube-user` (`{"cluster_token": "<oc whoami -t>"}`). Another platform administrator
can override it with `PUT /api/users/{login}/kube-user` (`{"kube_user": "<name>"}`, or `null` to
clear it); the override is audited on `user:<login>`. `system:` identities are refused. With
`[kueue] require_kube_user` (default `true`), a token carrying crucible-kueue scopes is refused
when the username is unknown; other tokens are issued without the claim, logged at warn.

| Variable | Meaning |
| --- | --- |
| `CONTROLLER_AGENT_ISSUER` | The issuer identifier, an origin such as `http://crucible-controller.crucible-system.svc.cluster.local:8080`. Unset turns the endpoint off. Supervisors must request their SVID with exactly this audience (`token_grant.jwt_svid_audience`). |
| `CONTROLLER_AGENT_SIGNING_KEY_FILE` | A PKCS#8 P-256 private key mounted from a Secret. The `kid` is its RFC 7638 thumbprint. |
| `CONTROLLER_AGENT_PREVIOUS_SIGNING_KEY_FILE` | Optional: the key before a rotation. It is published in the JWKS beside the current key, so tokens it signed keep verifying until they expire; nothing new is signed with it. Rotate by moving the current key here and mounting a new one, then drop this after one token lifetime. |
| `CONTROLLER_AGENT_TOKEN_TTL_SECS` | Agent token lifetime, 900 to 1800 seconds. Default 900. |
| `CONTROLLER_AGENT_TOKEN_ENDPOINT` | Where sandbox supervisors reach the token endpoint when not at the issuer itself (a spoke reaching the controller through a route). Default `<issuer>/oauth/token`. |
| `CONTROLLER_AGENT_SVID_ISSUER` | SPIRE's OIDC discovery issuer, the `iss` of every JWT-SVID. SVIDs must carry `iat` and live at most 15 minutes. |
| `CONTROLLER_AGENT_SVID_JWKS_URI` | SPIRE's JWKS, usually the in-cluster discovery provider Service. It must be `https://` unless its host is loopback, since whoever answers that fetch can mint agent tokens. |
| `CONTROLLER_AGENT_SVID_CA_FILE` | Optional PEM bundle trusted for that fetch (the service CA for an `https://*.svc` endpoint). |
| `CONTROLLER_AGENT_SVID_JWKS_INSECURE_HTTP` | `true` accepts a plain-http JWKS URI on a non-loopback host, logged at warn. Default `false`. |
| `CONTROLLER_AGENT_GATEWAY_PREFIX` | The SPIFFE ID path before the gateway id, ending in `/`: `spiffe://<trust domain>/openshell/gateway/`. Supervisor IDs are `<prefix><gateway id>/sandbox/<sandbox id>`. |
| `CONTROLLER_AGENT_AUDIENCES` | Comma-separated `<resource url>=<scope> <scope>` entries: the MCP resources a token may be issued for (anything else is `invalid_target`) and the scopes a token for each may carry. |

A run dispatched onto the `[kueue]` cluster gets the server's URL (`CRUCIBLE_KUEUE_URL`), the
token endpoint (`CRUCIBLE_AGENT_TOKEN_ENDPOINT`) and the issuer as SVID audience
(`CRUCIBLE_AGENT_SVID_AUDIENCE`) as plain env. The engine registers a token-grant provider from
them, so each sandbox's supervisor obtains its own token and injects it on traffic to the server
alone; no token is minted per run and nothing is recorded.

The deploy profile's `[kueue] workload_namespace` is the `namespace` claim for runs on its cluster;
a run elsewhere gets none. A token's scopes are what its audience lists here. For example:

```bash
CONTROLLER_AGENT_AUDIENCES="http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp=kueue.read kueue.submit,http://host.openshell.internal:8849/mcp=buildit.read buildit.build"
```

```bash
openssl ecparam -genkey -noout -name prime256v1 | openssl pkcs8 -topk8 -nocrypt -out agent-signing.pem
oc create secret generic crucible-agent-signing-key --from-file=agent-signing.pem
```

## Observability

- `/metrics` serves Prometheus text on the API port.
- Tracing goes to stderr as leveled `tracing` events; set `OTEL_EXPORTER_OTLP_ENDPOINT` to
  also export OTLP spans over HTTP.
- `/healthz` answers once the database is reachable.
- The `GET /api/system` payload names the commit the binary was built from.

## The API and the CLI

`GET /api/openapi.json` is the API document, behind the same bearer as everything else.
`crux` is the CLI over that API and the tool library the controller hosts at `/mcp`:

```sh
cargo install --locked --git https://github.com/neuralmagic/crucible.git crux
export CONTROLLER_URL=https://crucible.example.com
export CONTROLLER_API_TOKEN=crk_...
crux whoami
```

The `crk_` key comes from the UI's Settings page and carries its owner's identity; the static
`CONTROLLER_API_TOKEN` also reaches `/api`, as `anonymous` or as the identity
`CONTROLLER_API_TOKEN_IDENTITY` pins to it, and never reaches `/mcp`. The crux README is the
single source for its flags, environment variables and config file.
