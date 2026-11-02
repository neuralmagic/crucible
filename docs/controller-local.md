# Running the controller locally

The controller can run on a laptop without an identity provider, Vault, or a cluster.
Playbook launches run as `crucible plan run` subprocesses on the same machine. If the agent
turns of a pack need an OpenShell sandbox, the engine starts one on the podman of the host.

```sh
just controller-local
```

Then open <http://127.0.0.1:8870>. The recipe needs bun and a Rust toolchain. It needs
podman only for OpenShell sandboxes. The recipe:

- builds the UI and the `crucible`, `crucible-controller` (with `embedded-db`), and `crux`
  binaries;
- runs `crucible-controller autopilot` against its own Postgres, with the settings below.

`just controller-local 9000 wren` uses a different port and login. If no other address is
configured, crux uses `http://127.0.0.1:8870`, so on the default port it needs no setup:

```sh
crux whoami
crux draft-create notes --description "release notes"
crux draft-push notes examples/playbook --base-version 1
crux draft-launch notes --max-cost 1 --max-time 5m
```

## What the recipe sets

| Variable | Value | Why |
| --- | --- | --- |
| `DATABASE_URL` | `embedded` | The controller runs its own Postgres. |
| `CONTROLLER_STATE_DIR` | `$XDG_STATE_HOME/crucible-controller` | The location of the database and the local runs. |
| `CONTROLLER_DEV_IDENTITY` | `$USER` | Every request uses this login. |
| `CONTROLLER_ADMINS` | `$USER` | Makes that login an admin, so you can launch and edit in the UI. |
| `CONTROLLER_PLAYBOOK_EXECUTOR` | `local` | Launches run as a local subprocess, not as a work pod. |
| `CONTROLLER_SCOPE_EXECUTOR` | `disabled` | Disables autopilot scoping, which would otherwise run `crucible scope` automatically. |
| `CONTROLLER_SESSION_SECURE` | `false` | The UI is plain http. |
| `KUBECONFIG` | `/dev/null` | The controller does not connect to the cluster in your kubeconfig. |
| `OPENSHELL_PODMAN_SOCKET` | the API socket of the podman machine | The engine starts OpenShell sandboxes here. Set it to use a different socket. |
| `CRUCIBLE_BIN` | the built `crucible` | The engine a launch runs. |
| `CONTROLLER_HOOKS_ADDR` | `127.0.0.1:8871` (set `HOOKS_PORT` to change it) | The webhook delivery listener, on a separate port. |
| `CONTROLLER_HOOKS_PUBLIC_URL` | `http://127.0.0.1:8871` | The base of the delivery URL of each new webhook. |
| `CONTROLLER_WEBHOOK_KEY_FILE` | `$CONTROLLER_STATE_DIR/webhook.key`, generated once | Seals `hmac_sha256` webhook secrets. |

The recipe unsets `CONTROLLER_API_TOKEN`, `CONTROLLER_PROXY_TOKEN`, `CONTROLLER_OIDC_ISSUER`,
and `VAULT_ADDR` for the process, also if your shell exports them.

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

Without `CONTROLLER_API_TOKEN` and `CONTROLLER_OIDC_ISSUER`, the guard is open. It admits
every request and binds loopback, whatever `CONTROLLER_API_ADDR` is. An open request has no
identity, so it is a viewer. `CONTROLLER_DEV_IDENTITY` gives it an identity. It also removes
any `X-Auth-Request-*` header from the client, so a browser and curl get the same identity.
The controller does not boot if this variable is set on a guarded deployment, because it would
give every caller that identity.

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

The recipe serves the webhook delivery listener on `127.0.0.1:8871`.
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
