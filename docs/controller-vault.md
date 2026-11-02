# Vault and the secrets registry

Runs need credentials that belong to the people who launch them: a registry push token, a
Jira token, a GitHub App key. The secrets registry delivers these credentials to a run, so
nobody has to put them in a pod spec. A user or a team registers a secret in the UI, and the
controller keeps the value in Vault. When a launch binds the secret, the controller delivers
it to the pod of the run as a Kubernetes Secret that the pod owns ([ADR-0028](https://github.com/neuralmagic/crucible/tree/main/gov/adr),
[ADR-0036](https://github.com/neuralmagic/crucible/tree/main/gov/adr)).

The controller is the only Vault client in the fleet. Spokes do not have a Vault token. They
get Kubernetes Secrets that the hub writes for one run and deletes with the run.

## What the controller needs from Vault

- A **KV v2 mount**, `VAULT_MOUNT` (default `crucible`). The registry uses only the
  `crucible/` prefix in the mount. A registered secret is at
  `<mount>/crucible/registry/<owner>/<name>`, and every write makes a new version. Other
  applications can share the mount, because the registry does not use its top level.
- A **login method** with a policy over that mount, one of:

| `VAULT_AUTH` | Variables | Use |
| --- | --- | --- |
| `approle` | `VAULT_ROLE_ID` (or `VAULT_ROLE_ID_FILE`), `VAULT_SECRET_ID_FILE` (or `VAULT_SECRET_ID`, or `VAULT_SECRET_ID_WRAPPED` for a response-wrapped id), `VAULT_APPROLE_MOUNT` | Any cluster. The secret id is a mounted Secret. |
| `jwt` | `VAULT_JWT_ROLE`, `VAULT_JWT_TOKEN_FILE`, `VAULT_JWT_MOUNT` | Kubernetes workload identity: a projected ServiceAccount token whose audience the Vault JWT role binds. There is nothing to rotate. |

Also set `VAULT_ADDR`, `VAULT_CACERT` for a private CA, `VAULT_NAMESPACE` on Vault
Enterprise, and `VAULT_RENEW_THRESHOLD_SECS` (default 300), which sets how early the
controller renews the login token.

The controller manages the token. It gets the token on the first call, renews it before its
TTL ends, and gets a new token when a renewal fails or Vault denies a call. The controller
retries a denied call exactly one time after a new login. Secret values cannot go into logs:
tokens, secret ids, and payloads are redacted in `Debug` and have no `Display`.

Without `VAULT_ADDR`, the controller is not a Vault client. The write routes of the registry
answer 503, and all other functions work.

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

For the JWT method, enable `jwt` auth with the OIDC discovery URL of the cluster as the
issuer. Create a role bound to the ServiceAccount of the controller and to the audience of
the projected token. Set `VAULT_AUTH=jwt`, `VAULT_JWT_ROLE=crucible-hub`, and
`VAULT_JWT_TOKEN_FILE` to the mount path of the projected token.

## A local dev server

`vault server -dev` is sufficient for development. The end-to-end suite of the controller
uses it (`crucible-controller/tests/vault_e2e.rs`, which creates its own mount, roles, and
policies). Run the bootstrap above against
`docker run -d -p 8200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=root hashicorp/vault:2.0` to get a
working registry. Set `VAULT_SECRET_ID` directly, not as a file.

## How a secret moves

1. **Register.** A user or a team registers a secret on the Secrets page: a name, a kind
   (`opaque`, `file`, `registry_authfile`, `kubeconfig`, `inference_api_key`), and the value.
   The value goes directly to Vault as a new version at
   `<mount>/crucible/registry/<owner>/<name>`. The ledger keeps the pointer, the kind, the
   owner, and the audit trail, but not the value. You can also register a secret by
   reference: a path outside the mount. The controller verifies it one time with the Vault
   token of the person who registers it. It uses that token for one read and then discards
   it.
2. **Bind.** A binding attaches a secret to a scope (a playbook, a schedule) with a
   projection: an environment variable or a file. To bind, you need `bind` on the secret and
   `launch` on the scope. The policy set decides both.
3. **Redeem.** When the controller dispatches a bound launch, it reads the current version
   and writes a Kubernetes Secret in the namespace of the run, owner-referenced to the pod of
   the run. The pod spec projects it as the binding specifies. When the pod is deleted,
   Kubernetes deletes the Secret. The redactor of the engine gets every delivered value, so
   the value never appears in a session log.

The visibility of a secret tells if the agent can see it (`agent_visible`) or if only the
broker process on the loop pod can see it (`broker_only`, the default). Members of a team can
use secrets that the team owns, under the policy of the team. Sharing gives the same access to
named users or groups.

## Deployment secrets

The credentials of the controller (the GitHub token, the API token, the OIDC client secret)
are deployment configuration, not registry entries. Keep them in the same Vault, and sync them
into the namespace of the controller as Kubernetes Secrets. A CronJob or an external-secrets
operator that reads `<mount>/crucible/deploy/<release>/...` on a schedule can do this. The
controller is not involved.
