params = {
    "repo": {
        "type": "string",
        "required": True,
        "doc": "GitHub repository to triage, owner/name",
        "pattern": "^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$",
    },
    "label": {
        "type": "string",
        "default": "",
        "doc": "only open issues carrying this label; empty means all open issues",
    },
    "limit": {
        "type": "string",
        "default": "6",
        "doc": "newest N open issues to triage",
    },
}

# Discovery is an agent because parameters reach prompts and nothing else: a command
# cannot be handed `repo` without laundering its provenance.
scan = skill(
    name = "scan",
    skill = "skills/scan-issues",
    args = {"repo": param("repo"), "label": param("label"), "limit": param("limit")},
    emits = ["issues"],
    emits_files = ["ISSUES.md"],
)

# One isolated instance per discovered issue, keyed by issue number.
triage = skill(
    name = "triage",
    skill = "skills/triage-issue",
    args = {"repo": param("repo")},
    depends_on = [scan],
    over = scan.issues,
    max_fanout = 12,
    isolated = True,
    required = False,
    emits = ["classification", "severity", "confidence"],
    emits_files = ["TRIAGE.md"],
)

# Deterministic and free: the report is assembled from what the passing instances
# captured, not from an agent's memory of what it did.
roundup = command(
    name = "roundup",
    run = "python3 roundup.py",
    depends_on = [scan, triage],
    join = "passed",
    emits = ["triaged"],
    emits_files = ["REPORT.md"],
)

workflow(type = "playbook", tasks = [scan, triage, roundup], result = roundup)
