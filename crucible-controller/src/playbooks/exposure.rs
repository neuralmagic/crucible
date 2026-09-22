//! The pack's declared exposure (RFC-0001 C-OUTPUTS + C-CAPABILITY-DISCLOSURE): where a run of it
//! may write, how many times, and what reach it holds.
//!
//! Extraction links the pinned engine: [`extract`] loads the stored manifest and calls
//! `crucible::exposure::compute`, so the controller discloses exactly what a run of that pin
//! would hold, and a pack the engine cannot load refuses the registration.

use anyhow::{Context, Result};

use crucible_contract::content_digest;

use serde::{Deserialize, Serialize};

use std::path::Path;

use utoipa::ToSchema;

/// The manifest file `plan exposure` is pointed at.
const MANIFEST: &str = "crucible.toml";

/// Where a bounded output may land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum OutputTarget {
    /// One address, fixed by the manifest.
    Fixed { fixed: String },
    /// A scope an address has to fall inside, optionally bound by a workflow param.
    Open { open: OpenScope },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OpenScope {
    pub scope: String,
    /// The workflow param whose value binds the address, when the pack named one.
    #[serde(default)]
    pub param: Option<String>,
}

impl OutputTarget {
    /// The target as one presentation token: an address, or the scope it has to fall inside.
    pub fn render(&self) -> String {
        match self {
            OutputTarget::Fixed { fixed } => fixed.clone(),
            OutputTarget::Open { open } => match &open.param {
                Some(param) => format!("{} (param {param})", open.scope),
                None => open.scope.clone(),
            },
        }
    }
}

/// One declared output bound: a kind, how many times a run may spend it, and where it may land.
/// `target` is absent for a kind that addresses nothing (`gpu-capture`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExposureOutput {
    pub kind: String,
    pub count: u32,
    #[serde(default)]
    pub target: Option<OutputTarget>,
    /// Where the bound came from (`manifest`, an engine default). Absent on older documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl ExposureOutput {
    /// The one-line presentation form: `draft-pr x1 -> owner/repo`.
    pub fn line(&self) -> String {
        match &self.target {
            Some(target) => format!("{} x{} -> {}", self.kind, self.count, target.render()),
            None => format!("{} x{}", self.kind, self.count),
        }
    }
}

/// Which side of the sandbox holds a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CredentialContext {
    /// The value is projected into the agent's environment: the agent can read it.
    Agent,
    /// The broker holds it; no agent process ever sees the bytes.
    Broker,
    #[serde(other)]
    Other,
}

/// A disclosed capability. [`Capability::Other`] carries a shape this build does not know
/// verbatim, so a newer engine's disclosure is never dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum Capability {
    Known(KnownCapability),
    #[schema(value_type = Object)]
    Other(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum KnownCapability {
    Egress {
        host: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        access: Option<String>,
        #[serde(default)]
        source: Option<String>,
    },
    Credential {
        name: String,
        context: CredentialContext,
        #[serde(default)]
        system: Option<String>,
        #[serde(default)]
        scope: Option<String>,
    },
    Relay {
        path: String,
        #[serde(default)]
        sources: Vec<String>,
    },
    BrokerBin {
        bin: String,
    },
    ExternalCommands {
        present: bool,
    },
}

impl Capability {
    /// The one-line presentation form.
    pub fn line(&self) -> String {
        let known = match self {
            Capability::Known(k) => k,
            Capability::Other(value) => {
                return format!("? {}", crate::model::sorted_json(value.clone()));
            }
        };
        match known {
            KnownCapability::Egress {
                host,
                port,
                access,
                source,
            } => {
                let port = port.map(|p| format!(":{p}")).unwrap_or_default();
                let access = access.as_deref().unwrap_or("full");
                let source = source.as_deref().unwrap_or("manifest");
                format!("egress {host}{port} {access} ({source})")
            }
            KnownCapability::Credential {
                name,
                context,
                system,
                scope,
            } => {
                let context = match context {
                    CredentialContext::Agent => "agent",
                    CredentialContext::Broker => "broker",
                    CredentialContext::Other => "?",
                };
                let system = system.as_deref().unwrap_or("undeclared");
                let scope = scope
                    .as_deref()
                    .map(|s| format!(" scope {s}"))
                    .unwrap_or_default();
                format!("credential {name} ({context}) {system}{scope}")
            }
            KnownCapability::Relay { path, sources } => {
                format!("relay {path} <- {}", sources.join(", "))
            }
            KnownCapability::BrokerBin { bin } => format!("broker-bin {bin}"),
            KnownCapability::ExternalCommands { present } => {
                format!("external-commands present:{present}")
            }
        }
    }

    /// The credential's name, when this is a credential the agent itself can read.
    fn agent_credential(&self) -> Option<&str> {
        match self {
            Capability::Known(KnownCapability::Credential {
                name,
                context: CredentialContext::Agent,
                ..
            }) => Some(name.as_str()),
            _ => None,
        }
    }
}

/// The whole document `crucible plan exposure` prints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Exposure {
    pub version: u32,
    #[serde(default)]
    pub outputs: Vec<ExposureOutput>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

impl Exposure {
    /// Decode a stored document.
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        serde_json::from_value(value).context("decoding a stored exposure document")
    }

    pub fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).context("encoding an exposure document")
    }

    /// The document's content digest, over its canonical form: outputs and capabilities sorted by
    /// their serialized text, so a manifest that only reorders its declarations keeps its digest.
    pub fn digest(&self) -> Result<String> {
        let canonical = serde_json::to_value(self.canonical()?)
            .map(crate::model::sorted_json)
            .context("serializing the exposure document")?;
        let bytes = serde_json::to_vec(&canonical).context("serializing the exposure document")?;
        Ok(content_digest(&bytes))
    }

    fn canonical(&self) -> Result<Exposure> {
        fn sorted<T: Clone + serde::Serialize>(items: &[T]) -> Result<Vec<T>> {
            let mut keyed = items
                .iter()
                .map(|item| {
                    let key = serde_json::to_value(item).map(crate::model::sorted_json)?;
                    Ok((key.to_string(), item.clone()))
                })
                .collect::<Result<Vec<_>, serde_json::Error>>()
                .context("serializing an exposure entry")?;
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(keyed.into_iter().map(|(_, item)| item).collect())
        }
        Ok(Exposure {
            version: self.version,
            outputs: sorted(&self.outputs)?,
            capabilities: sorted(&self.capabilities)?,
        })
    }

    /// Does the disclosure cover an agent-readable credential under `name`? The engine discloses
    /// `[agent].env` names as `context=agent` credentials.
    pub fn covers_agent_credential(&self, name: &str) -> bool {
        self.capabilities
            .iter()
            .filter_map(Capability::agent_credential)
            .any(|declared| declared == name)
    }

    /// Every disclosed agent-readable credential name.
    pub fn agent_credentials(&self) -> Vec<&str> {
        self.capabilities
            .iter()
            .filter_map(Capability::agent_credential)
            .collect()
    }

    /// One line per output bound, in declaration order.
    pub fn output_lines(&self) -> Vec<String> {
        self.outputs.iter().map(ExposureOutput::line).collect()
    }

    /// One line per disclosed capability.
    pub fn capability_lines(&self) -> Vec<String> {
        self.capabilities.iter().map(Capability::line).collect()
    }
}

/// What a row records as its exposure.
#[derive(Debug, Clone, PartialEq)]
pub enum Extraction {
    /// The document the engine computed for the exact stored content.
    Declared(Exposure),
    /// No document. A registered launch row stores none, since its disclosure is the registry
    /// revision's; a row frozen before extraction existed reads back the same way. A launch
    /// reading such a row grants the agent nothing it would have had to disclose.
    Absent,
}

impl Extraction {
    pub fn declared(&self) -> Option<&Exposure> {
        match self {
            Extraction::Declared(e) => Some(e),
            Extraction::Absent => None,
        }
    }

    /// The pair a row stores: the document and its digest, or `(None, None)` when absent.
    pub fn stored(&self) -> Result<(Option<serde_json::Value>, Option<String>)> {
        match self {
            Extraction::Declared(e) => Ok((Some(e.to_value()?), Some(e.digest()?))),
            Extraction::Absent => Ok((None, None)),
        }
    }
}

/// Why an extraction failed. `Refused` is the engine rejecting this pack's manifest, and fails the
/// caller's registration or bump verbatim; `Internal` is the extraction itself breaking.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Compute the exposure of the pack at `pack_root` with the linked engine. `pr_repo` is the
/// publish target a launch would be rendered with, which the engine folds into the `draft-pr`
/// default the same way it does at run time; `None` (or empty) where the caller has none.
pub fn extract(pack_root: &Path, pr_repo: Option<&str>) -> Result<Exposure, ExtractError> {
    let manifest = pack_root.join(MANIFEST);
    if !manifest.is_file() {
        return Err(ExtractError::Refused(format!(
            "the pack has no {MANIFEST}, so it discloses no exposure"
        )));
    }
    let loaded = crucible::manifest::Manifest::load(&manifest)
        .map_err(|e| ExtractError::Refused(format!("{e:#}")))?;
    let computed = crucible::exposure::compute(&loaded, pr_repo.filter(|r| !r.is_empty()));
    let value = serde_json::to_value(&computed).context("encoding the computed exposure")?;
    Exposure::from_value(value).map_err(ExtractError::Internal)
}

/// The presentation block every surface renders: the outputs, the capabilities, or — for a row
/// that stored nothing — [`UNDECLARED`]. Never empty.
pub fn present(stored: Option<&Exposure>) -> Vec<String> {
    let Some(exposure) = stored else {
        return vec![UNDECLARED.to_string()];
    };
    let mut lines = Vec::new();
    if exposure.outputs.is_empty() {
        lines.push("outputs: none declared".to_string());
    } else {
        lines.push("outputs:".to_string());
        lines.extend(
            exposure
                .output_lines()
                .into_iter()
                .map(|l| format!("  {l}")),
        );
    }
    if exposure.capabilities.is_empty() {
        lines.push("capabilities: none declared".to_string());
    } else {
        lines.push("capabilities:".to_string());
        lines.extend(
            exposure
                .capability_lines()
                .into_iter()
                .map(|l| format!("  {l}")),
        );
    }
    lines
}

/// What every surface says for a row that stored no exposure document.
pub const UNDECLARED: &str = "outputs undeclared (no exposure stored for this revision)";

/// The stored exposure of a registered playbook. The outer `None` is an unknown id; the inner
/// `None` is absent-legacy.
pub async fn registered(pool: &sqlx::PgPool, id: &str) -> Result<Option<Option<Exposure>>> {
    use sqlx::Row as _;
    let row = sqlx::query("SELECT exposure FROM playbooks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("reading a playbook exposure")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored: Option<serde_json::Value> = row
        .try_get("exposure")
        .context("decoding a playbook exposure")?;
    stored.map(Exposure::from_value).transpose().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::model::{TaskName, producing_task};

    const DOC: &str = r#"{"version":1,
        "outputs":[
          {"kind":"draft-pr","count":1,"target":{"fixed":"neuralmagic/crucible"},"source":"manifest"},
          {"kind":"tracker-comment","count":3,"target":{"open":{"scope":"PROJ-","param":"issue"}}},
          {"kind":"gpu-capture","count":2,"target":null}],
        "capabilities":[
          {"kind":"egress","host":"api.github.com","port":443,"access":"read-write","source":"builtin"},
          {"kind":"credential","name":"GH_TOKEN","context":"agent","system":"github","scope":"repo"},
          {"kind":"credential","name":"SLACK_WEBHOOK_URL","context":"broker","system":"slack","scope":null},
          {"kind":"relay","path":"/relay/vertex","sources":["VERTEX_KEY"]},
          {"kind":"broker-bin","bin":"/usr/local/bin/crucible-broker"},
          {"kind":"external-commands","present":true},
          {"kind":"time-travel","era":"cretaceous"}]}"#;

    fn pack<'a>(dir: &'a Path, manifest: &str) -> &'a Path {
        std::fs::write(dir.join(MANIFEST), manifest).expect("manifest");
        dir
    }

    #[test]
    fn a_document_decodes_renders_and_digests() {
        let exposure: Exposure = serde_json::from_str(DOC).expect("decodes");
        assert_eq!(exposure.version, 1);
        assert_eq!(
            exposure.output_lines(),
            vec![
                "draft-pr x1 -> neuralmagic/crucible",
                "tracker-comment x3 -> PROJ- (param issue)",
                "gpu-capture x2",
            ]
        );
        assert_eq!(
            exposure.capability_lines(),
            vec![
                "egress api.github.com:443 read-write (builtin)",
                "credential GH_TOKEN (agent) github scope repo",
                "credential SLACK_WEBHOOK_URL (broker) slack",
                "relay /relay/vertex <- VERTEX_KEY",
                "broker-bin /usr/local/bin/crucible-broker",
                "external-commands present:true",
                "? {\"era\":\"cretaceous\",\"kind\":\"time-travel\"}",
            ],
            "a capability shape this build does not know still reaches the approval surface"
        );
        let digest = exposure.digest().expect("digests");
        assert_eq!(
            digest,
            Exposure::from_value(exposure.to_value().expect("encodes"))
                .expect("round-trips")
                .digest()
                .expect("digests"),
            "a stored-and-reread document digests the same"
        );
    }

    #[test]
    fn only_agent_context_credentials_cover_an_agent_binding() {
        let exposure: Exposure = serde_json::from_str(DOC).expect("decodes");
        assert!(exposure.covers_agent_credential("GH_TOKEN"));
        assert!(
            !exposure.covers_agent_credential("SLACK_WEBHOOK_URL"),
            "a broker-held credential covers no agent-visible binding"
        );
        assert!(!exposure.covers_agent_credential("NOPE"));
        assert_eq!(exposure.agent_credentials(), vec!["GH_TOKEN"]);
    }

    #[test]
    fn a_widened_bound_changes_the_digest() {
        let narrow: Exposure = serde_json::from_str(DOC).expect("decodes");
        let widened: Exposure =
            serde_json::from_str(&DOC.replace(r#""count":1"#, r#""count":9"#)).expect("decodes");
        assert_ne!(
            narrow.digest().expect("digest"),
            widened.digest().expect("digest")
        );
    }

    #[test]
    fn a_stored_manifest_extracts_the_document_the_engine_computes() {
        let dir = tempfile::tempdir().expect("scratch");
        let root = pack(dir.path(), crate::testing::fixtures::LOOP_PACK_MANIFEST);
        let exposure = extract(root, Some("wren/repo-fork")).expect("extracts");
        assert_eq!(exposure.version, 1);
        let lines = exposure.output_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("draft-pr") && l.contains("wren/repo-fork")),
            "the caller's publish target is the draft-pr default: {lines:?}"
        );
        let stored = Extraction::Declared(exposure.clone())
            .stored()
            .expect("stores");
        assert!(stored.0.is_some() && stored.1.is_some());
        assert_eq!(
            extract(root, Some("")).expect("extracts").output_lines(),
            extract(root, None).expect("extracts").output_lines(),
            "an empty target is no target"
        );
    }

    #[test]
    fn a_pack_the_engine_cannot_load_is_refused() {
        let dir = tempfile::tempdir().expect("scratch");
        let err = extract(dir.path(), None).expect_err("no manifest");
        assert!(
            matches!(&err, ExtractError::Refused(m) if m.contains(MANIFEST)),
            "{err}"
        );

        let root = pack(dir.path(), "[agent]\nbackend = 7\n");
        let err = extract(root, None).expect_err("a malformed manifest");
        assert!(matches!(err, ExtractError::Refused(_)), "{err}");
    }

    #[test]
    fn a_reordered_declaration_keeps_its_digest() {
        let credential = |name: &str| {
            Capability::Known(KnownCapability::Credential {
                name: name.to_string(),
                context: CredentialContext::Agent,
                system: None,
                scope: None,
            })
        };
        let output = |kind: &str| ExposureOutput {
            kind: kind.to_string(),
            count: 1,
            target: None,
            source: None,
        };
        let one = Exposure {
            version: 1,
            outputs: vec![output("draft-pr"), output("chat-message")],
            capabilities: vec![credential("GH_TOKEN"), credential("SLACK_TOKEN")],
        };
        let other = Exposure {
            version: 1,
            outputs: vec![output("chat-message"), output("draft-pr")],
            capabilities: vec![credential("SLACK_TOKEN"), credential("GH_TOKEN")],
        };
        assert_eq!(
            one.digest().expect("digest"),
            other.digest().expect("digest")
        );
        let widened = Exposure {
            capabilities: vec![credential("GH_TOKEN")],
            ..one.clone()
        };
        assert_ne!(
            one.digest().expect("digest"),
            widened.digest().expect("digest")
        );
    }

    fn names(names: &[&str]) -> Vec<TaskName> {
        names.iter().copied().map(TaskName::from).collect()
    }

    #[test]
    fn an_output_attaches_to_the_step_that_spends_it() {
        let tasks = names(&["scan", "work", "publish", "deploy_candidate"]);
        let publish = TaskName::from("publish");
        assert_eq!(producing_task("draft-pr", &tasks), Some(&publish));
        assert_eq!(producing_task("tracker-comment", &tasks), Some(&publish));
        assert_eq!(
            producing_task("deploy", &tasks),
            Some(&TaskName::from("deploy_candidate"))
        );
        assert_eq!(
            producing_task("draft-pr", &names(&["scan", "work"])),
            None,
            "a plan with no publishing step attaches the bound to the sink instead"
        );
        assert_eq!(
            producing_task("gpu-capture", &tasks),
            None,
            "a kind that addresses nothing names no producer"
        );
        assert_eq!(
            producing_task("time-travel", &tasks),
            None,
            "a kind this build does not know still renders, off the sink"
        );
    }

    #[test]
    fn an_absent_exposure_presents_the_undeclared_marker() {
        assert_eq!(present(None), vec![UNDECLARED.to_string()]);
        let exposure: Exposure = serde_json::from_str(DOC).expect("decodes");
        let lines = present(Some(&exposure));
        assert_eq!(lines.first().map(String::as_str), Some("outputs:"));
        assert!(lines.iter().any(|l| l.contains("credential GH_TOKEN")));

        let empty = Exposure {
            version: 1,
            outputs: Vec::new(),
            capabilities: Vec::new(),
        };
        assert_eq!(
            present(Some(&empty)),
            vec![
                "outputs: none declared".to_string(),
                "capabilities: none declared".to_string()
            ],
            "a pack that declares nothing is not the same as a row that stored nothing"
        );
    }
}
