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
