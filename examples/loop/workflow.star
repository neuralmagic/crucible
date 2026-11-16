# A bounded loop: `turn` and `score` run in rounds while `score.go` is true, for at most five
# rounds. Each round resumes the `work` session and reads the round before under `loop`.

turn = agent(
    name = "turn",
    prompt = "Take one more step toward the goal. `loop.previous.score` is how the last one scored.",
    session = "work",
    emits = ["summary"],
)

score = command(
    name = "score",
    run = "./score.sh",
    depends_on = [turn],
    emits = {"go": "boolean", "points": "integer"},
)

report = command(
    name = "report",
    run = "./report.sh",
    depends_on = [score],
)

workflow(type = "playbook", tasks = [
    loop(body = [turn, score], again = score.go, rounds = 5),
    report,
])
