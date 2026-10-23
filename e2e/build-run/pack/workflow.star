build_run = agent(
    name = "build_run",
    prompt = prompt_file("prompts/build_run.md"),
)
workflow(type = "playbook", tasks = [build_run], result = build_run)
