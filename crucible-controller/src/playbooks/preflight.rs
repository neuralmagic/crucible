//! The capability preflight: does the pack's sandbox image provide what the pack and the harness
//! it will run under require? One verdict, computed against the image catalog, consumed by the
//! preview surfaces, the launch endpoints and the reconcile-side dispatch alike.
use std::collections::BTreeMap;

use crucible::manifest::Harness;
use crucible_capability::{Unsatisfied, unsatisfied};
use serde::Serialize;
use utoipa::ToSchema;

use crate::images::model::CatalogImage;
use crate::playbooks::dispatch::PackAgent;
use crate::playbooks::providers::{harness_name, parse_harness};

/// Where the harness a dispatch resolved came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessSource {
    /// The launch pinned a provider explicitly.
    Pin,
    /// A domain or platform dispatch default supplied it.
    Default,
}

/// The harness the dispatch resolved, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHarness {
    pub harness: Harness,
    pub provider: String,
    pub source: HarnessSource,
}

/// The preflight verdict for one pack's image.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub struct ImagePreflight {
    /// The image reference the manifest names, verbatim.
    pub reference: Option<String>,
    /// The digest the catalog resolved the reference to.
    pub digest: Option<String>,
    /// The digest of the capability document matched against.
    pub capability_digest: Option<String>,
    /// The channel tags pointing at the matched digest, so a digest-pinned reference still reads
    /// as its promoted channel.
    pub tags: Vec<String>,
    /// False when the catalog is empty: there was nothing to match against and the image was
    /// not checked.
    pub checked: bool,
    /// The catalog knows the reference.
    pub catalogued: bool,
    /// The catalogued image carries a capability document.
    pub verified: bool,
    /// The launch proceeds on `allow_unverified_image` rather than a capability match.
    pub overridden: bool,
    /// Required predicates the image does not satisfy.
    pub unsatisfied: Vec<Unsatisfied>,
    /// Why the launch is refused; empty when it may proceed.
    pub refusals: Vec<String>,
    /// What the launcher should know even though the launch may proceed.
    pub warnings: Vec<String>,
}

impl ImagePreflight {
    pub fn refused(&self) -> bool {
        !self.refusals.is_empty()
    }
}

/// The harness a launch will run under: its provider pin, else the domain's or platform's
/// dispatch default, else `None` when nothing is configured and the manifest decides.
pub async fn resolve_harness(
    pool: &sqlx::PgPool,
    provider: Option<&str>,
    domain: Option<&str>,
) -> anyhow::Result<Option<ResolvedHarness>> {
    use crate::playbooks::providers::{DispatchOverride, WorkloadClass, resolve_dispatch};
    let over = provider.map(|provider_id| DispatchOverride {
        provider_id,
        model: None,
    });
    let source = if provider.is_some() {
        HarnessSource::Pin
    } else {
        HarnessSource::Default
    };
    Ok(
        resolve_dispatch(pool, over, domain, WorkloadClass::Playbook)
            .await?
            .map(|r| ResolvedHarness {
                harness: r.harness,
                provider: r.provider.id,
                source,
            }),
    )
}

/// The capability predicate a harness needs its sandbox to provide.
fn harness_predicate(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::Claude => Some("agent.claude-code"),
        Harness::Codex => Some("agent.codex"),
        Harness::OpenCode => Some("agent.opencode"),
        Harness::Pi => Some("agent.pi"),
        Harness::Hermes => None,
    }
}

/// The requirements a pack declares plus the one the effective harness implies.
fn effective_requires(
    requires: &BTreeMap<String, String>,
    harness: Harness,
) -> BTreeMap<String, String> {
    let mut requires = requires.clone();
    if let Some(predicate) = harness_predicate(harness) {
        requires
            .entry(predicate.to_string())
            .or_insert_with(|| "*".to_string());
    }
    requires
}

/// Split an image reference into its repository and what it pins by: `Some(digest)` for
/// `repo@sha256:…`, `Some(tag)` for `repo:tag`, `latest` for a bare repository.
fn split_reference(reference: &str) -> (String, ReferencePin) {
    if let Some((repository, digest)) = reference.split_once('@') {
        return (
            repository.to_string(),
            ReferencePin::Digest(digest.to_string()),
        );
    }
    let last_slash = reference.rfind('/').map(|i| i + 1).unwrap_or(0);
    match reference[last_slash..].rfind(':') {
        Some(i) => {
            let split = last_slash + i;
            (
                reference[..split].to_string(),
                ReferencePin::Tag(reference[split + 1..].to_string()),
            )
        }
        None => (
            reference.to_string(),
            ReferencePin::Tag("latest".to_string()),
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferencePin {
    Digest(String),
    Tag(String),
}

/// The catalogued image a reference names.
fn find_image<'a>(catalog: &'a [CatalogImage], reference: &str) -> Option<&'a CatalogImage> {
    let (repository, pin) = split_reference(reference);
    catalog.iter().find(|image| {
        image.repository == repository
            && match &pin {
                ReferencePin::Digest(d) => &image.digest == d,
                ReferencePin::Tag(t) => image.tags.iter().any(|tag| tag == t),
            }
    })
}

/// The one preflight. `resolved` is the harness dispatch will run, when the caller knows it: the
/// launch's pin or the scope's default. At preview time it is `None` and the manifest's own
/// harness stands in.
pub fn preflight(
    agent: &PackAgent,
    resolved: Option<&ResolvedHarness>,
    catalog: &[CatalogImage],
) -> ImagePreflight {
    let mut out = ImagePreflight {
        reference: agent.sandbox_image.clone(),
        ..ImagePreflight::default()
    };
    let declared = agent.harness.as_deref().and_then(parse_harness);
    if let (Some(declared), Some(resolved)) = (declared, resolved)
        && resolved.source == HarnessSource::Default
        && resolved.harness != declared
    {
        out.refusals.push(format!(
            "the dispatch default resolves to provider {:?}, whose harness is {}, but the pack \
             declares harness = {:?}; pin a provider explicitly to run it on {}",
            resolved.provider,
            harness_name(resolved.harness),
            harness_name(declared),
            harness_name(resolved.harness),
        ));
    }
    let harness = resolved.map(|r| r.harness).or(declared).unwrap_or_default();
    let Some(reference) = agent.sandbox_image.as_deref() else {
        return out;
    };
    if catalog.is_empty() {
        out.warnings.push(
            "the image catalog is empty, so the sandbox image was not checked against the pack's \
             requirements"
                .to_string(),
        );
        return out;
    }
    out.checked = true;
    let image = find_image(catalog, reference);
    out.catalogued = image.is_some();
    let doc = image.and_then(|i| i.capabilities.as_ref());
    if let Some(image) = image {
        out.digest = Some(image.digest.clone());
        out.capability_digest = image.capability_digest.clone();
        out.tags = image.tags.clone();
    }
    out.verified = doc.is_some();
    let Some(doc) = doc else {
        let why = if image.is_some() {
            "carries no capability document"
        } else {
            "is not in the image catalog"
        };
        if agent.allow_unverified_image {
            out.overridden = true;
            out.warnings.push(format!(
                "sandbox image {reference} {why}; launching on [agent] allow_unverified_image \
                 without matching the pack's requirements"
            ));
        } else {
            out.refusals.push(format!(
                "sandbox image {reference} {why}; pick a catalogued image, or set [agent] \
                 allow_unverified_image = true to launch it unmatched"
            ));
        }
        return out;
    };
    let requires = effective_requires(&agent.requires, harness);
    out.unsatisfied = unsatisfied(doc, &requires);
    for miss in &out.unsatisfied {
        let implied = harness_predicate(harness) == Some(miss.predicate.as_str())
            && !agent.requires.contains_key(&miss.predicate);
        out.refusals.push(match (&miss.found, implied) {
            (None, true) => format!(
                "sandbox image {reference} lacks {}, which harness {} needs",
                miss.predicate,
                harness_name(harness)
            ),
            (None, false) => format!(
                "sandbox image {reference} lacks {} (the pack requires {})",
                miss.predicate, miss.required
            ),
            (Some(found), _) => format!(
                "sandbox image {reference} has {} {found}, the pack requires {}",
                miss.predicate, miss.required
            ),
        });
    }
    out
}

/// One compatible image, with what ranks it: fewer surplus predicates first (the slimmest image
/// that satisfies everything), more preferred predicates first within that.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct RankedImage {
    pub image: crate::images::api::CatalogImageDto,
    /// Predicates the image provides beyond what the pack requires.
    pub surplus: usize,
    /// `[agent.prefers]` predicates the image satisfies.
    pub preferred: usize,
    /// The selection a picker defaults to: the top-ranked image on a promoted channel.
    pub default: bool,
}

/// One image the requirements exclude, with the predicates that excluded it.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ExcludedImage {
    pub image: crate::images::api::CatalogImageDto,
    pub unsatisfied: Vec<Unsatisfied>,
}

/// The catalog ranked for one pack (C-UX): compatible images in default order, excluded images
/// with their unsatisfied predicates, and unverified images that nothing could match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub struct RankedCatalog {
    pub compatible: Vec<RankedImage>,
    pub excluded: Vec<ExcludedImage>,
    pub unverified: Vec<crate::images::api::CatalogImageDto>,
}

const PROMOTED_TAG: &str = "latest";

/// Rank the catalog for a pack's requirements and preferences under `harness`.
pub fn rank(
    catalog: &[CatalogImage],
    requires: &BTreeMap<String, String>,
    prefers: &BTreeMap<String, String>,
    harness: Harness,
) -> RankedCatalog {
    let effective = effective_requires(requires, harness);
    let mut out = RankedCatalog::default();
    for image in catalog {
        let Some(doc) = image.capabilities.as_ref() else {
            out.unverified.push(image.clone().into());
            continue;
        };
        let missing = unsatisfied(doc, &effective);
        if !missing.is_empty() {
            out.excluded.push(ExcludedImage {
                image: image.clone().into(),
                unsatisfied: missing,
            });
            continue;
        }
        let surplus = doc
            .predicates
            .keys()
            .filter(|p| !effective.contains_key(*p))
            .count();
        let preferred = prefers.len() - unsatisfied(doc, prefers).len();
        out.compatible.push(RankedImage {
            image: image.clone().into(),
            surplus,
            preferred,
            default: false,
        });
    }
    out.compatible.sort_by(|a, b| {
        b.preferred
            .cmp(&a.preferred)
            .then(a.surplus.cmp(&b.surplus))
            .then(a.image.name.cmp(&b.image.name))
            .then(a.image.digest.cmp(&b.image.digest))
    });
    if let Some(first) = out
        .compatible
        .iter_mut()
        .find(|r| r.image.tags.iter().any(|t| t == PROMOTED_TAG))
    {
        first.default = true;
    }
    out.excluded.sort_by(|a, b| {
        a.image
            .name
            .cmp(&b.image.name)
            .then(a.image.digest.cmp(&b.image.digest))
    });
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible::manifest::Harness;
    use crucible_capability::{CAPABILITIES_SCHEMA, CapabilityDoc};

    use crate::images::model::CatalogImage;
    use crate::playbooks::dispatch::PackAgent;
    use crate::playbooks::preflight::{
        HarnessSource, ReferencePin, ResolvedHarness, preflight, split_reference,
    };

    const REPO: &str = "ghcr.io/acme/sandbox-go-cc";
    const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn image(predicates: &[(&str, &str)]) -> CatalogImage {
        CatalogImage {
            repository: REPO.into(),
            digest: DIGEST.into(),
            tags: vec!["latest".into(), "abc123".into()],
            arches: vec!["amd64".into()],
            created_at: None,
            capabilities: Some(CapabilityDoc {
                features: vec!["base".into(), "go".into(), "claude-code".into()],
                image: "sandbox-go-cc".into(),
                predicates: predicates
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                schema: CAPABILITIES_SCHEMA.into(),
            }),
            capability_digest: Some("sha256:cap".into()),
            intro_digest: None,
            first_seen: String::new(),
            last_seen: String::new(),
        }
    }

    fn agent(reference: &str, requires: &[(&str, &str)]) -> PackAgent {
        PackAgent {
            requires: requires
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..PackAgent::new("openshell", Some(reference.to_string()))
        }
    }

    fn pinned(harness: Harness) -> ResolvedHarness {
        ResolvedHarness {
            harness,
            provider: "vertex".into(),
            source: HarnessSource::Pin,
        }
    }

    fn defaulted(harness: Harness) -> ResolvedHarness {
        ResolvedHarness {
            harness,
            provider: "openai".into(),
            source: HarnessSource::Default,
        }
    }

    #[test]
    fn references_split_into_repository_and_pin() {
        assert_eq!(
            split_reference("ghcr.io/acme/x@sha256:ab"),
            (
                "ghcr.io/acme/x".into(),
                ReferencePin::Digest("sha256:ab".into())
            )
        );
        assert_eq!(
            split_reference("ghcr.io/acme/x:1.2"),
            ("ghcr.io/acme/x".into(), ReferencePin::Tag("1.2".into()))
        );
        assert_eq!(
            split_reference("localhost:5000/acme/x"),
            (
                "localhost:5000/acme/x".into(),
                ReferencePin::Tag("latest".into())
            )
        );
        assert_eq!(
            split_reference("localhost:5000/acme/x:dev"),
            (
                "localhost:5000/acme/x".into(),
                ReferencePin::Tag("dev".into())
            )
        );
    }

    #[test]
    fn a_matching_image_passes_and_records_its_digests() {
        let catalog = vec![image(&[
            ("toolchain.go", "1.25.11"),
            ("agent.claude-code", "2.1.270"),
        ])];
        let out = preflight(
            &agent(&format!("{REPO}:latest"), &[("toolchain.go", ">=1.25")]),
            Some(&pinned(Harness::Claude)),
            &catalog,
        );
        assert!(!out.refused(), "{out:?}");
        assert!(out.checked && out.catalogued && out.verified && !out.overridden);
        assert_eq!(out.digest.as_deref(), Some(DIGEST));
        assert_eq!(out.capability_digest.as_deref(), Some("sha256:cap"));
        assert!(out.unsatisfied.is_empty());
    }

    #[test]
    fn a_digest_reference_matches_by_digest() {
        let catalog = vec![image(&[("agent.claude-code", "2.1.270")])];
        let out = preflight(&agent(&format!("{REPO}@{DIGEST}"), &[]), None, &catalog);
        assert!(out.catalogued && !out.refused(), "{out:?}");
        let out = preflight(&agent(&format!("{REPO}@sha256:0000"), &[]), None, &catalog);
        assert!(!out.catalogued && out.refused());
    }

    #[test]
    fn an_unsatisfied_requirement_is_refused_naming_the_predicate() {
        let catalog = vec![image(&[
            ("toolchain.go", "1.25.11"),
            ("agent.claude-code", "2.1.270"),
        ])];
        let out = preflight(
            &agent(
                &format!("{REPO}:latest"),
                &[("toolchain.go", ">=1.26"), ("toolchain.cuda", ">=13")],
            ),
            None,
            &catalog,
        );
        assert!(out.refused());
        assert_eq!(out.unsatisfied.len(), 2);
        assert!(
            out.refusals[0].contains("lacks toolchain.cuda"),
            "{:?}",
            out.refusals
        );
        assert!(
            out.refusals[1].contains("has toolchain.go 1.25.11, the pack requires >=1.26"),
            "{:?}",
            out.refusals
        );
    }

    #[test]
    fn the_resolved_harness_implies_its_agent_predicate() {
        let catalog = vec![image(&[("agent.claude-code", "2.1.270")])];
        let claude_only = agent(&format!("{REPO}:latest"), &[]);
        assert!(!preflight(&claude_only, Some(&pinned(Harness::Claude)), &catalog).refused());
        let out = preflight(&claude_only, Some(&pinned(Harness::Codex)), &catalog);
        assert!(out.refused());
        assert!(
            out.refusals[0].contains("lacks agent.codex, which harness codex needs"),
            "{:?}",
            out.refusals
        );
        // Hermes implies nothing the catalog can vouch for.
        assert!(!preflight(&claude_only, Some(&pinned(Harness::Hermes)), &catalog).refused());
    }

    #[test]
    fn a_dispatch_default_may_not_contradict_a_declared_harness_but_a_pin_may() {
        let catalog = vec![image(&[
            ("agent.claude-code", "2.1.270"),
            ("agent.codex", "0.154.0"),
        ])];
        let declared = PackAgent {
            harness: Some("claude".into()),
            ..agent(&format!("{REPO}:latest"), &[])
        };
        let out = preflight(&declared, Some(&defaulted(Harness::Codex)), &catalog);
        assert!(out.refused());
        assert!(
            out.refusals[0].contains("pin a provider explicitly"),
            "{:?}",
            out.refusals
        );
        assert!(!preflight(&declared, Some(&pinned(Harness::Codex)), &catalog).refused());
        assert!(!preflight(&declared, Some(&defaulted(Harness::Claude)), &catalog).refused());
    }

    #[test]
    fn an_uncatalogued_or_unverified_image_needs_the_override() {
        let catalog = vec![image(&[("agent.claude-code", "2.1.270")])];
        let custom = agent("quay.io/acme/custom:dev", &[]);
        let out = preflight(&custom, None, &catalog);
        assert!(out.checked && !out.catalogued && out.refused());
        assert!(out.refusals[0].contains("is not in the image catalog"));

        let overridden = PackAgent {
            allow_unverified_image: true,
            ..custom.clone()
        };
        let out = preflight(&overridden, None, &catalog);
        assert!(!out.refused() && out.overridden);
        assert_eq!(out.warnings.len(), 1);

        let mut unverified = image(&[]);
        unverified.capabilities = None;
        unverified.capability_digest = None;
        let out = preflight(&agent(&format!("{REPO}:latest"), &[]), None, &[unverified]);
        assert!(out.catalogued && !out.verified && out.refused());
        assert!(out.refusals[0].contains("carries no capability document"));
    }

    fn named(name: &str, tags: &[&str], predicates: &[(&str, &str)]) -> CatalogImage {
        let mut img = image(predicates);
        img.repository = format!("ghcr.io/acme/{name}");
        img.digest = format!("sha256:{}", name.len());
        img.tags = tags.iter().map(|t| t.to_string()).collect();
        img
    }

    #[test]
    fn ranking_prefers_the_slimmest_compatible_promoted_image_and_names_exclusions() {
        let catalog = vec![
            named(
                "sandbox-omnibus",
                &["latest"],
                &[
                    ("toolchain.go", "1.25.11"),
                    ("toolchain.rust", "1.90"),
                    ("domain.vllm-dev", "0.28.0"),
                    ("agent.claude-code", "2.1.270"),
                    ("agent.codex", "0.154.0"),
                ],
            ),
            named(
                "sandbox-go-cc",
                &["latest"],
                &[
                    ("toolchain.go", "1.25.11"),
                    ("agent.claude-code", "2.1.270"),
                ],
            ),
            named(
                "sandbox-go-cc-old",
                &["abc123"],
                &[
                    ("toolchain.go", "1.25.11"),
                    ("agent.claude-code", "2.1.270"),
                ],
            ),
            named(
                "sandbox-rust-cc",
                &["latest"],
                &[("toolchain.rust", "1.90"), ("agent.claude-code", "2.1.270")],
            ),
        ];
        let mut unverified = named("custom", &["latest"], &[]);
        unverified.capabilities = None;
        let mut catalog = catalog;
        catalog.push(unverified);

        let requires: BTreeMap<String, String> =
            [("toolchain.go".to_string(), ">=1.25".to_string())]
                .into_iter()
                .collect();
        let ranked = super::rank(&catalog, &requires, &BTreeMap::new(), Harness::Claude);
        let names: Vec<_> = ranked
            .compatible
            .iter()
            .map(|r| r.image.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["sandbox-go-cc", "sandbox-go-cc-old", "sandbox-omnibus"],
            "{ranked:?}"
        );
        assert!(ranked.compatible[0].default);
        assert!(!ranked.compatible[1].default && !ranked.compatible[2].default);
        assert_eq!(ranked.compatible[0].surplus, 0);
        assert_eq!(ranked.compatible[2].surplus, 3);
        assert_eq!(ranked.excluded.len(), 1);
        assert_eq!(ranked.excluded[0].image.name, "sandbox-rust-cc");
        assert_eq!(ranked.excluded[0].unsatisfied[0].predicate, "toolchain.go");
        assert_eq!(ranked.unverified.len(), 1);

        // Preferences reorder within the compatible set without excluding anything.
        let prefers: BTreeMap<String, String> = [("domain.vllm-dev".to_string(), "*".to_string())]
            .into_iter()
            .collect();
        let ranked = super::rank(&catalog, &requires, &prefers, Harness::Claude);
        assert_eq!(ranked.compatible[0].image.name, "sandbox-omnibus");
        assert_eq!(ranked.compatible.len(), 3);

        // The harness implies its predicate: codex excludes the claude-only images.
        let ranked = super::rank(&catalog, &requires, &BTreeMap::new(), Harness::Codex);
        let names: Vec<_> = ranked
            .compatible
            .iter()
            .map(|r| r.image.name.as_str())
            .collect();
        assert_eq!(names, vec!["sandbox-omnibus"]);
        assert!(ranked.excluded.iter().any(
            |e| e.image.name == "sandbox-go-cc" && e.unsatisfied[0].predicate == "agent.codex"
        ));
    }

    #[test]
    fn an_empty_catalog_checks_nothing_and_says_so() {
        let out = preflight(&agent(&format!("{REPO}:latest"), &[]), None, &[]);
        assert!(!out.checked && !out.refused());
        assert_eq!(out.warnings.len(), 1);
        // No image at all: nothing to check, nothing to say.
        let out = preflight(&PackAgent::new("local", None), None, &[]);
        assert!(out.warnings.is_empty() && !out.refused());
        let _ = BTreeMap::<String, String>::new();
    }
}
