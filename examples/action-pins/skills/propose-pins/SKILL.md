---
name: propose-pins
description: Replace located GitHub Actions tags and branches with full commit SHAs and propose the patch as a pull request.
---

# Propose Pins

The previous phases produced `inputs/roundup/FINDINGS.json`. That file is your complete
scope. It contains `repo`, `ref`, and `unpinned` findings with `workflow`, `line`, `uses`,
`action`, and `ref`. Do not search for or change any workflow line not represented there.

## Step 1: Read and classify findings

For every finding whose `kind` is `tag-or-branch`, resolve the action ref to its full commit
SHA. The action repository is the first two path components of `action`; for example,
`actions/checkout` in `actions/checkout@v4`. Use the GitHub commits API, which resolves
branches, lightweight tags, and annotated tags:

```sh
gh api "repos/<action-owner>/<action-name>/commits/<ref>" --jq .sha
```

If authenticated `gh api` is unavailable, resolve the same branch/tag with
`git ls-remote` against the action repository, preferring a peeled `refs/tags/<ref>^{}`
entry for an annotated tag. The result must match exactly 40 hexadecimal characters. Do
not guess a SHA. Findings with `kind: dynamic`, an invalid action name, an unavailable ref,
or any other resolution failure stay unchanged and are carried into the human-attention
table.

## Step 2: Get the working tree

```sh
git clone --depth 1 --branch <ref> https://github.com/<repo>.git repo
```

Work inside `repo/`. Do not touch anything outside it except the output files in the
workspace root.

## Step 3: Apply only grounded substitutions

For each successfully resolved finding, open the named workflow and verify its line number
still contains the exact `uses` value from the finding. Replace only the ref after `@` with
the resolved full SHA, preserving indentation, quoting, comments, and all unrelated text.
If the line or exact value does not match, do not edit it; put that finding in the
human-attention table. A full SHA already present in the file is not a change.

## Step 4: Capture the patch

From the workspace root, stage only the cloned repository's grounded edits and write the
staged diff to `fix.patch`:

```sh
git -C repo add -A
git -C repo diff --cached > fix.patch
```

If there are no grounded edits, create an empty `fix.patch`.

## Step 5: Write output

Write `PROPOSAL.md` as the pull request body: one sentence summarizing the number of
workflow refs pinned, a table of every edit (workflow, line, before, after), and a table of
every finding left alone with the reason it needs a person. If the credential or a remote
operation later fails, add that fact to the proposal. Do not include process narration.

Write `PLAN_TASK_RESULT.json` exactly:

```json
{"proposed": "<pull request URL, or `patch only`, or `nothing to fix`>", "fixed": "<N>"}
```

`fixed` is the number of workflow references actually rewritten.

## Step 6: Open the pull request only when authorized

Only when `propose` is exactly `yes` and `fix.patch` is non-empty:

```sh
gh auth setup-git
git -C repo checkout -b action-pins/$(date +%Y%m%d-%H%M%S)
git -C repo -c user.email=crucible@local -c user.name=crucible commit -qam "ci: pin GitHub Actions to commit SHAs"
git -C repo push -u origin HEAD
gh pr create --repo <repo> --base <ref> --title "ci: pin GitHub Actions to commit SHAs" --body-file ../PROPOSAL.md
```

`gh` reads the registry-projected credential from `GH_TOKEN`. Never print it, write it to a
file, or put it in a remote URL. If `GH_TOKEN` is unset, or push/PR creation is refused,
do not retry elsewhere: set `proposed` to `patch only`, record the refusal in
`PROPOSAL.md`, and finish. If `propose` is anything else, do not push or open a PR; use
`patch only` when a patch exists, otherwise `nothing to fix`.
