probe = command(name = "probe", run = "python3 probe.py")
serve = command(name = "serve", run = "python3 serve.py", depends_on = [probe])

workflow(type = "playbook", tasks = [probe, serve], result = serve)
