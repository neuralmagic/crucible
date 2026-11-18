gpu = command(name = "gpu", run = "python3 gpu.py", timeout = "40m")

workflow(type = "playbook", tasks = [gpu], result = gpu)
