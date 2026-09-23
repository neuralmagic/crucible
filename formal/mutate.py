import pathlib
import subprocess
import sys

formal = pathlib.Path(__file__).resolve().parent
out = formal / ".lake" / "mutants"
out.mkdir(parents=True, exist_ok=True)
src = (formal / "CrucibleSpec/PlanExec.lean").read_text()
src = src.replace("#model_check compiled { task := Fin 3 }", "#model_check compiled { task := Fin 2 }")
i = src.index("sat trace [can_complete]")
j = src.index("}\n", i) + 2
src = src[:i] + src[j:]
src = src.replace("#check_invariants\n", "")

MUTANTS = {
    "epilogue_short_circuits": (
        "  if required t ∧ ¬ epilogue t ∧ plan = p_dispatching then",
        "  if required t ∧ (plan = p_dispatching ∨ plan = p_draining) then",
    ),
    "dispatch_ignores_join": (
        "  require deps_allow t\n  status t := t_running",
        "  status t := t_running",
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
}

failed = False
for name, (old, new) in MUTANTS.items():
    assert src.count(old) == 1, name
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
