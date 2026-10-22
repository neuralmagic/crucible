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
  relay); `configmaps` and `secrets` create/delete (the pack ConfigMap and the run Secret,
  which holds the pod token and the delivered secrets, both owner-referenced to the pod); and
  `leases` get/create/update/patch for leader election;
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
