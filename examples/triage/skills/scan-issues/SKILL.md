---
name: scan-issues
description: List the newest open issues of a GitHub repository and emit them as a fan-out list.
---

# Scan Open Issues

You are given a repository (`repo`, as `owner/name`), an optional `label`, and a `limit`.
Produce the list of open issues a later phase will triage one by one.

## Step 1: Fetch the issue list

Prefer the unauthenticated public API; it needs no token for public repositories:

```sh
curl -sf "https://api.github.com/repos/<repo>/issues?state=open&per_page=<limit>&sort=created&direction=desc[&labels=<label>]"
```

Only add the `labels` parameter when `label` is non-empty. If `gh` is available and
authenticated (`gh auth status` succeeds), you may use
`gh issue list -R <repo> --state open --limit <limit> --json number,title,labels,createdAt`
instead; it is equivalent.

Rules:

- The issues endpoint returns pull requests too. **Drop every item that has a
  `pull_request` key.** Fetch a second page if the filter leaves you short of `limit`.
- Keep at most `limit` issues, newest first.
- If the repository does not exist or the API refuses, fail loudly: exit without writing
  the result file. Do not invent issues.

## Step 2: Write your output

You are running unattended inside a crucible playbook. There is nobody to ask, and
nothing you print is read. Two files are your entire output:

1. `ISSUES.md` in the workspace root — one table row per issue: number, title, labels,
   created date. A heading naming the repository and the filter, nothing else.
2. `PLAN_TASK_RESULT.json` in the workspace root — exactly:

   {"issues": ["<number>", "<number>", ...]}

   Issue numbers as strings, newest first, at most `limit` of them. Each string becomes
   one isolated triage instance, so a wrong entry burns a real turn.

Write both before you finish. Do not ask for approval and do not stop to confirm
anything.
