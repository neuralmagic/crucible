# Vault and the secrets registry

Runs need credentials the people launching them own: a registry push token, a Jira token, a
GitHub App key. The secrets registry is how those reach a run without ever passing through a
pod spec by hand. A user or a team registers a secret in the UI, the controller keeps the value
in Vault, and a launch that binds the secret has it delivered to the run's pod as a Kubernetes
Secret owned by that pod ([ADR-0028](https://github.com/neuralmagic/crucible/tree/main/gov/adr),
[ADR-0036](https://github.com/neuralmagic/crucible/tree/main/gov/adr)).

The controller is the fleet's only Vault client. Spokes never hold a Vault token; they receive
Kubernetes Secrets the hub wrote for one run and deletes with it.

## What the controller needs from Vault

- A **KV v2 mount**, `VAULT_MOUNT` (default `crucible`). The registry claims only the
  `crucible/` prefix inside it: a registered secret lives at
  `<mount>/crucible/registry/<owner>/<name>` and every write is a new version, so a mount
  shared with other applications is never claimed at its top level.
- A **login method** with a policy over that mount, one of:

| `VAULT_AUTH` | Variables | Use |
| --- | --- | --- |
| `approle` | `VAULT_ROLE_ID` (or `VAULT_ROLE_ID_FILE`), `VAULT_SECRET_ID_FILE` (or `VAULT_SECRET_ID`, `VAULT_SECRET_ID_WRAPPED` for a response-wrapped one), `VAULT_APPROLE_MOUNT` | Any cluster; the secret id is a mounted Secret. |
| `jwt` | `VAULT_JWT_ROLE`, `VAULT_JWT_TOKEN_FILE`, `VAULT_JWT_MOUNT` | Kubernetes workload identity: a projected ServiceAccount token whose audience Vault's JWT role binds. Nothing to rotate. |

Plus `VAULT_ADDR`, `VAULT_CACERT` for a private CA, `VAULT_NAMESPACE` on Vault Enterprise, and
`VAULT_RENEW_THRESHOLD_SECS` (default 300) for how early the login token is renewed.

The token is the controller's business: it is minted on the first call, renewed before its
TTL, and re-minted when a renewal fails or a call is denied. A denied call is retried exactly
once after a fresh login. Nothing secret is loggable: tokens, secret ids and payloads redact in
`Debug` and have no `Display`.

Without `VAULT_ADDR` the controller is not a Vault client: the registry's write routes answer
503 and everything else works.

## Bootstrapping a mount and an AppRole

```sh
export VAULT_ADDR=https://vault.example.com VAULT_TOKEN=...
vault secrets enable -path=crucible -version=2 kv
vault auth enable approle
vault policy write crucible-hub - <<'EOF'
path "crucible/data/crucible/*"     { capabilities = ["create", "read", "update", "delete"] }
path "crucible/metadata/crucible/*" { capabilities = ["read", "list", "delete"] }
path "crucible/delete/crucible/*"   { capabilities = ["update"] }
EOF
vault write auth/approle/role/crucible-hub token_policies=crucible-hub \
  token_ttl=20m token_max_ttl=60m secret_id_num_uses=0 secret_id_ttl=0
vault read -field=role_id auth/approle/role/crucible-hub/role-id
vault write -f -field=secret_id auth/approle/role/crucible-hub/secret-id
```

Mount the secret id as a file and point the controller at it:

```sh
export VAULT_AUTH=approle
export VAULT_ROLE_ID=<role_id>
export VAULT_SECRET_ID_FILE=/var/run/secrets/vault-approle/secret-id
export VAULT_MOUNT=crucible
```

For the JWT method, enable `jwt` auth with the cluster's OIDC discovery URL as the issuer,
create a role bound to the controller's ServiceAccount and to the audience the projected token
is minted for, and set `VAULT_AUTH=jwt`, `VAULT_JWT_ROLE=crucible-hub` and
`VAULT_JWT_TOKEN_FILE` to the projected token's mount path.

## A local dev server

`vault server -dev` is enough for development and is what the controller's own end-to-end
suite runs against (`crucible-controller/tests/vault_e2e.rs`, which provisions its own mount,
roles and policies). The same bootstrap as above against
`docker run -d -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=root hashicorp/vault:2.0` gives a
working registry; set `VAULT_SECRET_ID` directly instead of a file.

## How a secret moves

1. **Register.** A user or a team registers a secret from the Secrets page: a name, a kind
   (`opaque`, `file`, `registry_authfile`, `kubeconfig`, `inference_api_key`) and the value.
   The value goes straight to Vault as a new version at
   `<mount>/crucible/registry/<owner>/<name>`; the ledger keeps the pointer, the kind,
   the owner and the audit trail, never the value. A secret can also be registered by
   reference: a path outside the mount, verified once with the registrant's own Vault token,
   which is used for one read and dropped.
2. **Bind.** A binding attaches a secret to a scope (a playbook, a schedule) with a projection:
   an environment variable or a file. Binding needs `bind` on the secret and `launch` on the
   scope, decided by the policy set.
3. **Redeem.** When a bound launch is dispatched, the controller reads the current version and
   writes a Kubernetes Secret in the run's namespace, owner-referenced to the run's pod, and
   the pod spec projects it as the binding says. The pod's deletion garbage-collects the Secret.
   The engine's redactor receives every delivered value, so it never appears in a session log.

Visibility on a secret says whether the agent may see it (`agent_visible`) or only the broker
process on the loop pod may (`broker_only`, the default). Team-owned secrets are usable by the
team's members under the team's policy; sharing extends that to named users or groups.

## Deployment secrets

The controller's own credentials (the GitHub token, the API token, the OIDC client secret) are
deployment configuration, not registry entries. Keeping them in the same Vault and syncing them
into the controller's namespace as Kubernetes Secrets is the pattern; a CronJob or an
external-secrets operator reading `<mount>/crucible/deploy/<release>/...` on a schedule does it without
the controller's involvement.
