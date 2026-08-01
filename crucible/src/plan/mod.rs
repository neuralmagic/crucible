//! Work graphs (ADR-0025): a **plan** is a versioned, validated DAG of **tasks**; a
//! deterministic executor runs it. The model plans, the engine executes — never the reverse.
//!
//! Three front-ends produce the same IR: built-in templates (the canonical loop, the wide
//! tournament), pack-authored TOML, and agent-emitted `PLAN.json` (admission-gated). One
//! executor consumes it.
//!
//!   authoring                    admission                 execution
//!   ─────────                    ─────────                 ─────────
//!   template ─┐
//!   TOML ─────┼─> Plan ──validate──> ValidPlan ──admit──> Executor ──> PlanOutcome
//!   PLAN.json ┘        (unique names,          (budget/size caps,    (readiness loop,
//!                       edges resolve,          authorship ≠          retry≠check,
//!                       acyclic, budget)        authorization)        short-circuit)

pub mod cli;
pub mod exec;
pub mod harness;
pub mod ir;
pub mod runner;
pub mod term_img;
pub mod worktree;
