# formal

Veil models of the engine, checked by `just formal` (`lake build` in this directory).

`CrucibleSpec/PlanExec.lean` models the plan executor in `crucible/src/plan/exec.rs` over the
tables in `crucible/src/plan/machine.rs`: serial dispatch, revise rounds, truncation, halts, and
epilogues, with the RFC-0002 playbook obligations as invariants. `#model_check` explores every
well-formed three-task graph and every run of it; `#check_invariants` proves the same invariants
inductive for graphs of any size. Fan-out, transport retries and concurrent isolated batches are
not modelled yet.

`just formal-mutants` breaks one executor rule per copy of the model and fails unless the model
checker finds a violation in every copy.
