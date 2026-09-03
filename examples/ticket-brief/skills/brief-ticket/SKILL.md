---
name: brief-ticket
description: Write a one-page brief for one ticket key, from the key and the launcher's note alone.
---

# Brief Ticket

You are given a `jira_key` (`PROJ-123`) and a `note` (possibly empty). Produce a short brief
for whoever picks the ticket up next.

## Rules

- Do not fetch anything from the network. You have no tracker access here; the key and the
  note are the whole input. Say what you would check first and why, not what the ticket says.
- Keep it to one page: a heading with the key, three bullets of "first things to look at",
  and one line quoting the note if there is one.

## Write your output

You are running unattended inside a crucible playbook. There is nobody to ask, and nothing
you print is read. Two files are your entire output:

1. `BRIEF.md` in the workspace root, the brief described above.
2. `PLAN_TASK_RESULT.json` in the workspace root, exactly:

   {"jira_key": "<the key you were given>", "summary": "<one line, under 120 characters>"}

Write both before you finish. Do not ask for approval and do not stop to confirm anything.
