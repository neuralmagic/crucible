# The codex harness

`crucible --harness codex` (or `[agent].harness = "codex"`) runs the turn with the OpenAI Codex
CLI instead of Claude Code. Everything after the decoder is the same. The turn emits
`AgentEvent` NDJSON, and keep/discard reads the same `Result`.

```toml
[agent]
harness = "codex"

[agent.codex]
# The shared `[agent].model` names a Claude model, so a codex domain overrides it here.
model = "gpt-5.6-sol"
# auto (default), api, or chatgpt
auth = "api"
# Which of the deployment's OpenAI keys to use; unset = the unnamed default key.
api_key = "WORK"
```

Differences from a claude turn:

- **No session resume.** Crucible does not use `codex exec resume`. The second turn of a
  logical session fails. It does not start a new session.
- **No OTEL.** `codex exec` exports no metrics. The cost is an estimate from the pricing table
  and the token usage in the live `--json` stream, not an `otel_summary`.
- **Egress.** Codex adds `chatgpt.com`, `auth.openai.com`, `api.openai.com`, and
  `ab.chatgpt.com` to the sandbox allowlist. These hosts apply only to codex turns. The
  allowlist of a claude turn does not change.

## Auth selection

Crucible supports both Codex login methods. `[agent.codex].auth` selects the method:

- `auto` (default) uses the selected API key if it is not empty. If it is empty, it uses
  ChatGPT OAuth.
- `api` requires the selected API key. It does not use OAuth.
- `chatgpt` uses the OAuth flow, also when API keys are present.

`[agent.codex].api_key` names one of the OpenAI keys of the deployment (uppercase letters,
digits, and underscore). If it is not set, the unnamed default key is used. The manifest
names a key, not the variable that holds it. The host resolves `api_key = "WORK"` to
`OPENAI_API_KEY_WORK`, and an unset `api_key` to `OPENAI_API_KEY`. A manifest cannot reach
other host credentials. An openshell provider that can deliver the key can later replace this
resolution, and the manifest does not change.

A Kubernetes deploy profile supplies keys under those names. You can rotate each key
separately:

```toml
[[secret_env]]
name = "OPENAI_API_KEY_WORK"
secret = "crucible-openai-work"
key = "OPENAI_API_KEY"

[[secret_env]]
name = "OPENAI_API_KEY_PERSONAL"
secret = "crucible-openai-personal"
key = "OPENAI_API_KEY"
```

To select a key for new turns, change `api_key` in the manifest, or select a deployment
profile with that manifest. Use dedicated project-scoped keys, and rotate their Kubernetes
Secrets separately. The engine does not export the selected key into the sandbox environment,
but Codex can read it from its seeded auth file.

## ChatGPT OAuth: `CODEX_CREDENTIALS` and the single-refresher rule

Codex authenticates to the ChatGPT backend with a personal subscription, not Vertex. The
credential is an OAuth pair that `codex login` creates on a host with a browser. It is stored
at `~/.codex/auth.json`.

Setup:

1. Run `codex login` on your machine. Then make sure that `~/.codex/auth.json` exists.
2. Give its **exact contents** to the loop process as the `CODEX_CREDENTIALS` environment
   variable. In a cluster, use a `secretKeyRef`, as for `GCLOUD_CREDENTIALS`:

   ```sh
   oc create secret generic codex-auth \
     --from-file=auth.json="$HOME/.codex/auth.json"
   ```

3. Reference it from the `[[secret_env]]` of the deploy profile.

At the start of each turn, the loop process does the OAuth `refresh_token` grant against
`https://auth.openai.com/oauth/token`. It writes the result into the sandbox as
`$CODEX_HOME/auth.json`: the access token, the account id, the id token, and a placeholder
refresh token. An environment variable from a provider cannot hold this value. The sandbox
sees only an `openshell:resolve:env:` placeholder that the L7 egress proxy must resolve, and
codex connects to the backend over a WebSocket through an L4 tunnel. All four `tokens` fields
must be present. If one is missing, codex ignores the object, runs without authentication, and
gets repeated 401 errors.

**Single-refresher rule: the refresh token stays in the loop process.** Only the loop process
does the grant, so the copy of `auth.json` on the host and the copy in a sandbox cannot
compete for a rotation. OpenAI rotates the refresh token on each grant, and an old copy is no
longer valid. As a result, a sandbox has a fixed short-lived access token. Codex reads the
seeded `auth.json` at exec, so if a turn lasts longer than the access token, the turn fails
with an error. It does not authenticate again.

The loop process writes the rotated refresh token of each grant to
`$HOME/.config/crucible/codex-credentials.json`. On the next grant, that file has priority
over `CODEX_CREDENTIALS`. The environment secret is used only for the first grant. The first
rotation makes it invalid, so a new process with a new `$HOME` needs a new secret. Run
`codex login` again and replace the secret before each pod restart. Two loop processes must
not share one credential. Each needs its own `codex login`.

Also, a refresh token that is not used becomes invalid after about one week. If the grant
fails with `refresh_token_expired`, run `codex login` again on the host, replace the secret,
and delete the state file if the process keeps a home directory.

In `auto` mode, Crucible uses OAuth if the selected API key is absent or empty. `chatgpt`
selects OAuth explicitly.
