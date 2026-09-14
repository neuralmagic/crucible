//! One-line summaries for the built-in tools of the lowercase-named harnesses (opencode, pi).
//! Both name their tools `bash`, `read`, `write`, `edit`, `grep`, and so on, and differ only in
//! the argument key each uses for a path, so one table serves both decoders and both transcript
//! readers.

use crate::stream_json::truncate_chars;
use serde_json::Value;

/// Char bound on a summary line.
pub const SUMMARY_CAP: usize = 200;

/// The compact `name`-specific line for a call with `args`: the command for a shell, the path for
/// a file tool, the pattern for a search. An unknown tool falls back to its arguments serialized.
pub fn summarize(name: &str, args: &Value) -> String {
    let get = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(v) = args.get(k) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                if !s.is_empty() {
                    return s;
                }
            }
        }
        String::new()
    };
    let path = || get(&["path", "filePath", "file_path", "file"]);
    let summary = match name {
        "bash" | "shell" | "powershell" => {
            let cmd = get(&["command", "cmd"]);
            let desc = get(&["description"]);
            if desc.is_empty() {
                format!("$ {cmd}")
            } else {
                format!("$ {cmd}  # {desc}")
            }
        }
        "read" => {
            let mut s = path();
            if let Some(o) = args.get("offset") {
                s.push_str(&format!(" L{o}"));
            }
            if let Some(l) = args.get("limit") {
                s.push_str(&format!(" +{l}"));
            }
            s
        }
        "write" => path(),
        "edit" => {
            let old = get(&["oldText", "oldString", "old_string"]);
            let first = old.lines().next().unwrap_or("");
            let preview: String = first.chars().take(60).collect();
            let ell = if first.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            let p = path();
            if preview.is_empty() {
                p
            } else {
                format!("{p}: {preview}{ell}")
            }
        }
        "grep" | "glob" | "find" | "ls" | "list" => {
            let pattern = get(&["pattern", "glob", "query"]);
            let p = path();
            match (pattern.is_empty(), p.is_empty()) {
                (true, true) => String::new(),
                (true, false) => p,
                (false, true) => pattern,
                (false, false) => format!("{pattern} in {p}"),
            }
        }
        "webfetch" | "fetch" => get(&["url"]),
        "task" => get(&["description", "prompt"]),
        "skill" => get(&["name", "skill"]),
        "todowrite" | "todoread" => {
            let n = args
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if n == 0 {
                String::new()
            } else {
                format!("{n} items")
            }
        }
        _ => match args {
            Value::Null => String::new(),
            Value::Object(o) if o.is_empty() => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        },
    };
    truncate_chars(&summary, SUMMARY_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shell_and_file_tools_summarize_their_key_argument() {
        assert_eq!(
            summarize(
                "bash",
                &json!({"command": "echo hi", "description": "Print hi"})
            ),
            "$ echo hi  # Print hi"
        );
        assert_eq!(summarize("bash", &json!({"command": "ls"})), "$ ls");
        assert_eq!(
            summarize(
                "read",
                &json!({"path": "src/a.rs", "offset": 10, "limit": 5})
            ),
            "src/a.rs L10 +5"
        );
        assert_eq!(
            summarize("write", &json!({"filePath": "hello.txt", "content": "x"})),
            "hello.txt"
        );
        assert_eq!(
            summarize(
                "edit",
                &json!({"path": "a.rs", "oldText": "fn a()\n{", "newText": "fn b()"})
            ),
            "a.rs: fn a()"
        );
        assert_eq!(
            summarize("grep", &json!({"pattern": "TODO", "path": "src"})),
            "TODO in src"
        );
        assert_eq!(summarize("glob", &json!({"pattern": "**/*.rs"})), "**/*.rs");
        assert_eq!(summarize("ls", &json!({"path": "."})), ".");
    }

    #[test]
    fn unknown_tools_fall_back_to_their_arguments_and_the_line_is_bounded() {
        assert_eq!(summarize("mystery", &json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(summarize("mystery", &Value::Null), "");
        let long = "x".repeat(SUMMARY_CAP * 2);
        let s = summarize("bash", &json!({"command": long}));
        assert_eq!(s.chars().count(), SUMMARY_CAP + 1);
        assert!(s.ends_with('…'));
    }
}
