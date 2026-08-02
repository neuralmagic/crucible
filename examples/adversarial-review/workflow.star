# The complete autoresearch iteration; its type enforces the engine lifecycle.

candidate = propose(name = "propose")

reviewers = [
    agent(
        name = "review-correctness",
        prompt = prompt_file("prompts/correctness.md"),
        model = "claude-opus-4-6",
        effort = "high",
        isolated = True,
        depends_on = [candidate],
    ),
    agent(
        name = "review-copy",
        prompt = prompt_file("prompts/copy.md"),
        model = "claude-sonnet-5",
        required = False,
        isolated = True,
        depends_on = [candidate],
    ),
]

gate = command(
    name = "gate",
    run = "./join_gate.sh",
    depends_on = deps(reviewers),
    join = "passed",
)

deployed = apply(name = "apply", depends_on = [gate])
score = measure(name = "measure", depends_on = [deployed])
decision = decide(name = "decide", measurement = score)

workflow(
    type = "autoresearch",
    tasks = [candidate] + reviewers + [gate, deployed, score, decision],
    result = decision,
)
