params = {
    "repo": {"type": "string", "required": True, "doc": "owner/name the run's GitHub App is installed on"},
    "base": {"type": "string", "default": "main", "doc": "the branch smoke branches start from"},
}

task_push = command(
    name = "task_push",
    run = "python3 smoke.py push --who task --out evidence/task.json",
    emits_files = ["evidence/task.json"],
    timeout = "10m",
)

agent_push = agent(
    name = "agent_push",
    prompt = prompt_file("prompt.md") + "\n\nrepo: " + param("repo") + "\nbase: " + param("base") + "\n",
    emits_files = ["evidence/agent.json"],
    timeout = "20m",
)

check = evaluate(
    name = "check",
    run = "python3 smoke.py check evidence/task.json evidence/agent.json",
    depends_on = [task_push, agent_push],
    emits = {"prs": "list"},
)

workflow(type = "playbook", tasks = [task_push, agent_push, check], result = check)
