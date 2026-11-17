idle = command(
    name = "idle",
    run = "true",
)

workflow(type = "playbook", tasks = [idle])
