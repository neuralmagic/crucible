# action-pins

Finds GitHub Actions workflow steps whose `uses:` reference is not a full 40-character
commit SHA, reports the exact workflow lines, and prepares a patch that replaces resolvable
tags and branches with their commit SHAs. With `propose = yes`, the last phase can push that
patch and open a pull request.

| param | default | meaning |
| --- | --- | --- |
| `repo` | required | GitHub repository, `owner/name` |
| `ref` | `main` | branch or tag to inspect |
| `propose` | `no` | `yes` pushes a branch and opens a pull request; anything else stops at the patch |

The first agent turn captures the workflow files exactly. Each workflow is checked in an
isolated deterministic task, so findings carry the path, line, original `uses:` value, and
the ref that needs pinning. The final agent resolves explicit refs through GitHub, edits only
those located lines, and leaves expressions or refs it cannot resolve for a human.

The report and proposal are available as captured files at the end of a run:
`REPORT.md`, `FINDINGS.json`, `PROPOSAL.md`, and `fix.patch`.

Drafted by an agent from the `draft-a-playbook` skill and `crux` alone, as a test of whether that
skill is sufficient on its own. It compiles (`crux draft-preview action-pins`) and has not been
launched, so treat the shapes as the worked part and the behaviour as unproven.
