# Authentication

Three kinds of caller reach the controller, each on its own credential:

| Caller | Credential | Reaches |
| --- | --- | --- |
| A person in the browser | a session the controller minted after an OIDC login | the UI and `/api` |
| A person's tools (crux, an MCP client) | an API key (`crk_...`) minted on the Settings page | `/api` and `/mcp` |
| A deployment's automation (CD, sidecars) | the static `CONTROLLER_API_TOKEN` | `/api` only |

Authorization is separate from all three: who may do what is decided by teams, ownership and
the policy set ([RFC-0003](./rfc/RFC-0003.md)), from the login and groups the credential
carries. Three lists bootstrap that on a fresh deployment: `CONTROLLER_ADMINS` (logins that
administer the platform), `CONTROLLER_OPERATORS` (logins that may operate) and
`CONTROLLER_OPERATOR_GROUPS` (IdP groups whose members may operate). They seed the platform
teams while those teams reach nobody; after that, membership is managed in the UI and the
lists are inert.

## The controller as the OIDC relying party

With `CONTROLLER_AUTH_MODE=native` the controller owns `/auth/login`, `/auth/callback` and
`/auth/logout`, runs the authorization-code flow with PKCE against the issuer, validates the ID
token against the issuer's JWKS, and mints its own server-side session (a cookie id, rows in
Postgres). No proxy sits in front, and no identity header is read on any path.

| Variable | Meaning |
| --- | --- |
| `CONTROLLER_AUTH_MODE` | `native` |
| `CONTROLLER_OIDC_ISSUER` | The issuer URL; discovery is read from `<issuer>/.well-known/openid-configuration`. |
| `CONTROLLER_OIDC_CLIENT_ID`, `CONTROLLER_OIDC_CLIENT_SECRET` | The confidential client registered on the issuer. |
| `CONTROLLER_OIDC_REDIRECT_URL` | A callback URL on this controller's host, registered on the client. The controller answers at that path and at `/auth/callback`. |
| `CONTROLLER_OIDC_SCOPES` | Default `openid,email,profile,offline_access`. Never add `groups`: the claim comes from a client mapper, not a scope, and an unregistered scope fails every login. |
| `CONTROLLER_OIDC_POST_LOGOUT_REDIRECT` | Where RP-initiated logout lands the browser. |
| `CONTROLLER_SESSION_SECURE` | `true` behind TLS: the cookie is `Secure` and `SameSite=Lax`. |

The session carries the login, the email and the `groups` claim as the issuer asserted them.
Groups are what team membership rules match on, so an issuer that emits no `groups` claim
grants nobody anything through groups. That is fail-closed and silent; prove it before
switching modes:

```sh
CONTROLLER_OIDC_ISSUER=https://sso.example.com/realms/crucible \
CONTROLLER_OIDC_CLIENT_ID=crucible-controller \
CONTROLLER_OIDC_CLIENT_SECRET=... \
crucible-controller oidc-claims
```

`oidc-claims` runs a one-shot login and prints every claim the issuer hands this client. A
`groups` array with the paths you expect is the pass condition. After the switch,
`GET /api/whoami` in a browser reports `"mode": "native"`, the login, and non-empty groups.

### Offline credentials for scheduled work

A scheduled launch fires with no session, and a launch under a team's secret needs the owner's
live group membership. With `CONTROLLER_CREDENTIAL_KEY` (or `CONTROLLER_CREDENTIAL_KEY_FILE`)
set to a base64 AES-256 key, the callback seals each user's offline refresh token under it,
and a schedule refreshes its owner's groups at fire time through that credential. Without a
key nothing is stored and a schedule runs on the group snapshot its last save took, parking
once that snapshot ages past `CONTROLLER_SCHEDULE_OWNER_TTL_SECS`.

Three things line up, two of them on the issuer:

1. `offline_access` in `CONTROLLER_OIDC_SCOPES`.
2. The issuer granting `offline_access` to the client and to the user.
3. Refresh-token reuse allowed on the client. Two schedules of one owner can fire in one sweep;
   the controller serializes their refreshes per subject, but an issuer that revokes on any reuse
   kills the credential on an overlap it cannot see.

Rotate by putting the new key on the first line and keeping the old one below it: the first line
seals, every line decrypts. `CONTROLLER_SESSION_GROUP_REFRESH_MINUTES` bounds how stale a live
session's groups may get before the next request re-reads them; a refusal from the issuer
downgrades the session to viewer until the user signs in again. Users revoke their own credential
from Settings.

## Keycloak

A confidential client with the authorization-code flow and a group-membership mapper. The
realm the controller's own test suite runs against is at
`crucible-controller/tests/keycloak/crucible-realm.json` and imports as-is with
`start-dev --import-realm`; it is the reference for the settings below.

1. **Client** `crucible-controller`: confidential, Standard flow on, valid redirect URI
   `https://<host>/auth/callback` (plus the exact `CONTROLLER_OIDC_REDIRECT_URL` if it differs).
2. **Group mapper** on that client: mapper type *Group Membership*, token claim name `groups`,
   full group path on, added to the ID token and the userinfo endpoint. The controller reads
   groups as paths (`/platform-devs`), which is what team rules and `CONTROLLER_OPERATOR_GROUPS`
   match against.
3. **Offline access**, if schedules will run under team secrets: the realm's `offline_access`
   role assigned to the users (or to the default roles), refresh-token reuse allowed on the client
   (Revoke Refresh Token off, or Refresh Token Max Reuse at least 1).

With the reference realm running locally:

```sh
docker run -d --name keycloak -p 8180:8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=admin \
  -v "$PWD/crucible-controller/tests/keycloak:/opt/keycloak/data/import:ro" \
  quay.io/keycloak/keycloak:26.4 start-dev --import-realm
export CONTROLLER_AUTH_MODE=native
export CONTROLLER_OIDC_ISSUER=http://127.0.0.1:8180/realms/crucible
export CONTROLLER_OIDC_CLIENT_ID=crucible-controller
export CONTROLLER_OIDC_CLIENT_SECRET=$(jq -r '.clients[] | select(.clientId=="crucible-controller") | .secret' crucible-controller/tests/keycloak/crucible-realm.json)
export CONTROLLER_OIDC_REDIRECT_URL=http://127.0.0.1:8080/auth/callback
```

`alice` (password `alice-password`) is in `/platform-devs`; `mallory` (`mallory-password`) is in
`/other-team`.

## Dex

Dex emits `groups` as a top-level claim whenever the upstream connector supplies groups, so no
mapper is needed. The controller matches the strings Dex emits verbatim, which for most
connectors are bare names (`platform-devs`) rather than paths; use the same spelling in
`CONTROLLER_OPERATOR_GROUPS` and in team rules.

A static client:

```yaml
issuer: https://dex.example.com
staticClients:
  - id: crucible-controller
    secret: ...
    name: crucible
    redirectURIs:
      - https://crucible.example.com/auth/callback
oauth2:
  skipApprovalScreen: true
connectors:
  - type: github
    id: github
    name: GitHub
    config:
      clientID: ...
      clientSecret: ...
      redirectURI: https://dex.example.com/callback
      orgs:
        - name: your-org
          teams:
            - platform-devs
```

Dex issues refresh tokens when `offline_access` is requested and the connector supports it
(GitHub, LDAP, OIDC upstreams do); `oidc-claims` shows whether `groups` arrives with the shape
you expect before you point the controller at it.

## Behind a proxy instead

`CONTROLLER_AUTH_MODE=proxy` is the other model: an OIDC proxy in front runs the login and
asserts the result upstream in `X-Auth-Request-User`, `X-Auth-Request-Email` and
`X-Auth-Request-Groups`. The controller trusts those three headers only on requests that carry
the proxy's own bearer in `CONTROLLER_PROXY_TOKEN`, drops them everywhere else, and refuses a
request that carries the static API token together with any of them. The proxy token must not
be the API token: sharing one would let every API-token holder name any user and any group.

Native mode is the one to build on. Proxy mode exists so a deployment can flip and flip back;
flipping native to proxy flushes every native session.

## API keys

The Settings page mints `crk_<id>_<secret>` keys. The controller stores only the secret's
SHA-256, so a lost key is re-minted, not recovered; keys default to 90 days and can be revoked
on the same page. A key carries its owner's login and the groups their last browser sign-in
recorded, so it is never more powerful than the person and never fresher than their last login.
Minting and revoking refuse a caller who authenticated with a key, so a key can neither mint its
successor nor outlive its own revocation.

Every refusal on the bearer path names what to do next: no header, not a key, not this
controller's key, expired, or revoked. `crux whoami` answering `credential: bearer (anonymous)`
means the string in the header does not start with `crk_`.
