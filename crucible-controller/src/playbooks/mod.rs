//! Playbooks: the registry of runnable packs, their drafts and imports, what a pack declares
//! (exposure, agent substrate, params) and what this deployment can dispatch it onto.

pub(crate) mod api;
pub mod co_draft;
pub mod dispatch;
pub mod drafts;
pub mod exposure;
pub mod imports;
pub mod packs;
pub mod param_placeholder;
pub mod plan_graph;
pub mod preflight;
pub mod preview;
pub mod providers;
pub mod registry;
