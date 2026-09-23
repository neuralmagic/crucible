# formal

Veil models of the engine, checked by `just formal` (`lake build` in this directory).

`CrucibleSpec/PlanExec.lean` models the plan executor in `crucible/src/plan/exec.rs` over the
tables in `crucible/src/plan/machine.rs`: serial dispatch, concurrent isolated batches, revise
rounds, truncation, halts, and epilogues, with the RFC-0002 playbook obligations as invariants.
`#model_check` explores every well-formed three-task graph and every run of it;
`#check_invariants` proves the same invariants inductive for graphs of any size. Fan-out and
transport retries are not modelled yet.

`just formal-mutants` breaks one executor rule per copy of the model and fails unless the model
checker finds a violation in every copy.

`every_three_task_graph_keeps_the_model_invariants` and `every_revise_pair_keeps_the_loop_invariants`
in `crucible/src/plan/exec.rs` run `execute` itself on every graph of that size and check the same
invariants, so a gap between the model and the Rust shows up as a failing unit test.
