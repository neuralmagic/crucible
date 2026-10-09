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

Every caller presents an OAuth access token; there are no static or per-run tenant tokens.
`--run-issuer <issuer>=<jwks uri>` (repeatable) trusts an issuer of runs' tokens (the crucible
controller), `--person-issuer` one of people's tokens (the crucible realm, which needs
`--kube-authz`), and `--resource` names this server's canonical URLs, one of which a token's `aud`
must name. The issuer decides which kind a token is, never its claims. A run's `run_id`,
`namespace`, `role`, and `queues` claims scope it; its `sub` (the human the run acts for) is stamped
on created workloads as `crucible.dev/launched-by`, and its presenter (`act.sub`, the sandbox) as
`crucible.dev/actor`.

Key sets are fetched over https; `--jwks-ca <pem>` adds CAs for the issuers' hosts, and
`--insecure-jwks-http` allows plain http off loopback for a cluster network you trust.

Every tool and the tasks extension's two methods are declared in
[src/access.rs](src/access.rs) with the scope each needs and, for a token carrying `kube_user`,
the SubjectAccessReviews each needs; anything else is refused. `--kube-authz openshift|token`
decides those reviews and turns on people's tokens; without it a call that needs a review is
refused. `--tool-policy` optionally adds CEL validate and mutate rules over tool arguments. Every
tool takes an optional `namespace`: a run's token is pinned to its own, and a person names
one per call. Hosts can embed the server with `crucible_kueue::router(client, Options { … })`. The
shipped policy is
[deploy/kueue-policy.toml](deploy/kueue-policy.toml); the auth design and the e2e's configuration
are in [crucible-mcp-auth](../crucible-mcp-auth/README.md).

## Deploy

The deploy profile's `[kueue]` table renders the Deployment, Service, ServiceAccount, and RBAC:

```bash
crucible deploy render --kueue --profile <profile.toml>
crucible deploy apply --kueue --profile <profile.toml>
```

The server's identity gets get/list on `clusterqueues`, `resourceflavors`, and `nodes`
cluster-wide; in the workload namespace only, get/list on `localqueues`, get/list/patch on
`workloads`, get/list on `localqueues/pendingworkloads` in `visibility.kueue.x-k8s.io`,
create/get/list/patch/delete on each submittable kind under its own API group, and get/list on pods, `pods/log`, and
events. `--allowed-host` is set to the Service DNS name. The image is `Containerfile.kueue` at the repository root.

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
