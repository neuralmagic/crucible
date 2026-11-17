//! Checks a view on standard input the way the app does before drawing it: a view, or the
//! `{"view": ...}` or `{"error": ...}` a plugin answers with. `--runs` checks it as a runs view.
//! Prints what it found and exits nonzero when the app would not draw it.

use std::io::Read;
use std::process::ExitCode;

use crucible_view::{Limits, Reply, View, ViewKind};

/// What the arguments after the program's name ask for: a view of one run, or with `--runs` a
/// runs view.
fn kind(args: &[String]) -> Result<ViewKind, String> {
    match args {
        [] => Ok(ViewKind::Run),
        [flag] if flag == "--runs" => Ok(ViewKind::Runs),
        _ => Err("usage: crucible-view-check [--runs] < view.json".to_string()),
    }
}

fn checked(input: &str, kind: ViewKind) -> Result<String, String> {
    let view = match serde_json::from_str::<Reply>(input) {
        Ok(Reply::View(view)) => view,
        Ok(Reply::Error(error)) => {
            return Err(format!("the plugin answered with an error: {error}"));
        }
        Err(_) => serde_json::from_str::<View>(input).map_err(|e| format!("not a view: {e}"))?,
    };
    view.check(kind, &Limits::DEFAULT)
        .map_err(|problem| format!("the view was refused: {problem}"))?;
    let mut counts = view.body.counts();
    for control in &view.controls {
        *counts.entry(control.kind()).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(name, n)| format!("{n} {name}"))
        .collect();
    Ok(format!("ok: \"{}\" with {}", view.title, parts.join(", ")))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kind = match kind(&args) {
        Ok(kind) => kind,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("reading standard input: {error}");
        return ExitCode::from(2);
    }
    match checked(&input, kind) {
        Ok(found) => {
            println!("{found}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_view::ViewKind;

    use crate::kind;

    fn checked(input: &str) -> Result<String, String> {
        crate::checked(input, ViewKind::Run)
    }

    #[test]
    fn a_view_or_a_plugin_s_answer_is_checked_and_summed_up() {
        let view = r#"{"title":"T","body":{"stack":{"children":[
            {"text":{"text":"hi"}},{"text":{"text":"there"}},{"stats":{"items":[]}}]}}}"#;
        assert_eq!(
            checked(view),
            Ok("ok: \"T\" with 1 stack, 1 stats, 2 text".to_string())
        );
        assert_eq!(checked(&format!(r#"{{"view":{view}}}"#)), checked(view));
        let laid_out = r#"{"title":"T","body":{"section":{"id":"s","title":"More","body":
            {"columns":{"children":[{"text":{"text":"a"}},{"stats":{"items":[]}}]}}}}}"#;
        assert_eq!(
            checked(laid_out),
            Ok("ok: \"T\" with 1 columns, 1 section, 1 stats, 1 text".to_string())
        );
        let charts = r#"{"title":"T","body":{"stack":{"children":[
            {"histogram":{"x":"days","values":[1,2,3]}},
            {"heatmap":{"columns":["a"],"rows":["r"],"values":[[1]]}}]}}}"#;
        assert_eq!(
            checked(charts),
            Ok("ok: \"T\" with 1 heatmap, 1 histogram, 1 stack".to_string())
        );
        let controlled = r#"{"title":"T","body":{"text":{"text":"a"}},"controls":[
            {"toggle":{"id":"a","label":"A","on":true}},
            {"range":{"id":"b","label":"B","min":0,"max":10,"step":1,"value":3}}]}"#;
        assert_eq!(
            checked(controlled),
            Ok("ok: \"T\" with 1 range, 1 text, 1 toggle".to_string())
        );
        let unoffered = r#"{"title":"T","body":{"text":{"text":"a"}},"controls":[
            {"select":{"id":"a","label":"A","options":[],"value":"x"}}]}"#;
        assert_eq!(
            checked(unoffered),
            Err("the view was refused: the control a has a value it does not offer".to_string())
        );
        let log = r#"{"title":"T","body":{"plot":{"kind":"line","x":"a","y":"b","y_scale":"log",
            "series":[{"label":"s","points":[[1,0]]}]}}}"#;
        assert_eq!(
            checked(log),
            Err(
                "the view was refused: the series s has a value that is not above 0 on a log axis"
                    .to_string()
            )
        );
    }

    #[test]
    fn what_the_app_would_not_draw_is_named() {
        assert_eq!(
            checked(r#"{"error":"no verdicts"}"#),
            Err("the plugin answered with an error: no verdicts".to_string())
        );
        let no_tabs = r#"{"title":"T","body":{"tabs":{"id":"t","tabs":[]}}}"#;
        assert_eq!(
            checked(no_tabs),
            Err("the view was refused: tabs t has no tabs".to_string())
        );
        let why = checked(r#"{"title":"T","body":{"script":{}}}"#).unwrap_err();
        assert!(
            why.starts_with("not a view: unknown variant `script`"),
            "{why}"
        );
    }

    #[test]
    fn a_runs_view_takes_launch_links_and_refuses_links_into_one_run() {
        let linked = |href: &str| {
            format!(
                r#"{{"title":"T","body":{{"table":{{"id":"t","columns":[{{"id":"a","title":"A"}}],
                "rows":[{{"id":"1","cells":[{{"text":"x","href":"{href}"}}]}}]}}}}}}"#
            )
        };
        for href in [
            "launch:PB-1",
            "launch:PB-1/task/apply",
            "https://example.com",
        ] {
            let ok = Ok("ok: \"T\" with 1 table".to_string());
            assert_eq!(crate::checked(&linked(href), ViewKind::Runs), ok, "{href}");
            assert_eq!(checked(&linked(href)), ok, "{href}");
        }
        for href in ["run:task/apply", "run:file/apply/verdicts.json"] {
            assert_eq!(
                crate::checked(&linked(href), ViewKind::Runs),
                Err(format!(
                    "the view was refused: {href} is a link into one run, which a runs view \
                     has none of"
                ))
            );
            assert!(checked(&linked(href)).is_ok(), "{href}");
        }
    }

    #[test]
    fn the_arguments_name_the_kind_of_view() {
        let args =
            |given: &[&str]| kind(&given.iter().map(|arg| arg.to_string()).collect::<Vec<_>>());
        assert_eq!(args(&[]), Ok(ViewKind::Run));
        assert_eq!(args(&["--runs"]), Ok(ViewKind::Runs));
        for off in [&["--run"][..], &["--runs", "--runs"], &["view.json"]] {
            assert!(args(off).unwrap_err().starts_with("usage:"), "{off:?}");
        }
    }
}
