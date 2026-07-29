## The `goal.md` contract: frame the problem, NEVER the fix

This is the single most important thing you write, and the easiest to get wrong. A *different*
agent solves the issue against your gate later, autonomously — the whole point of the research is
measuring what that agent can figure out on its own. If your `goal.md` hands it the answer, you've
poisoned the experiment: you're no longer measuring autonomous solve capability, you're measuring
transcription. So `goal.md` frames the problem and **nothing about the solution**.

`goal.md` MUST contain, and ONLY contain:

- **The problem, behaviorally.** What is wrong, slow, or incorrect, described by how it *manifests*
  — the observable symptom, the wrong output, the systemic effect. Where it shows up in behavior,
  not where it lives in the source.
- **The measurement.** What the gate runs, what the score is, and what a better score means (the
  direction). The solver needs to know how it will be judged.
- **The acceptance threshold.** What "solved" means — the bar the gate holds it to.
- **The constraints.** What must not break (existing behavior/tests), and which files are frozen
  (the injected gate/tests it may not touch).

`goal.md` is EXPLICITLY FORBIDDEN from containing any of:

- The name of any file, function, type, field, or symbol the solver should edit.
- The algorithm, formula, data structure, or approach that fixes it.
- Any "how" — any step of the solution, in code or in prose, in whole or paraphrased.

**De-prescribe the upstream issue.** The issue text above will very often prescribe its own fix —
it'll name the function, quote the one-line formula, walk through the diff. That is exactly the
poison you must strip. Your job is to *extract the problem* from that issue and *drop the
solution*: read the "here's what's broken and how you'd observe it" out of the issue, and leave the
"here's the file and the formula" on the cutting room floor. If the issue says "in `foo.go`'s
`Bar()`, change `x/y` to `x/z`," your `goal.md` says "under condition C, the computed capacity is
systematically too small, so downstream consumers under-report" — the symptom, never the site.

**Your reference fix is private.** To build a discriminating self-test you need to know a real fix
(the `good_cmd` patch under `_controls/`). That knowledge is *your* calibration evidence — it
proves your gate can tell a fix from a no-op. It must never leak into `goal.md` (or, later, the
engine-generated `SCOPE.md`, which mirrors the shipped `goal.md`, not your controls). The controls
live only in the pack's private `_controls/` and are stripped from the pack before it ships to the
solver; keep the goal on the problem side of that line.
