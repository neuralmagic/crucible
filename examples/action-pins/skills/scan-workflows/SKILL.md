---
name: scan-workflows
description: Capture the exact GitHub Actions workflow files in a repository for deterministic pin checks.
---

# Scan Workflows

You are given `repo` (`owner/name`) and `ref`. Fetch every YAML workflow file under
`.github/workflows/` at that ref, capture its exact text, and list its repo-relative path.

## Step 1: Fetch the workflow paths

Prefer the authenticated GitHub CLI when it works:

```sh
gh api "repos/<repo>/git/trees/<ref>?recursive=1"
```

Otherwise use:

```sh
curl -fsSL "https://api.github.com/repos/<repo>/git/trees/<ref>?recursive=1"
```

Keep entries whose type is `blob`, whose path starts with `.github/workflows/`, and whose
name ends in `.yml` or `.yaml`. If the repository or ref cannot be fetched, fail without
writing the result file. Do not invent workflow paths. A truncated tree is still usable
for the paths received, but note that limitation in `SCAN.md`.

## Step 2: Capture exact contents

For each selected path, fetch the file at the requested ref. Use `gh api
"repos/<repo>/contents/<path>?ref=<ref>"` and decode its base64 `content`, or fetch the
raw file from `https://raw.githubusercontent.com/<repo>/<ref>/<path>`. Preserve the file
text exactly, including line endings as returned, and do not parse or rewrite YAML.

## Step 3: Write output

You are running unattended. Write these files in the workspace root:

1. `WORKFLOWS.json`, exactly this shape:

   ```json
   {
     "repo": "owner/name",
     "ref": "main",
     "workflows": [
       {"path": ".github/workflows/ci.yml", "content": "...exact text..."}
     ]
   }
   ```

   The list is sorted by path. An empty repository of workflows is valid.
2. `SCAN.md`, naming the repository and ref, listing every captured path, and stating if
   the API tree was truncated.
3. `PLAN_TASK_RESULT.json`, exactly:

   ```json
   {"workflows": [".github/workflows/ci.yml"]}
   ```

   The paths must match `WORKFLOWS.json` and must be repo-relative strings. Do not print
   credentials or put them in any output file.
