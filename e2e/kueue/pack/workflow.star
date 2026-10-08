submit = agent(
    name = "submit",
    prompt = prompt_file("prompts/submit.md"),
)
workflow(type = "playbook", tasks = [submit], result = submit)
