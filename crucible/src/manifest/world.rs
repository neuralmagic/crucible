use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorldCfg {
    #[serde(default)]
    pub apply_cmd: Option<String>,
    #[serde(default)]
    pub snapshot_cmd: Option<String>,
    #[serde(default)]
    pub restore_cmd: Option<String>,
    /// Variables of the engine's environment these commands receive besides the base.
    #[serde(default)]
    pub env: Vec<crate::host_env::EnvName>,
}
