# Running the controller locally

The controller runs on a laptop with no identity provider, no Vault and no cluster. Playbook
launches run as `crucible plan run` subprocesses on the same machine, and a pack whose agent
turns ask for an OpenShell sandbox gets one on the host's podman.

```sh
just controller-local
```

Then open <http://127.0.0.1:8787>. The recipe needs podman (a running `podman machine` on
macOS), bun, and a Rust toolchain. It:

- starts Postgres in a `crucible-local-pg` container on `127.0.0.1:55434`, with its data in the
  `crucible-local-pg` volume, so launches and drafts survive a restart;
- builds the UI and the `crucible`, `crucible-controller` and `crux` binaries;
- runs `crucible-controller autopilot` with the settings below.

`just controller-local 9000 wren` picks another port and login. Drive it from a shell with crux:

```sh
export CONTROLLER_URL=http://127.0.0.1:8787
crux whoami
crux draft-create notes --description "release notes"
crux draft-push notes examples/playbook --base 1
crux draft-launch notes --max-cost 1 --max-time 5m
```

## What the recipe sets

| Variable | Value | Why |
| --- | --- | --- |
| `CONTROLLER_DEV_IDENTITY` | `$USER` | Every request lands as this login. |
| `CONTROLLER_ADMINS` | `$USER` | Makes that login an admin, so the UI can launch and edit. |
| `CONTROLLER_PLAYBOOK_EXECUTOR` | `local` | Launches run as a subprocess here instead of a work pod. |
| `CONTROLLER_SCOPE_EXECUTOR` | `disabled` | No autopilot scoping, which would otherwise shell `crucible scope` on its own. |
| `CONTROLLER_SESSION_SECURE` | `false` | The UI is plain http. |
| `KUBECONFIG` | `/dev/null` | The controller never reaches a cluster your kubeconfig happens to point at. |
| `OPENSHELL_PODMAN_SOCKET` | the podman machine's API socket | Where an OpenShell sandbox is booted. Set it yourself to override. |
| `CRUCIBLE_BIN` | the built `crucible` | The engine a launch runs. |

`CONTROLLER_API_TOKEN`, `CONTROLLER_PROXY_TOKEN`, `CONTROLLER_OIDC_ISSUER` and `VAULT_ADDR` are
unset for the process, whatever your shell exports.

## The dev identity

With no `CONTROLLER_API_TOKEN` and no `CONTROLLER_OIDC_ISSUER` the guard is open: it admits
every request and binds loopback whatever `CONTROLLER_API_ADDR` asks for. An open request names
nobody, so it is a viewer. `CONTROLLER_DEV_IDENTITY` names it instead, and drops any
`X-Auth-Request-*` header the client wrote first, so a browser and a curl land the same. The
controller refuses to boot with it set on a guarded deployment, where it would name every
caller.

## Agent turns

A local run starts from an empty environment. It keeps `PATH`, `HOME`, `USER`,
`OPENSHELL_PODMAN_SOCKET` and the `CRUCIBLE_*` set, and nothing else the controller holds.

- `backend = "local"` (the default) runs the agent CLI on this machine, under your own login.
  The harness finds it through `HOME` and `USER`; this spends real money.
- `backend = "command"` runs the pack's own command, with no model.
- `backend = "openshell"` runs each turn in a sandbox from `sandbox_image`, booted by the engine
  on podman. It needs `openshell` and `openshell-gateway` on `PATH`, and the image must be
  pullable by podman or already built into it. `just revise-loop-e2e` builds one with no model.

Any other variable a run needs goes in `CONTROLLER_LOCAL_SECRET_ALLOWLIST`, and the pack has to
disclose it as an agent credential before a run is handed it.

## What is off

- The secrets registry answers 503 on its write routes without Vault. A pack that binds no
  secrets launches normally; one that binds some is refused.
- Runtime overrides (the admin page's live caps) need the overrides ConfigMap, so the parsed
  configuration stands.
- Discovery, triage and the work-pod paths have no cluster and no watched repos, and stay idle.
