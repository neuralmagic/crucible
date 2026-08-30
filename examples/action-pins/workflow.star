params = {
    "repo": {
        "type": "string",
        "required": True,
        "doc": "GitHub repository whose workflow files to inspect, owner/name",
        "pattern": "^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$",
    },
    "ref": {
        "type": "string",
        "default": "main",
        "doc": "branch or tag containing the workflow files",
    },
    "propose": {
        "type": "string",
        "default": "no",
        "doc": "`yes` pushes the pinning branch and opens a pull request; anything else stops at the patch",
    },
}

# Parameters can reach prompts and nowhere else. The scan agent therefore fetches the
# workflow list and exact contents once, and emits paths for the deterministic fan-out.
scan = skill(
    name = "scan",
    skill = "skills/scan-workflows",
    args = {"repo": param("repo"), "ref": param("ref")},
    emits = ["workflows"],
    emits_files = ["SCAN.md", "WORKFLOWS.json"],
)

# Each workflow is checked independently from the captured content. FINDINGS.json carries
# locations and refs for the proposal; counts are only for the report summary.
check = command(
    name = "check",
    run = "python3 check_workflow.py",
    depends_on = [scan],
    over = scan.workflows,
    max_fanout = 64,
    isolated = True,
    required = False,
    emits = ["unpinned", "checked"],
    emits_files = ["CHECK.md", "FINDINGS.json"],
)

# Fold only passing checks into one deterministic report and proposal input.
roundup = command(
    name = "roundup",
    run = "python3 roundup.py",
    depends_on = [scan, check],
    join = "passed",
    emits = ["unpinned", "checked", "workflows"],
    emits_files = ["REPORT.md", "FINDINGS.json"],
)

propose = skill(
    name = "propose",
    skill = "skills/propose-pins",
    args = {"repo": param("repo"), "ref": param("ref"), "propose": param("propose")},
    depends_on = [roundup],
    emits = ["proposed", "fixed"],
    emits_files = ["PROPOSAL.md", "fix.patch"],
)

workflow(type = "playbook", tasks = [scan, check, roundup, propose], result = propose)
