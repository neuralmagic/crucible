//! In-Rust OpenShell backend: run a sandboxed agent turn against the local OpenShell gateway.
//! The control-plane boundary is the gateway's native gRPC API ([`grpc`]), typed Health /
//! sandbox / provider / policy / exec / logs RPCs. Two things stay on the `openshell` CLI
//! because no RPC covers them: file upload/download (SSH-tar) and the gateway/podman boot.
//!
//! Module map:
//! - [`grpc`]: the tonic channel, mTLS setup, and the typed per-turn gateway client.
//! - [`provider`]: mint the Vertex token from ADC (`gcp_auth`) per turn (constants + minting).
//! - `policy`: resolve the egress allowlist (endpoints + binaries) from the manifest.
//! - `sandbox`: the per-workspace sandbox name + the surviving CLI upload/download argv.
//! - `gateway`: boot/teardown the local gateway + rootless podman socket (still subprocess).
//! - `run`: the per-turn flow: env script, prompt-over-stdin, exec, download.

pub mod gateway;
pub mod grpc;
pub mod policy;
pub mod provider;
pub mod run;
pub mod sandbox;

/// The Vertex keys a manifest-less turn (`rank-grounded`, scope-propose) relays from its own
/// process env into the agent env: the claude switches plus every alias `run::vertex_config`
/// honors. A domain loop gets these from `[agent].env` (the manifest validates them); a bare
/// `git clone` turn has no manifest, so the turn pod's plain env (the deploy profile's `[env]`)
/// carries them and this relay is the explicit bridge. `vertex_config`/`env_script` stay
/// manifest-only, they never read the process env themselves.
const VERTEX_RELAY_KEYS: &[&str] = &[
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "GCP_PROJECT_ID",
    "VERTEX_LOCATION",
];

/// Seed `env` with the process values of [`VERTEX_RELAY_KEYS`]: only keys that are set and
/// non-empty relay, and an existing entry (a manifest-provided value) always wins.
pub fn relay_vertex_env(env: &mut Vec<(String, String)>) {
    for key in VERTEX_RELAY_KEYS {
        if env.iter().any(|(k, _)| k == key) {
            continue;
        }
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            env.push((key.to_string(), v));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::openshell::{VERTEX_RELAY_KEYS, relay_vertex_env};

    fn clear_relay_keys() {
        for k in VERTEX_RELAY_KEYS {
            unsafe {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn relay_copies_set_nonempty_keys_and_skips_empty_and_unset() {
        let _guard = crate::test_env_lock();
        clear_relay_keys();
        unsafe {
            std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        }
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "proj-x");
        }
        unsafe {
            std::env::set_var("CLOUD_ML_REGION", "");
        }

        let mut env = Vec::new();
        relay_vertex_env(&mut env);
        clear_relay_keys();

        assert_eq!(
            env,
            vec![
                ("CLAUDE_CODE_USE_VERTEX".to_string(), "1".to_string()),
                (
                    "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
                    "proj-x".to_string()
                ),
            ]
        );
    }

    #[test]
    fn manifest_provided_values_win_over_the_process_env() {
        let _guard = crate::test_env_lock();
        clear_relay_keys();
        unsafe {
            std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "from-process");
        }
        unsafe {
            std::env::set_var("VERTEX_LOCATION", "us-east5");
        }

        let mut env = vec![(
            "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
            "from-manifest".to_string(),
        )];
        relay_vertex_env(&mut env);
        clear_relay_keys();

        assert_eq!(
            env,
            vec![
                (
                    "ANTHROPIC_VERTEX_PROJECT_ID".to_string(),
                    "from-manifest".to_string()
                ),
                ("VERTEX_LOCATION".to_string(), "us-east5".to_string()),
            ]
        );
    }
}
