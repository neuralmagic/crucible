//! A linked render opens a `tracing` span, so a caller that links the library (RFC-0004
//! C-LINKED-RENDER) gets the render inside its own trace instead of inside a subprocess whose
//! spans went nowhere. Its own binary: callsite interest is registered once per process, so a
//! test sharing a binary with renders that run unsubscribed would see nothing.

use crucible::deploy::{DeployProfile, TurnKind, TurnOpts, render_turn};
use crucible::plan::starlark::{declared_params, parent_or_cwd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

#[derive(Clone, Default)]
struct Spans(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> Layer<S> for Spans {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: Context<'_, S>,
    ) {
        struct Render(String);
        impl tracing::field::Visit for Render {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={v:?}", f.name()));
            }
        }
        let mut r = Render(attrs.metadata().name().to_string());
        attrs.record(&mut r);
        if let Ok(mut spans) = self.0.lock() {
            spans.push(r.0);
        }
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn turn_opts() -> TurnOpts {
    TurnOpts {
        kind: TurnKind::Scope,
        name: "crucible-scope-7".to_string(),
        issue: "example/router#7".to_string(),
        goal_text: Some("raise the number".to_string()),
        repo_url: "https://github.com/example/router.git".to_string(),
        repo_ref: None,
        sandbox_image: "registry.example.com/router-sandbox:latest".to_string(),
        max_cost: 2.5,
        digests: None,
        tier: None,
        gaming_refine_rounds: 1,
        skip_gaming_review: false,
        authoritative: false,
        harness: None,
        model: None,
    }
}

#[test]
fn linked_renders_open_spans_carrying_their_inputs() {
    let spans = Spans::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(spans.clone()))
        .expect("no other subscriber in this binary");

    let profile = crate_dir().join("tests/fixtures/deploy/gamma/delta/profile.toml");
    render_turn(
        &DeployProfile::load(&profile).expect("profile parses"),
        &turn_opts(),
    )
    .expect("turn render");

    let workflow = crate_dir().join("../examples/paper/workflow.star");
    let source = std::fs::read_to_string(&workflow).expect("workflow source");
    declared_params(&source, &workflow).expect("params");
    let _ = parent_or_cwd(&workflow);

    let seen = spans.0.lock().expect("span log");
    let turn = seen
        .iter()
        .find(|s| s.starts_with("render_turn"))
        .unwrap_or_else(|| panic!("no render_turn span: {seen:?}"));
    assert!(turn.contains("turn_kind=Scope"), "{turn}");
    assert!(turn.contains("issue=example/router#7"), "{turn}");
    assert!(turn.contains("pinned=false"), "{turn}");

    let params = seen
        .iter()
        .find(|s| s.starts_with("declared_params"))
        .unwrap_or_else(|| panic!("no declared_params span: {seen:?}"));
    assert!(params.contains("workflow.star"), "{params}");
}
