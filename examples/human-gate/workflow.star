plan = command(
    name = "plan",
    run = "./plan.sh",
    emits = {"model": "string", "gpus": "integer", "hours": "number", "est_usd": "number"},
    emits_files = ["out/cost.html", "out/nodes.csv", "out/plan.md"],
)

lint = command(name = "lint", run = "./act.sh lint", depends_on = [plan])

gate = route(
    name = "gate",
    depends_on = [plan],
    human = True,
    review = "reviews/gate.md.j2",
    timeout = "20m",
    questions = {
        "launch": choice(
            ask = "Launch the training job?",
            options = {
                "approve": "start it on the cluster",
                "deny": "shelve the plan",
            },
        ),
    },
)

launch = command(
    name = "launch",
    run = "./act.sh launch",
    depends_on = [gate],
    when = gate.launch,
    answers = "approve",
)

shelve = command(
    name = "shelve",
    run = "./act.sh shelve",
    depends_on = [gate],
    when = gate.launch,
    otherwise = True,
)

done = command(
    name = "done",
    run = "./act.sh done",
    depends_on = [launch, shelve, lint],
    join = "passed",
)

workflow(type = "playbook", tasks = [plan, lint, gate, launch, shelve, done], result = done)
