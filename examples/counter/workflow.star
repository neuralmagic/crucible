# The smallest complete autoresearch workflow. Binding the proposer to the `solver` session
# makes repeated loop iterations resume one logical agent conversation; omitting `session`
# restores the historical fresh-turn behavior.

solver = session(name = "solver")
candidate = propose(name = "propose", session = solver)
applied = apply(name = "apply", depends_on = [candidate])

# Parallel isolated checks; `score` is the primary reading and declares its output contract:
# a passing run missing a declared field is a measured failure at the source.
shape = evaluate(
    name = "shape",
    run = "test -s value.txt && echo '{\"pass\": true, \"score\": 1}'",
    depends_on = [applied],
    isolated = True,
)
score = evaluate(
    name = "score",
    run = "./measure.nu",
    depends_on = [applied],
    isolated = True,
    emits = ["score", "pass"],
)
measurement = grade(
    name = "grade",
    evidence = [shape, score],
    score = score,
)
decision = decide(name = "decide", measurement = measurement)

workflow(
    type = "autoresearch",
    tasks = [candidate, applied, shape, score, measurement, decision],
    result = decision,
)
