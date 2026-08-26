//! A rejected workflow tells a consumer where it broke as data. An editor placing a marker, or a
//! controller storing a finding, reads the anchor and the message off the error itself instead of
//! parsing them back out of the rendered text.

use std::path::Path;

use crucible::plan::starlark::{CompileError, read_params};

fn rejected(source: &str) -> CompileError {
    read_params(source, Path::new("workflow.star")).expect_err("refused")
}

#[test]
fn a_parse_failure_keeps_the_span_it_was_found_at() {
    let error = rejected("params = {\n    \"repo\": {\n");
    let anchor = error.anchor().expect("the parser said where it gave up");
    assert_eq!(anchor.file, "workflow.star");
    assert!(anchor.span.begin_line >= 1, "1-based: {anchor:?}");
    assert!(
        anchor.span.begin_column >= 1 && anchor.span.end_column >= 1,
        "1-based: {anchor:?}"
    );
    assert!(
        matches!(error, CompileError::Parse { .. }),
        "{}",
        error.message()
    );
    assert!(!error.message().is_empty());
}

/// The anchor is data, not a substring of the message: nothing has to re-parse `file:line:col`
/// back out of rendered prose.
#[test]
fn the_message_and_the_anchor_are_separate() {
    let error = rejected("workflow(name = \"x\")\nparams = {}\n");
    assert!(
        matches!(error, CompileError::ParamsNotFirst),
        "{}",
        error.message()
    );
    assert_eq!(
        error.message(),
        "params must be the source's first statement"
    );
    assert!(!error.message().contains("workflow.star"));
}

#[test]
fn an_anchor_survives_a_round_trip_as_data() {
    let anchor = rejected("params = {\n    \"repo\": {\n")
        .anchor()
        .expect("anchored");
    let encoded = serde_json::to_string(&anchor).expect("serialize");
    let decoded: crucible::plan::starlark::SourceAnchor =
        serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, anchor);
}

/// An unanchored error says so rather than inventing a location.
#[test]
fn an_unlocated_error_has_no_anchor() {
    let error = rejected("params = 3\n");
    assert!(matches!(error, CompileError::ParamsNotLiteral { .. }));
    assert!(error.anchor().is_none());
    assert!(error.message().contains("dictionary literal"));
}
