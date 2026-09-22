//! A stand-in value for a declared parameter nobody supplied, derived from the parameter's own
//! JSON Schema so the engine binds it instead of refusing the compile.
//!
//! Binding and validation are one step in the engine: an unvalued required parameter is a compile
//! error, so a source that is otherwise perfect produces no graph until a launcher types
//! something. Compile-on-save has no launcher. The placeholder satisfies whatever the declaration
//! constrains — an enum's first choice, a string matching the declared pattern, a number inside
//! the declared range — and is `None` when nothing here can satisfy it, which leaves the engine's
//! own diagnostic as the answer.

use regex_syntax::hir::{Class, Hir, HirKind};

/// Longest placeholder this builds. A pattern that needs more than this to match is one the
/// generator gives up on.
const MAX_BYTES: usize = 256;

/// Characters preferred over a class's lowest member, so `[A-Za-z0-9._-]` yields `a` rather than
/// `-` and the placeholder reads as text.
const PREFERRED: &[char] = &['a', 'A', '0', '-', '.', '_', '/', ':'];

/// A value for `name` that satisfies `property`, or `None` when the declaration is one this does
/// not know how to satisfy.
pub fn placeholder(name: &str, property: &serde_json::Value) -> Option<String> {
    if let Some(first) = property.get("enum").and_then(|e| e.as_array()?.first()) {
        return Some(match first {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    let min = property.get("minimum").and_then(serde_json::Value::as_f64);
    let max = property.get("maximum").and_then(serde_json::Value::as_f64);
    match property.get("type").and_then(|t| t.as_str())? {
        "string" => match property.get("pattern").and_then(|p| p.as_str()) {
            Some(pattern) => from_pattern(pattern),
            None => Some(format!("<{name}>")),
        },
        "integer" => Some(format!("{}", in_range(min, max).round() as i64)),
        "number" => Some(format!("{}", in_range(min, max))),
        "boolean" => Some("false".to_string()),
        "array" => Some("[]".to_string()),
        _ => None,
    }
}

/// Zero, pulled to the nearer end of a range that excludes it.
fn in_range(min: Option<f64>, max: Option<f64>) -> f64 {
    match (min, max) {
        (Some(min), _) if min > 0.0 => min,
        (_, Some(max)) if max < 0.0 => max,
        _ => 0.0,
    }
}

/// The shortest string the pattern accepts, taking the first branch of every alternation and the
/// minimum count of every repetition. The result is matched against the pattern before it is
/// returned, so a construct the walk handles loosely (a word boundary, say) yields nothing rather
/// than a value the engine would refuse.
fn from_pattern(pattern: &str) -> Option<String> {
    let hir = regex_syntax::parse(pattern).ok()?;
    let mut out = String::new();
    emit(&hir, &mut out)?;
    regex::Regex::new(pattern)
        .ok()?
        .is_match(&out)
        .then_some(out)
}

fn emit(hir: &Hir, out: &mut String) -> Option<()> {
    if out.len() > MAX_BYTES {
        return None;
    }
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Some(()),
        HirKind::Literal(literal) => {
            out.push_str(std::str::from_utf8(&literal.0).ok()?);
            Some(())
        }
        HirKind::Class(class) => {
            out.push(pick(class)?);
            Some(())
        }
        HirKind::Repetition(repetition) => {
            if repetition.min as usize > MAX_BYTES {
                return None;
            }
            for _ in 0..repetition.min {
                emit(&repetition.sub, out)?;
            }
            Some(())
        }
        HirKind::Capture(capture) => emit(&capture.sub, out),
        HirKind::Concat(parts) => parts.iter().try_for_each(|part| emit(part, out)),
        HirKind::Alternation(branches) => branches.iter().find_map(|branch| {
            let mut taken = String::new();
            emit(branch, &mut taken)?;
            out.push_str(&taken);
            Some(())
        }),
    }
}

/// One character the class admits.
fn pick(class: &Class) -> Option<char> {
    match class {
        Class::Unicode(class) => PREFERRED
            .iter()
            .copied()
            .find(|c| {
                class
                    .ranges()
                    .iter()
                    .any(|range| range.start() <= *c && *c <= range.end())
            })
            .or_else(|| class.ranges().first().map(|range| range.start())),
        Class::Bytes(class) => PREFERRED
            .iter()
            .copied()
            .find(|c| {
                let Ok(byte) = u8::try_from(*c) else {
                    return false;
                };
                class
                    .ranges()
                    .iter()
                    .any(|range| range.start() <= byte && byte <= range.end())
            })
            .or_else(|| {
                class
                    .ranges()
                    .first()
                    .map(|range| range.start())
                    .filter(u8::is_ascii)
                    .map(char::from)
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn matching(pattern: &str) -> String {
        let property = json!({"type": "string", "pattern": pattern});
        let value = placeholder("p", &property).unwrap_or_else(|| panic!("no value for {pattern}"));
        assert!(
            regex::Regex::new(pattern)
                .expect("pattern compiles")
                .is_match(&value),
            "{value:?} does not match {pattern}"
        );
        value
    }

    #[test]
    fn a_plain_string_names_itself() {
        assert_eq!(
            placeholder("repo_url", &json!({"type": "string"})),
            Some("<repo_url>".to_string())
        );
    }

    #[test]
    fn a_pattern_yields_a_string_that_matches_it() {
        assert_eq!(
            matching(r"^https://(github\.com|gitlab\.com)/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$"),
            "https://github.com/a/a"
        );
        assert_eq!(matching(r"^[a-z]+/[a-z]+$"), "a/a");
        assert_eq!(matching(r"^v\d+\.\d+$"), "v0.0");
        matching(r"^\w{3,8}$");
        matching(r"^\bx\b$");
        matching(r"^(a|bb)*c$");
        matching(r"^.+$");
        matching(r"[0-9]");
    }

    #[test]
    fn an_unsatisfiable_pattern_yields_nothing() {
        assert_eq!(
            placeholder("p", &json!({"type": "string", "pattern": "^(?"})),
            None,
            "a pattern that does not parse has no placeholder"
        );
        assert_eq!(
            placeholder("p", &json!({"type": "string", "pattern": "^(?!x)y$"})),
            None,
            "a look-around no engine here supports has no placeholder"
        );
        assert_eq!(
            placeholder("p", &json!({"type": "string", "pattern": "^a{9999}$"})),
            None,
            "a match longer than the cap is refused"
        );
    }

    #[test]
    fn an_enum_takes_its_first_choice() {
        assert_eq!(
            placeholder("p", &json!({"type": "string", "enum": ["deep", "shallow"]})),
            Some("deep".to_string())
        );
    }

    #[test]
    fn a_number_lands_inside_its_declared_range() {
        assert_eq!(
            placeholder("p", &json!({"type": "integer"})),
            Some("0".to_string())
        );
        assert_eq!(
            placeholder(
                "p",
                &json!({"type": "integer", "minimum": 3.0, "maximum": 9.0})
            ),
            Some("3".to_string())
        );
        assert_eq!(
            placeholder("p", &json!({"type": "integer", "maximum": -4.0})),
            Some("-4".to_string())
        );
        assert_eq!(
            placeholder("p", &json!({"type": "number", "minimum": 0.5})),
            Some("0.5".to_string())
        );
    }

    #[test]
    fn the_other_declared_types_have_values_and_unknown_ones_do_not() {
        assert_eq!(
            placeholder("p", &json!({"type": "boolean"})),
            Some("false".to_string())
        );
        assert_eq!(
            placeholder("p", &json!({"type": "array", "items": {"type": "string"}})),
            Some("[]".to_string())
        );
        assert_eq!(placeholder("p", &json!({"type": "object"})), None);
        assert_eq!(placeholder("p", &json!({})), None);
    }
}
