# Deploying the controller

The controller is the control plane around the engine. It has a Postgres ledger of issues,
scopes, runs, and launches, a daemon that reconciles them into loop pods, and an HTTP API.
The API serves the embedded UI, `/api`, and the hosted MCP server at `/mcp`. Run one replica
per cluster. The binary is `crucible-controller`. It ships as
`ghcr.io/neuralmagic/crucible-controller`, built on the runtime image, so the image also
contains the engine binaries that the controller launches.

This page describes what the controller needs and how its parts connect.
[Authentication](./controller-auth.md) describes identity, and [Vault](./controller-vault.md)
describes the secrets registry. To try the controller on a laptop first, see
[Running the controller locally](./controller-local.md).

## Requirements

| Requirement | Setting | Notes |
| --- | --- | --- |
| Postgres | `DATABASE_URL` | The daemon migrates the schema when it opens the database. There is no separate migrate step. `crucible-controller db verify` tests the migration set against a scratch database. `embedded` runs a separate Postgres, for [local use](./controller-local.md#the-embedded-database). |
| A bind address | `CONTROLLER_API_ADDR` | Without `CONTROLLER_API_TOKEN`, the API binds only loopback, whatever address you set. |
| A static bearer | `CONTROLLER_API_TOKEN` | The deployment's machine credential, for CD and the cluster-internal Service. Required for any routable bind. |
| A GitHub token | `GITHUB_TOKEN` | Discovery and triage use it to read the watched repositories. Public repositories need only public read access. |
| Cluster access | in-cluster ServiceAccount | Pod `get/list/watch/create` in the loop namespace (`CONTROLLER_POD_NAMESPACE`). Spokes are separate, below. |
| Scratch space | `CONTROLLER_SCRATCH_DIR` | Repository checkouts and the flow cache. The controller can create them again. An `emptyDir` is sufficient. |
| A public URL | `CONTROLLER_PUBLIC_URL` | The base URL for the UI, the MCP handoff, and emitted links. |

All durable state is in Postgres. You can replace the controller pod at any time.

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

`autopilot` is the resident daemon. It does discovery, ranking, scoping, launching, and
recording, and it serves the API. `autopilot --once` processes the queue and exits, for a
one-time reconcile. Each flag has a `CONTROLLER_*` environment variable.
`crucible-controller autopilot --help` lists all of them with their defaults.

The admission caps limit what the daemon can spend without a person:

| Variable | Default | Bounds |
| --- | --- | --- |
| `CONTROLLER_MAX_CONCURRENT_PODS` | 2 | loop pods running at once |
| `CONTROLLER_MAX_SCOPES_PER_DAY` | 20 | scopes admitted per UTC day |
| `CONTROLLER_DAILY_COST_CEILING_USD` | 50 | agent spend per UTC day |
| `CONTROLLER_PER_RECONCILE_COST_USD` | 10 | spend one reconcile may commit |

You can disable autopilot at run time on the admin page. This pauses only the spend that the
daemon starts.

## The autoresearch lane

Playbooks are the core of the controller: the registry, drafts, launches, schedules,
watches, and their runs. The autoresearch lane is separate and disabled by default. It
contains GitHub discovery and triage, ranking, scope proposal and its approval gate, image
builds, and scored loop runs. Two settings enable it:

- the `autoresearch` cargo feature, which compiles the lane
  (`cargo build -p crucible-controller --features autoresearch`). The published image and
  release binaries include it.
- `CONTROLLER_AUTORESEARCH=true`, which enables the lane in a build that has it. If the
  variable is set on a build without the feature, the controller does not start.

When the lane is disabled, the controller does not serve its routes or run its discovery
polls, and it does not change rows that are not playbook launches. `GET /api/version` reports
`autoresearch`, and the UI hides the pages of the lane. The engine has a matching
`autoresearch` feature for the scored loop and `crucible scope`. A deployment that enables
the lane needs both.

## On Kubernetes

The controller runs as a Deployment. In a cluster, replicas compete for a Lease
(`CONTROLLER_LEASE_NAME`), and only the holder reconciles. During a rolling update, two
daemons do not apply the check-then-act caps at the same time.
`CONTROLLER_LEADER_ELECTION=off` disables leader election for a single-replica deployment
that does not use a Lease.

A minimal set of objects:

- a **ServiceAccount** for the controller, with a **Role** in the loop namespace bound to it:
  `pods` get/list/watch/create/delete and `pods/log` get (dispatch, the watch, and the log
  relay); `pods/finalizers` update (the pod's ConfigMap and Secret block its deletion, which
  OpenShift's owner-reference admission checks); `configmaps` and `secrets` create/delete (the
  pack ConfigMap and the run Secret, which holds the pod token and the delivered secrets, both
  owner-referenced to the pod); and `leases` get/create/update/patch for leader election;
- a **Secret** for `CONTROLLER_API_TOKEN` and one for `GITHUB_TOKEN`, referenced by env;
- a **Service** on the API port;
- the **Deployment**, env from the table above, `emptyDir` at `CONTROLLER_SCRATCH_DIR`.

Before you expose the Service outside the cluster, put an OIDC login in front of it. See
[Authentication](./controller-auth.md). You can expose the machine paths (`/api`, `/mcp`) on
a separate hostname, because they authenticate with a bearer token.

The webhook delivery listener is a third endpoint, on a separate port. It is the only part
that you publish to the internet. [Launching from webhooks](./webhooks.md#deploying) gives its
variables and the `webhooks` values of the chart.

The image labels contain the contract version of the engine
(`io.crucible.contract-version`). Before the controller launches a pod, it reads the same
label from the dispatch image. If the versions are different, it does not launch the pod.

## Where loops run

By default, loop pods run in the cluster of the controller, in `CONTROLLER_POD_NAMESPACE`.
A deploy profile (`CONTROLLER_DEPLOY_PROFILE`, a TOML file) sets the cluster values that a
launch needs: the loop image, pull secrets, the service account, the results bucket, and the
environment for the engine. The engine renders the loop pod from the profile in the
controller process, so the controller validates the profile when it starts, not when a launch
fails.

If the controller runs loops in other clusters (spokes):

- `CONTROLLER_CLUSTERS_DIR` holds one kubeconfig for each spoke. `CONTROLLER_DISPATCH_CLUSTER`
  names the default target. `CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT` selects the cluster by
  the broker contract of the pack.
- `CONTROLLER_CLUSTER_POLICY` limits which users can target which cluster
  (`name=group:/path;user:login`, comma-separated).
- If the hub cannot reach a spoke, the spoke sends reports to the hub.
  `crucible-controller cluster-snapshot` runs on the spoke as a CronJob and posts to the push
  endpoint of the hub with a bearer token that the hub lists in `CONTROLLER_PUSH_TOKENS`.

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
where its supervisor runs, so with `CONTROLLER_AGENT_SPIRE` set the controller refuses to start
unless the deploy profile runs each run's gateway in its own pod: `sandbox_driver = "kubernetes"`
with `[cluster] sandbox_namespace`, which profile load refuses to share with the loop, rig, or
crucible-kueue namespaces. It also refuses to start the issuer when its own namespace, where the
signing key Secret is mounted from, is any of those namespaces or `[buildit] namespace` (the loop
account can exec into any pod there), and when a SPIRE's namespace in `CONTROLLER_AGENT_SPIRE` is
one pack code reaches (loop, rig, sandbox, buildit or crucible-kueue workload namespace), since a
pod there could exec into the SPIRE server and mint any SVID. Its namespace is `POD_NAMESPACE`,
else the service account's namespace file; a controller rendered by `crucible deploy render` runs
in the loop namespace and so cannot hold the key.

**Gateway identity.** Each run's gateway has its own OpenShell gateway id, the run pod's name. The
gateway stamps it as `openshell.ai/gateway-id` on every supervisor pod and Sandbox it creates, from
its own config, and `driver_config` cannot set labels or annotations. Scope the sandbox
ClusterSPIFFEID to the sandbox namespace and build the ID from that label and the supervisor's
`openshell.ai/sandbox-id` annotation:

```yaml
spiffeIDTemplate: 'spiffe://{{ .TrustDomain }}/openshell/gateway/{{ index .PodMeta.Labels "openshell.ai/gateway-id" }}/sandbox/{{ index .PodMeta.Annotations "openshell.ai/sandbox-id" }}'
```

Each cluster's `CONTROLLER_AGENT_SPIRE` entry names its gateway prefix, everything before the
gateway id (`spiffe://<trust domain>/openshell/gateway/`). The controller reads that SPIRE's
keys from its OIDC discovery provider through the cluster's API server Service proxy, with the
same client it dispatches with, so its identity on each cluster needs `get` on `services/proxy`
in SPIRE's namespace. The `iss` of an SVID picks the cluster whose keys verify it, and a token is
issued only when the sandbox's run is on that cluster.

The engine binds each sandbox it creates by posting its id and workdir to
`/api/sandbox-bindings/runs/{run}` with the run's pod token (`CRUCIBLE_POD_TOKEN`, the same token that
uploads its evidence), and only when the loop pod carries `CRUCIBLE_AGENT_TOKEN_ENDPOINT`. A spoke
that reaches the controller through an external route needs `/api/sandbox-bindings` and
`/oauth/token` there, beside `/api/evidence` and the issuer's `/.well-known/jwks.json`. The
controller binds only a `Sandbox` resource
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
`pods` get/list/create/delete and `pods/finalizers` update (the gateway's Secrets block its
pod's deletion); `services`, `secrets` and `networkpolicies` list/create/delete in the sandbox
namespace; and `services` list/create/delete in the loop namespace.

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

**`kube_user`.** Tokens carry the dispatcher's Kubernetes username on the run's cluster when the
controller knows it, by the same rule people's tokens use there (Settings > Cluster identities,
[Cluster identities](controller-auth.md#cluster-identities)); a server that sees it runs
SubjectAccessReview as that user. A `link` cluster's username is the one the person linked there.
On the hub a platform administrator can override it with `PUT /api/users/{login}/kube-user`
(`{"kube_user": "<name>"}`, or `null` to clear it); the override is audited on `user:<login>`, and
no administrator sets their own. `system:` identities are refused. crucible-kueue acts only as a
token's `kube_user`, so a token carrying crucible-kueue scopes is refused when the username is
unknown; other tokens are issued without the claim, logged at warn. The linking and override routes
are served whenever the controller mints agent tokens, people tokens, or has a `[cluster_link]`
table.

| Variable | Meaning |
| --- | --- |
| `CONTROLLER_AGENT_ISSUER` | The issuer identifier, an origin such as `http://crucible-controller.crucible-system.svc.cluster.local:8080`. Unset turns the endpoint off. Supervisors must request their SVID with exactly this audience (`token_grant.jwt_svid_audience`). |
| `CONTROLLER_AGENT_SIGNING_KEY_FILE` | A PKCS#8 P-256 private key mounted from a Secret. The `kid` is its RFC 7638 thumbprint. |
| `CONTROLLER_AGENT_SIGNING_KEY` | The same key inline, as PEM, in place of the file. |
| `CONTROLLER_AGENT_PREVIOUS_SIGNING_KEY_FILE` | Optional: the key before a rotation. It is published in the JWKS beside the current key, so tokens it signed keep verifying until they expire; nothing new is signed with it. Rotate by moving the current key here and mounting a new one, then drop this after one token lifetime. |
| `CONTROLLER_AGENT_TOKEN_TTL_SECS` | Agent token lifetime, 900 to 1800 seconds. Default 900. |
| `CONTROLLER_AGENT_TOKEN_ENDPOINT` | Where sandbox supervisors reach the token endpoint when not at the issuer itself (a spoke reaching the controller through a route). Default `<issuer>/oauth/token`. |
| `CONTROLLER_AGENT_SPIRE` | Comma-separated `<cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>` entries, one per cluster hosting isolated gateways (`hub` for the controller's own): the `iss` of that SPIRE's JWT-SVIDs, its gateway prefix (`spiffe://<trust domain>/openshell/gateway/`; supervisor IDs are `<prefix><gateway id>/sandbox/<sandbox id>`; one path segment may be `{tenant}`, as in `spiffe://<trust domain>/openshell/{tenant}/gateway/`, to match IDs with a DNS-label tenant there), and its OIDC discovery provider Service, whose `/keys` is read through that cluster's API server (over https, or http when the Service is prefixed `http://`). Each cluster and each issuer appears once. SVIDs must carry `iat` and live at most 15 minutes; SPIRE's upstream chart issues 1-hour JWT-SVIDs by default, so give the supervisors' ClusterSPIFFEID `jwtTtl: 5m`. Unset, the issuer mints no agent tokens and serves people only. |
| `CONTROLLER_AGENT_AUDIENCES` | Comma-separated `[<cluster>:]<resource url>=<scope> <scope>` entries: the MCP resources a token may be issued for (anything else is `invalid_target`) and the scopes a token for each may carry. With `<cluster>:` an entry serves runs on that cluster only, so the same in-cluster URL can carry different scopes per cluster; without it the entry serves every cluster. Agent tokens carry the run's cluster as `kube_cluster`; start each crucible-kueue with `--cluster <name>` so it refuses tokens for another cluster's server at the same URL. Required with `CONTROLLER_AGENT_SPIRE`. |
| `CONTROLLER_PEOPLE_AUDIENCES` | Comma-separated `<cluster>:<resource url>=<scope> <scope>` entries: the MCP servers people's clients may get tokens for through `/oauth/authorize`, the cluster each acts on (whose identity rule gives the `kube_user`), and the scopes it grants. Unset turns people's tokens off. See [People's tokens](controller-auth.md#peoples-tokens-and-refresh). |
| `CONTROLLER_PEOPLE_ACCESS_TTL_SECS` | People's access token lifetime, 1 to 3600 seconds. Default 900. |
| `CONTROLLER_PEOPLE_CLIENTS_FILE` | A TOML file of `[[clients]]` (`client_id`, `name`, `redirect_uris`, optional `resources`): the public clients people may sign in with. It replaces the built-in `claude-code`, `pi` and `codex`, so list those too to keep them. Unset keeps the built-ins. A malformed file, or a `resources` entry missing from `CONTROLLER_PEOPLE_AUDIENCES`, stops the controller at startup. See [MCP clients](controller-auth.md#mcp-clients). |
| `CONTROLLER_PEOPLE_DCR` | `false` turns off self-registration (`POST /oauth/register`, RFC 7591, loopback public clients only), and the metadata then advertises no `registration_endpoint`. On by default with `CONTROLLER_PEOPLE_AUDIENCES`, which is what lets pi, Codex and Claude Code sign in without a client id. Registrations idle for 30 days are deleted; each source address may register 20 times an hour, and about 10000 live at once (a soft cap across replicas). See [Self-registered clients](controller-auth.md#self-registered-clients). |
| `CONTROLLER_PEOPLE_TRUSTED_PROXIES` | Comma-separated networks or addresses (`10.128.0.0/14`) whose `X-Forwarded-For` names a registration's source address, such as the cluster router's. Unset, every registration counts against its TCP peer. |
| `CONTROLLER_PEOPLE_CIMD_HOSTS` | Comma-separated host names (`chatgpt.com,pi.dev`) allowed to publish Client ID Metadata Documents. Set, an `https://` `client_id` on one of these hosts is fetched and accepted as a public client, and the metadata advertises `client_id_metadata_document_supported`. Unset (the default) refuses every URL `client_id`. The controller needs outbound HTTPS to these hosts; it honours `HTTPS_PROXY`. |

A run dispatched onto one of the `[kueue]` clusters (`clusters = ["spoke-a", "spoke-b"]`, or
`cluster = "spoke-a"`), or of any server the profile describes, gets each server's name, URL,
scopes and transport (`CRUCIBLE_AGENT_SERVERS`), the token endpoint
(`CRUCIBLE_AGENT_TOKEN_ENDPOINT`) and the issuer as SVID audience (`CRUCIBLE_AGENT_SVID_AUDIENCE`)
as plain env. The engine registers a token-grant provider for each, so each sandbox's supervisor obtains its own token and injects it on traffic to the server
alone; no token is minted per run. A server is granted only on a cluster in
`CONTROLLER_AGENT_SPIRE` whose URL is in `CONTROLLER_AGENT_AUDIENCES`. The run records what it got,
with the audience's scopes and, for a server that acts as `kube_user` such as crucible-kueue, the
dispatcher's username there, and the run page shows it. A third server needs an
`[[agent_servers]]` entry and its URL in `CONTROLLER_AGENT_AUDIENCES`, nothing else.

The deploy profile describes each server that takes agent tokens (`DeployProfile::agent_servers`):
`[kueue]` needs a `kube_user` in every token and names `workload_namespaces`, the first as the
`namespace` claim, for runs on its clusters; `[buildit]` needs neither. An audience the profile does
not describe gets no `kube_user` requirement and no namespace. `[kueue] run_issuer` must equal `CONTROLLER_AGENT_ISSUER`: the rendered
server trusts runs' tokens from it, fetching keys from `<run_issuer>/.well-known/jwks.json` unless
`run_jwks_uri` names another URI (a route, for a server that cannot reach the issuer's address).
`kube_authz` (`openshift` or `token`) is where the server takes a caller's groups for its
SubjectAccessReviews. With `people = true` the rendered server also accepts people's tokens from
the same issuer (`--person-issuer`), for a deployment whose `CONTROLLER_PEOPLE_AUDIENCES` names it.
A token's scopes are what its audience lists here. For example:

```bash
CONTROLLER_AGENT_AUDIENCES="http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp=kueue.read kueue.submit,http://host.openshell.internal:8848/mcp=buildit.read buildit.build"
```

**buildit.** A run dispatched onto one of the `[buildit]` clusters starts `buildit mcp` in its loop
pod on port 8848, and its sandboxes reach it at `http://host.openshell.internal:8848/mcp` with
tokens their supervisors obtain like crucible-kueue's. buildit takes only tokens for that
audience whose `run_id` is its own run, reads each build's context from the token's `sandbox`
and `workdir` through the run's gateway, and runs builder pods in `[buildit] namespace`, which
profile load refuses to share with the loop, rig, sandbox or crucible-kueue namespaces. The loop
render grants the loop service account `pods` create/get/list/watch/delete, `pods/exec` and
`pods/log` there and nowhere else, since those rights in the loop namespace would reach every run's
loop pod and Secret. Create the namespace before the first run; builder pods run as its `default`
service account, so on OpenShift bind that account to an SCC that admits them (uid 1000). The
rights still let runs see each other's builder pods in that namespace, including a push's
authfile while it is copied in; buildit under a service account of its own is the clean end state.
A build's `RUN` steps run the agent's code unsandboxed in the builder pod, so the render also
creates the `buildit-builders` NetworkPolicy there: no ingress, egress only to DNS and
`[buildit] egress` (default TCP 443 anywhere). Policies match networks, not host names: list your
registries' networks, or put the cluster's pod and service networks in `except`, to keep builds
off in-cluster services. `push`
publishes a succeeded build to `push_repo` only, tagged `<sandbox>-<build_id>`, with the loop
pod's `[secrets] push_authfile`; without either, buildit pushes nothing.

```toml
[buildit]
clusters     = ["spoke-a", "spoke-b"]
namespace    = "crucible-builds"                    # builder pods, apart from loop pods
egress       = [{ cidr = "0.0.0.0/0", except = ["10.128.0.0/14", "172.30.0.0/16"] }]  # optional
run_issuer   = "https://crucible.example"          # CONTROLLER_AGENT_ISSUER
run_jwks_uri = "https://crucible-ext.example/.well-known/jwks.json"  # optional
push_repo    = "registry.example.com/team/crucible-smoke"  # optional
```

```bash
openssl ecparam -genkey -noout -name prime256v1 | openssl pkcs8 -topk8 -nocrypt -out agent-signing.pem
oc create secret generic crucible-agent-signing-key --from-file=agent-signing.pem
```

## Observability

- `/metrics` serves Prometheus text on the API port.
- Tracing goes to stderr as leveled `tracing` events. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to
  also export OTLP spans over HTTP.
- `/healthz` answers when the database is reachable.
- The `GET /api/system` payload names the commit the binary was built from.

## The API and the CLI

`GET /api/openapi.json` is the API document. It needs the same bearer token as the other
routes. `crux` is the CLI for that API and the tool library that the controller hosts at
`/mcp`:

```sh
cargo install --locked --git https://github.com/neuralmagic/crucible.git crux
export CONTROLLER_URL=https://crucible.example.com
export CONTROLLER_API_TOKEN=crk_...
crux whoami
```

You get the `crk_` key on the Settings page of the UI. It has the identity of its owner. The
static `CONTROLLER_API_TOKEN` also works on `/api`, as `anonymous` or as the identity that
`CONTROLLER_API_TOKEN_IDENTITY` sets. It does not work on `/mcp`. The crux README is the only
reference for its flags, environment variables, and configuration file.
