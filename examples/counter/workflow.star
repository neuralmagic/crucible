# The smallest complete autoresearch workflow. Binding the proposer to `solver` makes repeated
# graph-loop iterations resume one logical agent conversation; omitting `session` restores the
# historical fresh-turn behavior.

candidate = propose(name = "propose", session = "solver")
applied = apply(name = "apply", depends_on = [candidate])

# Both checks see the same applied candidate and are ready together, so isolation lets the
# executor run them concurrently. `score` is the primary reading; `shape` is supporting evidence.
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
