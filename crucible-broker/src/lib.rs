//! `crucible-broker`: the domain-agnostic mediated-provisioning broker.
//!
//! The sandboxed agent has NO direct authority to spend GPU, submit a Kueue Workload, or
//! dispatch CI. Instead it asks this server-side broker (the loop-pod MCP) for a resource,
//! today `request_trace`: "give me a calibrated trace for these run params." The broker holds
//! the privilege and the admission logic and decides:
//!
//! ```text
//!   cache HIT             -> return the cached knobs, the turn continues GPU-free
//!   MISS + in-scope       -> admission-gate a capture (no judge change)
//!   MISS + judge-changing -> needs human approval (re-scope), or escalate-halt if headless
//! ```
//!
//! This crate is the reusable engine: the typed request/resolution surface ([`types`]), the
//! content key + judge-impact classifier ([`types::trace_id`] / [`types::judge_impact`]), the
//! [`TraceResolver`]/[`Admission`]/[`ApprovalBackend`] boundaries, the MCP wire ([`server`]), and
//! the engine-side build/measure/profile tools (forge-backed). It holds NO domain logic: the
//! concrete trace-cache resolver is injected by the per-domain broker binary. crucible spawns
//! that binary; it never links it.

pub mod admission;
pub mod approval;
pub mod auth;
pub mod bounds;
pub mod broker;
pub mod build;
pub mod codegen;
pub mod control_approval;
pub mod deploy;
pub mod describe;
pub mod distress;
pub mod draft_pr;
pub(crate) mod gateway;
pub mod gpu_check;
pub mod jira;
pub(crate) mod measure;
pub mod profile;
pub mod report;
pub mod server;
pub(crate) mod slack;
pub(crate) mod steps;
pub mod telemetry;
pub(crate) mod turn;
pub mod types;
pub(crate) mod watch;

pub use admission::{Admission, AdmissionDecision, GpuNeed};
pub use approval::{ApprovalBackend, ApprovalChannel, ApprovalRequest, ApprovalState};
pub use bounds::Bounds;
pub use broker::{Broker, NullResolver, TraceResolver};
pub use types::{JudgeImpact, Resolution, TraceId, TraceParams, judge_impact, trace_id};
