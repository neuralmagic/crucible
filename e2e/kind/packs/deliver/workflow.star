check = command(name = "check", run = "sh check.sh")

workflow(type = "playbook", tasks = [check], result = check)
