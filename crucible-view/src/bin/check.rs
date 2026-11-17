//! Checks a view on standard input the way the app does before drawing it: a view, or the
//! `{"view": ...}` or `{"error": ...}` a plugin answers with. Prints what it found and exits
//! nonzero when the app would not draw it.

use std::io::Read;
use std::process::ExitCode;

use crucible_view::{Limits, Reply, View};

fn checked(input: &str) -> Result<String, String> {
    let view = match serde_json::from_str::<Reply>(input) {
        Ok(Reply::View(view)) => view,
        Ok(Reply::Error(error)) => {
            return Err(format!("the plugin answered with an error: {error}"));
        }
        Err(_) => serde_json::from_str::<View>(input).map_err(|e| format!("not a view: {e}"))?,
    };
    view.check(&Limits::DEFAULT)
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
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("reading standard input: {error}");
        return ExitCode::from(2);
    }
    match checked(&input) {
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
    use crate::checked;

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
}
