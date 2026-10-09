# The crucible controller as the single OAuth issuer for people

Status: DRAFT (unit tests pass; live Claude Code login pending).

## What was built (branch `spike-dex-auth`, uncommitted)

| Piece | Where |
| --- | --- |
| Authorization code + PKCE (S256) for public client `claude-code`, consent page, code store (60 s, single use, bound to client, redirect, challenge, resource, login) | `crucible-controller/src/agent_auth/people.rs` |
| `POST /oauth/token` dispatches `authorization_code` and `refresh_token` (rotating, 8 h from approval) beside the agents' `client_credentials` | `crucible-controller/src/agent_auth/mod.rs` |
| RFC 8414 metadata gains `authorization_endpoint`, `code`, `S256`, `none`, `authorization_response_iss_parameter_supported` only when people are served | `crucible-controller/src/agent_auth/issuer.rs` |
| Per-(subject, cluster) identity links proven by that cluster's TokenReview; token never stored | `crucible-controller/src/identity/kube_links.rs`, migration `0059_kube_identity_links.sql` |
| `GET /api/me/kube-users`, `PUT /api/me/kube-users/{cluster}` | `crucible-controller/src/identity/api/kube_user.rs` |
| Per-cluster `kube_users = "linked" \| "sso-login"` (operator-declared) | `forge/src/fleet.rs` (`[clusters.<name>]`) |
| `/oauth/authorize` mounted inside the session layer, outside the API guard | `crucible-controller/src/lib.rs` |

Configuration: `CONTROLLER_PEOPLE_AUDIENCES=<cluster>:<resource url>=<scopes>,...` on top of the
existing `CONTROLLER_AGENT_*` issuer (same key, same `iss`, same JWKS).

## Token a person gets

```json
{"iss": "<controller>", "sub": "weaton", "aud": "<resource>", "azp": "claude-code",
 "scope": "kueue.read kueue.submit", "kube_user": "wseaton", "kube_cluster": "pirate",
 "iat": 0, "nbf": 0, "exp": 900, "jti": "..."}
```

No `run_id`, `act`, `namespace` or `role`, so crucible-kueue treats it as a person (RBAC as
`kube_user`, any namespace). crucible-kueue and crucible-mcp-auth need no change.

## Tests

`cargo test -p crucible-controller --lib -- agent_auth:: identity::kube_links identity::api::kube_user client::tests`

- redirect validation: https-localhost, no port, :80 (normalized away), :0, wrong/trailing path,
  dot segments, query, fragment, userinfo, uppercase host, `localhost.evil`, 127.0.0.2, 0.0.0.0,
  v4-mapped v6
- PKCE: RFC 7636 vector, mismatch, short/long/space verifiers, malformed challenges
- codes: single use, expiry (an expired code is spent too), bounded store with pruning
- authorize: bad client/redirect shown to the browser, everything else redirected with `state`;
  resource allowlist (`invalid_target`), unknown scope, plain method, missing/oversized state,
  repeated parameters
- full flow over a real session store: anonymous -> `/auth/login?rd=<this request>`, consent,
  token from another cluster refused, link, code, token claims, code replay refused, approval
  replay refused, forged pending id, deny -> `access_denied`
- token endpoint: wrong verifier, missing verifier, wrong redirect, wrong resource, wrong client,
  `client_secret` present, failed redemption spends the code
- refresh: rotation, reuse refused, scope widening refused
- links: TokenReview username recorded, token not stored, another cluster's token refused,
  `system:` users refused, unreachable cluster fails closed, relink per cluster
- `sso-login` cluster: no link needed, `kube_user` = login

## Live evidence

TODO

## Fleet sketch (not built)

A fleet-wide MCP server (one resource fronting several clusters) gets
`"kube_users": {"pirate": "wseaton", "mpp": "weaton"}` instead of `kube_user`, built from every
link plus every `sso-login` cluster at mint time. crucible-mcp-auth would read
`claims.kube_users[cluster]` for the cluster a call targets and refuse a call to a cluster the map
omits. Single-cluster audiences keep the flat `kube_user`.

## Caveats

TODO
