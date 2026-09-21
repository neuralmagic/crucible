read = command(name = "read", run = "./read.sh", emits = ["ticket"])

gate = route(
    name = "gate",
    depends_on = [read],
    min_confidence = 0.8,
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

page = command(name = "page", run = "./act.sh page", depends_on = [gate], when = gate.urgent)

oncall = command(name = "oncall", run = "./act.sh oncall", depends_on = [gate], when = gate.bucket, answers = "outage")

finance = command(name = "finance", run = "./act.sh finance", depends_on = [gate], when = gate.bucket, answers = "billing")

backlog = command(
    name = "backlog",
    run = "./act.sh backlog",
    depends_on = [gate],
    when = gate.bucket,
    otherwise = True,
)

filed = command(
    name = "filed",
    run = "./act.sh filed",
    depends_on = [oncall, finance, backlog],
    join = "passed",
)

workflow(type = "playbook", tasks = [read, gate, page, oncall, finance, backlog, filed], result = filed)
