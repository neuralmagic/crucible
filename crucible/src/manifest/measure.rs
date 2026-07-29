use serde::Deserialize;

/// The codegen tool contract for a GPU-measured code domain. Projected
/// into the loop pod's broker-child env as BROKER_CODEGEN=1 + BROKER_CODEGEN_TOOLS_DEFAULTS, so the
/// frozen-command contract lives in the domain manifest (the source of truth), not hand-written JSON
/// in a per-cluster deploy profile. Captured as a raw table and serialized verbatim: the broker
/// (crucible-broker codegen::config::ToolsOverlay) is the single validator, it finalize()s this at
/// startup, so we don't re-declare its schema here. The [measure] table is exactly the
/// BROKER_CODEGEN_TOOLS_DEFAULTS JSON shape: { gpus, build, benchmark, lm_eval, profile? }.
#[derive(Deserialize, Clone, Default)]
#[serde(transparent)]
pub struct MeasureCfg {
    pub tools: toml::Table,
}

impl MeasureCfg {
    /// The broker-child env this contract projects: the enable flag + the tools-defaults JSON. The
    /// per-cluster substrate facts (namespace/queue/PVCs/max_gpus) come from the deploy profile, not here.
    pub fn broker_env(&self) -> Result<Vec<(String, String)>, String> {
        let json = serde_json::to_string(&self.tools).map_err(|e| {
            format!("serializing [measure] to BROKER_CODEGEN_TOOLS_DEFAULTS json: {e}")
        })?;
        Ok(vec![
            ("BROKER_CODEGEN".to_string(), "1".to_string()),
            ("BROKER_CODEGEN_TOOLS_DEFAULTS".to_string(), json),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_projects_codegen_enable_flag_and_tools_defaults_json() {
        // A [measure] table (the codegen tool contract) projects the enable flag plus the tools JSON,
        // and that JSON round-trips back to the same gpus/build.base_image the broker will finalize().
        let cfg: MeasureCfg = toml::from_str(
            r#"
            gpus = 2
            [build]
            base_image = "registry.example.com/kernel-base:latest"
            [benchmark]
            command = "./bench.sh"
        "#,
        )
        .expect("measure table parses");

        let env = cfg.broker_env().expect("projects env");
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("BROKER_CODEGEN"), Some("1"), "enable flag");

        let json = get("BROKER_CODEGEN_TOOLS_DEFAULTS").expect("tools-defaults json present");
        let parsed: serde_json::Value =
            serde_json::from_str(json).expect("tools-defaults is valid json");
        assert_eq!(parsed["gpus"], 2, "gpus survives the toml -> json trip");
        assert_eq!(
            parsed["build"]["base_image"], "registry.example.com/kernel-base:latest",
            "nested build.base_image survives"
        );
        assert_eq!(parsed["benchmark"]["command"], "./bench.sh");
    }
}
