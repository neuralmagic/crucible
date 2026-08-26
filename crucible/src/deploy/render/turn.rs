use crate::deploy::profile::DeployProfile;
use crate::deploy::render::kube::{
    FORGE_STORAGE_ROOT, INGEST_TOKEN_DIR, INGEST_TOKEN_TTL_SECS, INGEST_TOKEN_VOLUME,
    PULL_AUTHFILE_PATH, kubernetes_sandbox_env, node_avoid_affinity, resources, secret_env_vars,
};
use crate::deploy::render::{DigestResolver, pin_image};
use crate::openshell::gateway::ComputeDriver;
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1 as core;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[error("unknown --turn-kind `{got}` (expected `rank` or `scope`)")]
pub struct UnknownTurnKind {
    got: String,
}

/// What kind of one-shot turn a rendered turn pod runs. End-to-end strong type: the CLI parses it,
/// the renderer switches the wrapper command + label on it, and `WorkPodSpec::render_argv` round-trips
/// it back to the CLI flag the subprocess sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// A code-grounded triage-ranking turn (`crucible rank-grounded`).
    Rank,
    /// A scope-propose turn (`crucible scope --propose --json --force --marker`).
    Scope,
}

impl TurnKind {
    /// The `crucible.io/work-kind` label value the pod carries.
    pub fn label_value(self) -> &'static str {
        match self {
            TurnKind::Rank => GROUNDED_RANK_WORK_KIND,
            TurnKind::Scope => SCOPE_WORK_KIND,
        }
    }

    /// Parse the `--turn-kind` CLI flag value.
    pub fn parse_cli(s: &str) -> Result<Self> {
        match s {
            "rank" => Ok(TurnKind::Rank),
            "scope" => Ok(TurnKind::Scope),
            other => Err(UnknownTurnKind {
                got: other.to_owned(),
            }
            .into()),
        }
    }
}

/// The confirmed tier a propose turn drafts against: threaded from the controller's ranker verdict
/// (`--tier t0|t1`) into the prompt's `{{TIER}}` slot, so the agent follows the right section of
/// `scope-propose.md` instead of guessing.
/// `T0` is the engine default when the flag is absent, every call site predating this flag never set
/// it, and a bare `crucible scope --propose` (no controller in front of it) should keep behaving exactly
/// as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ProposeTier {
    #[default]
    #[value(name = "t0")]
    T0,
    #[value(name = "t1")]
    T1,
}

impl ProposeTier {
    /// The `{{TIER}}` spelling the prompt substitutes, matches the ranker/DB vocabulary
    /// (`Tier::as_str` in the controller crate) so a human reading the rendered prompt recognizes
    /// it as the same tier the ledger shows.
    pub fn as_str(self) -> &'static str {
        match self {
            ProposeTier::T0 => "T0",
            ProposeTier::T1 => "T1",
        }
    }

    /// The `--tier` CLI spelling (the clap `#[value(name)]`s above), what the turn-pod wrapper
    /// renders into its `crucible scope --propose --tier …` invocation.
    pub fn cli_value(self) -> &'static str {
        match self {
            ProposeTier::T0 => "t0",
            ProposeTier::T1 => "t1",
        }
    }
}

/// Options for [`render_turn`]: a single one-shot agent-turn pod (WorkPod
/// primitive). Unlike the loop pod it runs no manifest, the wrapper clones `repo_url`, then runs
/// the kind-specific command over that checkout and prints the marker its logs are scraped for.
/// [`TurnOpts::new`] takes the fields every turn needs; the rest default to "no flag".
#[derive(Clone)]
pub struct TurnOpts {
    /// What the turn pod does (rank vs scope), governing the wrapper command + the work-kind label.
    pub kind: TurnKind,
    /// The pod's k8s object name (the controller supplies it so its `work_pods` row + ownerRef
    /// stamping key on a name it chose).
    pub name: String,
    /// The issue to rank/scope, `owner/repo#N`. Ignored by a scope turn when `goal_text` is set.
    pub issue: String,
    /// A non-upstream scenario's ledgered free-text goal (no GitHub item to fetch): rendered into
    /// the scope wrapper as a base64-decoded `goal.md` + `--goal-file`, instead of `--issue`. `None`
    /// (and every rank turn, which is always upstream) keeps the `--issue` spelling.
    pub goal_text: Option<String>,
    /// The clone URL of the repo under test (the wrapper clones it fresh into the pod).
    pub repo_url: String,
    /// Branch or tag the wrapper clones at. `None` is the repo's default branch.
    pub repo_ref: Option<String>,
    /// The agent sandbox image carrying the claude CLI the loop/crucible image does not (the
    /// `openshell` backend pulls it via `REGISTRY_AUTH_FILE` pointing at the mounted authfile).
    pub sandbox_image: String,
    /// Cap on the turn's cost in USD.
    pub max_cost: f64,
    /// Resolve image tags to `@sha256:…` through this resolver; `None` for an air-gapped render.
    pub digests: Option<Arc<dyn DigestResolver>>,
    /// The issue's confirmed tier (The confirmed tier), rendered into the scope wrapper's
    /// `crucible scope --propose --tier …`. `None` (or a rank turn) emits no flag, the engine
    /// defaults to t0.
    pub tier: Option<ProposeTier>,
    /// Max gaming-review concern→refine→re-review cycles, rendered into the scope wrapper's
    /// `crucible scope --propose --gaming-refine-rounds …`. Ignored by a rank turn.
    pub gaming_refine_rounds: u32,
    /// Skip the adversarial gaming review entirely, rendered into the scope wrapper as
    /// `crucible scope --propose --skip-gaming-review` instead of `--gaming-refine-rounds`, an
    /// operator escape hatch for demo/bring-up postures where the review's fail-closed loop blocks
    /// the first e2e run through a new deployment. Ignored by a rank turn.
    pub skip_gaming_review: bool,
    /// The goal is an authoritative brief, rendered into the scope wrapper as
    /// `crucible scope --propose --authoritative`. Ignored by a rank turn.
    pub authoritative: bool,
    /// The agent harness the in-pod turn runs, rendered as `--harness <h>` into both wrapper
    /// commands. `None` emits no flag, the in-pod engine keeps its manifest/default harness.
    pub harness: Option<crate::manifest::Harness>,
    /// The model the in-pod turn runs, rendered as `--model <m>` into both wrapper commands.
    /// `None` emits no flag, the in-pod engine derives the model from the resolved harness.
    pub model: Option<String>,
    /// A pack the checkout already carries, relative to its root. Set, a scope turn validates and
    /// freezes that pack (`crucible scope --pack`) instead of drafting one, which spends no agent
    /// turn and needs no sandbox. Ignored by a rank turn.
    pub pack_path: Option<PackPath>,
}

/// A pack directory inside the cloned checkout, validated on construction because it is
/// concatenated into the wrapper script's `--pack "$CHECKOUT/<path>"`.
///
/// Relative, no traversal, no shell metacharacters: the wrapper interpolates it into a
/// double-quoted word, so a `"` or `$` would break out of it and anything absolute or climbing
/// would leave the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPath(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PackPathError {
    #[error("pack path must be non-empty")]
    Empty,
    #[error("pack path must be relative to the checkout, got {got:?}")]
    Absolute { got: String },
    #[error("pack path must not contain '..', got {got:?}")]
    Traversal { got: String },
    #[error(
        "pack path may only contain ASCII letters, digits, '.', '_', '-', and '/', got {got:?}"
    )]
    Charset { got: String },
}

impl PackPath {
    pub fn parse(raw: &str) -> Result<Self, PackPathError> {
        let path = raw.trim().trim_end_matches('/');
        if path.is_empty() {
            return Err(PackPathError::Empty);
        }
        if path.starts_with('/') {
            return Err(PackPathError::Absolute {
                got: raw.to_owned(),
            });
        }
        if path.split('/').any(|segment| segment == "..") {
            return Err(PackPathError::Traversal {
                got: raw.to_owned(),
            });
        }
        if !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        {
            return Err(PackPathError::Charset {
                got: raw.to_owned(),
            });
        }
        Ok(PackPath(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PackPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TurnOpts {
    pub fn new(
        kind: TurnKind,
        name: impl Into<String>,
        issue: impl Into<String>,
        repo_url: impl Into<String>,
        sandbox_image: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            issue: issue.into(),
            goal_text: None,
            repo_url: repo_url.into(),
            repo_ref: None,
            sandbox_image: sandbox_image.into(),
            max_cost: 0.0,
            digests: None,
            tier: None,
            gaming_refine_rounds: 0,
            skip_gaming_review: false,
            authoritative: false,
            harness: None,
            model: None,
            pack_path: None,
        }
    }
}

/// Render an optional `--harness <h>` wrapper flag. The value is clap's own `ValueEnum` name, so
/// the string the wrapper emits is by construction the one the in-pod CLI parses back.
pub(super) fn harness_flag(harness: Option<crate::manifest::Harness>, sep: char) -> String {
    use clap::ValueEnum as _;
    harness
        .and_then(|h| h.to_possible_value())
        .map(|v| format!(" --harness{sep}{}", v.get_name()))
        .unwrap_or_default()
}

/// Render an optional `--model <m>` wrapper flag.
pub(super) fn model_flag(model: Option<&String>, sep: char) -> String {
    model
        .map(|m| format!(" --model{sep}{m}"))
        .unwrap_or_default()
}

/// The `crucible.io/work-kind` label value a grounded-rank turn pod carries, the selector a
/// controller sweep reconciles its `work_pods` rows against.
pub const GROUNDED_RANK_WORK_KIND: &str = "grounded-rank";

/// The `crucible.io/work-kind` label value a scope turn pod carries.
pub const SCOPE_WORK_KIND: &str = "scope";

/// Render one turn `Pod` (WorkPod primitive): the library form of `crucible deploy render-turn`,
/// which reads `--goal-file` into [`TurnOpts::goal_text`]. Image pinning happens through
/// [`TurnOpts::digests`] or not at all.
///
/// The same security/auth scaffolding as the loop pod, privileged only under the podman driver
/// ([`crate::deploy::render::kube::agent_security_context`]), the pull secret, the run-as service
/// account, the istio-inject-off annotation, `restartPolicy: Never`, and the `REGISTRY_AUTH_FILE`
/// env, but no broker/deploy env, no kube RBAC mounts, and no NetworkPolicy: the turn only clones
/// a repo and runs one read-only ranking turn. `automountServiceAccountToken` follows the sandbox
/// driver: `false` under podman (no API calls), `true` under kubernetes (the in-pod gateway needs
/// the token to reach the API server for Sandbox CRs).
#[derive(Debug, thiserror::Error)]
#[error("--repo-ref {got:?} is not a plain branch or tag name")]
pub struct BadRepoRef {
    got: String,
}

/// A ref that is safe to interpolate unquoted into the wrapper script: git's own ref grammar minus
/// anything the shell would read (whitespace, quotes, `$`, glob characters), and never a leading
/// `-`, so it cannot be mistaken for another `git clone` flag.
fn checked_git_ref(r: &str) -> Result<&str, BadRepoRef> {
    let ok = !r.is_empty()
        && !r.starts_with('-')
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '@' | '+'));
    if !ok {
        return Err(BadRepoRef { got: r.to_string() });
    }
    Ok(r)
}

#[tracing::instrument(skip_all, fields(turn_kind = ?opts.kind, issue = %opts.issue, pinned = opts.digests.is_some()), err)]
pub fn render_turn(profile: &DeployProfile, opts: &TurnOpts) -> Result<String> {
    let image = pin_image(
        opts.digests.as_deref(),
        "loop image",
        &profile.image.loop_image,
    )?;
    let sandbox_image = pin_image(
        opts.digests.as_deref(),
        "turn sandbox image",
        &opts.sandbox_image,
    )?;

    let mut env = secret_env_vars(profile);
    let plain = |name: &str, value: String| core::EnvVar {
        name: name.to_string(),
        value: Some(value),
        value_from: None,
    };
    env.push(plain(
        "OPENSHELL_SUPERVISOR_IMAGE",
        profile.cluster.supervisor_image.clone(),
    ));
    env.push(plain("FORGE_STORAGE_ROOT", FORGE_STORAGE_ROOT.to_string()));
    // See `Renderer::env`: podman reads REGISTRY_AUTH_FILE ahead of its own lookup path, so the
    // nested podman pulls a private sandbox without a `podman login` shell-out. Only when mounted.
    if profile.secrets.pull_authfile.is_some() {
        env.push(plain("REGISTRY_AUTH_FILE", PULL_AUTHFILE_PATH.to_string()));
    }
    // Under the kubernetes driver, project the config the runtime `gateway_toml()` reads to build
    // the `[openshell.drivers.kubernetes]` block. Without this the turn/scope pod's gateway boots
    // with the podman driver's default (see the `--compute-driver` flag below), nests a pull of
    // `sandbox_image` through the authfile path instead of the kubelet's `imagePullSecrets`, and
    // never creates a `Sandbox` CR.
    if profile.cluster.sandbox_driver == ComputeDriver::Kubernetes {
        env.extend(kubernetes_sandbox_env(profile, &sandbox_image));
    }
    // The GitHub / gate / Vertex-project env the profile carries (rank-grounded fetches the issue
    // from the GitHub API; the backend needs its Vertex project/region). Generic, names are the
    // profile's, none baked in here. This plain env is how the Vertex vars reach a manifest-less
    // turn: no `[agent].env` exists, so the engine relays them from its own process env
    // (`openshell::relay_vertex_env`).
    for (k, v) in &profile.env {
        env.push(plain(k, v.clone()));
    }

    // Tier 2 ingest: when the profile names the controller's drop-box URL, tell the turn
    // where to POST its large artifacts and where its projected `crucible-ingest`-audience token is.
    // Absent = no drop-box; the engine falls back to marker emission. The token volume itself is
    // added below, it's a separate, audience-locked file, independent of the API-server automount
    // above.
    if let Some(ingest_url) = &profile.cluster.ingest_url {
        env.push(plain(crucible_contract::ENV_INGEST_URL, ingest_url.clone()));
        env.push(plain(
            crucible_contract::ENV_INGEST_TOKEN_PATH,
            format!("{INGEST_TOKEN_DIR}/token"),
        ));
        // The pod learns its own name from the downward API, so the `{pod}` path segment it POSTs to
        // equals the token's bound-pod claim by construction (pod-binding = turn-scoping). The uid
        // rides along so the in-pod gateway can own the objects it publishes, which is what gets
        // them garbage-collected with the turn.
        let downward = |name: &str, field_path: &str| core::EnvVar {
            name: name.to_string(),
            value: None,
            value_from: Some(core::EnvVarSource {
                field_ref: Some(core::ObjectFieldSelector {
                    field_path: field_path.to_string(),
                    api_version: None,
                }),
                ..Default::default()
            }),
        };
        env.push(downward(crucible_contract::ENV_POD_NAME, "metadata.name"));
        env.push(downward("CRUCIBLE_POD_UID", "metadata.uid"));
    }

    let TurnOpts {
        kind,
        issue,
        goal_text,
        repo_url,
        repo_ref,
        max_cost,
        tier,
        gaming_refine_rounds,
        skip_gaming_review,
        authoritative,
        harness,
        model,
        pack_path,
        ..
    } = opts;
    let harness_flag = harness_flag(*harness, ' ');
    let model_flag = model_flag(model.as_ref(), ' ');
    // Same flag the loop wrapper passes (`Renderer::wrapper_script`): without it the turn's
    // subcommand synthesizes a fresh `Args` (`Cli::parse_from(["crucible"])`) that always defaults
    // to the podman compute driver, so the gateway resolves `sandbox_image` through the nested
    // podman/authfile path instead of the kubelet's `imagePullSecrets` and never creates a Sandbox.
    let compute_driver_flag = match profile.cluster.sandbox_driver {
        ComputeDriver::Kubernetes => " --compute-driver=kubernetes",
        ComputeDriver::Podman => "",
    };
    let clone_ref = match repo_ref.as_deref() {
        Some(r) => format!(" --branch {}", checked_git_ref(r)?),
        None => String::new(),
    };
    let wrapper = match kind {
        TurnKind::Rank => format!(
            r#"set -e
CHECKOUT=/tmp/crucible-turn-checkout
rm -rf "$CHECKOUT"
git clone --depth 50{clone_ref} {repo_url} "$CHECKOUT"
crucible rank-grounded --issue {issue} --workspace "$CHECKOUT" --max-cost {max_cost}{harness_flag}{model_flag} \
  --json --marker --agent-backend openshell --sandbox-image {sandbox_image}{compute_driver_flag}
"#
        ),
        // A pack the repo already carries needs validating and freezing, not drafting: no agent,
        // no sandbox, no goal, and none of the propose turn's tier/gaming/authoritative knobs,
        // which all describe how a pack gets written.
        TurnKind::Scope if pack_path.is_some() => {
            let pack = pack_path.as_ref().map(PackPath::as_str).unwrap_or_default();
            format!(
                r#"set -e
CHECKOUT=/tmp/crucible-turn-checkout
rm -rf "$CHECKOUT"
git clone --depth 50{clone_ref} {repo_url} "$CHECKOUT"
crucible scope --pack "$CHECKOUT/{pack}" --json --force --marker
"#
            )
        }
        TurnKind::Scope => {
            let tier_flag = tier
                .map(|t| format!(" --tier {}", t.cli_value()))
                .unwrap_or_default();
            let gaming_flag = if *skip_gaming_review {
                " --skip-gaming-review".to_string()
            } else {
                format!(" --gaming-refine-rounds {gaming_refine_rounds}")
            };
            let authoritative_flag = if *authoritative {
                " --authoritative"
            } else {
                ""
            };
            // A non-upstream scenario has no GitHub item to fetch: its goal is the free text
            // ledgered at adoption. Base64-decode it into a file inside the pod and pass
            // `--goal-file` (which `--issue` conflicts with at the CLI), the same local-file
            // `Ingest` arm the non-pod executor uses (`engine::scope_propose`), just with the write
            // happening in-pod since the goal text can't ride a local path into a remote pod.
            let goal_source = match goal_text {
                Some(text) => {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                    format!(
                        "GOAL_FILE=/tmp/crucible-scope-goal.md\n\
                         echo {encoded} | base64 -d > \"$GOAL_FILE\"\n"
                    )
                }
                None => String::new(),
            };
            let goal_flag = if goal_text.is_some() {
                "--goal-file \"$GOAL_FILE\""
            } else {
                "--issue"
            };
            let goal_arg = if goal_text.is_some() {
                String::new()
            } else {
                format!(" {issue}")
            };
            format!(
                r#"set -e
CHECKOUT=/tmp/crucible-turn-checkout
rm -rf "$CHECKOUT"
git clone --depth 50{clone_ref} {repo_url} "$CHECKOUT"
SCOPE_OUT=/tmp/crucible-scope-out
rm -rf "$SCOPE_OUT"
{goal_source}crucible scope --propose --json --force --marker {goal_flag}{goal_arg} \
  --repo "$CHECKOUT" --out "$SCOPE_OUT" --max-cost {max_cost}{tier_flag}{gaming_flag}{authoritative_flag}{harness_flag}{model_flag} \
  --agent-backend openshell --sandbox-image {sandbox_image}{compute_driver_flag}
"#
            )
        }
    };

    let mut mounts = Vec::new();
    let mut volumes = Vec::new();
    if let Some(secret) = &profile.secrets.pull_authfile {
        mounts.push(core::VolumeMount {
            name: "quay-auth".to_string(),
            mount_path: PULL_AUTHFILE_PATH.to_string(),
            sub_path: Some("auth.json".to_string()),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(core::Volume {
            name: "quay-auth".to_string(),
            secret: Some(core::SecretVolumeSource {
                secret_name: Some(secret.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    // Ephemeral per-turn podman storage (the nested openshell sandbox writes here).
    mounts.push(core::VolumeMount {
        name: "forge-storage".to_string(),
        mount_path: FORGE_STORAGE_ROOT.to_string(),
        ..Default::default()
    });
    volumes.push(core::Volume {
        name: "forge-storage".to_string(),
        empty_dir: Some(core::EmptyDirVolumeSource::default()),
        ..Default::default()
    });

    // Tier 2 ingest token (Tier 2 ingest): a projected SA token audience'd to the ingest endpoint only,
    // useless against the kube API or any other service. Mounted read-only; the engine reads
    // `<dir>/token` and sends it as the bearer. Added only when the drop-box URL is configured.
    if profile.cluster.ingest_url.is_some() {
        mounts.push(core::VolumeMount {
            name: INGEST_TOKEN_VOLUME.to_string(),
            mount_path: INGEST_TOKEN_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(core::Volume {
            name: INGEST_TOKEN_VOLUME.to_string(),
            projected: Some(core::ProjectedVolumeSource {
                sources: Some(vec![core::VolumeProjection {
                    service_account_token: Some(core::ServiceAccountTokenProjection {
                        audience: Some(crucible_contract::INGEST_TOKEN_AUDIENCE.to_string()),
                        expiration_seconds: Some(INGEST_TOKEN_TTL_SECS),
                        path: "token".to_string(),
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    let container = core::Container {
        name: "turn".to_string(),
        image: Some(image),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
        args: Some(vec![wrapper]),
        security_context: crate::deploy::render::kube::agent_security_context(
            profile.cluster.sandbox_driver,
        ),
        env: Some(env),
        volume_mounts: Some(mounts),
        resources: Some(resources(profile)),
        ..Default::default()
    };

    let pod = core::Pod {
        metadata: ObjectMeta {
            name: Some(opts.name.clone()),
            namespace: Some(profile.cluster.loop_namespace.clone()),
            annotations: Some(BTreeMap::from([(
                "sidecar.istio.io/inject".to_string(),
                "false".to_string(),
            )])),
            labels: Some(BTreeMap::from([
                (
                    crucible_contract::MANAGED_BY_KEY.to_string(),
                    crucible_contract::MANAGED_BY_VALUE.to_string(),
                ),
                (
                    "crucible.io/work-kind".to_string(),
                    opts.kind.label_value().to_string(),
                ),
            ])),
            ..Default::default()
        },
        spec: Some(core::PodSpec {
            image_pull_secrets: Some(vec![core::LocalObjectReference {
                name: profile.image.pull_secret.clone(),
            }]),
            service_account_name: Some(profile.cluster.service_account.clone()),
            // Under kubernetes the in-pod gateway calls the API server directly (Sandbox CRs) and has
            // no mounted kubeconfig like the loop pod does, so it needs the automounted token; podman
            // never touches the API and stays tokenless.
            automount_service_account_token: Some(
                profile.cluster.sandbox_driver == ComputeDriver::Kubernetes,
            ),
            restart_policy: Some("Never".to_string()),
            affinity: node_avoid_affinity(profile),
            containers: vec![container],
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    };

    serde_norway::to_string(&pod).context("serializing the turn pod")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grounded-rank turn pod renders the reduced agent-turn shape: same security/auth scaffolding
    /// as the loop pod (privileged under the default podman driver, pull secret, run-as SA with token automount off, istio off,
    /// restartPolicy Never, `REGISTRY_AUTH_FILE` env, the Vertex ADC secret env), but no broker/deploy
    /// env, no kube RBAC mounts, and no NetworkPolicy, the turn just clones and ranks.
    #[test]
    fn turn_pod_renders_the_reduced_agent_shape() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
            [secrets]
            pull_authfile = "example-pull"
            [[secret_env]]
            name = "GCLOUD_CREDENTIALS"
            secret = "gcloud-adc"
            key = "adc.json"
            [env]
            GITHUB_TOKEN = "unused-in-render"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Rank,
                name: "crucible-turn-owner-repo-42-abcd".to_string(),
                issue: "owner/repo#42".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 5.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render turn");

        assert!(yaml.contains("kind: Pod"));
        assert!(yaml.contains("name: crucible-turn-owner-repo-42-abcd"));
        assert!(yaml.contains("namespace: autoresearch"));
        // The security/auth scaffolding shared with the loop pod.
        assert!(yaml.contains("privileged: true"));
        assert!(yaml.contains("automountServiceAccountToken: false"));
        assert!(yaml.contains("restartPolicy: Never"));
        assert!(yaml.contains("sidecar.istio.io/inject: 'false'"));
        assert!(yaml.contains("name: example-pull"), "pull secret");
        assert!(yaml.contains("serviceAccountName: autoresearch-publisher"));
        // The Vertex ADC secret env the caller mints its token from.
        assert!(yaml.contains("name: GCLOUD_CREDENTIALS"));
        assert!(yaml.contains("secretKeyRef"));
        // The work-kind label a controller sweep reconciles rows against.
        assert!(yaml.contains("crucible.io/work-kind: grounded-rank"));
        assert!(yaml.contains("app.kubernetes.io/managed-by: crucible"));
        // The one-shot command: clone then rank-grounded with the marker + openshell backend.
        assert!(yaml.contains("git clone"));
        assert!(yaml.contains("rank-grounded --issue owner/repo#42"));
        assert!(yaml.contains("--marker"));
        assert!(yaml.contains("--agent-backend openshell"));
        assert!(!yaml.contains("--tier"), "a rank turn carries no --tier");
        assert!(yaml.contains("--sandbox-image registry.example.com/epp-sandbox:latest"));
        assert!(yaml.contains("name: REGISTRY_AUTH_FILE"));
        assert!(!yaml.contains("podman login"));
        // Default `sandbox_driver` (podman): no `--compute-driver` flag, no kubernetes driver env.
        assert!(!yaml.contains("--compute-driver"), "podman needs no flag");
        assert!(
            !yaml.contains("CRUCIBLE_SANDBOX_"),
            "podman driver env leak"
        );
        // No loop-only machinery leaks in: no broker env, no netpol, no kube RBAC mount.
        assert!(!yaml.contains("BROKER_"), "no broker env on a turn pod");
        assert!(!yaml.contains("kind: NetworkPolicy"));
        assert!(!yaml.contains("kind: RoleBinding"));
        // No avoid-list in the profile, no affinity stanza.
        assert!(!yaml.contains("affinity"), "no affinity key: {yaml}");
        // No drop-box configured → no Tier 2 ingest env/volume leaks in (Tier 2 ingest).
        assert!(
            !yaml.contains("CRUCIBLE_INGEST_URL"),
            "no ingest env without a drop-box"
        );
        assert!(
            !yaml.contains("crucible-ingest-token"),
            "no ingest token volume"
        );
    }

    /// Under `sandbox_driver = "kubernetes"` a turn pod must thread the same `--compute-driver`
    /// flag and `CRUCIBLE_SANDBOX_*` env the loop pod's wrapper gets (`kube.rs`'s
    /// `kubernetes_sandbox_env`), without it the turn's `rank-grounded`/`scope --propose`
    /// subcommand synthesizes a fresh `Args` that defaults to the podman driver regardless of the
    /// profile, and the gateway resolves the sandbox image through the nested-podman/authfile path
    /// (wrong credential) instead of the kubelet's `imagePullSecrets`. It must also automount the SA
    /// token, the in-pod gateway calls the API server directly for Sandbox CRs and has no mounted
    /// kubeconfig to fall back on, so without the token it never becomes healthy.
    #[test]
    fn turn_pod_under_kubernetes_driver_threads_the_flag_and_sandbox_env() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            sandbox_driver = "kubernetes"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull-secret"
            [secrets]
            pull_authfile = "quay-authfile"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Rank,
                name: "crucible-turn-owner-repo-42-abcd".to_string(),
                issue: "owner/repo#42".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 5.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render turn");

        // The wrapper passes the driver through explicitly.
        assert!(
            yaml.contains("rank-grounded --issue owner/repo#42"),
            "{yaml}"
        );
        assert!(
            yaml.contains("--compute-driver=kubernetes"),
            "the compute driver must reach the turn's subcommand: {yaml}"
        );
        // The gateway needs the API server to reach Sandbox CRs, no mounted kubeconfig like the
        // loop pod, so the automounted token is the only path in.
        assert!(
            yaml.contains("automountServiceAccountToken: true"),
            "the kubernetes driver needs the SA token automounted: {yaml}"
        );
        // The runtime `gateway_toml()` env, the same keys `kubernetes_sandbox_env` projects for
        // the loop pod, so the kubelet (not the nested podman/authfile path) pulls the sandbox
        // image via `imagePullSecrets`.
        assert!(yaml.contains("name: CRUCIBLE_POD_IP"));
        assert!(yaml.contains("fieldPath: status.podIP"));
        assert!(yaml.contains("name: CRUCIBLE_SANDBOX_NAMESPACE"));
        assert!(yaml.contains("value: autoresearch"));
        assert!(yaml.contains("name: CRUCIBLE_SANDBOX_SERVICE_ACCOUNT"));
        assert!(yaml.contains("name: CRUCIBLE_SANDBOX_DEFAULT_IMAGE"));
        assert!(yaml.contains("value: registry.example.com/epp-sandbox:latest"));
        assert!(yaml.contains("name: CRUCIBLE_SANDBOX_IMAGE_PULL_SECRETS"));
        assert!(yaml.contains("value: example-pull-secret"));
        assert!(yaml.contains("name: CRUCIBLE_SANDBOX_APP_ARMOR_PROFILE"));
        // The podman-driver authfile env is untouched by the switch, still projected whenever the
        // profile mounts one, so a manual `--compute-driver=podman` override still works.
        assert!(yaml.contains("name: REGISTRY_AUTH_FILE"));
        assert!(
            !yaml.contains("privileged"),
            "no securityContext under the kubernetes driver: {yaml}"
        );
    }

    /// With `[cluster].ingest_url` set, a turn pod grows the Tier 2 ingest scaffolding (Tier 2 ingest):
    /// the `CRUCIBLE_INGEST_URL`/`_TOKEN_PATH`/`POD_NAME` env, a projected `crucible-ingest`-audience
    /// SA token volume mounted read-only, while `automountServiceAccountToken: false` still stands
    /// (this is one explicit audience-locked file, not the kube-API passport).
    #[test]
    fn turn_pod_with_ingest_url_projects_the_drop_box_token_and_env() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "crucible-turn"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            ingest_url = "http://crucible-controller.autoresearch.svc:8080"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Scope,
                name: "crucible-scope-owner-repo-42-abcd".to_string(),
                issue: "owner/repo#42".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 5.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render turn");

        // The env carrying the drop-box URL + the token path + the pod's own name (downward API).
        assert!(yaml.contains("CRUCIBLE_INGEST_URL"));
        assert!(yaml.contains("http://crucible-controller.autoresearch.svc:8080"));
        assert!(yaml.contains("CRUCIBLE_INGEST_TOKEN_PATH"));
        assert!(yaml.contains("CRUCIBLE_POD_NAME"));
        assert!(yaml.contains("metadata.name"), "downward-API pod name");
        // The uid is what lets the in-pod gateway own what it publishes, so that material is
        // collected with the turn instead of outliving it.
        assert!(yaml.contains("CRUCIBLE_POD_UID"));
        assert!(yaml.contains("metadata.uid"), "downward-API pod uid");
        // The projected, audience-locked token volume + its read-only mount.
        assert!(yaml.contains("crucible-ingest-token"));
        assert!(yaml.contains("audience: crucible-ingest"));
        assert!(yaml.contains("/var/run/secrets/crucible.io/ingest"));
        // The default stays: the kube-API automount is still off.
        assert!(yaml.contains("automountServiceAccountToken: false"));
    }

    /// A scope turn pod renders the same agent-turn shape as a rank turn, but with the scope wrapper
    /// command (`crucible scope --propose --json --force --marker`) and the `scope` work-kind label.
    #[test]
    fn scope_turn_pod_renders_the_scope_wrapper_and_label() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
            [secrets]
            pull_authfile = "example-pull"
            [[secret_env]]
            name = "GCLOUD_CREDENTIALS"
            secret = "gcloud-adc"
            key = "adc.json"
            [env]
            GITHUB_TOKEN = "unused-in-render"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Scope,
                name: "crucible-scope-owner-repo-42-abcd".to_string(),
                issue: "owner/repo#42".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 8.0,
                digests: None,
                tier: Some(ProposeTier::T1),
                gaming_refine_rounds: 3,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render scope turn");

        assert!(yaml.contains("kind: Pod"));
        assert!(yaml.contains("name: crucible-scope-owner-repo-42-abcd"));
        assert!(yaml.contains("namespace: autoresearch"));
        assert!(yaml.contains("privileged: true"));
        assert!(yaml.contains("restartPolicy: Never"));
        // The scope work-kind label (not grounded-rank).
        assert!(
            yaml.contains("crucible.io/work-kind: scope"),
            "scope label, not grounded-rank"
        );
        assert!(!yaml.contains("crucible.io/work-kind: grounded-rank"));
        assert!(yaml.contains("app.kubernetes.io/managed-by: crucible"));
        // The scope wrapper command: clone then scope --propose with the marker.
        assert!(yaml.contains("git clone"));
        assert!(
            yaml.contains("crucible scope --propose --json --force --marker --issue owner/repo#42"),
            "scope command in wrapper"
        );
        assert!(yaml.contains("--max-cost 8"));
        assert!(yaml.contains("--repo \"$CHECKOUT\""));
        assert!(yaml.contains("--out \"$SCOPE_OUT\""));
        // The in-pod turn runs on the openshell backend, the loop image has no claude CLI.
        assert!(yaml.contains("--agent-backend openshell"));
        assert!(yaml.contains("--sandbox-image registry.example.com/epp-sandbox:latest"));
        // The confirmed tier: the confirmed tier lands in the wrapper's scope invocation.
        assert!(
            yaml.contains("--max-cost 8 --tier t1"),
            "the confirmed tier is forwarded: {yaml}"
        );
        // The gaming-review refine bound rides the same invocation.
        assert!(
            yaml.contains("--gaming-refine-rounds 3"),
            "the gaming refine bound is forwarded: {yaml}"
        );
        // No rank-grounded command in this wrapper.
        assert!(
            !yaml.contains("rank-grounded"),
            "scope turn must not run rank-grounded"
        );
        assert!(yaml.contains("name: REGISTRY_AUTH_FILE"));
        assert!(!yaml.contains("podman login"));
        // Same no-loop-machinery invariant.
        assert!(!yaml.contains("BROKER_"), "no broker env on a turn pod");
        assert!(!yaml.contains("kind: NetworkPolicy"));
        assert!(!yaml.contains("kind: RoleBinding"));
    }

    /// A scope turn with a scenario's ledgered `goal_text` set renders `--goal-file` (base64-decoded
    /// into an in-pod scratch file) instead of `--issue`, the Pod-executor counterpart of the
    /// non-pod executor's local-file `Ingest` arm, so a non-upstream scenario never routes into the
    /// engine's GitHub fetch.
    #[test]
    fn scope_turn_pod_with_goal_text_uses_goal_file_not_issue() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
            [secrets]
            pull_authfile = "example-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Scope,
                name: "crucible-scope-adopt-me".to_string(),
                issue: "scenario:deadbeef".to_string(),
                goal_text: Some("fix the reticulator".to_string()),
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 8.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render scope turn");

        assert!(
            yaml.contains("--goal-file \"$GOAL_FILE\""),
            "goal_text set: the scope wrapper uses --goal-file: {yaml}"
        );
        assert!(
            !yaml.contains("--issue scenario:deadbeef"),
            "goal_text set: --issue never carries the synthetic scenario key: {yaml}"
        );
        assert!(
            yaml.contains("base64 -d"),
            "the ledgered goal text is base64-decoded into the in-pod scratch file: {yaml}"
        );
    }

    /// A scope turn with no confirmed tier renders no `--tier` flag at all, the in-pod engine
    /// falls back to its own t0 default (back-compat with pre-J3 controllers).
    #[test]
    fn scope_turn_pod_without_a_tier_omits_the_flag() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
            [secrets]
            pull_authfile = "example-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Scope,
                name: "crucible-scope-owner-repo-43-abcd".to_string(),
                issue: "owner/repo#43".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 8.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 1,
                skip_gaming_review: false,
                authoritative: false,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render scope turn");
        assert!(yaml.contains("crucible scope --propose"));
        assert!(!yaml.contains("--tier"), "no tier, no flag: {yaml}");
    }

    /// `skip_gaming_review` renders `--skip-gaming-review` and OMITS `--gaming-refine-rounds`,
    /// the operator escape hatch for demo/bring-up postures where the review's fail-closed loop
    /// blocks the first e2e run through a new deployment.
    #[test]
    fn scope_turn_pod_skip_gaming_review_omits_the_rounds_flag() {
        let profile: DeployProfile = toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
            [secrets]
            pull_authfile = "example-pull"
        "#,
        )
        .expect("profile parses");

        let yaml = render_turn(
            &profile,
            &TurnOpts {
                kind: TurnKind::Scope,
                name: "crucible-scope-owner-repo-44-abcd".to_string(),
                issue: "owner/repo#44".to_string(),
                goal_text: None,
                repo_url: "https://github.com/owner/repo.git".to_string(),
                repo_ref: None,
                sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
                max_cost: 8.0,
                digests: None,
                tier: None,
                gaming_refine_rounds: 2,
                skip_gaming_review: true,
                authoritative: true,
                harness: None,
                model: None,
                pack_path: None,
            },
        )
        .expect("render scope turn");
        assert!(
            yaml.contains("--skip-gaming-review"),
            "the skip flag is forwarded: {yaml}"
        );
        assert!(
            !yaml.contains("--gaming-refine-rounds"),
            "the rounds flag is omitted once the review is skipped: {yaml}"
        );
        assert!(
            yaml.contains("--authoritative"),
            "the authoritative flag is forwarded: {yaml}"
        );
    }

    fn minimal_profile() -> DeployProfile {
        toml::from_str(
            r#"
            [cluster]
            loop_namespace = "autoresearch"
            rig_namespace = "rig"
            service_account = "autoresearch-publisher"
            supervisor_image = "registry.example.com/openshell-supervisor:latest"
            [image]
            loop = "ghcr.io/neuralmagic/crucible:latest"
            pull_secret = "example-pull"
        "#,
        )
        .expect("profile parses")
    }

    fn opts_with_run_config(
        kind: TurnKind,
        harness: Option<crate::manifest::Harness>,
        model: Option<&str>,
    ) -> TurnOpts {
        TurnOpts {
            kind,
            name: "crucible-turn-owner-repo-45-abcd".to_string(),
            issue: "owner/repo#45".to_string(),
            goal_text: None,
            repo_url: "https://github.com/owner/repo.git".to_string(),
            repo_ref: None,
            sandbox_image: "registry.example.com/epp-sandbox:latest".to_string(),
            max_cost: 5.0,
            digests: None,
            tier: None,
            gaming_refine_rounds: 1,
            skip_gaming_review: false,
            authoritative: false,
            harness,
            model: model.map(str::to_string),
            pack_path: None,
        }
    }

    /// A run-level harness/model reaches the in-pod invocation of both turn kinds, spelled the way
    /// the in-pod CLI parses it back.
    #[test]
    fn turn_pod_forwards_the_run_harness_and_model() {
        let profile = minimal_profile();
        for kind in [TurnKind::Rank, TurnKind::Scope] {
            let yaml = render_turn(
                &profile,
                &opts_with_run_config(
                    kind,
                    Some(crate::manifest::Harness::Hermes),
                    Some("hermes-4-70b"),
                ),
            )
            .expect("render turn");
            assert!(
                yaml.contains("--harness hermes"),
                "the run harness is forwarded: {yaml}"
            );
            assert!(
                yaml.contains("--model hermes-4-70b"),
                "the run model is forwarded: {yaml}"
            );
        }
    }

    /// Either half can ride alone: a model override on the default harness emits `--model` only.
    #[test]
    fn turn_pod_forwards_a_model_without_a_harness() {
        let yaml = render_turn(
            &minimal_profile(),
            &opts_with_run_config(TurnKind::Scope, None, Some("claude-opus-4-6")),
        )
        .expect("render turn");
        assert!(yaml.contains("--model claude-opus-4-6"), "{yaml}");
        assert!(!yaml.contains("--harness"), "no harness, no flag: {yaml}");
    }

    /// Unset run config emits neither flag, so a pre-existing render stays byte-identical.
    #[test]
    fn turn_pod_without_run_config_omits_both_flags() {
        let profile = minimal_profile();
        for kind in [TurnKind::Rank, TurnKind::Scope] {
            let yaml = render_turn(&profile, &opts_with_run_config(kind, None, None))
                .expect("render turn");
            assert!(!yaml.contains("--harness"), "no harness, no flag: {yaml}");
            assert!(!yaml.contains("--model"), "no model, no flag: {yaml}");
        }
    }
    #[test]
    fn repo_ref_clones_at_the_branch_in_both_wrappers() {
        for kind in [TurnKind::Rank, TurnKind::Scope] {
            let mut opts = opts_with_run_config(kind, None, None);
            opts.repo_ref = Some("feature/x-1.2".to_string());
            let yaml = render_turn(&minimal_profile(), &opts).expect("render turn");
            assert!(
                yaml.contains(
                    "git clone --depth 50 --branch feature/x-1.2 https://github.com/owner/repo.git"
                ),
                "{kind:?} wrapper clones at the ref:\n{yaml}"
            );
        }
    }

    #[test]
    fn no_repo_ref_clones_the_default_branch() {
        let opts = opts_with_run_config(TurnKind::Scope, None, None);
        let yaml = render_turn(&minimal_profile(), &opts).expect("render turn");
        assert!(yaml.contains("git clone --depth 50 https://github.com/owner/repo.git"));
        assert!(!yaml.contains("--branch"));
    }

    /// A pack the repo already carries is validated, not drafted: the wrapper runs
    /// `scope --pack` over the checkout and none of the propose turn's agent machinery.
    #[test]
    fn a_pack_path_turn_validates_instead_of_proposing() {
        let mut opts = opts_with_run_config(TurnKind::Scope, None, None);
        opts.pack_path = Some(PackPath::parse("examples/selfhost").expect("valid"));
        opts.tier = Some(ProposeTier::T1);
        opts.gaming_refine_rounds = 3;
        opts.authoritative = true;
        opts.goal_text = Some("a goal the pack does not need".to_string());
        let yaml = render_turn(&minimal_profile(), &opts).expect("render");

        assert!(
            yaml.contains(
                r#"crucible scope --pack "$CHECKOUT/examples/selfhost" --json --force --marker"#
            ),
            "{yaml}"
        );
        assert!(yaml.contains("git clone"), "still clones the repo");
        // It is still a scope turn to the pod watcher, so the existing dispatch arm picks it up.
        assert!(yaml.contains("crucible.io/work-kind: scope"));
        // No agent runs, so nothing that only describes how a pack gets drafted may appear.
        for absent in [
            "--propose",
            "--tier",
            "--gaming-refine-rounds",
            "--authoritative",
            "--agent-backend",
            "--sandbox-image",
            "--goal-file",
            "--max-cost",
        ] {
            assert!(
                !yaml.contains(absent),
                "{absent} has no place here:\n{yaml}"
            );
        }
    }

    #[test]
    fn a_pack_path_leaving_the_checkout_is_refused() {
        for bad in [
            "/etc",
            "../secrets",
            "packs/../../etc",
            "",
            "   ",
            "pack\"; rm -rf /",
            "$(id)",
            "pack dir",
        ] {
            assert!(PackPath::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_pack_path_keeps_the_relative_path_it_was_given() {
        for (raw, want) in [
            ("examples/selfhost", "examples/selfhost"),
            ("examples/selfhost/", "examples/selfhost"),
            ("  domains/deepgemm  ", "domains/deepgemm"),
            ("pack", "pack"),
            ("a_b-c.d/e", "a_b-c.d/e"),
        ] {
            assert_eq!(PackPath::parse(raw).expect(raw).as_str(), want);
        }
    }

    /// A rank turn has no pack to validate; setting one must not change its command.
    #[test]
    fn a_rank_turn_ignores_a_pack_path() {
        let mut opts = opts_with_run_config(TurnKind::Rank, None, None);
        let plain = render_turn(&minimal_profile(), &opts).expect("render");
        opts.pack_path = Some(PackPath::parse("examples/selfhost").expect("valid"));
        let with_pack = render_turn(&minimal_profile(), &opts).expect("render");
        assert_eq!(plain, with_pack);
        assert!(with_pack.contains("crucible rank-grounded"));
    }

    #[test]
    fn a_shell_hostile_repo_ref_is_refused() {
        for bad in ["", "-x", "main; rm -rf /", "a b", "$(id)", "v1*", "it's"] {
            let mut opts = opts_with_run_config(TurnKind::Rank, None, None);
            opts.repo_ref = Some(bad.to_string());
            let err = render_turn(&minimal_profile(), &opts).expect_err(bad);
            assert!(err.to_string().contains("--repo-ref"), "{bad:?}: {err}");
        }
    }
}
