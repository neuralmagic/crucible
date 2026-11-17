//! Checks a view on standard input the way the app does before drawing it: a view, or the
//! `{"view": ...}` or `{"error": ...}` a plugin answers with. Prints what it found and exits
//! nonzero when the app would not draw it.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::ExitCode;

use crucible_view::{Limits, Node, Reply, View};

fn count(node: &Node, counts: &mut BTreeMap<&'static str, usize>) {
    let name = match node {
        Node::Stack { children } => {
            for child in children {
                count(child, counts);
            }
            "stack"
        }
        Node::Tabs { tabs, .. } => {
            for tab in tabs {
                count(&tab.body, counts);
            }
            "tabs"
        }
        Node::Stats { .. } => "stats",
        Node::Bars { .. } => "bars",
        Node::Plot { .. } => "plot",
        Node::Flow { .. } => "flow",
        Node::Canvas { .. } => "canvas",
        Node::Grid { .. } => "grid",
        Node::Table { .. } => "table",
        Node::Text { .. } => "text",
        Node::Markdown { .. } => "markdown",
        Node::Mermaid { .. } => "mermaid",
    };
    *counts.entry(name).or_default() += 1;
}

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
    let mut counts = BTreeMap::new();
    count(&view.body, &mut counts);
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
