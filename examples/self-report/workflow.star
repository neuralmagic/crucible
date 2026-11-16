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

explain = agent(
    name = "explain",
    prompt = prompt_file("prompts/explain.md"),
    depends_on = [go, python],
    emits_files = ["report.view.js", "view.ts"],
    timeout = "25m",
)

workflow(type = "playbook", tasks = [go, python, explain])
