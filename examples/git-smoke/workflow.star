params = {
    "repo": {"type": "string", "required": True, "doc": "owner/name the run's GitHub App is installed on"},
    "base": {"type": "string", "default": "main", "doc": "the branch smoke branches start from"},
    "outside_public": {"type": "string", "default": "neuralmagic/crucible", "doc": "a public repository outside the bound, which the run must not push to"},
    "outside_private": {"type": "string", "default": "", "doc": "a private repository the App reaches outside the bound, which the run must not read; empty skips the check"},
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

deny = evaluate(
    name = "deny",
    run = "python3 smoke.py deny",
    depends_on = [task_push, agent_push],
)

passthrough = evaluate(
    name = "passthrough",
    run = "python3 smoke.py passthrough",
)

check = evaluate(
    name = "check",
    run = "python3 smoke.py check evidence/task.json evidence/agent.json",
    depends_on = [task_push, agent_push, deny, passthrough],
    emits = {"prs": "list"},
)

workflow(type = "playbook", tasks = [task_push, agent_push, deny, passthrough, check], result = check)
