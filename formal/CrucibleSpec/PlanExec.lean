module

public import Veil

/-! # crucible plan executor, serial

`crucible::plan::exec::execute` over the tables in `crucible/src/plan/machine.rs`: one task in
flight, no fan-out, revise rounds or transport retries. The graph is state that `after_init`
chooses and no action writes, so `#model_check` covers every well-formed graph of the instance.
-/

veil module PlanExec

type task

enum tstate = { t_pending, t_running, t_pass, t_fail, t_skipped, t_not_taken, t_transport, t_blocked, t_truncated }
enum pstate = { p_admitted, p_dispatching, p_draining, p_completed, p_halted, p_truncated }
enum jkind = { j_all, j_passed, j_settled }
enum hkind = { h_none, h_short, h_ceiling }

instantiate topo : TotalOrder task
instantiate topo_dec : DecidableRel topo.le
open TotalOrder

relation dep : task → task → Bool
relation required : task → Bool
relation epilogue : task → Bool
relation runnable : task → Bool
relation route_of : task → task → Bool
function join : task → jkind

function status : task → tstate
individual plan : pstate
individual halt : hkind
relation dispatched : task → Bool
relation dispatched_while_halted : task → Bool

#gen_state

ghost relation settled (t : task) := status t ≠ t_pending ∧ status t ≠ t_running

ghost relation held (t : task) := status t = t_pass ∨ status t = t_not_taken

ghost relation main_settled := ∀ m, ¬ epilogue m → settled m

ghost relation deps_settled (t : task) := ∀ d, dep t d → settled d

ghost relation idle := ∀ u, status u ≠ t_running

ghost relation open_for (t : task) :=
  (plan = p_dispatching ∨ (plan = p_draining ∧ halt = h_short ∧ epilogue t)) ∧
  (epilogue t → main_settled)

ghost relation route_not_taken (t : task) := ∃ r, route_of t r ∧ status r = t_not_taken

ghost relation route_undecided (t : task) :=
  ∃ r, route_of t r ∧ status r ≠ t_pass ∧ status r ≠ t_not_taken ∧ join t ≠ j_all

ghost relation branch_not_taken (t : task) :=
  join t = j_all ∧ ∃ d, dep t d ∧ status d = t_not_taken

ghost relation route_passed (t : task) := ∃ r, route_of t r ∧ status r = t_pass

ghost relation deps_allow (t : task) :=
  (join t = j_all → ∀ d, dep t d → status d = t_pass) ∧
  (join t = j_passed → ∃ d, dep t d ∧ status d = t_pass)

after_init {
  dep T D := *
  require ∀ t d, dep t d → (le d t ∧ d ≠ t)
  runnable T := *
  require ∀ t d, runnable t ∧ dep t d → runnable d
  epilogue T := *
  require ∀ t d, epilogue t ∧ dep t d → epilogue d
  route_of T R := *
  require ∀ t r, route_of t r → dep t r
  require ∀ t r1 r2, route_of t r1 ∧ route_of t r2 → r1 = r2
  required T := *
  join T := *
  status T := t_pending
  plan := p_admitted
  halt := h_none
  dispatched T := false
  dispatched_while_halted T := false
}

-- C-PLAYBOOK-CAPS: a required main-graph task the substrate cannot run truncates the plan.
action truncate {
  require plan = p_admitted
  require ∃ t, required t ∧ ¬ epilogue t ∧ ¬ runnable t
  plan := p_truncated
  status T := t_truncated
}

action start {
  require plan = p_admitted
  require ∀ t, required t ∧ ¬ epilogue t → runnable t
  plan := p_dispatching
}

procedure short_circuit (t : task) {
  if required t ∧ ¬ epilogue t ∧ plan = p_dispatching then
    plan := p_draining
    halt := h_short
}

-- After a halt everything left settles blocked, except an epilogue reporting a short circuit.
action block_on_halt (t : task) {
  require plan = p_draining
  require status t = t_pending
  require epilogue t → main_settled
  require ¬ (epilogue t ∧ halt = h_short)
  status t := t_blocked
}

-- An advisory task the substrate cannot run is skipped (Pending --Unrunnable--> Skipped).
action skip_unrunnable (t : task) {
  require open_for t
  require status t = t_pending
  require ¬ runnable t
  status t := t_skipped
  short_circuit t
}

-- ConditionUnmet or BranchNotTaken. A passed route's answer is not modelled: any `when` may miss it.
action settle_not_taken (t : task) {
  require open_for t
  require status t = t_pending
  require runnable t
  require deps_settled t
  require route_not_taken t ∨ route_passed t ∨ route_undecided t ∨ branch_not_taken t
  status t := t_not_taken
}

action block_on_dependency (t : task) {
  require open_for t
  require status t = t_pending
  require runnable t
  require deps_settled t
  require ¬ route_not_taken t ∧ ¬ route_undecided t ∧ ¬ branch_not_taken t
  require ¬ deps_allow t
  status t := t_blocked
  short_circuit t
}

action dispatch (t : task) {
  require open_for t
  require idle
  require status t = t_pending
  require runnable t
  require deps_settled t
  require ¬ route_not_taken t ∧ ¬ route_undecided t ∧ ¬ branch_not_taken t
  require deps_allow t
  status t := t_running
  dispatched t := true
  if plan = p_draining then
    dispatched_while_halted t := true
}

action settle (t : task) {
  require status t = t_running
  let s :| s = t_pass ∨ s = t_fail ∨ s = t_skipped ∨ s = t_transport
  status t := s
  if s ≠ t_pass then
    short_circuit t
}

-- The cost or wall-clock ceiling, reached between dispatches.
action ceiling {
  require plan = p_dispatching
  require idle
  plan := p_draining
  halt := h_ceiling
}

action finish {
  require plan = p_dispatching
  require ∀ t, settled t
  plan := p_completed
}

action finish_halted {
  require plan = p_draining
  require ∀ t, settled t
  plan := p_halted
}

-- The graph never changes after init.
invariant [acyclic] dep T D → (le D T ∧ D ≠ T)
invariant [runnable_closed] runnable T ∧ dep T D → runnable D
invariant [epilogue_closed] epilogue T ∧ dep T D → epilogue D
invariant [route_is_dep] route_of T R → dep T R

-- Facts the safety properties rest on.
invariant [pending_undispatched] status T = t_pending → ¬ dispatched T
invariant [admitted_untouched] plan = p_admitted → status T = t_pending ∧ ¬ dispatched T
invariant [dispatching_held] plan = p_dispatching ∧ required T ∧ ¬ epilogue T ∧ settled T → held T

-- C-PLAYBOOK-CAPS: a truncated plan dispatches nothing.
safety [truncation_dispatches_nothing] plan = p_truncated → ¬ dispatched T

safety [truncated_only_with_plan] status T = t_truncated → plan = p_truncated

safety [unrunnable_required_truncates]
  required T ∧ ¬ epilogue T ∧ ¬ runnable T → plan = p_admitted ∨ plan = p_truncated

-- A not-taken branch never dispatched and so spent nothing.
safety [not_taken_never_dispatched] status T = t_not_taken → ¬ dispatched T

safety [not_taken_has_a_cause]
  status T = t_not_taken → (∃ r, route_of T r) ∨ (join T = j_all ∧ ∃ d, dep T d ∧ status d = t_not_taken)

safety [dispatch_after_dependencies] dispatched T ∧ dep T D → settled D

-- An `all` join reads only passing dependencies.
safety [all_join_reads_passing] dispatched T ∧ join T = j_all ∧ dep T D → status D = t_pass

-- An epilogue observes the settled main graph.
safety [epilogue_after_main] dispatched E ∧ epilogue E ∧ ¬ epilogue M → settled M

-- C-PLAYBOOK-LANE: `exit == Completed` alone implies every required main-graph task held.
safety [completed_means_valid] plan = p_completed ∧ required T ∧ ¬ epilogue T → held T

-- C-PLAYBOOK-LANE: only a required main-graph task short-circuits, so an epilogue never changes the verdict.
safety [short_circuit_has_a_cause]
  halt = h_short → ∃ t, required t ∧ ¬ epilogue t ∧ settled t ∧ ¬ held t

-- After a halt, the only dispatch is an epilogue reporting a short circuit.
safety [nothing_runs_after_a_ceiling]
  dispatched_while_halted T → epilogue T ∧ halt = h_short

safety [one_in_flight] status T = t_running ∧ status U = t_running → T = U

safety [halt_matches_plan]
  (plan = p_dispatching ∨ plan = p_completed ∨ plan = p_admitted ∨ plan = p_truncated) ↔ halt = h_none

#gen_spec

sat trace [can_complete] {
  any 4 actions
  assert (plan = p_completed)
}

#model_check compiled { task := Fin 3 } {} (parallelCfg := some { numSubTasks := 8, thresholdToParallel := 20 })

#check_invariants

end PlanExec
