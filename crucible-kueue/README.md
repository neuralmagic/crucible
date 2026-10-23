# crucible-kueue

MCP server (rmcp, streamable HTTP, stateless) that lets agents in OpenShell sandboxes submit and
watch Kueue workloads. The server holds the Kubernetes credentials. A caller holds an access token
naming the Kubernetes user it acts as (a run's launcher, or a person), and that user's RBAC
decides every call.

Tools: `whoami`, `queues_list`, `queue_describe`, `hardware_describe`, `workload_plan`,
`workload_submit`, `workload_status`, `workload_list`, `workload_logs`, `workload_stop`,
`workload_resume`, `workload_scale`, `workload_delete`.

Submittable kinds: batch/v1 Job, ray.io/v1 RayJob, leaderworkerset.x-k8s.io/v1 LeaderWorkerSet,
apps/v1 Deployment, apps/v1 StatefulSet. Kueue admits a Deployment pod by pod (it can run
partially admitted and scale freely) and everything else all or nothing; a StatefulSet scales only
to or from zero. Stop/resume deactivates the Kueue Workload for batch kinds and scales
Deployments and StatefulSets to zero and back. A Job or RayJob that sets no
`activeDeadlineSeconds` gets 24 hours, and one that sets no `ttlSecondsAfterFinished` is deleted an
hour after it finishes (a RayJob only when `shutdownAfterJobFinishes` is true, the only case
KubeRay accepts a TTL). `workload_plan` and `workload_submit` report that TTL as
`finished_retention_seconds`; past it the object, its pods, logs and task are gone.

Every tool answers JSON text with a `status`: `ok`, `refused`, `rejected`, `not_found`, `error`,
or `failed` (a completed task whose workload failed). Anything but `ok` is a tool result with
`isError: true` and a `retryable` flag (an admission webhook's refusal, a 5xx, or an API server
that did not answer). The auth guard answers a refused tool call the same way.

`workload_status` answers a summary by default: phase, reason, queue position and a fresh fit
estimate while pending, pod counts by state, up to five pods with problems (stuck, failed,
restarted, unschedulable) with their exit code and termination message, recent events of the
object, its Workloads and those pods, and a `next_action`. `detail: "full"` adds every
Workload's conditions, the object's full status and every pod. `until`
(`admitted`/`running`/`ready`/`finished`) waits on the server for up to `timeout_seconds` (max
50) and stops early once the workload settles short of it or a pod is stuck.

`workload_logs` shares a byte budget (16 KiB by default, `max_bytes` up to 64 KiB) across at most
four pods, failed ones first, then a RayJob's head and submitter or a LeaderWorkerSet's leader.
Each pod keeps the end of its log; `truncated` and `bytes_omitted` say what was cut. `grep` and
`since_seconds` narrow it first.

## Tasks

The server implements the MCP tasks extension (`io.modelcontextprotocol/tasks`, protocol
2026-07-28) for batch kinds. When a request declares the extension in `_meta`, submitting a Job or
RayJob returns a task with id `<namespace>/<Kind>/<name>/<uid>`; `tasks/get` derives its state from
the cluster on every call and `tasks/cancel` stops the workload. A task completes when Kueue
finishes the Workload (a failed run completes with `isError: true`). Clients without the extension
get a synchronous reply that still carries `task_id`, and poll `workload_status`.

Deployments, StatefulSets and LeaderWorkerSets serve until stopped, so they are never tasks: their
submission is answered synchronously, and `workload_status` reports `readiness` (`ready` once every
wanted replica is ready and admitted, never when scaled to zero).

## Tokens

Every caller presents an OAuth access token; there are no static or per-run tenant tokens.
`--run-issuer <issuer>=<jwks uri>` (repeatable) trusts an issuer of runs' tokens (the crucible
controller), `--person-issuer` one of people's tokens (the crucible realm, which needs
`--kube-authz`), and `--resource` names this server's canonical URLs, one of which a token's `aud`
must name. The issuer decides which kind a token is, never its claims. Every token needs
`kube_user`; its `namespace` claim, if any, is only the default namespace. A run's `run_id` owns
what it creates, its `sub` (the human the run acts for) is stamped on created workloads as
`crucible.dev/launched-by`, and its presenter (`act.sub`, the sandbox) as `crucible.dev/actor`.
`role` and `queues` claims are ignored.

Key sets are fetched over https; `--jwks-ca <pem>` adds CAs for the issuers' hosts, and
`--insecure-jwks-http` allows plain http off loopback for a cluster network you trust.

Every tool and the tasks extension's two methods are declared in
[src/access.rs](src/access.rs) with the scope each needs and, for a token carrying `kube_user`,
the SubjectAccessReviews each needs; anything else is refused. `--kube-authz openshift|token`
decides those reviews and turns on people's tokens; without it a call that needs a review is
refused. `--tool-policy` optionally adds CEL validate and mutate rules over tool arguments. Every
tool takes an optional `namespace`, defaulting to the token's `namespace` claim. Hosts can embed the server with `crucible_kueue::router(client, Options { … })`. The
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
events; cluster-wide it may also create `subjectaccessreviews` and, with `kube_authz = "openshift"`,
list `groups.user.openshift.io`. The server trusts runs' tokens from `run_issuer` (the controller's
`CONTROLLER_AGENT_ISSUER`) with keys from `run_jwks_uri` (default the issuer's
`/.well-known/jwks.json`; a plain-http URI adds `--insecure-jwks-http`), and its `--resource` is
its Service URL. `--allowed-host` is set to the Service DNS name. The image is `Containerfile.kueue` at the repository root.

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
