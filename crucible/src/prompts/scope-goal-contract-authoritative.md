## The `goal.md` contract: this issue is an authoritative brief

The issue above is an authoritative brief: an implementation plan a human adopted with its
prescriptions intact, on purpose. Do not neutralize it. Carry the brief into `goal.md` faithfully
— the files, the approach, the evidence it cites. The gate you build is what keeps the run honest,
not what the goal withholds.

`goal.md` MUST also contain:

- **The measurement.** What the gate runs, what the score is, and what a better score means (the
  direction). The solver needs to know how it will be judged.
- **The acceptance threshold.** What "solved" means — the bar the gate holds it to.
- **The constraints.** What must not break (existing behavior/tests), and which files are frozen
  (the injected gate/tests it may not touch).

**Your reference fix is private.** To build a discriminating self-test you need to know a real fix
(the `good_cmd` patch under `_controls/`). That calibration evidence still never leaks into
`goal.md`: the brief's own prescriptions belong there, your `_controls/` do not.
