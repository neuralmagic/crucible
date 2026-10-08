# MCP auth on pirate

Keycloak (realm `crucible`) is the authorization server for crucible's MCP servers; each MCP
server is an OAuth resource server built on the `crucible-mcp-auth` crate. Humans log in through
OpenShift (client `claude-code`). Sandboxes authenticate with their SPIRE JWT-SVID: the OpenShell
supervisor's provider `token_grant` posts it as a `client_credentials` `client_assertion` to a
per-run `federated-jwt` client pinned to that sandbox's SPIFFE ID, and injects the resulting
bearer only on egress to the bound MCP endpoint.

```text
 supervisor ──JWT-SVID──▶ Keycloak (federated-jwt client run-<run>) ──access token──▶ supervisor
   sub=spiffe://pirate.crucible/openshell/sandbox/<id>        sub=<launcher>, act.sub=<spiffe id>,
                                                               aud=<MCP URLs>, run_id, namespace,
                                                               queues, role, scope
 supervisor ──Authorization: Bearer──▶ crucible-kueue ──▶ JWKS check, tool policy, Kueue
```

| File | Scope | What |
| --- | --- | --- |
| `operators.yaml` | cluster | RHBK 26.6 + Zero Trust Workload Identity Manager operators |
| `spire.yaml` | cluster | SPIRE server/agent DaemonSet/CSI driver/OIDC discovery, trust domain `pirate.crucible` |
| `clusterspiffeid.yaml` | cluster | SPIFFE IDs for supervisors in crucible-system and the spike pod in rhbk |
| `openshift-oauth-client.yaml` | cluster | OAuthClient for the openshift-v4 IdP |
| `keycloak.yaml` | rhbk | Postgres, Keycloak, edge Route |
| `realm.yaml` | rhbk | Realm import (IdPs, scopes, `claude-code`, the `run-auth-spike` client) |
| `spike.yaml` | rhbk | Pod that fetches one JWT-SVID shaped like a supervisor's |
| `kueue-policy.toml` | - | crucible-kueue tool policy (`--tool-policy`) |
| `apply.sh` | - | Applies all of the above in order; safe to re-run |

## Apply

```bash
crucible-kueue/deploy/auth/apply.sh
```

`realm.yaml` is a `KeycloakRealmImport`, which only creates a realm that does not exist; changes
to it after the first import go through the admin console or API.

## Realm

- IdP `spire` (OIDC type, `supportsClientAssertions`, `supportsClientAssertionReuse` because
  JWT-SVIDs carry no `jti`) verifies assertions against SPIRE's in-cluster JWKS.
- A run client has `clientAuthenticatorType: federated-jwt`, `jwt.credential.issuer: spire`,
  `jwt.credential.sub: <sandbox SPIFFE ID>`, default scopes `kueue.read kueue.submit` (and not
  `basic`, so `sub` comes only from its hardcoded mapper), and hardcoded claim mappers for `sub`,
  `act`, `run_id`, `namespace`, `queues`, `role`. Deleting it revokes the sandbox.
- Keycloak finds the client without a `client_id` (OpenShell sends none): the assertion's `iss`
  selects the IdP and its `sub` selects the client. The assertion's `aud` must be exactly the
  realm issuer URL, which is OpenShell's default JWT-SVID audience for a token endpoint under it.
- Scopes `kueue.read` / `kueue.submit` carry the crucible-kueue audiences, so requesting the
  scope binds the token to that resource server.

## crucible-kueue

```bash
crucible-kueue \
  --oidc-issuer https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible \
  --oidc-jwks-uri http://keycloak-service.rhbk.svc.cluster.local:8080/realms/crucible/protocol/openid-connect/certs \
  --resource http://crucible-kueue.crucible-system.svc.cluster.local:8850/mcp \
  --tool-policy /etc/crucible-kueue/kueue-policy.toml \
  --tenant-namespace crucible-kueue-tenants
```

Tenant tokens keep working alongside access tokens; each request logs `auth=oidc` or
`auth=tenant-token`. A realm token must carry `run_id`, `namespace`, `role` and may carry
`queues`; its `sub` is stamped on created workloads as `crucible.dev/launched-by`. The shipped
policy additionally records `act.sub` as the `crucible.dev/actor` annotation.

Unauthenticated requests get `401` with
`WWW-Authenticate: Bearer resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`;
the metadata document (RFC 9728) names the realm as the authorization server.

## OpenShell provider

The sandbox side needs the embedded gateway to have `providers_v2_enabled` set and
`provider_spiffe_workload_api_socket_path = "/spiffe-workload-api/spire-agent.sock"` in its
`[openshell.drivers.kubernetes]` block (the driver then mounts `csi.spiffe.io` into the supervisor
pod). The provider profile's credential:

```toml
[token_grant]
token_endpoint = "http://keycloak-service.rhbk.svc.cluster.local:8080/realms/crucible/protocol/openid-connect/token"
jwt_svid_audience = "https://keycloak.apps.openshift.nm.rdu4.corp.redhat.com/realms/crucible"
```

`jwt_svid_audience` is required because the in-cluster token endpoint differs from the issuer.
Pod networks cannot resolve `*.apps` hosts on pirate, so the in-cluster Service is the only
token endpoint a supervisor can reach.

## Claude Code

Humans use the pre-registered public client `claude-code` (auth code + PKCE S256, loopback
redirect on port 8765, OpenShift login). In `.mcp.json`:

```json
{
  "mcpServers": {
    "kueue": {
      "type": "http",
      "url": "https://crucible-kueue.apps.openshift.nm.rdu4.corp.redhat.com/mcp",
      "oauth": { "clientId": "claude-code", "callbackPort": 8765 }
    }
  }
}
```

Claude Code follows the 401's `resource_metadata` to the realm and requests `kueue.read`. A human
token only reaches tools if its user carries `crucible_namespace`, `crucible_queues`, and
`crucible_role` attributes (mapped by the `crucible-tenant` scope) and a `run_id`; there is no
crucible-kueue Route yet. pirate has no wildcard DNS for `*.apps`, so the Keycloak host also needs
an `/etc/hosts` entry pointing at the ingress VIP.

## Spike and e2e

```bash
CRUCIBLE_KUEUE_E2E_CONTEXT=crucible-system/api-openshift-nm-rdu4-corp-redhat-com:6443/wseaton \
CRUCIBLE_KUEUE_E2E_NAMESPACE=crucible-kueue-e2e CRUCIBLE_KUEUE_E2E_QUEUE=crucible-e2e \
CRUCIBLE_KUEUE_E2E_INGRESS_IP=10.14.217.8 \
cargo test -p crucible-kueue --test oidc_e2e -- --ignored --nocapture
```

The e2e recreates `spike.yaml`, exchanges its JWT-SVID at the realm exactly as the supervisor
does, and drives crucible-kueue on loopback with the resulting token and `kueue-policy.toml`.
