go = command(
    name = "triage-go",
    run = "./triage.sh go",
    emits_files = ["verdicts.json"],
)

python = command(
    name = "triage-python",
    run = "./triage.sh python",
    depends_on = [go],
    emits_files = ["verdicts.json"],
)

workflow(type = "playbook", tasks = [go, python])
