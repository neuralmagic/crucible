//! The run's injected inference bindings, read from the process environment once per use.

use crucible_contract::inference::{ENV_INFERENCE, InferenceError, ResolvedInference};

pub fn from_process_env() -> Result<ResolvedInference, InferenceError> {
    ResolvedInference::parse(&std::env::var(ENV_INFERENCE).unwrap_or_default())
}
