//! A GraphQL request reduced to what the policy decides on: the operation that runs, and its
//! top-level fields with their arguments resolved against the request's variables. A document the
//! parser rejects, that holds several operations without naming one, or that reaches its top level
//! through a fragment is refused rather than guessed at.

use apollo_parser::Parser;
use apollo_parser::cst::{self, CstNode};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Mutation,
    Subscription,
}

/// One top-level field: its name (never its alias), its arguments, and the name of every field
/// selected beneath it, through fragments too.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub args: Map<String, Value>,
    pub nested: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub kind: Kind,
    pub fields: Vec<Field>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Unreadable {
    #[error("the GraphQL request is not JSON with a query string")]
    NotARequest,
    #[error("the GraphQL document does not parse: {0}")]
    Syntax(String),
    #[error("the GraphQL document names no operation to run among several")]
    Ambiguous,
    #[error("the GraphQL document has no operation {0:?}")]
    NoSuchOperation(String),
    #[error("the GraphQL operation reaches its top level through a fragment")]
    Fragment,
    #[error("the GraphQL document spreads a fragment it does not define, or one that recurses")]
    UnknownFragment,
}

/// The operation `body` (a GraphQL-over-HTTP JSON request) runs.
pub fn operation(body: &[u8]) -> Result<Operation, Unreadable> {
    let request: Value = serde_json::from_slice(body).map_err(|_| Unreadable::NotARequest)?;
    let query = request
        .get("query")
        .and_then(Value::as_str)
        .ok_or(Unreadable::NotARequest)?;
    let variables = request
        .get("variables")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let wanted = request.get("operationName").and_then(Value::as_str);

    let tree = Parser::new(query).parse();
    if let Some(error) = tree.errors().next() {
        return Err(Unreadable::Syntax(error.message().to_string()));
    }
    let mut operations = Vec::new();
    let mut fragments = std::collections::HashMap::new();
    for definition in tree.document().definitions() {
        match definition {
            cst::Definition::OperationDefinition(op) => operations.push(op),
            cst::Definition::FragmentDefinition(f) => {
                if let Some(name) = f.fragment_name().and_then(|n| n.name()) {
                    fragments.insert(name.text().to_string(), f);
                }
            }
            _ => {}
        }
    }
    let chosen = match (wanted, operations.as_slice()) {
        (None, [only]) => only.clone(),
        (None, _) => return Err(Unreadable::Ambiguous),
        (Some(name), ops) => ops
            .iter()
            .find(|op| op.name().is_some_and(|n| n.text() == name))
            .cloned()
            .ok_or_else(|| Unreadable::NoSuchOperation(name.to_string()))?,
    };
    let kind = match chosen.operation_type() {
        None => Kind::Query,
        Some(t) if t.mutation_token().is_some() => Kind::Mutation,
        Some(t) if t.subscription_token().is_some() => Kind::Subscription,
        Some(_) => Kind::Query,
    };
    let mut fields = Vec::new();
    for selection in chosen
        .selection_set()
        .map(|s| s.selections().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let cst::Selection::Field(field) = selection else {
            return Err(Unreadable::Fragment);
        };
        let Some(name) = field.name() else {
            continue;
        };
        let mut args = Map::new();
        for arg in field.arguments().into_iter().flat_map(|a| a.arguments()) {
            if let (Some(n), Some(v)) = (arg.name(), arg.value()) {
                args.insert(n.text().to_string(), value(&v, &variables));
            }
        }
        let mut nested = Vec::new();
        if let Some(set) = field.selection_set() {
            collect(&set, &fragments, &mut Vec::new(), &mut nested)?;
        }
        fields.push(Field {
            name: name.text().to_string(),
            args,
            nested,
        });
    }
    Ok(Operation { kind, fields })
}

/// Every field name selected in `set`, following inline fragments and named ones; `seen` guards
/// against a fragment that spreads itself.
fn collect(
    set: &cst::SelectionSet,
    fragments: &std::collections::HashMap<String, cst::FragmentDefinition>,
    seen: &mut Vec<String>,
    out: &mut Vec<String>,
) -> Result<(), Unreadable> {
    for selection in set.selections() {
        match selection {
            cst::Selection::Field(f) => {
                if let Some(name) = f.name() {
                    out.push(name.text().to_string());
                }
                if let Some(inner) = f.selection_set() {
                    collect(&inner, fragments, seen, out)?;
                }
            }
            cst::Selection::InlineFragment(frag) => {
                if let Some(inner) = frag.selection_set() {
                    collect(&inner, fragments, seen, out)?;
                }
            }
            cst::Selection::FragmentSpread(spread) => {
                let name = spread
                    .fragment_name()
                    .and_then(|n| n.name())
                    .map(|n| n.text().to_string())
                    .ok_or(Unreadable::UnknownFragment)?;
                if seen.contains(&name) {
                    return Err(Unreadable::UnknownFragment);
                }
                let def = fragments.get(&name).ok_or(Unreadable::UnknownFragment)?;
                seen.push(name);
                if let Some(inner) = def.selection_set() {
                    collect(&inner, fragments, seen, out)?;
                }
                seen.pop();
            }
        }
    }
    Ok(())
}

/// A literal argument as JSON, a variable as its value in the request.
fn value(v: &cst::Value, variables: &Map<String, Value>) -> Value {
    match v {
        cst::Value::Variable(var) => var
            .name()
            .and_then(|n| variables.get(n.text().as_str()).cloned())
            .unwrap_or(Value::Null),
        cst::Value::StringValue(s) => Value::String(String::from(s)),
        cst::Value::BooleanValue(b) => Value::Bool(b.true_token().is_some()),
        cst::Value::IntValue(i) => i
            .syntax()
            .text()
            .to_string()
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or(Value::Null),
        cst::Value::ListValue(list) => {
            Value::Array(list.values().map(|v| value(&v, variables)).collect())
        }
        cst::Value::ObjectValue(obj) => Value::Object(
            obj.object_fields()
                .filter_map(|f| Some((f.name()?.text().to_string(), value(&f.value()?, variables))))
                .collect(),
        ),
        cst::Value::EnumValue(e) => e
            .name()
            .map(|n| Value::String(n.text().to_string()))
            .unwrap_or(Value::Null),
        cst::Value::FloatValue(_) | cst::Value::NullValue(_) => Value::Null,
    }
}
