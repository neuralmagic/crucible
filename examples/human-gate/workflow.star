plan = command(
    name = "plan",
    run = "./plan.sh",
    emits = {
        "model": "string",
        "gpus": "integer",
        "hours": "number",
        "est_usd": "number",
        "nodes": "list",
    },
    emits_files = ["out/cost.html", "out/nodes.csv", "out/plan.md"],
)

lint = command(name = "lint", run = "./act.sh lint", depends_on = [plan])

gate = route(
    name = "gate",
    decider = human(review = "reviews/gate.md.j2", timeout = "20m"),
    questions = {
        "launch": choice(
            ask = "Launch the training job?",
            options = {
                "approve": "start it on the cluster",
                "deny": "shelve the plan",
            },
        ),
        "nodes": pick(
            ask = "Which nodes should run it?",
            source = plan.nodes,
            multiple = True,
        ),
        "checks": choice(
            ask = "Which checks run alongside it?",
            options = {
                "smoke": "a one-step smoke run",
                "eval": "the held-out eval",
            },
            multiple = True,
            drop = ["uncertain"],
        ),
    },
)

launch = command(
    name = "launch",
    run = "./act.sh launch",
    when = gate.launch.any_of("approve"),
    over = each(gate.nodes, max = 8),
)

smoke = command(
    name = "smoke",
    run = "./act.sh smoke",
    when = gate.checks.any_of("smoke"),
)

held_out = command(
    name = "eval",
    run = "./act.sh eval",
    when = gate.checks.any_of("eval"),
)

shelve = command(
    name = "shelve",
    run = "./act.sh shelve",
    when = gate.launch.otherwise(),
)

done = command(
    name = "done",
    run = "./act.sh done",
    depends_on = [launch, smoke, held_out, shelve, lint],
    join = "passed",
)

workflow(
    type = "playbook",
    tasks = [plan, lint, gate, launch, smoke, held_out, shelve, done],
    result = done,
)
