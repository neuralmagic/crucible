use serde::Deserialize;

/// The codegen tool contract for a GPU-measured code domain. Projected
/// into the loop pod's broker-child env as BROKER_CODEGEN=1 + BROKER_CODEGEN_TOOLS_DEFAULTS, so the
/// frozen-command contract lives in the domain manifest (the source of truth), not hand-written JSON
/// in a per-cluster deploy profile. Captured as a raw table and serialized verbatim: the broker
/// (crucible-broker codegen::config::ToolsOverlay) is the single validator, it finalize()s this at
/// startup, so we don't re-declare its schema here. The [measure] table is exactly the
/// BROKER_CODEGEN_TOOLS_DEFAULTS JSON shape: { gpus, build, benchmark, lm_eval, profile?, probe? }.
#[derive(Deserialize, Clone, Default)]
#[serde(transparent)]
pub struct MeasureCfg {
    pub tools: toml::Table,
}

impl MeasureCfg {
    /// The build modes `[measure.build].mutable_kwargs.mode` declares, in declared order. `None`
    /// when the key is absent or is not an array of strings: the broker owns the schema, so a
    /// shape this doesn't recognize is reported as "not declared" rather than re-validated here.
    pub fn build_modes(&self) -> Option<Vec<String>> {
        self.tools
            .get("build")?
            .as_table()?
            .get("mutable_kwargs")?
            .as_table()?
            .get("mode")?
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect()
    }

    /// The broker-child env this contract projects: the enable flag + the tools-defaults JSON. The
    /// per-cluster substrate facts (namespace/queue/PVCs/max_gpus) come from the deploy profile, not here.
    /// `[measure.probe].script` names a pack file; its text rides the JSON as `script_text`.
    pub fn broker_env(&self, pack_dir: &std::path::Path) -> Result<Vec<(String, String)>, String> {
        let mut tools = self.tools.clone();
        if let Some(probe) = tools.get_mut("probe").and_then(toml::Value::as_table_mut)
            && let Some(script) = probe.remove("script")
        {
            const RELATIVE: &str = "[measure.probe].script must be a path inside the pack";
            let rel = std::path::Path::new(script.as_str().ok_or(RELATIVE)?);
            if !rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err(format!("{RELATIVE}, got {}", rel.display()));
            }
            let path = pack_dir.join(rel);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading [measure.probe].script {}: {e}", path.display()))?;
            probe.insert("script_text".to_string(), toml::Value::String(text));
        }
        let json = serde_json::to_string(&tools).map_err(|e| {
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

        let env = cfg
            .broker_env(std::path::Path::new("."))
            .expect("projects env");
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

    #[test]
    fn a_probe_script_is_read_from_inside_the_pack_and_rides_as_text() {
        let dir = std::env::temp_dir().join(format!("measure-probe-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/probe.py"), "print('probe')\n").unwrap();
        let cfg = |script: &str| -> MeasureCfg {
            toml::from_str(&format!(
                "gpus = 1\n[probe]\ncommand = \"python3 -c \\\"$CRUCIBLE_PROBE_SCRIPT\\\"\"\nscript = {script:?}\n"
            ))
            .unwrap()
        };
        let env = cfg("tools/probe.py").broker_env(&dir).unwrap();
        let json: serde_json::Value = serde_json::from_str(&env[1].1).unwrap();
        assert_eq!(json["probe"]["script_text"], "print('probe')\n");
        assert!(json["probe"].get("script").is_none());
        for escape in ["../outside.py", "/etc/passwd"] {
            let err = cfg(escape).broker_env(&dir).unwrap_err();
            assert!(err.contains("inside the pack"), "{escape}: {err}");
        }
        let _ = std::fs::remove_file(dir.join("tools/probe.py"));
    }
}
