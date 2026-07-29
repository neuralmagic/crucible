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
}
