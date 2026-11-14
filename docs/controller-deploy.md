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
  owner-referenced to the pod); `secrets` list/patch, with which the leader re-mints a running
  run's GitHub App token into its Secret once it has less than 30 minutes left; and `leases` get/create/update/patch
  for leader election;
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
- A pack whose runs need one cluster (a build that reaches a mirror only that cluster can see)
  pins it in its manifest with `[dispatch] target = "<cluster>"`. Every launch of that pack goes
  there, and a launch or schedule that names another target is refused. The pin does not bypass
  `CONTROLLER_CLUSTER_POLICY`: a caller fenced off the pinned cluster cannot launch the pack.
  A draft-head schedule saved before its draft added a pin fails its firing until it is saved
  again.
- The identity in each spoke's kubeconfig needs the Role above in that spoke's loop namespace,
  without `leases`. Name it in the spoke profile's `[cluster] sandbox_readers` and the loop render
  emits that grant as the Role `crucible-dispatch`, so it is not written by hand. It includes
  `secrets` patch. With create and delete alone the hub delivers each run's Secret but every
  re-mint of its GitHub App token is refused, and the run's GitHub calls answer 401 an hour after
  dispatch. The verb cannot be limited to run Secrets by `resourceNames`, because each is named
  after its pod (`<pod>-secrets`) and a Role lists exact names.
  `kubectl --kubeconfig <spoke kubeconfig> auth can-i patch secrets -n <loop namespace>` answers
  whether a spoke has it.
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
where its supervisor runs, so with agent clusters the controller refuses to start
unless the deploy profile runs each run's gateway in its own pod: `[cluster] sandbox_namespace`
(which means the kubernetes driver), which profile load refuses to share with the loop, rig, or
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

Each cluster's gateway prefix is everything before the gateway id
(`spiffe://<trust domain>/openshell/gateway/`, from its discovered trust domain, or its
`CONTROLLER_AGENT_SPIRE` entry). The controller reads that SPIRE's
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

Whatever the gateway mode, `sandbox_readers` is also granted the Role `crucible-dispatch` in the
loop namespace, which is what the controller dispatches and collects run pods with: `pods`
create/get/list/watch/delete, `pods/log` get, `pods/finalizers` update, `configmaps`
create/delete, and `secrets` create/delete/list/patch. It has no `get` on Secrets.

Grounded-rank and scope turns get a gateway of their own the same way, with certificates valid for
the turn's deadline; a failed launch tears it down, and the gateway sweep removes it once the
turn's row closes. Turn sandboxes are never bound, so they get no agent tokens.

Supervisors of runs that get agent servers mount the SPIFFE Workload API (the `csi.spiffe.io`
driver) at `/spiffe-workload-api/spire-agent.sock`; `[cluster] sandbox_spiffe_socket` names another
path. Runs without agent servers mount none, so a cluster without SPIRE still starts sandboxes.

Task tokens (the tokens a pack's `command` and `evaluate` tasks get for `[workflow] servers`) need
no SPIRE: the engine exchanges the run's pod token for them. Where the cluster has SPIRE, set
`[cluster] loop_svid = true` to also require the loop pod's own JWT-SVID, so a pod token copied
out of the pod mints nothing. The loop pod then mounts the Workload API socket through
`csi.spiffe.io` at the same path, which the loop namespace's pod security must admit, and the
cluster needs a ClusterSPIFFEID for loop pods:

```yaml
apiVersion: spire.spiffe.io/v1alpha1
kind: ClusterSPIFFEID
metadata:
  name: crucible-loop
spec:
  jwtTtl: 5m
  namespaceSelector:
    matchLabels:
      kubernetes.io/metadata.name: <loop namespace>
  podSelector:
    matchLabels:
      crucible.io/work-kind: run
  spiffeIDTemplate: 'spiffe://{{ .TrustDomain }}/crucible/run/{{ .PodMeta.Name }}'
```

The controller refuses a task token whose SVID names another pod or another cluster's SPIRE.

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

The issuer needs `CONTROLLER_AGENT_ISSUER`, a signing key and the deploy profile. Audiences, people
resources, each cluster's SPIRE and platform, the servers' trusted issuer and the cluster link
callback are derived from those; the rest of the table is overrides. See
[Minimal configuration](agent-mcp-architecture.md#91-minimal-configuration) and the derived values
beside it.

| Variable | Meaning |
| --- | --- |
| `CONTROLLER_AGENT_ISSUER` | The issuer identifier, an origin such as `http://crucible-controller.crucible-system.svc.cluster.local:8080`. Unset turns the endpoint off. Supervisors must request their SVID with exactly this audience (`token_grant.jwt_svid_audience`). |
| `CONTROLLER_AGENT_SIGNING_KEY_FILE` | A PKCS#8 P-256 private key mounted from a Secret. The `kid` is its RFC 7638 thumbprint. |
| `CONTROLLER_AGENT_SIGNING_KEY` | The same key inline, as PEM, in place of the file. |
| `CONTROLLER_AGENT_PREVIOUS_SIGNING_KEY_FILE` | Optional: the key before a rotation. It is published in the JWKS beside the current key, so tokens it signed keep verifying until they expire; nothing new is signed with it. Rotate by moving the current key here and mounting a new one, then drop this after one token lifetime. |
| `CONTROLLER_AGENT_TOKEN_TTL_SECS` | Agent token lifetime, 900 to 1800 seconds. Default 900. |
| `CONTROLLER_AGENT_TOKEN_ENDPOINT` | Where sandbox supervisors reach the token endpoint when not at the issuer itself (a spoke reaching the controller through a route). Default `<issuer>/oauth/token`. |
| `CONTROLLER_AGENT_SPIRE` | Optional: by default each cluster an agent server serves has its SPIRE discovered through the controller's own cluster client (OpenShift: the Zero Trust Workload Identity Manager's `ZeroTrustWorkloadIdentityManager` and `SpireOIDCDiscoveryProvider` named `cluster`, its discovery Service in `zero-trust-workload-identity-manager` on https:443; elsewhere: the `spire-server` ConfigMap's `server.conf`, its discovery Service in `spire-server` on http:80), and `GET /api/agent-clusters` shows each cluster's state. For a SPIRE installed elsewhere, comma-separated `<cluster>=<svid issuer> <gateway prefix> [http://]<namespace>/<service>:<port>` entries (`hub` for the controller's own): the `iss` of that SPIRE's JWT-SVIDs, its gateway prefix (`spiffe://<trust domain>/openshell/gateway/`; supervisor IDs are `<prefix><gateway id>/sandbox/<sandbox id>`; one path segment may be `{tenant}`, as in `spiffe://<trust domain>/openshell/{tenant}/gateway/`, to match IDs with a DNS-label tenant there), and its OIDC discovery provider Service, whose `/keys` is read through that cluster's API server (over https, or http when the Service is prefixed `http://`). Each cluster and each issuer appears once. SVIDs must carry `iat` and live at most 15 minutes; SPIRE's upstream chart issues 1-hour JWT-SVIDs by default, so give the supervisors' ClusterSPIFFEID `jwtTtl: 5m`. |
| `CONTROLLER_PEOPLE_ACCESS_TTL_SECS` | People's access token lifetime, 1 to 3600 seconds. Default 900. |
| `CONTROLLER_PEOPLE_CLIENTS_FILE` | A TOML file of `[[clients]]` (`client_id`, `name`, `redirect_uris`, optional `resources`): the public clients people may sign in with. It replaces the built-in `claude-code`, `pi` and `codex`, so list those too to keep them. Unset keeps the built-ins. A malformed file, or a `resources` entry that is no people resource, stops the controller at startup. See [MCP clients](controller-auth.md#mcp-clients). |
| `CONTROLLER_PEOPLE_DCR` | `false` turns off self-registration (`POST /oauth/register`, RFC 7591, loopback public clients only), and the metadata then advertises no `registration_endpoint`. On by default when the deploy profile serves people, which is what lets pi, Codex and Claude Code sign in without a client id. Registrations idle for 30 days are deleted; each source address may register 20 times an hour, and about 10000 live at once (a soft cap across replicas). See [Self-registered clients](controller-auth.md#self-registered-clients). |
| `CONTROLLER_PEOPLE_TRUSTED_PROXIES` | Comma-separated networks or addresses (`10.128.0.0/14`) whose `X-Forwarded-For` names a registration's source address, such as the cluster router's. Unset, every registration counts against its TCP peer. |
| `CONTROLLER_PEOPLE_CIMD_HOSTS` | Comma-separated host names (`chatgpt.com,pi.dev`) allowed to publish Client ID Metadata Documents. Set, an `https://` `client_id` on one of these hosts is fetched and accepted as a public client, and the metadata advertises `client_id_metadata_document_supported`. Unset (the default) refuses every URL `client_id`. The controller needs outbound HTTPS to these hosts; it honours `HTTPS_PROXY`. |

A run dispatched onto one of the `[kueue]` clusters (`clusters = ["spoke-a", "spoke-b"]`, or
`cluster = "spoke-a"`), or of any server the profile describes, gets each server's name, URL,
scopes and transport (`CRUCIBLE_AGENT_SERVERS`), the token endpoint
(`CRUCIBLE_AGENT_TOKEN_ENDPOINT`) and the issuer as SVID audience (`CRUCIBLE_AGENT_SVID_AUDIENCE`)
as plain env. The engine registers a token-grant provider for each, so each sandbox's supervisor obtains its own token and injects it on traffic to the server
alone; no token is minted per run. A server is granted only on a cluster whose SPIRE
the issuer trusts. The run records what it got, with the server's scopes and the run's Kubernetes
identity its tokens carry, and the run page shows it. A third server needs an
`[[agent_servers]]` entry with its `url` and `scopes`, nothing else.

The deploy profile describes each server that takes agent tokens (`DeployProfile::agent_servers`):
`[kueue]` names `workload_namespaces`, the first as the `namespace` claim, for runs on its
clusters; `[buildit]` needs neither. `[kueue] shared_namespaces` (a subset of
`workload_namespaces`, checked at profile load) names the namespaces every run may act in: the
render binds ClusterRole `crucible-kueue-caller` — every verb the crucible-kueue access checks
can ask, Secret reads narrowed to `pull_secrets` — to Group `crucible:runs` there. Naming them
explicitly means adding a team namespace to `workload_namespaces` later never opens it to every
run. Each server's URL on each
of its clusters is an agent audience, with its scopes: `kueue.read kueue.submit` for crucible-kueue
and `buildit.read buildit.build` for buildit. The rendered server trusts runs' tokens from the controller's
`CONTROLLER_AGENT_ISSUER` (`crucible deploy render --kueue --issuer <it>`), fetching keys from
`<issuer>/.well-known/jwks.json` unless `run_jwks_uri` names another URI (a route, for a server that cannot reach the issuer's address).
`kube_authz` (`openshift` or `token`) is where the server takes a person's groups for its
SubjectAccessReviews; a run's groups always come from its own token. With `people = true` the rendered server also accepts people's tokens from
the same issuer (`--person-issuer`), and each cluster's server becomes a resource people's MCP
clients get tokens for, at its Service URL or the cluster's `people_urls` entry.

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
pod's `[secrets] push_authfile`; without either, buildit pushes nothing. `queue` submits builder
pods to that Kueue LocalQueue in `namespace`; set it where Kueue manages the namespace and holds
pods that name no queue (`manageJobsWithoutQueueName`), or builds wait 10 minutes and fail.
`[buildit.builders]` sizes builder pods on every cluster: `node_arch` (their `kubernetes.io/arch`),
`max_builds` live per sandbox, `pod_deadline_secs`, `requests`/`limits`, `builder`,
`storage_driver`, `bud_args`, `layer_cache`, and `layer_compression`.
`[buildit.cluster_builders.<cluster>]` replaces any of those on one cluster, so a mixed-architecture cluster can pin its builds. Without
`node_arch`, builder pods run on the loop pod's architecture; unset limits are buildit's defaults
(4 builds, 7200 s, no requests or limits). `builder` is what builds a Dockerfile when the
build names no builder: `buildah` (the default) or `buildkit` (experimental), which builds as
`docker build` does; `buildit/README.md` lists what differs. Apply the builder admission policy of the same release, which admits the BuildKit image. `storage_driver` is `vfs` (the default), a full copy
of the filesystem per image layer, or `overlay`, each layer stored once with kernel overlayfs in
the builder's user namespace; `overlay` needs a kernel that allows that (5.13 and later, such as
RHEL 9's). BuildKit builds use overlayfs either way: with `overlay` a build fails where it does
not mount, with `vfs` it falls back to copying and says so in its log and `status`. `bud_args` are extra `buildah bud` arguments for every Dockerfile build, placed after
buildit's own. `layer_cache` is where BuildKit builds keep their layers between builder pods:
`registry` (the default), `cache-` tags of the repository builds push to, which a registry that
is never garbage-collected grows with, or `off`. `layer_compression` is how a BuildKit build
compresses the layers it makes: `zstd` (the default), `gzip` for runtimes older than containerd
1.5 or Docker 23, or `uncompressed`, which keeps the layers' full size in the registry.

```toml
[buildit]
clusters     = ["spoke-a", "spoke-b"]
namespace    = "crucible-builds"                    # builder pods, apart from loop pods
egress       = [{ cidr = "0.0.0.0/0", except = ["10.128.0.0/14", "172.30.0.0/16"] }]  # optional
run_jwks_uri = "https://crucible-ext.example/.well-known/jwks.json"  # optional
push_repo    = "registry.example.com/team/crucible-smoke"  # optional
queue        = "crucible-builds"                    # optional, a Kueue LocalQueue in namespace
image        = "ghcr.io/neuralmagic/crucible-buildit@sha256:..."  # optional, default the loop image's

[buildit.builders]                                  # optional
max_builds        = 2
pod_deadline_secs = 3600
requests          = { cpu = "8" }
limits            = { cpu = "32" }
storage_driver    = "overlay"
bud_args          = ["--retry", "5"]

[buildit.cluster_builders.spoke-b]                  # optional, per cluster
node_arch         = "amd64"
builder           = "buildkit"
layer_cache       = "registry"
layer_compression = "gzip"                          # nodes that cannot pull zstd layers

[buildit.cache]                                     # optional, a cache builds share
clusters     = ["spoke-a"]                          # default the hub; deploy it on each
token_secret = "buildit-cache-token"
size_gib     = 500
writers      = "run"                                # "none", "run" (default) or "all"
```

`image` pins buildit apart from the loop image: loop pods copy its `/usr/local/bin/buildit` in
with an init container and run it ahead of their own, and the cache runs it unless
`[buildit.cache] image` says otherwise. Every main build publishes
`ghcr.io/neuralmagic/crucible-buildit`, so a buildit-only change rolls by moving this pin; the
controller reads the profile at each dispatch, and the cache needs `crucible deploy
--buildit-cache` applied again.
A tag instead of a digest floats: loop pods pull it every time, so
`ghcr.io/neuralmagic/crucible-buildit:stable`, which each release moves, follows releases with no
profile change. The cache is a Deployment and keeps the image it started with until it restarts.

`[buildit.cache]` adds a `buildit cache` in the loop namespace that every build may use: ccache
objects and stored build outputs shared across runs. `crucible deploy --buildit-cache --profile
<profile>` renders its PersistentVolumeClaim, Deployment, Service and a NetworkPolicy that admits
builder pods only; apply it on each cluster in `clusters`. The loop render opens builder egress to
it, and runs on those clusters, and no others, get its URL. Create the token Secret first, in the
loop namespace, with a URL-safe value:

```bash
oc create secret generic buildit-cache-token --from-literal=token="$(openssl rand -hex 32)"
```

Every build reads the cache. `writers` says which builds also write to it:

- `"run"` (the default): the builds a run's own command and evaluate tasks start. A pack can seed
  the cache from a known ref before its agents fan out, and agents' builds only read.
- `"all"`: agents' builds too. A build's `RUN` steps can read the token, so one run's agent could
  store objects other runs' builds use. Use it only on clusters whose runs trust each other.
- `"none"`: no build writes, and loop pods do not mount the token.

```bash
openssl ecparam -genkey -noout -name prime256v1 | openssl pkcs8 -topk8 -nocrypt -out agent-signing.pem
oc create secret generic crucible-agent-signing-key --from-file=agent-signing.pem
```

`[git]` runs `crucible-git` as a sidecar of the loop pod of each run on `clusters`, and makes it
the only holder of the run's GitHub credential. At dispatch the controller takes the binding that
carries one, a minted `mint://github-app/<owner>` secret bound as a file or any broker-only
binding projected as `GH_TOKEN` or `GITHUB_TOKEN` (a personal access token, say), out of the loop
container and mounts
it into the sidecar alone, by directory. A variable the binding was projected as still exists in the
loop container, holding the placeholder `crucible-git`, so a pack's tools find it set and its
scripts need no change. A minted token is re-minted into the run's Secret by the leader once it has
less than 30 minutes left, so a long run never pushes with a lapsed one. The leader checks every
minute and retries a failed re-mint at the next check, logging `GitHub token refresh failed` with
the run's id; a personal access token does not expire
hourly and is not refreshed. A run binding two GitHub credentials is refused. An agent-visible
binding projected as `GH_TOKEN` or `GITHUB_TOKEN` stays as bound, with a warning at dispatch: the
sandbox reaches GitHub directly, outside `crucible-git`. A minted token under any other variable
is the pack's own and stays as bound, as on a cluster without `[git]`, where a run gets every
binding exactly as bound.

The run's command and evaluate tasks use stock `gh` and `git` against `github.com`. The pod maps
GitHub's hostnames to loopback, where `crucible-git` serves them over TLS with a CA it mints for the
run; tasks trust that CA (`SSL_CERT_FILE` and friends) and hold a placeholder `GH_TOKEN`, which
`crucible-git` replaces with the run's credential. Sandboxes reach it at `url` with run tokens: git there
needs no credential (`http://host.openshell.internal:8853/git/<owner>/<repo>.git`), and the
`open_pr` tool opens pull requests.

The credential goes only as far as the run's `[outputs.draft-pr]` bound allows, a personal access
token included, however much more its owner could reach: reads of a bound or
public repository; pushes, and opening a pull request (spending the bound's count), on a bound one;
labels, reviewers and edits on pull requests the run opened. GraphQL is parsed and scoped field by
field the same way. A task's request carrying a credential of its own, such as the engine's own
clone or publish push, is relayed with it untouched, and one carrying none that the bound does not
lend the token to goes out anonymously, so public downloads work as they would without
`crucible-git`. The pod needs the safe sysctl
`net.ipv4.ip_unprivileged_port_start` for `crucible-git` to bind 443, and `run_jwks_uri` must be
https.

```toml
[git]
clusters     = ["spoke-a"]
run_jwks_uri = "https://crucible-ext.example/.well-known/jwks.json"  # optional
url          = "http://host.openshell.internal:8853/mcp"            # optional
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
