load("@crucible//calls", "gitlab_pipeline", "job_outputs")

prep = command(
    name = "prep",
    run = "python3 prep.py",
    emits = {"message": "string", "targets": "list"},
)

pipeline = call(
    name = "pipeline",
    target = gitlab_pipeline(
        host = "GITLAB_HOST",
        project = "GITLAB_PROJECT",
        ref = "main",
        outputs = job_outputs(
            job = "report",
            fields = {"message": "string", "count": "integer", "targets": "list"},
            files = ["out/notes.md"],
        ),
    ),
    params = {"message": prep.message, "targets": prep.targets, "count": 3},
    depends_on = [prep],
    budget = budget(wall_clock = "20m"),
)

report = command(
    name = "report",
    run = "python3 report.py",
    depends_on = [pipeline],
    emits = {"message": "string", "count": "integer", "notes_lines": "integer"},
    emits_files = ["REPORT.md"],
)

workflow(type = "playbook", tasks = [prep, pipeline, report], result = report)
