load("@crucible//calls", "artifact_outputs", "github_workflow", "playbook")

prep = command(
    name = "prep",
    run = "python3 prep.py",
    emits = {"label": "string", "config": "object"},
    emits_files = ["NOTES.md"],
)

child = call(
    name = "child",
    target = playbook("draft:calls-child"),
    params = {
        "label": prep.label,
        "config": prep.config,
        "notes": prep.files["NOTES.md"],
    },
    depends_on = [prep],
    budget = budget(cost = 0.5, wall_clock = "10m"),
)

ci = call(
    name = "ci",
    target = github_workflow(
        repo = "OWNER/REPO",
        workflow = ".github/workflows/crucible-call-demo.yml",
        ref = "crucible-call-demo",
        outputs = artifact_outputs(
            artifact = "crucible-result",
            fields = {
                "summary": "string",
                "flavor": "string",
                "loud": "boolean",
                "legs": "integer",
                "payload_keys": "list",
            },
            files = ["notes.md"],
        ),
    ),
    params = {
        "message": prep.label,
        "payload": prep.config,
        "flavor": "thorough",
        "loud": False,
    },
    depends_on = [prep],
    budget = budget(wall_clock = "15m"),
)

summary = command(
    name = "summary",
    run = "python3 summary.py",
    depends_on = [child, ci],
    emits = {
        "label": "string",
        "child_words": "integer",
        "ci_summary": "string",
        "ci_legs": "integer",
        "ci_payload_keys": "list",
    },
    emits_files = ["SUMMARY.md"],
)

workflow(type = "playbook", tasks = [prep, child, ci, summary], result = summary)
