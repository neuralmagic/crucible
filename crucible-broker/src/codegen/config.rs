//! Tool-contract config: manifest defaults ∘ scenario overlay → frozen [`ToolsConfig`].
//! `mutable_kwargs` declares the only parameters the agent may vary, with their domains.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildMode {
    Derive,
    Full,
}

impl BuildMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BuildMode::Derive => "derive",
            BuildMode::Full => "full",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "derive" => Some(BuildMode::Derive),
            "full" => Some(BuildMode::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Lower,
    Higher,
}

/// The metric the gate optimizes, echoed into every measure result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Objective {
    pub(crate) key: String,
    pub(crate) direction: Direction,
}

/// Inclusive domain of a bounded-int kwarg, plus the value used when the agent omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntDomain {
    min: u32,
    max: u32,
    default: u32,
}

impl IntDomain {
    fn validate(&self, name: &str, v: Option<u32>) -> Result<u32, String> {
        match v {
            None => Ok(self.default),
            Some(n) if (self.min..=self.max).contains(&n) => Ok(n),
            Some(n) => Err(format!(
                "{name}={n} is out of its allowed range {}..={}",
                self.min, self.max
            )),
        }
    }
}

// ---- resolved (frozen) config ----

#[derive(Debug, Clone, PartialEq)]
pub struct ToolsConfig {
    /// The workload's hardware profile (TP size). Request AND limit in the rendered job.
    pub(crate) gpus: u32,
    pub(crate) build: BuildCfg,
    pub(crate) benchmark: BenchCfg,
    pub(crate) lm_eval: LmEvalCfg,
    /// Optional: only deployments whose scenario declares `[tools.profile]` get the `codegen_profile` tool.
    /// `None` = the tool answers with a readable rejection instead of failing config load.
    pub(crate) profile: Option<ProfileCfg>,
}

/// `Hash` participates in the build memo key: every field that shapes the produced image (or could)
/// must feed the fingerprint, so a config change can never replay a stale digest.
#[derive(Debug, Clone, PartialEq, Hash)]
pub struct BuildCfg {
    pub base_image: String,
    pub src_dir: String,
    pub install_cmd: String,
    pub full_install_cmd: Option<String>,
    /// `COPY --chown=<value>` owner for the copied source (e.g. "1000:1000"), so an editable install
    /// running as the base image's non-root USER can write into the tree. Unset = plain COPY.
    pub copy_chown: Option<String>,
    pub mutable_kwargs: BuildMutableKwargs,
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub struct BuildMutableKwargs {
    pub mode: Vec<BuildMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BenchCfg {
    pub command: String,
    pub output_len: u64,
    pub num_prompts: u64,
    pub objective: Objective,
    pub mutable_kwargs: BenchmarkMutableKwargs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BenchmarkMutableKwargs {
    /// Toggle env-name → the values the agent may set it to. Name AND value must be declared.
    pub toggles: BTreeMap<String, Vec<String>>,
    /// `None` = the kwarg is not declared, so passing it is rejected.
    pub reps: Option<IntDomain>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LmEvalCfg {
    pub command: String,
    pub objective: Objective,
    pub mutable_kwargs: LmEvalMutableKwargs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LmEvalMutableKwargs {
    pub limit: Option<IntDomain>,
}

/// A GPU capture run: the frozen command writes its trace artifact to `$OUT`; `trace_ext` is the
/// artifact's extension, preserved in the returned handle name so a downstream analyzer keys on it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileCfg {
    pub(crate) command: String,
    pub(crate) trace_ext: String,
}

impl BuildCfg {
    pub fn parse_mode(&self, mode: &str) -> Result<BuildMode, String> {
        let m = BuildMode::parse(mode)
            .ok_or_else(|| format!("mode {mode:?} is not one of: derive, full"))?;
        if self.mutable_kwargs.mode.contains(&m) {
            Ok(m)
        } else {
            Err(format!(
                "mode {mode:?} is not allowed for this scenario (allowed: {})",
                self.mutable_kwargs
                    .mode
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    pub fn install_for(&self, mode: BuildMode) -> Result<String, String> {
        match mode {
            BuildMode::Derive => Ok(self.install_cmd.clone()),
            BuildMode::Full => self.full_install_cmd.clone().ok_or_else(|| {
                "mode=full requires a configured full_install_cmd, which this scenario lacks"
                    .to_string()
            }),
        }
    }
}

impl BenchmarkMutableKwargs {
    pub fn validate_toggles(
        &self,
        toggles: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::with_capacity(toggles.len());
        for (name, value) in toggles {
            if !is_env_name(name) {
                return Err(format!(
                    "toggle name {name:?} is not a legal env identifier (A-Z, 0-9, '_')"
                ));
            }
            let Some(domain) = self.toggles.get(name) else {
                return Err(format!(
                    "toggle {name:?} is not declared for this scenario (declared: {})",
                    declared_names(&self.toggles)
                ));
            };
            if !domain.iter().any(|v| v == value) {
                return Err(format!(
                    "toggle {name:?} value {value:?} is out of its declared domain [{}]",
                    domain.join(", ")
                ));
            }
            out.push((name.clone(), value.clone()));
        }
        Ok(out)
    }
}

/// Validate a bounded-int kwarg against its declared domain. Undeclared + passed = rejected;
/// undeclared + omitted = `None` (the frozen command's own default applies).
pub fn resolve_int_kwarg(
    name: &str,
    v: Option<u32>,
    domain: Option<&IntDomain>,
) -> Result<Option<u32>, String> {
    match (domain, v) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(format!(
            "{name} is not a declared mutable kwarg for this scenario"
        )),
        (Some(d), v) => d.validate(name, v).map(Some),
    }
}

fn declared_names(domains: &BTreeMap<String, Vec<String>>) -> String {
    if domains.is_empty() {
        "none".to_string()
    } else {
        domains.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

// ---- overlay (partial) config ----

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsOverlay {
    gpus: Option<u32>,
    build: BuildOverlay,
    benchmark: BenchOverlay,
    lm_eval: LmEvalOverlay,
    /// `Option` (not a plain default struct) so an ABSENT section stays distinguishable from a
    /// present-but-empty one: absent ⇒ the tool is unconfigured, present ⇒ `command` is required.
    profile: Option<ProfileOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildOverlay {
    pub base_image: Option<String>,
    pub src_dir: Option<String>,
    pub install_cmd: Option<String>,
    pub full_install_cmd: Option<String>,
    pub copy_chown: Option<String>,
    pub mutable_kwargs: BuildMutableKwargsOverlay,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildMutableKwargsOverlay {
    pub mode: Option<Vec<BuildMode>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchOverlay {
    pub command: Option<String>,
    pub output_len: Option<u64>,
    pub num_prompts: Option<u64>,
    pub objective: Option<Objective>,
    pub mutable_kwargs: BenchMutableKwargsOverlay,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BenchMutableKwargsOverlay {
    pub toggles: Option<BTreeMap<String, Vec<String>>>,
    pub reps: Option<IntDomainOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LmEvalOverlay {
    pub command: Option<String>,
    pub objective: Option<Objective>,
    pub mutable_kwargs: LmEvalMutableKwargsOverlay,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LmEvalMutableKwargsOverlay {
    pub limit: Option<IntDomainOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileOverlay {
    pub command: Option<String>,
    pub trace_ext: Option<String>,
}

impl ProfileOverlay {
    fn merge(self, base: ProfileOverlay) -> ProfileOverlay {
        ProfileOverlay {
            command: self.command.or(base.command),
            trace_ext: self.trace_ext.or(base.trace_ext),
        }
    }
}

/// Declaring the kwarg with `{}` takes the per-kwarg default bounds; fields override them.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntDomainOverlay {
    pub min: Option<u32>,
    pub max: Option<u32>,
    pub default: Option<u32>,
}

impl IntDomainOverlay {
    fn resolve(self, fallback: IntDomain, field: &str) -> Result<IntDomain, String> {
        let min = self.min.unwrap_or(fallback.min);
        let max = self.max.unwrap_or(fallback.max);
        if min > max {
            return Err(format!("{field}: min {min} exceeds max {max}"));
        }
        let default = self.default.unwrap_or(fallback.default).clamp(min, max);
        Ok(IntDomain { min, max, default })
    }

    fn merge(self, base: Option<IntDomainOverlay>) -> IntDomainOverlay {
        let base = base.unwrap_or_default();
        IntDomainOverlay {
            min: self.min.or(base.min),
            max: self.max.or(base.max),
            default: self.default.or(base.default),
        }
    }
}

const REPS_DEFAULT: IntDomain = IntDomain {
    min: 1,
    max: 3,
    default: 1,
};
const LIMIT_DEFAULT: IntDomain = IntDomain {
    min: 32,
    max: 500,
    default: 500,
};

impl ToolsOverlay {
    /// Overlay wins per field; `base` fills the gaps.
    pub(crate) fn merge(self, base: ToolsOverlay) -> ToolsOverlay {
        ToolsOverlay {
            gpus: self.gpus.or(base.gpus),
            build: BuildOverlay {
                base_image: self.build.base_image.or(base.build.base_image),
                src_dir: self.build.src_dir.or(base.build.src_dir),
                install_cmd: self.build.install_cmd.or(base.build.install_cmd),
                full_install_cmd: self.build.full_install_cmd.or(base.build.full_install_cmd),
                copy_chown: self.build.copy_chown.or(base.build.copy_chown),
                mutable_kwargs: BuildMutableKwargsOverlay {
                    mode: self
                        .build
                        .mutable_kwargs
                        .mode
                        .or(base.build.mutable_kwargs.mode),
                },
            },
            benchmark: BenchOverlay {
                command: self.benchmark.command.or(base.benchmark.command),
                output_len: self.benchmark.output_len.or(base.benchmark.output_len),
                num_prompts: self.benchmark.num_prompts.or(base.benchmark.num_prompts),
                objective: self.benchmark.objective.or(base.benchmark.objective),
                mutable_kwargs: BenchMutableKwargsOverlay {
                    toggles: self
                        .benchmark
                        .mutable_kwargs
                        .toggles
                        .or(base.benchmark.mutable_kwargs.toggles),
                    reps: match self.benchmark.mutable_kwargs.reps {
                        Some(o) => Some(o.merge(base.benchmark.mutable_kwargs.reps)),
                        None => base.benchmark.mutable_kwargs.reps,
                    },
                },
            },
            lm_eval: LmEvalOverlay {
                command: self.lm_eval.command.or(base.lm_eval.command),
                objective: self.lm_eval.objective.or(base.lm_eval.objective),
                mutable_kwargs: LmEvalMutableKwargsOverlay {
                    limit: match self.lm_eval.mutable_kwargs.limit {
                        Some(o) => Some(o.merge(base.lm_eval.mutable_kwargs.limit)),
                        None => base.lm_eval.mutable_kwargs.limit,
                    },
                },
            },
            profile: match (self.profile, base.profile) {
                (Some(o), Some(b)) => Some(o.merge(b)),
                (Some(o), None) => Some(o),
                (None, b) => b,
            },
        }
    }

    /// Resolve the frozen config. `max_gpus` is the substrate ceiling: a config asking for more is
    /// rejected here rather than at job submit.
    pub(crate) fn finalize(self, max_gpus: u32) -> Result<ToolsConfig, String> {
        let gpus = self
            .gpus
            .ok_or("gpus is required (the workload's hardware profile)")?;
        if gpus == 0 || gpus > max_gpus {
            return Err(format!(
                "gpus={gpus} is outside what this deployment admits (1..={max_gpus})"
            ));
        }
        const SRC_DIR_REQUIRED: &str =
            "build.src_dir is required (where the agent's tree lands in the image)";
        const INSTALL_CMD_REQUIRED: &str =
            "build.install_cmd is required (the derive-mode install command)";
        let build = BuildCfg {
            base_image: self
                .build
                .base_image
                .filter(|s| !s.trim().is_empty())
                .ok_or("build.base_image is required")?,
            src_dir: self
                .build
                .src_dir
                .filter(|s| !s.trim().is_empty())
                .ok_or(SRC_DIR_REQUIRED)?,
            install_cmd: self
                .build
                .install_cmd
                .filter(|s| !s.trim().is_empty())
                .ok_or(INSTALL_CMD_REQUIRED)?,
            full_install_cmd: self.build.full_install_cmd,
            copy_chown: self.build.copy_chown.filter(|s| !s.trim().is_empty()),
            mutable_kwargs: BuildMutableKwargs {
                mode: self
                    .build
                    .mutable_kwargs
                    .mode
                    .unwrap_or_else(|| vec![BuildMode::Derive]),
            },
        };
        let benchmark = BenchCfg {
            command: require_cmd(self.benchmark.command, "benchmark.command")?,
            output_len: self.benchmark.output_len.unwrap_or(1024),
            num_prompts: self.benchmark.num_prompts.unwrap_or(4),
            objective: self.benchmark.objective.unwrap_or(Objective {
                key: "tpot_ms".to_string(),
                direction: Direction::Lower,
            }),
            mutable_kwargs: BenchmarkMutableKwargs {
                toggles: self.benchmark.mutable_kwargs.toggles.unwrap_or_default(),
                reps: self
                    .benchmark
                    .mutable_kwargs
                    .reps
                    .map(|o| o.resolve(REPS_DEFAULT, "benchmark.mutable_kwargs.reps"))
                    .transpose()?,
            },
        };
        let lm_eval = LmEvalCfg {
            command: require_cmd(self.lm_eval.command, "lm_eval.command")?,
            objective: self.lm_eval.objective.unwrap_or(Objective {
                key: "score".to_string(),
                direction: Direction::Higher,
            }),
            mutable_kwargs: LmEvalMutableKwargs {
                limit: self
                    .lm_eval
                    .mutable_kwargs
                    .limit
                    .map(|o| o.resolve(LIMIT_DEFAULT, "lm_eval.mutable_kwargs.limit"))
                    .transpose()?,
            },
        };
        let profile = self.profile.map(finalize_profile).transpose()?;
        Ok(ToolsConfig {
            gpus,
            build,
            benchmark,
            lm_eval,
            profile,
        })
    }
}

/// A declared `[tools.profile]` section requires its frozen command; `trace_ext` defaults to a
/// gzipped-JSON trace and is normalized (leading dot stripped) so the handle name is clean.
fn finalize_profile(o: ProfileOverlay) -> Result<ProfileCfg, String> {
    let trace_ext = o
        .trace_ext
        .map(|s| s.trim().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "json.gz".to_string());
    if !trace_ext
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.')
    {
        return Err(format!(
            "profile.trace_ext {trace_ext:?} must be a bare extension (letters, digits, '.')"
        ));
    }
    Ok(ProfileCfg {
        command: require_cmd(o.command, "profile.command")?,
        trace_ext,
    })
}

fn require_cmd(v: Option<String>, field: &str) -> Result<String, String> {
    v.filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{field} is required (the frozen command is server-side)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(extra: serde_json::Value) -> ToolsOverlay {
        let mut base = serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install"},
            "benchmark": {"command": "c"},
            "lm_eval": {"command": "l"}
        });
        merge_json(&mut base, extra);
        serde_json::from_value(base).unwrap()
    }

    fn merge_json(base: &mut serde_json::Value, extra: serde_json::Value) {
        if let (Some(b), serde_json::Value::Object(e)) = (base.as_object_mut(), extra) {
            for (k, v) in e {
                match b.get_mut(&k) {
                    Some(slot) if slot.is_object() && v.is_object() => merge_json(slot, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
    }

    #[test]
    fn merge_lets_the_overlay_win_and_keeps_base_defaults() {
        let base: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "base-img", "src_dir": "/base-src", "install_cmd": "base-install", "copy_chown": "1000:1000"},
            "benchmark": {"command": "base-bench", "output_len": 512},
            "lm_eval": {"command": "base-lmeval"}
        }))
        .unwrap();
        let overlay: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "benchmark": {"output_len": 1024}
        }))
        .unwrap();
        let merged = overlay.merge(base).finalize(2).unwrap();
        assert_eq!(merged.benchmark.output_len, 1024);
        assert_eq!(merged.build.base_image, "base-img");
        assert_eq!(merged.build.install_cmd, "base-install");
        assert_eq!(merged.build.copy_chown.as_deref(), Some("1000:1000"));
        assert_eq!(merged.benchmark.command, "base-bench");
        assert_eq!(merged.gpus, 2);
    }

    #[test]
    fn merge_carries_mutable_kwargs_through_layers() {
        let base: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 1,
            "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install", "mutable_kwargs": {"mode": ["derive", "full"]}},
            "benchmark": {"command": "c", "mutable_kwargs": {"toggles": {"A_B": ["1"]}, "reps": {}}},
            "lm_eval": {"command": "l", "mutable_kwargs": {"limit": {"min": 50, "max": 100}}}
        }))
        .unwrap();
        let merged = ToolsOverlay::default().merge(base).finalize(2).unwrap();
        assert_eq!(
            merged.build.mutable_kwargs.mode,
            vec![BuildMode::Derive, BuildMode::Full]
        );
        assert!(merged.benchmark.mutable_kwargs.toggles.contains_key("A_B"));
        // Declared with {} => default bounds.
        assert_eq!(
            merged.benchmark.mutable_kwargs.reps,
            Some(IntDomain {
                min: 1,
                max: 3,
                default: 1
            })
        );
        // Declared bounds override the defaults; the default value clamps into them.
        let limit = merged.lm_eval.mutable_kwargs.limit.unwrap();
        assert_eq!((limit.min, limit.max, limit.default), (50, 100, 100));
    }

    #[test]
    fn finalize_defaults_and_undeclared_kwargs() {
        let cfg = minimal(serde_json::json!({})).finalize(2).unwrap();
        assert_eq!(cfg.benchmark.objective.direction, Direction::Lower);
        assert_eq!(cfg.benchmark.objective.key, "tpot_ms");
        assert_eq!(cfg.lm_eval.objective.direction, Direction::Higher);
        assert_eq!(cfg.build.mutable_kwargs.mode, vec![BuildMode::Derive]);
        assert_eq!(cfg.build.src_dir, "/src");
        // Kwargs not declared at all are not accepted.
        assert_eq!(cfg.benchmark.mutable_kwargs.reps, None);
        assert_eq!(cfg.lm_eval.mutable_kwargs.limit, None);
        assert!(resolve_int_kwarg("reps", Some(2), None).is_err());
        assert_eq!(resolve_int_kwarg("reps", None, None).unwrap(), None);
    }

    #[test]
    fn finalize_rejects_gpus_over_the_substrate_ceiling() {
        let err = minimal(serde_json::json!({"gpus": 4}))
            .finalize(2)
            .unwrap_err();
        assert!(err.contains("gpus=4"), "{err}");
        assert!(minimal(serde_json::json!({"gpus": 0})).finalize(2).is_err());
        assert!(minimal(serde_json::json!({"gpus": 2})).finalize(2).is_ok());
        // Missing gpus is a readable error rather than a default.
        let no_gpus: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install"},
            "benchmark": {"command": "c"},
            "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert!(no_gpus.finalize(2).unwrap_err().contains("gpus"));
    }

    #[test]
    fn finalize_errors_on_missing_required_fields() {
        let no_base: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2, "benchmark": {"command": "c"}, "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert!(no_base.finalize(2).unwrap_err().contains("base_image"));

        let no_bench_cmd: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2, "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install"}, "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert!(
            no_bench_cmd
                .finalize(2)
                .unwrap_err()
                .contains("benchmark.command")
        );

        // src_dir and install_cmd carry no baked-in defaults: each names its missing key.
        let no_src_dir: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2, "build": {"base_image": "b", "install_cmd": "install"},
            "benchmark": {"command": "c"}, "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert!(
            no_src_dir
                .finalize(2)
                .unwrap_err()
                .contains("build.src_dir")
        );

        let no_install: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2, "build": {"base_image": "b", "src_dir": "/src"},
            "benchmark": {"command": "c"}, "lm_eval": {"command": "l"}
        }))
        .unwrap();
        assert!(
            no_install
                .finalize(2)
                .unwrap_err()
                .contains("build.install_cmd")
        );
    }

    #[test]
    fn int_domain_bounds_validate() {
        let d = IntDomain {
            min: 1,
            max: 3,
            default: 1,
        };
        assert_eq!(d.validate("reps", None).unwrap(), 1);
        assert_eq!(d.validate("reps", Some(3)).unwrap(), 3);
        assert!(d.validate("reps", Some(0)).is_err());
        assert!(d.validate("reps", Some(4)).unwrap_err().contains("reps=4"));
        // min > max in config is a finalize-time error.
        let bad = minimal(serde_json::json!({
            "benchmark": {"mutable_kwargs": {"reps": {"min": 3, "max": 1}}}
        }))
        .finalize(2);
        assert!(bad.unwrap_err().contains("min 3 exceeds max 1"));
    }

    #[test]
    fn unknown_config_field_is_a_hard_error() {
        let bad: Result<ToolsOverlay, _> = serde_json::from_value(serde_json::json!({
            "benchmark": {"comand": "typo"}
        }));
        assert!(bad.is_err());
        // The pre-rename flat field must not silently no-op.
        let old_name: Result<ToolsOverlay, _> = serde_json::from_value(serde_json::json!({
            "benchmark": {"toggle_domains": {"A": ["1"]}}
        }));
        assert!(old_name.is_err());
    }

    #[test]
    fn full_mode_needs_a_compile_command() {
        let cfg = minimal(serde_json::json!({
            "build": {"mutable_kwargs": {"mode": ["derive", "full"]}}
        }))
        .finalize(2)
        .unwrap();
        assert!(cfg.build.parse_mode("full").is_ok());
        assert!(
            cfg.build
                .install_for(BuildMode::Full)
                .unwrap_err()
                .contains("full_install_cmd")
        );
        assert!(cfg.build.install_for(BuildMode::Derive).is_ok());
    }

    #[test]
    fn profile_is_optional_and_absent_by_default() {
        // No `[tools.profile]` section ⇒ the tool is unconfigured, NOT a config-load error.
        let cfg = minimal(serde_json::json!({})).finalize(2).unwrap();
        assert_eq!(cfg.profile, None);
    }

    #[test]
    fn profile_present_requires_a_command_and_defaults_the_ext() {
        let cfg = minimal(serde_json::json!({
            "profile": {"command": "capture --out \"$OUT\""}
        }))
        .finalize(2)
        .unwrap();
        let p = cfg.profile.unwrap();
        assert_eq!(p.command, "capture --out \"$OUT\"");
        assert_eq!(p.trace_ext, "json.gz");

        // A present-but-command-less section IS a hard config error (unlike an absent section).
        let err = minimal(serde_json::json!({"profile": {"trace_ext": "pb.gz"}}))
            .finalize(2)
            .unwrap_err();
        assert!(err.contains("profile.command"), "{err}");
    }

    #[test]
    fn profile_trace_ext_is_normalized_and_validated() {
        let cfg = minimal(serde_json::json!({
            "profile": {"command": "c", "trace_ext": ".pb.gz"}
        }))
        .finalize(2)
        .unwrap();
        assert_eq!(cfg.profile.unwrap().trace_ext, "pb.gz");
        // A path-y extension is rejected (it lands in a handle name and an $OUT path).
        let err = minimal(serde_json::json!({
            "profile": {"command": "c", "trace_ext": "../etc/x"}
        }))
        .finalize(2)
        .unwrap_err();
        assert!(err.contains("trace_ext"), "{err}");
    }

    #[test]
    fn profile_merges_across_layers() {
        let base: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "gpus": 2,
            "build": {"base_image": "b", "src_dir": "/src", "install_cmd": "install"},
            "benchmark": {"command": "c"},
            "lm_eval": {"command": "l"},
            "profile": {"command": "base-capture", "trace_ext": "json.gz"}
        }))
        .unwrap();
        // Overlay overrides just the command; the base ext survives.
        let overlay: ToolsOverlay = serde_json::from_value(serde_json::json!({
            "profile": {"command": "overlay-capture"}
        }))
        .unwrap();
        let merged = overlay.merge(base).finalize(2).unwrap().profile.unwrap();
        assert_eq!(merged.command, "overlay-capture");
        assert_eq!(merged.trace_ext, "json.gz");
    }

    #[test]
    fn is_env_name_guards_identifiers() {
        assert!(is_env_name("VLLM_USE_X"));
        assert!(!is_env_name("lowercase"));
        assert!(!is_env_name("1LEADING"));
        assert!(!is_env_name("has space"));
        assert!(!is_env_name(""));
    }

    /// The BROKER_CODEGEN_TOOLS_DEFAULTS example in docs/hand-rolled-pipelines.md must stay a
    /// config that finalize() actually accepts (a reader will paste it).
    #[test]
    fn doc_tools_defaults_example_passes_finalize() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/hand-rolled-pipelines.md");
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let marker = "BROKER_CODEGEN_TOOLS_DEFAULTS = ";
        let start = doc.find(marker).expect("doc example marker present") + marker.len();
        let rest = &doc[start..];
        let end = rest.find("```").expect("doc example closing fence");
        let overlay: ToolsOverlay =
            serde_json::from_str(rest[..end].trim()).expect("doc example parses");
        overlay.finalize(2).expect("doc example passes finalize");
    }
}
