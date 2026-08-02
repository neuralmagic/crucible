# The smallest complete autoresearch workflow. Binding the proposer to `solver` makes repeated
# graph-loop iterations resume one logical agent conversation; omitting `session` restores the
# historical fresh-turn behavior.

candidate = propose(name = "propose", session = "solver")
applied = apply(name = "apply", depends_on = [candidate])
score = measure(name = "measure", depends_on = [applied])
decision = decide(name = "decide", measurement = score)

workflow(
    type = "autoresearch",
    tasks = [candidate, applied, score, decision],
    result = decision,
)
