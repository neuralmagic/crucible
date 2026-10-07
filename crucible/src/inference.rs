//! The run's injected inference bindings, read from the process environment once per use.

use crucible_contract::inference::{
    ENV_INFERENCE, InferenceError, InferenceRole, ResolvedInference,
};

pub fn from_process_env() -> Result<ResolvedInference, InferenceError> {
    ResolvedInference::parse(&std::env::var(ENV_INFERENCE).unwrap_or_default())
}

/// Keep the inference document and the decision binding's credential out of a task process. A
/// decision has no consumer outside the engine (RFC-0001 C-INFERENCE): only a route task asks it.
pub fn withhold_from_task(cmd: &mut std::process::Command) {
    if let Ok(inference) = from_process_env()
        && let Some(key) = inference
            .binding(InferenceRole::Decision)
            .and_then(|binding| binding.key_env.as_ref())
    {
        cmd.env_remove(key.as_str());
    }
    cmd.env_remove(ENV_INFERENCE);
}
