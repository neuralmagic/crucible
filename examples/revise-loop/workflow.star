author = agent(
    name = "author",
    prompt = "Write PROBE.md, a probe that demonstrates the reported bug.",
    session = "author",
    emits_files = ["PROBE.md"],
)

review = command(
    name = "review",
    run = "./check.sh",
    depends_on = [author],
    emits_files = ["evidence/review.json"],
    revise = author,
    max_rounds = 3,
)

workflow(type = "playbook", tasks = [author, review])
