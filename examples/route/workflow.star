read = command(name = "read", run = "./read.sh", emits = ["ticket"])

gate = route(
    name = "gate",
    depends_on = [read],
    decider = model(min_confidence = 0.8),
    questions = {
        "bucket": choice(
            ask = "Which queue owns this ticket?",
            options = {
                "outage": "the product is down or unusable",
                "billing": "charges, invoices, refunds",
                "feature": "a request for something new",
            },
        ),
        "urgent": noul(
            ask = "Does the customer need a reply within the hour?",
            drop = ["no", "uncertain"],
        ),
    },
)

page = command(name = "page", run = "./act.sh page", when = gate.urgent)

oncall = command(name = "oncall", run = "./act.sh oncall", when = gate.bucket.any_of("outage"))

finance = command(name = "finance", run = "./act.sh finance", when = gate.bucket.any_of("billing"))

backlog = command(
    name = "backlog",
    run = "./act.sh backlog",
    when = gate.bucket.otherwise(),
)

filed = command(
    name = "filed",
    run = "./act.sh filed",
    depends_on = [oncall, finance, backlog],
    join = "passed",
)

workflow(type = "playbook", tasks = [read, gate, page, oncall, finance, backlog, filed], result = filed)
