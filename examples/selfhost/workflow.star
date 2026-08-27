# The autoresearch loop as a work graph, so the controller can register and launch this pack the
# same way it launches a playbook. Same shape as examples/counter: propose -> apply -> gate ->
# grade -> decide. The gate is measure.sh (tests + clippy + the hash-guarded bench).

solver = session(name = "solver")
candidate = propose(name = "propose", session = solver)
applied = apply(name = "apply", depends_on = [candidate])

score = evaluate(
    name = "score",
    run = "./measure.sh",
    depends_on = [applied],
    isolated = True,
    emits = ["score", "pass"],
)
measurement = grade(
    name = "grade",
    evidence = [score],
    score = score,
)
decision = decide(name = "decide", measurement = measurement)

workflow(
    type = "autoresearch",
    tasks = [candidate, applied, score, measurement, decision],
    result = decision,
)
