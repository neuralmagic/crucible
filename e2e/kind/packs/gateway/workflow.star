turn = agent(name = "turn", prompt = "Boot the gateway.")

workflow(type = "playbook", tasks = [turn], result = turn)
