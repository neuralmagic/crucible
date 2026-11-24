# Secrets and credentials

A pack never holds a credential. It declares the ones it needs by name, the value lives in the
secrets registry (Vault behind the controller, see [Vault and the secrets
registry](./controller-vault.md)), and a binding attaches one to the scope you launch.

Three steps, and the second one is the one people skip:

1. **Declare** the secret in the pack, by name.
2. **Register** the value in the registry, once, under your user or your team.
3. **Bind** the registered secret to the draft or playbook, with a projection.

A declared secret with no binding is a pack that previews cleanly and is refused at launch:
`the pack declares <name>, and <scope> has no binding for it`.

## Declare

```toml
[[secret]]
name = "gitlab-read-token"   # the name the pack and the binding agree on
kind = "opaque"              # what the value is
env  = "GITLAB_TOKEN"        # or: path = "/etc/gitlab/token"
```

`name` is what the binding's `--declared-name` matches. The projection is either an environment
variable (`env`) or an absolute path the value is written to (`path`). Command tasks read either.
An agent turn sees the projection only if the secret is `agent_visible` (below).

## Kinds

The kind says what the value *is*, not where it goes. A kind outside this table is a manifest
error.

| `kind` | The value | Who can use it |
| --- | --- | --- |
| `opaque` | A bare string: an API key, a token, a header value. The only kind that can be `agent_visible`. | A run, or the hub |
| `file` | File content, projected into the pod's tmpfs at `path` | A run |
| `registry_authfile` | A container registry authfile, for in-pod pulls and pushes. Not the kubelet's pull secret. | A run |
| `kubeconfig` | A cluster credential the hub itself uses. It makes you a dispatch target, not a run credential. | The hub |
| `inference_api_key` | An inference provider's key. Projected as an environment variable the loop reads, never relayed into the sandbox: the agent reaches the model through the loop. | A run |
| `github_token` | A GitHub token the hub pushes and opens pull requests with on its owner's behalf. Never bound to a scope, never projected into a run. | The hub |

## Register

```sh
read -rs TOKEN && printf '%s' "$TOKEN" | crux secret-create gitlab-read-token --kind opaque
crux secrets                              # ids, kinds, owners
crux secret-rotate <secret id>            # value from stdin, a new version
```

The value arrives on stdin and goes straight to Vault; the registry keeps a pointer, the kind, the
owner and the audit trail, never the value. The registry offers no route that returns a value.

Defaults: `broker_only` visibility, and you as the owner. `--owner group:<group path>` registers
it for a team, which is what you want for anything a playbook uses rather than a draft. A secret
can also be registered by *reference* to a path someone else owns, or *minted* per dispatch (for
example a GitHub App installation token); both are described in [Vault and the secrets
registry](./controller-vault.md).

## Bind

```sh
crux secret-bind <secret id> --playbook <draft or playbook id> \
  --env GITLAB_TOKEN --declared-name gitlab-read-token
```

`--env` or `--file /abs/path` picks the projection; `--declared-name` defaults to the secret's
name. A binding needs `bind` on the secret and `launch` on the scope. A draft started from a
team's blueprint inherits that team's bindings for members of the team and gets a placeholder for
every other secret the pack declares.

At dispatch the controller reads the current version and writes a Kubernetes Secret in the
namespace of the run, owned by the pod, deleted with the pod. Every delivered value is redacted
from the session log.

## Visibility: may the agent see it?

`broker_only` is the default everywhere, and it is the right answer unless you have a reason.
It means the value reaches the run's pod — so command tasks, which run in the loop pod, see the
projection and can use it — but not the agent's sandbox. The broker acts on the agent's behalf:
a mediated Jira tool, a git sidecar, a registry push all use the credential without the agent ever
holding it.

`agent_visible` puts the value where the agent can read it, which is only available to `opaque`
secrets. Take it when the agent itself must run the tool: it shells out to `gh`, it runs `git push`,
it calls an API the broker has no mediated tool for. That choice is an exception to the run's
information boundary, so the pack must declare it as exposure or the launch is refused. See
[Bounded outputs](./outputs.md).

Rule of thumb: if a broker or a command task could make the call, it should. Every `agent_visible`
secret is one the agent can leak into a comment, a commit or a log.

## A kubeconfig is a dispatch target, not a credential

Every secret also has a *consumer*: `run` (a dispatched pod) or `hub` (the controller, on its own
behalf). A kubeconfig defaults to `hub`, because a cluster credential the hub holds is a place the
hub can start pods — a dispatch target. A kubeconfig consumed by a run is projected to the pod like
any other secret; with `[[agent.relay]]` that is how an agent reaches a cluster itself, without the
hub needing to reach it at all.

Registering a hub-consumed kubeconfig is the whole setup for a personal dispatch target. The
controller **probes it at registration**, and refuses it with
`this kubeconfig cannot serve as a dispatch target: <reason>`. The probe:

| Check | Failure says |
| --- | --- |
| The YAML parses and resolves to a cluster | `the kubeconfig does not parse: <detail>` |
| A namespace is set, and it is not `default` | `the kubeconfig names no namespace; a dispatch target must be namespace-scoped` |
| The API server answers the hub | an unreachable or TLS error, wrapped in the refusal above |
| The identity may create pods there | `denied` for that namespace, from a `SelfSubjectAccessReview` |

The probe runs from the hub, with the hub's own network position and the kubeconfig's identity.
A cluster whose API server only admits certain sources needs the hub allowlisted before the
registration will succeed; that is network policy on the cluster, not a Crucible setting.

Once registered, the target appears in the launch form's target list and in `--dispatch-target`,
for you alone unless you share or team-own the secret. See [Your first
playbook](./playbooks.md#dispatch-targets) for how a launch picks one.

## What this does not do

A secret is delivered, not scoped down. Crucible cannot limit what a token is allowed to do, and
cannot tell you what a value will be used for beyond the projection you bound. Register a
credential that is already least-privilege: read-only for reads, one repository for one repo, one
namespace for one cluster. `crucible check` prints the egress and credentials a pack resolves to
before any launch, which is the cheapest place to notice a pack asking for more than it needs.
