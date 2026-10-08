# crucible-kueue

MCP server (rmcp, streamable HTTP, stateless) that lets agents in OpenShell sandboxes submit and
watch Kueue workloads. The server holds the Kubernetes credentials. An agent holds a bearer token
whose SHA-256 is registered to one namespace, a role (`viewer` or `submitter`), and a LocalQueue
allowlist. ADR-0066 records the design.

Tools: `whoami`, `queues_list`, `queue_describe`, `hardware_describe`, `workload_plan`,
`workload_submit`, `workload_status`, `workload_list`, `workload_logs`, `workload_stop`,
`workload_resume`, `workload_scale`, `workload_delete`.

Submittable kinds: batch/v1 Job, ray.io/v1 RayJob, leaderworkerset.x-k8s.io/v1 LeaderWorkerSet,
apps/v1 Deployment, apps/v1 StatefulSet. Kueue admits a Deployment pod by pod (it can run
partially admitted and scale freely) and everything else all or nothing; a StatefulSet scales only
to or from zero. Stop/resume deactivates the Kueue Workload for batch kinds and scales
Deployments and StatefulSets to zero and back.

## Tasks

The server implements the MCP tasks extension (`io.modelcontextprotocol/tasks`, protocol
2026-07-28). When a request declares the extension in `_meta`, `workload_submit` returns a task
with id `<Kind>/<name>/<uid>`; `tasks/get` derives its state from the cluster on every call and
`tasks/cancel` stops the workload. Batch tasks complete when Kueue finishes the Workload (a failed
run completes with `isError: true`); serving tasks complete once every replica is admitted and
ready, and stay complete afterwards. Clients without the extension get a synchronous reply that
still carries `task_id`, and poll `workload_status`.

## Tokens

Two sources, checked in this order:

- `--tenants <file>`: a static TOML file the operator writes.
- `--tenant-namespace <ns>`: ConfigMaps labelled `crucible.dev/kueue-tenant=true`, each holding
  one JSON record under `tenant.json`. The controller writes one per run it dispatches onto the
  server's cluster and deletes it when the run is collected. Only the controller may write
  ConfigMaps in that namespace.

```toml
# tenants.toml
[[tenant]]
run_id = "run-123"
token_sha256 = "<sha256 hex of the bearer token>"
namespace = "team-a"
role = "submitter"
queues = ["crucible-e2e"]
expires_at = 1791500000   # optional, Unix seconds
```

`echo -n <token> | crucible-kueue --digest` prints a token's digest.

### Access tokens

With `--issuer <issuer>=<jwks uri>` (repeatable) and `--resource`, the server also accepts OAuth
access tokens from each listed issuer, verified against that issuer's own keys: the crucible
controller for sandboxed agents, the crucible realm for people (a compact JWT is never looked up
as a tenant token). The token's `run_id`, `namespace`, `role`, and `queues` claims give the same
scope as a tenant record, and its `sub` (the human the run acts for) is stamped on created
workloads as `crucible.dev/launched-by`.
`--tool-policy` adds per-tool scopes and CEL rules over the caller's claims and the call's
arguments. With `--kube-authz openshift|token` the policy's authorize rules become
SubjectAccessReviews for any token carrying `kube_user`, and people's tokens (no `run_id`) are
accepted. Every tool takes an optional `namespace`: run and tenant tokens are pinned to theirs, and
a person names one per call. Hosts can embed the server with
`crucible_kueue::router(client, Options { … })`. The shipped policy is
[deploy/kueue-policy.toml](deploy/kueue-policy.toml); the auth design and the e2e's configuration
are in [crucible-mcp-auth](../crucible-mcp-auth/README.md).

## Deploy

The deploy profile's `[kueue]` table renders the Deployment, Service, ServiceAccount, the
tenants namespace, and RBAC:

```bash
crucible deploy render --kueue --profile <profile.toml>
crucible deploy apply --kueue --profile <profile.toml>
```

The server's identity gets get/list on `clusterqueues`, `resourceflavors`, and `nodes`
cluster-wide; in the workload namespace only, get/list on `localqueues`, get/list/patch on
`workloads`, get/list on `localqueues/pendingworkloads` in `visibility.kueue.x-k8s.io`,
create/get/list/patch/delete on each submittable kind under its own API group, and get/list on pods, `pods/log`, and
events; get/list/watch on ConfigMaps in the tenants namespace. `--allowed-host` is set to the
Service DNS name. The image is `Containerfile.kueue` at the repository root.

## Test

```bash
cargo test -p crucible-kueue --lib
```

Cluster e2e of the server alone (creates and deletes real Jobs; apply
`e2e/kueue/namespace.yaml` first):

```bash
CRUCIBLE_KUEUE_E2E_CONTEXT=<ctx> CRUCIBLE_KUEUE_E2E_NAMESPACE=<namespace> \
CRUCIBLE_KUEUE_E2E_QUEUE=crucible-e2e cargo test -p crucible-kueue --test cluster_e2e -- --ignored --nocapture
```

The full path, from a controller dispatch to a sandboxed agent's Job, is `just kueue-e2e`.
