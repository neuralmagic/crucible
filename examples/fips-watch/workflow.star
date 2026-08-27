params = {
    "downstream_repo": {
        "type": "string",
        "default": "opendatahub-io/modelexpress",
        "doc": "the fork that inherits this code on the next sync, named in the tracking issue",
    },
}

scan = command(
    name = "scan",
    run = "python3 scan.py",
    emits = ["variants"],
    emits_files = ["VARIANTS.md"],
)

probe = command(
    name = "probe",
    run = "python3 probe.py",
    depends_on = [scan],
    over = scan.variants,
    max_fanout = 12,
    isolated = True,
    required = False,
    emits = ["status", "blockers"],
    emits_files = ["PROBE.md"],
)

select = command(
    name = "select",
    run = "python3 pick_dirty.py",
    depends_on = [probe],
    join = "passed",
    emits = ["dirty"],
    emits_files = ["DIRTY.md"],
)

triage = skill(
    name = "triage",
    skill = "skills/triage-fips",
    args = {"downstream_repo": param("downstream_repo")},
    depends_on = [select],
    over = select.dirty,
    max_fanout = 6,
    isolated = True,
    required = False,
    emits = ["blocker", "root_cause", "confidence"],
    emits_files = ["TRIAGE.md", "ISSUE.json"],
)

roundup = command(
    name = "roundup",
    run = "python3 roundup.py",
    depends_on = [scan, probe, select, triage],
    join = "passed",
    emits = ["revision", "clean", "dirty", "blockers"],
    emits_files = ["REPORT.md", "ISSUES.json"],
)

file_issues = command(
    name = "file",
    run = "python3 file_issues.py",
    depends_on = [roundup],
    join = "passed",
    required = False,
    emits = ["filed", "skipped"],
    emits_files = ["FILED.md"],
)

card = command(
    name = "card",
    run = "python3 card.py",
    depends_on = [roundup, file_issues],
    join = "passed",
    emits = [
        "verdict",
        "revision",
        "clean_variants",
        "dirty_variants",
        "crypto_blockers",
        "issues_filed",
        "issues_skipped",
    ],
)

publish_report = report(
    name = "publish-report",
    destination = {"kind": "slack"},
    template = "reports/slack.md.j2",
    result = card,
    required = True,
)

workflow(
    type = "playbook",
    tasks = [scan, probe, select, triage, roundup, file_issues, card, publish_report],
    result = roundup,
)
