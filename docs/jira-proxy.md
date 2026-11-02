# JIRA tools (mediated)

The broker contains a native JIRA Cloud client (the shared `ujira` crate). It gives the
sandboxed agent the **read and comment** functions of that client over the MCP connection of
the broker. This is the ADR-0002 mediation pattern applied to JIRA: the agent gets JIRA tools
but never has an Atlassian credential. There is no upstream MCP child process. The broker
calls the JIRA REST API directly, on the server side.

## What the agent sees

The broker has three JIRA tools. Their names have the broker name as prefix, for example `mcp__epp-broker__jira_search`:

| Tool | Args | Returns |
| --- | --- | --- |
| `jira_search` | `jql`, `limit` (default 25) | compact rows: `{key, summary, type, status, labels}` |
| `jira_get_issue` | `issue_key`, `raw` (default false) | a short record that uses few tokens. Set `raw=true` for the full issue. |
| `jira_add_comment` | `issue_key`, `comment` | `{id, url}` |

`jira_add_comment` is the **only** write. The agent has no tool to create, transition, edit,
or delete. When the broker creates the client, it limits the client to read and comment
(`crucible-broker/src/jira.rs`). The code enforces the scope. It does not depend on the agent.
If JIRA is not configured, the tools return `{"status":"disabled"}`, as `build_epp` and
`profile` do when their feature is disabled. The tools are always present.

**Context hint.** The deployment can name its projects with `BROKER_JIRA_PROJECTS`
(comma-separated, for example `PROJ,PROJ2`). The `jira_search` tool description then tells the
agent to limit JQL to those boards and not to search the whole instance.
`BROKER_DESC_JIRA_JQL_EXAMPLE` and `BROKER_DESC_JIRA_KEY_EXAMPLE` set the inline examples in
the descriptions. If they are not set, the descriptions are generic.

## Trust boundary

The Atlassian credentials are in the environment of the **broker pod**. They are **never** in
`[agent.env]`, so the sandbox never sees a token, as for the build and deploy credentials. The
agent can reach only the broker endpoint, which is already on the egress allowlist. The REST
calls to Atlassian Cloud use the egress of the loop pod, not of the sandbox, so the sandbox
egress does not change.

## Enable it (broker pod environment)

| Env | Meaning |
| --- | --- |
| `JIRA_URL` | The Cloud site, e.g. `https://your-org.atlassian.net`. |
| `JIRA_USERNAME` | The account email (Cloud basic auth is `JIRA_USERNAME:JIRA_API_TOKEN`). |
| `JIRA_API_TOKEN` | The API token, from a Secret. |

If all three are set, the `jira_*` tools work. If one is missing, they answer `disabled`. The
broker sets the access level to read and comment, whatever the environment or the
configuration of the shared crate allows.

