#![allow(clippy::disallowed_macros)]

use super::grounded::TierGate;
use super::grounded::*;
use super::lifecycle::*;
use super::scope::*;
use super::*;
use crate::Db;
use crate::config::Profile;
use crate::issues::model::{NewIssue, NewScope};
use crate::model::{ParkReason, ParkedBy};
use crate::runs::completion::*;
use crate::runs::model::NewRun;
use crucible_contract::Tier;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

fn db_with(pool: PgPool) -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    (Db::new(pool), dir)
}

/// Store `files` as `key`'s durable pack tarball — what the reconcile readers materialize.
async fn seed_stored_pack(db: &Db, key: &str, files: &[(&str, &str)]) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("pack file");
    }
    if !files.iter().any(|(name, _)| *name == "crucible.toml") {
        std::fs::write(dir.path().join("crucible.toml"), "[repo]\nurl = \"x\"\n")
            .expect("pack manifest");
    }
    crate::playbooks::packs::store_pack_tree(db.pool(), key, dir.path())
        .await
        .expect("store pack");
}

/// Like [`db_with`], but with a metrics registry attached so a test can assert a choke point
/// actually moved a family.
fn db_with_metrics(pool: PgPool) -> (Db, crate::metrics::Metrics, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let metrics = crate::metrics::Metrics::new().expect("metrics");
    let db = Db::new(pool).with_metrics(metrics.clone());
    (db, metrics, dir)
}

/// Applying a plain text-tier verdict moves `crucible_rank_verdicts_total` with the verdict's
/// tier/disposition/confidence/grounded labels — the reconcile choke point, driven directly.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn apply_verdict_moves_the_rank_verdict_counter(pool: PgPool) -> Result<()> {
    let (db, metrics, dir) = db_with_metrics(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: "o/r#1".into(),
            repo: "o/r".into(),
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
    let issue = crate::issues::store::get_issue(db.pool(), "o/r#1")
        .await?
        .expect("issue");

    // A high-confidence T0 verdict: `grounded_wanted(High)` is false, so no grounding is
    // reached, and the verdict finalizes through `apply_rank_result`.
    let av = crate::issues::ranker::Verdict {
        tier: Tier::T0,
        affinity: crate::issues::ranker::Affinity::Perf,
        rationale: "clear repro + failing test".into(),
        cost_usd: Some(0.02),
        confidence: crate::issues::ranker::Confidence::High,
    };
    let gate = apply_verdict(&db, &cfg, &issue, "hash-1", av).await?;
    assert!(matches!(gate, TierGate::Proceed));

    let text = metrics.gather(crate::api::metrics::gauge_state(&db, Some(50.0)).await?)?;
    assert!(
        text.contains(
            r#"crucible_rank_verdicts_total{confidence="high",disposition="tier",grounded="false",tier="T0"} 1"#
        ),
        "verdict counter did not move as expected:\n{text}"
    );
    // The text-rank cost also re-exported as spend.
    assert!(
        text.contains(r#"crucible_spend_usd_total{cost_tag="rank"}"#),
        "{text}"
    );
    Ok(())
}

/// Completing a run moves `crucible_runs_total{outcome,repo}` and observes the run's parsed
/// duration/iterations — the ingest choke point in `complete_run`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn complete_run_moves_the_run_counter(pool: PgPool) -> Result<()> {
    let (db, metrics, _dir) = db_with_metrics(pool);
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: "o/r#7".into(),
            repo: "o/r".into(),
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
    assert!(
        crate::issues::store::claim_issue(db.pool(), "o/r#7", Status::New, Status::Scoped).await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &crate::issues::model::NewScope {
            issue: "o/r#7".into(),
            pack_digest: Some("v1:beef".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    crate::issues::store::claim_issue(db.pool(), "o/r#7", Status::Scoped, Status::Running).await?;

    let log = [
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":180.0}}"#,
        r#"{"v":1,"kind":"budget","spent":1.25,"elapsed_secs":640}"#,
        r#"{"v":1,"kind":"summary","best_score":180.0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished"}"#,
    ]
    .join("\n");

    complete_run(
        &db,
        "o/r#7",
        Some(scope_id),
        "run-7",
        Some("pod-7"),
        &log,
        None,
    )
    .await?;

    let text = metrics.gather(crate::api::metrics::gauge_state(&db, Some(50.0)).await?)?;
    assert!(
        text.contains(r#"crucible_runs_total{outcome="finished",repo="o/r"} 1"#),
        "run counter did not move:\n{text}"
    );
    assert!(
        text.contains(r#"crucible_run_duration_seconds_count 1"#),
        "run duration not observed:\n{text}"
    );
    Ok(())
}

/// A tick with nothing to do creates NO span, while a tick that ingests still traces. With OTLP
/// export on, the old span-per-tick shipped an identical 0-second `reconcile` ROOT trace for every
/// enqueue of every non-terminal row — an idle daemon buried Tempo's search in them. The spans
/// belong on the actions, so a trace means the controller actually moved something.
///
/// Runs alone: callsite interest is cached process-wide, and a sibling test reaching
/// `complete_run` with no subscriber attached caches it as `never` under this test's feet.
/// CI and `just test-controller` invoke it in its own process.
#[ignore = "process-global tracing interest cache; run alone with --ignored"]
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_idle_tick_creates_no_span_but_an_ingest_still_traces(pool: PgPool) -> Result<()> {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Clone)]
    struct SpanBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SpanBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = SpanBuf(buf.clone());
    // NEW + CLOSE: one line when a span opens and one when it closes, the same set of spans the
    // OTLP layer would export. The open line is written synchronously on first poll, so the
    // assertions below never race a close that lands after the await returns. Attached to the
    // future (not the thread) so it holds across the test's awaits.
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE),
        ),
    );
    let spans = || String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
    // With a single registered dispatcher, tracing-core computes a callsite's interest from the
    // hitting thread's default subscriber, so a sibling test reaching complete_run first would
    // cache `never`. A second live dispatcher makes every rebuild walk all registrars.
    let _second_registrar = tracing::Dispatch::new(tracing_subscriber::registry());

    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());

    // Idle tick 1: an untracked key — a stale queue hint, the shape a re-enqueue drives constantly.
    reconcile(&db, &cfg, "o/r#404")
        .with_subscriber(dispatch.clone())
        .await?;

    // Idle tick 2: a terminal row. `done` is record-only, so the pass does nothing.
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: "o/r#8".into(),
            repo: "o/r".into(),
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
    assert!(
        crate::issues::store::claim_issue(db.pool(), "o/r#8", Status::New, Status::Done).await?
    );
    reconcile(&db, &cfg, "o/r#8")
        .with_subscriber(dispatch.clone())
        .await?;

    assert!(
        !spans().contains("reconcile{"),
        "an idle tick must not create a span (it becomes a root trace per tick), got: {}",
        spans()
    );

    // The work still traces: a real completion ingests through `complete_run`, which carries the span.
    // Sibling tests call `complete_run` with no subscriber attached, and whichever reaches the
    // callsite first caches its interest process-wide as `never` — after which no scoped
    // dispatcher can revive it. Rebuild with this test's interested dispatch installed as the
    // default, so the callsite caches as enabled instead.
    tracing::dispatcher::with_default(&dispatch, tracing::callsite::rebuild_interest_cache);
    crate::issues::store::upsert_issue(
        db.pool(),
        &NewIssue {
            key: "o/r#7".into(),
            repo: "o/r".into(),
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
    assert!(
        crate::issues::store::claim_issue(db.pool(), "o/r#7", Status::New, Status::Scoped).await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "o/r#7".into(),
            pack_digest: Some("v1:beef".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "o/r#7", Status::Scoped, Status::Running)
            .await?
    );
    let log = [
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"summary","best_score":180.0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished"}"#,
    ]
    .join("\n");

    complete_run(
        &db,
        "o/r#7",
        Some(scope_id),
        "run-7",
        Some("pod-7"),
        &log,
        None,
    )
    .with_subscriber(dispatch.clone())
    .await?;

    let out = spans();
    assert!(
        out.contains("complete_run{"),
        "the ingest must still trace, got: {out}"
    );
    assert!(
        out.contains("run_id=run-7"),
        "the ingest's span must carry the run it folded in, got: {out}"
    );
    Ok(())
}

/// A `ControllerCfg` rooted at a temp state dir, with the given caps (defaults otherwise).
/// Every existing test in this module exercises `reconcile_new` without setting up a
/// grounded-verdict-capable stand-in bin, so the default here turns the pre-scope gate off;
/// the tests that exercise it explicitly opt back in (`prescope_grounded: true, ..cfg_with(..)`).
fn cfg_with(state_dir: &Path, profile: Profile) -> ControllerCfg {
    ControllerCfg {
        // The build tests push to `ghcr.io/org/…`; allowlist that org so the build whitelist
        // admits them (its deny path is unit-tested in `build.rs`).
        allowed_orgs: vec!["org".to_string()],
        profile,
        ..crate::testing::cfg_with(state_dir)
    }
}

/// A stand-in `crucible` binary: on `scope`, print a `ScopeReport` JSON to stdout — a surviving
/// pack (a digest, all stages passed) or a dead one (a failing validate stage, no digest). The
/// same command-backend seam the engine's real proposer uses one level up.
fn fake_crucible(dir: &Path, survive: bool) -> PathBuf {
    let json = if survive {
        r#"{"stages":[{"name":"ingest","passed":true,"detail":"goal"},{"name":"propose","passed":true,"detail":"drafted (turn cost $0.4200)"},{"name":"validate","passed":true,"detail":"crucible check: OK"},{"name":"freeze","passed":true,"detail":"wrote SCOPE.md"}],"digest":"v1:deadbeefcafef00d","cost":0.42}"#
    } else {
        r#"{"stages":[{"name":"ingest","passed":true,"detail":"goal"},{"name":"propose","passed":true,"detail":"drafted (turn cost $0.4200)"},{"name":"validate","passed":false,"detail":"crucible check: 1 finding(s): measure_cmd not executable"}],"digest":null,"cost":0.42}"#
    };
    let path = dir.join(if survive {
        "crucible-ok"
    } else {
        "crucible-bad"
    });
    // `printf %s` emits the JSON verbatim on stdout; a non-surviving run also exits 1 like the
    // real CLI, which the engine tolerates (it parses stdout regardless of exit code). The
    // real CLI also honors `--transcript-out` (gzipped session NDJSON) — mimic that, so the
    // local executor's transcript pickup is exercised end to end.
    let exit = if survive { "exit 0" } else { "exit 1" };
    crate::testing::write_exec(
        &path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = scope ]; then\n \
             prev=''\n out=''\n pack=''\n \
             for a in \"$@\"; do\n  if [ \"$prev\" = '--transcript-out' ]; then out=\"$a\"; fi\n  if [ \"$prev\" = '--out' ]; then pack=\"$a\"; fi\n  prev=\"$a\"\n done\n \
             if [ -n \"$out\" ]; then printf '%s\\n' '{{\"kind\":\"note\",\"msg\":\"round 1: propose turn\"}}' | gzip -c > \"$out\"; fi\n \
             if [ -n \"$pack\" ]; then mkdir -p \"$pack\"; printf '%s' '{manifest}' > \"$pack/crucible.toml\"; printf 'identity: v1:deadbeefcafef00d\\n' > \"$pack/SCOPE.md\"; fi\n \
             printf '%s' '{json}'\n {exit}\nfi\nexit 0\n",
            manifest = crate::testing::fixtures::LOOP_PACK_MANIFEST,
        ),
    );
    path
}

/// Flip an already-upserted issue row's `input_kind` to `scenario` — `upsert_issue` has no
/// `InputKind` param (Phase 1 keeps GitHub ingest untouched), so tests stamp it directly.
async fn mark_scenario(pool: &sqlx::PgPool, key: &str) -> Result<()> {
    sqlx::query("UPDATE issues SET input_kind = 'scenario' WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Insert a `scenarios` sidecar row (plus its single-entry `scenario_repos` hint) — no adopt
/// endpoint exists yet (Phase 4), so tests stamp it directly, the same way [`mark_scenario`]
/// stamps `input_kind`.
async fn insert_scenario_body(pool: &sqlx::PgPool, key: &str, body: &str) -> Result<()> {
    sqlx::query("INSERT INTO scenarios (key, title, body, created_by) VALUES ($1, $2, $3, $4)")
        .bind(key)
        .bind("a scenario")
        .bind(body)
        .bind("test")
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO scenario_repos (key, repo, position) VALUES ($1, $2, 0)")
        .bind(key)
        .bind("owner/repo")
        .execute(pool)
        .await?;
    Ok(())
}

/// Like [`fake_crucible`], but also dumps every `scope` invocation's argv (one arg per line) to
/// `argv_path` — for tests asserting exactly which flags the controller passed.
fn fake_crucible_capturing_argv(dir: &Path, argv_path: &Path) -> PathBuf {
    let json = r#"{"stages":[{"name":"ingest","passed":true,"detail":"goal"},{"name":"propose","passed":true,"detail":"drafted (turn cost $0.4200)"},{"name":"validate","passed":true,"detail":"crucible check: OK"},{"name":"freeze","passed":true,"detail":"wrote SCOPE.md"}],"digest":"v1:deadbeefcafef00d","cost":0.42}"#;
    let path = dir.join("crucible-argv-capture");
    crate::testing::write_exec(
        &path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = scope ]; then\n \
             : > {argv_path:?}\n \
             for a in \"$@\"; do printf '%s\\n' \"$a\" >> {argv_path:?}; done\n \
             prev=''\n pack=''\n \
             for a in \"$@\"; do\n  if [ \"$prev\" = '--out' ]; then pack=\"$a\"; fi\n  prev=\"$a\"\n done\n \
             if [ -n \"$pack\" ]; then mkdir -p \"$pack\"; printf '%s' '{manifest}' > \"$pack/crucible.toml\"; fi\n \
             printf '%s' '{json}'\n exit 0\nfi\nexit 0\n",
            manifest = crate::testing::fixtures::LOOP_PACK_MANIFEST,
        ),
    );
    path
}

fn sample_issue(key: &str) -> NewIssue {
    NewIssue {
        key: key.to_string(),
        repo: "owner/repo".to_string(),
        priority: 0,
        evidence_url: Some(format!(
            "https://github.com/{}",
            key.replace('#', "/issues/")
        )),
        title: None,
        author: None,
        body: None,
        labels: Vec::new(),
        upstream_updated_at: None,
    }
}

// --- fixtures: a fake GitHub (the single-issue GET `confirm_tier` makes) and a fake
// Vertex ranking endpoint (`CONTROLLER_RANKER_API_URL`, the real test seam — a `wiremock` HTTP
// double, not an in-process mock). Every `reconcile_new` test now runs through stage 2 first,
// so both are needed wherever a test drives the `new` path. One server serves both routes
// (different HTTP methods, no conflict), so one env-var pair covers a whole test.

/// Mount a single-issue GET response (`GET /repos/{repo}/issues/{number}`) on `server` — the
/// one GET `confirm_tier` makes to hash+rank an issue's current content.
async fn mount_issue(
    server: &wiremock::MockServer,
    repo: &str,
    number: u64,
    title: &str,
    body: &str,
    labels: &[&str],
) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let labels_json: Vec<serde_json::Value> = labels
        .iter()
        .map(|l| serde_json::json!({"name": l}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo}/issues/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": number,
            "title": title,
            "body": body,
            "labels": labels_json,
            "html_url": format!("https://github.com/{repo}/issues/{number}"),
            "updated_at": "2026-07-01T00:00:00Z",
            "state": "open",
        })))
        .mount(server)
        .await;
}

/// Mount an OpenAI-chat-completions-shaped ranking response on `server`: any POST gets back
/// this verdict text as `choices[0].message.content` (there's only ever one ranking call in
/// flight per test, so no path/body matching is needed).
async fn mount_ranker_verdict(server: &wiremock::MockServer, verdict_json: &str) {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": verdict_json}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        })))
        .mount(server)
        .await;
}

/// A ranking response that confirms/overrides to `tier` with a zero self-reported cost (so it
/// never perturbs a test's ledger-total assertions), a fixed rationale, and `perf` affinity (the
/// pass-through value; the off-affinity park has its own double below).
async fn mount_ranker_confirms(server: &wiremock::MockServer, tier: &str) {
    mount_ranker_verdict(
        server,
        &format!(
            r#"{{"tier":"{tier}","affinity":"perf","rationale":"confirmed by test double","cost_usd":0.0}}"#
        ),
    )
    .await;
}

/// A ranking response whose affinity is `unrelated`: whatever the tier says, the verdict must
/// park the issue off-rubric before any grounding or scope spend.
async fn mount_ranker_unrelated(server: &wiremock::MockServer, tier: &str) {
    mount_ranker_verdict(
        server,
        &format!(
            r#"{{"tier":"{tier}","affinity":"unrelated","rationale":"docs chore, no perf angle","cost_usd":0.0}}"#
        ),
    )
    .await;
}

/// A ranking response that never parses — the malformed-output path.
async fn mount_ranker_malformed(server: &wiremock::MockServer) {
    mount_ranker_verdict(server, "not json").await;
}

/// How many ranking calls `server` has received so far (the cache-hit test's proof, counting
/// real HTTP requests instead of inferring from a counter file or ledger cost).
async fn rank_calls(server: &wiremock::MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("request recording is on by default")
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn new_with_a_surviving_pack_goes_scoped_with_ledger_and_event(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 1, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#1")).await?;
    reconcile(&db, &cfg, "owner/repo#1").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Scoped);

    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), "owner/repo#1")
        .await?
        .expect("scope row");
    assert_eq!(scope.pack_digest.as_deref(), Some("v1:deadbeefcafef00d"));
    assert_eq!(scope.check_outcome.as_deref(), Some("PASS"));

    let report = crate::issues::store::latest_scope_report(db.pool(), "owner/repo#1")
        .await?
        .expect("structured report stored");
    assert!(report.survived);
    assert!(report.report_json.contains("v1:deadbeefcafef00d"));

    // The turn's preserved transcript rides with the report, keyed to its row, gunzippable
    // back to the session NDJSON the turn streamed.
    let transcript = crate::issues::store::latest_scope_transcript(db.pool(), "owner/repo#1")
        .await?
        .expect("transcript stored");
    assert_eq!(transcript.scope_report_id, report.id);
    let mut ndjson = String::new();
    std::io::Read::read_to_string(
        &mut flate2::read::GzDecoder::new(transcript.transcript_gz.as_slice()),
        &mut ndjson,
    )?;
    assert!(ndjson.contains("round 1: propose turn"));

    let today = crate::clock::today_utc();
    assert_eq!(
        crate::issues::store::count_scopes_on_day(db.pool(), &today).await?,
        1
    );
    // The confirming rank call self-reports a zero cost, so the ledger total is unchanged by
    // stage 2 landing — only the scope turn's own $0.42 shows up.
    assert!((crate::ledger::ledger_day_total(db.pool(), &today).await? - 0.42).abs() < 1e-9);

    let events = crate::event_log::export_string(db.pool()).await?;
    let lines: Vec<&str> = events.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "one rank-confirmation line, then one transition line: {lines:?}"
    );
    let rank: serde_json::Value = serde_json::from_str(lines[0])?;
    assert_eq!(rank["from"], "new");
    assert_eq!(rank["to"], "new");
    let transition: serde_json::Value = serde_json::from_str(lines[1])?;
    assert_eq!(transition["from"], "new");
    assert_eq!(transition["to"], "scoped");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn new_with_a_dead_proposal_parks_machine_with_the_reason(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), false);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 2, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#2")).await?;
    reconcile(&db, &cfg, "owner/repo#2").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#2")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert!(iss.parked_reason.unwrap().contains("measure_cmd"));
    // A dead scope still consumes a scope-turn (its cost ledgered), so the cap counts it.
    assert_eq!(
        crate::issues::store::count_scopes_on_day(db.pool(), &crate::clock::today_utc()).await?,
        1
    );
    assert!(
        crate::issues::store::latest_scope_for_issue(db.pool(), "owner/repo#2")
            .await?
            .is_none(),
        "no pack survived"
    );
    // The structured report survives the park — the ScopeProgress UI renders from it.
    let report = crate::issues::store::latest_scope_report(db.pool(), "owner/repo#2")
        .await?
        .expect("structured report stored");
    assert!(!report.survived);
    assert!(report.pod_name.is_none(), "local executor, no pod");
    assert!(report.report_json.contains("measure_cmd"));
    // A failed turn keeps its transcript too — that's the one a human most wants to read.
    assert!(
        crate::issues::store::latest_scope_transcript(db.pool(), "owner/repo#2")
            .await?
            .is_some(),
        "transcript persists on failure"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn daily_ceiling_declines_a_new_scope_and_records_capped(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    // Seed the day over the ceiling; no CRUCIBLE_BIN needed — reconcile must decline before it
    // would ever spawn scope.
    db.ledger_append(None, "run", 100.0).await?;
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            ..Profile::default()
        },
    );

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#3")).await?;
    reconcile(&db, &cfg, "owner/repo#3").await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#3")
            .await?
            .unwrap()
            .status,
        Status::New
    );
    let capped = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='capped'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(capped.n, 1, "crossing the ceiling records one capped event");
    assert_eq!(
        crate::issues::store::count_scopes_on_day(db.pool(), &crate::clock::today_utc()).await?,
        0,
        "no scope ran"
    );
    Ok(())
}

/// A non-upstream (scenario) `new` row with a NULL `upstream_updated_at` and every
/// autopilot gate already blown (rank horizon, daily ceiling, scopes/day) still reaches the
/// scope turn — the human adoption is the authorization, not the ranker or the caps.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scenario_kind_bypasses_rank_horizon_tier_and_caps(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    // No wiremock server is mounted and GITHUB_API_URL/CONTROLLER_RANKER_API_URL are unset: a
    // stray ranker or GitHub call would hard-fail this test, proving neither ever fires.
    db.ledger_append(None, "run", 200.0).await?; // over the daily ceiling
    db.ledger_append(None, "scope", 0.5).await?; // over max_scopes_per_day (cap below is 1)
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            max_scopes_per_day: 1,
            ..Profile::default()
        },
    );
    let cfg = ControllerCfg {
        rank_horizon_days: 7,
        ..cfg
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:bypass-1")).await?;
    mark_scenario(db.pool(), "scenario:bypass-1").await?;
    reconcile(&db, &cfg, "scenario:bypass-1").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "scenario:bypass-1")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "reached run_scope_and_transition and survived, despite blown caps + NULL upstream + rank_horizon_days>0"
    );
    assert_eq!(
        iss.parked_by, None,
        "never parked — no StaleRankHorizon, no tier park"
    );
    let capped = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='capped'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(capped.n, 0, "the caps were bypassed, not merely passed");
    Ok(())
}

/// Phase 4, end to end: a scenario minted through the real `POST /api/scenarios` transaction
/// (`operations::adopt_scenario`, not the manual `mark_scenario`/`insert_scenario_body` test
/// stamps the other scenario tests use) still reaches the scope turn with the daily ceiling and
/// scopes/day cap already blown — R2's caps-bypass claim, proven against the actual adopt path
/// rather than a synthetic row.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopted_scenario_bypasses_caps_end_to_end(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    db.ledger_append(None, "run", 200.0).await?; // over the daily ceiling
    db.ledger_append(None, "scope", 0.5).await?; // over max_scopes_per_day (cap below is 1)
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            max_scopes_per_day: 1,
            ..Profile::default()
        },
    );
    let cfg = ControllerCfg {
        rank_horizon_days: 7,
        ..cfg
    };

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins::default(),
        "admin",
    )
    .await?;
    reconcile(&db, &cfg, &key).await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "reached run_scope_and_transition and survived, despite blown caps + NULL upstream + rank_horizon_days>0"
    );
    assert_eq!(iss.parked_by, None, "never parked");
    let capped = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='capped'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(
        capped.n, 0,
        "the caps were bypassed for a real adopt-endpoint row, not just a synthetic one"
    );
    Ok(())
}

/// The bypass is idempotent: a scenario re-driven mid-flight (a non-blocking pod dispatch that
/// hasn't gone terminal yet) takes the same kind-keyed bypass again rather than falling through
/// into the rank-horizon gate — because the branch keys on `kind.has_upstream()`, not on a
/// one-shot stash flag that a first pass might have cleared.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scenario_bypass_is_idempotent_across_pod_redrives(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(StillRunningDispatcher));
    let mut cfg = pod_scope_cfg(dir.path());
    cfg.rank_horizon_days = 7;

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:idem-1")).await?;
    mark_scenario(db.pool(), "scenario:idem-1").await?;

    // First drive: the bypass reaches dispatch_scope, which creates the turn pod and returns
    // non-blocking (`Launched`) — the row stays `new`.
    reconcile(&db, &cfg, "scenario:idem-1").await?;
    let after_first = crate::issues::store::get_issue(db.pool(), "scenario:idem-1")
        .await?
        .expect("issue");
    assert_eq!(
        after_first.status,
        Status::New,
        "launched, not yet collected"
    );
    assert_eq!(
        after_first.parked_by, None,
        "not parked by the rank-horizon gate"
    );

    // Second drive: the pod is still running (never terminal), so the adopt-first pre-pass
    // leaves it alone and reconcile_new runs again — the bypass must fire a second time, not
    // fall through into the rank-horizon gate now that the row is being re-driven.
    reconcile(&db, &cfg, "scenario:idem-1").await?;
    crate::runs::workpod::reset_dispatcher();

    let after_second = crate::issues::store::get_issue(db.pool(), "scenario:idem-1")
        .await?
        .expect("issue");
    assert_eq!(
        after_second.status,
        Status::New,
        "still new — the pod never went terminal"
    );
    assert_eq!(
        after_second.parked_by, None,
        "the re-drive took the bypass again, not the rank-horizon park"
    );
    Ok(())
}

/// A scenario row's scope turn is framed by its ledgered free text, not a GitHub fetch: the
/// argv the controller passes `crucible scope --propose` carries `--goal-file`, never `--issue`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_propose_uses_goal_file_not_issue_for_a_scenario(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let argv_path = dir.path().join("argv.log");
    let bin = fake_crucible_capturing_argv(dir.path(), &argv_path);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:goal-1")).await?;
    mark_scenario(db.pool(), "scenario:goal-1").await?;
    insert_scenario_body(db.pool(), "scenario:goal-1", "fix the frobnicator").await?;

    let cfg = cfg_with(dir.path(), Profile::default());
    reconcile(&db, &cfg, "scenario:goal-1").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let argv = std::fs::read_to_string(&argv_path).expect("argv captured");
    assert!(
        argv.lines().any(|l| l == "--goal-file"),
        "scenario scope turn must pass --goal-file: {argv}"
    );
    assert!(
        !argv.lines().any(|l| l == "--issue"),
        "scenario scope turn must not pass --issue: {argv}"
    );

    let iss = crate::issues::store::get_issue(db.pool(), "scenario:goal-1")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped);
    Ok(())
}

/// The scope freeze extracts the exposure with the same publish target a loop run of the pack is
/// rendered with, so the disclosure an approver signs names the fork the run would open against
/// rather than an incomplete engine default.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn the_frozen_exposure_carries_the_publish_target_a_run_would_use(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:fork-1")).await?;
    mark_scenario(db.pool(), "scenario:fork-1").await?;
    insert_scenario_body(db.pool(), "scenario:fork-1", "fix the frobnicator").await?;

    let mut cfg = cfg_with(dir.path(), Profile::default());
    cfg.pr_repo_map = vec!["owner/repo=wren/repo-fork".to_string()];
    reconcile(&db, &cfg, "scenario:fork-1").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), "scenario:fork-1")
        .await?
        .expect("the survived scope was stored");
    let lines = scope
        .exposure
        .as_ref()
        .expect("the frozen exposure was stored")
        .output_lines();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("draft-pr") && l.ends_with("-> wren/repo-fork")),
        "the draft-pr bound names the fork a run would open against: {lines:?}"
    );
    Ok(())
}

/// A kind with no PR backlink (a scenario) never calls `open_pack_pr`: `reconcile_scoped` skips
/// straight to `awaiting-approval` with no `approval_pr`, for the Phase 4 UI-approve endpoint.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn reconcile_scoped_never_opens_a_pack_pr_for_a_scenario(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    // No CRUCIBLE_BIN, no CONTROLLER_PACK_REPO: a stray engine call, or an `open_pack_pr` call
    // (which needs the pack repo to do anything real), would hard-fail this test, proving the PR
    // path is never reached.
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
        std::env::remove_var("CONTROLLER_PACK_REPO");
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:approval-1")).await?;
    mark_scenario(db.pool(), "scenario:approval-1").await?;
    crate::issues::store::claim_issue(
        db.pool(),
        "scenario:approval-1",
        Status::New,
        Status::Scoped,
    )
    .await?;

    let cfg = cfg_with(dir.path(), Profile::default());
    reconcile(&db, &cfg, "scenario:approval-1").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "scenario:approval-1")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::AwaitingApproval,
        "skips the PR approval straight to awaiting-approval"
    );
    Ok(())
}

/// Phase 6 e2e: adopt → scope (`--goal-file`, no ranker) → awaiting-approval (no PR) → approve
/// (the same `operations::record_approval` the API handler calls) → the run launches — one
/// scenario row driven through every reconcile stage with no synthetic status jumps. Proves the
/// full adopt-to-run wire, not just each stage in isolation.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scenario_adopt_to_run_end_to_end(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let argv_path = dir.path().join("argv.log");
    let scope_bin = fake_crucible_capturing_argv(dir.path(), &argv_path);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &scope_bin);
        std::env::remove_var("CONTROLLER_PACK_REPO");
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    // Adopt: the real transaction the API handler drives (issues + scenarios + ledger).
    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/target-repo".to_string()],
        false,
        crate::issues::store::AdoptPins::default(),
        "admin",
    )
    .await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), &key)
            .await?
            .unwrap()
            .status,
        Status::New
    );

    // New -> Scoped: the R1 bypass runs the scope turn straight away (no ranker call was mounted,
    // so a fetch would have hard-failed this test).
    reconcile(&db, &cfg, &key).await?;
    let argv = std::fs::read_to_string(&argv_path).expect("argv captured");
    assert!(
        argv.lines().any(|l| l == "--goal-file"),
        "the scope turn is framed by the ledgered goal text: {argv}"
    );
    assert!(
        !argv.lines().any(|l| l == "--issue"),
        "a scenario has no upstream issue to fetch: {argv}"
    );
    let iss = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Scoped);

    // Scoped -> AwaitingApproval: no PR backlink, so no draft PR — `open_pack_pr` would need
    // CONTROLLER_PACK_REPO (unset above) to do anything real, proving the approval is skipped.
    reconcile(&db, &cfg, &key).await?;
    let iss = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::AwaitingApproval);
    let scope = crate::issues::store::latest_scope_for_issue(db.pool(), &key)
        .await?
        .expect("scope row");
    assert!(scope.approval_pr.is_none(), "no draft PR ever opened");

    // Approve: the same operation `POST /api/scenarios/{key}/approve` calls.
    let approved = crate::issues::store::record_approval(
        db.pool(),
        scope.id,
        "admin",
        &crate::clock::now_rfc3339(),
    )
    .await?;
    assert!(approved, "first approval stamp wins");

    // AwaitingApproval -> Running: the deploy-render + pod-create leg, driven by the same
    // dispatch_run every kind uses — the run itself (not the controller) opens the code PR
    // against the target repo, from the pack's own `[publish] pr_repo` baked in at freeze time.
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg
    };
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(StillRunningDispatcher));
    let res = reconcile(&db, &cfg, &key).await;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    res?;

    let iss = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Running, "the approved run launched");
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scopes_per_day_cap_declines_a_new_scope(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 4, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    // Two scope turns already ledgered today; the cap is 2.
    db.ledger_append(None, "scope", 0.5).await?;
    db.ledger_append(None, "scope", 0.5).await?;
    let cfg = cfg_with(
        dir.path(),
        Profile {
            max_scopes_per_day: 2,
            ..Profile::default()
        },
    );

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#4")).await?;
    reconcile(&db, &cfg, "owner/repo#4").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#4")
            .await?
            .unwrap()
            .status,
        Status::New
    );
    assert_eq!(
        crate::issues::store::count_scopes_on_day(db.pool(), &crate::clock::today_utc()).await?,
        2,
        "still 2 — the new one declined"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn concurrent_pod_cap_declines_an_approved_launch(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    // Two issues already running; the cap is 2. An approved third must not launch.
    for k in ["owner/repo#10", "owner/repo#11"] {
        crate::issues::store::upsert_issue(db.pool(), &sample_issue(k)).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), k, Status::New, Status::Running).await?
        );
    }
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#12")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#12",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    // A scope with a recorded approval (what the approval watch stamps).
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#12".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;

    let cfg = cfg_with(
        dir.path(),
        Profile {
            max_concurrent_pods: 2,
            ..Profile::default()
        },
    );
    reconcile(&db, &cfg, "owner/repo#12").await?;

    // Declined: still awaiting, one capped event, no run row.
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#12")
            .await?
            .unwrap()
            .status,
        Status::AwaitingApproval
    );
    let capped = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='capped'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(capped.n, 1);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn dispatch_failure_surfaces_the_error_chain_on_the_event_log(pool: PgPool) -> Result<()> {
    // An approved run whose dispatch dies in render (here: no deploy profile configured) must
    // leave the real cause on the issue's event log before the error propagates — the live
    // regression was a bad pack manifest whose TOML error never reached logs or events.
    let (db, dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#77")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#77",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#77".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;

    // Default profile: cap not hit, budget fine — but no deploy profile, so dispatch_run errors.
    let cfg = cfg_with(dir.path(), Profile::default());
    assert!(cfg.deploy_profile.is_none(), "the failure precondition");
    let res = reconcile(&db, &cfg, "owner/repo#77").await;
    assert!(res.is_err(), "the dispatch error propagates");

    let events = db.events().read_all().await?;
    let dispatch_evt = events
        .iter()
        .find(|e| e.reason.as_deref() == Some("loop-run dispatch failed"))
        .expect("a dispatch-failure event was recorded");
    assert_eq!(dispatch_evt.key, "owner/repo#77");
    assert!(
        dispatch_evt
            .evidence
            .as_deref()
            .is_some_and(|ev| ev.contains("deploy profile")),
        "the underlying error chain is on the event: {:?}",
        dispatch_evt.evidence
    );
    // The row stays awaiting-approval for the retry.
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#77")
            .await?
            .unwrap()
            .status,
        Status::AwaitingApproval
    );
    Ok(())
}

/// End to end: an admin redispatch of a finished (`done`) issue whose approval is still approved
/// mints a FRESH run id, re-launches the SAME stored pack through `dispatch_run`, and clears the
/// one-shot stash — all while autopilot is PAUSED (the human is the authorization). The prior run
/// row is untouched immutable history. This is the relay-testbed re-run driver's happy path.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn redispatch_relaunches_the_stored_pack_with_a_fresh_run_id(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());

    let flag = crate::daemon::autopilot_flag::AutopilotFlag::load(db.pool()).await?;
    flag.set(false, Some("test"), "proving-phase pause").await?;
    let cfg = ControllerCfg {
        autopilot: Some(flag),
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };

    // A finished issue with an approved approval and a PRIOR run row (immutable history).
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#20")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#20", Status::New, Status::Done)
            .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#20".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: "owner_repo_20-1".into(),
            scope: Some(scope_id),
            issue: None,
            identity_digest: Some("v1:abc".into()),
            status: "finished".into(),
            pod: Some("crucible-run-owner-repo-20-1".into()),
            session_uri: None,
            best_score: Some(200.0),
            cost_usd: Some(1.0),
        },
    )
    .await?;

    // The approved pack is durably stored — this is what redispatch reuses; no new scope turn
    // ever regenerates it. A valid (build-free) frozen manifest: the redispatch path reads it via
    // `plan_builds` to decide whether the pack needs the `building` state before launch — with no
    // `[build]` block it launches directly, exactly as before.
    seed_stored_pack(
        &db,
        "owner/repo#20",
        &[(
            "crucible.toml",
            crate::testing::fixtures::LOOP_PACK_MANIFEST,
        )],
    )
    .await;

    // The post-apply state (apply_redispatch's job, covered in overrides.rs): the approval reopened +
    // the one-shot stash set. Autopilot is paused, so ONLY the stash's exemption lets this launch.
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#20",
            Status::Done,
            Status::AwaitingApproval
        )
        .await?
    );
    crate::runs::work_pods::set_redispatch(db.pool(), "owner/repo#20", "re-run the approved pack")
        .await?;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile(&db, &cfg, "owner/repo#20").await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#20")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Running, "the re-dispatched run is live");
    assert!(
        iss.redispatch_justification.is_none(),
        "the one-shot stash was cleared at launch"
    );
    assert!(
        crate::playbooks::packs::read_pack_file(db.pool(), "owner/repo#20", "crucible.toml")
            .await?
            .is_some(),
        "the stored pack survives the re-dispatch (reused, not consumed)"
    );

    // A NEW run row at `running` with a fresh id; the prior finished row is untouched history.
    let runs = sqlx::query!(
        r#"SELECT run_id AS "run_id!", status AS "status!" FROM runs ORDER BY run_id"#
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(
        runs.len(),
        2,
        "history preserved: a second run row, not a mutated one"
    );
    let fresh: Vec<_> = runs.iter().filter(|r| r.status == "running").collect();
    assert_eq!(fresh.len(), 1, "exactly one live run");
    assert_ne!(
        fresh[0].run_id, "owner_repo_20-1",
        "a fresh run id was minted"
    );
    assert!(
        runs.iter()
            .any(|r| r.run_id == "owner_repo_20-1" && r.status == "finished"),
        "the prior run row is immutable history"
    );
    Ok(())
}

/// The exemption is real: WITHOUT a redispatch stash, an approved issue at the approval stays paused
/// while autopilot is disabled — the pack alone never re-launches, only the human's re-run does.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approved_approval_stays_paused_without_a_redispatch(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let flag = crate::daemon::autopilot_flag::AutopilotFlag::load(db.pool()).await?;
    flag.set(false, Some("test"), "proving-phase pause").await?;
    let cfg = ControllerCfg {
        autopilot: Some(flag),
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#21")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#21",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#21".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;

    reconcile(&db, &cfg, "owner/repo#21").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#21")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::AwaitingApproval,
        "paused: no dispatch without the human exemption"
    );
    let runs = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM runs"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(runs.n, 0, "no run launched while paused");
    Ok(())
}

/// A fake build backend for the `building`-state reconcile tests: dispatch records a job name,
/// poll reports still-running, resolve returns a pinned digest. The cluster boundary's hermetic
/// double (the WorkPod tests' fake-dispatcher discipline), never an internal collaborator.
struct FakeBuildBackend;

#[async_trait::async_trait]
impl crate::builds::lifecycle::BuildBackend for FakeBuildBackend {
    async fn dispatch(
        &self,
        _ns: &str,
        req: &crate::builds::lifecycle::BuildRequest,
    ) -> Result<String> {
        Ok(format!("job-{}", req.name))
    }
    async fn poll(
        &self,
        _ns: &str,
        _req: &crate::builds::lifecycle::BuildRequest,
        _id: &str,
    ) -> Result<crate::builds::lifecycle::BuildProgress> {
        Ok(crate::builds::lifecycle::BuildProgress::Running)
    }
    async fn resolve_digest(&self, req: &crate::builds::lifecycle::BuildRequest) -> Result<String> {
        Ok(format!("{}@sha256:beef", req.image))
    }
    async fn adopt(&self, _ns: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Store a frozen pack tarball declaring one cluster `[build]` block (what
/// [`crate::builds::lifecycle::plan_builds`] reads off the materialized tree).
async fn write_build_pack(db: &Db, key: &str) {
    seed_stored_pack(
        db,
        key,
        &[(
            "crucible.toml",
            &format!(
                "{}\n[build.sandbox]\nbackend = \"cluster\"\nimage = \"ghcr.io/org/sandbox\"\ntimeout = \"30m\"\n[build.sandbox.cluster]\ncontainerfile = \"Containerfile\"\n",
                crate::testing::fixtures::LOOP_PACK_MANIFEST
            ),
        )],
    )
    .await;
}

async fn approve_scope(db: &Db, key: &str) -> Result<i64> {
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: key.into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;
    Ok(scope_id)
}

/// A malformed `[build]` table (a self-referencing `needs` — a `forge::spec::SpecError`) can never
/// plan. The reconcile driver parks the issue (machine) with the spec error as the reason rather than
/// returning `Err` and letting the queue retry a deterministic failure `park_after` times.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_malformed_build_table_parks_instead_of_erroring(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#55")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#55",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    approve_scope(&db, "owner/repo#55").await?;
    // `needs = ["sandbox"]` self-references → forge's `SpecError::SelfNeeds`, a permanent plan error.
    seed_stored_pack(
        &db,
        "owner/repo#55",
        &[(
            "crucible.toml",
            "[build.sandbox]\nbackend = \"cluster\"\nimage = \"ghcr.io/org/sandbox\"\ntimeout = \"30m\"\nneeds = [\"sandbox\"]\n[build.sandbox.cluster]\ncontainerfile = \"Containerfile\"\n",
        )],
    )
    .await;

    // Parks (machine) — never errors out to the queue's retry loop.
    reconcile(&db, &cfg, "owner/repo#55").await?;
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#55")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked, "parked, not left for the queue");
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    let reason = iss.parked_reason.unwrap_or_default();
    assert!(
        reason.starts_with("image build failed:"),
        "parks as ImageBuildFailed: {reason}"
    );
    assert!(
        reason.contains("needs references itself"),
        "carries the forge spec error verbatim: {reason}"
    );
    Ok(())
}

/// The block: an approved pack that declares a `[build]` block enters `building` (not
/// `running`) and dispatches its builds; the run is NEVER launched while a build is still in
/// flight (no `runs` row, no digest yet).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn approved_pack_with_a_build_blocks_the_run_at_building(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#50")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#50",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id = approve_scope(&db, "owner/repo#50").await?;
    write_build_pack(&db, "owner/repo#50").await;

    // First pass: the approved-but-build-needing pack flips to `building`, launches no run.
    reconcile(&db, &cfg, "owner/repo#50").await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#50")
            .await?
            .unwrap()
            .status,
        Status::Building
    );

    // Second pass (building): the builds dispatch, but the run is STILL blocked (poll = running).
    crate::builds::lifecycle::install_build_backend(std::sync::Arc::new(FakeBuildBackend));
    reconcile(&db, &cfg, "owner/repo#50").await?;
    reconcile(&db, &cfg, "owner/repo#50").await?;
    crate::builds::lifecycle::reset_build_backend();

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#50")
            .await?
            .unwrap()
            .status,
        Status::Building,
        "the run stays blocked while the build is in flight"
    );
    let builds = crate::builds::store::builds_for_scope(db.pool(), scope_id).await?;
    assert_eq!(builds.len(), 1);
    assert_eq!(
        builds[0].state,
        crate::builds::model::BuildState::Dispatched
    );
    assert!(
        builds[0].digest_ref.is_none(),
        "no digest → run not dispatchable"
    );
    let runs = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM runs"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(runs.n, 0, "no loop run launched while building");
    Ok(())
}

/// Unblock-on-success: a `building` issue whose builds all carry a pinned digest launches the
/// loop run (`building` → `running`) — the same launch path a build-free pack takes.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn building_launches_the_run_once_every_build_pins(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#51")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#51",
            Status::New,
            Status::Building
        )
        .await?
    );
    let scope_id = approve_scope(&db, "owner/repo#51").await?;
    write_build_pack(&db, "owner/repo#51").await;
    // A build row already succeeded with a pinned digest (the state the poll sees as ready).
    let id = crate::builds::store::insert_build(
        db.pool(),
        &crate::builds::model::NewBuild {
            scope: Some(scope_id),
            name: "sandbox".into(),
            image: "ghcr.io/org/sandbox".into(),
            tag: "ctx-x".into(),
            context_digest: "sha256:x".into(),
            backend: crate::builds::model::BuildBackendKind::Cluster,
            timeout_secs: 1800,
        },
    )
    .await?;
    crate::builds::store::set_build_dispatched(db.pool(), id, "job-sandbox").await?;
    crate::builds::store::set_build_succeeded(db.pool(), id, "ghcr.io/org/sandbox@sha256:beef")
        .await?;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile(&db, &cfg, "owner/repo#51").await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#51")
            .await?
            .unwrap()
            .status,
        Status::Running,
        "every build pinned → the run launches"
    );
    let runs = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM runs WHERE status='running'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(runs.n, 1, "exactly one loop run launched");
    Ok(())
}

/// Park-on-failure: a `building` issue with a failed build parks (machine) with the build-log
/// pointer carried in the park reason — the evidence the next person reads.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn building_parks_on_a_failed_build_with_the_log_pointer(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#52")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#52",
            Status::New,
            Status::Building
        )
        .await?
    );
    let scope_id = approve_scope(&db, "owner/repo#52").await?;
    write_build_pack(&db, "owner/repo#52").await;
    let id = crate::builds::store::insert_build(
        db.pool(),
        &crate::builds::model::NewBuild {
            scope: Some(scope_id),
            name: "sandbox".into(),
            image: "ghcr.io/org/sandbox".into(),
            tag: "ctx-x".into(),
            context_digest: "sha256:x".into(),
            backend: crate::builds::model::BuildBackendKind::Cluster,
            timeout_secs: 1800,
        },
    )
    .await?;
    crate::builds::store::set_build_dispatched(db.pool(), id, "job-sandbox").await?;
    crate::builds::store::set_build_failed(
        db.pool(),
        id,
        crate::builds::model::BuildState::Failed,
        Some("https://logs/build/52"),
    )
    .await?;

    reconcile(&db, &cfg, "owner/repo#52").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#52")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert!(
        iss.parked_reason.unwrap().contains("https://logs/build/52"),
        "the build-log pointer is the park evidence"
    );
    Ok(())
}

/// The deterministic wedge repro at the reconcile level: an approved pack that declares a
/// `[build]` with NO real backend installed must PARK with a clear "backend not installed"
/// reason on the first building pass — never wedge at `building` forever.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn building_with_no_backend_parks_not_wedges(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#53")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#53",
            Status::New,
            Status::Building
        )
        .await?
    );
    approve_scope(&db, "owner/repo#53").await?;
    write_build_pack(&db, "owner/repo#53").await;

    // No install_build_backend → the stub is active. One building pass must park, not wedge.
    crate::builds::lifecycle::reset_build_backend();
    reconcile(&db, &cfg, "owner/repo#53").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#53")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked, "the stub parks, never wedges");
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert!(
        iss.parked_reason.unwrap().contains("not installed"),
        "the park reason names the missing backend"
    );
    Ok(())
}

/// A scope whose fixed build demand exceeds `build_pod_cap` can never be admitted (the run
/// needs every image at once), so it parks with a clear reason instead of re-driving a
/// forever-capped dispatch.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn building_parks_when_demand_exceeds_the_cap(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = Profile {
        build_pod_cap: 1,
        ..Profile::default()
    };
    let cfg = cfg_with(dir.path(), profile);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#54")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#54",
            Status::New,
            Status::Building
        )
        .await?
    );
    approve_scope(&db, "owner/repo#54").await?;
    // Two declared cluster builds, cap of 1 → unsatisfiable.
    seed_stored_pack(
        &db,
        "owner/repo#54",
        &[(
            "crucible.toml",
            "[build.a]\nbackend = \"cluster\"\nimage = \"ghcr.io/org/a\"\n[build.a.cluster]\ncontainerfile = \"A\"\n\n[build.b]\nbackend = \"cluster\"\nimage = \"ghcr.io/org/b\"\n[build.b.cluster]\ncontainerfile = \"B\"\n",
        )],
    )
    .await;

    reconcile(&db, &cfg, "owner/repo#54").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#54")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    let reason = iss.parked_reason.unwrap();
    assert!(reason.contains("build_pod_cap=1"), "{reason}");
    assert!(
        crate::builds::store::builds_for_scope(
            db.pool(),
            crate::issues::store::latest_scope_for_issue(db.pool(), "owner/repo#54")
                .await?
                .unwrap()
                .id
        )
        .await?
        .is_empty(),
        "an unadmittable scope creates no build rows"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn awaiting_without_approval_is_a_noop_approval_wait(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#5")).await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#5",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#5".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;

    reconcile(&db, &cfg, "owner/repo#5").await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#5")
            .await?
            .unwrap()
            .status,
        Status::AwaitingApproval,
        "no approval recorded: the approval stays shut"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn losing_the_claim_is_a_clean_noop(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 6, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#6")).await?;
    // Two reconciles race the same `new` row; both run the (fake) rank + scope, one claim
    // wins each of the two independent CASes (the rank-application CAS, then the status CAS).
    let (a, b) = tokio::join!(
        reconcile(&db, &cfg, "owner/repo#6"),
        reconcile(&db, &cfg, "owner/repo#6"),
    );
    a?;
    b?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#6")
            .await?
            .unwrap()
            .status,
        Status::Scoped
    );
    // Exactly one scope row + one scope-cost ledger row survived the race.
    let scopes =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM scopes WHERE issue='owner/repo#6'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(scopes.n, 1, "the loser recorded nothing");
    let events = crate::event_log::export_string(db.pool()).await?;
    assert_eq!(
        events.lines().count(),
        2,
        "one rank-confirmation line, one transition line — each CAS has exactly one winner"
    );
    assert_eq!(
        crate::issues::store::count_scopes_on_day(db.pool(), &crate::clock::today_utc()).await?,
        1
    );
    Ok(())
}

/// An ERROR outcome parks (retryable, reason = the engine's own shutdown words) instead of
/// completing — `done` means solved-or-exhausted, never "died". The live failure: the first
/// loop run errored at baseline and the issue quietly finished.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn complete_run_parks_an_errored_outcome(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#8")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#8", Status::New, Status::Running)
            .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#8".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    let log = [
        r#"{"v":1,"kind":"phase","phase":"baseline","iter":0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"error","reason":"baseline measurement invalid: measure produced no JSON line"}"#,
    ]
    .join("\n");

    complete_run(
        &db,
        "owner/repo#8",
        Some(scope_id),
        "run-8",
        Some("loop-8"),
        &log,
        None,
    )
    .await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#8")
        .await?
        .unwrap();
    assert_eq!(
        iss.status,
        Status::Parked,
        "an errored run parks, never done"
    );
    assert!(
        iss.parked_reason
            .as_deref()
            .is_some_and(|r| r.contains("measure produced no JSON line")),
        "the engine's shutdown reason rides the park: {:?}",
        iss.parked_reason
    );
    Ok(())
}

/// The live failure: the park read `run errored: short-circuited at scan`, which names where the
/// run stopped and not why, and the pod carrying the real chain was deleted on collection. The
/// failing task's note has to ride the park too.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_errored_park_carries_the_failing_tasks_cause(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#12")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#12", Status::New, Status::Running)
            .await?
    );
    let log = [
        r#"{"v":1,"kind":"plan_admitted","plan_version":1,"tasks":[]}"#,
        r#"{"v":1,"kind":"task_result","task":"scan","status":"transport","plan_version":1,"task_kind":"agent","iter":0,"attempts":3,"cost_usd":0.0,"secs":1.0,"note":"transport retries exhausted (3 attempts): agent spawn failed: failed to launch agent source: No such file or directory (os error 2)"}"#,
        r#"{"v":1,"kind":"task_result","task":"judge","status":"blocked","plan_version":1,"task_kind":"agent","iter":0,"attempts":0,"cost_usd":0.0,"secs":0.0,"note":"waiting on scan"}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"error","reason":"short-circuited at scan"}"#,
    ]
    .join("\n");

    let disposition = complete_run(&db, "owner/repo#12", None, "run-12", None, &log, None).await?;

    assert_eq!(disposition, crate::runs::workpod::RunDisposition::Errored);
    let reason = crate::issues::store::get_issue(db.pool(), "owner/repo#12")
        .await?
        .expect("issue")
        .parked_reason
        .expect("parked");
    assert!(
        reason.contains("short-circuited at scan"),
        "the engine's own words stay: {reason}"
    );
    assert!(
        reason.contains(
            "transport retries exhausted (3 attempts): agent spawn failed: failed to launch \
             agent source: No such file or directory (os error 2)"
        ),
        "the failing task's whole note rides the park: {reason}"
    );
    assert!(
        !reason.contains("waiting on scan"),
        "a blocked task is fallout, not the cause: {reason}"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn complete_run_names_the_transport_losses_a_finished_run_carried(
    pool: PgPool,
) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#71")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#71", Status::New, Status::Running)
            .await?
    );
    let log = [
        r#"{"v":1,"kind":"task_result","task":"triage[a]","status":"transport","iter":0,"note":"gateway did not become healthy"}"#,
        r#"{"v":1,"kind":"task_result","task":"triage","status":"fail","iter":0,"note":"1 of 1 instances failed: a"}"#,
        r#"{"v":1,"kind":"task_result","task":"card","status":"pass","iter":0,"note":""}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"completed"}"#,
    ]
    .join("\n");
    complete_run(
        &db,
        "owner/repo#71",
        None,
        "run-71",
        Some("loop-71"),
        &log,
        None,
    )
    .await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#71")
            .await?
            .unwrap()
            .status,
        Status::Done,
        "an advisory loss does not park the run"
    );
    let ev = db.events().read_for_key("owner/repo#71").await?;
    let last = ev.last().expect("the completion transition");
    assert_eq!(last.to, "done");
    assert_eq!(
        last.reason.as_deref(),
        Some("run finished; outcome ingested; 1 task attempt died on transport")
    );
    assert_eq!(
        crate::runs::task_results::count_transport_losses(db.pool(), "run-71").await?,
        1
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn complete_run_ingests_and_advances_running_to_done(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#7")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#7", Status::New, Status::Running)
            .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#7".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;

    let log = [
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":230.0}}"#,
        r#"{"v":1,"kind":"budget","spent":1.1,"elapsed_secs":120}"#,
        r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":230.0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
    ]
    .join("\n");

    complete_run(
        &db,
        "owner/repo#7",
        Some(scope_id),
        "run-7",
        Some("loop-7"),
        &log,
        None,
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#7")
            .await?
            .unwrap()
            .status,
        Status::Done
    );
    let run = sqlx::query!(r#"SELECT best_score, cost_usd, status FROM runs WHERE run_id='run-7'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(run.best_score, Some(230.0));
    assert_eq!(run.status, "finished");
    let cands =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM candidates WHERE run_id='run-7'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(cands.n, 2);
    Ok(())
}

/// The P1 fix: a run whose session log carries a `pr_links` event lands `pr-open` (not `done`),
/// the kept candidate row carries the PR url, and the transition's event records it as evidence.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn complete_run_lands_pr_open_when_a_pr_opened(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#8")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#8", Status::New, Status::Running)
            .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#8".into(),
            pack_digest: None,
            check_outcome: None,
        },
    )
    .await?;
    let log = [
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":230.0}}"#,
        r#"{"v":1,"kind":"pr_links","links":[{"url":"https://github.com/owner/repo/pull/9","repo":"owner/repo","name":""}]}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
    ]
    .join("\n");

    complete_run(
        &db,
        "owner/repo#8",
        Some(scope_id),
        "run-8",
        None,
        &log,
        None,
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#8")
            .await?
            .unwrap()
            .status,
        Status::PrOpen
    );
    let kept = crate::runs::store::kept_candidate_prs(db.pool()).await?;
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].pr_url, "https://github.com/owner/repo/pull/9");
    // The transition event carries the PR as evidence.
    let ev = db.events().read_for_key("owner/repo#8").await?;
    let last = ev.last().expect("a transition event");
    assert_eq!(last.to, "pr-open");
    assert_eq!(
        last.evidence.as_deref(),
        Some("https://github.com/owner/repo/pull/9")
    );
    Ok(())
}

// --- the pod-log session scrape (the run completion edge's pod leg) --------------------------

/// A canned run-pod dispatcher: `await_terminal` peeks the given phase, `logs` serves the given
/// text, and `create` keeps every pod it is handed so a test can read the rendered wrapper.
struct RunPodDispatcher {
    phase: crate::runs::workpod::TurnPhase,
    logs: String,
    created: CreatedPods,
}

/// The pods a fake dispatcher created, whole.
type CreatedPods = std::sync::Arc<std::sync::Mutex<Vec<k8s_openapi::api::core::v1::Pod>>>;

/// The wrapper script of the one pod a fake dispatcher created.
fn only_wrapper(created: &CreatedPods) -> String {
    let pods = created.lock().expect("lock");
    assert_eq!(pods.len(), 1, "one pod created");
    crate::testing::fixtures::wrapper_of(&pods[0])
}

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for RunPodDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        mut pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        // The API server stamps a UID on create, and the grant binding depends on reading it back.
        pod.metadata.uid = Some("run-pod-uid".to_string());
        self.created.lock().expect("lock").push(pod.clone());
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Ok(crate::runs::workpod::TerminalState {
            phase: self.phase,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        Ok(self.logs.clone())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

struct GoneRunPodDispatcher;

struct WaitingRunPodDispatcher;

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for WaitingRunPodDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Ok(crate::runs::workpod::TerminalState {
            phase: crate::runs::workpod::TurnPhase::TimedOut,
            message: Some("CreateContainerConfigError: secret crucible-slack not found".into()),
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        panic!("logs must not be read from a pre-start pod")
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for GoneRunPodDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Err(kube::Error::Api(Box::new(kube::core::Status {
            status: Some(kube::core::response::StatusSummary::Failure),
            message: "pod not found".into(),
            reason: "NotFound".into(),
            code: 404,
            ..Default::default()
        }))
        .into())
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        panic!("logs must not be read after a definitive 404")
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// Seed an issue at `running` with a scope and a live run row carrying a pod name — the state
/// `ingest_completion` reads on the pod watch's completion edge.
async fn seed_running_pod_run(db: &Db, key: &str, run_id: &str, pod: &str) -> Result<i64> {
    crate::issues::store::upsert_issue(db.pool(), &sample_issue(key)).await?;
    assert!(crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running).await?);
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: key.into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: run_id.into(),
            scope: Some(scope_id),
            issue: None,
            identity_digest: None,
            status: "running".into(),
            pod: Some(pod.into()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    Ok(scope_id)
}

/// The stranded-session blocker, fixed end to end: nothing stored yet, the pod terminal with the
/// wrapper's delimited dump in its logs → the session is scraped into the artifact store,
/// ingested, and the issue advances off `running`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_scrapes_the_session_from_the_finished_pods_logs(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#40", "run-40", "crucible-run-40").await?;

    let logs = [
        "podman login noise",
        "Trying to pull ghcr.io/x/sandbox:latest...",
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#, // the live tee leg
        "=================== SESSION (rc=0) ===================",
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":230.0}}"#,
        r#"{"v":1,"kind":"budget","spent":1.1,"elapsed_secs":120}"#,
        r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":230.0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
    ]
    .join("\n");
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs,
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#40").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(outcome?, "the completion edge ingested");
    assert_eq!(
        crate::runs::blob_store::get_run_engine_log(db.pool(), "run-40")
            .await?
            .as_deref(),
        Some(
            "podman login noise\nTrying to pull ghcr.io/x/sandbox:latest...\n\
             {\"v\":1,\"kind\":\"row\",\"row\":{\"iter\":0,\"decision\":\"baseline\",\"score\":200.0}}\n"
        ),
        "the pod's own output before the delimiter outlives the pod"
    );
    let written = crate::runs::blob_store::get_run_session(db.pool(), "run-40")
        .await?
        .expect("the scraped session log was stored");
    assert!(
        written.starts_with(r#"{"v":1,"kind":"row""#),
        "only the post-delimiter dump landed: {written}"
    );
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#40")
            .await?
            .unwrap()
            .status,
        Status::Done
    );
    let run = sqlx::query!(r#"SELECT status, best_score FROM runs WHERE run_id='run-40'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(run.status, "finished");
    assert_eq!(run.best_score, Some(230.0));
    Ok(())
}

/// Store a gzipped run-session artifact exactly where the Tier 2 approval would (the pod-evidence
/// owner in the artifact store), so the completion edge finds it and prefers it.
async fn seed_dropbox_run_session(pool: &sqlx::PgPool, pod: &str, ndjson: &str) {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson.as_bytes()).expect("gzip");
    crate::runs::blob_store::put_artifact_bytes(
        pool,
        &crate::runs::blob_store::ArtifactOwner::PodEvidence {
            pod: pod.to_string(),
        },
        crucible_contract::ArtifactKind::RunSession.as_str(),
        u64::MAX,
        enc.finish().expect("gzip finish"),
    )
    .await
    .expect("store artifact");
}

/// The Tier 2 drop-box run-session is PREFERRED over the log scrape: with the artifact present
/// the completion edge decompresses it into the run's stored session and folds it — proven by feeding
/// the dispatcher logs that carry NO session at all (which would otherwise park loud), so only
/// the drop-box could have produced the ingested run.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_prefers_the_dropbox_run_session(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#43", "run-43", "crucible-run-43").await?;

    let session = [
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":245.0}}"#,
        r#"{"v":1,"kind":"budget","spent":1.4,"elapsed_secs":90}"#,
        r#"{"v":1,"kind":"summary","rows":[],"gate":"bench","best_score":245.0}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"done"}"#,
    ]
    .join("\n");
    seed_dropbox_run_session(db.pool(), "crucible-run-43", &session).await;

    // Logs carry NO session (the new wrapper skips the delimiter when ingest is on): if the scrape
    // ran it would NoDelimiter-park. Only the drop-box can advance this run.
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: "podman login noise\n".into(),
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#43").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(outcome?, "the completion edge ingested from the drop-box");
    assert_eq!(
        crate::runs::blob_store::get_run_engine_log(db.pool(), "run-43")
            .await?
            .as_deref(),
        Some("podman login noise\n"),
        "the pod's own output is kept even when the session came from the drop-box"
    );
    let written = crate::runs::blob_store::get_run_session(db.pool(), "run-43")
        .await?
        .expect("the drop-box session was persisted for the run");
    assert_eq!(
        written, session,
        "the plain NDJSON round-trips out of the drop-box gzip"
    );
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#43")
            .await?
            .unwrap()
            .status,
        Status::Done,
        "the issue advanced off running via the drop-box, not the (empty) scrape"
    );
    let run = sqlx::query!(r#"SELECT status, best_score FROM runs WHERE run_id='run-43'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(run.status, "finished");
    assert_eq!(run.best_score, Some(245.0));
    Ok(())
}

/// A `running` issue with NO live run behind it (the last run went terminal — e.g. an unparked
/// no-session park) self-heals back to `awaiting-approval`, where a standing approval
/// re-dispatches. The live failure: an operator unpark restored `running` and nothing ever
/// converged it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn runless_running_self_heals_to_the_approval_approval(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#88", "run-88", "crucible-run-88").await?;
    sqlx::query!(r#"UPDATE runs SET status='no-session' WHERE run_id='run-88'"#)
        .execute(db.pool())
        .await?;

    reconcile_running(
        &db,
        &cfg,
        &crate::issues::store::get_issue(db.pool(), "owner/repo#88")
            .await?
            .unwrap(),
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#88")
            .await?
            .unwrap()
            .status,
        Status::AwaitingApproval,
        "a runless running row converges to the approval gate"
    );
    let events = db.events().read_for_key("owner/repo#88").await?;
    let last = events.last().expect("self-heal event");
    assert_eq!(last.from, "running");
    assert_eq!(last.to, "awaiting-approval");
    Ok(())
}

/// A playbook launch has no scope row, so the self-heal has to find its live run through the
/// row's own issue link. While the pod is in flight the issue stays `running`; once the pod
/// finishes, the completion edge folds the session and lands `done`. The live failure: every
/// pod-dispatched playbook bounced to `awaiting-approval` a minute after launch and its run row
/// stayed `running` forever.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_playbook_issue_with_a_live_pod_run_completes_instead_of_self_healing(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-2";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.5).await;
    assert!(crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running).await?);
    let run_id = format!("{}-1787618376", crate::model::sanitize_key(key));
    crate::runs::store::insert_run(
        db.pool(),
        &NewRun {
            run_id: run_id.clone(),
            scope: None,
            issue: Some(key.into()),
            identity_digest: None,
            status: "running".into(),
            pod: Some("crucible-run-playbook-survey".into()),
            session_uri: None,
            best_score: None,
            cost_usd: None,
        },
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::TimedOut,
        logs: String::new(),
        created: Default::default(),
    }));
    let in_flight = reconcile_running(&db, &cfg, &issue).await;
    crate::runs::workpod::reset_dispatcher();
    in_flight?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("issue")
            .status,
        Status::Running,
        "a live pod run is not a runless row"
    );
    assert!(
        db.events()
            .read_for_key(key)
            .await?
            .iter()
            .all(|e| e.to != "awaiting-approval"),
        "no self-heal fired"
    );

    let session = [
        r#"{"v":1,"kind":"budget","spent":0.28,"elapsed_secs":53}"#,
        r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"graph complete"}"#,
    ]
    .join("\n");
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: format!("plan v1: completed\n{session}\n"),
        created: Default::default(),
    }));
    let finished = reconcile_running(&db, &cfg, &issue).await;
    crate::runs::workpod::reset_dispatcher();
    finished?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("issue")
            .status,
        Status::Done
    );
    let run = sqlx::query!(
        r#"SELECT status, cost_usd, issue FROM runs WHERE run_id = $1"#,
        run_id
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(run.status, "finished");
    assert_eq!(run.cost_usd, Some(0.28));
    assert_eq!(
        run.issue.as_deref(),
        Some(key),
        "the fold keeps the launch's link"
    );
    Ok(())
}

/// A `running` issue whose run row is still live is left alone by the self-heal — the
/// completion edge owns it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn in_flight_running_is_not_self_healed(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#89", "run-89", "crucible-run-89").await?;
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::TimedOut,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile_running(
        &db,
        &cfg,
        &crate::issues::store::get_issue(db.pool(), "owner/repo#89")
            .await?
            .unwrap(),
    )
    .await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#89")
            .await?
            .unwrap()
            .status,
        Status::Running,
        "a live run stays running"
    );
    Ok(())
}

/// A TERMINAL pod whose logs carry no session material must fail LOUD, not wedge: the issue is
/// machine-parked with the reason (event-visible), the run row leaves `running`, and the pod's
/// ledger row is retained as failed for `kubectl logs`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_parks_loud_when_a_terminal_pod_has_no_session(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#41", "run-41", "crucible-run-41").await?;
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &crate::runs::workpod::NewWorkPod {
            pod_name: "crucible-run-41".into(),
            kind: crate::runs::workpod::WorkKind::Run.label_value().into(),
            issue_key: Some("owner/repo#41".into()),
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: crate::runs::workpod::WorkKind::Run.cost_tag().into(),
            cluster: "hub".to_string(),
        },
    )
    .await?;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Failed,
        logs: "podman noise\ncrucible: exploded before the loop started\n".into(),
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#41").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(!outcome?, "nothing ingested");
    assert!(
        crate::runs::blob_store::get_run_session(db.pool(), "run-41")
            .await?
            .is_none()
    );
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#41")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked, "loud, not a silent wedge");
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    let reason = iss.parked_reason.as_deref().unwrap();
    // These logs carry no delimiter → the honest NoDelimiter reason, with the trailing tail as
    // evidence (so the operator reads it in the UI, not via pod forensics).
    assert!(
        reason.contains("no SESSION delimiter at all"),
        "the reason distinguishes the failure mode: {reason:?}"
    );
    assert!(
        reason.contains("exploded before the loop started"),
        "the reason carries the pod-log tail as evidence: {reason:?}"
    );
    let ev = db.events().read_for_key("owner/repo#41").await?;
    assert!(
        ev.iter().any(|e| e.to == "parked"),
        "the park is event-visible"
    );
    let run = sqlx::query!(r#"SELECT status FROM runs WHERE run_id='run-41'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(run.status, "no-session", "the run row left `running`");
    let row = crate::runs::work_pods::get_work_pod(db.pool(), "crucible-run-41")
        .await?
        .expect("row");
    assert_eq!(row.state, crate::runs::workpod::WorkPodState::Failed);
    // Idempotent: a duplicate completion event on the now-parked row is a clean no-op.
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Failed,
        logs: String::new(),
        created: Default::default(),
    }));
    let second = ingest_completion(&db, &cfg, "owner/repo#41").await;
    crate::runs::workpod::reset_dispatcher();
    assert!(!second?);
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn disappeared_prestart_pod_is_infrastructure_failure_not_finished(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#404", "run-404", "crucible-run-404").await?;
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &crate::runs::workpod::NewWorkPod {
            pod_name: "crucible-run-404".into(),
            kind: crate::runs::workpod::WorkKind::Run.label_value().into(),
            issue_key: Some("owner/repo#404".into()),
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: crate::runs::workpod::WorkKind::Run.cost_tag().into(),
            cluster: "hub".into(),
        },
    )
    .await?;

    // First reconciliation observes the required-Secret CreateContainerConfigError and persists it.
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(WaitingRunPodDispatcher));
    assert!(!ingest_completion(&db, &cfg, "owner/repo#404").await?);
    crate::runs::workpod::reset_dispatcher();

    // A later reconciliation (including after controller restart) sees that the pod disappeared.
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(GoneRunPodDispatcher));
    let first = ingest_completion(&db, &cfg, "owner/repo#404").await;
    crate::runs::workpod::reset_dispatcher();
    assert!(!first?);

    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#404")
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked);
    assert!(
        issue
            .parked_reason
            .unwrap_or_default()
            .contains("CreateContainerConfigError")
    );
    let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE run_id = 'run-404'")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(status, "infrastructure-error");
    Ok(())
}

/// The OTHER no-session mode: a pod whose logs DO carry the `SESSION (rc=N)` delimiter but an empty
/// dump (the loop died before writing a single event — e.g. the pack manifest wasn't found). The
/// park reason must name THAT failure (wrapper rc + the pre-delimiter log line), not blame log
/// rotation, so the operator sees the real cause in the UI.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_parks_with_the_rc_when_the_loop_died_before_publishing(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#51", "run-51", "crucible-run-51").await?;
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &crate::runs::workpod::NewWorkPod {
            pod_name: "crucible-run-51".into(),
            kind: crate::runs::workpod::WorkKind::Run.label_value().into(),
            issue_key: Some("owner/repo#51".into()),
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: crate::runs::workpod::WorkKind::Run.cost_tag().into(),
            cluster: "hub".to_string(),
        },
    )
    .await?;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Failed,
        logs: "Error: reading manifest /opt/crucible/domains/x/crucible.toml: No such file \
               or directory\n=================== SESSION (rc=1) ===================\n"
            .into(),
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#51").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(!outcome?, "nothing ingested");
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#51")
        .await?
        .unwrap();
    assert_eq!(iss.status, Status::Parked);
    let reason = iss.parked_reason.as_deref().unwrap();
    assert!(
        reason.contains("rc=1") && reason.contains("empty session.jsonl"),
        "the reason names the wrapper rc + the empty dump: {reason:?}"
    );
    assert!(
        reason.contains("No such file"),
        "the pre-delimiter failure line is the evidence: {reason:?}"
    );
    assert!(
        !reason.contains("no SESSION delimiter"),
        "must NOT blame rotation when the delimiter was present: {reason:?}"
    );
    Ok(())
}

/// A still-running pod (the upstream poll re-enqueued a live run) stays quiet: no ingest, no
/// park, the row keeps waiting on the pod watch.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_waits_quietly_while_the_pod_still_runs(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#42", "run-42", "crucible-run-42").await?;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::TimedOut,
        logs: "should never be read".into(),
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#42").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(!outcome?);
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#42")
            .await?
            .unwrap()
            .status,
        Status::Running
    );
    assert!(
        crate::runs::blob_store::get_run_session(db.pool(), "run-42")
            .await?
            .is_none()
    );
    Ok(())
}

/// Truncated/garbage session content remains parse-tolerant, but without a shutdown record it is
/// not terminal evidence: the run parks instead of being finalized from a torn stream.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ingest_completion_parks_a_truncated_session_without_shutdown(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());
    seed_running_pod_run(&db, "owner/repo#43", "run-43", "crucible-run-43").await?;

    // No delimiter (rotation ate the tail dump) — the live-streamed tee lines, last one torn.
    let logs = [
        "Trying to pull ghcr.io/x/sandbox:latest...",
        r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline","score":200.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","score":230.0}}"#,
        r#"{"v":1,"kind":"row","row":{"iter":2,"deci"#,
    ]
    .join("\n");
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs,
        created: Default::default(),
    }));
    let outcome = ingest_completion(&db, &cfg, "owner/repo#43").await;
    crate::runs::workpod::reset_dispatcher();

    assert!(!outcome?, "a session without shutdown is not complete");
    let run = sqlx::query!(r#"SELECT status, best_score FROM runs WHERE run_id='run-43'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(run.status, "infrastructure-error");
    assert_eq!(run.best_score, None, "uncommitted rows are not finalized");
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#43")
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked);
    let expected = ParkReason::TerminalSessionInvalid {
        run_id: "run-43".into(),
        detail: crate::runs::ingest::TerminalSessionError::MissingShutdown.to_string(),
    }
    .to_string();
    assert_eq!(issue.parked_reason.as_deref(), Some(expected.as_str()));
    Ok(())
}

// --- LLM-only triage ranking ----------------------------------------------------------------

/// A fresh (never-ranked) issue's first reconcile assigns its tier from the ranking call: the
/// row's `tier` moves from `NULL` to the verdict, the rationale lands as event-log evidence,
/// and the call's cost ledgers under `kind = "rank"`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn ranker_assigns_the_first_tier(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), false); // scope outcome is irrelevant to this test
    let gh = wiremock::MockServer::start().await;
    mount_issue(
        &gh,
        "owner/repo",
        20,
        "confirm me",
        "body",
        &["performance"],
    )
    .await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#20")).await?;
    assert!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#20")
            .await?
            .unwrap()
            .tier
            .is_none(),
        "triage never guesses a tier"
    );
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#20",
    )
    .await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#20")
        .await?
        .expect("issue");
    assert_eq!(iss.tier.as_deref(), Some("T1"));
    assert!(iss.ranked_content_hash.is_some());

    let rank_cost = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(rank_cost.n, 1, "the ranking call ledgers under kind=rank");

    let events = crate::event_log::export_string(db.pool()).await?;
    let first: serde_json::Value = serde_json::from_str(events.lines().next().unwrap())?;
    assert_eq!(first["reason"], "confirmed by test double");
    Ok(())
}

/// A changed issue content hash re-ranks: the row's `tier` moves to the new verdict, not the
/// stale one — proof `apply_rank_result`'s cache guard keys off the hash, not off "already
/// ranked."
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_changes_tier_when_content_hash_changes(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), false);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 21, "actually a T0", "body", &[]).await;
    mount_ranker_confirms(&gh, "T0").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#21")).await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#21", "T1", "perf", "a-stale-hash")
        .await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#21",
    )
    .await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#21")
        .await?
        .expect("issue");
    assert_eq!(
        iss.tier.as_deref(),
        Some("T0"),
        "the fresh verdict wins over the stale cached tier"
    );
    assert_ne!(iss.ranked_content_hash.as_deref(), Some("a-stale-hash"));
    Ok(())
}

/// An `N` verdict parks the issue (machine, "unscopeable per ranker") and never reaches the
/// scope turn — no `CRUCIBLE_BIN` override is even set, so a stray call would be a hard error.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn n_verdict_parks_the_issue_as_unscopeable(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(
        &gh,
        "owner/repo",
        22,
        "let's discuss the roadmap",
        "body",
        &[],
    )
    .await;
    mount_ranker_confirms(&gh, "N").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#22")).await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#22",
    )
    .await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#22")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert_eq!(iss.parked_reason.as_deref(), Some("unscopeable per ranker"));
    assert_eq!(iss.tier.as_deref(), Some("N"));
    Ok(())
}

/// An `unrelated`-affinity verdict parks the issue no matter how measurable its tier says it is —
/// a T0 docs chore must not sit in the table as scopeable work. The tier still stamps (with the
/// content hash), so the rank cache holds and a later sweep never re-spends on the same content.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn unrelated_affinity_parks_the_issue_despite_a_scopeable_tier(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(
        &gh,
        "owner/repo",
        24,
        "fix broken README links",
        "body",
        &[],
    )
    .await;
    mount_ranker_unrelated(&gh, "T0").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#24")).await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#24",
    )
    .await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#24")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert_eq!(
        iss.parked_reason.as_deref(),
        Some("unrelated to the performance loop per ranker: docs chore, no perf angle")
    );
    assert_eq!(iss.tier.as_deref(), Some("T0"));
    Ok(())
}

/// An `unrelated` verdict parks BEFORE the grounded escalation, even at low confidence: affinity
/// is settled by the issue text, so a code-grounded turn has nothing to add and must not spend.
/// `CRUCIBLE_BIN` points at a nonexistent binary — if the escalation (or a scope turn) were
/// wrongly reached, the spawn would fail and this test would error.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn unrelated_affinity_parks_without_a_grounded_escalation(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 25, "bump CI runner image", "body", &[]).await;
    mount_ranker_verdict(
        &gh,
        r#"{"tier":"T1","affinity":"unrelated","rationale":"CI chore, no perf angle","confidence":"low","cost_usd":0.0}"#,
    )
    .await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", "/nonexistent/crucible-must-not-run");
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#25")).await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#25",
    )
    .await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#25")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert_eq!(
        iss.parked_reason.as_deref(),
        Some("unrelated to the performance loop per ranker: CI chore, no perf angle")
    );
    assert_eq!(iss.tier.as_deref(), Some("T1"));
    Ok(())
}

/// A malformed verdict (after the bounded retry) leaves the tier `NULL` — there is no
/// heuristic to fall back to — logs the failure, does NOT park the issue, and (the ranker
/// being the sole gate to the scope turn) DEFERS the scope: no verdict, no spend. The issue
/// stays `new` for a later sweep to retry.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn malformed_verdict_defers_the_scope_turn_and_leaves_tier_null(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    // Deliberately NO fake crucible bin: if reconcile wrongly proceeds to the scope turn,
    // the spawn fails and this test errors — deferral must never reach the engine at all.
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 23, "ambiguous", "body", &[]).await;
    mount_ranker_malformed(&gh).await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", "/nonexistent/crucible-must-not-run");
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#23")).await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#23",
    )
    .await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#23")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::New,
        "no verdict, no scope turn — the row waits for a later sweep"
    );
    assert!(
        iss.tier.is_none(),
        "no heuristic to fall back to — a failed rank leaves an honest NULL"
    );
    assert!(
        iss.ranked_content_hash.is_none(),
        "cache key untouched on failure"
    );

    let events = crate::event_log::export_string(db.pool()).await?;
    let first: serde_json::Value = serde_json::from_str(events.lines().next().unwrap())?;
    assert_eq!(first["from"], "new");
    assert_eq!(first["to"], "new");
    assert!(
        first["reason"]
            .as_str()
            .unwrap()
            .contains("scope deferred until ranked")
    );
    Ok(())
}

/// Same content hash on a second reconcile means the ranker is never invoked again — proven
/// by counting the ranking endpoint's received requests, not by inference from cost alone.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn unchanged_content_hash_skips_a_second_rank_call(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 24, "stable content", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#24")).await?;
    // Cap the scope turn at zero so the issue stays at `new` after ranking (no CRUCIBLE_BIN
    // needed at all: the scopes/day cap declines before any subprocess would spawn).
    let cfg = cfg_with(
        dir.path(),
        Profile {
            max_scopes_per_day: 0,
            ..Profile::default()
        },
    );
    reconcile(&db, &cfg, "owner/repo#24").await?;
    assert_eq!(rank_calls(&gh).await, 1, "the first reconcile ranks once");
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#24")
            .await?
            .unwrap()
            .status,
        Status::New,
        "the scope cap declines, so the row stays `new` for a second reconcile"
    );

    // Same issue, unchanged content: a second reconcile must not call the ranker at all.
    reconcile(&db, &cfg, "owner/repo#24").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        rank_calls(&gh).await,
        1,
        "the cache hit must not invoke the ranker a second time"
    );
    Ok(())
}

/// A force re-rank ([`crate::client::Db::clear_rank`]) invalidates the cache for real: after
/// clearing, a sweep over the same unchanged content ranks again instead of cache-hitting.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_cleared_rank_cache_reranks_on_the_next_sweep(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 26, "stable content", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#26")).await?;
    // Zero scopes/day keeps the row at `new` between sweeps (see the cache-hit test above).
    let cfg = cfg_with(
        dir.path(),
        Profile {
            max_scopes_per_day: 0,
            ..Profile::default()
        },
    );
    reconcile(&db, &cfg, "owner/repo#26").await?;
    reconcile(&db, &cfg, "owner/repo#26").await?;
    assert_eq!(
        rank_calls(&gh).await,
        1,
        "unchanged content cache-hits before the clear"
    );

    assert!(crate::issues::store::clear_rank(db.pool(), "owner/repo#26").await?);
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#26")
        .await?
        .expect("issue");
    assert_eq!(
        iss.tier.as_deref(),
        Some("T1"),
        "the standing tier holds while the re-rank is pending"
    );

    reconcile(&db, &cfg, "owner/repo#26").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        rank_calls(&gh).await,
        2,
        "the cleared cache forces a fresh ranking call on the next sweep"
    );
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#26")
        .await?
        .expect("issue");
    assert_eq!(iss.tier.as_deref(), Some("T1"), "fresh verdict recorded");
    assert!(
        iss.ranked_content_hash.is_some(),
        "the cache is repopulated by the fresh verdict"
    );
    Ok(())
}

/// When the daily ceiling is already crossed, `confirm_tier` must not invoke the ranker at
/// all (no spend), the tier stands untouched (`NULL` — never ranked), and a `capped` event is
/// recorded.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn capped_before_the_rank_call_skips_the_ranker(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    // No GITHUB_API_URL mount needed: `reconcile_new`'s ceiling check declines before
    // `confirm_tier` (and its issue-content GET) ever runs.
    db.ledger_append(None, "run", 100.0).await?;
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            ..Profile::default()
        },
    );

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#25")).await?;
    reconcile(&db, &cfg, "owner/repo#25").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    assert_eq!(
        gh.received_requests()
            .await
            .expect("request recording is on by default")
            .len(),
        0,
        "the ranking endpoint must never be called"
    );

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#25")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::New);
    assert!(iss.tier.is_none());
    let capped = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='capped'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(
        capped.n, 1,
        "declined once, before ever spawning the ranker"
    );
    Ok(())
}

/// The baseline schema carries no `tier_source` (dropped back in the SQLite era's migration
/// 0005) while `ranked_content_hash` (the rank cache) is present and round-trips.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn baseline_has_no_tier_source_and_keeps_ranked_content_hash(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#30")).await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#30", "T1", "perf", "somehash")
        .await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#30")
        .await?
        .expect("issue");
    assert_eq!(iss.tier.as_deref(), Some("T1"));
    assert_eq!(iss.ranked_content_hash.as_deref(), Some("somehash"));

    match sqlx::query("SELECT tier_source FROM issues WHERE key = 'owner/repo#30'")
        .fetch_one(db.pool())
        .await
    {
        Ok(_) => panic!("tier_source must not exist in the baseline schema"),
        Err(e) => assert!(
            e.to_string().to_lowercase().contains("does not exist"),
            "tier_source must be gone: {e}"
        ),
    }
    Ok(())
}

// --- T3: multi-component-live-rig-required ---------------------------------------------------

/// A T3 verdict records the tier + rationale + cost like any other verdict, but — with
/// `allow_t3` off (the default) — the row stays `new` instead of proceeding to a scope turn,
/// and a `tier-deferred` line lands in the event log exactly once.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn t3_verdict_defers_instead_of_scoping(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    // No CRUCIBLE_BIN: a T3 deferral must never reach the scope turn — a stray call is a
    // hard error (the same discipline the N-verdict park test uses).
    let gh = wiremock::MockServer::start().await;
    mount_issue(
        &gh,
        "owner/repo",
        26,
        "P/D disaggregation regresses under NIXL handoff",
        "body",
        &[],
    )
    .await;
    mount_ranker_confirms(&gh, "T3").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#26")).await?;
    reconcile(
        &db,
        &cfg_with(dir.path(), Profile::default()),
        "owner/repo#26",
    )
    .await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#26")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::New, "deferred, not scoped or parked");
    assert_eq!(iss.tier.as_deref(), Some("T3"));

    let events = crate::event_log::export_string(db.pool()).await?;
    let lines: Vec<serde_json::Value> = events
        .lines()
        .map(|l| serde_json::from_str(l).expect("event line is JSON"))
        .collect();
    assert_eq!(
        lines
            .iter()
            .filter(|e| e["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("tier-deferred"))
            .count(),
        1,
        "tier-deferred logged exactly once: {lines:?}"
    );
    Ok(())
}

/// The same T3 verdict, with `CONTROLLER_ALLOW_T3`/`allow_t3` on, proceeds to the scope turn
/// like any other tier — no deferral, no `tier-deferred` line.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn allow_t3_lets_a_t3_row_proceed_to_scope(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 27, "a T3 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T3").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#27")).await?;
    let cfg = ControllerCfg {
        allow_t3: true,
        ..cfg_with(dir.path(), Profile::default())
    };
    reconcile(&db, &cfg, "owner/repo#27").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#27")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped, "allow_t3 lets T3 through");
    assert_eq!(iss.tier.as_deref(), Some("T3"));

    let events = crate::event_log::export_string(db.pool()).await?;
    assert!(
        !events.contains("tier-deferred"),
        "allow_t3 means never deferred: {events}"
    );
    Ok(())
}

/// A T3 row's exclusion holds on a cache-hit sweep too (unchanged content, already ranked
/// T3): `reconcile_new` must not run the scope turn a second time either, and — critically —
/// `tier-deferred` is not re-logged on the repeat sweep.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn t3_exclusion_holds_on_a_cache_hit_and_logs_once(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 28, "a T3 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T3").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#28")).await?;
    reconcile(&db, &cfg, "owner/repo#28").await?;
    reconcile(&db, &cfg, "owner/repo#28").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        rank_calls(&gh).await,
        1,
        "the cache hit skips a second rank call"
    );
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#28")
            .await?
            .unwrap()
            .status,
        Status::New,
        "still deferred on the second sweep"
    );

    let events = crate::event_log::export_string(db.pool()).await?;
    let t3_lines = events
        .lines()
        .filter(|l| l.contains("tier-deferred"))
        .count();
    assert_eq!(t3_lines, 1, "tier-deferred logged once, not per sweep");
    Ok(())
}

// --- allowed_tiers (the tier-gate matrix) -----------------------------------------------------

/// `allowed_tiers = [t0]` (an operator narrowing the default) defers a T1 verdict exactly like
/// the old `allow_t3`-only gate deferred T3: the row stays `new`, `tier-deferred` names T1.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn allowed_tiers_t0_only_defers_a_t1_verdict(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    // No CRUCIBLE_BIN: a deferred tier must never reach the scope turn.
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 40, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#40")).await?;
    let cfg = ControllerCfg {
        allowed_tiers: vec![Tier::T0],
        ..cfg_with(dir.path(), Profile::default())
    };
    reconcile(&db, &cfg, "owner/repo#40").await?;
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#40")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::New, "deferred, not scoped or parked");
    assert_eq!(iss.tier.as_deref(), Some("T1"));

    let events = crate::event_log::export_string(db.pool()).await?;
    assert!(
        events.contains("tier-deferred: T1 not in the allowed-tiers set"),
        "{events}"
    );
    Ok(())
}

/// `allowed_tiers = [t0, t1]` (the shipped default) admits a T1 verdict straight through to a
/// scope turn.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn allowed_tiers_t0_t1_admits_a_t1_verdict_to_scope(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 41, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#41")).await?;
    let cfg = ControllerCfg {
        allowed_tiers: vec![Tier::T0, Tier::T1],
        ..cfg_with(dir.path(), Profile::default())
    };
    reconcile(&db, &cfg, "owner/repo#41").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#41")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped, "t1 admitted by allowed_tiers");
    assert_eq!(iss.tier.as_deref(), Some("T1"));

    let events = crate::event_log::export_string(db.pool()).await?;
    assert!(!events.contains("tier-deferred"), "{events}");
    Ok(())
}

/// `allowed_tiers` defaults to `[t0, t1]` — confirmed directly against
/// `EffectiveConfig::resolve`, independent of the reconcile plumbing above.
#[test]
fn allowed_tiers_defaults_to_t0_and_t1() {
    let base = cfg_with(Path::new("/tmp/allowed-tiers-default"), Profile::default()).base_config();
    assert_eq!(base.allowed_tiers, vec![Tier::T0, Tier::T1]);
    let eff = crate::daemon::overrides_store::EffectiveConfig::resolve(
        &base,
        &crate::daemon::overrides_store::OverrideSet::default(),
    );
    assert_eq!(eff.allowed_tiers, vec![Tier::T0, Tier::T1]);
}

/// The `allow_t3` alias still works after `allowed_tiers` lands: it unions `t3` into whatever
/// `allowed_tiers` already resolved to, rather than replacing it.
#[test]
fn allow_t3_alias_unions_t3_without_dropping_the_rest() {
    let mut base = cfg_with(Path::new("/tmp/allow-t3-alias"), Profile::default()).base_config();
    base.allow_t3 = true;
    let eff = crate::daemon::overrides_store::EffectiveConfig::resolve(
        &base,
        &crate::daemon::overrides_store::OverrideSet::default(),
    );
    assert!(eff.allowed_tiers.contains(&Tier::T0));
    assert!(eff.allowed_tiers.contains(&Tier::T1));
    assert!(
        eff.allowed_tiers.contains(&Tier::T3),
        "allow_t3 = true must union T3 in: {:?}",
        eff.allowed_tiers
    );
}

// --- code-grounded escalation (the T3-aware openshell tier) ---------------------------------

/// A stand-in `crucible` binary that handles both subcommands the escalation path touches:
/// `scope` prints a surviving pack, `rank-grounded` prints a grounded verdict of `grounded_tier`.
fn fake_crucible_grounded(dir: &Path, grounded_tier: &str) -> PathBuf {
    let scope_json = r#"{"stages":[{"name":"validate","passed":true,"detail":"ok"}],"digest":"v1:deadbeefcafef00d","cost":0.42}"#;
    let grounded_json = format!(
        r#"{{"tier":"{grounded_tier}","rationale":"grounded: a failing test exists in tests/x.rs","confidence":"high","cost_usd":0.10,"over_budget":false}}"#
    );
    let path = dir.join("crucible-grounded");
    crate::testing::write_exec(
        &path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = scope ]; then\n \
             prev=''\n pack=''\n \
             for a in \"$@\"; do\n  if [ \"$prev\" = '--out' ]; then pack=\"$a\"; fi\n  prev=\"$a\"\n done\n \
             if [ -n \"$pack\" ]; then mkdir -p \"$pack\"; printf '%s' '{manifest}' > \"$pack/crucible.toml\"; fi\n \
             printf '%s' '{scope_json}'\n exit 0\nfi\nif [ \"$1\" = rank-grounded ]; then\n printf '%s\\n' '{grounded_json}'\n exit 0\nfi\nexit 0\n",
            manifest = crate::testing::fixtures::LOOP_PACK_MANIFEST,
        ),
    );
    path
}

/// Seed the controller's per-repo checkout as a bare local git repo so `ensure_checkout` takes
/// the existing-checkout branch (a best-effort offline refresh) instead of cloning from GitHub.
fn seed_checkout(state_dir: &Path, repo: &str) {
    let dir = state_dir.join("checkouts").join(repo.replace('/', "-"));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "init", "-q"])
            .status()
            .unwrap()
            .success()
    );
}

/// A low-confidence text verdict of `tier` — what triggers the grounded escalation.
async fn mount_ranker_low(server: &wiremock::MockServer, tier: &str) {
    mount_ranker_verdict(
        server,
        &format!(
            r#"{{"tier":"{tier}","affinity":"perf","rationale":"unsure from the issue text alone","confidence":"low","cost_usd":0.0}}"#
        ),
    )
    .await;
}

/// A `low`-confidence API verdict escalates to the grounded ranker, whose tier overrides the API
/// one: the row lands on the *grounded* tier (T0), not the low-confidence API tier (T2), the
/// grounded rationale is the recorded evidence, and the grounded turn's cost ledgers separately
/// under `rank-grounded`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn low_confidence_verdict_escalates_to_the_grounded_ranker(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_grounded(dir.path(), "T0");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 30, "ambiguous perf issue", "body", &[]).await;
    mount_ranker_low(&gh, "T2").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_SANDBOX_IMAGE");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#30")).await?;
    reconcile(&db, &cfg, "owner/repo#30").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#30")
        .await?
        .expect("issue");
    assert_eq!(
        iss.tier.as_deref(),
        Some("T0"),
        "the grounded verdict overrides the low-confidence API T2"
    );
    assert_eq!(iss.status, Status::Scoped, "T0 proceeds to the scope turn");

    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(grounded.n, 1, "the grounded turn's cost ledgers separately");

    let events = crate::event_log::export_string(db.pool()).await?;
    let first: serde_json::Value = serde_json::from_str(events.lines().next().unwrap())?;
    assert!(
        first["reason"].as_str().unwrap().contains("grounded"),
        "the grounded rationale is the recorded evidence: {}",
        first["reason"]
    );
    Ok(())
}

// --- the pod executor arm (the WorkPod primitive driven through reconcile) ------------------

/// A canned [`crate::runs::workpod::PodDispatcher`] for the pod-arm reconcile tests: every turn pod
/// "succeeds" with the given logs, and `create` keeps the rendered pod for the test to read.
struct PodArmDispatcher {
    logs: String,
    created: CreatedPods,
}

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for PodArmDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        self.created.lock().expect("lock").push(pod.clone());
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Ok(crate::runs::workpod::TerminalState {
            phase: crate::runs::workpod::TurnPhase::Succeeded,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        Ok(self.logs.clone())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// A stand-in bin for the pod arm's local scope executor: `scope` prints a surviving pack report.
fn fake_crucible_pod_arm(dir: &Path) -> PathBuf {
    let scope_json = r#"{"stages":[{"name":"validate","passed":true,"detail":"ok"}],"digest":"v1:beef","cost":0.42}"#;
    let path = dir.join("crucible-pod-arm");
    crate::testing::write_exec(
        &path,
        &format!(
            r#"#!/bin/sh
if [ "$1" = plan ] && [ "$2" = exposure ]; then
  printf '%s' '{{"version":1,"outputs":[],"capabilities":[]}}'
  exit 0
fi
if [ "$1" = scope ]; then
  prev=""; pack=""
  for a in "$@"; do
    if [ "$prev" = "--out" ]; then pack="$a"; fi
    prev="$a"
  done
  if [ -n "$pack" ]; then mkdir -p "$pack"; printf '%s' '{manifest}' > "$pack/crucible.toml"; fi
  printf '%s' '{scope_json}'; exit 0
fi
exit 0
"#,
            manifest = crate::testing::fixtures::LOOP_PACK_MANIFEST,
        ),
    );
    path
}

fn pod_arm_cfg(state_dir: &Path) -> ControllerCfg {
    let profile_path = crate::testing::fixtures::write_deploy_profile(state_dir);
    let mut cfg = cfg_with(state_dir, Profile::default());
    cfg.grounded_executor = crucible_controller::GroundedExecutor::Pod;
    cfg.deploy_profile = Some(profile_path);
    cfg.grounded_sandbox_image = Some("ghcr.io/example/sandbox:latest".to_string());
    cfg
}

/// Drive reconcile repeatedly, standing in for the daemon's completion-watch re-drives that
/// collect a non-blocking turn: each pass launches a turn (or takes one gate step) and the next
/// collects it — the pod-arm dispatchers' fake pods are instantly terminal. Stops when the
/// issue's observable state (status + tier + the two content-hash caches) stops changing, so a
/// two-phase launch→collect settles without the test hard-coding a pass count.
async fn reconcile_to_fixpoint(
    db: &Db,
    cfg: &ControllerCfg,
    key: &str,
    max_passes: usize,
) -> Result<()> {
    type Snap = Option<(Status, Option<String>, Option<String>, Option<String>)>;
    let mut last: Snap = None;
    for _ in 0..max_passes {
        reconcile(db, cfg, key).await?;
        let snap: Snap = crate::issues::store::get_issue(db.pool(), key)
            .await?
            .map(|i| {
                (
                    i.status,
                    i.tier,
                    i.ranked_content_hash,
                    i.grounded_content_hash,
                )
            });
        if snap == last {
            break;
        }
        last = snap;
    }
    Ok(())
}

/// SINGLE-BOOKING INVARIANT, end to end: a pod-arm grounded escalation drives reconcile through
/// dispatch (which books the cost at collection) AND `apply_verdict` (which must NOT book it
/// again) — exactly ONE `rank-grounded` ledger row lands, carrying the turn's own cost.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_arm_verdict_ledgers_exactly_once_end_to_end(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 60, "ambiguous perf issue", "body", &[]).await;
    mount_ranker_low(&gh, "T2").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: "podman noise\nCRUCIBLE_VERDICT: {\"tier\":\"T0\",\"rationale\":\"grounded: failing test in tests/x.rs\",\"confidence\":\"high\",\"cost_usd\":0.15,\"over_budget\":false}\n".to_string(),
        created: Default::default(),
    }));
    let cfg = pod_arm_cfg(dir.path());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#60")).await?;
    // Non-blocking: pass 1 launches the grounded turn, pass 2 collects the verdict (stamping the
    // tier + rank cache), pass 3 proceeds through the prescope gate to the local scope turn.
    let outcome = reconcile_to_fixpoint(&db, &cfg, "owner/repo#60", 6).await;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    outcome?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#60")
        .await?
        .expect("issue");
    assert_eq!(iss.tier.as_deref(), Some("T0"), "the grounded tier landed");
    assert_eq!(iss.status, Status::Scoped, "T0 proceeded to the scope turn");

    let grounded = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64", COALESCE(SUM(cost_usd), 0.0) AS "sum!: f64" FROM ledger WHERE kind='rank-grounded'"#
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(
        grounded.n, 1,
        "exactly ONE rank-grounded ledger row (workpod booked it; apply_verdict must not re-book)"
    );
    assert!(
        (grounded.sum - 0.15).abs() < 1e-9,
        "the row carries the turn's own cost once: {}",
        grounded.sum
    );
    Ok(())
}

/// A queued grounded escalation defers the rank instead of silently degrading to the text
/// verdict: the tier is NOT finalized, nothing ledgers, and the queued row waits. When the
/// budget frees, the next reconcile pass drains the queued turn and the grounded tier lands —
/// with exactly one rank-grounded ledger row for the whole episode.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn queued_grounded_turn_defers_the_rank_then_drains(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 61, "ambiguous perf issue", "body", &[]).await;
    mount_ranker_low(&gh, "T2").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: "CRUCIBLE_VERDICT: {\"tier\":\"T0\",\"rationale\":\"grounded: failing test in tests/x.rs\",\"confidence\":\"high\",\"cost_usd\":0.15,\"over_budget\":false}\n".to_string(),
        created: Default::default(),
    }));
    let mut cfg = pod_arm_cfg(dir.path());
    cfg.profile.grounded_rank_daily_turns = 0; // budget exhausted → the turn queues

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#61")).await?;
    let pass1 = reconcile(&db, &cfg, "owner/repo#61").await;

    let after_queue = async {
        let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#61")
            .await?
            .expect("issue");
        assert_eq!(
            iss.tier, None,
            "a queued grounded turn must NOT finalize the tier from the text verdict"
        );
        assert_eq!(iss.status, Status::New, "the issue stays new, deferred");
        assert_eq!(
            iss.ranked_content_hash, None,
            "no rank recorded — the next sweep re-enters ranking"
        );
        let ledger = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger"#)
            .fetch_one(db.pool())
            .await?;
        assert_eq!(ledger.n, 0, "nothing ledgers while the turn is queued");
        let queued = crate::runs::work_pods::work_pods_in_states(
            db.pool(),
            &[crate::runs::workpod::WorkPodState::Queued],
        )
        .await?;
        assert_eq!(queued.len(), 1, "the turn waits as one queued row");
        anyhow::Ok(())
    }
    .await;

    // Budget frees → the drain launches the turn (non-blocking) and later passes collect the
    // verdict and proceed through the prescope gate to the scope turn.
    cfg.profile.grounded_rank_daily_turns = 50;
    let pass2 = reconcile_to_fixpoint(&db, &cfg, "owner/repo#61", 6).await;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    pass1?;
    after_queue?;
    pass2?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#61")
        .await?
        .expect("issue");
    assert_eq!(iss.tier.as_deref(), Some("T0"), "the drained grounded tier");
    assert_eq!(iss.status, Status::Scoped);
    assert!(
        crate::runs::work_pods::work_pods_in_states(
            db.pool(),
            &[crate::runs::workpod::WorkPodState::Queued]
        )
        .await?
        .is_empty(),
        "the queued row was consumed by the drain"
    );
    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(grounded.n, 1, "one turn, one ledger row for the episode");
    Ok(())
}

/// A gzip'd tar of a tiny frozen pack, the blob a surviving pod scope turn emits on its
/// `CRUCIBLE_SCOPE_PACK:` marker line.
fn sample_pack_tgz() -> Vec<u8> {
    use std::io::Write as _;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("crucible.toml"),
        crate::testing::fixtures::LOOP_PACK_MANIFEST,
    )
    .unwrap();
    std::fs::write(dir.path().join("SCOPE.md"), "identity: v1:beef\n").unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_dir_all(".", dir.path()).unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar_bytes).unwrap();
    enc.finish().unwrap()
}

/// A pod-executor scope cfg: `scope_executor = pod` on top of the pod-arm profile plumbing.
fn pod_scope_cfg(state_dir: &Path) -> ControllerCfg {
    let mut cfg = pod_arm_cfg(state_dir);
    cfg.scope_executor = crucible_controller::ScopeExecutor::Pod;
    cfg
}

/// The loop-run half of the reconcile-time chain: an approved issue pinned to a provider is
/// dispatched with that provider's flags on the wrapper the pod runs. `dispatch_target`-style
/// plumbing is only correct end to end, so this drives the real `reconcile`, not `for_loop`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pinned_issue_dispatches_its_loop_run_with_the_pair(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };
    crate::playbooks::providers::upsert(
        db.pool(),
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
            created_by: "wren",
        },
    )
    .await?;

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#21")).await?;
    crate::issues::store::set_agent_dispatch(
        db.pool(),
        "owner/repo#21",
        Some("openai-plat"),
        Some("gpt-5.6-sol"),
    )
    .await?;
    assert!(
        crate::issues::store::claim_issue(
            db.pool(),
            "owner/repo#21",
            Status::New,
            Status::AwaitingApproval
        )
        .await?
    );
    let scope_id = crate::issues::store::insert_scope(
        db.pool(),
        &NewScope {
            issue: "owner/repo#21".into(),
            pack_digest: Some("v1:abc".into()),
            check_outcome: Some("PASS".into()),
        },
    )
    .await?;
    sqlx::query!(
        "UPDATE scopes SET approved_by='alice', approved_at='2026-07-02T00:00:00Z' WHERE id=$1",
        scope_id
    )
    .execute(db.pool())
    .await?;
    seed_stored_pack(
        &db,
        "owner/repo#21",
        &[(
            "crucible.toml",
            crate::testing::fixtures::LOOP_PACK_MANIFEST,
        )],
    )
    .await;

    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: created.clone(),
    }));
    let res = reconcile(&db, &cfg, "owner/repo#21").await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let wrapper = only_wrapper(&created);
    assert!(wrapper.contains("--harness=codex"), "{wrapper}");
    assert!(wrapper.contains("--model=gpt-5.6-sol"), "{wrapper}");
    Ok(())
}

/// A scope-turn dispatcher that keeps the pod AND the Secret it was handed, so a test can read
/// both halves of a provider-backed turn.
struct ScopeTurnDispatcher {
    created: CreatedPods,
    created_secrets: std::sync::Arc<std::sync::Mutex<Vec<k8s_openapi::api::core::v1::Secret>>>,
}

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for ScopeTurnDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        mut pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        pod.metadata.uid = Some("scope-turn-uid".to_string());
        self.created.lock().expect("lock").push(pod.clone());
        Ok(pod)
    }
    async fn create_secret(
        &self,
        _cluster: &str,
        _ns: &str,
        secret: k8s_openapi::api::core::v1::Secret,
    ) -> Result<()> {
        self.created_secrets.lock().expect("lock").push(secret);
        Ok(())
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Ok(crate::runs::workpod::TerminalState {
            phase: crate::runs::workpod::TurnPhase::Failed,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// The whole reconcile-time chain for a scope turn: the issue's pinned pair resolves, reaches the
/// turn pod as `--harness`/`--model`, and the provider's registered key rides along as the
/// environment variable that harness reads. A turn rendered against a provider it cannot pay for
/// is the failure this covers.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pinned_scope_turn_renders_its_provider_and_carries_its_key(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let created: CreatedPods = Default::default();
    let created_secrets = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(ScopeTurnDispatcher {
        created: created.clone(),
        created_secrets: created_secrets.clone(),
    }));
    let mut cfg = pod_scope_cfg(dir.path());
    cfg.secret_provider = Some(std::sync::Arc::new(
        crate::secrets::provider::MapProvider::new([(
            "openai_key".to_string(),
            "value-of-openai_key".to_string(),
        )]),
    ));

    let owner = crate::authz::model::Principal::parse("user:platform-admin").expect("a principal");
    let mut conn = db.pool().acquire().await?;
    crate::secrets::store::insert(
        &mut conn,
        &crate::secrets::store::NewSecret {
            id: &uuid::Uuid::now_v7().to_string(),
            owner: &owner,
            name: &crate::secrets::SecretName::parse("openai_key").expect("a name"),
            kind: crate::secrets::SecretKind::InferenceApiKey,
            visibility: crate::secrets::Visibility::BrokerOnly,
            consumer: crate::secrets::ConsumerClass::Run,
            mode: crate::secrets::SecretMode::Managed,
            vault_path: "platform/openai-key",
            current_version: Some(1),
            created_by: Some("platform-admin"),
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    crate::playbooks::providers::upsert(
        db.pool(),
        &crate::playbooks::providers::NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "openai-plat",
            display_name: "OpenAI",
            kind: crate::playbooks::providers::ProviderKind::OpenAi,
            models: &[],
            default_model: None,
            secret: Some(&crate::playbooks::providers::ProviderSecretRef {
                name: "openai_key".to_string(),
                owner,
            }),
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "wren",
        },
    )
    .await?;

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#71")).await?;
    crate::issues::store::set_agent_dispatch(
        db.pool(),
        "owner/repo#71",
        Some("openai-plat"),
        Some("gpt-5.6-sol"),
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#71")
        .await?
        .expect("issue");
    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let pods = created.lock().expect("lock");
    let pod = pods.first().expect("a scope turn pod");
    let argv = crate::testing::fixtures::wrapper_of(pod);
    assert!(argv.contains("--harness codex"), "{argv}");
    assert!(argv.contains("--model gpt-5.6-sol"), "{argv}");
    let key = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .and_then(|c| c.env.as_ref())
        .expect("env")
        .iter()
        .find(|v| v.name == "OPENAI_API_KEY")
        .expect("the provider's key on the turn container")
        .clone();
    assert!(
        key.value.is_none() && key.value_from.is_some(),
        "the key rides a secretKeyRef, never a plain value"
    );
    let secret = created_secrets.lock().expect("lock");
    assert_eq!(
        secret
            .first()
            .expect("the turn's Secret")
            .string_data
            .as_ref()
            .and_then(|d| d.get("openai_key.OPENAI_API_KEY")),
        Some(&"value-of-openai_key".to_string())
    );
    Ok(())
}

/// The pack handoff, happy path: a surviving pod scope turn's pack blob is scraped off the
/// logs, stored as the durable pack tarball (what `reconcile_scoped` and `dispatch_run`
/// materialize), and only then does the row transition to `scoped`.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_survival_lands_the_pack_then_transitions(pool: PgPool) -> Result<()> {
    use base64::Engine as _;
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(sample_pack_tgz());
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: format!(
            "podman noise\nCRUCIBLE_SCOPE_PACK: {b64}\nCRUCIBLE_SCOPE_REPORT: {{\"stages\":[{{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}}],\"digest\":\"v1:beef\",\"cost\":0.42}}\n"
        ),
        created: Default::default(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#70")).await?;
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#70")
        .await?
        .expect("issue");
    // Phase 1: non-blocking — the scope turn pod launches, the issue stays `new`.
    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#70")
            .await?
            .unwrap()
            .status,
        Status::New,
        "launched, not yet collected"
    );
    // Phase 2: the completion re-drive's adopt-first pre-pass collects the terminal pod, lands
    // the pack, and transitions.
    reconcile(&db, &cfg, "owner/repo#70").await?;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#70")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped, "the survival transitioned");
    assert_eq!(
        crate::playbooks::packs::read_pack_file(db.pool(), "owner/repo#70", "crucible.toml")
            .await?
            .as_deref(),
        Some(crate::testing::fixtures::LOOP_PACK_MANIFEST),
        "the manifest landed in the pack store"
    );
    assert!(
        crate::playbooks::packs::read_pack_file(db.pool(), "owner/repo#70", "SCOPE.md")
            .await?
            .is_some(),
        "the frozen SCOPE.md landed in the pack store"
    );
    Ok(())
}

/// The loud-failure invariant: a survival WITHOUT a recoverable pack blob (a pre-feature
/// engine image, an oversize pack's error payload) never transitions to `scoped` over a
/// missing pack — the row stays put and the failure is evented on the issue.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_survival_without_a_pack_blob_fails_loudly(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: "podman noise\nCRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
        created: Default::default(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#71")).await?;
    let issue = crate::issues::store::get_issue(db.pool(), "owner/repo#71")
        .await?
        .expect("issue");
    // Phase 1: launch. Phase 2: the re-drive collects; the survival has no recoverable pack, so
    // the handoff fails loudly instead of transitioning over nothing.
    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    reconcile(&db, &cfg, "owner/repo#71").await?;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#71")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::New,
        "no pack, no `scoped` — the row stays put for a retry"
    );
    assert!(
        crate::runs::blob_store::get_pack_tarball(db.pool(), "owner_repo_71")
            .await?
            .is_none(),
        "nothing pretended to be a pack in the store"
    );
    let events = db.events().read_for_key("owner/repo#71").await?;
    assert!(
        events.iter().any(|e| e
            .reason
            .as_deref()
            .is_some_and(|n| n.contains("pack handoff failed"))),
        "the failure is evented on the issue: {events:?}"
    );
    Ok(())
}

/// A scenario row's scope turn on the Pod executor is framed by its ledgered free text, not a
/// GitHub fetch — same invariant as `scope_propose_uses_goal_file_not_issue_for_a_scenario`, but for
/// `scope_executor = pod`: the `deploy render-turn` argv the controller shells carries `--goal-file`,
/// never `--issue`, so the in-pod `crucible scope --propose` never routes into the GitHub Ingest arm.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_uses_goal_file_not_issue_for_a_scenario(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "podman noise\nCRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("scenario:pod-goal-1")).await?;
    mark_scenario(db.pool(), "scenario:pod-goal-1").await?;
    insert_scenario_body(db.pool(), "scenario:pod-goal-1", "fix the frobnicator").await?;
    let issue = crate::issues::store::get_issue(db.pool(), "scenario:pod-goal-1")
        .await?
        .expect("issue");

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    // The ledgered goal text rides into the in-pod command as `--goal`, so the engine never
    // routes the scenario key into its GitHub Ingest arm — see `render::turn::render_turn`'s
    // `TurnKind::Scope` branch.
    let wrapper = only_wrapper(&created);
    assert!(
        wrapper.contains("--goal "),
        "scenario scope turn on the pod executor must pass the goal itself: {wrapper}"
    );
    assert!(
        !wrapper.contains("--issue scenario:pod-goal-1"),
        "the scenario key is not an issue to fetch: {wrapper}"
    );
    Ok(())
}

/// The goal framing the pack agent reads: a pinned ref must appear in the text, not only in the
/// pod's own `--repo-ref`. `--repo-ref` decides what the SCOPE pod clones; the manifest the agent
/// writes decides what every later RUN clones, and nothing else in the pipeline puts the ref there.
#[test]
fn goal_framing_states_the_ref_the_manifest_must_pin() {
    let repos = vec!["owner/repo".to_string()];

    let pinned = render_goal_framing("fix the frobnicator", &repos, Some("nv_dev"), None);
    assert!(pinned.starts_with("fix the frobnicator"), "{pinned}");
    assert!(
        pinned.contains(r#"[repo] ref = "nv_dev""#),
        "the agent needs the literal manifest key to copy: {pinned}"
    );

    let unpinned = render_goal_framing("fix the frobnicator", &repos, None, None);
    assert_eq!(
        unpinned, "fix the frobnicator",
        "no ref, no added framing at all"
    );

    // Both blocks compose: the multi-repo hint list and the ref sentence are independent.
    let both = render_goal_framing(
        "fix it",
        &["owner/repo".to_string(), "owner/other".to_string()],
        Some("nv_dev"),
        None,
    );
    assert!(both.contains("- owner/other"), "{both}");
    assert!(both.contains(r#"[repo] ref = "nv_dev""#), "{both}");
}

/// End to end on the pod executor: a scenario adopted with a `git_ref` produces a `deploy
/// render-turn` argv carrying `--repo-ref <ref>`, so the turn pod clones the named branch instead
/// of the repo default.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_forwards_the_adopted_git_ref(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins {
            git_ref: Some("nv_dev"),
            ..Default::default()
        },
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(
        issue.git_ref.as_deref(),
        Some("nv_dev"),
        "adoption persists the ref on the issue row"
    );

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    let wrapper = only_wrapper(&created);
    assert!(
        wrapper.contains("git -C /checkout checkout --detach nv_dev"),
        "the pinned ref must ride the render: {wrapper}"
    );
    Ok(())
}

/// A scenario adopted against a pack the repo already carries validates that pack instead of
/// drafting one: no propose, no agent, no sandbox — which is the whole point of importing a pack
/// that is already scored.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_validates_an_adopted_pack_instead_of_proposing(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.0}\n".to_string(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "selfhost",
        "cut the decoder's ns/line",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins {
            pack_path: Some("examples/selfhost"),
            ..Default::default()
        },
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    let wrapper = only_wrapper(&created);
    assert!(
        wrapper.contains("crucible scope --pack /checkout/examples/selfhost"),
        "the adopted pack must be what the turn validates: {wrapper}"
    );
    for absent in ["--propose", "--agent-backend", "--sandbox-image", "--goal"] {
        assert!(
            !wrapper.contains(absent),
            "{absent} has no place here: {wrapper}"
        );
    }
    Ok(())
}

/// The complement: an adoption that named no ref renders no `--repo-ref` flag at all, so the clone
/// keeps taking the repo's default branch.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_omits_repo_ref_when_the_scenario_pinned_none(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins::default(),
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert!(issue.git_ref.is_none());

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    let wrapper = only_wrapper(&created);
    assert!(
        !wrapper.contains("--branch"),
        "no ref adopted, no branch to clone: {wrapper}"
    );
    Ok(())
}

/// The goal framing carries the contract VERBATIM, in a fenced json block. The pack the agent
/// authors is what the broker reads at run time, and the controller projects the same string as
/// `BROKER_CODEGEN_TOOLS_OVERLAY` — paraphrasing it here would let the two describe different
/// measurements.
#[test]
fn goal_framing_embeds_the_codegen_contract_verbatim() {
    let repos = vec!["owner/repo".to_string()];
    let contract = r#"{"gpus":1,"build":{"src_dir":"/opt/deepgemm"}}"#;

    let framed = render_goal_framing("make the kernel fast", &repos, None, Some(contract));
    assert!(framed.starts_with("make the kernel fast"), "{framed}");
    assert!(
        framed.contains(&format!("```json\n{contract}\n```")),
        "the contract must be fenced and byte-identical: {framed}"
    );
    assert!(
        framed.contains("`[measure]`"),
        "the agent must be told where the contract goes: {framed}"
    );
    assert!(
        framed.contains("codegen_build"),
        "and that the judge gate drives the broker MCP tools: {framed}"
    );

    let local = render_goal_framing("make the kernel fast", &repos, None, None);
    assert_eq!(
        local, "make the kernel fast",
        "no contract, no added framing at all"
    );

    // All three blocks compose independently.
    let all = render_goal_framing(
        "fix it",
        &["owner/repo".to_string(), "owner/other".to_string()],
        Some("nv_dev"),
        Some(contract),
    );
    assert!(all.contains("- owner/other"), "{all}");
    assert!(all.contains(r#"[repo] ref = "nv_dev""#), "{all}");
    assert!(all.contains(contract), "{all}");
}

/// End to end on the pod executor: a scenario adopted against a configured contract asks for a
/// broker-measured scope render, which the linked engine cannot express. That is a deterministic
/// boundary refusal: it is ledgered as a contract rejection naming the contract and the missing
/// option, the issue parks so no sweep re-renders it, and nothing reaches the cluster.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_forwards_broker_measure_for_a_contracted_scenario(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
    }));
    let contract = r#"{"gpus":1,"build":{"src_dir":"/opt/deepgemm"}}"#;
    let mut cfg = pod_scope_cfg(dir.path());
    cfg.broker_contracts = crate::config::BrokerContracts::from_map(
        std::collections::BTreeMap::from([("deepgemm".to_string(), contract.to_string())]),
    );

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster kernel",
        "make the blockfp8 megamoe kernel faster",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins {
            codegen_contract: Some("deepgemm"),
            ..Default::default()
        },
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(issue.codegen_contract.as_deref(), Some("deepgemm"));

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    assert!(
        created.lock().expect("lock").is_empty(),
        "a scope turn the engine cannot render under its contract never reaches the cluster"
    );
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(
        issue.status,
        Status::Parked,
        "a refusal no re-render can cure parks: {issue:?}"
    );
    assert_eq!(
        ParkReason::parse(issue.parked_reason.as_deref().unwrap_or_default()),
        ParkReason::UnsupportedTurnOption {
            option: crate::runs::workpod::UnsupportedTurnOption::BrokerMeasure {
                contract: "deepgemm".to_string(),
            }
            .to_string(),
        }
    );
    let events = db.events().read_for_key(&key).await?;
    let evidence = events
        .iter()
        .filter_map(|e| e.evidence.as_deref())
        .find(|v| v.contains("\"kind\":\"contract rejection\""))
        .unwrap_or_else(|| panic!("the refusal is ledgered on the issue: {events:?}"));
    assert!(
        evidence.contains("deepgemm") && evidence.contains("broker-measure"),
        "the evidence names the contract and the missing engine capability: {evidence}"
    );
    assert!(
        crate::runs::work_pods::work_pods_in_states(
            db.pool(),
            &[
                crate::runs::workpod::WorkPodState::Failed,
                crate::runs::workpod::WorkPodState::Running
            ]
        )
        .await?
        .is_empty(),
        "the refusal precedes any work-pod row"
    );
    Ok(())
}

/// The complement: an adoption that named no contract renders and launches as before, so the
/// pack keeps measuring locally on the loop pod.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_omits_broker_measure_when_the_scenario_named_no_contract(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        created: created.clone(),
        logs: "CRUCIBLE_SCOPE_REPORT: {\"stages\":[{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}],\"digest\":\"v1:beef\",\"cost\":0.42}\n".to_string(),
    }));
    let cfg = pod_scope_cfg(dir.path());

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins::default(),
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");

    run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope").await?;
    crate::runs::workpod::reset_dispatcher();

    let wrapper = only_wrapper(&created);
    assert!(
        wrapper.contains("crucible scope --propose"),
        "no contract, no refusal: the scope turn renders: {wrapper}"
    );
    Ok(())
}

/// A contract name the deploy no longer configures fails the scope dispatch loudly. Falling back to
/// local measure would scope a GPU problem into a pack that cannot measure it, and nothing would say
/// so until the run.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn pod_scope_fails_when_the_named_contract_is_not_configured(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let mut cfg = pod_scope_cfg(dir.path());
    cfg.broker_contracts = crate::config::BrokerContracts::from_map(
        std::collections::BTreeMap::from([("vllm".to_string(), r#"{"gpus":2}"#.to_string())]),
    );

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster kernel",
        "make it fast",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins {
            codegen_contract: Some("deepgemm"),
            ..Default::default()
        },
        "admin",
    )
    .await?;
    let issue = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");

    let err = run_scope_and_transition(&db, &cfg, &issue, 5.0, "test scope")
        .await
        .expect_err("an unresolvable contract must not scope");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("deepgemm") && msg.contains("CONTROLLER_BROKER_CONTRACTS"),
        "the error must name the contract and the knob: {msg}"
    );
    Ok(())
}

// --- adopt-first: orphaned turns are collected before any spend gate -------------------------

/// Seed a `running` turn work-pod row — an orphan whose in-band collector died on a restart.
async fn seed_running_turn_row(db: &Db, kind: &str, issue_key: &str, pod_name: &str) {
    crate::runs::work_pods::insert_work_pod(
        db.pool(),
        &crate::runs::workpod::NewWorkPod {
            pod_name: pod_name.to_string(),
            kind: kind.to_string(),
            issue_key: Some(issue_key.to_string()),
            state: crate::runs::workpod::WorkPodState::Running,
            cost_tag: kind.to_string(),
            cluster: "hub".to_string(),
        },
    )
    .await
    .expect("seed running turn row");
}

/// Shared adopt-first-peek collapse for the "orphaned turn behind an exhausted ceiling" pair
/// below: seed the running row (the issue itself is each caller's own setup, since grounded also
/// stamps a recorded rank hash between the two), reconcile once, and assert the adopt-first
/// pre-pass collected the row regardless of the (now-declining) spend gates. The per-kind
/// fold-tail (issue transition/tier stamp, ledger kind) stays in each caller.
async fn adopt_past_an_exhausted_ceiling(
    db: &Db,
    cfg: &ControllerCfg,
    kind: &str,
    issue_key: &str,
    pod_name: &str,
) -> Result<()> {
    seed_running_turn_row(db, kind, issue_key, pod_name).await;

    let outcome = reconcile(db, cfg, issue_key).await;
    crate::runs::workpod::reset_dispatcher();
    outcome?;

    let row = crate::runs::work_pods::get_work_pod(db.pool(), pod_name)
        .await?
        .expect("row");
    assert_eq!(row.state, crate::runs::workpod::WorkPodState::Collected);
    Ok(())
}

/// The restart-mid-ScopeNow orphan (the killer case): the stash was cleared before dispatch
/// (deliberate crash-safety), the controller died mid-turn, and the daily ceiling is exhausted —
/// so the re-driven issue takes the normal path and every spend gate declines before
/// `dispatch_scope`. Collection is not spend: the adopt-first pre-pass must collect the finished
/// pod anyway, land the pack, and transition the issue — the $5-11 report is never orphaned.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn orphaned_scope_turn_is_adopted_past_an_exhausted_ceiling(pool: PgPool) -> Result<()> {
    use base64::Engine as _;
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    } // adoption must never render a pod
    let b64 = base64::engine::general_purpose::STANDARD.encode(sample_pack_tgz());
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: format!(
            "podman noise\nCRUCIBLE_SCOPE_PACK: {b64}\nCRUCIBLE_SCOPE_REPORT: {{\"stages\":[{{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}}],\"digest\":\"v1:beef\",\"cost\":0.42}}\n"
        ),
        created: Default::default(),
    }));
    // Ceiling exhausted: the normal path declines at the very first gate.
    db.ledger_append(None, "run", 200.0).await?;
    let cfg = pod_scope_cfg(dir.path());
    assert!(cfg.profile.daily_cost_ceiling < 200.0, "ceiling exhausted");

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#301")).await?;
    // The ScopeNow stash is GONE (cleared pre-dispatch) and only the running row remains.
    adopt_past_an_exhausted_ceiling(
        &db,
        &cfg,
        "scope",
        "owner/repo#301",
        "crucible-scope-orphan-301",
    )
    .await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#301")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "the orphaned report was collected and the issue transitioned, ceiling notwithstanding"
    );
    assert!(
        crate::playbooks::packs::read_pack_file(db.pool(), "owner/repo#301", "crucible.toml")
            .await?
            .is_some(),
        "the adopted pack landed in the pack store"
    );
    let scope = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64", COALESCE(SUM(cost_usd), 0.0) AS "sum!: f64" FROM ledger WHERE kind='scope'"#
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(scope.n, 1, "the adopted turn's cost booked exactly once");
    assert!((scope.sum - 0.42).abs() < 1e-9);
    Ok(())
}

/// The grounded analogue: an orphaned rank turn behind an exhausted ceiling is collected by the
/// adopt-first pre-pass — verdict applied on the recorded rank hash, cost booked once — instead
/// of waiting for budget to free (which could be the next UTC day).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn orphaned_grounded_turn_is_adopted_past_an_exhausted_ceiling(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: "CRUCIBLE_VERDICT: {\"tier\":\"T0\",\"rationale\":\"grounded: failing test in tests/x.rs\",\"confidence\":\"high\",\"cost_usd\":0.15,\"over_budget\":false}\n".to_string(),
        created: Default::default(),
    }));
    db.ledger_append(None, "run", 200.0).await?;
    let cfg = pod_arm_cfg(dir.path());

    // The turn was launched off this recorded rank (the prescope-origin shape), so adoption
    // stamps the verdict under the same hash — no GitHub fetch needed.
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#302")).await?;
    crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#302", "T2", "perf", "hash-302")
        .await?;
    adopt_past_an_exhausted_ceiling(
        &db,
        &cfg,
        "grounded-rank",
        "owner/repo#302",
        "crucible-turn-orphan-302",
    )
    .await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#302")
        .await?
        .expect("issue");
    assert_eq!(
        iss.tier.as_deref(),
        Some("T0"),
        "the orphaned grounded verdict landed, ceiling notwithstanding"
    );
    assert_eq!(
        iss.grounded_content_hash.as_deref(),
        Some("hash-302"),
        "stamped under the recorded rank hash"
    );
    assert_eq!(
        iss.status,
        Status::New,
        "a tier verdict stamps, never transitions"
    );
    let grounded = sqlx::query!(
        r#"SELECT COUNT(*) AS "n!: i64", COALESCE(SUM(cost_usd), 0.0) AS "sum!: f64" FROM ledger WHERE kind='rank-grounded'"#
    )
    .fetch_one(db.pool())
    .await?;
    assert_eq!(grounded.n, 1, "one turn, one booking");
    assert!((grounded.sum - 0.15).abs() < 1e-9);
    Ok(())
}

/// A dispatcher whose pods are perpetually running — the peek must say "not adoptable".
struct StillRunningDispatcher;

#[async_trait::async_trait]
impl crate::runs::workpod::PodDispatcher for StillRunningDispatcher {
    async fn create(
        &self,
        _cluster: &str,
        _ns: &str,
        pod: k8s_openapi::api::core::v1::Pod,
    ) -> Result<k8s_openapi::api::core::v1::Pod> {
        Ok(pod)
    }
    async fn await_terminal(
        &self,
        _cluster: &str,
        _ns: &str,
        _name: &str,
        _timeout: std::time::Duration,
    ) -> Result<crate::runs::workpod::TerminalState> {
        Ok(crate::runs::workpod::TerminalState {
            phase: crate::runs::workpod::TurnPhase::TimedOut,
            message: None,
        })
    }
    async fn logs(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<String> {
        anyhow::bail!("a still-running pod's logs must never be collected by the pre-pass")
    }
    async fn delete(&self, _cluster: &str, _ns: &str, _name: &str) -> Result<()> {
        Ok(())
    }
}

/// The non-blocking guarantee: a STILL-RUNNING orphan is peeked and left alone — the pre-pass
/// neither blocks the reconcile pass on it nor touches its row (the completion watch re-drives
/// on the terminal edge).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn adopt_first_leaves_a_still_running_turn_alone(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(StillRunningDispatcher));
    db.ledger_append(None, "run", 200.0).await?; // gates decline, so nothing else runs either
    let cfg = pod_scope_cfg(dir.path());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#303")).await?;
    seed_running_turn_row(&db, "scope", "owner/repo#303", "crucible-scope-live-303").await;

    let outcome = reconcile(&db, &cfg, "owner/repo#303").await;
    crate::runs::workpod::reset_dispatcher();
    outcome?;

    assert_eq!(
        crate::runs::work_pods::get_work_pod(db.pool(), "crucible-scope-live-303")
            .await?
            .expect("row")
            .state,
        crate::runs::workpod::WorkPodState::Running,
        "a live turn is left for its terminal edge, never collected early"
    );
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#303")
            .await?
            .expect("issue")
            .status,
        Status::New
    );
    Ok(())
}

/// A `high`-confidence API verdict does NOT escalate: no grounded turn runs (the stand-in bin
/// here has no `rank-grounded` handler at all, so a stray call would parse-fail and be logged,
/// never changing the tier), and the row lands on the API tier directly.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn high_confidence_verdict_does_not_escalate(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true); // handles `scope` only, not `rank-grounded`
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 31, "clear bug", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await; // high confidence (no `confidence` field)
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#31")).await?;
    reconcile(&db, &cfg, "owner/repo#31").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#31")
            .await?
            .unwrap()
            .tier
            .as_deref(),
        Some("T1")
    );
    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(
        grounded.n, 0,
        "no grounded turn on a high-confidence verdict"
    );
    Ok(())
}

// --- stale disposition + the pre-scope grounded confirmation gate ---------------------------

/// A stand-in bin for the pre-scope gate tests: `rank-grounded` returns the given disposition
/// (a tier or `stale`); `scope` prints unparseable output, so a test asserting the gate must
/// never reach the scope turn fails loudly (a parse error) instead of silently passing.
fn fake_crucible_gate(dir: &Path, disposition: &str, rationale: &str) -> PathBuf {
    let grounded_json = format!(
        r#"{{"tier":"{disposition}","rationale":"{rationale}","confidence":"high","cost_usd":0.15,"over_budget":false}}"#
    );
    let path = dir.join("crucible-gate");
    crate::testing::write_exec(
        &path,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = rank-grounded ]; then\n printf '%s\\n' '{grounded_json}'\n exit 0\nfi\nif [ \"$1\" = scope ]; then\n echo 'scope must not run behind the pre-scope gate'\n exit 1\nfi\nexit 0\n"
        ),
    );
    path
}

/// A `stale` low-confidence escalation verdict (the [`apply_verdict`] arm, exercised
/// independent of the pre-scope gate) parks the issue with the grounded rationale attached,
/// and never touches `issues.tier` — `stale` is not a tier.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn stale_verdict_parks_with_the_rationale_attached(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_gate(dir.path(), "stale", "already fixed in src/foo.rs:42");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 40, "bug: X broken", "body", &[]).await;
    mount_ranker_low(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = cfg_with(dir.path(), Profile::default());

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#40")).await?;
    reconcile(&db, &cfg, "owner/repo#40").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#40")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked);
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert!(
        iss.park_reason()
            .is_some_and(|r| matches!(r, ParkReason::StaleAlreadyImplemented { .. }))
    );
    assert!(
        iss.tier.is_none(),
        "stale is not a tier — the column is untouched"
    );

    // Two event lines: `db.park`'s own (the fixed park message) then the grounded rationale as
    // separate evidence — either order is fine, so check the whole log rather than one line.
    let events = crate::event_log::export_string(db.pool()).await?;
    assert!(
        events.contains("src/foo.rs:42"),
        "the grounded rationale is recorded evidence somewhere in the log: {events}"
    );
    Ok(())
}

/// With the pre-scope gate on, a grounded verdict that demotes a T1 API tier to `N` parks the
/// issue instead of ever reaching the scope turn (the stand-in bin's `scope` handler would
/// fail the test if it ran).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prescope_gate_demotion_to_n_parks_instead_of_scoping(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_gate(dir.path(), "N", "grounded: no reproducible objective");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 41, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await; // high confidence: no low-confidence escalation
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = ControllerCfg {
        prescope_grounded: true,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#41")).await?;
    reconcile(&db, &cfg, "owner/repo#41").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#41")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked, "demoted to N, never scoped");
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert!(
        iss.parked_reason
            .as_deref()
            .unwrap()
            .contains("no reproducible objective")
    );
    assert_eq!(
        iss.tier.as_deref(),
        Some("N"),
        "the grounded demotion overwrites the API tier"
    );
    assert!(
        iss.grounded_content_hash.is_some(),
        "the grounded confirmation is recorded so a re-sweep never re-spends"
    );

    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(grounded.n, 1);
    Ok(())
}

/// A local grounded turn whose engine binary carries another contract version is a deterministic
/// boundary refusal, not a missing verdict: it is ledgered as a contract rejection and the issue
/// parks, so no sweep re-attempts it and no text-only verdict is finalized behind its back.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_mismatched_engine_binary_parks_the_local_grounded_turn(pool: PgPool) -> Result<()> {
    use crucible_controller::runs::contract::{ContractRegistry, TableReader};
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_grounded(dir.path(), "T1");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 43, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    crucible_controller::install_contracts(std::sync::Arc::new(ContractRegistry::new(
        std::sync::Arc::new(TableReader::new([(
            bin.display().to_string(),
            Ok(Some("0.0.1".to_string())),
        )])),
    )));
    let cfg = ControllerCfg {
        prescope_grounded: true,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#43")).await?;
    let out = reconcile(&db, &cfg, "owner/repo#43").await;
    crucible_controller::reset_contracts();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    out?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#43")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Parked, "{iss:?}");
    assert_eq!(iss.parked_by, Some(ParkedBy::Machine));
    assert_eq!(
        crucible_controller::model::ParkReason::parse(
            iss.parked_reason.as_deref().unwrap_or_default()
        ),
        crucible_controller::model::ParkReason::ContractRejected {
            image: bin.display().to_string(),
            engine_version: "0.0.1".to_string(),
            controller_version: crucible_controller::runs::contract::CONTROLLER_CONTRACT_VERSION
                .to_string(),
        }
    );
    let events = db.events().read_for_key("owner/repo#43").await?;
    assert!(
        events.iter().any(|e| e
            .evidence
            .as_deref()
            .is_some_and(|v| v.contains("\"kind\":\"contract rejection\"")
                && v.contains("local-grounded-rank"))),
        "the refusal is ledgered with its request kind: {events:?}"
    );
    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(grounded.n, 0, "a refused launch spends nothing");
    Ok(())
}

/// The same setup but the grounded verdict confirms T1 (matches the API tier): the pre-scope
/// gate proceeds and the scope turn actually runs.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prescope_gate_confirms_and_proceeds_to_scope(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_grounded(dir.path(), "T1");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 42, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = ControllerCfg {
        prescope_grounded: true,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#42")).await?;
    reconcile(&db, &cfg, "owner/repo#42").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#42")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped, "confirmed T1 proceeds to scope");
    assert_eq!(iss.tier.as_deref(), Some("T1"));
    assert!(iss.grounded_content_hash.is_some());
    Ok(())
}

/// A hash the pre-scope gate already grounded-confirmed is never re-spent on: a second
/// reconcile of the same (unchanged) issue makes no second `rank-grounded` call.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prescope_gate_skips_a_hash_already_grounded_confirmed(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_grounded(dir.path(), "T1");
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 43, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = ControllerCfg {
        prescope_grounded: true,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#43")).await?;
    reconcile(&db, &cfg, "owner/repo#43").await?;
    assert_eq!(
        crate::issues::store::get_issue(db.pool(), "owner/repo#43")
            .await?
            .unwrap()
            .status,
        Status::Scoped
    );

    // A second reconcile of the same issue key: `claim_issue` at `New -> Scoped` already lost
    // (the row is `scoped` now), so `reconcile` is a same-status no-op — this exercises the
    // pre-scope gate's own cache guard directly instead, since `reconcile` only calls
    // `reconcile_new` while the row is `new`.
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#43", Status::Scoped, Status::New)
            .await?,
        "rewind to `new` to re-drive reconcile_new against the unchanged content"
    );
    reconcile(&db, &cfg, "owner/repo#43").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let grounded =
        sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='rank-grounded'"#)
            .fetch_one(db.pool())
            .await?;
    assert_eq!(
        grounded.n, 1,
        "the second reconcile's cache hit must not spend on the grounded turn again"
    );
    Ok(())
}

/// A grounded call that produces no verdict at all (the fake bin's `rank-grounded` prints
/// nothing parseable) defers the scope turn — no verdict, no spend, extending the same rule
/// [`TierGate::Unranked`] enforces for the API tier.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prescope_gate_failure_defers_the_scope_turn(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let path = dir.path().join("crucible-gate-fail");
    crate::testing::write_exec(
        &path,
        "#!/bin/sh\nif [ \"$1\" = rank-grounded ]; then\n echo 'no comment, no verdict'\n exit 1\nfi\nif [ \"$1\" = scope ]; then\n echo 'scope must not run behind the pre-scope gate'\n exit 1\nfi\nexit 0\n",
    );
    seed_checkout(dir.path(), "owner/repo");
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 44, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &path);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = ControllerCfg {
        prescope_grounded: true,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#44")).await?;
    reconcile(&db, &cfg, "owner/repo#44").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#44")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::New,
        "no grounded verdict — the row waits for a later sweep"
    );
    assert!(iss.grounded_content_hash.is_none());
    Ok(())
}

/// `prescope_grounded: false` skips the new gate entirely: the row proceeds straight from the
/// (high-confidence, non-escalating) API tier to the scope turn without ever invoking
/// `rank-grounded` — the stand-in bin here has no handler for it at all, so a stray call would
/// be a hard parse failure, never a silent pass.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn prescope_grounded_off_preserves_old_behavior(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true); // handles `scope` only, not `rank-grounded`
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 45, "a T1 issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_BACKEND");
    }
    let cfg = ControllerCfg {
        prescope_grounded: false,
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#45")).await?;
    reconcile(&db, &cfg, "owner/repo#45").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#45")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped);
    assert_eq!(iss.tier.as_deref(), Some("T1"));
    assert!(
        iss.grounded_content_hash.is_none(),
        "the gate never ran, so no grounded confirmation was ever recorded"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_horizon_parks_old_and_null_rows_with_exact_reason(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, _dir) = db_with(pool);
    let cfg = ControllerCfg {
        rank_horizon_days: 90,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(tempfile::tempdir()?.path(), Profile::default())
    };

    // Old issue (120 days ago, outside the 90-day horizon)
    let old_date = (jiff::Timestamp::now() - jiff::Span::new().hours(120 * 24))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut iss = sample_issue("owner/repo#100");
    iss.upstream_updated_at = Some(old_date);
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    reconcile(&db, &cfg, "owner/repo#100").await?;

    let parked = crate::issues::store::get_issue(db.pool(), "owner/repo#100")
        .await?
        .unwrap();
    assert_eq!(parked.status, Status::Parked, "old timestamp parks");
    assert_eq!(parked.parked_by, Some(ParkedBy::Machine));
    assert_eq!(
        parked.parked_reason.as_deref(),
        Some("stale: no upstream activity in 90 days"),
        "exact reason string"
    );

    // NULL upstream_updated_at also parks
    let mut null_iss = sample_issue("owner/repo#101");
    null_iss.upstream_updated_at = None;
    crate::issues::store::upsert_issue(db.pool(), &null_iss).await?;
    reconcile(&db, &cfg, "owner/repo#101").await?;

    let null_parked = crate::issues::store::get_issue(db.pool(), "owner/repo#101")
        .await?
        .unwrap();
    assert_eq!(null_parked.status, Status::Parked, "NULL timestamp parks");
    assert_eq!(null_parked.parked_by, Some(ParkedBy::Machine));
    assert_eq!(
        null_parked.parked_reason.as_deref(),
        Some("stale: no upstream activity in 90 days"),
        "NULL gets same reason string"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_horizon_disabled_when_zero(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 101, "an old issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = ControllerCfg {
        rank_horizon_days: 0,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    // Issue with old upstream_updated_at (200 days ago)
    let old_date = (jiff::Timestamp::now() - jiff::Span::new().hours(200 * 24))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut iss = sample_issue("owner/repo#101");
    iss.upstream_updated_at = Some(old_date);
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    reconcile(&db, &cfg, "owner/repo#101").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss_after = crate::issues::store::get_issue(db.pool(), "owner/repo#101")
        .await?
        .unwrap();
    assert_eq!(
        iss_after.status,
        Status::Scoped,
        "horizon=0 disables the gate; old issue proceeds to rank and scope"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_horizon_fresh_row_proceeds(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 102, "a fresh issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = ControllerCfg {
        rank_horizon_days: 90,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    // Fresh issue (10 days ago, well within horizon)
    let fresh_date = (jiff::Timestamp::now() - jiff::Span::new().hours(10 * 24))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut iss = sample_issue("owner/repo#102");
    iss.upstream_updated_at = Some(fresh_date);
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    reconcile(&db, &cfg, "owner/repo#102").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let iss_after = crate::issues::store::get_issue(db.pool(), "owner/repo#102")
        .await?
        .unwrap();
    assert!(
        iss_after.status != Status::Parked,
        "fresh upstream_updated_at proceeds past rank horizon gate"
    );
    // Second reconcile to complete the scope turn
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    reconcile(&db, &cfg, "owner/repo#102").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    let final_state = crate::issues::store::get_issue(db.pool(), "owner/repo#102")
        .await?
        .unwrap();
    assert_eq!(
        final_state.status,
        Status::Scoped,
        "completes the scope turn"
    );
    Ok(())
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn rank_horizon_auto_unpark_on_fresh_activity(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    let gh = wiremock::MockServer::start().await;
    mount_issue(&gh, "owner/repo", 103, "a revived issue", "body", &[]).await;
    mount_ranker_confirms(&gh, "T1").await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    let cfg = ControllerCfg {
        rank_horizon_days: 90,
        grounded_executor: crucible_controller::GroundedExecutor::Local,
        deploy_profile: None,
        grounded_sandbox_image: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    // Park an old issue via the horizon gate
    let old_date = (jiff::Timestamp::now() - jiff::Span::new().hours(120 * 24))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut iss = sample_issue("owner/repo#103");
    iss.upstream_updated_at = Some(old_date);
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    reconcile(&db, &cfg, "owner/repo#103").await?;

    let parked = crate::issues::store::get_issue(db.pool(), "owner/repo#103")
        .await?
        .unwrap();
    assert_eq!(parked.status, Status::Parked);
    assert_eq!(parked.parked_by, Some(ParkedBy::Machine));

    // Re-upsert with fresh upstream_updated_at (simulating upstream activity)
    let fresh_date = (jiff::Timestamp::now() - jiff::Span::new().hours(5 * 24))
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    iss.upstream_updated_at = Some(fresh_date);
    crate::issues::store::upsert_issue(db.pool(), &iss).await?;
    reconcile(&db, &cfg, "owner/repo#103").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let revived = crate::issues::store::get_issue(db.pool(), "owner/repo#103")
        .await?
        .unwrap();
    assert_eq!(
        revived.status,
        Status::New,
        "fresh upstream_updated_at on a machine-parked row auto-unparks to New"
    );
    assert!(
        revived.parked_by.is_none(),
        "park authority cleared after auto-unpark"
    );

    // Second reconcile to complete the scope turn
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }
    reconcile(&db, &cfg, "owner/repo#103").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }

    let final_state = crate::issues::store::get_issue(db.pool(), "owner/repo#103")
        .await?
        .unwrap();
    assert_eq!(
        final_state.status,
        Status::Scoped,
        "second reconcile completes the scope turn after auto-unpark"
    );
    Ok(())
}

#[tokio::test]
async fn config_env_parse_rank_horizon_days() {
    let _g = crate::ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var("CONTROLLER_RANK_HORIZON_DAYS", "120");
    }
    let cfg = crate::testing::cfg_from_args(["crucible-controller"]);
    assert_eq!(cfg.rank_horizon_days, 120);
    unsafe {
        std::env::remove_var("CONTROLLER_RANK_HORIZON_DAYS");
    }

    // Default value (no env)
    let cfg_default = crate::testing::cfg_from_args(["crucible-controller"]);
    assert_eq!(cfg_default.rank_horizon_days, 0);
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn autopilot_disabled_skips_new_issue_reconcile(pool: PgPool) -> Result<()> {
    let (db, dir) = db_with(pool);
    let flag = crate::daemon::autopilot_flag::AutopilotFlag::load(db.pool()).await?;
    flag.set(false, Some("test"), "cost runaway").await?;

    let cfg = ControllerCfg {
        autopilot: Some(flag),
        overrides_configmap: "crucible-controller-overrides".to_string(),
        overrides_namespace: None,
        overrides: None,
        github_app: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#99")).await?;
    reconcile(&db, &cfg, "owner/repo#99").await?;

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#99")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::New,
        "autopilot disabled: status must not change"
    );
    let events = db.events().read_for_key("owner/repo#99").await?;
    assert!(
        events.is_empty(),
        "autopilot disabled: no events should be appended"
    );
    let cost = crate::ledger::ledger_day_total(db.pool(), &crate::clock::today_utc()).await?;
    assert!(
        cost < f64::EPSILON,
        "autopilot disabled: no ledger rows should land"
    );
    Ok(())
}

/// The autopilot pause holds machine-initiated GitHub discovery, but a scenario's `!has_upstream()`
/// is a standing exemption alongside `scope_now_justification`/`redispatch_justification` — the
/// human adoption already authorized it, so it must reach the scope turn even while paused.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn autopilot_disabled_does_not_hold_an_adopted_scenario(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let flag = crate::daemon::autopilot_flag::AutopilotFlag::load(db.pool()).await?;
    flag.set(false, Some("test"), "cost runaway").await?;

    let cfg = ControllerCfg {
        autopilot: Some(flag),
        overrides_configmap: "crucible-controller-overrides".to_string(),
        overrides_namespace: None,
        overrides: None,
        github_app: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    let key = crate::issues::store::adopt_scenario(
        db.pool(),
        "faster p99",
        "cut p99 latency under load",
        &["owner/repo".to_string()],
        false,
        crate::issues::store::AdoptPins::default(),
        "admin",
    )
    .await?;
    reconcile(&db, &cfg, &key).await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), &key)
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "the pause exempts a scenario the same way it exempts ScopeNow/redispatch"
    );
    Ok(())
}

/// ScopeNow bypasses ALL gates: daily ceiling, rank horizon, unranked tier, scopes/day cap.
/// An issue with `scope_now_justification` set goes straight to the scope turn even when the
/// autopilot would park/decline it.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_now_bypasses_ceiling_and_unranked_gates(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }

    // Push ledger past the daily ceiling so autopilot would decline.
    db.ledger_append(None, "run", 200.0).await?;
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            max_scopes_per_day: 0, // scopes/day also capped at 0
            ..Profile::default()
        },
    );

    // Issue: no tier (unranked), no upstream_updated_at (would fail rank horizon).
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#200")).await?;
    crate::runs::work_pods::set_scope_now(
        db.pool(),
        "owner/repo#200",
        "customer escalation",
        Some(5.0),
    )
    .await?;

    reconcile(&db, &cfg, "owner/repo#200").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#200")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "ScopeNow must bypass ceiling + unranked + scopes/day gates"
    );
    assert!(
        iss.scope_now_justification.is_none(),
        "stash cleared after dispatch"
    );

    // Event carries the justification.
    let events = crate::event_log::export_string(db.pool()).await?;
    assert!(
        events.contains("ScopeNow: customer escalation"),
        "event must log the justification: {events}"
    );
    Ok(())
}

/// ScopeNow on the POD executor, two-phase and non-blocking: pass 1 clears the stash and
/// LAUNCHES the scope turn (the issue stays `new`, no transition yet); the completion re-drive's
/// adopt-first pre-pass collects the terminal pod, lands the pack, and transitions to `scoped`.
/// The stash is gone by then (cleared pre-dispatch), so the collection rides the pre-pass — the
/// exact hole PR #131 closed — with an event trail that reads sensibly.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_now_pod_executor_launches_then_a_redrive_scopes(pool: PgPool) -> Result<()> {
    use base64::Engine as _;
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible_pod_arm(dir.path());
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(sample_pack_tgz());
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(PodArmDispatcher {
        logs: format!(
            "podman noise\nCRUCIBLE_SCOPE_PACK: {b64}\nCRUCIBLE_SCOPE_REPORT: {{\"stages\":[{{\"name\":\"validate\",\"passed\":true,\"detail\":\"ok\"}}],\"digest\":\"v1:beef\",\"cost\":0.42}}\n"
        ),
        created: Default::default(),
    }));
    // Ceiling exhausted: only ScopeNow (human-authorized) could reach the turn at all.
    db.ledger_append(None, "run", 200.0).await?;
    let cfg = pod_scope_cfg(dir.path());
    assert!(cfg.profile.daily_cost_ceiling < 200.0, "ceiling exhausted");

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#210")).await?;
    crate::runs::work_pods::set_scope_now(
        db.pool(),
        "owner/repo#210",
        "customer escalation",
        Some(5.0),
    )
    .await?;

    // Phase 1: the stash is consumed and the scope turn launches — no transition yet.
    reconcile(&db, &cfg, "owner/repo#210").await?;
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#210")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::New, "launched, not yet collected");
    assert!(
        iss.scope_now_justification.is_none(),
        "stash cleared at dispatch"
    );
    assert!(
        crate::runs::work_pods::find_running_work_pod(db.pool(), "scope", "owner/repo#210")
            .await?
            .is_some(),
        "a running scope turn is in flight"
    );

    // Phase 2: the completion re-drive's pre-pass collects the terminal pod and transitions.
    reconcile(&db, &cfg, "owner/repo#210").await?;
    crate::runs::workpod::reset_dispatcher();
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#210")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "the collected report transitioned it"
    );
    assert!(
        crate::playbooks::packs::read_pack_file(db.pool(), "owner/repo#210", "crucible.toml")
            .await?
            .is_some(),
        "the adopted pack landed in the pack store"
    );
    // Exactly one scope booking (the collection CAS), and the transition event reads sensibly.
    let scope = sqlx::query!(r#"SELECT COUNT(*) AS "n!: i64" FROM ledger WHERE kind='scope'"#)
        .fetch_one(db.pool())
        .await?;
    assert_eq!(scope.n, 1, "the turn's cost booked exactly once");
    let events = db.events().read_for_key("owner/repo#210").await?;
    assert!(
        events.iter().any(|e| e.to == "scoped"
            && e.reason
                .as_deref()
                .is_some_and(|r| r.contains("collected on completion"))),
        "the transition event reads sensibly: {events:?}"
    );
    Ok(())
}

/// The flag pauses the machine, never the humans: a stashed ScopeNow executes even while
/// autopilot is disabled (the disabled guard exempts it).
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_now_executes_when_autopilot_disabled(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }

    let flag = crate::daemon::autopilot_flag::AutopilotFlag::load(db.pool()).await?;
    flag.set(false, Some("test"), "proving-phase pause").await?;
    let cfg = ControllerCfg {
        autopilot: Some(flag),
        overrides_configmap: "crucible-controller-overrides".to_string(),
        overrides_namespace: None,
        overrides: None,
        github_app: None,
        ..cfg_with(dir.path(), Profile::default())
    };

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#201")).await?;
    crate::runs::work_pods::set_scope_now(
        db.pool(),
        "owner/repo#201",
        "human while paused",
        Some(5.0),
    )
    .await?;

    reconcile(&db, &cfg, "owner/repo#201").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#201")
        .await?
        .expect("issue");
    assert_eq!(
        iss.status,
        Status::Scoped,
        "ScopeNow must execute while autopilot is disabled"
    );
    Ok(())
}

/// A ScopeNow-scoped issue must not re-scope on the next reconcile: the stash is cleared
/// after dispatch, so a second reconcile goes through the normal (now-gateable) path.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn scope_now_no_double_dispatch(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_crucible(dir.path(), true);
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    // Push ledger past the ceiling so the normal path declines.
    db.ledger_append(None, "run", 200.0).await?;
    let cfg = cfg_with(
        dir.path(),
        Profile {
            daily_cost_ceiling: 50.0,
            ..Profile::default()
        },
    );

    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#201")).await?;
    crate::runs::work_pods::set_scope_now(db.pool(), "owner/repo#201", "hotfix", None).await?;

    reconcile(&db, &cfg, "owner/repo#201").await?;
    let iss = crate::issues::store::get_issue(db.pool(), "owner/repo#201")
        .await?
        .expect("issue");
    assert_eq!(iss.status, Status::Scoped);
    assert!(iss.scope_now_justification.is_none(), "stash cleared");

    // Rewind to `new` so reconcile_new runs again (simulating re-enqueue).
    crate::issues::store::claim_issue(db.pool(), "owner/repo#201", Status::Scoped, Status::New)
        .await?;

    // Without the stash, the normal path hits the ceiling and stays `new`.
    reconcile(&db, &cfg, "owner/repo#201").await?;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }

    let iss2 = crate::issues::store::get_issue(db.pool(), "owner/repo#201")
        .await?
        .expect("issue");
    assert_eq!(
        iss2.status,
        Status::New,
        "without the ScopeNow stash, the ceiling gate blocks re-scoping"
    );
    Ok(())
}

// --- burst guard (Lane O3): a newly-watched repo's whole backlog lands in one discovery
// sweep, but the tight runtime-override cap (PR #85) still throttles ranking one issue at a
// time, exactly as it would for a slow trickle. No burst-sized backlog may bypass the cap.

const BURST_SIZE: u64 = 10;

fn burst_issue(number: u64) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": format!("burst issue {number}"),
        "body": "",
        "labels": [],
        "html_url": format!("https://github.com/burst/repo/issues/{number}"),
        "updated_at": "2026-07-01T00:00:00Z",
        "state": "open",
    })
}

#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn burst_backlog_respects_the_tight_rank_cap_no_bypass(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);

    let gh = wiremock::MockServer::start().await;
    // The repo's whole open-issue backlog, all at once (one discovery sweep of a
    // freshly-watched repo).
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/repos/burst/repo/issues"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json((1..=BURST_SIZE).map(burst_issue).collect::<Vec<_>>()),
        )
        .mount(&gh)
        .await;
    // `confirm_tier`'s per-issue GET, for every issue in the backlog.
    for n in 1..=BURST_SIZE {
        mount_issue(&gh, "burst/repo", n, &format!("burst issue {n}"), "", &[]).await;
    }
    // Every rank call costs a fixed $1 — cheap enough to reason about against the ceiling.
    mount_ranker_verdict(
        &gh,
        r#"{"tier":"T2","affinity":"perf","rationale":"burst backlog verdict","cost_usd":1.0}"#,
    )
    .await;
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", "/bin/true");
    } // never reached (scope_executor disabled)
    unsafe {
        std::env::set_var("CONTROLLER_RANKER_API_URL", gh.uri());
    }
    unsafe {
        std::env::set_var("GITHUB_API_URL", gh.uri());
    }

    // The tight cap: a runtime override (Lane O2's ConfigStore, PR #85), not a redeploy —
    // exactly how an operator would throttle a burst live. $3 ceiling / $1 per rank ⇒ exactly
    // 3 of the 10 issues get ranked; the other 7 stay `ranked=NULL`, waiting their turn.
    let mut cfg = cfg_with(dir.path(), Profile::default());
    cfg.scope_executor = crate::config::ScopeExecutor::Disabled;
    let store = crate::daemon::overrides_store::ConfigStore::seeded_for_test(
        cfg.base_config(),
        crate::daemon::overrides_store::OverrideSet {
            daily_cost_ceiling: Some(3.0),
            ..Default::default()
        },
    );
    cfg.overrides = Some(store);

    // Discovery: the whole backlog lands as `new` issues in one sweep.
    let keys = triage::poll_repo(&db, "burst/repo").await?;
    assert_eq!(
        keys.len(),
        BURST_SIZE as usize,
        "the whole backlog is discovered at once"
    );

    // Drain: reconcile every non-terminal row once (the `--once` autopilot pattern), same as
    // a real daemon's worker would across many ticks.
    for key in crate::issues::store::non_terminal_keys(db.pool()).await? {
        reconcile(&db, &cfg, &key).await?;
    }

    let mut ranked = 0usize;
    let mut unranked = 0usize;
    for n in 1..=BURST_SIZE {
        let issue = crate::issues::store::get_issue(db.pool(), &format!("burst/repo#{n}"))
            .await?
            .expect("issue tracked");
        if issue.tier.is_some() {
            ranked += 1;
        } else {
            unranked += 1;
            assert_eq!(
                issue.status,
                Status::New,
                "an unranked row waits at `new`, not silently parked or scoped"
            );
        }
    }
    assert_eq!(ranked, 3, "exactly ceiling/cost issues got ranked");
    assert_eq!(unranked, 7, "the rest wait their turn, ranked=NULL");
    assert_eq!(
        crate::ledger::ledger_day_total(db.pool(), &crate::clock::today_utc()).await?,
        3.0,
        "the ledger stops exactly at the ceiling, never over"
    );

    // No bypass: draining again (a second sweep, still under the same tight ceiling) does not
    // rank any more of the backlog.
    for key in crate::issues::store::non_terminal_keys(db.pool()).await? {
        reconcile(&db, &cfg, &key).await?;
    }
    let still_ranked = {
        let mut n = 0;
        for k in 1..=BURST_SIZE {
            if crate::issues::store::get_issue(db.pool(), &format!("burst/repo#{k}"))
                .await?
                .expect("issue")
                .tier
                .is_some()
            {
                n += 1;
            }
        }
        n
    };
    assert_eq!(
        still_ranked, 3,
        "a second sweep under the same cap ranks nothing further — no cap bypass"
    );

    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    unsafe {
        std::env::remove_var("CONTROLLER_RANKER_API_URL");
    }
    unsafe {
        std::env::remove_var("GITHUB_API_URL");
    }
    Ok(())
}

// --- playbook launches --------------------------------------------------------

/// Seed a registered playbook whose stored tarball is a minimal build-free pack. Registration
/// itself is covered in `playbooks.rs`; this is the row a launch is adopted against.
async fn seed_registered_playbook(db: &Db, id: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("crucible.toml"),
        crate::testing::fixtures::PLAYBOOK_PACK_MANIFEST,
    )
    .expect("manifest");
    std::fs::write(
        dir.path().join("workflow.star"),
        crate::testing::fixtures::WORKFLOW_TOPIC_DEPTH,
    )
    .expect("source");
    let tar_gz = crate::playbooks::packs::tar_pack_tree(dir.path()).expect("tar");
    let bytes = tar_gz.len() as i64;
    let digest = crucible_contract::content_digest(&tar_gz);
    sqlx::query(
        r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz, tar_digest,
                                  tar_bytes, params_schema, schema_digest, core_rev, created_by,
                                  created_at, updated_at)
           VALUES ($1, 'a survey', 'owner/repo', NULL, 'deadbeef', '', $2, $4, $3,
                   '{"type":"object"}'::jsonb, 'sha256:form', 'deadbeef', 'wren',
                   '2026-08-22T00:00:00Z', '2026-08-22T00:00:00Z')"#,
    )
    .bind(id)
    .bind(&tar_gz)
    .bind(bytes)
    .bind(&digest)
    .execute(db.pool())
    .await
    .expect("seed playbook");
}

async fn adopt_launch(db: &Db, key: &str, max_cost: f64) {
    let params = serde_json::json!({"topic": "attention sinks", "depth": "deep"});
    let max_time = crate::model::MaxTime::parse("30m").expect("duration");
    assert!(
        matches!(
            crate::launches::store::adopt_playbook_launch(
                db.pool(),
                key,
                &crate::launches::model::NewPlaybookLaunch {
                    playbook: "survey",
                    repo: "owner/repo",
                    title: "a survey",
                    params: &params,
                    schema_digest: "sha256:form",
                    max_cost,
                    max_time: &max_time,
                    advance_dedupe: false,
                    dedupe_schedule: None,
                    origin: crate::model::LaunchOrigin::Manual,
                    draft_version: None,
                    created_by: Some("wren"),
                    launcher_groups: None,
                },
            )
            .await
            .expect("adopt"),
            crate::launches::store::AdoptPlaybookOutcome::Adopted
        ),
        "the registered playbook exists, so the launch lands"
    );
}

/// A schedule with a cursor, and a `running` launch of it, ready to be completed.
async fn scheduled_launch_with_cursor(db: &Db, key: &str) -> String {
    let max_time = crate::model::MaxTime::parse("30m").expect("duration");
    let spec = crate::launches::schedules::cron::CronSpec::parse("0 * * * *", "UTC").expect("expr");
    let cursor =
        crate::launches::model::CursorSpec::parse("$.scan.newest_created_at", Some("since"), None)
            .expect("cursor");
    let params = serde_json::json!({"topic": "attention sinks"});
    let scheduled = crate::launches::schedules::ScheduleStore::new(db.clone())
        .create(
            &crate::launches::schedules::NewSchedule {
                standing: crate::launches::standing::NewStanding {
                    playbook: "survey",
                    target_kind: "adopted",
                    eligible_draft_version: None,
                    params: &params,
                    schema_digest: "sha256:form",
                    max_cost: 3.5,
                    max_time: &max_time,
                    advance_dedupe: true,
                    enabled: true,
                    created_by: Some("wren"),
                    owner_principal: None,
                    owner_groups: None,
                    dispatch_target: None,
                    agent_provider: None,
                    agent_model: None,
                },
                cursor: Some(&cursor),
                spec: &spec,
            },
            jiff::Timestamp::now(),
        )
        .await
        .expect("create schedule");
    let adopted = crate::launches::store::adopt_playbook_launch(
        db.pool(),
        key,
        &crate::launches::model::NewPlaybookLaunch {
            playbook: "survey",
            repo: "owner/repo",
            title: "a survey",
            params: &params,
            schema_digest: "sha256:form",
            max_cost: 3.5,
            max_time: &max_time,
            advance_dedupe: true,
            dedupe_schedule: Some(&scheduled.id),
            origin: crate::model::LaunchOrigin::Schedule,
            draft_version: None,
            created_by: Some("wren"),
            launcher_groups: None,
        },
    )
    .await
    .expect("adopt");
    assert!(matches!(
        adopted,
        crate::launches::store::AdoptPlaybookOutcome::Adopted
    ));
    assert!(
        crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running)
            .await
            .expect("claim")
    );
    scheduled.id
}

/// A playbook log whose scan task emitted the field the cursor points at.
fn playbook_log(outcome: &str) -> String {
    [
        r#"{"v":1,"kind":"identity","identity":{"digest":"v1:aaaa"}}"#,
        r#"{"v":1,"kind":"task_result","task":"scan","status":"pass","output":{"newest_created_at":"2026-08-23T11:59:00Z"},"cost_usd":0.4}"#,
        r#"{"v":1,"kind":"budget","spent":0.4,"elapsed_secs":42}"#,
        &format!(r#"{{"v":1,"kind":"shutdown","outcome":"{outcome}","reason":"graph complete"}}"#),
    ]
    .join("\n")
}

/// The v1 cursor closes here: a scheduled launch that finished stores the field its run produced,
/// and the next firing has a value to pass.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_finished_playbook_run_stores_the_cursor_value(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-3";
    seed_registered_playbook(&db, "survey").await;
    let scheduled = scheduled_launch_with_cursor(&db, key).await;

    complete_run(
        &db,
        key,
        None,
        "run-cursor",
        None,
        &playbook_log("finished"),
        None,
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("row")
            .status,
        Status::Done
    );
    let row = crate::launches::schedules::ScheduleStore::new(db.clone())
        .get(&scheduled)
        .await?
        .expect("schedule");
    assert_eq!(row.cursor_value.as_deref(), Some("2026-08-23T11:59:00Z"));
    assert!(row.cursor_updated_at.is_some());
    Ok(())
}

/// A run that errored parks and leaves the cursor where it was, so the next firing re-reads the
/// same window. That is the whole of "stored only after a successful run".
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn an_errored_playbook_run_leaves_the_cursor_alone(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-4";
    seed_registered_playbook(&db, "survey").await;
    let scheduled = scheduled_launch_with_cursor(&db, key).await;

    complete_run(
        &db,
        key,
        None,
        "run-cursor",
        None,
        &playbook_log("error"),
        None,
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("row")
            .status,
        Status::Parked
    );
    let row = crate::launches::schedules::ScheduleStore::new(db.clone())
        .get(&scheduled)
        .await?
        .expect("schedule");
    assert_eq!(row.cursor_value, None, "a failed run advances nothing");
    Ok(())
}

/// A run that stopped at a ceiling completes to `done` but processed only part of its window, so
/// the cursor stays put and the next firing re-reads the same window. Only `finished`/`solved` is
/// evidence the inputs were consumed.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_stopped_run_leaves_the_cursor_alone(pool: PgPool) -> Result<()> {
    let (db, _dir) = db_with(pool);
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-5";
    seed_registered_playbook(&db, "survey").await;
    let scheduled = scheduled_launch_with_cursor(&db, key).await;

    complete_run(
        &db,
        key,
        None,
        "run-cursor",
        None,
        &playbook_log("stopped"),
        None,
    )
    .await?;

    assert_eq!(
        crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("row")
            .status,
        Status::Done,
        "a ceiling still completes the run"
    );
    let row = crate::launches::schedules::ScheduleStore::new(db.clone())
        .get(&scheduled)
        .await?
        .expect("schedule");
    assert_eq!(
        row.cursor_value, None,
        "a partial window advances no cursor"
    );
    assert_eq!(row.cursor_updated_at, None);
    Ok(())
}

/// End to end from a dequeued key: a launch re-reads its own stored row, renders the pod with
/// those values, and CASes `new` → `running` with a run row carrying no scope. No scope turn and
/// no approval gate stand between the POST and the pod.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_dequeued_playbook_key_dispatches_from_its_stored_row(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };

    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-1";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.5).await;

    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: created.clone(),
    }));
    let res = reconcile(&db, &cfg, key).await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let wrapper = only_wrapper(&created);
    assert!(
        wrapper.contains("--param 'depth=deep' --param 'topic=attention sinks'"),
        "the dispatch rendered the values it re-read: {wrapper}"
    );
    assert!(wrapper.contains("--max-cost 3.5"), "{wrapper}");
    assert!(wrapper.contains("--max-time 1800s"), "{wrapper}");

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Running);
    let runs = sqlx::query!(r#"SELECT run_id AS "run_id!", status AS "status!", scope FROM runs"#)
        .fetch_all(db.pool())
        .await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "running");
    assert!(runs[0].scope.is_none(), "a launch has no scope to point at");
    Ok(())
}

/// The value the test's provider hands back, distinctive enough that a leak into a dispatched
/// document is found by searching for it as literal text rather than assumed absent.
const SECRET_VALUE: &str = "ghp-test-only-4c2f9d1e";

/// Bind one secret to the playbook scope, owned by `owner`.
async fn bind_playbook_secret(db: &Db, playbook: &str, owner: &str, pack_rev: Option<&str>) {
    use crate::authz::model::Principal;
    use crate::secrets::store::{NewBinding, NewSecret};
    use crate::secrets::{
        ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, SecretName, Visibility,
    };
    let owner = Principal::parse(owner).expect("owner");
    let name = SecretName::parse("pr_token").expect("name");
    let id = uuid::Uuid::now_v7().to_string();
    let mut conn = db.pool().acquire().await.expect("conn");
    crate::secrets::store::insert(
        &mut conn,
        &NewSecret {
            id: &id,
            name: &name,
            owner: &owner,
            kind: SecretKind::Opaque,
            visibility: Visibility::BrokerOnly,
            consumer: ConsumerClass::Run,
            mode: SecretMode::Managed,
            vault_path: "user:wren/pr-token",
            current_version: Some(1),
            created_by: Some("wren"),
        },
    )
    .await
    .expect("register");
    crate::secrets::store::insert_binding(
        &mut conn,
        &NewBinding {
            id: &uuid::Uuid::now_v7().to_string(),
            secret_id: &id,
            scope_kind: ScopeKind::Playbook,
            scope_id: playbook,
            projection_kind: ProjectionKind::Env,
            projection: "AUTORESEARCH_PR_TOKEN",
            declared_name: &name,
            pack_rev,
            schema_digest: None,
            created_by: Some("wren"),
        },
    )
    .await
    .expect("bind");
}

/// A launch on a playbook scope whose owner the launcher covers dispatches, and leaves nothing
/// that maps a secret name to a value on the launch row or the argv.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_playbook_launch_with_a_binding_dispatches(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        // A scope that binds a secret refuses to dispatch without something that can read values;
        // this stands in for Vault and hands back the one the binding names.
        secret_provider: Some(std::sync::Arc::new(
            crate::secrets::provider::MapProvider::new([(
                "pr_token".to_string(),
                SECRET_VALUE.to_string(),
            )]),
        )),
        ..cfg_with(dir.path(), Profile::default())
    };
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-3";
    seed_registered_playbook(&db, "survey").await;
    bind_playbook_secret(&db, "survey", "user:wren", None).await;
    adopt_launch(&db, key, 3.5).await;

    let created = CreatedPods::default();
    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: created.clone(),
    }));
    let res = reconcile(&db, &cfg, key).await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Running, "{:?}", issue.parked_reason);
    // The launch row and the rendered wrapper carry the values, never a secret mapping.
    let stored: (serde_json::Value, Option<serde_json::Value>) =
        sqlx::query_as("SELECT params, launcher_groups FROM playbook_launches WHERE key = $1")
            .bind(key)
            .fetch_one(db.pool())
            .await?;
    let row = format!("{stored:?}");
    let wrapper = only_wrapper(&created);
    for doc in [&row, &wrapper] {
        assert!(!doc.contains("AUTORESEARCH_PR_TOKEN"), "{doc}");
        assert!(!doc.contains("pr_token"), "{doc}");
        assert!(
            !doc.contains(SECRET_VALUE),
            "the value itself must not reach a dispatched document: {doc}"
        );
    }
    Ok(())
}

/// A launcher who covers none of the bound owners never dispatches: the launch parks naming the
/// secret, which is also what a pin bump past the binding's revision does.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_playbook_launch_the_launcher_does_not_own_parks_with_the_secret_named(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-4";
    seed_registered_playbook(&db, "survey").await;
    bind_playbook_secret(&db, "survey", "group:/groups/team-x", None).await;
    adopt_launch(&db, key, 3.5).await;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile(&db, &cfg, key).await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked);
    let reason = issue.parked_reason.unwrap_or_default();
    assert!(reason.contains("pr_token"), "names the secret: {reason}");
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runs")
            .fetch_one(db.pool())
            .await?
            == 0,
        "nothing ran"
    );
    Ok(())
}

/// A pin bump moved the pack revision past what the binding was reviewed against, so the launch
/// parks until an owner member re-binds.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_pin_bump_past_the_bindings_revision_parks_the_launch(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(dir.path(), Profile::default())
    };
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-5";
    seed_registered_playbook(&db, "survey").await;
    // The registry row is at `deadbeef`; the binding was made against the revision before it.
    bind_playbook_secret(&db, "survey", "user:wren", Some("cafebabe")).await;
    adopt_launch(&db, key, 3.5).await;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile(&db, &cfg, key).await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked);
    let reason = issue.parked_reason.unwrap_or_default();
    assert!(
        reason.contains("cafebabe") && reason.contains("deadbeef"),
        "names both revisions: {reason}"
    );
    Ok(())
}

/// A stand-in engine for local mode: writes the argv it was given, publishes a session log where
/// `plan run --manifest` would, and exits.
fn fake_local_engine(dir: &Path, session: &str) -> PathBuf {
    let bin = dir.join("crucible-local-engine");
    crate::testing::write_exec(
        &bin,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CRUCIBLE_TEST_ARGV_OUT\"\nmkdir -p state\ncat > state/session.jsonl <<'LOG'\n{session}\nLOG\n"
        ),
    );
    bin
}

/// The local executor delivers no secrets, so a launch whose scope resolves a binding parks with
/// the refusal named instead of running silently without the secret it bound.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_local_launch_with_a_binding_parks_instead_of_running_without_it(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = fake_local_engine(dir.path(), &playbook_log("finished"));
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let cfg = ControllerCfg {
        playbook_executor: crucible_controller::PlaybookExecutor::Local,
        secret_provider: Some(std::sync::Arc::new(
            crate::secrets::provider::MapProvider::new([(
                "pr_token".to_string(),
                SECRET_VALUE.to_string(),
            )]),
        )),
        ..cfg_with(dir.path(), Profile::default())
    };
    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-b";
    seed_registered_playbook(&db, "survey").await;
    bind_playbook_secret(&db, "survey", "user:wren", None).await;
    adopt_launch(&db, key, 3.5).await;

    let res = reconcile(&db, &cfg, key).await;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    res?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked, "{:?}", issue.parked_reason);
    let reason = format!("{:?}", issue.parked_reason);
    assert!(
        reason.contains("local executor delivers no secrets"),
        "{reason}"
    );
    Ok(())
}

/// Local mode: the launch runs as a supervised subprocess with the launcher's own values and
/// ceilings, and its session lands in the same ledger rows a pod run's would — marked as the local
/// run it was.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_local_playbook_launch_runs_a_subprocess_into_the_ordinary_ledger(
    pool: PgPool,
) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let argv_out = dir.path().join("local-argv.txt");
    let bin = fake_local_engine(dir.path(), &playbook_log("finished"));
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
        std::env::set_var("CRUCIBLE_TEST_ARGV_OUT", &argv_out);
    }
    let cfg = ControllerCfg {
        playbook_executor: crucible_controller::PlaybookExecutor::Local,
        ..cfg_with(dir.path(), Profile::default())
    };

    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-9";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.5).await;

    let res = reconcile(&db, &cfg, key).await;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
        std::env::remove_var("CRUCIBLE_TEST_ARGV_OUT");
    }
    res?;

    // The supervisor folds the run in out of band, exactly as the pod watch does.
    let mut issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    for _ in 0..100 {
        if issue.status != Status::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        issue = crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("issue");
    }
    assert_eq!(
        issue.status,
        Status::Done,
        "parked: {:?}",
        issue.parked_reason
    );

    let argv: Vec<String> = std::fs::read_to_string(&argv_out)?
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        &argv[..2],
        &["plan".to_string(), "run".to_string()],
        "{argv:?}"
    );
    let params: Vec<&String> = argv
        .iter()
        .zip(argv.iter().skip(1))
        .filter(|(flag, _)| flag.as_str() == "--param")
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        params,
        vec!["depth=deep", "topic=attention sinks"],
        "{argv:?}"
    );
    assert!(argv.iter().any(|a| a == "3.5"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "30m"), "{argv:?}");

    let runs = sqlx::query!(
        r#"SELECT status AS "status!", pod, dispatch AS "dispatch!", cost_usd FROM runs"#
    )
    .fetch_all(db.pool())
    .await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].dispatch, "local", "the UI can say it ran here");
    assert_eq!(runs[0].pod, None, "a local run has no pod");
    assert_eq!(runs[0].status, "finished", "the ordinary ingest wrote it");
    assert_eq!(runs[0].cost_usd, Some(0.4), "the run's cost is booked once");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM work_pods")
            .fetch_one(db.pool())
            .await?,
        0,
        "no pod was tracked"
    );
    Ok(())
}

/// A local run whose engine dies without publishing a session parks the launch with the
/// supervisor's account, rather than leaving the row `running` forever.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_local_run_that_publishes_nothing_parks(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let bin = dir.path().join("crucible-dies");
    crate::testing::write_exec(&bin, "#!/bin/sh\necho 'boom' >&2\nexit 3\n");
    unsafe {
        std::env::set_var("CRUCIBLE_BIN", &bin);
    }
    let cfg = ControllerCfg {
        playbook_executor: crucible_controller::PlaybookExecutor::Local,
        ..cfg_with(dir.path(), Profile::default())
    };

    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-a";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.5).await;
    let res = reconcile(&db, &cfg, key).await;
    unsafe {
        std::env::remove_var("CRUCIBLE_BIN");
    }
    res?;

    let mut issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    for _ in 0..100 {
        if issue.status != Status::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        issue = crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("issue");
    }
    assert_eq!(issue.status, Status::Parked);
    let reason = issue.parked_reason.unwrap_or_default();
    assert!(
        reason.contains("ran locally and published no session"),
        "{reason}"
    );
    assert!(reason.contains("boom"), "{reason}");
    let runs = sqlx::query!(r#"SELECT status AS "status!" FROM runs"#)
        .fetch_all(db.pool())
        .await?;
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status, "no-session",
        "the run row settles with the launch"
    );
    Ok(())
}

/// The supervisor lives in the controller's process, so a restart mid-run loses the child. The
/// startup sweep ingests whatever session it had written and settles the rest, instead of leaving
/// the launch at `running` forever.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_restart_adopts_the_local_runs_it_orphaned(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = ControllerCfg {
        playbook_executor: crucible_controller::PlaybookExecutor::Local,
        ..cfg_with(dir.path(), Profile::default())
    };

    let published = "playbook:survey:0199c0de-7c2c-71a5-8000-b";
    let silent = "playbook:survey:0199c0de-7c2c-71a5-8000-c";
    seed_registered_playbook(&db, "survey").await;
    for key in [published, silent] {
        adopt_launch(&db, key, 3.5).await;
        crate::issues::store::claim_issue(db.pool(), key, Status::New, Status::Running).await?;
        let run_id = crucible_controller::runs::model::new_run_id(key);
        crate::runs::store::insert_run(
            db.pool(),
            &crucible_controller::NewRun {
                run_id: run_id.clone(),
                scope: None,
                issue: Some(key.to_string()),
                identity_digest: None,
                status: "running".to_string(),
                pod: None,
                session_uri: None,
                best_score: None,
                cost_usd: None,
            },
        )
        .await?;
        sqlx::query("UPDATE runs SET dispatch = 'local' WHERE run_id = $1")
            .bind(&run_id)
            .execute(db.pool())
            .await?;
        if key == published {
            let state = cfg
                .scratch_root()
                .join("local-runs")
                .join(crucible_controller::model::sanitize_key(&run_id))
                .join("pack")
                .join("state");
            std::fs::create_dir_all(&state)?;
            std::fs::write(state.join("session.jsonl"), playbook_log("finished"))?;
        }
    }

    crucible_controller::runs::local_run::adopt_orphans(&db, &cfg).await?;

    let done = crate::issues::store::get_issue(db.pool(), published)
        .await?
        .expect("issue");
    assert_eq!(done.status, Status::Done, "the written session was folded");
    let parked = crate::issues::store::get_issue(db.pool(), silent)
        .await?
        .expect("issue");
    assert_eq!(parked.status, Status::Parked);
    assert!(
        parked
            .parked_reason
            .unwrap_or_default()
            .contains("the controller restarted"),
        "the sweep says why"
    );
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM runs ORDER BY seq")
        .fetch_all(db.pool())
        .await?;
    assert_eq!(statuses, vec!["finished", "no-session"]);
    Ok(())
}

/// A launch whose stored row went missing parks instead of dispatching on guessed values.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_playbook_launch_without_its_row_parks(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let cfg = cfg_with(dir.path(), Profile::default());

    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-2";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.0).await;
    sqlx::query("DELETE FROM playbook_launches WHERE key = $1")
        .bind(key)
        .execute(db.pool())
        .await?;

    reconcile(&db, &cfg, key).await?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(issue.status, Status::Parked);
    assert_eq!(issue.parked_by, Some(crate::model::ParkedBy::Machine));
    assert_eq!(
        issue.parked_reason.as_deref().map(ParkReason::parse),
        Some(ParkReason::PlaybookLaunchMissing)
    );
    Ok(())
}

/// The concurrency cap declines a launch the same way it declines a loop run: the row stays at
/// `new` and re-drives when a slot frees, with a `capped` ledger row for the audit.
#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
async fn a_capped_playbook_launch_stays_new(pool: PgPool) -> Result<()> {
    let _g = crate::ENV_LOCK.lock().await;
    let (db, dir) = db_with(pool);
    let profile = crate::testing::fixtures::write_deploy_profile(dir.path());
    let cfg = ControllerCfg {
        deploy_profile: Some(profile),
        ..cfg_with(
            dir.path(),
            Profile {
                max_concurrent_pods: 1,
                ..Profile::default()
            },
        )
    };

    // One issue already running fills the single slot.
    crate::issues::store::upsert_issue(db.pool(), &sample_issue("owner/repo#31")).await?;
    assert!(
        crate::issues::store::claim_issue(db.pool(), "owner/repo#31", Status::New, Status::Running)
            .await?
    );

    let key = "playbook:survey:0199c0de-7c2c-71a5-8000-3";
    seed_registered_playbook(&db, "survey").await;
    adopt_launch(&db, key, 3.0).await;

    crate::runs::workpod::install_dispatcher(std::sync::Arc::new(RunPodDispatcher {
        phase: crate::runs::workpod::TurnPhase::Succeeded,
        logs: String::new(),
        created: Default::default(),
    }));
    let res = reconcile(&db, &cfg, key).await;
    crate::runs::workpod::reset_dispatcher();
    res?;

    let issue = crate::issues::store::get_issue(db.pool(), key)
        .await?
        .expect("issue");
    assert_eq!(
        issue.status,
        Status::New,
        "capped launches wait, they never park"
    );
    Ok(())
}
