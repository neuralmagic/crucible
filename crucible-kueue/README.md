# crucible-kueue

MCP server (rmcp, streamable HTTP, stateless) that lets agents submit and watch Kueue workloads.
The server holds the Kubernetes credentials. A caller holds an access token naming the Kubernetes
user it acts as (a run's launcher, or a person), and that user's RBAC decides every call. With no
configuration it is crucible's server; [Generic Kueue MCP](#generic-kueue-mcp) runs it for any
cluster with a stock OIDC provider and plain RBAC.

Tools: `whoami`, `queues_list`, `queue_describe`, `hardware_describe`, `workload_plan`,
`workload_submit`, `workload_status`, `workload_list`, `workload_logs`, `workload_stop`,
`workload_resume`, `workload_scale`, `workload_delete`.

Submittable kinds: batch/v1 Job, ray.io/v1 RayJob, leaderworkerset.x-k8s.io/v1 LeaderWorkerSet,
apps/v1 Deployment, apps/v1 StatefulSet, jobset.x-k8s.io/v1alpha2 JobSet, kubeflow.org/v1
PyTorchJob; `whoami` lists those the cluster installs. An object's pods are found through its own
selector or its controller's pod labels, so a workload made by kubectl or a pipeline reads the
same as one this server made. Kueue admits a Deployment pod by pod (it can run
partially admitted and scale freely) and everything else all or nothing; a StatefulSet scales only
to or from zero. Stop/resume deactivates the Kueue Workload for batch kinds and scales
Deployments and StatefulSets to zero and back. A batch workload that sets no
`activeDeadlineSeconds` gets 24 hours, and one that sets no `ttlSecondsAfterFinished` is deleted an
hour after it finishes (a RayJob only when `shutdownAfterJobFinishes` is true, the only case
KubeRay accepts a TTL; a JobSet's deadline goes on each replicated Job, a PyTorchJob's on its
`runPolicy`). Both defaults are flags. `workload_plan` and `workload_submit` report that TTL as
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

## Generic Kueue MCP

Nothing in the server needs crucible. For a cluster whose users sign in with an OIDC provider
(Keycloak, Dex, Entra, Okta) and whose namespaces carry ordinary RoleBindings:

```bash
crucible-kueue \
  --person-issuer https://idp.example.com/realms/ml=https://idp.example.com/realms/ml/protocol/openid-connect/certs \
  --resource https://kueue-mcp.example.com/mcp \
  --kube-authz token --user-claim preferred_username --user-prefix 'oidc:' --groups-claim groups \
  --run-id-claim '' --label-domain kueue-mcp.example.com \
  --ownership rbac --allowlist-profile tenant
```

- The provider issues access tokens with `aud` naming the `--resource` URL and scopes
  `kueue.read` (every call) and `kueue.submit` (plan, submit, stop, resume, scale, delete,
  `tasks/cancel`). The server publishes RFC 9728 metadata at
  `/.well-known/oauth-protected-resource/mcp` so MCP clients find the provider.
- `--user-claim` and `--user-prefix` should match the API server's `--oidc-username-claim` and
  `--oidc-username-prefix`, so the user RBAC sees is the one the server reviews. `--kube-authz
  openshift` reads groups from OpenShift Group objects instead of `--groups-claim`.
- Every call is a SubjectAccessReview for that user before the server acts with its own service
  account: `get`/`list` for reads, `use` on the LocalQueue (`--queue-verb get` where roles do not
  grant Kueue's custom verb) and `create` on the kind to submit, `get` on each Secret and
  PersistentVolumeClaim a manifest names, and `update`/`delete` (or the scale subresource) to
  control. A caller's subject that is not a label value is sanitized with a hash.
- `--ownership rbac` lets a caller observe and control any workload of a served kind its RBAC
  permits; `workload_list` with `filter: "all"` lists them. `managed` (the default) limits
  control to what this server created for the caller.
- The server's service account needs what [Deploy](#deploy) lists, in each namespace it serves.
  A caller can use the served namespaces (`--namespace`, else every namespace holding a
  LocalQueue) where its RBAC lets it list LocalQueues; `whoami` lists them with their queues. A
  token that names no namespace defaults to the only one, when there is exactly one. A call in a
  namespace the server is not installed in answers `not_found` with that list, never the API
  server's message about the server's own account.

### Configuration

| Flag (env `CRUCIBLE_KUEUE_*`) | Default | Meaning |
|---|---|---|
| `--label-domain` (`LABEL_DOMAIN`) | `crucible.dev` | Domain of the stamped keys: `managed-by`, `run-id`, `launched-by`, `workload`, `plan-hash`, `stopped-replicas`, `actor`, `task-cancelled` |
| `--ownership` (`OWNERSHIP`) | `managed` | `managed` or `rbac` |
| `--allowlist-profile` (`ALLOWLIST_PROFILE`) | `strict` | `strict`, `tenant`, or a TOML file |
| `--user-claim` (`USER_CLAIM`) | `kube_user` | Claim naming the Kubernetes user |
| `--user-prefix` (`USER_PREFIX`) | empty | Prepended to the user claim's value |
| `--groups-claim` (`GROUPS_CLAIM`) | `kube_groups` | Claim listing groups, for `--kube-authz token` |
| `--run-id-claim` (`RUN_ID_CLAIM`) | `run_id` | Claim naming a run in a run issuer's tokens; empty for none |
| `--namespace-claim` (`NAMESPACE_CLAIM`) | `namespace` | Claim naming the default namespace |
| `--queue-verb` (`QUEUE_VERB`) | `use` | LocalQueue verb submitting needs: `use` or `get` |
| `--default-active-deadline` (`DEFAULT_ACTIVE_DEADLINE`) | `86400` | Seconds, or `none` |
| `--default-ttl-after-finished` (`DEFAULT_TTL_AFTER_FINISHED`) | `3600` | Seconds, or `none` |
| `--suspend` (`SUSPEND`) | `true` | Create Jobs, JobSets and PyTorchJobs suspended for Kueue |
| `--namespace` (`NAMESPACES`) | discovered | Namespaces the server is installed in; empty lists LocalQueues cluster-wide to find them |
| `--pull-secret` (`PULL_SECRETS`) | none | Image pull secrets pod templates may name; with none, `imagePullSecrets` is refused |
| `--accelerator-resource` (`ACCELERATOR_RESOURCES`) | `nvidia.com/gpu`, `nvidia.com/mig-*`, `amd.com/gpu`, `habana.ai/gaudi`, `google.com/tpu` | Resources `hardware_describe` counts; `*` matches a prefix |
| `--accelerator-product-label` (`ACCELERATOR_PRODUCT_LABELS`) | `nvidia.com/gpu.product`, `amd.com/gpu.product-name`, `habana.ai/gaudi.product`, `cloud.google.com/gke-tpu-accelerator` | Node labels naming the product |

The issuer, resource, JWKS, `--kube-authz` and `--tool-policy` flags are described under
[Tokens](#tokens).

### Allowlist profiles

`strict` is crucible's: pod templates may not reach Secrets, volume claims beyond ephemeral ones,
tolerations, priority classes or DRA claims, and every label or annotation under
`kueue.x-k8s.io`, `openshell.ai`, `spiffe.io` and the label domain is reserved. `tenant` adds
`tolerations`, `priorityClassName`, `resourceClaims` and container `resources.claims`, `secret`
and `persistentVolumeClaim` volumes, `secretKeyRef` and `envFrom.secretRef`, and reserves only the
label domain and the queue label, so Kueue's `kueue.x-k8s.io/priority-class` label and the TAS
`kueue.x-k8s.io/podset-required-topology`/`podset-preferred-topology` annotations are the
caller's to set. Both refuse host namespaces and paths, privileged containers and added
capabilities, service accounts and their tokens, `nodeName`, `schedulerName` and
`runtimeClassName`. A refused field's error lists the fields allowed beside it.

A file starts from either and adds or removes entries:

```toml
base = "tenant"

[add]
pod = ["runtimeClassName"]

[remove]
volume_sources = ["persistentVolumeClaim"]
```

The lists are `pod`, `container`, `resources`, `volume_sources`, `projected_sources`,
`env_sources`, `env_from`, `reserved_domains`, `allowed_labels` and `allowed_annotations`.
`--tool-policy` tightens further with CEL.

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
events (discovery of the kinds needs no grant); cluster-wide it may also create `subjectaccessreviews` and, with `kube_authz = "openshift"`,
list `groups.user.openshift.io`. The server trusts runs' tokens from `run_issuer` (the controller's
`CONTROLLER_AGENT_ISSUER`) with keys from `run_jwks_uri` (default the issuer's
`/.well-known/jwks.json`; a plain-http URI adds `--insecure-jwks-http`), and its `--resource` is
its Service URL. `--allowed-host` is set to the Service DNS name. The image is `Containerfile.kueue` at the repository root.

## Test

```bash
cargo test -p crucible-kueue --lib
```

Cluster e2e of the server alone (creates and deletes real Jobs; apply
`e2e/kueue/namespace.yaml` first). `CRUCIBLE_KUEUE_E2E_MODE=tenant` runs it as the generic setup
above (people's tokens with `preferred_username`, `tenant`, `rbac`, another label domain):

```bash
CRUCIBLE_KUEUE_E2E_CONTEXT=<ctx> CRUCIBLE_KUEUE_E2E_NAMESPACE=<namespace> \
CRUCIBLE_KUEUE_E2E_QUEUE=crucible-e2e CRUCIBLE_KUEUE_E2E_KUBE_USER=<user> \
CRUCIBLE_KUEUE_E2E_KUBE_GROUPS=<groups> CRUCIBLE_KUEUE_E2E_MODE=crucible|tenant \
cargo test -p crucible-kueue --test cluster_e2e -- --ignored --nocapture
```

The crucible suites that drive the controller's agent token endpoint live in crucible-controller
(`cargo test -p crucible-controller --test kueue_oidc_e2e` and `--test kueue_gateway_e2e`), so
crucible-kueue builds with crucible-mcp-auth as its only workspace dependency. The full path, from
a controller dispatch to a sandboxed agent's Job, is `just kueue-e2e`.
