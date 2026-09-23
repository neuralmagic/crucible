module

public import Veil

/-! # crucible plan executor, serial

`crucible::plan::exec::execute` over the tables in `crucible/src/plan/machine.rs`: one task or
revise loop in flight, no fan-out or transport retries. The graph is state that `after_init`
chooses and no action writes, so `#model_check` covers every well-formed graph of the instance.
-/

veil module PlanExec

set_option maxRecDepth 8192
set_option synthInstance.maxHeartbeats 400000

type task

enum tstate = { t_pending, t_running, t_revising, t_pass, t_fail, t_skipped, t_not_taken, t_transport, t_blocked, t_truncated }
enum pstate = { p_admitted, p_dispatching, p_draining, p_completed, p_halted, p_truncated }
enum jkind = { j_all, j_passed, j_settled }
enum hkind = { h_none, h_short, h_ceiling }
enum rphase = { rp_none, rp_target, rp_review, rp_between }

instantiate topo : TotalOrder task
instantiate topo_dec : DecidableRel topo.le
open TotalOrder

relation dep : task → task → Bool
relation required : task → Bool
relation epilogue : task → Bool
relation runnable : task → Bool
relation route_of : task → task → Bool
function join : task → jkind
-- `revises r t`: reviewer r sends t back (C-REVISE-LOOP).
relation revises : task → task → Bool
relation reach : task → task → Bool
immutable individual max_rounds : Nat

function status : task → tstate
individual plan : pstate
individual halt : hkind
relation dispatched : task → Bool
relation dispatched_while_halted : task → Bool
function phase : task → rphase
function rounds : task → Nat
function last : task → tstate
relation review_ran : task → Bool

#gen_state

assumption [rounds_range] 2 ≤ max_rounds

ghost relation settled (t : task) :=
  status t ≠ t_pending ∧ status t ≠ t_running ∧ status t ≠ t_revising

ghost relation held (t : task) := status t = t_pass ∨ status t = t_not_taken

ghost relation main_settled := ∀ m, ¬ epilogue m → settled m

ghost relation deps_settled (t : task) := ∀ d, dep t d → settled d

ghost relation idle := ∀ u, status u ≠ t_running ∧ status u ≠ t_revising

ghost relation open_for (t : task) :=
  (plan = p_dispatching ∨ (plan = p_draining ∧ halt = h_short ∧ epilogue t)) ∧
  (epilogue t → main_settled)

ghost relation route_not_taken (t : task) := ∃ r, route_of t r ∧ status r = t_not_taken

ghost relation route_undecided (t : task) :=
  ∃ r, route_of t r ∧ status r ≠ t_pass ∧ status r ≠ t_not_taken ∧ join t ≠ j_all

ghost relation branch_not_taken (t : task) :=
  join t = j_all ∧ ∃ d, dep t d ∧ status d = t_not_taken

ghost relation route_passed (t : task) := ∃ r, route_of t r ∧ status r = t_pass

-- A target with a runnable reviewer only runs inside the loop.
ghost relation paired (t : task) := ∃ r, revises r t ∧ runnable r

-- The reviewer's join against this round's target result.
ghost relation review_allowed (r t : task) :=
  (join r = j_all → last t = t_pass ∧ ∀ d, dep r d ∧ d ≠ t → status d = t_pass) ∧
  (join r = j_passed → last t = t_pass ∨ ∃ d, dep r d ∧ d ≠ t ∧ status d = t_pass)

ghost relation deps_allow (t : task) :=
  (join t = j_all → ∀ d, dep t d → status d = t_pass) ∧
  (join t = j_passed → ∃ d, dep t d ∧ status d = t_pass)

after_init {
  dep T D := *
  require ∀ t d, dep t d → (le d t ∧ d ≠ t)
  join T := *
  require ∀ t, join t ≠ j_all → ∃ d, dep t d
  runnable T := *
  require ∀ t, runnable t →
    (join t = j_all → ∀ d, dep t d → runnable d) ∧ (join t = j_passed → ∃ d, dep t d ∧ runnable d)
  epilogue T := *
  require ∀ t d, dep t d → (epilogue t ↔ epilogue d)
  route_of T R := *
  require (∀ t r, route_of t r → dep t r) ∧ (∀ t r1 r2, route_of t r1 ∧ route_of t r2 → r1 = r2)
  reach A B := *
  require ∀ a b, reach a b ↔ (dep a b ∨ ∃ m, dep a m ∧ reach m b)
  revises R T := *
  require (∀ r t, revises r t → dep r t ∧ ¬ epilogue r) ∧
    (∀ r t1 t2, revises r t1 ∧ revises r t2 → t1 = t2) ∧
    (∀ r1 r2 t, revises r1 t ∧ revises r2 t → r1 = r2) ∧
    (∀ r t x, revises r t → ¬ revises t x ∧ ¬ revises x r) ∧
    (∀ r t d, revises r t ∧ dep r d ∧ d ≠ t → ¬ reach d t)
  required T := *
  status T := t_pending
  plan := p_admitted
  halt := h_none
  dispatched T := false
  dispatched_while_halted T := false
  phase T := rp_none
  rounds T := 0
  last T := t_pending
  review_ran T := false
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
  require idle
  require plan = p_draining
  require status t = t_pending
  require epilogue t → main_settled
  require ¬ (epilogue t ∧ halt = h_short)
  status t := t_blocked
}

-- An advisory task the substrate cannot run is skipped (Pending --Unrunnable--> Skipped).
action skip_unrunnable (t : task) {
  require idle
  require open_for t
  require status t = t_pending
  require ¬ runnable t
  status t := t_skipped
  short_circuit t
}

-- ConditionUnmet or BranchNotTaken. A passed route's answer is not modelled: any `when` may miss it.
action settle_not_taken (t : task) {
  require idle
  require open_for t
  require status t = t_pending
  require runnable t
  require deps_settled t
  require route_not_taken t ∨ route_passed t ∨ route_undecided t ∨ branch_not_taken t
  status t := t_not_taken
}

action block_on_dependency (t : task) {
  require idle
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
  require ¬ paired t
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

-- C-REVISE-LOOP: the pair dispatches as one loop once the target is ready under its own join and
-- every other dependency of the reviewer has settled.
action start_rounds (t : task) {
  require open_for t
  require idle
  require status t = t_pending
  require runnable t
  require deps_settled t
  require ¬ route_not_taken t ∧ ¬ route_undecided t ∧ ¬ branch_not_taken t
  require deps_allow t
  require ∃ r, revises r t ∧ runnable r ∧ ∀ d, dep r d → d = t ∨ settled d
  status R := if revises R t then t_revising else status R
  status t := t_revising
  rounds t := 1
  phase t := rp_target
}

procedure end_rounds (t r : task) {
  status t := last t
  status r := last r
  phase t := rp_none
  if last t ≠ t_pass then
    short_circuit t
  if last r ≠ t_pass then
    short_circuit r
}

action target_round (t : task) {
  require phase t = rp_target
  let s :| s = t_pass ∨ s = t_fail ∨ s = t_skipped ∨ s = t_transport
  last t := s
  dispatched t := true
  phase t := rp_review
}

action review_round (t r : task) {
  require phase t = rp_review
  require revises r t
  require plan = p_dispatching
  if review_allowed r t then
    let s :| s = t_pass ∨ s = t_fail ∨ s = t_skipped ∨ s = t_transport
    last r := s
    review_ran r := true
    if s = t_fail ∧ rounds t < max_rounds then
      phase t := rp_between
    else
      end_rounds t r
  else
    last r := t_blocked
    end_rounds t r
}

action next_round (t : task) {
  require phase t = rp_between
  require plan = p_dispatching
  rounds t := rounds t + 1
  phase t := rp_target
}

-- A ceiling before the reviewer's dispatch blocks the reviewer; before another round, the last
-- round's rows stand.
action ceiling_in_rounds (t r : task) {
  require phase t = rp_review ∨ phase t = rp_between
  require revises r t
  require plan = p_dispatching
  if phase t = rp_review then
    last r := t_blocked
  plan := p_draining
  halt := h_ceiling
  end_rounds t r
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
-- `runnable_set`: an all join needs every dependency runnable, a passed join one, a settled join none.
invariant [runnable_by_join]
  runnable T → (join T = j_all → ∀ d, dep T d → runnable d) ∧ (join T = j_passed → ∃ d, dep T d ∧ runnable d)
invariant [stages_do_not_cross] dep T D → (epilogue T ↔ epilogue D)
invariant [route_is_dep] route_of T R → dep T R
invariant [lossy_join_has_deps] join T ≠ j_all → ∃ d, dep T d

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

invariant [revise_well_formed] revises R T → dep R T ∧ ¬ epilogue R ∧ ¬ revises T X ∧ ¬ revises X R
invariant [one_reviewer] revises R T ∧ revises Q T → R = Q
invariant [one_target] revises R T ∧ revises R U → T = U
invariant [reach_closure] reach A B ↔ (dep A B ∨ ∃ m, dep A m ∧ reach m B)
invariant [phase_means_revising] phase T ≠ rp_none → status T = t_revising ∧ ∃ r, revises r T ∧ status r = t_revising
invariant [revising_has_phase] status T = t_revising ∧ revises R T → phase T ≠ rp_none
invariant [reviewer_revising] status R = t_revising ∧ revises R T → status T = t_revising
invariant [revising_is_paired] status T = t_revising → (∃ r, revises r T) ∨ (∃ t, revises T t)
invariant [loop_only_while_dispatching] phase T ≠ rp_none → plan = p_dispatching
invariant [reviewer_alone_means_no_loop] revises R T ∧ dispatched R → ¬ dispatched T
invariant [pending_reviewer_means_no_loop]
  revises R T ∧ runnable R ∧ status R = t_pending → ¬ dispatched T ∧ ¬ review_ran R
invariant [rounds_start_at_one] phase T ≠ rp_none → 1 ≤ rounds T
invariant [one_loop] phase T ≠ rp_none ∧ phase U ≠ rp_none → T = U
invariant [nothing_runs_in_a_loop] phase T ≠ rp_none → status U ≠ t_running
invariant [loop_target_ready] phase T ≠ rp_none → (∀ d, dep T d → settled d) ∧ deps_allow T ∧ ¬ epilogue T
invariant [target_round_result]
  (phase T = rp_review ∨ phase T = rp_between) →
    last T = t_pass ∨ last T = t_fail ∨ last T = t_skipped ∨ last T = t_transport
invariant [reviewer_runs_alone_only_without_a_loop] revises R T ∧ status R = t_running → ¬ dispatched T ∧ ¬ review_ran R
invariant [reviewer_waits_for_target]
  status T = t_pending ∧ revises R T →
    ¬ review_ran R ∧
      (status R = t_pending ∨ (status R = t_blocked ∧ plan = p_draining) ∨ (status R = t_skipped ∧ ¬ runnable R))
invariant [revising_only_in_the_loop] status U = t_revising → ∃ t, phase t ≠ rp_none ∧ (U = t ∨ revises U t)

-- C-REVISE-LOOP: the bound holds.
safety [rounds_bounded] rounds T ≤ max_rounds

-- Another round begins only after a failing review with rounds to spare.
safety [another_round_needs_a_failing_review]
  phase T = rp_between → rounds T < max_rounds ∧ ∃ r, revises r T ∧ last r = t_fail ∧ review_ran r

safety [a_second_round_follows_a_review] 2 ≤ rounds T → ∃ r, revises r T ∧ review_ran r

-- A dependent of either task waits until both have settled under their own names.
invariant [paired_target_never_runs_alone] revises R T ∧ runnable R → status T ≠ t_running
invariant [pair_settles_together] revises R T ∧ runnable R ∧ dispatched T ∧ settled T → settled R
safety [dependent_waits_for_the_pair] dispatched X ∧ dep X T ∧ revises R T ∧ runnable R ∧ dispatched T → settled R

-- A failing reviewer row ends the loop only at the bound, or when a ceiling stopped it.
safety [failing_review_ends_at_the_bound]
  revises R T ∧ review_ran R ∧ status R = t_fail → rounds T = max_rounds ∨ halt = h_ceiling

safety [halt_matches_plan]
  (plan = p_dispatching ∨ plan = p_completed ∨ plan = p_admitted ∨ plan = p_truncated) ↔ halt = h_none

#gen_spec

#model_check compiled { task := Fin 3 } { max_rounds := 2 } (parallelCfg := some { numSubTasks := 8, thresholdToParallel := 20 })

#check_invariants

end PlanExec
