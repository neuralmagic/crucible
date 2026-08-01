# Scope-time authoring source for the tasks inserted after the engine's proposer and
# before its frozen apply/measure/decide stages.

reviewers = [
    agent(
        name = "review-correctness",
        prompt = prompt_file("prompts/correctness.md"),
        model = "claude-opus-4-6",
        effort = "high",
        isolated = True,
    ),
    agent(
        name = "review-copy",
        prompt = prompt_file("prompts/copy.md"),
        model = "claude-sonnet-5",
        required = False,
        isolated = True,
    ),
]

workflow(reviewers + [
    command(
        name = "gate",
        run = "./join_gate.sh",
        depends_on = deps(reviewers),
        join = "passed",
    ),
])
