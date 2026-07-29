# JIRA tools (mediated)

The broker embeds a native JIRA Cloud client (the shared `jira-mcp` crate) and exposes a
**read+comment** slice of it to the sandboxed agent over the broker's existing MCP wire. It's the
ADR-0002 mediation pattern, JIRA edition: the agent gets JIRA tools without ever holding an
Atlassian credential. There is no upstream MCP child process; the broker calls JIRA's REST API
directly, server-side.

## What the agent sees

Three tools on the broker wire (prefixed by the broker name, e.g. `mcp__epp-broker__jira_search`):

| Tool | Args | Returns |
| --- | --- | --- |
| `jira_search` | `jql`, `limit` (default 25) | compact rows: `{key, summary, type, status, labels}` |
| `jira_get_issue` | `issue_key`, `raw` (default false) | a curated, token-frugal record; `raw=true` for the full issue |
| `jira_add_comment` | `issue_key`, `comment` | `{id, url}` |

`jira_add_comment` is the **only** write. The agent has no tool for create/transition/edit/delete,
and the client itself is pinned to the read+comment ceiling at construction
(`crucible-broker/src/jira.rs`), so scope is enforced by construction, never by trusting the
agent. When JIRA isn't configured the tools return `{"status":"disabled"}` (same as
`build_epp`/`profile` when their feature is off), so they never vanish.

**Context hint.** The deployment can name its projects of interest with `BROKER_JIRA_PROJECTS`
(comma-separated, e.g. `PROJ,PROJ2`); the `jira_search` tool description then tells the agent to
scope JQL to those boards instead of searching the whole instance. `BROKER_DESC_JIRA_JQL_EXAMPLE`
and `BROKER_DESC_JIRA_KEY_EXAMPLE` flavor the inline examples the descriptions show. Unset, the
descriptions stay fully generic.

## Trust boundary

The Atlassian credentials live in the **broker pod's** env. They are **never** in `[agent.env]`, so
the sandbox never sees a token, exactly like the build/deploy creds. The agent reaches only the
broker endpoint (already egress-allowlisted); the REST calls to Atlassian Cloud ride the loop pod's
egress, not the sandbox's, so no sandbox egress change is needed.

## Enabling it (broker-pod env)

| Env | Meaning |
| --- | --- |
| `JIRA_URL` | The Cloud site, e.g. `https://your-org.atlassian.net`. |
| `JIRA_USERNAME` | The account email (Cloud basic auth is `JIRA_USERNAME:JIRA_API_TOKEN`). |
| `JIRA_API_TOKEN` | The API token, from a Secret. |

All three set = the `jira_*` tools go live; anything missing = they answer `disabled`. The broker
overrides any configured access level to read+comment regardless of what the env or the shared
crate's config would allow.

## Smoke-test your real instance

The `jira_smoke` example drives the exact client the broker uses (same read+comment ceiling, same
compact renderings) against a live Atlassian Cloud instance. Configure it the same way the broker
pod is configured, all through the environment (creds never land on a command line):

```bash
export JIRA_URL=https://your-org.atlassian.net
export JIRA_USERNAME=you@your-org.com
export JIRA_API_TOKEN="$(cat ~/.jiratoken)"

# read-only: a JQL search
cargo run -p crucible-broker --example jira_smoke -- 'project = PROJ ORDER BY created DESC'

# pass '' for the JQL plus an issue key and body to exercise get_issue and post one comment (the write path)
cargo run -p crucible-broker --example jira_smoke -- '' PROJ-123 'hello from the mediated broker'
```
