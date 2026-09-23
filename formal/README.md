# formal

Veil models of the engine, checked by `just formal` (`lake build` in this directory).

`CrucibleSpec/PlanExec.lean` models the serial plan executor in `crucible/src/plan/exec.rs` over
the tables in `crucible/src/plan/machine.rs`, with the RFC-0002 playbook obligations as
invariants. `#model_check` explores every well-formed three-task graph and every run of it;
`#check_invariants` proves the same invariants inductive for graphs of any size.
