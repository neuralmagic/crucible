smoke = agent(
    name = "smoke",
    prompt = prompt_file("prompts/smoke.md"),
)
workflow(type = "playbook", tasks = [smoke], result = smoke)
