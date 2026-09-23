#![allow(clippy::disallowed_macros)]

use crate::client::Db;
use crate::config::ControllerCfg;
#[cfg(feature = "autoresearch")]
use crate::issues::engine;
use crate::playbooks::providers::AgentSelection;
use anyhow::Result;
#[cfg(feature = "autoresearch")]
use crucible_contract::{ArtifactKind, ArtifactRef, Envelope, EnvelopeKind, content_digest};
use k8s_openapi::api::core::v1::{ConfigMap, Container, EnvVar, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "autoresearch")]
#[test]
fn turn_specs_convert_bare_repo_slugs_to_clone_urls() {
    let scope = crate::runs::workpod::WorkPodSpec::scope(
        "p".into(),
        "llm-d/llm-d-router#1878".into(),
        "llm-d/llm-d-router".into(),
        5.0,
        "img".into(),
        1,
        false,
        TurnInputs::default(),
    );
    assert_eq!(scope.repo_url, "https://github.com/llm-d/llm-d-router.git");
    let rank = crate::runs::workpod::WorkPodSpec::grounded_rank(
        "p".into(),
        "o/r#1".into(),
        "https://example.com/x.git".into(),
        1.0,
        "img".into(),
        None,
    );
    assert_eq!(
        rank.repo_url, "https://example.com/x.git",
        "URLs pass through"
    );
}
use super::*;
#[cfg(feature = "autoresearch")]
use crate::runs::workpod::spec::TurnSpec as _;
#[cfg(feature = "autoresearch")]
use crucible::deploy::ProposeTier;
#[cfg(feature = "autoresearch")]
use crucible_contract::{Disposition, Tier};
use std::sync::Mutex;

#[test]
fn work_kind_label_and_cost_tag_round_trip() {
    let k = WorkKind::AgentTurn(TurnKind::GroundedRank);
    assert_eq!(k.label_value(), "grounded-rank");
    assert_eq!(k.cost_tag(), "rank-grounded");
    assert_eq!(WorkKind::parse_label("grounded-rank").unwrap(), k);
    assert!(WorkKind::parse_label("bogus").is_err());

    // The loop-run kind: label + cost tag both `run`, round-tripping through the label.
    assert_eq!(WorkKind::Run.label_value(), "run");
    assert_eq!(WorkKind::Run.cost_tag(), "run");
    assert_eq!(WorkKind::parse_label("run").unwrap(), WorkKind::Run);
}

#[test]
fn work_pod_state_round_trips_and_flags_terminal() {
    for s in [
        WorkPodState::Queued,
        WorkPodState::Running,
        WorkPodState::Succeeded,
        WorkPodState::Failed,
        WorkPodState::Collected,
        WorkPodState::Swept,
    ] {
        assert_eq!(WorkPodState::parse(s.as_str()).unwrap(), s);
    }
    assert!(WorkPodState::Succeeded.is_terminal());
    assert!(WorkPodState::Failed.is_terminal());
    assert!(!WorkPodState::Running.is_terminal());
    assert!(WorkPodState::parse("nope").is_err());
}

#[cfg(feature = "autoresearch")]
#[test]
fn admit_respects_both_cap_and_daily_budget() {
    // Under both → spawn.
    assert_eq!(admit(0, 4, 0, 50), Admission::Spawn);
    assert_eq!(admit(3, 4, 49, 50), Admission::Spawn);
    // At the concurrency cap → queue.
    assert_eq!(admit(4, 4, 0, 50), Admission::Queue);
    // At the daily budget → queue, even with a free slot.
    assert_eq!(admit(0, 4, 50, 50), Admission::Queue);
    // A zero cap/budget always queues (a hard off switch).
    assert_eq!(admit(0, 0, 0, 50), Admission::Queue);
    assert_eq!(admit(0, 4, 0, 0), Admission::Queue);
}

#[cfg(feature = "autoresearch")]
#[test]
fn pod_name_is_dns_safe_unique_and_bounded() {
    let n = grounded_rank_pod_name("owner/repo#42");
    assert!(n.starts_with("crucible-turn-owner-repo-42-"));
    assert!(n.len() <= 63, "DNS-1123 label ≤63: {n} ({})", n.len());
    assert!(
        n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "DNS-1123 body only: {n}"
    );
    assert!(!n.starts_with('-') && !n.ends_with('-'));
    // A very long org/repo can't blow the 63 budget.
    let long = grounded_rank_pod_name(&format!("{}/{}#1", "a".repeat(80), "b".repeat(80)));
    assert!(long.len() <= 63, "{long} ({})", long.len());
}

#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_is_the_render_turn_contract() {
    let spec = WorkPodSpec::grounded_rank(
        "crucible-turn-owner-repo-42-abcd".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        5.0,
        "ghcr.io/example/sandbox:latest".to_string(),
        None,
    );
    let opts = spec.turn_opts(None).expect("a rank turn always renders");
    assert_eq!(opts.kind, crucible::deploy::TurnKind::Rank);
    assert_eq!(opts.name, "crucible-turn-owner-repo-42-abcd");
    assert_eq!(opts.issue, "owner/repo#42");
    assert_eq!(opts.repo_url, "https://github.com/owner/repo.git");
    assert_eq!(opts.sandbox_image, "ghcr.io/example/sandbox:latest");
    assert_eq!(opts.max_cost, 5.0);
    assert!(opts.digests.is_none(), "no resolver, no pinning");
    assert_eq!(opts.tier, None, "a rank turn never carries a tier");
    assert!(
        !opts.skip_gaming_review,
        "a rank turn never skips a review it never runs"
    );
    assert!(!opts.authoritative);
    assert_eq!(opts.goal_text, None);
    assert_eq!(opts.harness, None);
    assert_eq!(opts.model, None);
}

/// A scope turn renders the harness and model the issue's dispatch resolved; an issue that
/// resolved none renders neither flag, which is the pre-registry turn pod.
#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_scope_carries_the_resolved_harness_and_model() {
    let scope_with = |agent: AgentSelection| {
        WorkPodSpec::scope(
            "crucible-scope-owner-repo-42-abcd".to_string(),
            "owner/repo#42".to_string(),
            "https://github.com/owner/repo.git".to_string(),
            8.0,
            "img".to_string(),
            1,
            false,
            TurnInputs {
                agent,
                ..TurnInputs::default()
            },
        )
        .turn_opts(None)
        .expect("renders")
    };

    let unresolved = scope_with(AgentSelection::default());
    assert_eq!(unresolved.harness, None);
    assert_eq!(unresolved.model, None);

    let resolved = scope_with(AgentSelection {
        harness: Some(crucible::manifest::Harness::Codex),
        model: Some("gpt-5.6-luna".to_string()),
    });
    assert_eq!(resolved.harness, Some(crucible::manifest::Harness::Codex));
    assert_eq!(resolved.model.as_deref(), Some("gpt-5.6-luna"));
}

#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_scope_carries_kind_tier_and_the_gaming_bound() {
    let spec = WorkPodSpec::scope(
        "crucible-scope-owner-repo-42-abcd".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "ghcr.io/example/sandbox:latest".to_string(),
        2,
        false,
        TurnInputs {
            tier: Some(Tier::T1),
            ..TurnInputs::default()
        },
    );
    let opts = spec.turn_opts(None).expect("renders");
    assert_eq!(opts.kind, crucible::deploy::TurnKind::Scope);
    assert_eq!(opts.max_cost, 8.0, "max-cost round-trips");
    assert_eq!(
        opts.tier,
        Some(ProposeTier::T1),
        "the confirmed tier rides into the render"
    );
    assert_eq!(
        opts.gaming_refine_rounds, 2,
        "the effective bound round-trips"
    );
    assert!(
        !opts.skip_gaming_review,
        "skip_gaming_review = false leaves the review on"
    );

    // No tier -> none forwarded (the engine's t0 default applies in-pod).
    let untiered = WorkPodSpec::scope(
        "p".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs::default(),
    );
    assert_eq!(untiered.turn_opts(None).expect("renders").tier, None);

    // A tier with no engine-side spelling (T3) is filtered, not forwarded.
    let t3 = WorkPodSpec::scope(
        "p".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs {
            tier: Some(Tier::T3),
            ..TurnInputs::default()
        },
    );
    assert_eq!(
        t3.turn_opts(None).expect("renders").tier,
        None,
        "t3 has no engine spelling and is filtered"
    );
}

#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_scope_skip_gaming_review_rides_the_flag() {
    let spec = WorkPodSpec::scope(
        "p".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        2,
        true,
        TurnInputs::default(),
    );
    let opts = spec.turn_opts(None).expect("renders");
    assert!(
        opts.skip_gaming_review,
        "skip_gaming_review rides when the knob is on"
    );
    assert!(
        !opts.authoritative,
        "an ordinary scenario is not authoritative"
    );
}

#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_scope_forwards_the_goal_text_and_authoritative() {
    let spec = WorkPodSpec::scope(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs {
            goal_text: Some("the ledgered brief".to_string()),
            authoritative: true,
            ..TurnInputs::default()
        },
    );
    let opts = spec.turn_opts(None).expect("renders");
    assert_eq!(opts.goal_text.as_deref(), Some("the ledgered brief"));
    assert!(opts.authoritative, "authoritative rides the render");
}

/// `repo_ref` is the whole point of `issues.git_ref`: it rides the render for BOTH turn kinds when
/// the issue pinned a ref, and is absent entirely when it did not (the clone then takes the repo's
/// default branch, which is what every github/jira row does).
#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_forwards_repo_ref_only_when_the_issue_pinned_one() {
    let scope = WorkPodSpec::scope(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs {
            git_ref: Some("nv_dev".to_string()),
            ..TurnInputs::default()
        },
    );
    assert_eq!(
        scope.turn_opts(None).expect("renders").repo_ref.as_deref(),
        Some("nv_dev"),
        "a scope turn clones the pinned ref"
    );

    let rank = WorkPodSpec::grounded_rank(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        1.0,
        "img".to_string(),
        Some("v1.2.3".to_string()),
    );
    assert_eq!(
        rank.turn_opts(None).expect("renders").repo_ref.as_deref(),
        Some("v1.2.3"),
        "a rank turn clones too, so it honours the ref as well"
    );

    let unpinned = WorkPodSpec::scope(
        "p".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs::default(),
    );
    assert_eq!(
        unpinned.turn_opts(None).expect("renders").repo_ref,
        None,
        "a NULL git_ref forwards no ref at all, not an empty one"
    );
    let unpinned_rank = WorkPodSpec::grounded_rank(
        "p".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        1.0,
        "img".to_string(),
        None,
    );
    assert_eq!(
        unpinned_rank.turn_opts(None).expect("renders").repo_ref,
        None,
        "same for a rank turn"
    );
}

/// A codegen contract asks the scope turn for a broker-measured render, which the linked engine's
/// `TurnOpts` cannot express: the dispatch refuses by name rather than dropping the contract, a
/// scope turn without one renders, and a rank turn ignores the field entirely (it measures
/// nothing, so a broker contract is meaningless to it).
#[cfg(feature = "autoresearch")]
#[test]
fn turn_opts_refuses_a_scope_turn_under_a_codegen_contract() {
    let scope_with = WorkPodSpec::scope(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs {
            codegen_contract: Some("deepgemm".to_string()),
            ..TurnInputs::default()
        },
    );
    let Err(err) = scope_with.turn_opts(None) else {
        panic!("a contract the engine cannot render is refused");
    };
    assert_eq!(
        err,
        UnsupportedTurnOption::BrokerMeasure {
            contract: "deepgemm".to_string()
        }
    );
    assert!(
        err.to_string().contains("deepgemm") && err.to_string().contains("broker-measure"),
        "the refusal names the contract and the missing capability: {err}"
    );

    let scope_without = WorkPodSpec::scope(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        8.0,
        "img".to_string(),
        1,
        false,
        TurnInputs::default(),
    );
    assert!(
        scope_without.turn_opts(None).is_ok(),
        "no contract, no refusal"
    );

    let rank = WorkPodSpec::grounded_rank(
        "p".to_string(),
        "scenario:abc".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        1.0,
        "img".to_string(),
        None,
    );
    assert!(rank.turn_opts(None).is_ok());
}

#[cfg(feature = "autoresearch")]
#[test]
fn stamp_pod_adds_issue_annotation_selector_and_owner() {
    let spec = WorkPodSpec::grounded_rank(
        "crucible-turn-owner-repo-42-abcd".to_string(),
        "owner/repo#42".to_string(),
        "https://github.com/owner/repo.git".to_string(),
        5.0,
        "img".to_string(),
        None,
    );
    let mut pod = Pod::default();
    let owner = OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "Deployment".to_string(),
        name: "crucible-controller".to_string(),
        uid: "abc-123".to_string(),
        controller: Some(true),
        block_owner_deletion: None,
    };
    stamp_pod(&mut pod, &spec, Some(owner));

    let ann = pod.metadata.annotations.unwrap();
    assert_eq!(
        ann.get(crate::daemon::ISSUE_KEY_ANNOTATION)
            .map(String::as_str),
        Some("owner/repo#42"),
        "the exact key round-trips on the annotation"
    );
    let labels = pod.metadata.labels.unwrap();
    assert_eq!(
        labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str),
        Some("crucible"),
        "the pod-watch selector"
    );
    assert_eq!(
        labels
            .get(crate::daemon::ISSUE_KEY_LABEL)
            .map(String::as_str),
        Some("owner-repo-42"),
        "the lossy label hint"
    );
    let owners = pod.metadata.owner_references.unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].uid, "abc-123");
    assert_eq!(owners[0].controller, Some(true));
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_verdict_logs_scrapes_the_marker_past_noise() {
    let logs = "\
Trying to pull ghcr.io/example/sandbox...
Cloning into '/tmp/crucible-turn-checkout'...
inspecting the checkout, grepping tests
CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"needs a new bench in bench/\",\"confidence\":\"low\",\"cost_usd\":0.42,\"over_budget\":false}
";
    let v = parse_verdict_logs(logs).expect("verdict");
    assert_eq!(v.disposition, Disposition::Tier(Tier::T1));
    assert_eq!(v.confidence.as_deref(), Some("low"));
    assert!((v.cost_usd - 0.42).abs() < 1e-9);
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_verdict_logs_takes_the_last_marker_and_surfaces_error_objects() {
    // Two markers (a retried turn): the LAST wins.
    let logs = "CRUCIBLE_VERDICT: {\"tier\":\"N\",\"rationale\":\"x\",\"cost_usd\":0.1}\nnoise\nCRUCIBLE_VERDICT: {\"tier\":\"stale\",\"rationale\":\"already done in src/a.rs\",\"cost_usd\":0.2}\n";
    let v = parse_verdict_logs(logs).expect("verdict");
    assert_eq!(v.disposition, Disposition::Stale);

    // An error-object marker (no verdict) surfaces as Err — the caller keeps the text tier.
    let logs =
        "CRUCIBLE_VERDICT: {\"error\":\"no verdict\",\"cost_usd\":0.3,\"over_budget\":false}\n";
    assert!(parse_verdict_logs(logs).is_err());

    // No marker at all → Err.
    assert!(parse_verdict_logs("just podman noise, no marker\n").is_err());
}

/// Round-trip a verdict through a Tier 1 envelope the way a terminated container's message
/// carries it: the engine's self-capped `to_capped_json` on one side, `collect_verdict` on the
/// other, no logs at all (the message is authoritative).
#[cfg(feature = "autoresearch")]
#[test]
fn verdict_rides_the_termination_message_envelope() {
    let payload = serde_json::json!({
        "tier": "T1",
        "rationale": "needs a new bench in bench/",
        "confidence": "low",
        "cost_usd": 0.42,
        "over_budget": false,
    });
    let msg = Envelope::new(EnvelopeKind::Verdict, payload)
        .to_capped_json()
        .expect("cap");
    let v = collect_verdict(Some(&msg), "").expect("verdict from the termination message");
    assert_eq!(v.disposition, Disposition::Tier(Tier::T1));
    assert_eq!(v.confidence.as_deref(), Some("low"));
    assert!((v.cost_usd - 0.42).abs() < 1e-9);

    // An error-object payload is a definitive no-verdict, even though the message parsed: the
    // envelope is authoritative, so this must NOT silently fall through to a (missing) marker.
    let err_msg = Envelope::new(
        EnvelopeKind::Verdict,
        serde_json::json!({"error": "no verdict", "cost_usd": 0.3}),
    )
    .to_capped_json()
    .expect("cap");
    assert!(collect_verdict(Some(&err_msg), "").is_err());
}

/// The phase-1 compat fallback: an absent, foreign, or wrong-kind termination message drops
/// through to the marker log scrape (an old engine image emits only the marker).
#[cfg(feature = "autoresearch")]
#[test]
fn verdict_falls_back_to_the_marker_when_the_message_is_not_our_envelope() {
    let logs =
        "podman noise\nCRUCIBLE_VERDICT: {\"tier\":\"N\",\"rationale\":\"x\",\"cost_usd\":0.1}\n";

    // No message at all (old image / a pod that wrote nothing) → marker.
    let v = collect_verdict(None, logs).expect("verdict from marker");
    assert_eq!(v.disposition, Disposition::Tier(Tier::N));

    // A non-envelope message (stray log text on the file) → marker.
    let v = collect_verdict(Some("not json at all"), logs).expect("verdict from marker");
    assert_eq!(v.disposition, Disposition::Tier(Tier::N));

    // A well-formed but WRONG-KIND envelope (a scope report where a verdict is expected) →
    // marker, rather than a hard error that would drop the recoverable marker verdict.
    let foreign = Envelope::new(
        EnvelopeKind::ScopeReport,
        serde_json::json!({"stages": [], "digest": null, "cost": 0.1}),
    )
    .to_capped_json()
    .expect("cap");
    let v = collect_verdict(Some(&foreign), logs).expect("verdict from marker");
    assert_eq!(v.disposition, Disposition::Tier(Tier::N));
}

/// A scope report rides the termination message while its pack + transcript still ride their log
/// markers: the report core comes from the envelope, the two blobs attach from the logs exactly
/// as the pure marker path does.
#[cfg(feature = "autoresearch")]
#[test]
fn scope_report_rides_the_message_with_pack_and_transcript_from_logs() {
    use base64::Engine as _;
    use std::io::Write as _;

    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(b"{\"kind\":\"note\",\"msg\":\"round 1\"}\n")
        .unwrap();
    let transcript_gz = enc.finish().unwrap();
    let transcript_b64 = base64::engine::general_purpose::STANDARD.encode(&transcript_gz);
    let pack_tgz = sample_pack_tgz();
    let pack_b64 = base64::engine::general_purpose::STANDARD.encode(&pack_tgz);
    let logs = format!(
        "podman noise\n{SCOPE_TRANSCRIPT_MARKER} {transcript_b64}\n{SCOPE_PACK_MARKER} {pack_b64}\n"
    );

    let payload = serde_json::json!({
        "stages": [{"name": "freeze", "passed": true, "detail": "ok"}],
        "digest": "v1:beef",
        "cost": 0.1,
    });
    let msg = Envelope::new(EnvelopeKind::ScopeReport, payload)
        .to_capped_json()
        .expect("cap");

    let report = collect_scope_report(Some(&msg), &logs).expect("report from the message");
    assert_eq!(report.digest.as_deref(), Some("v1:beef"));
    assert!(report.survived());
    assert!(
        !report.raw.is_empty(),
        "raw carries the report JSON for the store"
    );
    assert_eq!(report.transcript_gz, Some(transcript_gz));
    assert_eq!(report.pack_tgz, Some(pack_tgz));
    assert!(report.pack_error.is_none());
}

/// Wrong-kind / absent message on the scope path also falls back to the marker scrape.
#[cfg(feature = "autoresearch")]
#[test]
fn scope_report_falls_back_to_the_marker_when_the_message_is_not_our_envelope() {
    let logs = format!(
        "{SCOPE_REPORT_MARKER} {{\"stages\":[{{\"name\":\"freeze\",\"passed\":true,\"detail\":\"ok\"}}],\"digest\":\"v1:cafe\",\"cost\":0.2}}\n"
    );
    let report = collect_scope_report(None, &logs).expect("report from marker");
    assert_eq!(report.digest.as_deref(), Some("v1:cafe"));

    // A verdict envelope where a scope report is expected → marker fallback.
    let foreign = Envelope::new(
        EnvelopeKind::Verdict,
        serde_json::json!({"tier": "T1", "rationale": "x"}),
    )
    .to_capped_json()
    .expect("cap");
    let report = collect_scope_report(Some(&foreign), &logs).expect("report from marker");
    assert_eq!(report.digest.as_deref(), Some("v1:cafe"));
}

/// Build a minimal surviving ScopeReport (digest frozen, one passed stage) for the drop-box
/// fold tests.
#[cfg(feature = "autoresearch")]
fn survived_report() -> engine::ScopeReport {
    let env = Envelope::new(
        EnvelopeKind::ScopeReport,
        serde_json::json!({
            "stages": [{"name": "freeze", "passed": true, "detail": "ok"}],
            "digest": "v1:beef",
            "cost": 0.1,
        }),
    )
    .to_capped_json()
    .expect("cap");
    collect_scope_report(Some(&env), "").expect("report")
}

#[cfg(feature = "autoresearch")]
async fn drop_artifact(pool: &sqlx::PgPool, pod: &str, kind: ArtifactKind, bytes: &[u8]) {
    crate::runs::blob_store::put_artifact_bytes(
        pool,
        &crate::runs::blob_store::ArtifactOwner::PodEvidence {
            pod: pod.to_string(),
        },
        kind.as_str(),
        u64::MAX,
        bytes.to_vec(),
    )
    .await
    .expect("store artifact");
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dropbox_pack_is_preferred_and_digest_validated(pool: sqlx::PgPool) {
    let pack = b"the-real-gzipped-pack";
    drop_artifact(&pool, "pod-x", ArtifactKind::ScopePack, pack).await;
    let manifest = vec![ArtifactRef {
        kind: ArtifactKind::ScopePack,
        digest: content_digest(pack),
        bytes: pack.len() as u64,
        delivered: true,
    }];
    let mut report = survived_report();
    apply_dropbox_artifacts(&mut report, &manifest, &pool, "pod-x")
        .await
        .expect("valid drop-box pack accepted");
    assert_eq!(report.pack_tgz.as_deref(), Some(&pack[..]));
    assert!(report.pack_error.is_none());
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn survived_scope_with_a_missing_delivered_pack_fails_loudly(pool: sqlx::PgPool) {
    // Manifest claims delivered:true but nothing is stored.
    let manifest = vec![ArtifactRef {
        kind: ArtifactKind::ScopePack,
        digest: "sha256:whatever".to_string(),
        bytes: 10,
        delivered: true,
    }];
    let mut report = survived_report();
    let err = apply_dropbox_artifacts(&mut report, &manifest, &pool, "pod-x")
        .await
        .expect_err("a survival's missing pack is fatal");
    assert!(err.contains("scope pack not recovered"), "{err}");
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_digest_mismatch_fails_loudly(pool: sqlx::PgPool) {
    drop_artifact(&pool, "pod-x", ArtifactKind::ScopePack, b"actual-bytes").await;
    let manifest = vec![ArtifactRef {
        kind: ArtifactKind::ScopePack,
        digest: content_digest(b"different-bytes"),
        bytes: 12,
        delivered: true,
    }];
    let mut report = survived_report();
    let err = apply_dropbox_artifacts(&mut report, &manifest, &pool, "pod-x")
        .await
        .expect_err("digest mismatch is fatal for a survival");
    assert!(err.contains("integrity mismatch"), "{err}");
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_undelivered_pack_on_a_survival_fails_loudly(pool: sqlx::PgPool) {
    let manifest = vec![ArtifactRef {
        kind: ArtifactKind::ScopePack,
        digest: "sha256:x".to_string(),
        bytes: 1,
        delivered: false,
    }];
    let mut report = survived_report();
    let err = apply_dropbox_artifacts(&mut report, &manifest, &pool, "pod-x")
        .await
        .expect_err("delivered:false pack on a survival is fatal");
    assert!(err.contains("delivered:false"), "{err}");
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_transcript_problem_is_best_effort_not_fatal(pool: sqlx::PgPool) {
    // A valid pack so the survival requirement is met; the transcript is not stored.
    let pack = b"pack-bytes";
    drop_artifact(&pool, "pod-x", ArtifactKind::ScopePack, pack).await;
    let manifest = vec![
        ArtifactRef {
            kind: ArtifactKind::ScopePack,
            digest: content_digest(pack),
            bytes: pack.len() as u64,
            delivered: true,
        },
        ArtifactRef {
            kind: ArtifactKind::ScopeTranscript,
            digest: "sha256:missing".to_string(),
            bytes: 5,
            delivered: true,
        },
    ];
    let mut report = survived_report();
    // No error: the missing transcript is logged, not fatal.
    apply_dropbox_artifacts(&mut report, &manifest, &pool, "pod-x")
        .await
        .expect("transcript problem is best-effort");
    assert_eq!(report.pack_tgz.as_deref(), Some(&pack[..]));
    assert!(
        report.transcript_gz.is_none(),
        "missing transcript stays absent"
    );
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_empty_manifest_leaves_the_report_untouched(pool: sqlx::PgPool) {
    let mut report = survived_report();
    apply_dropbox_artifacts(&mut report, &[], &pool, "pod-x")
        .await
        .expect("no-op");
    assert!(report.pack_tgz.is_none());
}

/// End to end over the dispatcher boundary: a terminated pod whose `state.terminated.message`
/// carries the envelope is adopted into a Report/Verdict without any marker in its logs — the
/// same read path startup adoption and the completion watch both drive.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adoption_reads_the_result_from_pod_status(pool: sqlx::PgPool) -> Result<()> {
    let created = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));

    // Scope adoption: the report rides the termination message, logs are empty (no marker).
    let scope_msg = Envelope::new(
        EnvelopeKind::ScopeReport,
        serde_json::json!({
            "stages": [{"name": "freeze", "passed": true, "detail": "ok"}],
            "digest": "v1:beef",
            "cost": 0.1,
        }),
    )
    .to_capped_json()
    .expect("cap");
    let scope_dispatcher = FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: Some(scope_msg),
        logs: String::new(),
        created: created.clone(),
        deleted: deleted.clone(),
    };
    let tmp = tempfile::tempdir().expect("tmp");
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let cfg = pod_cfg(&profile, "img");
    match crate::runs::workpod::spec::ScopeSpec
        .watch(&scope_dispatcher, &cfg, &pool, "hub", "ns", "pod")
        .await?
    {
        crate::runs::workpod::spec::TurnRunOutcome::Result(r) => {
            assert_eq!(r.digest.as_deref(), Some("v1:beef"))
        }
        crate::runs::workpod::spec::TurnRunOutcome::NoResult { reason } => {
            panic!("expected a report, got: {reason}")
        }
    }

    // Verdict adoption: same, on the grounded-rank path.
    let verdict_msg = Envelope::new(
        EnvelopeKind::Verdict,
        serde_json::json!({"tier": "T2", "rationale": "x", "cost_usd": 0.25}),
    )
    .to_capped_json()
    .expect("cap");
    let verdict_dispatcher = FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: Some(verdict_msg),
        logs: String::new(),
        created,
        deleted,
    };
    match crate::runs::workpod::spec::GroundedRankSpec
        .watch(&verdict_dispatcher, &cfg, &pool, "hub", "ns", "pod")
        .await?
    {
        crate::runs::workpod::spec::TurnRunOutcome::Result(v) => {
            assert_eq!(v.disposition, Disposition::Tier(Tier::T2))
        }
        crate::runs::workpod::spec::TurnRunOutcome::NoResult { reason } => {
            panic!("expected a verdict, got: {reason}")
        }
    }
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_scope_report_logs_attaches_the_transcript_marker() {
    use base64::Engine as _;
    use std::io::Write as _;
    let ndjson = "{\"kind\":\"note\",\"msg\":\"round 1: propose turn\"}\n";
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson.as_bytes()).unwrap();
    let gz = enc.finish().unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&gz);
    let logs = format!(
        "podman noise\n{SCOPE_TRANSCRIPT_MARKER} {b64}\n{SCOPE_REPORT_MARKER} {{\"stages\":[],\"digest\":null,\"cost\":0.1}}\n"
    );
    let report = parse_scope_report_logs(&logs).expect("report");
    assert_eq!(report.transcript_gz.as_deref(), Some(gz.as_slice()));
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_scope_report_logs_tolerates_a_missing_or_garbled_transcript() {
    let logs = format!("{SCOPE_REPORT_MARKER} {{\"stages\":[],\"digest\":null,\"cost\":0.1}}\n");
    let report = parse_scope_report_logs(&logs).expect("report");
    assert!(report.transcript_gz.is_none(), "no marker → no transcript");

    let logs = format!(
        "{SCOPE_TRANSCRIPT_MARKER} not@base64!\n{SCOPE_REPORT_MARKER} {{\"stages\":[],\"digest\":null,\"cost\":0.1}}\n"
    );
    let report = parse_scope_report_logs(&logs).expect("report");
    assert!(
        report.transcript_gz.is_none(),
        "a garbled transcript never fails the report"
    );
}

/// A gzip'd tar of a tiny pack tree, built with the same crates the engine emits with — the
/// blob the pack-marker tests and the unpack tests share.
fn sample_pack_tgz() -> Vec<u8> {
    use std::io::Write as _;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
    std::fs::write(dir.path().join("SCOPE.md"), "identity: v1:beef\n").unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(dir.path().join("prompts/goal.md"), "fix the thing\n").unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all(".", dir.path()).unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar_bytes).unwrap();
    enc.finish().unwrap()
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_scope_report_logs_attaches_the_pack_marker() {
    use base64::Engine as _;
    let tgz = sample_pack_tgz();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&tgz);
    let logs = format!(
        "podman noise\n{SCOPE_PACK_MARKER} {b64}\n{SCOPE_REPORT_MARKER} {{\"stages\":[{{\"name\":\"freeze\",\"passed\":true,\"detail\":\"ok\"}}],\"digest\":\"v1:beef\",\"cost\":0.1}}\n"
    );
    let report = parse_scope_report_logs(&logs).expect("report");
    assert_eq!(report.pack_tgz.as_deref(), Some(tgz.as_slice()));
    assert!(report.pack_error.is_none());
}

#[cfg(feature = "autoresearch")]
#[test]
fn parse_scope_pack_logs_distinguishes_absent_error_and_garbled() {
    // No marker at all (a dead proposal, a pre-feature engine): absent, no error.
    assert_eq!(parse_scope_pack_logs("just noise\n"), (None, None));

    // The engine's honest oversize refusal: an {"error":…} payload.
    let logs = format!("{SCOPE_PACK_MARKER} {{\"error\":\"pack tar is over the cap\"}}\n");
    let (blob, err) = parse_scope_pack_logs(&logs);
    assert!(blob.is_none());
    assert!(
        err.as_deref().is_some_and(|e| e.contains("over the cap")),
        "the engine's error rides through: {err:?}"
    );

    // Garbled base64: unusable, with the reason.
    let logs = format!("{SCOPE_PACK_MARKER} not@base64!\n");
    let (blob, err) = parse_scope_pack_logs(&logs);
    assert!(blob.is_none());
    assert!(err.is_some(), "a garbled payload names itself");
}

#[test]
fn unpack_pack_tgz_lands_the_tree_and_replaces_a_stale_dir() {
    let tgz = sample_pack_tgz();
    let dest = tempfile::tempdir().expect("tempdir");
    let out = dest.path().join("pack");
    // A stale pack from a prior scope must not survive the unpack.
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("stale.md"), "old pack leftovers").unwrap();

    crate::playbooks::packs::unpack_pack_tgz(&tgz, &out).expect("unpacks");
    assert_eq!(
        std::fs::read_to_string(out.join("crucible.toml")).expect("manifest landed"),
        "[repo]\nurl = \"x\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("prompts/goal.md")).expect("nested file landed"),
        "fix the thing\n"
    );
    assert!(
        !out.join("stale.md").exists(),
        "the stale pack dir was replaced, not merged into"
    );
}

/// Traversal entries reject the WHOLE pack — the blob came from an agent-authored pod, and a
/// pack that half-unpacked outside the dir must never be trusted.
#[test]
fn unpack_pack_tgz_rejects_traversal_entries() {
    use std::io::Write as _;
    // `tar::Builder::append_data` itself refuses `..` paths, so a hostile archive has to be
    // crafted at the raw-header level — exactly what a malicious pod could emit.
    let evil_tgz = |path: &str| -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        let name = &mut header.as_gnu_mut().expect("gnu header").name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append(&header, "evil".as_bytes())
            .expect("append evil entry");
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    };

    let dest = tempfile::tempdir().expect("tempdir");
    let out = dest.path().join("pack");
    for path in [
        "../escaped.md",
        "nested/../../escaped.md",
        "/tmp/escaped.md",
    ] {
        let err = crate::playbooks::packs::unpack_pack_tgz(&evil_tgz(path), &out)
            .expect_err("traversal must reject");
        assert!(
            format!("{err:#}").contains("escapes"),
            "the rejection names the escape for {path}: {err:#}"
        );
        assert!(
            !dest.path().join("escaped.md").exists(),
            "nothing landed outside the pack dir for {path}"
        );
    }
}

#[test]
fn extract_run_session_takes_the_dump_after_the_last_delimiter() {
    let logs = [
        "podman login noise",
        r#"{"v":1,"kind":"row","row":{"iter":0}}"#, // the live tee copy, superseded by the dump
        "=================== SESSION (rc=1) ===================",
        "stale first attempt",
        "=================== SESSION (rc=0) ===================",
        r#"{"v":1,"kind":"row","row":{"iter":0}}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished"}"#,
    ]
    .join("\n");
    let RunSessionScrape::Found(payload) = extract_run_session_logs(&logs) else {
        panic!("expected Found");
    };
    assert!(
        payload.starts_with(r#"{"v":1,"kind":"row""#),
        "last delimiter wins, nothing before it: {payload}"
    );
    assert!(payload.contains("shutdown"));
}

#[test]
fn extract_run_session_falls_back_to_streamed_lines_without_a_delimiter() {
    // Rotation ate the tail dump: the `--ui=stream` tee lines are all that's left.
    let logs = [
        "Trying to pull ghcr.io/x/sandbox...",
        r#"{"v":1,"kind":"row","row":{"iter":0}}"#,
        "broker: eprintln chatter",
        r#"{"v":1,"kind":"budget","spent":1.0}"#,
    ]
    .join("\n");
    let RunSessionScrape::Found(payload) = extract_run_session_logs(&logs) else {
        panic!("expected Found");
    };
    assert_eq!(
        payload,
        "{\"v\":1,\"kind\":\"row\",\"row\":{\"iter\":0}}\n{\"v\":1,\"kind\":\"budget\",\"spent\":1.0}",
        "only the JSON-shaped lines survive the fallback"
    );

    // A delimiter followed by an EMPTY dump (the session file never landed on the pod's disk)
    // also falls back to the streamed copy instead of returning an empty payload.
    let logs = "{\"v\":1,\"kind\":\"budget\",\"spent\":2.0}\n=== SESSION (rc=1) ===\n";
    let RunSessionScrape::Found(payload) = extract_run_session_logs(logs) else {
        panic!("expected Found (the streamed budget line survives)");
    };
    assert!(payload.contains("budget"));
}

/// The two no-session failure modes are told apart with honest evidence: a delimiter-but-empty
/// dump surfaces the wrapper rc + the pre-delimiter tail (the loop died before publishing), while a
/// wholly missing delimiter surfaces the trailing tail (rotation/truncation). Chatter alone with no
/// delimiter is `NoDelimiter`, never a false "loop failed".
#[test]
fn extract_run_session_distinguishes_the_two_no_session_modes() {
    // No delimiter at all: empty and chatter-only both land on NoDelimiter with a tail.
    assert_eq!(
        extract_run_session_logs(""),
        RunSessionScrape::NoDelimiter {
            tail: String::new()
        }
    );
    match extract_run_session_logs("podman noise\ncrucible: exploded early\n") {
        RunSessionScrape::NoDelimiter { tail } => {
            assert!(
                tail.contains("exploded early"),
                "carries the trailing tail: {tail}"
            );
        }
        other => panic!("expected NoDelimiter, got {other:?}"),
    }
    // Delimiter present but no session lines: the loop died before publishing. Carry rc + the
    // lines BEFORE the delimiter (the actual failure), not the delimiter itself.
    let logs = "reading manifest /opt/crucible/domains/x/crucible.toml: No such file\n\
                    === SESSION (rc=1) ===\n";
    match extract_run_session_logs(logs) {
        RunSessionScrape::DelimiterButNoSession { rc, tail } => {
            assert_eq!(rc, Some(1), "the wrapper exit code is recovered");
            assert!(
                tail.contains("No such file"),
                "the pre-delimiter failure is the evidence: {tail}"
            );
            assert!(
                !tail.contains("SESSION"),
                "the delimiter itself isn't the tail"
            );
        }
        other => panic!("expected DelimiterButNoSession, got {other:?}"),
    }
}

#[test]
fn failed_pod_sweep_respects_the_retention_window() {
    let retention = Duration::from_secs(24 * 60 * 60);
    // 1h after terminal → retained.
    assert!(!failed_pod_should_sweep(
        Some("2026-07-03T10:00:00Z"),
        "2026-07-03T11:00:00Z",
        retention
    ));
    // 25h after terminal → swept.
    assert!(failed_pod_should_sweep(
        Some("2026-07-03T10:00:00Z"),
        "2026-07-04T11:00:00Z",
        retention
    ));
    // No terminal timestamp → swept immediately (nothing to retain against).
    assert!(failed_pod_should_sweep(
        None,
        "2026-07-03T11:00:00Z",
        retention
    ));
}

fn retained_row(pod_name: &str, terminal_at: Option<&str>) -> WorkPodRow {
    WorkPodRow {
        pod_uid: None,
        pod_name: pod_name.to_string(),
        kind: "grounded-rank".to_string(),
        issue_key: Some("owner/repo#1".to_string()),
        state: WorkPodState::Failed,
        cost_tag: "rank-grounded".to_string(),
        result: None,
        error: Some("boom".to_string()),
        created_at: "2026-07-04T00:00:00Z".to_string(),
        updated_at: "2026-07-04T00:00:00Z".to_string(),
        terminal_at: terminal_at.map(str::to_string),
        cluster: "hub".to_string(),
    }
}

#[test]
fn failed_pod_overflow_keeps_the_newest_n() {
    let rows = vec![
        retained_row("pod-a", Some("2026-07-04T01:00:00Z")),
        retained_row("pod-b", Some("2026-07-04T03:00:00Z")),
        retained_row("pod-c", Some("2026-07-04T02:00:00Z")),
        retained_row("pod-d", None),
    ];
    // Keep the newest 2 (b, c); the older a and the stamp-less d overflow, oldest last. Each
    // result pairs the pod name with the cluster recorded on its row.
    assert_eq!(
        failed_pod_overflow(&rows, 2),
        vec![
            ("pod-a".to_string(), "hub".to_string()),
            ("pod-d".to_string(), "hub".to_string()),
        ]
    );
    // Exactly at the cap → nothing overflows.
    assert!(failed_pod_overflow(&rows, 4).is_empty());
    assert!(failed_pod_overflow(&rows, 10).is_empty());
    // keep = 0 → sweep them all.
    assert_eq!(failed_pod_overflow(&rows, 0).len(), 4);
    // Empty input never overflows.
    assert!(failed_pod_overflow(&[], 0).is_empty());
}

/// A genuine boundary double (not a mock-in-the-middle): a scripted [`PodDispatcher`] that
/// records calls and returns canned phases/logs, so the orchestration's accounting + state
/// machine + verdict fold run without a cluster, rendering through the linked engine.
struct FakeDispatcher {
    phase: TurnPhase,
    logs: String,
    /// The kubelet-captured termination message the fake reports; `None` models an old engine
    /// image (marker only) or a pod that wrote nothing.
    message: Option<String>,
    created: Arc<Mutex<Vec<String>>>,
    deleted: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PodDispatcher for FakeDispatcher {
    async fn create(&self, _cluster: &str, _ns: &str, mut pod: Pod) -> Result<Pod> {
        self.created
            .lock()
            .expect("lock")
            .push(pod.metadata.name.clone().unwrap_or_default());
        // Populate a UID like the API server would, so anything owner-ref'ing the created pod (the
        // pack ConfigMap) has one to point at.
        pod.metadata.uid = Some("fake-pod-uid".to_string());
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: self.phase,
            message: self.message.clone(),
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        Ok(self.logs.clone())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, name: &str) -> Result<()> {
        self.deleted.lock().expect("lock").push(name.to_string());
        Ok(())
    }
}

fn pod_cfg(profile: &Path, sandbox: &str) -> ControllerCfg {
    crate::testing::cfg_from_args([
        "ctl",
        "--deploy-profile",
        &profile.to_string_lossy(),
        "--render-no-pin",
        "--grounded-sandbox-image",
        sandbox,
    ])
}

/// The precedence every dispatch site resolves through: what the launch pinned wins, then the
/// GPU-measured contract's route, then the controller's configured default. Late resolution is the
/// point — repointing the default has to move the work that never asked for a cluster, and leave
/// the work that did where it was put.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_dispatch_cluster_prefers_the_pin_then_the_contract_then_the_default(
    pool: sqlx::PgPool,
) -> Result<()> {
    let mut cfg = crate::testing::cfg_from_args(["ctl"]);
    cfg.dispatch_cluster = "hub".to_string();
    cfg.dispatch_cluster_by_contract = vec!["deepgemm=gpu-spoke".to_string()];
    let db = Db::new(pool);

    let plain = crate::issues::model::NewIssue {
        key: "owner/repo#1".to_string(),
        repo: "owner/repo".to_string(),
        priority: 1,
        evidence_url: None,
        title: None,
        author: None,
        body: None,
        labels: Vec::new(),
        upstream_updated_at: None,
    };
    crate::issues::store::upsert_issue(db.pool(), &plain).await?;
    assert_eq!(
        crate::runs::workpod::issue_dispatch_cluster(&db, &cfg, "owner/repo#1").await?,
        "hub",
        "no pin and no contract takes the configured default"
    );

    // A GPU-measured issue routes on its contract, which is what the chart's long-unread
    // CONTROLLER_DISPATCH_CLUSTER_BY_CONTRACT was always meant to do.
    sqlx::query("UPDATE issues SET codegen_contract = 'deepgemm' WHERE key = $1")
        .bind("owner/repo#1")
        .execute(db.pool())
        .await?;
    assert_eq!(
        crate::runs::workpod::issue_dispatch_cluster(&db, &cfg, "owner/repo#1").await?,
        "gpu-spoke"
    );

    // An explicit pin outranks the contract route: the launcher chose, and was authorized for it.
    crate::issues::store::set_dispatch_target(db.pool(), "owner/repo#1", Some("wharf")).await?;
    assert_eq!(
        crate::runs::workpod::issue_dispatch_cluster(&db, &cfg, "owner/repo#1").await?,
        "wharf"
    );

    // A contract with no route falls through rather than failing the dispatch.
    sqlx::query(
        "UPDATE issues SET codegen_contract = 'unrouted', dispatch_target = NULL WHERE key = $1",
    )
    .bind("owner/repo#1")
    .execute(db.pool())
    .await?;
    assert_eq!(
        crate::runs::workpod::issue_dispatch_cluster(&db, &cfg, "owner/repo#1").await?,
        "hub"
    );
    Ok(())
}

/// Two-phase, non-blocking: dispatch LAUNCHES a pod and returns without awaiting (nothing
/// booked, the row `running`); a later re-drive (the completion watch's edge, modelled by a
/// second dispatch call) peeks the now-terminal pod, adopts it, and books the verdict once.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_launches_then_a_redrive_collects_and_ledgers_a_verdict(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "ghcr.io/example/sandbox:latest");
    let created = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_VERDICT: {\"tier\":\"T2\",\"rationale\":\"one live service\",\"confidence\":\"low\",\"cost_usd\":0.25,\"over_budget\":false}\n".to_string(),
            created: created.clone(),
            deleted: deleted.clone(),
        });
    let today = crate::clock::today_utc();

    // Phase 1: non-blocking launch — a pod is created, the row goes `running`, nothing booked.
    let out = dispatch_grounded_rank(
        &db,
        &cfg,
        dispatcher.clone(),
        "owner/repo#42",
        "https://github.com/owner/repo.git",
        None,
    )
    .await?;
    assert!(
        matches!(out, DispatchOutcome::Launched),
        "dispatch launches, never awaits: {out:?}"
    );
    assert_eq!(created.lock().expect("lock").len(), 1, "one pod created");
    assert!(
        deleted.lock().expect("lock").is_empty(),
        "not collected yet"
    );
    assert_eq!(
        crate::runs::work_pods::count_active_work_pods(db.pool(), "grounded-rank").await?,
        1,
        "the turn is running"
    );
    assert!(
        crate::ledger::ledger_day_total(db.pool(), &today)
            .await?
            .abs()
            < 1e-9,
        "nothing booked at launch"
    );

    // Phase 2: the completion re-drive collects the now-terminal pod — verdict + one booking.
    let out = dispatch_grounded_rank(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#42",
        "https://github.com/owner/repo.git",
        None,
    )
    .await?;

    let DispatchOutcome::Verdict(v) = out else {
        panic!("expected the collected verdict, got {out:?}");
    };
    assert_eq!(v.disposition, Disposition::Tier(Tier::T2));
    assert_eq!(created.lock().expect("lock").len(), 1, "no second pod");
    assert_eq!(
        deleted.lock().expect("lock").len(),
        1,
        "the succeeded pod is GC'd on collection"
    );
    assert_eq!(
        crate::runs::work_pods::count_active_work_pods(db.pool(), "grounded-rank").await?,
        0
    );
    assert_eq!(
        crate::runs::work_pods::count_work_pod_turns_on_day(db.pool(), "grounded-rank", &today)
            .await?,
        1,
        "one turn dispatched today"
    );
    assert!((crate::ledger::ledger_day_total(db.pool(), &today).await? - 0.25).abs() < 1e-9);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_queues_when_over_the_daily_budget(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.grounded_rank_daily_turns = 0; // hard off → always queue

    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#7", "u", None).await?;

    assert!(
        matches!(out, DispatchOutcome::Queued),
        "over budget → queued, not dropped: {out:?}"
    );
    assert!(
        created.lock().expect("lock").is_empty(),
        "no pod created when queued"
    );
    // A queued row was persisted (backpressure — nothing silently skipped).
    let queued = crate::runs::work_pods::next_queued_work_pod(db.pool(), "grounded-rank")
        .await?
        .expect("a queued row");
    assert_eq!(queued.state, WorkPodState::Queued);
    assert_eq!(queued.issue_key.as_deref(), Some("owner/repo#7"));
    Ok(())
}

/// The queue's full lifecycle: repeated over-budget dispatches for one issue dedupe onto ONE
/// queued row, and the first admitted dispatch consumes that row — promoting it to running under
/// its reserved pod name — so nothing stays `queued` once its turn actually ran and no duplicate
/// rows accumulate.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn queued_turn_dedupes_and_drains_on_a_free_slot(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.grounded_rank_daily_turns = 0; // budget exhausted → queue

    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_VERDICT: {\"tier\":\"T0\",\"rationale\":\"failing test in tests/x.rs\",\"cost_usd\":0.2,\"over_budget\":false}\n".to_string(),
            created: created.clone(),
            deleted: Arc::new(Mutex::new(Vec::new())),
        });

    // Two over-budget dispatches → both Queued, ONE row (deduped), no pod created.
    for _ in 0..2 {
        let out = dispatch_grounded_rank(&db, &cfg, dispatcher.clone(), "owner/repo#5", "u", None)
            .await?;
        assert!(matches!(out, DispatchOutcome::Queued));
    }
    let queued =
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Queued]).await?;
    assert_eq!(queued.len(), 1, "repeated queuing dedupes to one row");
    let reserved_name = queued[0].pod_name.clone();
    assert!(created.lock().expect("lock").is_empty());

    // Budget frees → the next dispatch LAUNCHES, consuming the queued row under its reserved name
    // (promoted to `running`, non-blocking). A re-drive then collects it to a verdict.
    cfg.profile.grounded_rank_daily_turns = 50;
    let out =
        dispatch_grounded_rank(&db, &cfg, dispatcher.clone(), "owner/repo#5", "u", None).await?;
    assert!(
        matches!(out, DispatchOutcome::Launched),
        "the drain launches, not awaits: {out:?}"
    );
    assert!(
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Queued])
            .await?
            .is_empty(),
        "no row stays queued after its turn launched"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), &reserved_name)
            .await?
            .expect("row")
            .state,
        WorkPodState::Running,
        "the queued row was promoted in place, now running"
    );

    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#5", "u", None).await?;
    assert!(matches!(out, DispatchOutcome::Verdict(_)), "{out:?}");

    let row = crate::runs::work_pods::get_work_pod(db.pool(), &reserved_name)
        .await?
        .expect("the queued row was promoted, not replaced");
    assert_eq!(row.state, WorkPodState::Collected);
    assert_eq!(
        created.lock().expect("lock").as_slice(),
        &[reserved_name],
        "the pod ran under the queued row's reserved name"
    );
    // Exactly one work_pods row total: queue → spawn consumed the row in place.
    let all = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM work_pods"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(all.n, 1);
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_records_the_error_when_the_turn_produces_no_verdict(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Failed,
        message: None,
        logs: "podman exploded, no marker here\n".to_string(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    });

    // Phase 1: the pod launches fine — the failure is only observed at collection.
    let out =
        dispatch_grounded_rank(&db, &cfg, dispatcher.clone(), "owner/repo#9", "u", None).await?;
    assert!(
        matches!(out, DispatchOutcome::Launched),
        "launch succeeds regardless of the eventual turn outcome: {out:?}"
    );
    // Phase 2: the re-drive peeks the (Failed-phase) pod, collects it, finds no verdict.
    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#9", "u", None).await?;

    assert!(
        matches!(out, DispatchOutcome::Failed),
        "no verdict → Failed, caller keeps the text tier: {out:?}"
    );
    // A failed pod is RETAINED for debugging (not deleted here) with the real error on the row.
    assert!(
        deleted.lock().expect("lock").is_empty(),
        "failed pod retained for kubectl logs"
    );
    let name = grounded_rank_pod_name("owner/repo#9");
    // The row exists at `failed`; find it via the states query (name has a random suffix).
    let failed =
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Failed]).await?;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].state, WorkPodState::Failed);
    assert!(
        failed[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no verdict")
    );
    let _ = name;
    Ok(())
}

// --- level-triggered adoption + race-safe collection ----------------------------------------

/// Seed a `running` turn row — the fixture for a turn whose in-band collector died on a restart,
/// or that a level-triggered re-drive races.
async fn seed_running_turn(db: &Db, kind: WorkKind, issue_key: &str, pod_name: &str) -> Result<()> {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: pod_name.to_string(),
            kind: kind.label_value().to_string(),
            issue_key: Some(issue_key.to_string()),
            state: WorkPodState::Running,
            cost_tag: kind.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await
}

/// Shared assertion tail for "adopt a running turn, never double-launch": adoption never renders
/// a second pod, exactly one row exists (promoted in place, not inserted twice), it lands
/// `collected`, the succeeded pod is GC'd, and its cost books exactly once.
#[cfg(feature = "autoresearch")]
async fn assert_adopted_not_relaunched(
    db: &Db,
    pod_name: &str,
    created: &Mutex<Vec<String>>,
    deleted: &Mutex<Vec<String>>,
    expected_cost: f64,
) -> Result<()> {
    assert!(
        created.lock().expect("lock").is_empty(),
        "adoption re-enters the existing pod, never creates a second"
    );
    let all = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM work_pods"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(
        all.n, 1,
        "adoption reuses the running row, inserts no second"
    );
    let row = crate::runs::work_pods::get_work_pod(db.pool(), pod_name)
        .await?
        .expect("row");
    assert_eq!(row.state, WorkPodState::Collected);
    assert_eq!(deleted.lock().expect("lock").as_slice(), &[pod_name]);
    let today = crate::clock::today_utc();
    assert!(
        (crate::ledger::ledger_day_total(db.pool(), &today).await? - expected_cost).abs() < 1e-9,
        "the adopted turn's cost books exactly once"
    );
    Ok(())
}

/// Slice 1: a grounded dispatch that finds a `running` turn for the issue ADOPTS it — collects
/// the existing pod on the shared tail — instead of launching a second (and double-spending).
/// Adoption must never render a second pod.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn grounded_dispatch_adopts_a_running_turn_never_double_launches(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);
    seed_running_turn(&db, kind, "owner/repo#42", "crucible-turn-adopt-me").await?;

    let created = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"x\",\"cost_usd\":0.3,\"over_budget\":false}\n".to_string(),
            created: created.clone(),
            deleted: deleted.clone(),
        });

    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#42", "u", None).await?;
    let DispatchOutcome::Verdict(v) = out else {
        panic!("expected the adopted verdict, got {out:?}");
    };
    assert_eq!(v.disposition, Disposition::Tier(Tier::T1));
    assert_adopted_not_relaunched(&db, "crucible-turn-adopt-me", &created, &deleted, 0.3).await
}

/// Slice 1 for scope: the costliest kind ($5-11) must adopt a running scope turn, not relaunch.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_dispatch_adopts_a_running_turn_never_double_launches(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let kind = WorkKind::AgentTurn(TurnKind::Scope);
    seed_running_turn(&db, kind, "owner/repo#7", "crucible-scope-adopt-me").await?;

    let created = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"freeze\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.1}\n".to_string(),
            created: created.clone(),
            deleted: deleted.clone(),
        });

    let out = dispatch_scope(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner/repo",
        8.0,
        TurnInputs {
            tier: Some(Tier::T1),
            ..TurnInputs::default()
        },
    )
    .await?;
    let ScopeOutcome::Report { pod_name, .. } = out else {
        panic!("expected the adopted report, got {out:?}");
    };
    assert_eq!(pod_name, "crucible-scope-adopt-me");
    assert_adopted_not_relaunched(&db, "crucible-scope-adopt-me", &created, &deleted, 0.1).await
}

/// Slice 2, the collection race: the CAS in [`Db::try_finish_running_work_pod`] admits exactly
/// one winner out of `running`, so the in-band collector and a pod-watch re-drive can both reach
/// the terminal pod and the cost still books once — kind-agnostic, parameterized over both turn
/// kinds.
#[cfg(feature = "autoresearch")]
async fn conformance_try_finish_running_work_pod_is_an_exclusive_cas(
    db: &Db,
    kind: WorkKind,
) -> Result<()> {
    seed_running_turn(db, kind, "owner/repo#5", "crucible-turn-cas").await?;
    let first = crate::runs::work_pods::try_finish_running_work_pod(
        db.pool(),
        "crucible-turn-cas",
        WorkPodState::Collected,
        None,
        None,
    )
    .await?;
    let second = crate::runs::work_pods::try_finish_running_work_pod(
        db.pool(),
        "crucible-turn-cas",
        WorkPodState::Collected,
        None,
        None,
    )
    .await?;
    assert!(first, "the first collector wins the running→collected CAS");
    assert!(
        !second,
        "the second finds the row already collected and loses"
    );
    // A `failed` CAS on the same (now collected) row also loses — only a `running` row transitions.
    assert!(
        !crate::runs::work_pods::try_finish_running_work_pod(
            db.pool(),
            "crucible-turn-cas",
            WorkPodState::Failed,
            None,
            Some("boom")
        )
        .await?
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn try_finish_running_work_pod_is_an_exclusive_cas_grounded(
    pool: sqlx::PgPool,
) -> Result<()> {
    let db = Db::new(pool);
    conformance_try_finish_running_work_pod_is_an_exclusive_cas(
        &db,
        WorkKind::AgentTurn(TurnKind::GroundedRank),
    )
    .await
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn try_finish_running_work_pod_is_an_exclusive_cas_scope(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    conformance_try_finish_running_work_pod_is_an_exclusive_cas(
        &db,
        WorkKind::AgentTurn(TurnKind::Scope),
    )
    .await
}

/// A dispatcher that injects the collection race: right as the dispatch-under-test reads the
/// verdict logs, a concurrent collector wins the row's terminal CAS. The dispatch must then LOSE
/// its own CAS and book nothing — the single-booking guarantee under a real race.
#[cfg(feature = "autoresearch")]
struct RacingDispatcher {
    pool: sqlx::PgPool,
    pod_name: String,
}

#[cfg(feature = "autoresearch")]
#[async_trait::async_trait]
impl PodDispatcher for RacingDispatcher {
    async fn create(&self, _cluster: &str, _ns: &str, pod: Pod) -> Result<Pod> {
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        // The other collector gets there first (wins the running→collected CAS).
        sqlx::query(
            "UPDATE work_pods SET state = 'collected' WHERE pod_name = $1 AND state = 'running'",
        )
        .bind(&self.pod_name)
        .execute(&self.pool)
        .await?;
        Ok("CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"x\",\"cost_usd\":0.9,\"over_budget\":false}\n".to_string())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// Kind-agnostic: races a concurrent collector against a fresh dispatch's own CAS via the shared
/// [`crate::runs::workpod::spec::dispatch_turn`] machine, for both turn kinds.
#[cfg(feature = "autoresearch")]
async fn conformance_adopting_collector_that_loses_the_cas_books_nothing<
    S: crate::runs::workpod::spec::TurnSpec,
>(
    spec: &S,
    issue_key: &str,
    repo_url: &str,
    max_cost: f64,
) -> Result<()> {
    #[cfg(feature = "autoresearch")]
    use crate::runs::workpod::spec::{TurnCollected, TurnDispatch, dispatch_turn};

    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(crate::client::connect(&crate::test_ledger_url()).await?);
    let cfg = pod_cfg(&profile, "img");
    seed_running_turn(&db, spec.kind(), issue_key, "crucible-turn-race").await?;
    let dispatcher = Arc::new(RacingDispatcher {
        pool: db.pool().clone(),
        pod_name: "crucible-turn-race".to_string(),
    });

    let out = dispatch_turn(
        spec,
        &db,
        &cfg,
        dispatcher,
        issue_key,
        repo_url,
        max_cost,
        TurnInputs::default(),
    )
    .await?;
    assert!(
        matches!(
            out,
            TurnDispatch::Collected(TurnCollected::AlreadyCollected)
        ),
        "the CAS loser yields to the winner"
    );
    let today = crate::clock::today_utc();
    assert!(
        (crate::ledger::ledger_day_total(db.pool(), &today).await? - 0.0).abs() < 1e-9,
        "the loser books no cost (the winner already did)"
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[tokio::test]
async fn adopting_collector_that_loses_the_cas_books_nothing_grounded() -> Result<()> {
    conformance_adopting_collector_that_loses_the_cas_books_nothing(
        &crate::runs::workpod::spec::GroundedRankSpec,
        "owner/repo#5",
        "u",
        0.0,
    )
    .await
}

#[cfg(feature = "autoresearch")]
#[tokio::test]
async fn adopting_collector_that_loses_the_cas_books_nothing_scope() -> Result<()> {
    conformance_adopting_collector_that_loses_the_cas_books_nothing(
        &crate::runs::workpod::spec::ScopeSpec,
        "owner/repo#5",
        "owner/repo",
        8.0,
    )
    .await
}

/// Slice 2 startup coverage, kind-agnostic: a `running` turn is LEFT for adoption at startup,
/// never mis-parsed. The old pass scraped `CRUCIBLE_VERDICT:` (which a scope turn never prints)
/// and wrongly marked it failed, dropping the pack; the new pass delegates to the one collection
/// tail regardless of kind.
async fn conformance_startup_leaves_a_running_turn_for_adoption(
    db: &Db,
    kind: WorkKind,
    pod_name: &str,
) -> Result<()> {
    seed_running_turn(db, kind, "owner/repo#9", pod_name).await?;

    let deleted = Arc::new(Mutex::new(Vec::new()));
    // A dispatcher that reports the pod terminal + logs empty — the OLD pass would have peeked,
    // failed to parse a verdict, and marked the row failed. The new pass must not touch it.
    let dispatcher = FakeDispatcher {
        phase: TurnPhase::Failed,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    };
    reconcile_on_startup(db, &dispatcher, "autoresearch", 20).await;

    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), pod_name)
            .await?
            .unwrap()
            .state,
        WorkPodState::Running,
        "startup leaves a running turn for the adoption path, never mis-marks it"
    );
    assert!(
        deleted.lock().expect("lock").is_empty(),
        "a running turn's pod is never swept at startup"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn startup_leaves_a_running_grounded_turn_for_adoption(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    conformance_startup_leaves_a_running_turn_for_adoption(
        &db,
        WorkKind::AgentTurn(TurnKind::GroundedRank),
        "crucible-turn-live",
    )
    .await
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn startup_leaves_a_running_scope_turn_for_adoption(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    conformance_startup_leaves_a_running_turn_for_adoption(
        &db,
        WorkKind::AgentTurn(TurnKind::Scope),
        "crucible-scope-live",
    )
    .await
}

// --- WorkKind::Run --------------------------------------------------------------------------

#[test]
fn run_pod_name_is_dns_safe_and_bounded() {
    let n = run_pod_name("owner_repo_7-1730000000");
    assert!(n.starts_with("crucible-run-owner-repo-7-"));
    assert!(n.len() <= 63, "DNS-1123 label ≤63: {n} ({})", n.len());
    assert!(
        n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "DNS-1123 body only: {n}"
    );
    assert!(!n.starts_with('-') && !n.ends_with('-'));
    // A pathological run id can't blow the 63 budget.
    let long = run_pod_name(&format!("{}-1", "a".repeat(120)));
    assert!(long.len() <= 63, "{long} ({})", long.len());
}

#[test]
fn stamp_run_pod_sets_name_workkind_runid_and_managed_meta() {
    let mut pod = Pod::default();
    let owner = OwnerReference {
        api_version: "apps/v1".to_string(),
        kind: "Deployment".to_string(),
        name: "crucible-controller".to_string(),
        uid: "uid-9".to_string(),
        controller: Some(true),
        block_owner_deletion: None,
    };
    stamp_run_pod(
        &mut pod,
        "crucible-run-owner-repo-7-42",
        "owner/repo#7",
        "owner_repo_7-42",
        Some(owner),
        None,
    );

    assert_eq!(
        pod.metadata.name.as_deref(),
        Some("crucible-run-owner-repo-7-42"),
        "the controller owns the pod name"
    );
    let ann = pod.metadata.annotations.as_ref().unwrap();
    assert_eq!(
        ann.get(crate::daemon::ISSUE_KEY_ANNOTATION)
            .map(String::as_str),
        Some("owner/repo#7"),
        "the exact issue key round-trips"
    );
    assert_eq!(
        ann.get(crate::daemon::RUN_ID_ANNOTATION)
            .map(String::as_str),
        Some("owner_repo_7-42"),
        "the run id round-trips so the completion edge maps back"
    );
    let labels = pod.metadata.labels.as_ref().unwrap();
    assert_eq!(
        labels.get(WORK_KIND_LABEL).map(String::as_str),
        Some("run"),
        "the work-kind label a sweep reconciles on"
    );
    assert_eq!(
        labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str),
        Some("crucible"),
        "the pod-watch selector"
    );
    assert_eq!(
        pod.metadata.owner_references.as_ref().unwrap()[0].uid,
        "uid-9"
    );
    assert!(
        pod.spec.is_none(),
        "a local-measure run gets no overlay env, so nothing forces a spec into existence"
    );
}

/// The run-dispatch half of a codegen contract: the resolved contract JSON lands on every MAIN
/// container as `BROKER_CODEGEN_TOOLS_OVERLAY` (the broker merges it over the profile-wide
/// `BROKER_CODEGEM_TOOLS_DEFAULTS`), init containers are left alone (they stage, they never dial the
/// broker), and a re-stamp replaces rather than appends.
#[test]
fn stamp_run_pod_projects_the_codegen_contract_onto_the_loop_containers() {
    let contract = r#"{"gpus":1,"build":{"src_dir":"/opt/deepgemm"}}"#;
    let mut pod = Pod {
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![
                Container {
                    name: "loop".to_string(),
                    env: Some(vec![EnvVar {
                        name: "BROKER_CODEGEN_TOOLS_OVERLAY".to_string(),
                        value: Some("{\"stale\":true}".to_string()),
                        value_from: None,
                    }]),
                    ..Default::default()
                },
                Container {
                    name: "sidecar".to_string(),
                    ..Default::default()
                },
            ],
            init_containers: Some(vec![Container {
                name: "pack".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    stamp_run_pod(
        &mut pod,
        "crucible-run-x",
        "scenario:abc",
        "scenario_abc-1",
        None,
        Some(contract),
    );

    let overlay = |c: &Container| -> Option<String> {
        c.env
            .as_ref()?
            .iter()
            .find(|v| v.name == "BROKER_CODEGEN_TOOLS_OVERLAY")?
            .value
            .clone()
    };
    let spec = pod.spec.clone().expect("spec");
    assert_eq!(
        overlay(&spec.containers[0]).as_deref(),
        Some(contract),
        "the stale copy is replaced, not appended to"
    );
    assert_eq!(
        spec.containers[0]
            .env
            .as_ref()
            .expect("env")
            .iter()
            .filter(|v| v.name == "BROKER_CODEGEN_TOOLS_OVERLAY")
            .count(),
        1,
        "exactly one copy"
    );
    assert_eq!(overlay(&spec.containers[1]).as_deref(), Some(contract));
    assert!(
        overlay(&spec.init_containers.expect("inits")[0]).is_none(),
        "init containers stage the pack; they never talk to the broker"
    );

    // The complement: a local-measure run leaves the env off entirely, rather than setting it empty.
    let mut plain = Pod {
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![Container {
                name: "loop".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    stamp_run_pod(
        &mut plain,
        "crucible-run-y",
        "owner/repo#7",
        "r-1",
        None,
        None,
    );
    assert!(
        overlay(&plain.spec.expect("spec").containers[0]).is_none(),
        "no contract, no env var"
    );
}

/// The App's identity is the default on every container that did not name one.
#[test]
fn the_git_identity_is_a_default_that_an_explicit_one_beats() {
    let mut pod = Pod {
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![
                Container {
                    name: "loop".to_string(),
                    env: Some(vec![k8s_openapi::api::core::v1::EnvVar {
                        name: "GIT_AUTHOR_NAME".to_string(),
                        value: Some("Will Eaton".to_string()),
                        value_from: None,
                    }]),
                    ..Default::default()
                },
                Container {
                    name: "sidecar".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let who = crate::secrets::github_app::BotIdentity {
        name: "crucible-bot[bot]".to_string(),
        email: "299632118+crucible-bot[bot]@users.noreply.github.com".to_string(),
    };
    crate::runs::workpod::run::stamp_git_identity(&mut pod, &who);

    let containers = pod.spec.expect("spec").containers;
    let env_of = |name: &str| {
        containers
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.env.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|v| (v.name, v.value.unwrap_or_default()))
            .collect::<Vec<_>>()
    };

    // The container that named an author gets no part of the default.
    assert_eq!(
        env_of("loop"),
        vec![("GIT_AUTHOR_NAME".to_string(), "Will Eaton".to_string())],
        "a container that brought its own identity is left alone entirely"
    );

    // Its sibling named nothing, so it gets all four.
    assert_eq!(
        env_of("sidecar"),
        vec![
            ("GIT_AUTHOR_NAME".to_string(), who.name.clone()),
            ("GIT_AUTHOR_EMAIL".to_string(), who.email.clone()),
            ("GIT_COMMITTER_NAME".to_string(), who.name.clone()),
            ("GIT_COMMITTER_EMAIL".to_string(), who.email.clone()),
        ]
    );
}

#[cfg(feature = "autoresearch")]
#[test]
fn apply_build_digests_pins_matching_container_images() {
    // A rendered pod whose main + init containers reference a built image's repo (by tag) get
    // rewritten to the pinned digest; an unrelated image is untouched.
    let mut pod = Pod {
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![
                Container {
                    name: "loop".to_string(),
                    image: Some("ghcr.io/org/vllm-sandbox:m1".to_string()),
                    ..Default::default()
                },
                Container {
                    name: "sidecar".to_string(),
                    image: Some("ghcr.io/org/unrelated:latest".to_string()),
                    ..Default::default()
                },
            ],
            init_containers: Some(vec![Container {
                name: "pack".to_string(),
                image: Some("ghcr.io/org/vllm-sandbox@sha256:stale".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let digests = BTreeMap::from([(
        "ghcr.io/org/vllm-sandbox".to_string(),
        "ghcr.io/org/vllm-sandbox@sha256:fresh".to_string(),
    )]);
    apply_build_digests(&mut pod, &digests);
    let spec = pod.spec.unwrap();
    assert_eq!(
        spec.containers[0].image.as_deref(),
        Some("ghcr.io/org/vllm-sandbox@sha256:fresh"),
        "the tagged image is pinned to the built digest"
    );
    assert_eq!(
        spec.containers[1].image.as_deref(),
        Some("ghcr.io/org/unrelated:latest"),
        "an unmatched image is left alone"
    );
    assert_eq!(
        spec.init_containers.unwrap()[0].image.as_deref(),
        Some("ghcr.io/org/vllm-sandbox@sha256:fresh"),
        "a stale-digest init image is re-pinned too"
    );
}

#[cfg(feature = "autoresearch")]
#[test]
fn apply_build_digests_is_a_noop_on_an_empty_map() {
    let mut pod = Pod {
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![Container {
                name: "loop".to_string(),
                image: Some("ghcr.io/org/x:v1".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_build_digests(&mut pod, &BTreeMap::new());
    assert_eq!(
        pod.spec.unwrap().containers[0].image.as_deref(),
        Some("ghcr.io/org/x:v1"),
        "no builds ⇒ the static manifest image stands"
    );
}

/// A dispatcher for the run path that records both created pod names and created ConfigMaps (name +
/// owner refs), and stamps a UID onto the created pod like the API server would — so the test can
/// assert the CM is owner-ref'd to the pod for cascade GC.
struct RunBundleDispatcher {
    created_pods: Arc<Mutex<Vec<String>>>,
    created_cms: Arc<Mutex<Vec<ConfigMap>>>,
    deleted: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PodDispatcher for RunBundleDispatcher {
    async fn create(&self, _cluster: &str, _ns: &str, mut pod: Pod) -> Result<Pod> {
        self.created_pods
            .lock()
            .expect("lock")
            .push(pod.metadata.name.clone().unwrap_or_default());
        pod.metadata.uid = Some("pod-uid-123".to_string());
        Ok(pod)
    }
    async fn create_configmap(&self, _cluster: &str, _ns: &str, cm: ConfigMap) -> Result<()> {
        self.created_cms.lock().expect("lock").push(cm);
        Ok(())
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _n: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _n: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, name: &str) -> Result<()> {
        self.deleted.lock().expect("lock").push(name.to_string());
        Ok(())
    }
}

async fn contract_events(db: &Db, key: &str) -> Vec<crate::event_log::EventRecord> {
    db.events()
        .read_for_key(key)
        .await
        .expect("events read")
        .into_iter()
        .filter(|e| {
            e.evidence
                .as_deref()
                .is_some_and(|r| r.contains("\"kind\":\"contract rejection\""))
        })
        .collect()
}

#[cfg(feature = "autoresearch")]
async fn parked_event(db: &Db, key: &str) -> Option<crate::event_log::EventRecord> {
    db.events()
        .read_for_key(key)
        .await
        .expect("events read")
        .into_iter()
        .find(|e| e.to == "parked")
}

/// A turn whose loop image carries another contract version is refused before any row, pod, or
/// spend: the ledger gets a named contract-rejection event with both versions, the issue parks so
/// nothing retries it, and the image is read once (the answer is cached).
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_mismatched_loop_image_refuses_the_turn_and_parks_the_issue(
    pool: sqlx::PgPool,
) -> Result<()> {
    #[cfg(feature = "autoresearch")]
    use crate::runs::contract::{CONTROLLER_CONTRACT_VERSION, ContractRegistry, TableReader};
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let loop_image = crate::runs::contract::loop_image(&profile)?;
    let reader = Arc::new(TableReader::new([(
        loop_image.clone(),
        Ok(Some("0.0.1".to_string())),
    )]));
    crate::runs::workpod::install_contracts(Arc::new(ContractRegistry::new(reader.clone())));

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "quay.io/contract-test/sandbox:old");
    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    crate::issues::store::upsert_issue(
        db.pool(),
        &crate::issues::model::NewIssue {
            key: "owner/repo#77".to_string(),
            repo: "owner/repo".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        },
    )
    .await?;

    let out = dispatch_grounded_rank(
        &db,
        &cfg,
        dispatcher.clone(),
        "owner/repo#77",
        "https://github.com/owner/repo.git",
        None,
    )
    .await;
    crate::runs::workpod::reset_contracts();
    let out = out?;
    assert!(matches!(out, DispatchOutcome::Failed), "refused: {out:?}");
    assert!(created.lock().expect("lock").is_empty(), "no pod created");
    assert_eq!(
        crate::runs::work_pods::count_active_work_pods(db.pool(), "grounded-rank").await?,
        0,
        "no work-pod row written"
    );
    assert!(
        crate::runs::work_pods::find_queued_work_pod(db.pool(), "grounded-rank", "owner/repo#77")
            .await?
            .is_none(),
        "nothing queued either"
    );

    let events = contract_events(&db, "owner/repo#77").await;
    assert_eq!(events.len(), 1, "one contract-rejection event: {events:?}");
    assert_eq!(
        events[0].reason.as_deref(),
        Some(
            format!(
                "contract rejection: grounded-rank against {loop_image}: engine contract 0.0.1, \
                 controller contract {CONTROLLER_CONTRACT_VERSION}"
            )
            .as_str()
        )
    );
    let evidence: serde_json::Value =
        serde_json::from_str(events[0].evidence.as_deref().expect("evidence"))?;
    assert_eq!(evidence["kind"], "contract rejection");
    assert_eq!(evidence["request"], "grounded-rank");
    assert_eq!(evidence["image"], loop_image);
    assert_eq!(evidence["engine_version"], "0.0.1");
    assert_eq!(evidence["controller_version"], CONTROLLER_CONTRACT_VERSION);

    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#77")
        .await?
        .expect("issue");
    assert_eq!(issue.status, crate::model::Status::Parked);
    assert_eq!(
        crate::model::ParkReason::parse(issue.parked_reason.as_deref().unwrap_or_default()),
        crate::model::ParkReason::ContractRejected {
            image: loop_image.clone(),
            engine_version: "0.0.1".to_string(),
            controller_version: CONTROLLER_CONTRACT_VERSION.to_string(),
        }
    );
    let parked = parked_event(&db, "owner/repo#77")
        .await
        .expect("park event");
    assert_eq!(parked.from, "new");
    assert!(
        parked
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("contract rejection: ")),
        "{parked:?}"
    );
    assert_eq!(
        reader.reads(),
        1,
        "the loop image is read once and its mismatch ends the check"
    );
    Ok(())
}

/// A registry read failure is a transport failure, not a verdict: the dispatch errors so the next
/// reconcile pass retries it, and nothing is ledgered as a rejection or parked.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_unreadable_loop_image_fails_the_turn_without_parking(pool: sqlx::PgPool) -> Result<()> {
    use crate::runs::contract::{ContractReadError, ContractRegistry, TableReader};
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let loop_image = crate::runs::contract::loop_image(&profile)?;
    let reader = Arc::new(TableReader::new([(
        loop_image,
        Err(ContractReadError("503 registry unavailable".to_string())),
    )]));
    crate::runs::workpod::install_contracts(Arc::new(ContractRegistry::new(reader)));

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "quay.io/contract-test/sandbox:1");
    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    crate::issues::store::upsert_issue(
        db.pool(),
        &crate::issues::model::NewIssue {
            key: "owner/repo#78".to_string(),
            repo: "owner/repo".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        },
    )
    .await?;

    let out = dispatch_grounded_rank(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#78",
        "https://github.com/owner/repo.git",
        None,
    )
    .await;
    crate::runs::workpod::reset_contracts();
    let err = out.expect_err("an unreadable target fails the dispatch");
    assert!(
        format!("{err:#}").contains("503 registry unavailable"),
        "{err:#}"
    );
    assert!(created.lock().expect("lock").is_empty(), "no pod created");
    assert!(
        contract_events(&db, "owner/repo#78").await.is_empty(),
        "a transport failure is not ledgered as a contract rejection"
    );
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#78")
        .await?
        .expect("issue");
    assert_ne!(
        issue.status,
        crate::model::Status::Parked,
        "a registry blip must not park the issue"
    );
    Ok(())
}

/// A pack's sandbox image is an agent container, not an engine: it carries no contract label and
/// must not gate the run. The loop image is the only image the run is admitted on.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pack_sandbox_image_without_the_label_still_launches_the_run(
    pool: sqlx::PgPool,
) -> Result<()> {
    use crate::runs::contract::{ContractRegistry, TableReader};
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let pack = crate::testing::fixtures::write_loop_pack(tmp.path());
    let pack_sandbox = crate::playbooks::dispatch::pack_agent(&pack)?
        .sandbox_image
        .expect("the fixture pack names a sandbox image");
    let reader = Arc::new(TableReader::new([(pack_sandbox.clone(), Ok(None))]));
    crate::runs::workpod::install_contracts(Arc::new(ContractRegistry::new(reader)));

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created_pods = Arc::new(Mutex::new(Vec::new()));
    let created_cms = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(RunBundleDispatcher {
        created_pods: created_pods.clone(),
        created_cms: created_cms.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    seed_running_issue(&db, "owner/repo#8").await?;

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#8",
        "owner_repo_8-1",
        &pack,
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await;
    crate::runs::workpod::reset_contracts();
    let out = out?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "{out:?}");
    assert_eq!(created_pods.lock().expect("lock").len(), 1);
    assert!(
        contract_events(&db, "owner/repo#8").await.is_empty(),
        "no rejection ledgered"
    );
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#8")
        .await?
        .expect("issue");
    assert_ne!(issue.status, crate::model::Status::Parked);
    Ok(())
}

/// A pack dispatch creates BOTH docs and stamps them: the pod under the controller-owned name, then
/// the ConfigMap under the run-unique `<pod>-pack` name, carrying the managed-by selector + the
/// issue-key annotation + an ownerReference to the POD (so k8s cascade-GCs it with the pod, no
/// bespoke delete site). The CM name the pod's volume references matches the created CM's name.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_creates_and_stamps_both_docs_and_owner_refs_the_cm(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created_pods = Arc::new(Mutex::new(Vec::new()));
    let created_cms = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(RunBundleDispatcher {
        created_pods: created_pods.clone(),
        created_cms: created_cms.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;

    let RunAdmission::Launched { pod_name, .. } = out else {
        panic!("expected a launch, got {out:?}");
    };
    // The pod was created under the controller-owned name.
    assert_eq!(
        created_pods.lock().expect("lock").as_slice(),
        std::slice::from_ref(&pod_name)
    );

    // Exactly one CM created, under the run-unique `<pod>-pack` name (the pod's volume ref).
    let cms = created_cms.lock().expect("lock");
    assert_eq!(cms.len(), 1, "one pack ConfigMap created");
    let cm = &cms[0];
    assert_eq!(
        cm.metadata.name.as_deref(),
        Some(format!("{pod_name}-pack").as_str())
    );
    // Managed-by selector + issue-key annotation stamped, exactly like the pod.
    let labels = cm.metadata.labels.as_ref().expect("cm labels");
    assert_eq!(
        labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str),
        Some("crucible")
    );
    let anns = cm.metadata.annotations.as_ref().expect("cm annotations");
    assert_eq!(
        anns.get(crate::daemon::ISSUE_KEY_ANNOTATION)
            .map(String::as_str),
        Some("owner/repo#7")
    );
    // Owner-ref'd to the POD (not the controller Deployment): cascade GC glues the CM to the pod.
    let owners = cm
        .metadata
        .owner_references
        .as_ref()
        .expect("cm owner refs");
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].kind, "Pod");
    assert_eq!(owners[0].name, pod_name);
    assert_eq!(
        owners[0].uid, "pod-uid-123",
        "points at the created pod's UID"
    );
    assert_eq!(owners[0].controller, Some(true));
    Ok(())
}

async fn seed_running_issue(db: &Db, key: &str) -> Result<()> {
    crate::issues::store::upsert_issue(
        db.pool(),
        &crate::issues::model::NewIssue {
            key: key.to_string(),
            repo: "owner/repo".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        },
    )
    .await?;
    crate::issues::store::claim_issue(
        db.pool(),
        key,
        crate::model::Status::New,
        crate::model::Status::Running,
    )
    .await?;
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_launches_tracks_and_returns_the_stamped_pod(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;

    let RunAdmission::Launched { pod_name, .. } = out else {
        panic!("expected a launch, got {out:?}");
    };
    assert_eq!(pod_name, run_pod_name("owner_repo_7-42"));
    // The pod was created under the controller-owned name (render's `rendered-loop` overridden).
    assert_eq!(
        created.lock().expect("lock").as_slice(),
        std::slice::from_ref(&pod_name)
    );
    // A `run` work_pods row is tracked at `running` — but NO ledger row (runs book in ingest).
    let running =
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Running]).await?;
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].kind, "run");
    assert_eq!(running[0].issue_key.as_deref(), Some("owner/repo#7"));
    assert_eq!(running[0].pod_name, pod_name);
    let today = crate::clock::today_utc();
    assert!(
        (crate::ledger::ledger_day_total(db.pool(), &today).await? - 0.0).abs() < 1e-9,
        "a run dispatch books no cost here; the session ingest does, once"
    );
    Ok(())
}

/// A dispatcher that keeps every created pod whole, so a test can read the wrapper script the
/// render put on the loop container.
struct WrapperRecorder {
    pods: Arc<Mutex<Vec<Pod>>>,
}

impl WrapperRecorder {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<Pod>>>) {
        let pods = Arc::new(Mutex::new(Vec::new()));
        (Arc::new(WrapperRecorder { pods: pods.clone() }), pods)
    }
}

#[async_trait::async_trait]
impl PodDispatcher for WrapperRecorder {
    async fn create(&self, _cluster: &str, _ns: &str, pod: Pod) -> Result<Pod> {
        self.pods.lock().expect("lock").push(pod.clone());
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _n: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _n: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// The wrapper script of the one pod `recorder` created.
fn only_wrapper(pods: &Arc<Mutex<Vec<Pod>>>) -> String {
    let pods = pods.lock().expect("lock");
    assert_eq!(pods.len(), 1, "one pod created");
    crate::testing::fixtures::wrapper_of(&pods[0])
}

/// The value of `name` on each main container of the one pod `recorder` created.
fn only_pod_env(pods: &Arc<Mutex<Vec<Pod>>>, name: &str) -> Vec<Option<String>> {
    let pods = pods.lock().expect("lock");
    assert_eq!(pods.len(), 1, "one pod created");
    pods[0]
        .spec
        .as_ref()
        .expect("the rendered pod carries a spec")
        .containers
        .iter()
        .map(|c| {
            c.env
                .iter()
                .flatten()
                .find(|v| v.name == name)
                .and_then(|v| v.value.clone())
        })
        .collect()
}

/// The engine resolves its `tracker-comment` default target from `$CRUCIBLE_ITEM`. An issue-driven
/// run exports the item it is parameterized by; a playbook launch has no upstream item, exports
/// none, and the engine then refuses every tracker write.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_loop_run_exports_its_item_and_a_playbook_launch_exports_none(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let (dispatcher, pods) = WrapperRecorder::new();
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(&tmp.path().join("loop")),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;
    assert!(
        matches!(out, RunAdmission::Launched { .. }),
        "expected a launch, got {out:?}"
    );
    let item = only_pod_env(&pods, crate::runs::engine::ITEM_ENV);
    assert!(!item.is_empty());
    assert!(
        item.iter().all(|v| v.as_deref() == Some("owner/repo#7")),
        "every main container carries the run's item: {item:?}"
    );

    let (dispatcher, pods) = WrapperRecorder::new();
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "playbook:survey:0199c0de-7c2c-71a5-8000-1",
        "playbook_survey_0199c0de-42",
        &crate::testing::fixtures::write_playbook_pack(
            &tmp.path().join("playbook"),
            crate::testing::fixtures::WORKFLOW_TOPIC_DEPTH,
        ),
        &BTreeMap::new(),
        None,
        RunRenderOpts::Playbook {
            params: vec![
                ("depth".to_string(), "--deep".to_string()),
                ("topic".to_string(), "attention sinks".to_string()),
            ],
            max_cost: 3.5,
            max_time: crate::model::MaxTime::parse("30m").expect("duration"),
            agent: AgentSelection::default(),
        },
        None,
    )
    .await?;
    assert!(
        matches!(out, RunAdmission::Launched { .. }),
        "expected a launch, got {out:?}"
    );
    assert!(
        only_pod_env(&pods, crate::runs::engine::ITEM_ENV)
            .iter()
            .all(Option::is_none),
        "a launch with no upstream item addresses no tracker item"
    );
    Ok(())
}

/// The dispatch forwards the controller's run-iteration + budget knobs to the render, so the
/// rendered loop pod runs a real multi-turn budget instead of the stale `iters_total=1`,
/// `max_cost=0.0` that cut the first dispatched run off after one empty turn.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_forwards_iteration_and_budget_knobs(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    // pod_cfg leaves the run knobs at their defaults (6 iterations, $25 budget).
    let cfg = pod_cfg(&profile, "img");
    let (dispatcher, pods) = WrapperRecorder::new();

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;
    assert!(
        matches!(out, RunAdmission::Launched { .. }),
        "expected a launch, got {out:?}"
    );

    let wrapper = only_wrapper(&pods);
    assert!(
        wrapper.contains("--iterations=6"),
        "the run gets the controller's iteration budget, not the stale default of 1: {wrapper}"
    );
    assert!(
        wrapper.contains("--max-cost=25"),
        "the run gets a real cost budget, not the stale 0 that cut the turn off: {wrapper}"
    );
    Ok(())
}

/// The dispatch resolves the issue's stored repo against the controller's
/// `pr_repo_map` and forwards the mapped fork as `--pr-repo`, so a kept candidate opens its draft PR
/// against that fork. An unmapped repo forwards no flag (the loop then opens no PR unless its pack
/// carries a `[publish] pr_repo`).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_forwards_the_mapped_pr_repo(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.pr_repo_map = vec!["owner/repo=wren/repo-fork".to_string()];
    let (dispatcher, pods) = WrapperRecorder::new();

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");

    let wrapper = only_wrapper(&pods);
    assert!(
        wrapper.contains("--pr-repo=wren/repo-fork"),
        "the mapped fork is forwarded as --pr-repo: {wrapper}"
    );
    Ok(())
}

/// Adopted scenarios have opaque keys, so publication must resolve from the stored repo rather
/// than trying to parse `owner/repo` out of the key.
#[test]
fn scenario_run_resolves_the_mapped_pr_repo_from_its_repo() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let mut cfg = pod_cfg(&profile, "img");
    cfg.pr_repo_map = vec!["neuralmagic/crucible=wren/crucible".to_string()];

    assert_eq!(
        RunRenderOpts::for_loop(&cfg, "neuralmagic/crucible", AgentSelection::default()),
        RunRenderOpts::Loop {
            iterations: cfg.effective().run_iterations,
            max_cost: cfg.effective().run_max_cost,
            pr_repo: Some("wren/crucible".to_string()),
            agent: AgentSelection::default(),
        }
    );
    Ok(())
}

/// A repo with no `pr_repo_map` entry forwards NO `--pr-repo`, so the render leaves publishing to the
/// pack's own `[publish] pr_repo` (or opens nothing) rather than being handed a bad flag.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_omits_pr_repo_when_unmapped(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img"); // no pr_repo_map
    let (dispatcher, pods) = WrapperRecorder::new();

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");

    let wrapper = only_wrapper(&pods);
    assert!(
        !wrapper.contains("--pr-repo"),
        "no fork mapping -> no --pr-repo flag: {wrapper}"
    );
    Ok(())
}

/// The compatibility promise, asserted rather than eyeballed: with no provider registered,
/// resolution answers nothing, the loop wrapper carries neither `--harness` nor `--model`, and the
/// rendered pod is the one the controller rendered before the registry existed.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_empty_registry_renders_the_pod_it_always_did(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let cfg = pod_cfg(&profile, "img");
    let pack = crate::testing::fixtures::write_loop_pack(tmp.path());

    let resolved = crate::playbooks::providers::resolve_dispatch(
        &pool,
        None,
        Some("owner/repo"),
        crate::playbooks::providers::WorkloadClass::Autoresearch,
    )
    .await?;
    assert_eq!(resolved, None, "nothing registered, nothing resolved");

    // The pre-registry render, reproduced from the engine's own options rather than from anything
    // this feature computes: whatever `AgentSelection` does, an unresolved dispatch has to produce
    // that pod byte for byte.
    let before_the_registry = render_run_docs(
        &pack,
        &profile,
        "cm",
        &RunRenderOpts::Loop {
            iterations: cfg.effective().run_iterations,
            max_cost: cfg.effective().run_max_cost,
            pr_repo: cfg.pr_repo_for("owner/repo"),
            agent: AgentSelection {
                harness: None,
                model: None,
            },
        },
        None,
    )?
    .0;
    let with_the_registry = render_run_docs(
        &pack,
        &profile,
        "cm",
        &RunRenderOpts::for_loop(
            &cfg,
            "owner/repo",
            AgentSelection::from_resolved(resolved.as_ref()),
        ),
        None,
    )?
    .0;
    assert_eq!(
        with_the_registry, before_the_registry,
        "an unresolved dispatch renders the pre-registry pod"
    );

    let db = Db::new(pool);
    let (dispatcher, pods) = WrapperRecorder::new();
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &pack,
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(
            &cfg,
            "owner/repo",
            AgentSelection::from_resolved(resolved.as_ref()),
        ),
        None,
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");
    let wrapper = only_wrapper(&pods);
    assert!(!wrapper.contains("--harness"), "{wrapper}");
    assert!(!wrapper.contains("--model"), "{wrapper}");
    Ok(())
}

/// The other half of the promise: once a platform default names a provider, the pair it resolves
/// to reaches the loop wrapper as the flags the engine parses.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_resolved_provider_reaches_the_loop_wrapper(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let cfg = pod_cfg(&profile, "img");

    crate::playbooks::providers::upsert(
        &pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "openai-plat",
            display_name: "OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: None,
            secret: None,
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice",
        },
    )
    .await?;
    crate::playbooks::providers::set_default(
        &pool,
        &crate::playbooks::providers::DispatchDefault {
            scope_kind: crate::playbooks::providers::DefaultScope::Platform,
            scope_ref: String::new(),
            workload_class: crate::playbooks::providers::WorkloadClass::Autoresearch,
            provider_id: "openai-plat".to_string(),
            model: None,
        },
    )
    .await?;
    let resolved = crate::playbooks::providers::resolve_dispatch(
        &pool,
        None,
        Some("owner/repo"),
        crate::playbooks::providers::WorkloadClass::Autoresearch,
    )
    .await?;

    let db = Db::new(pool);
    let (dispatcher, pods) = WrapperRecorder::new();
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(
            &cfg,
            "owner/repo",
            AgentSelection::from_resolved(resolved.as_ref()),
        ),
        None,
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");
    let wrapper = only_wrapper(&pods);
    assert!(wrapper.contains("--harness=codex"), "{wrapper}");
    assert!(wrapper.contains("--model=gpt-5.6-luna"), "{wrapper}");
    Ok(())
}

/// A playbook launch renders the same pair as `plan run` flags, replacing the pack manifest's
/// `[agent]` table for every task that does not pin its own.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_resolved_provider_reaches_the_playbook_wrapper(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let cfg = pod_cfg(&profile, "img");

    crate::playbooks::providers::upsert(
        &pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "openai-plat",
            display_name: "OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: None,
            secret: None,
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice",
        },
    )
    .await?;
    let resolved = crate::playbooks::providers::resolve_dispatch(
        &pool,
        Some(crate::playbooks::providers::DispatchOverride {
            provider_id: "openai-plat",
            model: Some("gpt-5.6-luna"),
        }),
        Some("owner/repo"),
        crate::playbooks::providers::WorkloadClass::Playbook,
    )
    .await?;

    let db = Db::new(pool);
    let (dispatcher, pods) = WrapperRecorder::new();
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "playbook:survey:1",
        "playbook_survey_1-42",
        &crate::testing::fixtures::write_playbook_pack(
            tmp.path(),
            crate::testing::fixtures::WORKFLOW_TOPIC_DEPTH,
        ),
        &BTreeMap::new(),
        None,
        RunRenderOpts::Playbook {
            params: vec![("topic".to_string(), "attention sinks".to_string())],
            max_cost: 3.5,
            max_time: crate::model::MaxTime::parse("30m").expect("duration"),
            agent: AgentSelection::from_resolved(resolved.as_ref()),
        },
        None,
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");
    let wrapper = only_wrapper(&pods);
    assert!(
        wrapper.contains("crucible plan run --manifest")
            && wrapper.contains("--harness=codex --model=gpt-5.6-luna --param"),
        "{wrapper}"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_caps_at_the_concurrency_limit(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.max_concurrent_pods = 2;
    // Two runs already in flight (two issues at `running`) → the cap is full.
    seed_running_issue(&db, "owner/repo#1").await?;
    seed_running_issue(&db, "owner/repo#2").await?;

    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#3",
        "owner_repo_3-1",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;

    assert!(
        matches!(out, RunAdmission::Capped),
        "over cap → capped: {out:?}"
    );
    assert!(
        created.lock().expect("lock").is_empty(),
        "no pod created when capped"
    );
    assert!(
        crate::runs::work_pods::work_pods_in_states(
            db.pool(),
            &[WorkPodState::Running, WorkPodState::Failed]
        )
        .await?
        .is_empty(),
        "no work_pods row inserted for a declined run"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_honors_a_runtime_override_tightening_the_cap(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    // Base cap of 2 WOULD admit a third-slot check with one running, but a runtime override
    // tightens it to 1 — so with one run already in flight the dispatch must decline. This proves
    // the migrated read site (`cfg.effective().max_concurrent_pods`) honors the override.
    cfg.profile.max_concurrent_pods = 2;
    let store = crate::daemon::overrides_store::ConfigStore::seeded_for_test(
        cfg.base_config(),
        crate::daemon::overrides_store::OverrideSet {
            max_concurrent_pods: Some(1),
            ..Default::default()
        },
    );
    cfg.overrides = Some(store);
    seed_running_issue(&db, "owner/repo#1").await?;

    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#2",
        "owner_repo_2-1",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        None,
    )
    .await?;

    assert!(
        matches!(out, RunAdmission::Capped),
        "the override caps at 1 with one running → capped: {out:?}"
    );
    assert!(created.lock().expect("lock").is_empty(), "no pod created");
    Ok(())
}

/// The playbook render contract: the stored launch row's values reach the render as repeated
/// `--param name=value` (one argument each, so a value opening with a dash can never read as a
/// flag) alongside the launcher's ceilings. No `--iterations`: a playbook runs its graph once, not
/// a scored multi-turn loop. No `--pr-repo`: nothing links a PR back to a launch key.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_run_renders_a_playbook_launch_from_its_stored_row(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    // Mapped, to prove a playbook takes no `--pr-repo` even when the map would answer.
    cfg.pr_repo_map = vec!["survey=wren/repo-fork".to_string()];
    let (dispatcher, pods) = WrapperRecorder::new();

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "playbook:survey:0199c0de-7c2c-71a5-8000-1",
        "playbook_survey_0199c0de-42",
        &crate::testing::fixtures::write_playbook_pack(
            tmp.path(),
            crate::testing::fixtures::WORKFLOW_TOPIC_DEPTH,
        ),
        &BTreeMap::new(),
        None,
        RunRenderOpts::Playbook {
            params: vec![
                ("depth".to_string(), "--deep".to_string()),
                ("topic".to_string(), "attention sinks".to_string()),
            ],
            max_cost: 3.5,
            max_time: crate::model::MaxTime::parse("30m").expect("duration"),
            agent: AgentSelection::default(),
        },
        None,
    )
    .await?;
    assert!(
        matches!(out, RunAdmission::Launched { .. }),
        "expected a launch, got {out:?}"
    );

    let wrapper = only_wrapper(&pods);
    assert!(
        wrapper.contains("crucible plan run --manifest"),
        "a playbook launch renders in playbook mode: {wrapper}"
    );
    assert!(
        wrapper.contains("--param 'depth=--deep' --param 'topic=attention sinks'"),
        "each param is one quoted argument, so a value opening with a dash never reads as a \
         flag: {wrapper}"
    );
    assert!(wrapper.contains("--max-cost 3.5"), "{wrapper}");
    assert!(wrapper.contains("--max-time 1800s"), "{wrapper}");
    assert!(
        !wrapper.contains("--iterations"),
        "a playbook runs its graph once: {wrapper}"
    );
    assert!(
        !wrapper.contains("--pr-repo"),
        "a launch has nothing to link a PR back to: {wrapper}"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn collect_run_pod_marks_collected_and_gcs_the_pod(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: "crucible-run-x".to_string(),
            kind: WorkKind::Run.label_value().to_string(),
            issue_key: Some("owner/repo#7".to_string()),
            state: WorkPodState::Running,
            cost_tag: WorkKind::Run.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;

    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    };
    collect_run_pod(
        &db,
        &dispatcher,
        "autoresearch",
        "crucible-run-x",
        RunDisposition::Finished,
    )
    .await?;

    let row = crate::runs::work_pods::get_work_pod(db.pool(), "crucible-run-x")
        .await?
        .expect("row");
    assert_eq!(row.state, WorkPodState::Collected);
    assert_eq!(
        row.result.as_deref(),
        Some("loop run finished; session ingested")
    );
    assert_eq!(
        deleted.lock().expect("lock").as_slice(),
        &["crucible-run-x"]
    );
    // No ledger row: a run's cost is the session ingest's single booking, never collection's.
    let today = crate::clock::today_utc();
    assert!((crate::ledger::ledger_day_total(db.pool(), &today).await? - 0.0).abs() < 1e-9);
    // Idempotent: a second collect on the already-collected row is a clean no-op.
    collect_run_pod(
        &db,
        &dispatcher,
        "autoresearch",
        "crucible-run-x",
        RunDisposition::Finished,
    )
    .await?;
    assert_eq!(deleted.lock().expect("lock").len(), 1, "no second delete");
    Ok(())
}

/// The ledger note outlives the pod, so a run that errored may not leave one describing success.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn collect_run_pod_notes_an_errored_run_as_errored(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: "crucible-run-err".to_string(),
            kind: WorkKind::Run.label_value().to_string(),
            issue_key: Some("owner/repo#9".to_string()),
            state: WorkPodState::Running,
            cost_tag: WorkKind::Run.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    let dispatcher = FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: Arc::new(Mutex::new(Vec::new())),
    };

    collect_run_pod(
        &db,
        &dispatcher,
        "autoresearch",
        "crucible-run-err",
        RunDisposition::Errored,
    )
    .await?;

    let row = crate::runs::work_pods::get_work_pod(db.pool(), "crucible-run-err")
        .await?
        .expect("row");
    assert_eq!(row.state, WorkPodState::Collected);
    assert_eq!(
        row.result.as_deref(),
        Some("loop run errored; session ingested"),
        "the note may not read as a finish"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn startup_sweep_fails_a_dead_run_and_sweeps_a_retained_one(
    pool: sqlx::PgPool,
) -> Result<()> {
    let db = Db::new(pool);
    // A running run whose pod is observed Failed at startup → converge onto Failed (retention).
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: "crucible-run-dead".to_string(),
            kind: WorkKind::Run.label_value().to_string(),
            issue_key: Some("owner/repo#1".to_string()),
            state: WorkPodState::Running,
            cost_tag: WorkKind::Run.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    // A long-terminal failed run past its retention window → swept + deleted.
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: "crucible-run-old".to_string(),
            kind: WorkKind::Run.label_value().to_string(),
            issue_key: Some("owner/repo#2".to_string()),
            state: WorkPodState::Failed,
            cost_tag: WorkKind::Run.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    // Backdate the failed row's terminal clock well past retention (runtime query — a test-only
    // fixed string, kept out of the compile-time query cache).
    sqlx::query(
            "UPDATE work_pods SET terminal_at = '2000-01-01T00:00:00Z' WHERE pod_name = 'crucible-run-old'",
        )
        .execute(db.pool())
        .await?;

    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = FakeDispatcher {
        phase: TurnPhase::Failed,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    };
    reconcile_on_startup(&db, &dispatcher, "autoresearch", 20).await;

    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "crucible-run-dead")
            .await?
            .unwrap()
            .state,
        WorkPodState::Failed,
        "the dead run converged onto the failed-pod policy"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "crucible-run-old")
            .await?
            .unwrap()
            .state,
        WorkPodState::Swept,
        "the retained failed run was swept"
    );
    assert!(
        deleted
            .lock()
            .expect("lock")
            .contains(&"crucible-run-old".to_string()),
        "the swept pod was deleted"
    );
    Ok(())
}

// --- failed-pod count cap ---------------------------------------------------------------------

async fn seed_terminal_pod(
    db: &Db,
    name: &str,
    state: WorkPodState,
    terminal_at: &str,
) -> Result<()> {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: name.to_string(),
            kind: WorkKind::AgentTurn(TurnKind::GroundedRank)
                .label_value()
                .to_string(),
            issue_key: Some(format!("owner/repo#{name}")),
            state,
            cost_tag: "rank-grounded".to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    // Backdate the terminal clock (insert stamps it "now").
    sqlx::query("UPDATE work_pods SET terminal_at = $1 WHERE pod_name = $2")
        .bind(terminal_at)
        .bind(name)
        .execute(db.pool())
        .await?;
    Ok(())
}

fn fake_deleting_dispatcher() -> (FakeDispatcher, Arc<Mutex<Vec<String>>>) {
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = FakeDispatcher {
        phase: TurnPhase::Failed,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    };
    (dispatcher, deleted)
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn count_cap_sweeps_past_the_newest_n_even_inside_the_time_window(
    pool: sqlx::PgPool,
) -> Result<()> {
    let db = Db::new(pool);
    // Five retained-terminal rows, all well inside 24h of each other — the time sweep would
    // keep every one. An uncollected `succeeded` row counts against the cap too (its pod may
    // still sit on the cluster).
    seed_terminal_pod(&db, "pod-1", WorkPodState::Failed, "2026-07-04T01:00:00Z").await?;
    seed_terminal_pod(&db, "pod-2", WorkPodState::Failed, "2026-07-04T02:00:00Z").await?;
    seed_terminal_pod(
        &db,
        "pod-3",
        WorkPodState::Succeeded,
        "2026-07-04T03:00:00Z",
    )
    .await?;
    seed_terminal_pod(&db, "pod-4", WorkPodState::Failed, "2026-07-04T04:00:00Z").await?;
    seed_terminal_pod(&db, "pod-5", WorkPodState::Failed, "2026-07-04T05:00:00Z").await?;

    let (dispatcher, deleted) = fake_deleting_dispatcher();
    sweep_failed_pod_overflow(&db, &dispatcher, "autoresearch", 2).await;

    let mut swept = deleted.lock().expect("lock").clone();
    swept.sort();
    assert_eq!(
        swept,
        vec!["pod-1", "pod-2", "pod-3"],
        "the three oldest overflow the cap and their pods are deleted"
    );
    for name in ["pod-1", "pod-2", "pod-3"] {
        assert_eq!(
            crate::runs::work_pods::get_work_pod(db.pool(), name)
                .await?
                .unwrap()
                .state,
            WorkPodState::Swept
        );
    }
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-4")
            .await?
            .unwrap()
            .state,
        WorkPodState::Failed,
        "the newest N stay retained"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-5")
            .await?
            .unwrap()
            .state,
        WorkPodState::Failed
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn count_cap_at_exactly_n_and_zero_boundaries(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    seed_terminal_pod(&db, "pod-1", WorkPodState::Failed, "2026-07-04T01:00:00Z").await?;
    seed_terminal_pod(&db, "pod-2", WorkPodState::Failed, "2026-07-04T02:00:00Z").await?;

    // Exactly N retained rows → nothing swept.
    let (dispatcher, deleted) = fake_deleting_dispatcher();
    sweep_failed_pod_overflow(&db, &dispatcher, "autoresearch", 2).await;
    assert!(
        deleted.lock().expect("lock").is_empty(),
        "at the cap → no-op"
    );

    // keep = 0 → retain nothing: every terminal-retained pod goes now.
    let (dispatcher, deleted) = fake_deleting_dispatcher();
    sweep_failed_pod_overflow(&db, &dispatcher, "autoresearch", 0).await;
    let mut swept = deleted.lock().expect("lock").clone();
    swept.sort();
    assert_eq!(swept, vec!["pod-1", "pod-2"]);
    for name in ["pod-1", "pod-2"] {
        assert_eq!(
            crate::runs::work_pods::get_work_pod(db.pool(), name)
                .await?
                .unwrap()
                .state,
            WorkPodState::Swept
        );
    }
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn time_sweep_still_fires_under_the_count_cap(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    // One ancient failure (past 24h) + one fresh one — both comfortably under a cap of 10.
    seed_terminal_pod(&db, "pod-old", WorkPodState::Failed, "2000-01-01T00:00:00Z").await?;
    let now = crate::clock::now_rfc3339();
    seed_terminal_pod(&db, "pod-new", WorkPodState::Failed, &now).await?;

    let (dispatcher, deleted) = fake_deleting_dispatcher();
    reconcile_on_startup(&db, &dispatcher, "autoresearch", 10).await;

    assert_eq!(
        deleted.lock().expect("lock").as_slice(),
        &["pod-old"],
        "the count cap never extends the time window"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-old")
            .await?
            .unwrap()
            .state,
        WorkPodState::Swept
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-new")
            .await?
            .unwrap()
            .state,
        WorkPodState::Failed,
        "a fresh failure under both limits stays retained"
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_new_retained_failure_triggers_the_count_cap_inline(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.failed_pod_keep = 1;
    // A pre-existing retained failure; the next failed turn must push it out of the cap.
    seed_terminal_pod(&db, "pod-old", WorkPodState::Failed, "2026-07-04T01:00:00Z").await?;

    let deleted = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Failed,
        message: None,
        logs: "podman exploded, no marker here\n".to_string(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: deleted.clone(),
    });
    // Launch, then collect: the count-cap sweep fires when the turn is collected FAILED.
    let out =
        dispatch_grounded_rank(&db, &cfg, dispatcher.clone(), "owner/repo#9", "u", None).await?;
    assert!(matches!(out, DispatchOutcome::Launched), "{out:?}");
    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#9", "u", None).await?;
    assert!(matches!(out, DispatchOutcome::Failed), "{out:?}");

    assert_eq!(
        deleted.lock().expect("lock").as_slice(),
        &["pod-old"],
        "the failure that breached the cap swept the oldest retained pod inline"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-old")
            .await?
            .unwrap()
            .state,
        WorkPodState::Swept
    );
    let failed =
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Failed]).await?;
    assert_eq!(failed.len(), 1, "only the fresh failure stays retained");
    assert!(
        failed[0]
            .pod_name
            .starts_with("crucible-turn-owner-repo-9-")
    );
    Ok(())
}

// --- non-blocking fan-out + out-of-band timeout sweep ---------------------------------------

/// The whole point of non-blocking dispatch: distinct issues fan out to CONCURRENT turn pods up
/// to the per-kind cap, and the issue over the cap queues (never blocks the others). Under the
/// old in-band await only one turn could ever run.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_fans_out_to_the_cap_then_queues(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.grounded_rank_pod_cap = 2; // two may run at once

    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    // Two distinct issues both LAUNCH — two pods run concurrently, neither blocked on the other.
    for key in ["owner/repo#1", "owner/repo#2"] {
        let out = dispatch_grounded_rank(&db, &cfg, dispatcher.clone(), key, "u", None).await?;
        assert!(matches!(out, DispatchOutcome::Launched), "{key}: {out:?}");
    }
    assert_eq!(
        crate::runs::work_pods::count_active_work_pods(db.pool(), "grounded-rank").await?,
        2,
        "both turns run at once, up to the cap"
    );
    assert_eq!(created.lock().expect("lock").len(), 2, "two pods created");

    // The third issue is over the cap → it queues, and creates no pod (the others keep running).
    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#3", "u", None).await?;
    assert!(
        matches!(out, DispatchOutcome::Queued),
        "over the cap → queued, not blocked: {out:?}"
    );
    assert_eq!(created.lock().expect("lock").len(), 2, "no third pod");
    let queued =
        crate::runs::work_pods::work_pods_in_states(db.pool(), &[WorkPodState::Queued]).await?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].issue_key.as_deref(), Some("owner/repo#3"));
    Ok(())
}

/// The scope deadline scales with the gaming-refine allowance: skip-review is the flat base,
/// each cycle adds headroom, and a 6-cycle grant clears 2h comfortably (the live 3-cycle turn
/// ran ~82 min, and 6 cycles cannot fit in the old flat 90 min).
#[cfg(feature = "autoresearch")]
#[test]
fn scope_deadline_scales_with_the_gaming_allowance() {
    assert_eq!(scope_deadline(0), SCOPE_TIMEOUT, "skip-review is the base");
    assert!(
        scope_deadline(1) > scope_deadline(0),
        "each cycle adds headroom"
    );
    assert!(
        scope_deadline(6) >= scope_deadline(3),
        "monotonic in the allowance"
    );
    assert!(
        scope_deadline(6) > Duration::from_secs(2 * 60 * 60),
        "a 6-cycle grant clears 2h comfortably: got {}s",
        scope_deadline(6).as_secs()
    );
}

/// Seed a `running` turn row backdated to `created_at` — a turn dispatched in the past, so the
/// timeout sweep can decide whether it overran.
#[cfg(feature = "autoresearch")]
async fn seed_running_turn_at(
    db: &Db,
    kind: WorkKind,
    issue_key: &str,
    pod_name: &str,
    created_at: &str,
) -> Result<()> {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: pod_name.to_string(),
            kind: kind.label_value().to_string(),
            issue_key: Some(issue_key.to_string()),
            state: WorkPodState::Running,
            cost_tag: kind.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    sqlx::query("UPDATE work_pods SET created_at = $1 WHERE pod_name = $2")
        .bind(created_at)
        .bind(pod_name)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// The out-of-band deadline enforcement, kind-agnostic: a `running` turn older than its deadline
/// is reaped — pod deleted, row CAS-failed with the timeout on it, issue key returned for
/// re-drive — while a fresh turn of the same kind is left alone.
#[cfg(feature = "autoresearch")]
async fn conformance_timeout_sweep_reaps_a_hung_turn_and_frees_its_row(
    db: &Db,
    cfg: &ControllerCfg,
    kind: WorkKind,
) -> Result<()> {
    // One hung turn (dispatched long ago) + one fresh turn (dispatched "now").
    seed_running_turn_at(db, kind, "owner/repo#1", "pod-hung", "2000-01-01T00:00:00Z").await?;
    let now = crate::clock::now_rfc3339();
    seed_running_turn_at(db, kind, "owner/repo#2", "pod-fresh", &now).await?;

    let (dispatcher, deleted) = fake_deleting_dispatcher();
    let redrive = sweep_timed_out_turns(db, &dispatcher, cfg).await;

    assert_eq!(
        deleted.lock().expect("lock").as_slice(),
        &["pod-hung"],
        "only the overran pod is killed"
    );
    assert_eq!(
        redrive,
        vec!["owner/repo#1".to_string()],
        "its issue re-drives"
    );
    let hung = crate::runs::work_pods::get_work_pod(db.pool(), "pod-hung")
        .await?
        .expect("row");
    assert_eq!(hung.state, WorkPodState::Failed);
    assert!(
        hung.error.as_deref().unwrap_or("").contains("deadline"),
        "the reason names the timeout: {:?}",
        hung.error
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-fresh")
            .await?
            .expect("row")
            .state,
        WorkPodState::Running,
        "a fresh turn is left running"
    );
    Ok(())
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn timeout_sweep_reaps_a_hung_grounded_turn_and_frees_its_row(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    conformance_timeout_sweep_reaps_a_hung_turn_and_frees_its_row(
        &db,
        &cfg,
        WorkKind::AgentTurn(TurnKind::GroundedRank),
    )
    .await
}

#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn timeout_sweep_reaps_a_hung_scope_turn_and_frees_its_row(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    conformance_timeout_sweep_reaps_a_hung_turn_and_frees_its_row(
        &db,
        &cfg,
        WorkKind::AgentTurn(TurnKind::Scope),
    )
    .await
}

/// A hung SCOPE turn's deadline scales with the gaming allowance: at a 6-cycle grant a turn ~2h
/// old is still within budget (not reaped), where the flat old timeout would have abandoned it.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn timeout_sweep_honors_the_scaled_scope_deadline(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.scope_gaming_rounds = 6; // a generous gaming grant → a long deadline
    let kind = WorkKind::AgentTurn(TurnKind::Scope);

    // A scope turn dispatched ~2h ago: past the old flat 90 min, but well inside the 6-cycle grant.
    let two_hours_ago = (jiff::Timestamp::now() - jiff::Span::new().hours(2))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    seed_running_turn_at(&db, kind, "owner/repo#7", "pod-scope", &two_hours_ago).await?;

    let (dispatcher, deleted) = fake_deleting_dispatcher();
    let redrive = sweep_timed_out_turns(&db, &dispatcher, &cfg).await;

    assert!(
        deleted.lock().expect("lock").is_empty(),
        "a 2h scope turn is within the 6-cycle deadline, not reaped"
    );
    assert!(redrive.is_empty());
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "pod-scope")
            .await?
            .expect("row")
            .state,
        WorkPodState::Running,
    );
    Ok(())
}

// --- queue drain: collection re-drives the backlog, park purges it -----------------------------

/// A test [`crate::daemon::queue::Enqueue`] that records the issue keys the drain re-drives.
#[cfg(feature = "autoresearch")]
#[derive(Default)]
struct RecordingEnqueue {
    keys: Arc<Mutex<Vec<String>>>,
}
#[cfg(feature = "autoresearch")]
impl crate::daemon::queue::Enqueue for RecordingEnqueue {
    fn enqueue(&self, key: crate::daemon::queue::IssueKey) {
        self.keys.lock().expect("lock").push(key.0);
    }
}

/// Seed a `queued` work-pod row backdated to `created_at`, so the FIFO drain order is deterministic.
async fn seed_queued_turn_at(
    db: &Db,
    kind: WorkKind,
    issue_key: &str,
    pod_name: &str,
    created_at: &str,
) -> Result<()> {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &NewWorkPod {
            pod_name: pod_name.to_string(),
            kind: kind.label_value().to_string(),
            issue_key: Some(issue_key.to_string()),
            state: WorkPodState::Queued,
            cost_tag: kind.cost_tag().to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await?;
    sqlx::query("UPDATE work_pods SET created_at = $1 WHERE pod_name = $2")
        .bind(created_at)
        .bind(pod_name)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Track an issue and force its status (a runtime string, kept out of the query cache) — a `parked`
/// row here simulates a queued turn whose issue parked before the park-purge existed (a stale row).
async fn seed_issue_status(db: &Db, key: &str, status: &str) -> Result<()> {
    crate::issues::store::upsert_issue(
        db.pool(),
        &crate::issues::model::NewIssue {
            key: key.to_string(),
            repo: "owner/repo".to_string(),
            priority: 0,
            evidence_url: None,
            title: None,
            author: None,
            body: None,
            labels: Vec::new(),
            upstream_updated_at: None,
        },
    )
    .await?;
    sqlx::query("UPDATE issues SET status = $1 WHERE key = $2")
        .bind(status)
        .bind(key)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Collecting a turn frees a slot, so its tail re-drives the eldest queued row's ISSUE back through
/// the reconcile queue (it never dispatches the pod itself). The queued row stays `queued` — the
/// promotion is the reconcile pass's job, not the drain's.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn collection_drains_the_eldest_queued_rows_issue(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);

    // A running turn to collect (issue A) + two queued backlog rows (B eldest, C younger).
    seed_running_turn(&db, kind, "owner/repo#1", "crucible-turn-a").await?;
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#2",
        "queued-b",
        "2026-07-05T01:00:00Z",
    )
    .await?;
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#3",
        "queued-c",
        "2026-07-05T02:00:00Z",
    )
    .await?;

    let recorder = RecordingEnqueue::default();
    let keys = recorder.keys.clone();
    install_enqueue(Arc::new(recorder));

    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"x\",\"cost_usd\":0.3,\"over_budget\":false}\n".to_string(),
            created: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
        });

    let out = adopt_grounded_turn(&db, &cfg, dispatcher, "owner/repo#1")
        .await?
        .expect("adopted");
    reset_enqueue();

    assert!(matches!(out, DispatchOutcome::Verdict(_)), "{out:?}");
    assert_eq!(
        keys.lock().expect("lock").as_slice(),
        &["owner/repo#2"],
        "the freed slot re-drives the ELDEST queued row's issue (FIFO), not the younger one"
    );
    // The drain only enqueues — the row is still queued until the reconcile pass promotes it.
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "queued-b")
            .await?
            .expect("row")
            .state,
        WorkPodState::Queued,
    );
    Ok(())
}

/// Parking an issue purges its queued work pods (future spend the park rejects), leaving any
/// RUNNING pod for that issue untouched — both park verbs (`park_issue` and the CAS `park`).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn park_purges_queued_rows(pool: sqlx::PgPool) -> Result<()> {
    let db = Db::new(pool);
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);

    // Machine park: a queued row for the issue is swept; a running row for it survives.
    seed_issue_status(&db, "owner/repo#1", "new").await?;
    seed_queued_turn_at(&db, kind, "owner/repo#1", "q-1", "2026-07-05T01:00:00Z").await?;
    seed_running_turn(&db, kind, "owner/repo#1", "run-1").await?;
    crate::issues::transitions::park_and_purge(
        db.pool(),
        "owner/repo#1",
        "no repro",
        crate::model::ParkedBy::Machine,
    )
    .await?;
    assert!(
        crate::runs::work_pods::get_work_pod(db.pool(), "q-1")
            .await?
            .is_none(),
        "the queued row is deleted on park"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "run-1")
            .await?
            .expect("row")
            .state,
        WorkPodState::Running,
        "a running turn is NOT purged (only queued spend is rejected)"
    );

    // CAS park (only the winner purges): a queued row for a `new` issue is swept.
    seed_issue_status(&db, "owner/repo#2", "new").await?;
    seed_queued_turn_at(&db, kind, "owner/repo#2", "q-2", "2026-07-05T01:00:00Z").await?;
    let won = crate::issues::transitions::park(
        db.pool(),
        db.events(),
        "owner/repo#2",
        crate::model::Status::New,
        &crate::model::ParkReason::Legacy("dead proposal".to_string()),
        crate::model::ParkedBy::Machine,
    )
    .await?;
    assert!(won, "the CAS park won");
    assert!(
        crate::runs::work_pods::get_work_pod(db.pool(), "q-2")
            .await?
            .is_none(),
        "the CAS park deleted the queued row"
    );
    Ok(())
}

/// A queued row whose issue is ALREADY parked (a stale pre-purge row) can never promote, so the
/// drain skips + purges it and re-drives the next drainable row instead of wedging.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn drain_skips_and_purges_a_parked_issue_queued_row(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);

    seed_running_turn(&db, kind, "owner/repo#1", "crucible-turn-a").await?;
    // The eldest queued row's issue is parked (stale); the next is live.
    seed_issue_status(&db, "owner/repo#9", "parked").await?;
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#9",
        "queued-parked",
        "2026-07-05T01:00:00Z",
    )
    .await?;
    seed_issue_status(&db, "owner/repo#8", "new").await?;
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#8",
        "queued-live",
        "2026-07-05T02:00:00Z",
    )
    .await?;

    let recorder = RecordingEnqueue::default();
    let keys = recorder.keys.clone();
    install_enqueue(Arc::new(recorder));
    let dispatcher = Arc::new(FakeDispatcher {
            phase: TurnPhase::Succeeded,
            message: None,
            logs: "CRUCIBLE_VERDICT: {\"tier\":\"T1\",\"rationale\":\"x\",\"cost_usd\":0.3,\"over_budget\":false}\n".to_string(),
            created: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
        });
    adopt_grounded_turn(&db, &cfg, dispatcher, "owner/repo#1")
        .await?
        .expect("adopted");
    reset_enqueue();

    assert!(
        crate::runs::work_pods::get_work_pod(db.pool(), "queued-parked")
            .await?
            .is_none(),
        "the parked-issue queued row is purged, not left to wedge the drain"
    );
    assert_eq!(
        keys.lock().expect("lock").as_slice(),
        &["owner/repo#8"],
        "the drain skips the parked row and re-drives the next drainable issue"
    );
    Ok(())
}

/// The timeout sweep backstops the collection drain: it returns the eldest queued keys up to the
/// grounded free-slot count (FIFO), so a missed completion edge can't strand the backlog.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn timeout_sweep_returns_queued_keys_up_to_free_slots(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    let db = Db::new(pool);
    let mut cfg = pod_cfg(&profile, "img");
    cfg.profile.grounded_rank_pod_cap = 2; // two slots, none running → two free
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);

    // A parked-issue row at the FIFO head (purged, not returned) + three live queued rows.
    seed_issue_status(&db, "owner/repo#0", "parked").await?;
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#0",
        "q-parked",
        "2026-07-05T00:00:00Z",
    )
    .await?;
    seed_queued_turn_at(&db, kind, "owner/repo#1", "q-1", "2026-07-05T01:00:00Z").await?;
    seed_queued_turn_at(&db, kind, "owner/repo#2", "q-2", "2026-07-05T02:00:00Z").await?;
    seed_queued_turn_at(&db, kind, "owner/repo#3", "q-3", "2026-07-05T03:00:00Z").await?;

    let (dispatcher, _deleted) = fake_deleting_dispatcher();
    let drained = sweep_timed_out_turns(&db, &dispatcher, &cfg).await;

    assert_eq!(
        drained,
        vec!["owner/repo#1".to_string(), "owner/repo#2".to_string()],
        "up to the two free slots, eldest-first, past the purged parked head"
    );
    assert!(
        crate::runs::work_pods::get_work_pod(db.pool(), "q-parked")
            .await?
            .is_none(),
        "the parked head row is purged by the sweep"
    );
    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "q-3")
            .await?
            .expect("row")
            .state,
        WorkPodState::Queued,
        "the row past the free-slot count stays queued for the next tick"
    );
    Ok(())
}

/// The queue-wait metric fires when a queued row is PROMOTED (the drain's downstream effect): a
/// backdated queued row dispatched onto a free slot records how long it waited.
#[cfg(feature = "autoresearch")]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn promote_observes_the_queue_wait_metric(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());

    let metrics = crate::metrics::Metrics::new()?;
    let db = Db::new(pool).with_metrics(metrics.clone());
    let cfg = pod_cfg(&profile, "img");
    let kind = WorkKind::AgentTurn(TurnKind::GroundedRank);

    // A queued row backdated so the observed wait is a positive sample, then a dispatch promotes it.
    seed_queued_turn_at(
        &db,
        kind,
        "owner/repo#5",
        "queued-old",
        "2026-07-05T00:00:00Z",
    )
    .await?;
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        message: None,
        logs: String::new(),
        created: Arc::new(Mutex::new(Vec::new())),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    let out = dispatch_grounded_rank(&db, &cfg, dispatcher, "owner/repo#5", "u", None).await?;
    assert!(
        matches!(out, DispatchOutcome::Launched),
        "the queued row promoted: {out:?}"
    );

    // The promote observed one queue-wait sample.
    use prometheus::Encoder as _;
    let mut buf = Vec::new();
    prometheus::TextEncoder::new()
        .encode(&metrics.registry().gather(), &mut buf)
        .expect("encode");
    let text = String::from_utf8(buf).expect("utf8");
    let count = text
        .lines()
        .find_map(|l| l.strip_prefix("crucible_workpod_queue_wait_seconds_count "))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    assert!(
        count >= 1.0,
        "the promote fired the queue-wait metric: count={count}\n{text}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Run secrets: what a dispatch delivers, and what a refusal costs.
// ---------------------------------------------------------------------------------------------

/// A dispatcher whose create always answers AlreadyExists: the pod the controller meant to create
/// is not the pod that is there.
struct AlreadyExistsDispatcher;

#[async_trait::async_trait]
impl PodDispatcher for AlreadyExistsDispatcher {
    async fn create(&self, _cluster: &str, _ns: &str, _pod: Pod) -> Result<Pod> {
        anyhow::bail!("pods \"crucible-run-x\" already exists")
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _n: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _n: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// Register one opaque secret owned by `owner` and bind it to the repo scope under `declared`.
async fn bind_repo_secret(
    pool: &sqlx::PgPool,
    owner: &str,
    declared: &str,
    visibility: crate::secrets::Visibility,
) -> Result<String> {
    use crate::authz::model::Principal;
    use crate::secrets::store::{NewBinding, NewSecret};
    use crate::secrets::{
        ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName,
    };
    let owner = Principal::parse(owner).expect("owner");
    let name = SecretName::parse(declared).expect("name");
    let id = uuid::Uuid::now_v7().to_string();
    let mut conn = pool.acquire().await?;
    crate::secrets::store::insert(
        &mut conn,
        &NewSecret {
            id: &id,
            name: &name,
            owner: &owner,
            kind: SecretKind::Opaque,
            visibility,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "user:alice/pr-token",
            current_version: Some(1),
            created_by: Some("alice"),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::secrets::store::insert_binding(
        &mut conn,
        &NewBinding {
            id: &uuid::Uuid::now_v7().to_string(),
            secret_id: &id,
            scope_kind: ScopeKind::Repo,
            scope_id: "owner/repo",
            projection_kind: ProjectionKind::Env,
            projection: "AUTORESEARCH_PR_TOKEN",
            declared_name: &name,
            pack_rev: None,
            schema_digest: None,
            created_by: Some("alice"),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(id)
}

fn repo_secrets(launcher: crate::authz::model::Principals) -> crate::runs::workpod::LaunchSecrets {
    crate::runs::workpod::LaunchSecrets {
        scope: crate::secrets::launch::Scope::repo("owner/repo"),
        launcher,
        revision: crate::secrets::launch::OwnedRevision::Published(None),
        provider: Some(std::sync::Arc::new(
            crate::secrets::provider::MapProvider::new([(
                "pr_token".to_string(),
                "value-of-pr_token".to_string(),
            )]),
        )),
        inference_provider: None,
        exposure: Some(crate::playbooks::exposure::Exposure {
            version: 1,
            outputs: Vec::new(),
            capabilities: vec![crate::playbooks::exposure::Capability::Known(
                crate::playbooks::exposure::KnownCapability::Credential {
                    name: "AUTORESEARCH_PR_TOKEN".to_string(),
                    context: crate::playbooks::exposure::CredentialContext::Agent,
                    system: Some("github".to_string()),
                    scope: None,
                },
            )],
        }),
    }
}

/// A dispatch that resolved bindings launches, and `work_pods` records the UID the create response
/// returned — the UID the run's Secret is owner-referenced to.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_dispatch_with_bindings_records_the_created_pods_uid(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created_pods = Arc::new(Mutex::new(Vec::new()));
    let created_cms = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(RunBundleDispatcher {
        created_pods: created_pods.clone(),
        created_cms: created_cms.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&repo_secrets(crate::authz::model::Principals::new(
            Some("alice"),
            &[],
        ))),
    )
    .await?;
    let RunAdmission::Launched { pod_name, .. } = out else {
        panic!("expected a launch, got {out:?}");
    };

    let row = crate::runs::work_pods::get_work_pod(db.pool(), &pod_name)
        .await?
        .expect("a work pod row");
    assert_eq!(row.pod_uid.as_deref(), Some("pod-uid-123"));
    Ok(())
}

/// The crk_ guard runs on the redeemed bytes, so a reference-mode far end rotated into a
/// controller key after binding still refuses the dispatch.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_redeemed_agent_visible_controller_key_refuses_the_dispatch(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::AgentVisible,
    )
    .await?;
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let dispatcher = Arc::new(RunBundleDispatcher {
        created_pods: Arc::new(Mutex::new(Vec::new())),
        created_cms: Arc::new(Mutex::new(Vec::new())),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    let secrets = crate::runs::workpod::LaunchSecrets {
        provider: Some(std::sync::Arc::new(
            crate::secrets::provider::MapProvider::new([(
                "pr_token".to_string(),
                format!(
                    "{}0123456789abcdef_s3cr3t",
                    crate::identity::api_key::PREFIX
                ),
            )]),
        )),
        ..repo_secrets(crate::authz::model::Principals::new(Some("alice"), &[]))
    };

    let err = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&secrets),
    )
    .await
    .expect_err("a controller key behind an agent_visible binding must refuse the dispatch");
    let msg = format!("{err:#}");
    assert!(msg.contains("pr_token"), "{msg}");
    assert!(msg.contains("controller api key"), "{msg}");
    Ok(())
}

/// The pod spec, its argv, and the pack ConfigMap carry a reference into the run's Secret and
/// nothing that maps a secret name to a value.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_pod_spec_carries_a_secret_reference_and_no_value(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created = Arc::new(Mutex::new(Vec::new()));
    let created_cms = Arc::new(Mutex::new(Vec::new()));
    let created_secrets = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(SpecCapturingDispatcher {
        created_secrets: created_secrets.clone(),
        created: created.clone(),
        created_cms: created_cms.clone(),
    });
    dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&repo_secrets(crate::authz::model::Principals::new(
            Some("alice"),
            &[],
        ))),
    )
    .await?;

    let pods = created.lock().expect("lock");
    let pod = pods.first().expect("a created pod");
    let rendered = serde_json::to_string(pod)?;
    let cms = created_cms.lock().expect("lock");
    let rendered_cm = serde_json::to_string(&*cms)?;
    // Under ADR-0036 the spec names the Secret and the key it reads: that is a reference, and the
    // value it resolves to must never appear in a document the API server stores. FakeValues
    // returns `value-of-<name>`, so the value is searchable text rather than an assumption.
    for doc in [&rendered, &rendered_cm] {
        assert!(
            !doc.contains("value-of-"),
            "a secret value leaked into a dispatched document: {doc}"
        );
    }
    let container = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .expect("a container");
    let env = container.env.as_ref().expect("env");

    // The projection reaches the container as a reference into the run's own Secret, and the
    // Secret is owned by the pod, so deleting the pod collects the credential with it.
    let projected = env
        .iter()
        .find(|v| v.name == "AUTORESEARCH_PR_TOKEN")
        .expect("the declared projection");
    assert!(projected.value.is_none(), "a reference, never a value");
    let selector = projected
        .value_from
        .as_ref()
        .and_then(|f| f.secret_key_ref.as_ref())
        .expect("a secretKeyRef");
    assert_eq!(selector.name, "crucible-run-owner-repo-7-42-secrets");
    assert_eq!(selector.key, "pr_token");

    let secrets = created_secrets.lock().expect("lock");
    let secret = secrets.first().expect("the run's Secret");
    assert_eq!(
        secret.string_data.as_ref().and_then(|d| d.get("pr_token")),
        Some(&"value-of-pr_token".to_string())
    );
    let owner = &secret
        .metadata
        .owner_references
        .as_ref()
        .expect("owner-referenced")[0];
    assert_eq!(owner.uid, "pod-uid-123");
    assert_eq!(owner.kind, "Pod");
    Ok(())
}

/// Register one inference key under `owner`, the way an administrator would before pointing a
/// provider at it. Unlike a scope binding it is never bound to anything: the provider row names it.
async fn register_inference_key(pool: &sqlx::PgPool, owner: &str, name: &str) -> Result<()> {
    use crate::authz::model::Principal;
    use crate::secrets::store::NewSecret;
    use crate::secrets::{ConsumerClass, SecretKind, SecretMode, SecretName, Visibility};
    let owner = Principal::parse(owner).expect("owner");
    let name = SecretName::parse(name).expect("name");
    let mut conn = pool.acquire().await?;
    crate::secrets::store::insert(
        &mut conn,
        &NewSecret {
            id: &uuid::Uuid::now_v7().to_string(),
            name: &name,
            owner: &owner,
            kind: SecretKind::InferenceApiKey,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "platform/openai-key",
            current_version: Some(1),
            created_by: Some("alice"),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// The resolved provider's key rides the same per-run Secret as the scope's own bindings, projected
/// as the environment variable its kind reads, and the value never appears in the pod spec.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_resolved_providers_key_is_delivered_with_the_scopes_bindings(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    register_inference_key(&pool, "user:platform-admin", "openai_key").await?;
    crate::playbooks::providers::upsert(
        &pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "openai-plat",
            display_name: "OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: None,
            secret: Some(&crate::playbooks::providers::ProviderSecretRef {
                name: "openai_key".to_string(),
                owner: crate::authz::model::Principal::parse("user:platform-admin")
                    .expect("a principal"),
            }),
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice",
        },
    )
    .await?;
    let provider = crate::playbooks::providers::get(&pool, "openai-plat")
        .await?
        .expect("the registered provider");

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created = Arc::new(Mutex::new(Vec::new()));
    let created_secrets = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(SpecCapturingDispatcher {
        created_secrets: created_secrets.clone(),
        created: created.clone(),
        created_cms: Arc::new(Mutex::new(Vec::new())),
    });
    let mut secrets = repo_secrets(crate::authz::model::Principals::new(Some("alice"), &[]));
    secrets.inference_provider = Some(provider);
    secrets.provider = Some(std::sync::Arc::new(
        crate::secrets::provider::MapProvider::new([
            ("pr_token".to_string(), "value-of-pr_token".to_string()),
            ("openai_key".to_string(), "value-of-openai_key".to_string()),
        ]),
    ));

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&secrets),
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");

    let pods = created.lock().expect("lock");
    let pod = pods.first().expect("a created pod");
    assert!(
        !serde_json::to_string(pod)?.contains("value-of-"),
        "a secret value leaked into the pod spec"
    );
    let env = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .and_then(|c| c.env.as_ref())
        .expect("env");
    let key = env
        .iter()
        .find(|v| v.name == "OPENAI_API_KEY")
        .expect("the provider's key");
    let selector = key
        .value_from
        .as_ref()
        .and_then(|f| f.secret_key_ref.as_ref())
        .expect("a secretKeyRef");
    assert_eq!(selector.name, "crucible-run-owner-repo-7-42-secrets");
    assert_eq!(selector.key, "openai_key.OPENAI_API_KEY");
    assert!(
        env.iter().any(|v| v.name == "AUTORESEARCH_PR_TOKEN"),
        "the scope's own binding still rides along"
    );

    let secrets = created_secrets.lock().expect("lock");
    let data = secrets
        .first()
        .expect("the run's Secret")
        .string_data
        .as_ref()
        .expect("data");
    assert_eq!(
        data.get("openai_key.OPENAI_API_KEY"),
        Some(&"value-of-openai_key".to_string())
    );
    assert_eq!(data.get("pr_token"), Some(&"value-of-pr_token".to_string()));
    Ok(())
}

/// A custom provider hands the pod where to reach it as plain environment beside the secretKeyRef
/// that carries its key: the base URL under the name the harness reads, the Codex wire API the
/// engine renders into its config, and the harness flag its protocol decides.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_custom_provider_delivers_its_endpoint_beside_its_key(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    register_inference_key(&pool, "user:platform-admin", "vllm_key").await?;
    let endpoint = crate::playbooks::providers::Endpoint {
        url: "http://vllm.internal:8000/v1".to_string(),
        protocol: crate::playbooks::providers::InferenceProtocol::Responses,
    };
    crate::playbooks::providers::upsert(
        &pool,
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "onprem",
            display_name: "On-prem vLLM",
            kind: crate::playbooks::providers::ProviderKind::Custom,
            models: &[],
            default_model: Some("gpt-oss-120b"),
            secret: Some(&crate::playbooks::providers::ProviderSecretRef {
                name: "vllm_key".to_string(),
                owner: crate::authz::model::Principal::parse("user:platform-admin")
                    .expect("a principal"),
            }),
            endpoint: Some(&endpoint),
            harness: None,
            enabled: true,
            created_by: "alice",
        },
    )
    .await?;
    let resolved = crate::playbooks::providers::resolve_dispatch(
        &pool,
        Some(crate::playbooks::providers::DispatchOverride {
            provider_id: "onprem",
            model: None,
        }),
        None,
        crate::playbooks::providers::WorkloadClass::Autoresearch,
    )
    .await?;
    let provider = resolved.as_ref().map(|r| r.provider.clone());

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created = Arc::new(Mutex::new(Vec::new()));
    let created_secrets = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(SpecCapturingDispatcher {
        created_secrets: created_secrets.clone(),
        created: created.clone(),
        created_cms: Arc::new(Mutex::new(Vec::new())),
    });
    let mut secrets = repo_secrets(crate::authz::model::Principals::new(Some("alice"), &[]));
    secrets.inference_provider = provider;
    secrets.provider = Some(std::sync::Arc::new(
        crate::secrets::provider::MapProvider::new([(
            "vllm_key".to_string(),
            r#"{"OPENAI_API_KEY": "sk-onprem"}"#.to_string(),
        )]),
    ));

    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(
            &cfg,
            "owner/repo",
            AgentSelection::from_resolved(resolved.as_ref()),
        ),
        Some(&secrets),
    )
    .await?;
    assert!(matches!(out, RunAdmission::Launched { .. }), "got {out:?}");

    let pods = created.lock().expect("lock");
    let pod = pods.first().expect("a created pod");
    let doc = serde_json::to_string(pod)?;
    assert!(
        !doc.contains("sk-onprem"),
        "the key leaked into the pod spec"
    );
    let container = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .expect("the run container");
    let env = container.env.as_ref().expect("env");
    let var = |name: &str| env.iter().find(|v| v.name == name);
    assert_eq!(
        var("OPENAI_BASE_URL")
            .and_then(|v| v.value.clone())
            .as_deref(),
        Some("http://vllm.internal:8000/v1")
    );
    assert_eq!(
        var(crate::playbooks::providers::WIRE_API_ENV)
            .and_then(|v| v.value.clone())
            .as_deref(),
        Some("responses")
    );
    let key = var("OPENAI_API_KEY").expect("the key");
    assert!(
        key.value.is_none() && key.value_from.is_some(),
        "the key rides a secretKeyRef"
    );
    let wrapper = container
        .args
        .as_ref()
        .map(|a| a.join(" "))
        .unwrap_or_default();
    assert!(wrapper.contains("--harness=codex"), "{wrapper}");
    assert!(wrapper.contains("--model=gpt-oss-120b"), "{wrapper}");
    let secrets = created_secrets.lock().expect("lock");
    let data = secrets
        .first()
        .expect("the Secret")
        .string_data
        .as_ref()
        .expect("data");
    assert_eq!(
        data.get("vllm_key.OPENAI_API_KEY").map(String::as_str),
        Some("sk-onprem")
    );
    Ok(())
}

/// A provider key the registry cannot answer for refuses the dispatch outright: no pod, no Secret,
/// no work-pod row. The same shape covers a credentials map that would set a variable one of the
/// scope's own bindings already holds, which is the other way a provider's delivery is refused.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_provider_key_the_dispatch_cannot_resolve_refuses_the_run(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    let admin = crate::authz::model::Principal::parse("user:platform-admin").expect("a principal");
    // Registered nowhere: `register_inference_key` is deliberately not called.
    let missing = crate::playbooks::providers::ModelProvider {
        owner: crate::authz::model::Principal::platform(),
        id: "openai-plat".to_string(),
        display_name: "OpenAI".to_string(),
        kind: crate::playbooks::providers::ProviderKind::OpenAi,
        models: Vec::new(),
        default_model: "gpt-5.6-luna".to_string(),
        secret: Some(crate::playbooks::providers::ProviderSecretRef {
            name: "openai_key".to_string(),
            owner: admin.clone(),
        }),
        endpoint: None,
        harness: None,
        enabled: true,
        created_by: "alice".to_string(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    // Registered, but its credentials map also sets the variable the scope's own binding holds.
    register_inference_key(&pool, "user:platform-admin", "pr_token").await?;
    let colliding = crate::playbooks::providers::ModelProvider {
        owner: crate::authz::model::Principal::platform(),
        secret: Some(crate::playbooks::providers::ProviderSecretRef {
            name: "pr_token".to_string(),
            owner: admin,
        }),
        kind: crate::playbooks::providers::ProviderKind::Anthropic,
        ..missing.clone()
    };

    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let pack = crate::testing::fixtures::write_loop_pack(tmp.path());
    for (provider, expected) in [
        (missing, "which the registry does not hold"),
        (colliding, "already holds"),
    ] {
        let created = Arc::new(Mutex::new(Vec::new()));
        let created_secrets = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Arc::new(SpecCapturingDispatcher {
            created_secrets: created_secrets.clone(),
            created: created.clone(),
            created_cms: Arc::new(Mutex::new(Vec::new())),
        });
        let mut secrets = repo_secrets(crate::authz::model::Principals::new(Some("alice"), &[]));
        secrets.provider = Some(std::sync::Arc::new(
            crate::secrets::provider::MapProvider::new([
                (
                    "pr_token".to_string(),
                    r#"{"ANTHROPIC_API_KEY": "sk-ant", "AUTORESEARCH_PR_TOKEN": "leaked"}"#
                        .to_string(),
                ),
                ("openai_key".to_string(), "value-of-openai_key".to_string()),
            ]),
        ));
        secrets.inference_provider = Some(provider);

        let out = dispatch_run(
            &db,
            &cfg,
            dispatcher,
            "owner/repo#7",
            "owner_repo_7-42",
            &pack,
            &BTreeMap::new(),
            None,
            RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
            Some(&secrets),
        )
        .await?;
        let RunAdmission::SecretsRefused { reason } = out else {
            panic!("expected a refusal, got {out:?}");
        };
        assert!(reason.contains(expected), "{reason}");
        assert!(
            created.lock().expect("lock").is_empty(),
            "no pod was created"
        );
        assert!(
            created_secrets.lock().expect("lock").is_empty(),
            "no Secret was created"
        );
        assert!(
            crate::runs::work_pods::get_work_pod(db.pool(), &run_pod_name("owner_repo_7-42"))
                .await?
                .is_none(),
            "a refusal leaves no work-pod row"
        );
    }
    Ok(())
}

/// A dispatcher that keeps the whole created pod (not just its name), so a test can read the spec
/// the API server would have stored.
struct SpecCapturingDispatcher {
    created: Arc<Mutex<Vec<Pod>>>,
    created_cms: Arc<Mutex<Vec<ConfigMap>>>,
    created_secrets: Arc<Mutex<Vec<k8s_openapi::api::core::v1::Secret>>>,
}

#[async_trait::async_trait]
impl PodDispatcher for SpecCapturingDispatcher {
    async fn create_secret(
        &self,
        _cluster: &str,
        _ns: &str,
        secret: k8s_openapi::api::core::v1::Secret,
    ) -> Result<()> {
        self.created_secrets.lock().expect("lock").push(secret);
        Ok(())
    }

    async fn create(&self, _cluster: &str, _ns: &str, mut pod: Pod) -> Result<Pod> {
        pod.metadata.uid = Some("pod-uid-123".to_string());
        self.created.lock().expect("lock").push(pod.clone());
        Ok(pod)
    }
    async fn create_configmap(&self, _cluster: &str, _ns: &str, cm: ConfigMap) -> Result<()> {
        self.created_cms.lock().expect("lock").push(cm);
        Ok(())
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _n: &str,
        _t: Duration,
    ) -> Result<TerminalState> {
        Ok(TerminalState {
            phase: TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _n: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// The pod the controller meant to create is not the pod that is there, so the dispatch fails and
/// its work-pod row retains the reason.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_already_exists_create_fails_the_dispatch(pool: sqlx::PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "user:alice",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let err = dispatch_run(
        &db,
        &cfg,
        Arc::new(AlreadyExistsDispatcher),
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&repo_secrets(crate::authz::model::Principals::new(
            Some("alice"),
            &[],
        ))),
    )
    .await
    .expect_err("the create failed");
    assert!(format!("{err:#}").contains("already exists"));

    let row = crate::runs::work_pods::get_work_pod(db.pool(), &run_pod_name("owner_repo_7-42"))
        .await?
        .expect("a work pod row");
    assert_eq!(row.state, WorkPodState::Failed);
    assert!(
        row.error
            .as_deref()
            .is_some_and(|e| e.contains("already exists")),
        "the row keeps the reason: {:?}",
        row.error
    );
    Ok(())
}

/// A launcher who covers none of the bound owners is refused by name, and the refusal costs
/// nothing: no work-pod row and no pod.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_launcher_who_does_not_own_a_bound_secret_is_refused_before_anything_is_written(
    pool: sqlx::PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    let profile = crate::testing::fixtures::write_deploy_profile(tmp.path());
    bind_repo_secret(
        &pool,
        "group:/groups/team-x",
        "pr_token",
        crate::secrets::Visibility::BrokerOnly,
    )
    .await?;
    let db = Db::new(pool);
    let cfg = pod_cfg(&profile, "img");
    let created = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(FakeDispatcher {
        phase: TurnPhase::Succeeded,
        logs: String::new(),
        message: None,
        created: created.clone(),
        deleted: Arc::new(Mutex::new(Vec::new())),
    });
    let out = dispatch_run(
        &db,
        &cfg,
        dispatcher,
        "owner/repo#7",
        "owner_repo_7-42",
        &crate::testing::fixtures::write_loop_pack(tmp.path()),
        &BTreeMap::new(),
        None,
        RunRenderOpts::for_loop(&cfg, "owner/repo", AgentSelection::default()),
        Some(&repo_secrets(crate::authz::model::Principals::new(
            Some("bob"),
            &["/groups/team-y".to_string()],
        ))),
    )
    .await?;
    let RunAdmission::SecretsRefused { reason } = out else {
        panic!("expected a refusal, got {out:?}");
    };
    assert!(reason.contains("pr_token"), "names the secret: {reason}");
    assert!(created.lock().expect("lock").is_empty(), "no pod created");
    assert!(
        crate::runs::work_pods::get_work_pod(db.pool(), &run_pod_name("owner_repo_7-42"))
            .await?
            .is_none(),
        "no work-pod row written"
    );
    Ok(())
}
