# Group-owned workloads: authorization for `crucible-kueue`

Design note, not contracted behavior. A run acts as an identity derived from its owning scope,
not as the person who launched it. Kubernetes RBAC is the only place entitlements live.

## Where we are

This is implemented. A run's token carries its own identity (`crucible:run:<pod>`) with the
groups of its scope; `grant()` writes it at dispatch; `--kube-authz` describes people's group
source only, and a run's groups always come from its token. A person's token whose user or any
group starts with `crucible:` is refused, as `system:` always was.

## Runs act as their scope

The controller mints a run's identity from the run's owner:

| claim | value | job |
|---|---|---|
| `kube_user` | `crucible:run:<pod name>` | the SubjectAccessReview user |
| `kube_groups` | `crucible:runs`, `crucible:team:<slug>` (or `crucible:user:<login>`) | what the run may do |
| `sub` | whoever asked for the dispatch, else the owner | attribution, stamped as `<domain>/launched-by` |

An owner recorded before launch owners existed leaves the run its own group alone. A
team-dispatched run's `sub` is `team:<slug>`, so its `launched-by` label is the hashed,
label-safe form.

A SubjectAccessReview evaluates whatever user and groups it is given, so none of these need
to exist as cluster objects. `crucible-kueue` still writes with its own service account.

The launcher is recorded, never authorized. A person's own RBAC applies only when they call
`crucible-kueue` with their own token.

## Entitlements are RoleBindings

The deploy names its shared namespaces (`[kueue] shared_namespaces`, a subset of
`workload_namespaces`) and renders the caller role and its binding:

```yaml
kind: RoleBinding
metadata: {name: crucible-kueue-runs, namespace: crucible-kueue-e2e}
roleRef: {kind: ClusterRole, name: crucible-kueue-caller}
subjects: [{kind: Group, name: crucible:runs}]
```

`crucible-kueue-caller` holds every verb the crucible-kueue access checks can ask of a caller,
Secret reads narrowed to the profile's pull secrets; a render test walks the access checks, so a
new tool cannot ship without its RBAC. Making the namespace set explicit means adding a team
namespace to `workload_namespaces` later never opens it to every run.

A team namespace is the same binding to `crucible:team:<slug>`, written by whoever administers
that namespace. The team-to-group mapping is the slug, so the controller stores nothing about
RBAC.

`crucible:runs` rather than `system:authenticated`: the latter also opens the queue to every
person on the cluster.

## Group source per caller kind

| caller | user | groups |
|---|---|---|
| run token | `kube_user` (minted) | `kube_groups` (minted) |
| person token | `kube_user` | `--kube-authz`: OpenShift Group objects or `kube_groups` |

A person's token is refused if its user or any group starts with `crucible:`, as `system:` is
today, so no person can pose as a run or a team.

## Staging

| step | change | unblocks |
|---|---|---|
| 1 | issuer mints run identity from the owning scope; drop link resolution for runs | unlinked launchers |
| 2 | `crucible-kueue` takes run groups from the token regardless of `--kube-authz`; refuse `crucible:` on person tokens | run identity on OpenShift deploys |
| 3 | `grant()` writes `kube_user` at dispatch; delete `attribute()`'s identity lookup | no NULL rows |
| 4 | deploy renders `crucible-shared` and its `crucible:runs` binding | shared namespace by default |
| 5 | drop `requires_kube_user` for Kueue | config cleanup |

## Also fix

- `grant()` writes rows before the run exists, and a failed dispatch leaves them behind. Find
  them with `SELECT t.* FROM run_agent_tools t LEFT JOIN runs r USING (run_id) WHERE r.run_id IS NULL`.
- `whoami` should report the served namespaces, the token's user and groups, and which
  namespaces the review allowed.

## Resolved questions

- No identity provider on our clusters issues a username or group starting with `crucible:`; the
  reviewer refuses such a person token regardless.
- Personal-scope runs get `crucible:user:<login>`: it costs nothing and gives administrators
  something to bind to.
- `sub` is whoever asked for this dispatch, else the owner: a standing launch firing for its
  team is `team:<slug>`, a manual launch made acting as a team likewise, and a person's dispatch
  their login.
