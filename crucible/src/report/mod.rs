//! The boundary between the orchestration loop and how it talks to a human.
//!
//! The loop (`crate::run_loop`) is written once and calls a [`reporter::Reporter`]; the
//! implementations are [`crate::report::console::ConsoleReporter`] (headless: plain lines,
//! for CI / in-cluster pods / pipes) and [`crate::report::stream::SessionReporter`] (NDJSON session
//! events). All drive the identical keep/discard logic, so headless is a first-class
//! mode, not a degraded fallback.

#[cfg(feature = "autoresearch")]
pub(crate) mod console;
pub(crate) mod flow_dd;
pub(crate) mod ingest_client;
#[cfg(feature = "autoresearch")]
pub(crate) mod reporter;
#[cfg(feature = "autoresearch")]
pub(crate) mod result_mode;
pub(crate) mod session;
#[cfg(feature = "autoresearch")]
pub(crate) mod stream;
