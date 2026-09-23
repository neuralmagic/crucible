//! Runs: a dispatched engine from launch to ingested outcome. Work pods and the clusters they
//! land on, the live relay, the drop-box and log scrape that bring the session back, the run's
//! artifacts and evidence, and the completion edge that folds it into the ledger.

pub(crate) mod api;
pub mod artifacts;
pub mod blob_store;
pub mod cluster_push;
pub mod cluster_stats;
pub mod clusters;
pub mod completion;
pub mod contract;
pub mod dispatch_target;
pub mod engine;
pub mod export;
pub(crate) mod flow_enriched;
pub mod ingest;
pub mod ingest_auth;
pub mod ingest_drop;
pub mod launch;
pub(crate) mod live;
pub mod local_run;
pub mod migrate_state;
pub mod mlflow;
pub mod model;
pub mod run_files;
pub mod store;
pub mod task_evidence;
pub mod task_results;
#[cfg(feature = "autoresearch")]
pub(crate) mod turn_live;
pub mod work_pods;
pub mod workpod;
