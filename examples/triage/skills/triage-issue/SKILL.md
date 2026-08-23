---
name: triage-issue
description: Triage one GitHub issue - classify it, judge severity, and draft a maintainer response.
---

# Triage One Issue

You are given a repository (`repo`, as `owner/name`). Your task inputs carry `item`: the
number of the single issue you triage. Other instances handle the other issues; stay on
yours.

## Step 1: Fetch the issue

```sh
curl -sf "https://api.github.com/repos/<repo>/issues/<item>"
curl -sf "https://api.github.com/repos/<repo>/issues/<item>/comments"
```

(`gh issue view <item> -R <repo> --json title,body,labels,comments` is equivalent when
`gh` is authenticated.) If the issue cannot be fetched, fail loudly: exit without writing
the result file.

## Step 2: Triage

Work from the report and its comments alone; you have no checkout and clone nothing.

- **Classify**: exactly one of `bug`, `feature`, `question`, `duplicate`, `needs-info`.
- **Severity**: exactly one of `critical`, `high`, `medium`, `low`. A crash, data loss,
  or wrong-result report outranks an inconvenience; a feature request is `low` unless it
  blocks a stated use case.
- **Evidence**: quote the sentences that decided the classification. If the report lacks
  what a maintainer would need (version, reproduction, expected vs actual), list what is
  missing and classify `needs-info`.
- **Draft a response**: 2-6 sentences a maintainer could post as-is. Ask for the missing
  pieces, point at the likely area, or say what would be accepted. Plain and specific; no
  boilerplate thanks, no promises of a fix.

## Step 3: Write your output

You are running unattended inside a crucible playbook. There is nobody to ask, and
nothing you print is read. Two files are your entire output:

1. `TRIAGE.md` in the workspace root:

   ```markdown
   # <number>: <title>

   ## Classification
   <classification>, severity <severity>, confidence <confidence>

   ## Evidence
   ...

   ## Missing information
   ... (or "none")

   ## Draft response
   ...
   ```

2. `PLAN_TASK_RESULT.json` in the workspace root — exactly:

   {"classification": "<bug|feature|question|duplicate|needs-info>",
    "severity": "<critical|high|medium|low>",
    "confidence": "<high|medium|low>"}

Write both before you finish. Do not ask for approval and do not stop to confirm
anything.
