# Running the controller locally

The controller can run on a laptop without an identity provider, Vault, or a cluster.
Playbook launches run as `crucible plan run` subprocesses on the same machine. If the agent
turns of a pack need an OpenShell sandbox, the engine starts one on the podman of the host.

```sh
crucible-controller local
```

Then open <http://127.0.0.1:8870>. `local` needs a build with the `embedded-db` feature, and
podman only for OpenShell sandboxes.

| Flag | Default | What it sets |
| --- | --- | --- |
| `--port` | `8870` | The loopback port of the API and the UI. |
| `--hooks-port` | the port after `--port` (`HOOKS_PORT` also sets it) | The loopback port of the webhook delivery listener. |
| `--identity` | `$USER` | The login every request uses, and the only admin. |
| `--state-dir` | `$XDG_STATE_HOME/crucible-controller`, else `~/.local/state/crucible-controller` | The location of the database, the webhook key, and the local runs. |

The installed Playbooks app includes `crucible-controller` and the engine, and runs
`crucible-controller local --port <port>` for a profile whose URL is on this machine. It needs
no source checkout or build tools.

From a checkout, `just controller-local` builds the UI and the `crucible`,
`crucible-controller` (with `embedded-db`), and `crux` binaries, then runs
`crucible-controller local`. It needs bun and a Rust toolchain.
`just controller-local 9000 wren` uses a different port and login.

If no other address is configured, crux uses `http://127.0.0.1:8870`, so on the default port
it needs no setup:

```sh
crux whoami
crux draft-create notes --description "release notes"
crux draft-push notes examples/playbook --base-version 1
crux draft-launch notes --max-cost 1 --max-time 5m
```

## What `local` sets

`local` becomes `crucible-controller autopilot` with these settings:

| Variable | Value | Why |
| --- | --- | --- |
| `DATABASE_URL` | `embedded` | The controller runs its own Postgres. |
| `CONTROLLER_STATE_DIR` | `--state-dir` | The location of the database and the local runs. |
| `CONTROLLER_API_ADDR` | `127.0.0.1:<--port>` | The API and the UI. |
| `CONTROLLER_PUBLIC_URL` | `http://127.0.0.1:<--port>` | The base of the links the controller writes. |
| `CONTROLLER_DEV_IDENTITY` | `--identity` | Every request uses this login. |
| `CONTROLLER_ADMINS` | `--identity` | Makes that login an admin, so you can launch and edit in the UI. |
| `CONTROLLER_PLAYBOOK_EXECUTOR` | `local` | Launches run as a local subprocess, not as a work pod. |
| `CONTROLLER_SCOPE_EXECUTOR` | `disabled` | Disables autopilot scoping, which would otherwise run `crucible scope` automatically. |
| `CONTROLLER_SESSION_SECURE` | `false` | The UI is plain http. |
| `KUBECONFIG` | `/dev/null` | The controller does not connect to the cluster in your kubeconfig. |
| `OPENSHELL_PODMAN_SOCKET` | the API socket of the podman machine | The engine starts OpenShell sandboxes here. Set it to use a different socket. |
| `CRUCIBLE_BIN` | the `crucible` beside `crucible-controller`, when there is one | The engine a launch runs. |
| `CONTROLLER_HOOKS_ADDR` | `127.0.0.1:<--hooks-port>` | The webhook delivery listener, on a separate port. |
| `CONTROLLER_HOOKS_PUBLIC_URL` | `http://127.0.0.1:<--hooks-port>` | The base of the delivery URL of each new webhook. |
| `CONTROLLER_WEBHOOK_KEY_FILE` | `<--state-dir>/webhook.key`, generated once | Seals `hmac_sha256` webhook secrets. |

`local` unsets `CONTROLLER_API_TOKEN`, `CONTROLLER_OIDC_ISSUER`, and `VAULT_ADDR` for the
process, also if your shell exports them. Any other `CONTROLLER_*` variable in the environment
still applies.

## The embedded database

With `DATABASE_URL=embedded`, `crucible-controller autopilot` runs its own Postgres 16 and
does not connect to an external one. It needs a build with the `embedded-db` feature
(`cargo build -p crucible-controller --features embedded-db`). A build without the feature
rejects the value at boot.

- On the first start, the controller downloads the Postgres binaries one time into
  `~/.theseus/postgresql`. All embedded controllers on the machine share them.
- The data directory is `$CONTROLLER_STATE_DIR/postgres/data`, and its password is in
  `postgres/pgpass` next to it. The data stays after a restart. To start with an empty
  database, delete `postgres/`.
- At each start, the server listens on a free loopback port. Only the controller uses it.
  crux and the UI use the controller API.
- The server stops when the controller exits. If the controller is killed, the next start
  stops the old server before it starts a new one.

## The dev identity

Without `CONTROLLER_API_TOKEN` and `CONTROLLER_OIDC_ISSUER`, the controller starts only with
`CONTROLLER_DEV_IDENTITY`. The guard is then open: it admits every request as that login, and
binds loopback, whatever `CONTROLLER_API_ADDR` is. The controller removes any
`X-Auth-Request-*` header from the client, so a browser and curl get the same identity. An
open guard answers 403 to a request whose `Host` is not `localhost` or a loopback address, and
to a write that a browser sends for a page of another origin. A forwarder in front of it must
keep a loopback `Host`. The
controller does not boot if this variable is set on a guarded deployment, because it would
give every caller that identity, and it does not boot with none of the three, because an open
controller without a login is never what a deployment meant.

## Agent turns

A local run starts with an empty environment. It keeps only `PATH`, `HOME`, `USER`,
`OPENSHELL_PODMAN_SOCKET`, and the `CRUCIBLE_*` variables from the controller.

- `backend = "local"` (the default) runs the agent CLI on this machine, under your own login.
  The harness finds its configuration through `HOME` and `USER`. This costs real money.
- `backend = "command"` runs the command of the pack, without a model.
- `backend = "openshell"` runs each turn in a sandbox from `sandbox_image`, which the engine
  starts on podman. It needs `openshell` and `openshell-gateway` on `PATH`. Podman must be
  able to pull the image, or the image must already be in podman. `just revise-loop-e2e`
  builds an image without a model.

Put any other variable that a run needs in `CONTROLLER_LOCAL_SECRET_ALLOWLIST`. The pack must
disclose the variable as an agent credential before a run gets it.

## Webhooks

On the default port, `local` serves the webhook delivery listener on `127.0.0.1:8871`.
[Launching from webhooks](./webhooks.md#trying-it-locally) shows a pack with only commands,
from a `curl` delivery to a finished run.

## What is disabled

- By default, the build does not include the autoresearch lane (see
  [Deploying the controller](./controller-deploy.md#the-autoresearch-lane)), so the UI shows
  only playbooks.
- Without Vault, the write routes of the secrets registry answer 503. A pack that binds no
  secrets launches normally. A launch of a pack that binds secrets fails.
- Runtime overrides (the live caps on the admin page) need the overrides ConfigMap, so the
  controller uses the configuration that it parsed at start.
- Discovery, triage, and the work-pod paths have no cluster and no watched repositories, so
  they do nothing.
