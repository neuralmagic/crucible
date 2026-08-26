# The codex harness

`crucible --harness codex` (or `[agent].harness = "codex"`) runs the turn with OpenAI's Codex CLI
instead of Claude Code. Everything downstream of the decoder is unchanged: the turn still emits
`AgentEvent` NDJSON, and keep/discard still reads the same `Result`.

```toml
[agent]
harness = "codex"

[agent.codex]
# The shared `[agent].model` names a Claude model, so a codex domain overrides it here.
model = "gpt-5.6-sol"
# auto (default), api, or chatgpt
auth = "api"
# Select among separately injected Kubernetes secrets without renaming them.
api_key_env = "OPENAI_API_KEY_WORK"
```

What differs from a claude turn:

- **No session resume.** `codex exec resume` is not wired; a logical session's second turn errors
  rather than silently starting fresh.
- **No OTEL.** `codex exec` exports no metrics, so cost is the pricing-table estimate over the
  token usage the live `--json` stream reports, not an `otel_summary`.
- **Egress.** The codex arm adds `chatgpt.com`, `auth.openai.com`, `api.openai.com`, and
  `ab.chatgpt.com` to the sandbox allowlist. Those hosts are per-harness: a claude turn's
  allowlist is byte-identical to what it was before codex existed.

## Auth selection

Crucible supports both Codex login methods. `[agent.codex].auth` controls selection:

- `auto` (default) uses the selected non-empty API key and otherwise falls back to ChatGPT OAuth.
- `api` requires the selected API key and never silently falls back.
- `chatgpt` uses the OAuth flow even when API keys are present.

`[agent.codex].api_key_env` selects the key by environment-variable name and defaults to
`OPENAI_API_KEY`. A Kubernetes deploy profile can inject independently rotatable keys:

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

Switch `api_key_env` in the manifest (or select a deployment profile carrying that manifest) to
choose a key for newly created turns. Use dedicated project-scoped keys and rotate their
Kubernetes Secrets independently. The selected key is not exported into the sandbox environment,
but Codex can read it from its seeded auth file.

## ChatGPT OAuth: `CODEX_CREDENTIALS` and the single-refresher rule

Codex authenticates against the ChatGPT backend with a personal subscription, not Vertex. The
credential is an OAuth pair produced by `codex login` on a host with a browser, stored at
`~/.codex/auth.json`.

Setup:

1. `codex login` on your machine, then confirm `~/.codex/auth.json` exists.
2. Ship its **verbatim contents** to the loop process as the `CODEX_CREDENTIALS` env var. In
   cluster that is a `secretKeyRef`, exactly how `GCLOUD_CREDENTIALS` is delivered:

   ```sh
   oc create secret generic codex-auth \
     --from-file=auth.json="$HOME/.codex/auth.json"
   ```

3. Reference it from the deploy profile's `[[secret_env]]`.

At the top of every turn the loop process performs the OAuth `refresh_token` grant against
`https://auth.openai.com/oauth/token` and seeds the result into the sandbox as
`$CODEX_HOME/auth.json`: access token, account id, id token, and a placeholder refresh token.
Provider-delivered env cannot carry it, because the sandbox sees only an `openshell:resolve:env:`
placeholder that the L7 egress proxy would have to resolve, and codex reaches the backend over a
WebSocket through an L4 tunnel. All four `tokens` fields have to be present, or codex drops the
object and runs unauthenticated into a 401 loop.

**The single-refresher rule: the refresh token never leaves the loop process.** Exactly one thing
performs the grant, so there is no rotation race between the host's copy and a sandbox's copy of
`auth.json` (OpenAI rotates the refresh token on each grant, and a stale copy is dead). The
consequence is that a sandbox holds a fixed short-lived access token: the seeded `auth.json` is
read at exec, so a turn that outlives the access token fails loudly rather than silently
reauthenticating. That is accepted for now.

Each grant's rotated refresh token is persisted to
`$HOME/.config/crucible/codex-credentials.json`, and that file takes precedence over
`CODEX_CREDENTIALS` on the next mint. The env secret is seed material for the first mint only:
the first rotation spends it, so a fresh process with a fresh `$HOME` needs a freshly minted
secret (re-run `codex login` and replace it as part of any pod restart). Two loop processes must
never share one credential; each needs its own `codex login`.

Independently, an unused refresh token goes stale after roughly a week. When mints fail the grant
with `refresh_token_expired`, re-run `codex login` on the host, replace the secret, and delete
the state file if the process persists a home directory.

In `auto` mode this OAuth machinery remains the fallback when the selected API key is absent or
empty; `chatgpt` selects it explicitly.
