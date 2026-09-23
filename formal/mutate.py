import pathlib
import subprocess
import sys

formal = pathlib.Path(__file__).resolve().parent
out = formal / ".lake" / "mutants"
out.mkdir(parents=True, exist_ok=True)
src = (formal / "CrucibleSpec/PlanExec.lean").read_text()
src = src.replace("#check_invariants\n", "")

MUTANTS = {
    "epilogue_short_circuits": (
        "  if required t ∧ ¬ epilogue t ∧ plan = p_dispatching then",
        "  if required t ∧ (plan = p_dispatching ∨ plan = p_draining) then",
    ),
    "dispatch_ignores_join": (
        "  require deps_allow t\n  require ¬ paired t\n",
        "  require ¬ paired t\n",
    ),
    "required_failure_does_not_halt": (
        "  if s ≠ t_pass then\n    short_circuit t",
        "  if s = t_skipped then\n    short_circuit t",
    ),
    "ceiling_lets_epilogue_run": (
        "(plan = p_dispatching ∨ (plan = p_draining ∧ halt = h_short ∧ epilogue t))",
        "(plan = p_dispatching ∨ (plan = p_draining ∧ epilogue t))",
    ),
    "not_taken_without_cause": (
        "  require route_not_taken t ∨ route_passed t ∨ route_undecided t ∨ branch_not_taken t\n",
        "",
    ),
    "start_ignores_unrunnable": (
        "  require ∀ t, required t ∧ ¬ epilogue t → runnable t\n  plan := p_dispatching",
        "  plan := p_dispatching",
    ),
    "another_round_after_a_passing_review": (
        "    if s = t_fail ∧ rounds t < max_rounds then",
        "    if rounds t < max_rounds then",
    ),
    "rounds_unbounded": (
        "    if s = t_fail ∧ rounds t < max_rounds then",
        "    if s = t_fail then",
    ),
    "target_row_lands_before_the_review": (
        "  last t := s\n  dispatched t := true\n  phase t := rp_review",
        "  last t := s\n  status t := s\n  dispatched t := true\n  phase t := rp_review",
    ),
    "serial_task_joins_a_batch": (
        "  require isolated t ∧ batch_open ∧ (∀ u, status u = t_running → isolated u) ∨ idle",
        "  require batch_open ∨ idle",
    ),
}

for name, (old, _) in MUTANTS.items():
    assert src.count(old) == 1, f"{name}: the pattern matches {src.count(old)} times"

failed = False
for name, (old, new) in MUTANTS.items():
    body = src.replace(old, new).replace("veil module PlanExec", f"veil module M_{name}").replace(
        "end PlanExec", f"end M_{name}"
    )
    path = out / f"{name}.lean"
    path.write_text(body)
    r = subprocess.run(["lake", "env", "lean", str(path)], cwd=formal, capture_output=True, text=True)
    log = r.stdout + r.stderr
    caught = [l for l in log.splitlines() if "❌" in l]
    status = "CAUGHT" if caught else "SURVIVED"
    failed |= not caught
    print(f"{status:9} {name}")
    for l in caught[:3]:
        print("          ", l.strip()[:220])
    if not caught:
        print("          ", "\n           ".join(log.strip().splitlines()[-4:])[:600])
sys.exit(1 if failed else 0)
