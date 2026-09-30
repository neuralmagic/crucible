//! Webhook transforms (ADR-0055): the CEL filter, deduplication key, and param map a delivery
//! settles with, and the save-time rules that bound what they cost.

use crate::playbooks::registry::FieldError;
use cel::common::ast::{EntryExpr, Expr};
use cel::objects::{Key, Map as CelMap};
use cel::parser::Parser;
use cel::{Context, IdedExpr, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// The longest expression a save accepts.
pub(crate) const MAX_EXPRESSION_BYTES: usize = 2048;

const MAX_PARSE_DEPTH: u16 = 32;

/// The stack parsing and evaluation run on.
const PARSER_STACK_BYTES: usize = 16 * 1024 * 1024;

const MAX_COMPREHENSION_NESTING: usize = 2;

/// The longest dedupe key a delivery may derive.
pub(crate) const MAX_KEY_BYTES: usize = 512;

/// The longest evaluation error a settlement records.
const MAX_REASON_CHARS: usize = 500;

/// A webhook's compiled transform.
#[derive(Debug, Clone)]
pub(crate) struct Transform {
    filter: IdedExpr,
    dedupe: IdedExpr,
    params: Vec<(String, IdedExpr)>,
}

/// What a delivery exposes to its transform.
pub(crate) struct Input<'a> {
    /// The delivery's id: unique per delivery, so a key built on it never repeats.
    pub delivery: &'a str,
    pub body: &'a serde_json::Value,
    pub headers: &'a serde_json::Map<String, serde_json::Value>,
    pub received_at: &'a str,
}

/// One delivery's transform results. An `Err` is the reason the delivery settles failed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Evaluated {
    pub filter: Result<bool, String>,
    pub dedupe: Result<String, String>,
    pub params: Result<serde_json::Map<String, serde_json::Value>, String>,
}

impl Transform {
    /// Compile a stored or submitted transform. Every refusal names its field: `filter`, `dedupe`,
    /// or `derive.<param>`.
    pub(crate) fn compile(
        filter: &str,
        dedupe: &str,
        derive: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, Vec<FieldError>> {
        let mut refused = Vec::new();
        let filter = compile_field("filter", filter, &mut refused);
        let dedupe = compile_field("dedupe", dedupe, &mut refused);
        let mut compiled = Vec::with_capacity(derive.len());
        for (name, source) in derive {
            let field = format!("derive.{name}");
            match source.as_str() {
                Some(source) => {
                    if let Some(expr) = compile_field(&field, source, &mut refused) {
                        compiled.push((name.clone(), expr));
                    }
                }
                None => refused.push(FieldError {
                    field,
                    message: "an expression is a string".to_string(),
                }),
            }
        }
        match (filter, dedupe) {
            (Some(filter), Some(dedupe)) if refused.is_empty() => Ok(Transform {
                filter,
                dedupe,
                params: compiled,
            }),
            _ => Err(refused),
        }
    }

    pub(crate) fn evaluate(&self, input: &Input<'_>) -> Evaluated {
        on_parser_stack(|| self.evaluate_here(input)).unwrap_or_else(|reason| Evaluated {
            filter: Err(reason.clone()),
            dedupe: Err(reason.clone()),
            params: Err(reason),
        })
    }

    fn evaluate_here(&self, input: &Input<'_>) -> Evaluated {
        let context = scope(input);
        let filter = run(&self.filter, &context).and_then(|v| match v {
            Value::Bool(b) => Ok(b),
            other => Err(format!(
                "the filter must be a bool, got {}",
                type_name(&other)
            )),
        });
        let dedupe = run(&self.dedupe, &context).and_then(dedupe_key);
        let params = self
            .params
            .iter()
            .map(|(name, expr)| {
                run(expr, &context)
                    .and_then(param_value)
                    .map(|v| (name.clone(), v))
                    .map_err(|e| format!("derive.{name}: {e}"))
            })
            .collect();
        Evaluated {
            filter,
            dedupe,
            params,
        }
    }
}

fn compile_field(field: &str, source: &str, refused: &mut Vec<FieldError>) -> Option<IdedExpr> {
    compile_one(source)
        .map_err(|message| {
            refused.push(FieldError {
                field: field.to_string(),
                message,
            })
        })
        .ok()
}

fn compile_one(source: &str) -> Result<IdedExpr, String> {
    if source.len() > MAX_EXPRESSION_BYTES {
        return Err(format!(
            "is {} bytes; an expression is at most {MAX_EXPRESSION_BYTES}",
            source.len()
        ));
    }
    let depth = bracket_depth(source);
    if depth > usize::from(MAX_PARSE_DEPTH) {
        return Err(format!(
            "nests brackets {depth} deep; an expression nests at most {MAX_PARSE_DEPTH}"
        ));
    }
    let expr = on_parser_stack(|| {
        Parser::new()
            .max_recursion_depth(MAX_PARSE_DEPTH)
            .parse(source)
            .map_err(|e| {
                let text = e.to_string();
                text.lines().next().unwrap_or("does not parse").to_string()
            })
    })??;
    check(&expr, 0)?;
    Ok(expr)
}

/// The deepest `(`, `[`, or `{` nesting in `source`, string literals included.
fn bracket_depth(source: &str) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    for c in source.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// Run `f` on a thread with [`PARSER_STACK_BYTES`] of stack. A panic in `f` is an `Err`.
fn on_parser_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("cel".to_string())
            .stack_size(PARSER_STACK_BYTES)
            .spawn_scoped(scope, f)
            .map_err(|e| format!("could not start the evaluator: {e}"))?
            .join()
            .map_err(|_| "evaluation failed".to_string())
    })
}

/// Refuse what the interpreter cannot bound: a comprehension nested past the cap, and a loop
/// whose condition or step reaches anything but its own iteration and accumulator variables.
fn check(e: &IdedExpr, nesting: usize) -> Result<(), String> {
    match &e.expr {
        Expr::Unspecified => Err("contains an unspecified expression".to_string()),
        Expr::Struct(_) => Err("struct literals are not supported".to_string()),
        Expr::Ident(_) | Expr::Literal(_) => Ok(()),
        Expr::Select(s) => check(&s.operand, nesting),
        Expr::Call(c) => {
            if let Some(target) = &c.target {
                check(target, nesting)?;
            }
            c.args.iter().try_for_each(|a| check(a, nesting))
        }
        Expr::List(l) => l.elements.iter().try_for_each(|x| check(x, nesting)),
        Expr::Map(m) => m.entries.iter().try_for_each(|entry| match &entry.expr {
            EntryExpr::MapEntry(me) => {
                check(&me.key, nesting)?;
                check(&me.value, nesting)
            }
            EntryExpr::StructField(_) => Err("struct literals are not supported".to_string()),
        }),
        Expr::Comprehension(c) => {
            if nesting + 1 > MAX_COMPREHENSION_NESTING {
                return Err(format!(
                    "nests comprehensions more than {MAX_COMPREHENSION_NESTING} deep"
                ));
            }
            let mut own = BTreeSet::from([c.iter_var.clone(), c.accu_var.clone()]);
            if let Some(v2) = &c.iter_var2 {
                own.insert(v2.clone());
            }
            for part in [&c.loop_cond, &c.loop_step] {
                let mut free = BTreeSet::new();
                free_idents(part, &BTreeSet::new(), &mut free);
                let foreign: Vec<&String> = free.difference(&own).collect();
                if let Some(name) = foreign.first() {
                    return Err(format!(
                        "a comprehension over {} reads {name} on every iteration; a loop may read \
                         only its own variable",
                        c.iter_var
                    ));
                }
            }
            check(&c.iter_range, nesting)?;
            check(&c.accu_init, nesting)?;
            check(&c.loop_cond, nesting + 1)?;
            check(&c.loop_step, nesting + 1)?;
            check(&c.result, nesting + 1)
        }
    }
}

fn free_idents(e: &IdedExpr, bound: &BTreeSet<String>, out: &mut BTreeSet<String>) {
    match &e.expr {
        Expr::Ident(name) => {
            if !bound.contains(name) {
                out.insert(name.clone());
            }
        }
        Expr::Call(c) => {
            if let Some(target) = &c.target {
                free_idents(target, bound, out);
            }
            for a in &c.args {
                free_idents(a, bound, out);
            }
        }
        Expr::Select(s) => free_idents(&s.operand, bound, out),
        Expr::List(l) => l.elements.iter().for_each(|x| free_idents(x, bound, out)),
        Expr::Map(m) => {
            for entry in &m.entries {
                if let EntryExpr::MapEntry(me) = &entry.expr {
                    free_idents(&me.key, bound, out);
                    free_idents(&me.value, bound, out);
                }
            }
        }
        Expr::Comprehension(c) => {
            free_idents(&c.iter_range, bound, out);
            free_idents(&c.accu_init, bound, out);
            let mut inner = bound.clone();
            inner.insert(c.iter_var.clone());
            if let Some(v2) = &c.iter_var2 {
                inner.insert(v2.clone());
            }
            inner.insert(c.accu_var.clone());
            free_idents(&c.loop_cond, &inner, out);
            free_idents(&c.loop_step, &inner, out);
            free_idents(&c.result, &inner, out);
        }
        Expr::Literal(_) | Expr::Struct(_) | Expr::Unspecified => {}
    }
}

fn scope(input: &Input<'_>) -> Context<'static, 'static> {
    let mut context = Context::default();
    context.add_variable_from_value("body", to_cel(input.body));
    context.add_variable_from_value(
        "headers",
        to_cel(&serde_json::Value::Object(input.headers.clone())),
    );
    context.add_variable_from_value(
        "delivery",
        Value::String(Arc::new(input.delivery.to_string())),
    );
    context.add_variable_from_value(
        "received_at",
        Value::String(Arc::new(input.received_at.to_string())),
    );
    context
}

fn run(expr: &IdedExpr, context: &Context<'_, '_>) -> Result<Value, String> {
    Value::resolve(expr, context).map_err(|e| truncate(e.to_string()))
}

fn truncate(message: String) -> String {
    let mut message = message.replace('\0', "\u{fffd}");
    if let Some((cut, _)) = message.char_indices().nth(MAX_REASON_CHARS) {
        message.truncate(cut);
        message.push('…');
    }
    message
}

/// JSON as CEL sees it: an integral number that fits is an `int`, any other number a `double`.
fn to_cel(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(s) => Value::String(Arc::new(s.clone())),
        serde_json::Value::Array(items) => {
            Value::List(Arc::new(items.iter().map(to_cel).collect()))
        }
        serde_json::Value::Object(fields) => Value::Map(CelMap {
            map: Arc::new(
                fields
                    .iter()
                    .map(|(k, v)| (Key::String(Arc::new(k.clone())), to_cel(v)))
                    .collect::<HashMap<Key, Value>>(),
            ),
        }),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::Int(_) => "int",
        Value::UInt(_) => "uint",
        Value::Float(_) => "double",
        Value::String(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        _ => "an unsupported type",
    }
}

fn dedupe_key(v: Value) -> Result<String, String> {
    let key = match v {
        Value::String(s) => s.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        other => {
            return Err(format!(
                "the dedupe key must be a string or an integer, got {}",
                type_name(&other)
            ));
        }
    };
    if key.is_empty() {
        return Err("the dedupe key is empty".to_string());
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!(
            "the dedupe key is {} bytes; a key is at most {MAX_KEY_BYTES}",
            key.len()
        ));
    }
    if key.contains('\0') {
        return Err("the dedupe key contains a NUL character".to_string());
    }
    Ok(key)
}

/// A derived param as the overlay carries it: a string, number, bool, or list of strings.
fn param_value(v: Value) -> Result<serde_json::Value, String> {
    let text = |s: &str| {
        if s.contains('\0') {
            Err("a string contains a NUL character".to_string())
        } else {
            Ok(serde_json::Value::String(s.to_string()))
        }
    };
    match v {
        Value::String(s) => text(&s),
        Value::Int(i) => Ok(serde_json::Value::from(i)),
        Value::UInt(u) => Ok(serde_json::Value::from(u)),
        Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "a number must be finite".to_string()),
        Value::Bool(b) => Ok(serde_json::Value::Bool(b)),
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => text(s),
                other => Err(format!("a list holds strings, got {}", type_name(other))),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        other => Err(format!(
            "must be a string, number, bool, or list of strings, got {}",
            type_name(&other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::launches::webhooks::transform::{Input, MAX_EXPRESSION_BYTES, Transform};
    use serde_json::json;

    fn params(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().cloned().expect("object")
    }

    fn quay() -> serde_json::Value {
        json!({
            "repository": "neuralmagic/crucible-loop",
            "docker_url": "quay.io/neuralmagic/crucible-loop",
            "updated_tags": ["latest", "sha-2520177"],
            "manifest_digests": ["sha256:abc123"],
            "count": 1,
            "ratio": 0.5
        })
    }

    fn evaluate(
        filter: &str,
        dedupe: &str,
        derive: serde_json::Value,
        body: &serde_json::Value,
    ) -> crate::launches::webhooks::transform::Evaluated {
        let headers = params(json!({"x-github-event": "release"}));
        Transform::compile(filter, dedupe, &params(derive))
            .expect("compiles")
            .evaluate(&Input {
                delivery: "0199-delivery",
                body,
                headers: &headers,
                received_at: "2026-09-29T12:00:00Z",
            })
    }

    fn refusal(filter: &str) -> Vec<(String, String)> {
        Transform::compile(filter, "body.repository", &params(json!({})))
            .expect_err("refused")
            .into_iter()
            .map(|e| (e.field, e.message))
            .collect()
    }

    #[test]
    fn the_quay_preset_filters_keys_and_derives() {
        let out = evaluate(
            r#"body.repository == "neuralmagic/crucible-loop" && "latest" in body.updated_tags"#,
            "body.manifest_digests[0]",
            json!({
                "image": r#"body.docker_url + "@" + body.manifest_digests[0]"#,
                "tags": r#"body.updated_tags.filter(t, t.startsWith("sha-"))"#,
                "next": "body.count + 1",
                "half": "body.ratio",
                "event": r#"headers["x-github-event"]"#,
                "when": "received_at",
                "id": "delivery",
            }),
            &quay(),
        );
        assert_eq!(out.filter, Ok(true));
        assert_eq!(out.dedupe, Ok("sha256:abc123".to_string()));
        assert_eq!(
            out.params.expect("params"),
            params(json!({
                "image": "quay.io/neuralmagic/crucible-loop@sha256:abc123",
                "tags": ["sha-2520177"],
                "next": 2,
                "half": 0.5,
                "event": "release",
                "when": "2026-09-29T12:00:00Z",
                "id": "0199-delivery",
            })),
            "a JSON integer is an int, so arithmetic with a literal works"
        );
    }

    #[test]
    fn evaluation_failures_are_reasons_not_panics() {
        let body = quay();
        let out = evaluate(
            "body.missing",
            r#"has(body.missing) ? body.missing : """#,
            json!({"bad": "body.repository.matches(\"(((\")", "map": "body"}),
            &body,
        );
        assert!(out.filter.is_err_and(|e| e.contains("missing")));
        assert_eq!(out.dedupe, Err("the dedupe key is empty".to_string()));
        assert!(out.params.is_err());
        let typed = evaluate("1", "[1]", json!({"n": "body.count"}), &body);
        assert!(typed.filter.is_err_and(|e| e.contains("bool")));
        assert!(typed.dedupe.is_err_and(|e| e.contains("list")));
        let listy = evaluate("true", "1", json!({"xs": "[1, 2]"}), &body);
        assert_eq!(listy.dedupe, Ok("1".to_string()));
        assert!(listy.params.is_err_and(|e| e.starts_with("derive.xs")));
    }

    #[test]
    fn a_save_refuses_what_the_interpreter_cannot_bound() {
        assert_eq!(
            refusal(&"a".repeat(MAX_EXPRESSION_BYTES + 1))[0].0,
            "filter".to_string()
        );
        for (source, expect) in [
            ("body.l.map(x, body.l)", "reads body"),
            ("body.l.filter(x, body.l.exists(y, y == x))", "reads body"),
            ("[body.l, body.l].map(a, a.map(b, a))", "reads a"),
            (
                "body.items.exists(i, i.tags.exists(t, t == i.name))",
                "reads i",
            ),
            (
                "[1].exists(a, [a].exists(b, [b].exists(c, c == 1)))",
                "more than 2 deep",
            ),
            (&"(".repeat(200), "nests brackets 200 deep"),
            (
                &format!("{}1{}", "(".repeat(33), ")".repeat(33)),
                "nests brackets 33 deep",
            ),
            ("body.repository ==", "Syntax"),
        ] {
            let refused = refusal(source);
            assert_eq!(refused.len(), 1, "{source}: {refused:?}");
            assert!(
                refused[0].1.contains(expect),
                "{source}: {:?} does not mention {expect}",
                refused[0].1
            );
            assert!(!refused[0].1.contains('\n'), "one line: {:?}", refused[0].1);
        }
        for source in [
            r#""latest" in body.updated_tags"#,
            r#"body.updated_tags.filter(t, t.startsWith("sha-"))"#,
            r#"body.items.exists(i, i.tags.exists(t, t == "x"))"#,
        ] {
            assert!(
                Transform::compile(source, "body.repository", &params(json!({}))).is_ok(),
                "{source}"
            );
        }
    }

    #[test]
    fn every_refused_expression_is_named_by_its_field() {
        let refused = Transform::compile(
            "body.l.map(x, body.l)",
            "(",
            &params(json!({"image": "body.l.map(x, body)", "tag": 7, "ok": "body.repository"})),
        )
        .expect_err("refused");
        let mut fields: Vec<&str> = refused.iter().map(|e| e.field.as_str()).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            vec!["dedupe", "derive.image", "derive.tag", "filter"]
        );
    }

    #[test]
    fn a_value_postgres_cannot_store_fails_the_derivation() {
        let body = json!({"ref": "a\u{0}b", "long": "k".repeat(600)});
        let nul = evaluate(
            "true",
            "body.ref",
            json!({"r": "body.ref", "rs": "[body.ref]"}),
            &body,
        );
        assert!(nul.dedupe.is_err_and(|e| e.contains("NUL")));
        assert!(nul.params.is_err_and(|e| e.contains("NUL")));
        let long = evaluate("true", "body.long", json!({}), &body);
        assert!(long.dedupe.is_err_and(|e| e.contains("at most 512")));
        let reason = evaluate("body.ref == 1 ? true : body.missing", "1", json!({}), &body);
        assert!(reason.filter.is_err_and(|e| !e.contains('\0')));
    }
}
