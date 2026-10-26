# Authentication

Three kinds of callers use the controller. Each kind has its own credential:

| Caller | Credential | Reaches |
| --- | --- | --- |
| A person in the browser | a session that the controller creates after an OIDC login | the UI and `/api` |
| The tools of a person (crux, an MCP client) | an API key (`crk_...`) from the Settings page | `/api` and `/mcp` |
| The automation of a deployment (CD, sidecars) | the static `CONTROLLER_API_TOKEN` | `/api` only |

Authorization is separate from the credential. Teams, ownership, and the policy set
([RFC-0003](./rfc/RFC-0003.md)) decide what each caller can do, from the login and groups in
the credential. On a new deployment, four lists set the first members:

- `CONTROLLER_ADMINS`: logins that administer the platform;
- `CONTROLLER_OPERATORS`: logins that can operate;
- `CONTROLLER_OPERATOR_GROUPS`: IdP groups whose members can operate;
- `CONTROLLER_PUBLISHERS`: logins that can publish their own drafts without review.

These lists seed the `platform-administrators`, `platform-operators`, and
`playbook-publishers` teams while those teams have no members. A login on the admin or
operator list has that team membership on every request, also if the team does not list it.
Membership that you add in the UI has the same effect. An owner of `platform-administrators`
is an admin. Any other member of `platform-administrators` or `platform-operators` is an
operator. This applies to the routes that need admin or operator, and to the `role` that
`/api/whoami` reports.

Each team has a workspace at `/teams/<slug>`. Each list has a team path (for example
`/teams/<slug>/secrets`) that shows only the resources of that team.

### Opening a deployment to an organization

A signed-in user who is in no team can read and launch the playbooks and providers that
`team:platform-administrators` owns, and dispatch to its targets. The user owns all resources
that they create. To give one organization more access, without giving it to every login
that the issuer accepts, create a team whose members come from a rule. Then give the shared
resources to that team. The Teams page makes the first call:

```text
POST /api/teams {"slug": "my-org", "display_name": "My org",
  "members": [{"kind": "rule", "member": "group-suffix:my-org-group", "role": "member"},
              {"kind": "user", "member": "<you>", "role": "owner"}]}
PUT /api/playbooks/<id>/owner {"owner": "team:my-org"}
PUT /api/providers/<id>/owner {"owner": "team:my-org"}
```

Members can read, launch, and dispatch what the team owns. Maintainers and owners can also
edit it. A provider uses its key on every dispatch, so each user who can select the provider
spends with that key. Move a provider to a team only if the whole team must use it.
`email-domain:<domain>` rules match only in native mode, where the controller records the
email of each login.

### The autoresearch lane

On a deployment that runs autoresearch (`CONTROLLER_AUTORESEARCH=true`), only entitled
callers see the pages and routes of the lane (issues, inbox, turns, builds, repos, explore,
the autopilot switch). Other callers get a 404, and their navigation does not show these
pages. The entitlement is `read` on the lane, which the `autoresearch` team owns. Its members
have the entitlement, platform administrators have all entitlements, and a policy set can give
it to other users. The controller seeds the team with `platform-operators` as a nested team.
To give a person access to the lane, add a group rule or the person to the team.
`/api/whoami` reports the entitlements of the caller under `entitlements`.

### Viewing as another user

A platform administrator (`platform:impersonate`) can view the controller as any user who has
signed in, from the row of the user on a team page. The browser session then uses the identity
of that user, with the groups from their last sign-in, for one hour or until the administrator
stops it. The view is read-only. The controller rejects every write except
`DELETE /api/impersonation`. The event log records the start and the stop under the login of
the administrator. API keys and bearer tokens cannot use a view.

## The controller as the OIDC relying party

With `CONTROLLER_AUTH_MODE=native`, the controller serves `/auth/login`, `/auth/callback`,
and `/auth/logout`. It runs the authorization-code flow with PKCE against the issuer,
validates the ID token against the JWKS of the issuer, and creates its own server-side
session (a cookie id and rows in Postgres). There is no proxy in front, and the controller
reads no identity header on any path.

| Variable | Meaning |
| --- | --- |
| `CONTROLLER_AUTH_MODE` | `native` |
| `CONTROLLER_OIDC_ISSUER` | The issuer URL. The controller reads discovery from `<issuer>/.well-known/openid-configuration`. |
| `CONTROLLER_OIDC_CLIENT_ID`, `CONTROLLER_OIDC_CLIENT_SECRET` | The confidential client registered on the issuer. |
| `CONTROLLER_OIDC_REDIRECT_URL` | A callback URL on the host of this controller, registered on the client. The controller answers at that path and at `/auth/callback`. |
| `CONTROLLER_OIDC_SCOPES` | Default `openid,email,profile,offline_access`. Do not add `groups`. The claim comes from a client mapper, not a scope, and an unregistered scope makes every login fail. |
| `CONTROLLER_OIDC_POST_LOGOUT_REDIRECT` | The page that the browser opens after RP-initiated logout. |
| `CONTROLLER_SESSION_SECURE` | Set `true` behind TLS. The cookie is then `Secure` and `SameSite=Lax`. |

The session holds the login, the email, and the `groups` claim from the issuer. Team
membership rules match on groups. If the issuer emits no `groups` claim, no user gets access
through groups, and there is no error. Test the claim before you change the mode:

```sh
CONTROLLER_OIDC_ISSUER=https://sso.example.com/realms/crucible \
CONTROLLER_OIDC_CLIENT_ID=crucible-controller \
CONTROLLER_OIDC_CLIENT_SECRET=... \
crucible-controller oidc-claims
```

`oidc-claims` runs one login and prints every claim that the issuer gives this client. The
test passes if the output has a `groups` array with the expected paths. After the change,
`GET /api/whoami` in a browser reports `"mode": "native"`, the login, and a list of groups
that is not empty.

### Offline credentials for scheduled work

A scheduled launch has no session, and a launch that uses a team secret needs the current
group membership of the owner. Set `CONTROLLER_CREDENTIAL_KEY` (or
`CONTROLLER_CREDENTIAL_KEY_FILE`) to a base64 AES-256 key. The callback then seals the offline
refresh token of each user with that key, and a schedule uses that credential to refresh the
groups of its owner when it fires. Without a key, the controller stores nothing. A schedule
then uses the group snapshot from its last save, and it parks when the snapshot is older than
`CONTROLLER_SCHEDULE_OWNER_TTL_SECS`.

Three conditions are necessary. Two of them are on the issuer:

1. `offline_access` is in `CONTROLLER_OIDC_SCOPES`.
2. The issuer grants `offline_access` to the client and to the user.
3. The client allows refresh-token reuse. Two schedules of one owner can fire in one sweep.
   The controller runs their refreshes one at a time for each subject, but an issuer that
   revokes on any reuse can revoke the credential when two refreshes overlap.

To rotate the key, put the new key on the first line and keep the old key below it. The first
line seals, and every line decrypts. `CONTROLLER_SESSION_GROUP_REFRESH_MINUTES` sets the
maximum age of the groups of a live session before the next request reads them again. If the
issuer refuses, the session becomes a viewer until the user signs in again. Users revoke their
own credential on the Settings page.

## Keycloak

Use a confidential client with the authorization-code flow and a group-membership mapper.
The test suite of the controller uses the realm in
`crucible-controller/tests/keycloak/crucible-realm.json`. It imports without changes with
`start-dev --import-realm`. It is the reference for the settings below.

1. **Client** `crucible-controller`: confidential, Standard flow on, valid redirect URI
   `https://<host>/auth/callback` (plus the exact `CONTROLLER_OIDC_REDIRECT_URL` if it differs).
2. **Group mapper** on that client: mapper type *Group Membership*, token claim name
   `groups`, full group path on, added to the ID token and the userinfo endpoint. The
   controller reads groups as paths (`/platform-devs`). Team rules and
   `CONTROLLER_OPERATOR_GROUPS` match against these paths.
3. **Offline access**, if schedules run with team secrets: assign the `offline_access` role
   of the realm to the users (or to the default roles), and allow refresh-token reuse on the
   client (Revoke Refresh Token off, or Refresh Token Max Reuse at least 1).

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

Dex emits `groups` as a top-level claim when the upstream connector supplies groups, so you
do not need a mapper. The controller matches the exact strings that Dex emits. For most
connectors, these are bare names (`platform-devs`), not paths. Use the same names in
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

Dex issues refresh tokens if the client requests `offline_access` and the connector supports
it (GitHub, LDAP, and OIDC upstreams do). Before you point the controller at Dex, use
`oidc-claims` to check that `groups` has the expected format.

## Behind a proxy instead

With `CONTROLLER_AUTH_MODE=proxy`, an OIDC proxy in front of the controller runs the login. It
sends the result in `X-Auth-Request-User`, `X-Auth-Request-Email`, and
`X-Auth-Request-Groups`. The controller trusts these three headers only on requests that
carry the proxy bearer token from `CONTROLLER_PROXY_TOKEN`. It removes them from all other
requests. It rejects a request that has the static API token and any of these headers. The
proxy token must be different from the API token. If they were the same, every holder of the
API token could claim any user and any group.

Use native mode for new deployments. Proxy mode lets a deployment change modes and change
back. A change from native to proxy deletes all native sessions.

## API keys

The Settings page creates `crk_<id>_<secret>` keys. The controller stores only the SHA-256
of the secret, so you cannot recover a lost key. Create a new one. By default, keys expire
after 90 days. You can revoke a key on the same page. A key has the login of its owner and the
groups from their last browser sign-in. A key never has more access than its owner, and its
groups are never newer than the last login. A caller that authenticated with a key cannot
create or revoke keys. A key cannot create a replacement key or stay valid after revocation.

Each rejection on the bearer path tells the cause: no header, not a key, a key of a different
controller, expired, or revoked. If `crux whoami` answers `credential: bearer (anonymous)`,
the string in the header does not start with `crk_`.

## Cluster identities

MCP servers that check Kubernetes permissions (SubjectAccessReview) need to know who a person is on
the cluster they run on. People's access tokens carry it as `kube_user`, and Settings → Cluster
identities lists every cluster the deploy profile knows, the rule that cluster uses, and the
caller's username there. The deploy profile declares each cluster's rule:

```toml
[cluster]                                 # the hub
identity = { claim = "login" }            # usernames are SSO logins

[clusters.spoke-b]
kubeconfig_secret = "spoke-b-kubeconfig"
identity = { claim = "email" }            # the cluster's OIDC login uses the same email

[clusters.spoke-a]
kubeconfig_secret = "spoke-a-kubeconfig"
identity = "link"                         # the default: each person proves it to the cluster
```

`prefix` puts the API server's OIDC username prefix in front of a claim
(`{ claim = "login", prefix = "sso:" }`). A claim cluster shows the resolved name read-only. A
`link` cluster shows Link, Relink and Unlink, and records only what the cluster itself said: the
link's username, how it was proven, and when. The token used to prove it is never stored or logged.
On the hub, an administrator's override (`PUT /api/users/{login}/kube-user`, audited, never
self-applied) wins over the rule and shows as such; it is also what the person's runs act as there.

### Linking through an OpenShift cluster's OAuth server

The controller registers as a confidential `OAuthClient` on each cluster listed in the profile's
`[cluster_link]` table. Link sends the browser to that cluster's authorize endpoint (read from
`/.well-known/oauth-authorization-server` on its API server) with PKCE and a state bound to the
person's session. The callback redeems the code for a `user:info` token, the cluster's TokenReview
names its owner, the username is recorded, and the token is dropped.

```toml
[cluster_link]
external_url = "https://crucible.apps.hub.example.com"
openshift = ["spoke-a"]
# client_id = "crucible-controller"
# access_token_max_age_secs = 300
```

Render the client for one cluster, then create it there with a fresh secret, and give the same
secret to the controller under the cluster's name:

```sh
crucible deploy render --profile profile.toml --cluster-link spoke-a > oauthclient-spoke-a.yaml
secret=$(openssl rand -hex 32)
oc --context spoke-a apply -f oauthclient-spoke-a.yaml
oc --context spoke-a patch oauthclient crucible-controller --type merge -p "{\"secret\":\"$secret\"}"
oc --context hub -n crucible-system create secret generic crucible-cluster-link \
  --from-literal=spoke-a="$secret" --dry-run=client -o yaml | oc --context hub apply -f -
```

`OAuthClient` is cluster-scoped, so creating it needs cluster-admin on that cluster. Mount the
`crucible-cluster-link` Secret into the controller and point `CONTROLLER_CLUSTER_LINK_SECRETS` at
the directory: one file per cluster, named for it. A listed cluster without a file offers the paste
fallback only. The controller's own credential on each cluster needs `tokenreviews` create
(`system:auth-delegator`) to review the token, and the controller reaches the cluster's token
endpoint (its `oauth-openshift` route) directly, not through the cluster's `proxy_url`.

### Pasting a token

A `link` cluster without OAuth takes a pasted `oc whoami -t` or an OIDC ID token the cluster's API
server accepts. The cluster's TokenReview names the owner; the token is dropped the same way.

| Route | What it does |
| --- | --- |
| `GET /api/me/cluster-identities` | Every cluster's rule and the caller's username. |
| `PUT /api/me/cluster-identities/{cluster}` | Link by pasted token. |
| `POST /api/me/cluster-identities/{cluster}/link` | Start the OAuth link; browser sessions only. |
| `GET /oauth/cluster-link/{cluster}/callback` | The cluster's redirect; lands on `/settings?linked=` or `?link_error=`. |
| `DELETE /api/me/cluster-identities/{cluster}` | Unlink. |
| `GET /api/oauth/consent?id=` | The authorization request parked on the session, for the consent page. |
| `POST /api/oauth/consent` | Approve or deny it; answers the client's redirect URI to navigate to. |

API keys can read but never link or unlink.

### MCP clients

People sign in with public OAuth clients (no secret, PKCE S256 required). Each client has its own
`client_id`, and every code, approval and refresh token is bound to the client it was issued to:
redeeming a code as another client is `invalid_grant`, and presenting a refresh token as another
client is `invalid_grant` and revokes that approval. Access tokens carry the client in `azp`.
Without configuration, three clients are registered:

| `client_id` | Name | Redirect URIs it may use |
| --- | --- | --- |
| `claude-code` | Claude Code | `http://localhost:<any port>/callback` |
| `pi` | pi | `http://127.0.0.1:<any port>/callback` |
| `codex` | Codex | `http://127.0.0.1:<any port>/callback` and `http://127.0.0.1:<any port>/callback/<server id>` |

Redirect matching follows RFC 8252 §7.3. A registered `http` URI on a loopback host matches any
port on that same host with the same path, and nothing else: no query, no fragment, no
credentials, and the request's URI must already be in canonical form. `localhost`, `127.0.0.1` and
`[::1]` are three different hosts. A client registered on `localhost` cannot use `127.0.0.1`, and
the reverse. Any other registered URI must be `https` and matches only character for character.
`http` on a host that is not a loopback host is never accepted. Codex adds a 12 character id of the
MCP server to a pre-registered client's callback path. A registered path that ends in `/*` allows
that one extra segment (1 to 64 of `A-Z a-z 0-9 - _`), and the built-in `codex` client uses it.

To register other clients, or to change the built-in ones, point `CONTROLLER_PEOPLE_CLIENTS_FILE`
at a TOML file. The file replaces the built-in list:

```toml
[[clients]]
client_id = "claude-code"
name = "Claude Code"
redirect_uris = ["http://localhost/callback"]

[[clients]]
client_id = "codex"
name = "Codex"
redirect_uris = ["http://127.0.0.1/callback", "http://127.0.0.1/callback/*"]

[[clients]]
client_id = "lab-cli"
name = "Lab CLI"
redirect_uris = ["http://127.0.0.1/oauth/done"]
resources = ["https://crucible-kueue.apps.spoke-a.example.com/mcp"]   # default: every audience
```

A registered `client_id` is 1 to 64 of `A-Z a-z 0-9 . _ -`, so it can never look like a URL. If
you remove a client, or narrow its `resources`, its refreshes are refused from the next refresh.

#### Self-registered clients

pi 1.x, Codex (registration `auto`) and Claude Code without `--client-id` register a client of
their own before they sign in (RFC 7591). With people OAuth configured, the controller accepts
such registrations at `POST /oauth/register` and advertises `registration_endpoint`.
`CONTROLLER_PEOPLE_DCR=false` turns registration off and removes `registration_endpoint` from
the metadata. Registration accepts only native public clients:

- `token_endpoint_auth_method` is absent or `none`. No secret, registration access token or
  management endpoint is issued;
- `grant_types` stay within `authorization_code` and `refresh_token`, and `response_types` within
  `code`;
- every `redirect_uris` entry is `http` on `localhost`, `127.0.0.1` or `[::1]`, with no query,
  fragment or credentials, and at most 8 entries. At authorize, any port on the registered host
  with the registered path matches.

The client gets a random `dyn-…` `client_id`. Its codes and refresh tokens are bound to that id
like any other client's. The name it chose is cleaned and cut to 100 characters, and the consent
page and Settings always show it as "<name> (self-registered)". The consent page also shows
the `host:port` the code goes to. A registration that has made no successful token exchange (a
code or a refresh) for 30 days stops authorizing and refreshing, and the hourly sweep deletes it
with its approvals. Naming it to `/oauth/authorize`, signed in or not, does not keep it alive. Each source address may register
20 times an hour, and about 10000 registrations live at once (`429` and `503` past those
limits; concurrent registrations on several replicas can go slightly over the cap). The source is
the TCP peer, or the last `X-Forwarded-For` hop when the peer is in
`CONTROLLER_PEOPLE_TRUSTED_PROXIES` (the networks the cluster router connects from). Without that
setting every registration behind a router counts against the router's address.

#### Client ID Metadata Documents

When `CONTROLLER_PEOPLE_CIMD_HOSTS` lists hosts, a `client_id` that is an `https://` URL on one
of those hosts is resolved as an OAuth Client ID Metadata Document
(draft-ietf-oauth-client-id-metadata-document, the MCP 2025-11-25 client registration). The
metadata then advertises `client_id_metadata_document_supported: true`. This is off by
default, and every URL `client_id` is refused when it is off. The controller fetches the
document when it is asked to authorize and refuses the request, before any consent, unless all
of these are true:

- the URL is canonical `https` with a host name (not an IP address) and a path, and has no
  credentials, query or fragment;
- the host is on the allowlist, and resolves to a public address, never a loopback, private,
  link-local or other RFC 6890 special-use address (with `HTTPS_PROXY` set, the proxy resolves
  names, so this check moves to the proxy);
- the answer is `200` (redirects are not followed), JSON, at most 5 KiB, within 5 seconds;
- its `client_id` equals the URL exactly, it has a `client_name` and at least one
  `redirect_uris` entry, `token_endpoint_auth_method` is `none`, it carries no `client_secret`,
  and any `grant_types` or `response_types` include `authorization_code` and `code`.

Its redirect URIs follow the same rules as registered ones. A loopback URI in a document matches
any port, but a document cannot use `/*`. A valid document is cached for its `Cache-Control`
`max-age` (minus `Age`), 5 minutes if it sends none, and 1 hour at most. A document with
`no-store` or `no-cache`, and any refused document, is never cached. The token endpoint does not
fetch anything. It checks that the URL's host is still allowed, and the grant it redeems is bound
to that URL. The consent page and Settings show the document's `client_name` with the URL's
host (for example "Codex (chatgpt.com)"), so a document cannot pass for a registered client.

Codex publishes `https://chatgpt.com/oauth/codex/client.json` and pi publishes
`https://pi.dev/oauth/client.json`. Allowlist `chatgpt.com` and `pi.dev` to accept them.

#### Signing in from each tool

Use the MCP server's URL, a people resource of the deploy profile (`[kueue] people`, at its
Service URL or its `people_urls` entry). Each tool
finds this controller through the server's protected resource metadata.

With self-registration on (the default), no client options are needed:

```sh
# Claude Code: then run /mcp in a session and choose Authenticate.
claude mcp add --transport http crucible-kueue https://crucible-kueue.apps.spoke-a.example.com/mcp

# pi
pi mcp add crucible-kueue --url https://crucible-kueue.apps.spoke-a.example.com/mcp
pi mcp login crucible-kueue

# Codex (mcp add starts the sign-in itself; mcp login repeats it)
codex mcp add crucible-kueue --url https://crucible-kueue.apps.spoke-a.example.com/mcp
codex mcp login crucible-kueue
```

Each tool then signs in as a self-registered client. To sign in under the registered client
instead, so that consent and Settings show the verified name, or when `CONTROLLER_PEOPLE_DCR=false`,
pass the registered id: `claude mcp add --client-id claude-code …`,
`pi mcp add … --oauth-client-id pi`, `codex mcp add … --oauth-client-id codex`. Without
self-registration and without a client id, Codex ends with "Dynamic client registration not
supported", and pi ends with "Authorization server does not support dynamic client
registration".

If `chatgpt.com` is on `CONTROLLER_PEOPLE_CIMD_HOSTS`, Codex in `auto` mode prefers its metadata
document over registering, and signs in as `https://chatgpt.com/oauth/codex/client.json`. pi
signs in under its document when its `mcp.json` entry has
`"oauth": {"clientRegistration": "cimd", "callbackUrl": "http://127.0.0.1/callback"}` and `pi.dev`
is allowlisted.

pi used to borrow `claude-code`. Under `claude-code`, pi's `127.0.0.1` callback is now refused.
Re-add the server without OAuth options (or with `--oauth-client-id pi`) and sign in again. An
existing pi approval keeps refreshing as `claude-code` until it expires or someone signs it out.

### The consent page

`GET /oauth/authorize` from an MCP client parks the request on the browser session and sends the
browser to `/consent?id=…` in the controller UI. The page names the client (its registered name,
a metadata document's `client_name` with its host, or a self-registered client's chosen name
marked "(self-registered)") and shows its `client_id`, the `host:port` the code goes to, the server,
scopes and login, and the person's username on the server's cluster. A `link` cluster nobody has
linked offers Link (and the paste box where the cluster has no OAuth) right there, and the sign-in
lands back on the same page. Allow and Deny are a JSON call that returns the client's redirect URI
with a code or an error, and the page navigates to it. A request answers once, and only from the
browser and person that started it.

### People's tokens and refresh

Access tokens last 15 minutes. Codes and refresh tokens live in Postgres, keyed by the SHA-256 of
the secret, so every replica honours them and a restart signs nobody out. A refresh token is spent
on use and the next one returned; presenting a spent one again revokes the whole approval. An
approval dies after 14 days without a refresh or 90 days after it was granted. Every refresh reads
the person again: a deleted account, one whose stored credential the identity provider refused,
or a cluster username that was unlinked ends the approval, and a relink shows up in the next token.
Settings → MCP clients lists the live approvals by client. Each client has its own Sign out
(`DELETE /api/me/mcp-sign-ins?client_id=<id>`), and one button signs every client out
(`DELETE /api/me/mcp-sign-ins`). Expired codes and approvals are swept hourly.
