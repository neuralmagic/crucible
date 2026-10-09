//! Claims-based tool policy: CEL `validate` rules over a call's arguments, and `mutate` rules that
//! set server-owned argument paths from the caller's claims. Which tools exist, the scopes each
//! needs and the Kubernetes access each needs are the server's, declared in code
//! ([`crate::access::AccessRules`]); a tool the policy does not name runs without rules.
//!
//! ```toml
//! [[tools.workload_plan.mutate]]
//! path = "/manifest/metadata/annotations/example.dev~1owner"    # RFC 6901 JSON Pointer
//! value = "claims.sub"
//!
//! [[tools.workload_plan.mutate]]
//! path = "/namespace"
//! value = "claims.namespace"
//! when = "has(claims.namespace)"                                # otherwise the client's value stands
//!
//! [[tools.workload_scale.validate]]
//! name = "replica-cap"
//! rule = "args.replicas <= 8"
//! message = "at most 8 replicas"
//! ```
//!
//! A `tools/call` is processed as: every `when` evaluated over `claims` and the client's `args`;
//! the paths of the mutates that apply stripped from the client's arguments; each of those mutates
//! evaluated (over `claims` and the stripped `args`) and set; then each validate evaluated over the
//! final `args`. Anything other than a `true` validate result, including an evaluation error,
//! refuses the call and names the rule. Unconditional mutate paths are removed from the
//! `inputSchema` that `tools/list` advertises; conditional ones stay, since some callers set them.

use cel::{Context, Program};
use serde::Deserialize;
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("parsing the policy file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("tool {tool}: rule {rule}: {source}")]
    Compile {
        tool: String,
        rule: String,
        #[source]
        source: Box<cel::ParseErrors>,
    },
    #[error("tool {tool}: validate rule name {rule:?} is used twice")]
    DuplicateRule { tool: String, rule: String },
    #[error("tool {tool}: mutate path {path:?}: {reason}")]
    Path {
        tool: String,
        path: String,
        reason: &'static str,
    },
}

/// Why a call was refused. The `Display` text goes back to the caller.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Refusal {
    #[error("tools/call names no tool")]
    NoToolName,
    #[error("tool {tool}: arguments must be an object")]
    Arguments { tool: String },
    #[error("tool {tool} is not permitted")]
    Unlisted { tool: String },
    #[error("method {method} is not permitted")]
    Method { method: String },
    #[error("{tool} needs scope {}", missing.join(" "))]
    Scope { tool: String, missing: Vec<String> },
    #[error("tool {tool} refused by policy rule {rule}: {message}")]
    Rule {
        tool: String,
        rule: String,
        message: String,
    },
    #[error("tool {tool} refused: policy rule {rule} could not be evaluated: {error}")]
    RuleError {
        tool: String,
        rule: String,
        error: String,
    },
    #[error("{tool} refused by access check {rule}: {message}")]
    Denied {
        tool: String,
        rule: String,
        message: String,
    },
    #[error("{tool} refused: access check {rule} could not be made: {error}")]
    ReviewError {
        tool: String,
        rule: String,
        error: String,
    },
    #[error("tool {tool} refused: server-owned argument {path} could not be set: {error}")]
    Mutate {
        tool: String,
        path: String,
        error: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    tools: BTreeMap<String, ToolFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolFile {
    #[serde(default)]
    validate: Vec<ValidateFile>,
    #[serde(default)]
    mutate: Vec<MutateFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateFile {
    name: String,
    rule: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutateFile {
    path: String,
    value: String,
    #[serde(default)]
    when: Option<String>,
}

struct Validate {
    name: String,
    message: String,
    program: Program,
}

struct Mutate {
    pointer: String,
    segments: Vec<String>,
    program: Program,
    when: Option<Program>,
}

struct ToolRules {
    validate: Vec<Validate>,
    mutate: Vec<Mutate>,
}

/// Validate and mutate rules per tool. The default has none.
#[derive(Default)]
pub struct ToolPolicy {
    tools: BTreeMap<String, ToolRules>,
}

impl ToolPolicy {
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile = toml::from_str(text)?;
        let mut tools = BTreeMap::new();
        for (tool, rules) in file.tools {
            tools.insert(tool.clone(), compile_tool(&tool, rules)?);
        }
        Ok(Self { tools })
    }

    /// Apply `tool`'s mutate and validate rules to `args` for a caller with `claims`.
    pub fn apply(
        &self,
        tool: &str,
        args: &mut Map<String, Value>,
        claims: &Map<String, Value>,
    ) -> Result<(), Refusal> {
        let Some(rules) = self.tools.get(tool) else {
            return Ok(());
        };
        let rule_error = |rule: &str, error: String| Refusal::RuleError {
            tool: tool.to_string(),
            rule: rule.to_string(),
            error,
        };
        let claims_value = cel_value(&Value::Object(claims.clone()))
            .map_err(|error| rule_error("claims", error))?;
        let original = Value::Object(args.clone());
        let mut applying = Vec::new();
        for m in &rules.mutate {
            let applies = match &m.when {
                Some(when) => evaluate_bool(when, &claims_value, &original)
                    .map_err(|error| rule_error(&m.pointer, error))?,
                None => true,
            };
            if applies {
                remove_path(args, &m.segments);
                applying.push(m);
            }
        }
        let stripped = Value::Object(args.clone());
        for m in applying {
            let refuse = |error: String| Refusal::Mutate {
                tool: tool.to_string(),
                path: m.pointer.clone(),
                error,
            };
            let value = evaluate(&m.program, &claims_value, &stripped).map_err(refuse)?;
            let json = value.json().map_err(|e| refuse(e.to_string()))?;
            if !json.is_null() {
                set_path(args, &m.segments, json).map_err(refuse)?;
            }
        }
        let final_args = Value::Object(args.clone());
        for v in &rules.validate {
            match evaluate_bool(&v.program, &claims_value, &final_args) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(Refusal::Rule {
                        tool: tool.to_string(),
                        rule: v.name.clone(),
                        message: v.message.clone(),
                    });
                }
                Err(error) => return Err(rule_error(&v.name, error)),
            }
        }
        Ok(())
    }

    /// Remove every mutate path from `tool`'s advertised `inputSchema`: the property, and its
    /// entry in the parent's `required`.
    pub fn hide_injected(&self, tool: &str, input_schema: &mut Map<String, Value>) {
        let Some(rules) = self.tools.get(tool) else {
            return;
        };
        for m in rules.mutate.iter().filter(|m| m.when.is_none()) {
            hide_in_schema(input_schema, &m.segments);
        }
    }

    /// True if no tool has mutate rules, so `tools/list` needs no rewriting.
    pub fn hides_nothing(&self) -> bool {
        self.tools
            .values()
            .all(|r| r.mutate.iter().all(|m| m.when.is_some()))
    }
}

fn compile_tool(tool: &str, rules: ToolFile) -> Result<ToolRules, PolicyError> {
    let compile = |rule: &str, source: &str| {
        Program::compile(source).map_err(|e| PolicyError::Compile {
            tool: tool.to_string(),
            rule: rule.to_string(),
            source: Box::new(e),
        })
    };
    let mut names = HashSet::new();
    let mut validate = Vec::new();
    for v in rules.validate {
        if !names.insert(v.name.clone()) {
            return Err(PolicyError::DuplicateRule {
                tool: tool.to_string(),
                rule: v.name,
            });
        }
        validate.push(Validate {
            program: compile(&v.name, &v.rule)?,
            name: v.name,
            message: v.message,
        });
    }
    let mut mutate: Vec<Mutate> = Vec::new();
    for m in rules.mutate {
        let path_error = |reason| PolicyError::Path {
            tool: tool.to_string(),
            path: m.path.clone(),
            reason,
        };
        let segments = parse_pointer(&m.path)
            .ok_or_else(|| path_error("not a JSON Pointer with at least one segment"))?;
        if mutate
            .iter()
            .any(|o| o.segments.starts_with(&segments) || segments.starts_with(&o.segments))
        {
            return Err(path_error("overlaps another mutate path"));
        }
        mutate.push(Mutate {
            program: compile(&m.path, &m.value)?,
            when: m.when.as_deref().map(|w| compile(&m.path, w)).transpose()?,
            pointer: m.path,
            segments,
        });
    }
    Ok(ToolRules { validate, mutate })
}

/// RFC 6901: `/a/b~1c` -> `["a", "b/c"]`. The empty pointer (the whole document) is refused.
fn parse_pointer(pointer: &str) -> Option<Vec<String>> {
    let rest = pointer.strip_prefix('/')?;
    let segments: Vec<String> = rest
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect();
    (!segments.iter().any(String::is_empty)).then_some(segments)
}

fn remove_path(args: &mut Map<String, Value>, segments: &[String]) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut node = args;
    for segment in parents {
        match node.get_mut(segment) {
            Some(Value::Object(child)) => node = child,
            _ => return,
        }
    }
    node.remove(last);
}

fn set_path(
    args: &mut Map<String, Value>,
    segments: &[String],
    value: Value,
) -> Result<(), String> {
    let Some((last, parents)) = segments.split_last() else {
        return Err("empty path".to_string());
    };
    let mut node = args;
    for segment in parents {
        let child = node
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        node = match child {
            Value::Object(map) => map,
            other => return Err(format!("{segment} is {}, not an object", kind(other))),
        };
    }
    node.insert(last.clone(), value);
    Ok(())
}

fn hide_in_schema(schema: &mut Map<String, Value>, segments: &[String]) {
    let Some((first, rest)) = segments.split_first() else {
        return;
    };
    let Some(Value::Object(properties)) = schema.get_mut("properties") else {
        return;
    };
    if rest.is_empty() {
        properties.remove(first);
        if let Some(Value::Array(required)) = schema.get_mut("required") {
            required.retain(|r| r.as_str() != Some(first));
        }
        return;
    }
    if let Some(Value::Object(child)) = properties.get_mut(first) {
        hide_in_schema(child, rest);
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn evaluate_bool(program: &Program, claims: &cel::Value, args: &Value) -> Result<bool, String> {
    match evaluate(program, claims, args)? {
        cel::Value::Bool(b) => Ok(b),
        other => Err(format!("evaluated to {other:?}, not a bool")),
    }
}

fn evaluate(program: &Program, claims: &cel::Value, args: &Value) -> Result<cel::Value, String> {
    let args = cel_value(args)?;
    let mut context = Context::default();
    context.add_variable_from_value("claims", claims.clone());
    context.add_variable_from_value("args", args);
    program.execute(&context).map_err(|e| e.to_string())
}

/// JSON to CEL with every integer that fits `int` as `int`, so `args.replicas <= 8` compares
/// like with like (serde would make non-negative JSON integers `uint`).
fn cel_value(v: &Value) -> Result<cel::Value, String> {
    cel::to_value(CelJson(v)).map_err(|e| e.to_string())
}

struct CelJson<'a>(&'a Value);

impl Serialize for CelJson<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => s.serialize_i64(i),
                (None, Some(u), _) => s.serialize_u64(u),
                (None, None, Some(f)) => s.serialize_f64(f),
                (None, None, None) => s.serialize_none(),
            },
            Value::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&CelJson(item))?;
                }
                seq.end()
            }
            Value::Object(map) => {
                let mut out = s.serialize_map(Some(map.len()))?;
                for (k, item) in map {
                    out.serialize_entry(k, &CelJson(item))?;
                }
                out.end()
            }
            other => other.serialize(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::policy::*;
    use serde_json::json;

    const POLICY: &str = r#"
        [tools.workload_plan]

        [[tools.workload_plan.mutate]]
        path = "/manifest/metadata/annotations/crucible.dev~1actor"
        value = "has(claims.act) ? claims.act.sub : null"

        [[tools.workload_plan.mutate]]
        path = "/launched_by"
        value = "claims.sub"

        [[tools.workload_plan.validate]]
        name = "queue-allowed"
        rule = "size(claims.queues) == 0 || args.queue in claims.queues"
        message = "queue is not in this run's allowlist"

        [[tools.workload_plan.validate]]
        name = "launcher-matches"
        rule = "args.launched_by == claims.sub"
        message = "launched_by must be the caller"

        [tools.workload_scale]
        [[tools.workload_scale.validate]]
        name = "replica-cap"
        rule = "args.replicas <= 8"
        message = "at most 8 replicas"

        [tools.whoami]
    "#;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("{other}"),
        }
    }

    fn claims() -> Map<String, Value> {
        obj(json!({
            "sub": "alice",
            "act": {"sub": "spiffe://example.crucible/openshell/sandbox/s1"},
            "queues": ["crucible-e2e"],
        }))
    }

    fn plan_args() -> Map<String, Value> {
        obj(
            json!({"queue": "crucible-e2e", "manifest": {"kind": "Job", "metadata": {"name": "j"}}}),
        )
    }

    #[test]
    fn a_methods_table_is_refused_since_servers_declare_their_methods() {
        let text = "[methods.\"tasks/get\"]\nscopes = [\"kueue.read\"]\n";
        assert!(ToolPolicy::from_toml(text).is_err());
    }

    #[test]
    fn mutations_set_server_owned_paths_from_claims() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut args = plan_args();
        policy.apply("workload_plan", &mut args, &claims()).unwrap();
        assert_eq!(
            Value::Object(args),
            json!({
                "queue": "crucible-e2e",
                "launched_by": "alice",
                "manifest": {"kind": "Job", "metadata": {
                    "name": "j",
                    "annotations": {"crucible.dev/actor": "spiffe://example.crucible/openshell/sandbox/s1"},
                }},
            })
        );
    }

    #[test]
    fn client_supplied_values_at_mutate_paths_are_replaced() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut args = plan_args();
        args.insert("launched_by".into(), json!("someone-else"));
        args["manifest"]["metadata"]
            .as_object_mut()
            .unwrap()
            .insert(
                "annotations".into(),
                json!({"crucible.dev/actor": "forged", "keep": "me"}),
            );
        policy.apply("workload_plan", &mut args, &claims()).unwrap();
        assert_eq!(args["launched_by"], "alice");
        assert_eq!(
            args["manifest"]["metadata"]["annotations"],
            json!({"crucible.dev/actor": "spiffe://example.crucible/openshell/sandbox/s1", "keep": "me"})
        );
    }

    #[test]
    fn a_null_mutation_leaves_the_path_absent_even_if_the_client_set_it() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut undelegated = claims();
        undelegated.remove("act");
        let mut args = plan_args();
        args["manifest"]["metadata"]
            .as_object_mut()
            .unwrap()
            .insert(
                "annotations".into(),
                json!({"crucible.dev/actor": "forged"}),
            );
        policy
            .apply("workload_plan", &mut args, &undelegated)
            .unwrap();
        assert_eq!(args["manifest"]["metadata"]["annotations"], json!({}));
    }

    #[test]
    fn validate_sees_injected_values_and_refusals_name_the_rule() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut args = plan_args();
        args.insert("queue".into(), json!("someone-elses-queue"));
        let err = policy
            .apply("workload_plan", &mut args, &claims())
            .unwrap_err();
        assert_eq!(
            err,
            Refusal::Rule {
                tool: "workload_plan".into(),
                rule: "queue-allowed".into(),
                message: "queue is not in this run's allowlist".into(),
            }
        );
        assert_eq!(
            err.to_string(),
            "tool workload_plan refused by policy rule queue-allowed: queue is not in this run's allowlist"
        );
    }

    #[test]
    fn json_integers_compare_as_cel_ints() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let ok = |n: Value| {
            let mut args = obj(json!({"kind": "Deployment", "name": "d", "replicas": n}));
            policy.apply("workload_scale", &mut args, &claims())
        };
        ok(json!(8)).unwrap();
        ok(json!(0)).unwrap();
        assert!(matches!(ok(json!(9)), Err(Refusal::Rule { rule, .. }) if rule == "replica-cap"));
    }

    #[test]
    fn evaluation_errors_and_non_bool_results_fail_closed() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut no_replicas = obj(json!({"kind": "Deployment", "name": "d"}));
        let err = policy
            .apply("workload_scale", &mut no_replicas, &claims())
            .unwrap_err();
        assert!(
            matches!(&err, Refusal::RuleError { rule, .. } if rule == "replica-cap"),
            "{err}"
        );

        let text = r#"
            [[tools.t.validate]]
            name = "not-bool"
            rule = "args.x"
            message = "m"
        "#;
        let policy = ToolPolicy::from_toml(text).unwrap();
        let mut args = obj(json!({"x": "yes"}));
        let err = policy.apply("t", &mut args, &claims()).unwrap_err();
        assert!(
            matches!(&err, Refusal::RuleError { error, .. } if error.contains("not a bool")),
            "{err}"
        );
    }

    #[test]
    fn mutations_through_a_non_object_refuse_the_call() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut args = obj(
            json!({"queue": "crucible-e2e", "manifest": {"metadata": ["not", "an", "object"]}}),
        );
        let err = policy
            .apply("workload_plan", &mut args, &claims())
            .unwrap_err();
        assert!(
            matches!(&err, Refusal::Mutate { path, .. } if path == "/manifest/metadata/annotations/crucible.dev~1actor"),
            "{err}"
        );
    }

    #[test]
    fn a_mutation_that_errors_refuses_the_call() {
        let text = r#"
            [[tools.t.mutate]]
            path = "/owner"
            value = "claims.missing_claim"
        "#;
        let policy = ToolPolicy::from_toml(text).unwrap();
        let mut args = obj(json!({"owner": "client"}));
        let err = policy.apply("t", &mut args, &claims()).unwrap_err();
        assert!(
            matches!(&err, Refusal::Mutate { path, .. } if path == "/owner"),
            "{err}"
        );
    }

    #[test]
    fn injected_parameters_are_hidden_from_the_advertised_schema() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let mut schema = obj(json!({
            "type": "object",
            "properties": {
                "queue": {"type": "string"},
                "launched_by": {"type": "string"},
                "manifest": {"type": "object", "properties": {"metadata": {"type": "object", "properties": {
                    "name": {"type": "string"},
                    "annotations": {"type": "object", "properties": {"crucible.dev/actor": {"type": "string"}, "other": {}}, "required": ["crucible.dev/actor"]},
                }}}},
            },
            "required": ["queue", "launched_by", "manifest"],
        }));
        policy.hide_injected("workload_plan", &mut schema);
        assert_eq!(
            Value::Object(schema),
            json!({
                "type": "object",
                "properties": {
                    "queue": {"type": "string"},
                    "manifest": {"type": "object", "properties": {"metadata": {"type": "object", "properties": {
                        "name": {"type": "string"},
                        "annotations": {"type": "object", "properties": {"other": {}}, "required": []},
                    }}}},
                },
                "required": ["queue", "manifest"],
            })
        );
    }

    #[test]
    fn schemas_of_tools_without_mutations_are_untouched() {
        let policy = ToolPolicy::from_toml(POLICY).unwrap();
        let original = obj(
            json!({"type": "object", "properties": {"launched_by": {}}, "required": ["launched_by"]}),
        );
        let mut schema = original.clone();
        policy.hide_injected("workload_scale", &mut schema);
        policy.hide_injected("not_in_policy", &mut schema);
        assert_eq!(schema, original);
        assert!(!policy.hides_nothing());
        assert!(ToolPolicy::default().hides_nothing());
    }

    #[test]
    fn bad_policy_files_fail_at_load() {
        let cases = [
            ("unknown key", "[tools.t]\nscope = []"),
            ("unlisted", "unlisted = \"allow\""),
            ("scopes", "[tools.t]\nscopes = [\"kueue.read\"]"),
            (
                "authorize",
                "[[tools.t.authorize]]\nname = \"r\"\nresource = \"'pods'\"\nverb = \"'get'\"\nmessage = \"m\"",
            ),
            (
                "bad cel",
                "[[tools.t.validate]]\nname = \"r\"\nrule = \"args.(\"\nmessage = \"m\"",
            ),
            (
                "dup rule",
                "[[tools.t.validate]]\nname = \"r\"\nrule = \"true\"\nmessage = \"m\"\n[[tools.t.validate]]\nname = \"r\"\nrule = \"true\"\nmessage = \"m\"",
            ),
            (
                "empty pointer",
                "[[tools.t.mutate]]\npath = \"\"\nvalue = \"1\"",
            ),
            (
                "relative pointer",
                "[[tools.t.mutate]]\npath = \"a/b\"\nvalue = \"1\"",
            ),
            (
                "empty segment",
                "[[tools.t.mutate]]\npath = \"/a//b\"\nvalue = \"1\"",
            ),
            (
                "overlap",
                "[[tools.t.mutate]]\npath = \"/a\"\nvalue = \"1\"\n[[tools.t.mutate]]\npath = \"/a/b\"\nvalue = \"2\"",
            ),
        ];
        for (what, text) in cases {
            assert!(ToolPolicy::from_toml(text).is_err(), "{what} should fail");
        }
    }

    #[test]
    fn json_pointer_escapes_follow_rfc_6901() {
        assert_eq!(parse_pointer("/a~1b/c~0d").unwrap(), ["a/b", "c~d"]);
        assert_eq!(parse_pointer("/~01"), Some(vec!["~1".to_string()]));
    }

    const CONDITIONAL: &str = r#"
        [[tools.plan.mutate]]
        path = "/namespace"
        value = "claims.namespace"
        when = "has(claims.namespace)"
    "#;

    #[test]
    fn conditional_mutations_keep_the_clients_value_and_stay_in_the_schema() {
        let policy = ToolPolicy::from_toml(CONDITIONAL).unwrap();
        let person = obj(json!({"sub": "p", "kube_user": "alice"}));
        let mut args = obj(json!({"namespace": "team-b", "queue": "gpu", "kind": "Job"}));
        policy.apply("plan", &mut args, &person).unwrap();
        assert_eq!(args["namespace"], "team-b");
        let mut schema = obj(json!({"type": "object", "properties": {"namespace": {}}}));
        policy.hide_injected("plan", &mut schema);
        assert!(schema["properties"].get("namespace").is_some());
        assert!(policy.hides_nothing());
    }
}
