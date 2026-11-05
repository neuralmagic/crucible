Prove this run can write to GitHub through its `git` MCP server and nothing else.

You hold no GitHub credential and need none: the sandbox reaches the run's crucible-git at
`http://host.openshell.internal:8853`, and your requests to it are authenticated for you.

1. Clone `http://host.openshell.internal:8853/git/<repo>.git` (the repo below), at the base branch.
2. Create the branch `crucible-git-smoke/agent-<UTC timestamp, digits only>` from it, add one
   empty commit (`git commit --allow-empty -m "crucible-git smoke"`), and push it to `origin`.
3. Call the `git` MCP server's `open_pr` tool with that repo, the base branch, your branch as
   `head`, a short title, and a body naming this run. Leave `draft` unset: it must be a draft.
4. Write `evidence/agent.json`:
   `{"branch": "<branch>", "sha": "<pushed commit sha>", "pr": "<url open_pr returned>"}`.

Do not use `gh`, the GitHub API, or any token. If a step fails, write the error into
`evidence/agent.json` as `{"error": "..."}` and stop.
