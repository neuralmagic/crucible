//! Highlighting for pack files: rune-kit's scanners by file extension, Starlark through its
//! Python scanner, and a TOML scanner of our own.

use std::ops::Range;

use rune_kit::syntax::{self, Highlight, Highlighter, Paint, Syntax};

/// The language a pack file is written in, named as `highlighter` expects.
pub fn language(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, ext)| ext)?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "star" | "bzl" => "starlark",
        "toml" => "toml",
        "py" => "python",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" => "sh",
        "rs" => "rust",
        "js" | "mjs" => "js",
        "ts" => "ts",
        "sql" => "sql",
        "diff" | "patch" => "diff",
        _ => return None,
    })
}

pub fn highlighter() -> Highlighter {
    Highlighter::new(|lang, code| match lang {
        Some("toml") => toml(code),
        Some("starlark") => syntax::builtin(Some("python"), code),
        _ => syntax::builtin(lang, code),
    })
}

/// The highlights falling in `line`, a byte range of the source, clipped to it and moved to
/// start from the line's first byte. `highlights` are in order and do not overlap.
pub fn on_line(highlights: &[Highlight], line: Range<usize>) -> Vec<Highlight> {
    let first = highlights.partition_point(|(r, _)| r.end <= line.start);
    highlights[first..]
        .iter()
        .take_while(|(r, _)| r.start < line.end)
        .filter_map(|(r, paint)| {
            let start = r.start.max(line.start);
            let end = r.end.min(line.end);
            (start < end).then(|| (start - line.start..end - line.start, *paint))
        })
        .collect()
}

/// Each line's byte range in `source`, without its line ending.
pub fn lines(source: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (at, _) in source.match_indices('\n') {
        let end = if source[..at].ends_with('\r') {
            at - 1
        } else {
            at
        };
        out.push(start..end);
        start = at + 1;
    }
    if start < source.len() {
        out.push(start..source.len());
    }
    out
}

/// TOML: table headers, keys, strings, values and comments.
fn toml(code: &str) -> Vec<Highlight> {
    let s = code.as_bytes();
    let mut out: Vec<Highlight> = Vec::new();
    let mut push = |range: Range<usize>, kind: Syntax| out.push((range, Paint::Kind(kind)));
    let mut nest: Vec<u8> = Vec::new();
    let mut key = true;
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        let start = i;
        match b {
            b'\n' => {
                i += 1;
                if nest.is_empty() {
                    key = true;
                }
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' => {
                i = line_end(s, i);
                push(start..i, Syntax::Comment);
            }
            b'[' if key && nest.is_empty() => {
                i = line_end(s, i);
                let close = s[start..i]
                    .iter()
                    .rposition(|&c| c == b']')
                    .map_or(i, |at| start + at + 1);
                push(start..close, Syntax::Type);
                i = close;
                key = false;
            }
            b'"' | b'\'' => {
                i = string_end(s, i);
                push(
                    start..i,
                    if key {
                        Syntax::Property
                    } else {
                        Syntax::String
                    },
                );
            }
            b'=' => {
                i += 1;
                push(start..i, Syntax::Punctuation);
                key = false;
            }
            b'.' if key => {
                i += 1;
                push(start..i, Syntax::Punctuation);
            }
            b'[' | b'{' => {
                i += 1;
                push(start..i, Syntax::Punctuation);
                nest.push(b);
                key = b == b'{';
            }
            b']' | b'}' => {
                i += 1;
                push(start..i, Syntax::Punctuation);
                nest.pop();
                key = false;
            }
            b',' => {
                i += 1;
                push(start..i, Syntax::Punctuation);
                key = nest.last() == Some(&b'{');
            }
            _ if key && is_bare_key(b) => {
                while i < s.len() && is_bare_key(s[i]) {
                    i += 1;
                }
                push(start..i, Syntax::Property);
            }
            _ if b.is_ascii() => {
                while i < s.len() && is_value(s[i]) {
                    i += 1;
                }
                if i == start {
                    i += 1;
                    continue;
                }
                let word = &s[start..i];
                let kind = match word {
                    b"true" | b"false" | b"inf" | b"nan" => Syntax::Number,
                    _ if word[0].is_ascii_digit() || matches!(word[0], b'+' | b'-') => {
                        Syntax::Number
                    }
                    _ => continue,
                };
                push(start..i, kind);
            }
            _ => i += 1,
        }
    }
    out
}

fn line_end(s: &[u8], from: usize) -> usize {
    s[from..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(s.len(), |at| from + at)
}

/// Just past the string starting at `from`: basic or literal, single- or triple-quoted. An
/// unclosed single-line string ends at the line's end.
fn string_end(s: &[u8], from: usize) -> usize {
    let quote = s[from];
    let triple = s.get(from..from + 3) == Some(&[quote; 3][..]);
    let mut i = from + if triple { 3 } else { 1 };
    while i < s.len() {
        let c = s[i];
        if c == b'\\' && quote == b'"' {
            i += 2;
            continue;
        }
        if c == quote {
            if !triple {
                return i + 1;
            }
            if s.get(i..i + 3) == Some(&[quote; 3][..]) {
                let mut end = i + 3;
                while end < s.len() && s[end] == quote && end < i + 5 {
                    end += 1;
                }
                return end;
            }
        }
        if c == b'\n' && !triple {
            return i;
        }
        i += 1;
    }
    s.len().min(i)
}

fn is_bare_key(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_value(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.' | b':')
}

#[cfg(test)]
mod tests {
    use rune_kit::syntax::{Highlight, Paint, Syntax};

    use crate::syntax::{highlighter, language, lines, on_line, toml};

    fn kinds(code: &str, found: &[Highlight]) -> Vec<(String, Syntax)> {
        found
            .iter()
            .map(|(r, p)| {
                let Paint::Kind(k) = p else { panic!("{p:?}") };
                (code[r.clone()].to_string(), *k)
            })
            .collect()
    }

    fn of(code: &str) -> Vec<(String, Syntax)> {
        kinds(code, &toml(code))
    }

    fn pairs(list: &[(&str, Syntax)]) -> Vec<(String, Syntax)> {
        list.iter().map(|(t, k)| (t.to_string(), *k)).collect()
    }

    #[test]
    fn languages_follow_the_extension() {
        assert_eq!(language("workflow.star"), Some("starlark"));
        assert_eq!(language("lib/rules.BZL"), Some("starlark"));
        assert_eq!(language("crucible.toml"), Some("toml"));
        assert_eq!(language("tools/fix.sh"), Some("sh"));
        assert_eq!(language("prompts/edit.md"), None);
        assert_eq!(language("Makefile"), None);
    }

    #[test]
    fn toml_tables_keys_and_values() {
        use Syntax::*;
        let code =
            "# pack\n[agent.sandbox.\"big box\"]\nname = \"triage\" # why\nmax = 3\nok = true\n";
        assert_eq!(
            of(code),
            pairs(&[
                ("# pack", Comment),
                ("[agent.sandbox.\"big box\"]", Type),
                ("name", Property),
                ("=", Punctuation),
                ("\"triage\"", String),
                ("# why", Comment),
                ("max", Property),
                ("=", Punctuation),
                ("3", Number),
                ("ok", Property),
                ("=", Punctuation),
                ("true", Number),
            ])
        );
    }

    #[test]
    fn toml_inline_tables_and_arrays_know_keys_from_values() {
        use Syntax::*;
        let code = "a.b = { x = 1, 'y' = [2, \"z\"] }\n[[steps]]\n";
        assert_eq!(
            of(code),
            pairs(&[
                ("a", Property),
                (".", Punctuation),
                ("b", Property),
                ("=", Punctuation),
                ("{", Punctuation),
                ("x", Property),
                ("=", Punctuation),
                ("1", Number),
                (",", Punctuation),
                ("'y'", Property),
                ("=", Punctuation),
                ("[", Punctuation),
                ("2", Number),
                (",", Punctuation),
                ("\"z\"", String),
                ("]", Punctuation),
                ("}", Punctuation),
                ("[[steps]]", Type),
            ])
        );
    }

    #[test]
    fn toml_multiline_values_span_lines_and_arrays_keep_values() {
        use Syntax::*;
        let code = "doc = \"\"\"\nkey = \"no\"\n\"\"\"\nlist = [\n  \"a\",\n  # c\n]\nnext = 1979-05-27T07:32:00Z\n";
        assert_eq!(
            of(code),
            pairs(&[
                ("doc", Property),
                ("=", Punctuation),
                ("\"\"\"\nkey = \"no\"\n\"\"\"", String),
                ("list", Property),
                ("=", Punctuation),
                ("[", Punctuation),
                ("\"a\"", String),
                (",", Punctuation),
                ("# c", Comment),
                ("]", Punctuation),
                ("next", Property),
                ("=", Punctuation),
                ("1979-05-27T07:32:00Z", Number),
            ])
        );
    }

    #[test]
    fn toml_escapes_and_unclosed_strings_stay_on_their_line() {
        use Syntax::*;
        assert_eq!(
            of("a = \"x\\\"y\"\nb = 'open\nc = 2"),
            pairs(&[
                ("a", Property),
                ("=", Punctuation),
                ("\"x\\\"y\"", String),
                ("b", Property),
                ("=", Punctuation),
                ("'open", String),
                ("c", Property),
                ("=", Punctuation),
                ("2", Number),
            ])
        );
        assert!(toml("").is_empty());
        assert!(!toml("é = \"ü\"\n").is_empty());
    }

    #[test]
    fn starlark_colours_through_the_python_scanner() {
        let code = "load(\"//lib\", \"x\")\ndef run(ctx):\n    return task(\"a\", n = 2)  # go\n";
        let found = highlighter().highlight(Some("starlark"), code);
        let got = kinds(code, &found);
        for want in [
            ("def", Syntax::Keyword),
            ("\"//lib\"", Syntax::String),
            ("task", Syntax::Function),
            ("2", Syntax::Number),
            ("# go", Syntax::Comment),
        ] {
            assert!(
                got.contains(&(want.0.to_string(), want.1)),
                "{want:?} in {got:?}"
            );
        }
        assert!(highlighter().highlight(None, code).is_empty());
    }

    #[test]
    fn lines_split_without_their_endings() {
        assert_eq!(lines("a\r\nbc\n\nd"), vec![0..1, 3..5, 6..6, 7..8]);
        assert_eq!(lines("a\n"), vec![0..1]);
        assert!(lines("").is_empty());
    }

    #[test]
    fn highlights_clip_to_each_line() {
        let code = "x = \"\"\"\nab\n\"\"\" # c";
        let found = toml(code);
        let per: Vec<Vec<_>> = lines(code)
            .into_iter()
            .map(|line| {
                on_line(&found, line.clone())
                    .into_iter()
                    .map(|(r, _)| code[line.start..][r].to_string())
                    .collect()
            })
            .collect();
        assert_eq!(
            per,
            vec![
                vec!["x".to_string(), "=".into(), "\"\"\"".into()],
                vec!["ab".into()],
                vec!["\"\"\"".into(), "# c".into()],
            ]
        );
    }
}
