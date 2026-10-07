//! The types a task may declare for the fields of its JSON output.
//!
//! A typed `emits` promises each field's JSON type as well as its presence. The engine checks a
//! passing attempt against it and the compiler checks the graph's consumers against it; the
//! admitted-plan event carries it so a reader sees the same contract.

use std::collections::BTreeSet;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decision::{Label, NOUL_NO, NOUL_YES, QuestionKind};
use crate::link::ExternalLink;

/// The JSON type one declared output field holds.
///
/// Written as its token (`"string"`, `"integer"`, `"number"`, `"boolean"`, `"list"`, `"object"`,
/// `"link"`, `"links"`), as a list of labels, which declares a string that is one of them, or as
/// `{"schema": "<JSON Schema text>"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    String,
    /// A number with no fractional part.
    Integer,
    /// Any JSON number, integers included.
    Number,
    Boolean,
    List,
    Object,
    /// An http(s) url naming a result outside the run: a pull request, a pushed branch, an issue.
    Link,
    /// A list of urls, each one a [`FieldType::Link`].
    Links,
    /// A string equal to one of these labels.
    OneOf(Vec<Label>),
    /// A value this JSON Schema admits.
    Schema(JsonSchema),
}

/// Why a declared field type is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldTypeError {
    NoLabels,
    DuplicateLabel { label: Label },
}

impl std::fmt::Display for FieldTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldTypeError::NoLabels => f.write_str("a label list names no labels"),
            FieldTypeError::DuplicateLabel { label } => {
                write!(f, "label {label:?} is listed twice")
            }
        }
    }
}

impl std::error::Error for FieldTypeError {}

impl FieldType {
    /// Every scalar type, by its token.
    pub const SCALARS: [(&'static str, FieldType); 8] = [
        ("string", FieldType::String),
        ("integer", FieldType::Integer),
        ("number", FieldType::Number),
        ("boolean", FieldType::Boolean),
        ("list", FieldType::List),
        ("object", FieldType::Object),
        ("link", FieldType::Link),
        ("links", FieldType::Links),
    ];

    /// The scalar type a token names.
    pub fn scalar(token: &str) -> Option<FieldType> {
        Self::SCALARS
            .iter()
            .find(|(name, _)| *name == token)
            .map(|(_, ty)| ty.clone())
    }

    /// The token of a scalar type; `None` for a label list.
    pub fn token(&self) -> Option<&'static str> {
        Self::SCALARS
            .iter()
            .find(|(_, ty)| ty == self)
            .map(|(name, _)| *name)
    }

    /// True for a type every value of which is a JSON number. A schema qualifies through its
    /// top-level `type`, unless its `const` or `enum` names a value that is not a number.
    pub fn is_numeric(&self) -> bool {
        match self {
            FieldType::Integer | FieldType::Number => true,
            FieldType::Schema(schema) => {
                schema.coarse().is_some_and(|ty| ty.is_numeric())
                    && schema
                        .finite_values()
                        .is_none_or(|values| values.iter().all(Value::is_number))
            }
            _ => false,
        }
    }

    /// Why a list of this type may hold an item that cannot name a mapped instance, which has to
    /// be a string. `None` for a schema whose items are provably strings, and for every other
    /// type, which says nothing about its items.
    pub fn item_refusal(&self) -> Option<String> {
        match self {
            FieldType::Schema(schema) => schema.item_refusal(),
            _ => None,
        }
    }

    /// The answers an output-decided route reads off a value of this type when there are finitely
    /// many: a label list's labels, and `yes`/`no` for a boolean. `None` when a value may be a
    /// string outside any set, or something that is not a label at all.
    pub fn route_answers(&self) -> Option<BTreeSet<Label>> {
        match self {
            FieldType::OneOf(labels) => Some(labels.iter().cloned().collect()),
            FieldType::Boolean => Some(noul_answers()),
            FieldType::Schema(schema) => schema
                .finite_values()?
                .iter()
                .map(|value| match value {
                    Value::String(label) => Label::new(label.as_str()).ok(),
                    Value::Bool(true) => Label::new(NOUL_YES).ok(),
                    Value::Bool(false) => Label::new(NOUL_NO).ok(),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }

    /// [`Self::route_answers`] for a question of `kind`. A score reads level labels only, so a
    /// type that may hold a boolean answers nothing.
    pub fn answers_to(&self, kind: &QuestionKind) -> Option<BTreeSet<Label>> {
        if matches!(kind, QuestionKind::Score { .. }) && self.may_be_boolean() {
            return None;
        }
        self.route_answers()
    }

    fn may_be_boolean(&self) -> bool {
        match self {
            FieldType::Boolean => true,
            FieldType::Schema(schema) => schema
                .finite_values()
                .is_some_and(|values| values.iter().any(Value::is_boolean)),
            _ => false,
        }
    }

    /// True for a type every value of which is a JSON array.
    pub fn is_list(&self) -> bool {
        match self {
            FieldType::List => true,
            FieldType::Schema(schema) => schema.coarse() == Some(FieldType::List),
            _ => false,
        }
    }

    pub fn validate(&self) -> Result<(), FieldTypeError> {
        let FieldType::OneOf(labels) = self else {
            return Ok(());
        };
        if labels.is_empty() {
            return Err(FieldTypeError::NoLabels);
        }
        let mut seen = BTreeSet::new();
        for label in labels {
            if !seen.insert(label) {
                return Err(FieldTypeError::DuplicateLabel {
                    label: label.clone(),
                });
            }
        }
        Ok(())
    }

    /// Whether `value` is of this type.
    pub fn admits(&self, value: &Value) -> bool {
        match (self, value) {
            (FieldType::String, Value::String(_))
            | (FieldType::Number, Value::Number(_))
            | (FieldType::Boolean, Value::Bool(_))
            | (FieldType::List, Value::Array(_))
            | (FieldType::Object, Value::Object(_)) => true,
            (FieldType::Integer, Value::Number(n)) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            (FieldType::OneOf(labels), Value::String(s)) => labels.iter().any(|l| l.as_str() == s),
            (FieldType::Link | FieldType::Links, _) => self.link_refusal(value).is_none(),
            (FieldType::Schema(schema), value) => schema.admits(value),
            _ => false,
        }
    }

    /// Why a value does not satisfy a `link`/`links` declaration; `None` for a value that
    /// satisfies it, and for any other declared type.
    ///
    /// This becomes a task's fail note, which is persisted and served, so it never repeats the
    /// url it refused: a url a task got wrong is the one most likely to carry a token.
    pub fn link_refusal(&self, value: &Value) -> Option<String> {
        fn one(value: &Value) -> Option<String> {
            match value {
                Value::String(url) => ExternalLink::parse(url).err().map(|e| e.to_string()),
                other => Some(format!("{} is not a url string", value_type(other))),
            }
        }
        match (self, value) {
            (FieldType::Link, value) => one(value),
            (FieldType::Links, Value::Array(items)) => items
                .iter()
                .enumerate()
                .find_map(|(at, item)| one(item).map(|why| format!("item {at}: {why}"))),
            (FieldType::Links, other) => Some(format!("{} is not a list", value_type(other))),
            _ => None,
        }
    }

    /// Every link a value declared `link`/`links` holds, in order; empty for any other type and
    /// for a value this type does not admit.
    pub fn links(&self, value: &Value) -> Vec<ExternalLink> {
        match (self, value) {
            (FieldType::Link, Value::String(url)) => ExternalLink::parse(url).into_iter().collect(),
            (FieldType::Links, Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|url| ExternalLink::parse(url).ok())
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn noul_answers() -> BTreeSet<Label> {
    [NOUL_YES, NOUL_NO]
        .into_iter()
        .filter_map(|label| Label::new(label).ok())
        .collect()
}

/// How many local `$ref` hops a compile-time reading of a schema follows before it stops proving
/// anything.
const MAX_REF_HOPS: usize = 16;

/// The schema a local `$ref` (`#` or `#/json/pointer`) names, if it names one in `root`.
fn local_ref<'a>(root: &'a Value, schema: &Value) -> Option<&'a Value> {
    let pointer = schema.get("$ref")?.as_str()?.strip_prefix('#')?;
    root.pointer(pointer)
}

fn subschemas<'a>(schema: &'a Value, keyword: &str) -> &'a [Value] {
    schema
        .get(keyword)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The finitely many values every instance of `schema` is one of, read off `const`, `enum`, or
/// `type: "boolean"`, through local `$ref`s and `allOf`. An upper bound: `None` when the schema
/// does not bound them.
fn finite_values(root: &Value, schema: &Value, hops: usize) -> Option<Vec<Value>> {
    if hops > MAX_REF_HOPS {
        return None;
    }
    if let Some(value) = schema.get("const") {
        return Some(vec![value.clone()]);
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return Some(values.clone());
    }
    if schema.get("type").and_then(Value::as_str) == Some("boolean") {
        return Some(vec![Value::Bool(true), Value::Bool(false)]);
    }
    if let Some(target) = local_ref(root, schema)
        && let Some(values) = finite_values(root, target, hops + 1)
    {
        return Some(values);
    }
    subschemas(schema, "allOf")
        .iter()
        .find_map(|member| finite_values(root, member, hops + 1))
}

/// Whether every instance of `schema` is a string: its `type` is `"string"`, its `const` or `enum`
/// holds only strings, a local `$ref` or an `allOf` member says so, or every branch of its
/// `anyOf`/`oneOf` does.
fn provably_strings(root: &Value, schema: &Value, hops: usize) -> bool {
    if hops > MAX_REF_HOPS {
        return false;
    }
    let typed_string = match schema.get("type") {
        Some(Value::String(ty)) => ty == "string",
        Some(Value::Array(types)) => !types.is_empty() && types.iter().all(|t| t == "string"),
        _ => false,
    };
    if typed_string {
        return true;
    }
    let bounded = schema
        .get("const")
        .map(|value| vec![value.clone()])
        .or_else(|| schema.get("enum").and_then(Value::as_array).cloned());
    if let Some(values) = bounded
        && !values.is_empty()
        && values.iter().all(Value::is_string)
    {
        return true;
    }
    if local_ref(root, schema).is_some_and(|target| provably_strings(root, target, hops + 1)) {
        return true;
    }
    if subschemas(schema, "allOf")
        .iter()
        .any(|member| provably_strings(root, member, hops + 1))
    {
        return true;
    }
    ["anyOf", "oneOf"].into_iter().any(|keyword| {
        let branches = subschemas(schema, keyword);
        !branches.is_empty()
            && branches
                .iter()
                .all(|branch| provably_strings(root, branch, hops + 1))
    })
}

/// A subschema as a refusal quotes it: its JSON, cut short past a few dozen characters.
fn quoted(schema: &Value) -> String {
    const SHOWN: usize = 80;
    let text = schema.to_string();
    match text.char_indices().nth(SHOWN) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// The JSON Schema (draft 2020-12) a declared value must satisfy. [`JsonSchema::new`] is the only
/// constructor, so a held schema is a valid 2020-12 document that compiles without fetching
/// anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSchema(Box<Value>);

/// Why a document is not a usable [`JsonSchema`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    NotJson { error: String },
    OtherDialect { declared: String },
    Invalid { error: String },
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::NotJson { error } => write!(f, "the schema is not JSON: {error}"),
            SchemaError::OtherDialect { declared } => write!(
                f,
                "the schema declares $schema {declared:?}; only {DRAFT_2020_12:?} is accepted, \
                 or no $schema at all"
            ),
            SchemaError::Invalid { error } => {
                write!(f, "the schema is not a valid 2020-12 JSON Schema: {error}")
            }
        }
    }
}

impl std::error::Error for SchemaError {}

/// `value` with every object's keys in sorted order, so its text is the same whether or not
/// serde_json preserves insertion order.
fn sorted_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted_keys).collect()),
        other => other,
    }
}

/// The only `$schema` a [`JsonSchema`] may declare.
pub const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// How many validation errors a refusal names before it stops.
pub const MAX_SCHEMA_ERRORS: usize = 3;

impl JsonSchema {
    pub fn new(document: Value) -> Result<JsonSchema, SchemaError> {
        if let Some(declared) = document.get("$schema").and_then(Value::as_str)
            && declared != DRAFT_2020_12
        {
            return Err(SchemaError::OtherDialect {
                declared: declared.to_owned(),
            });
        }
        jsonschema::draft202012::new(&document).map_err(|error| SchemaError::Invalid {
            error: error.to_string(),
        })?;
        Ok(JsonSchema(Box::new(sorted_keys(document))))
    }

    pub fn parse(text: &str) -> Result<JsonSchema, SchemaError> {
        let document = serde_json::from_str(text).map_err(|error| SchemaError::NotJson {
            error: error.to_string(),
        })?;
        JsonSchema::new(document)
    }

    /// The scalar type its top-level `type` names, which is what a consumer of the field may
    /// rely on. `None` when `type` is absent, a list, or `"null"`.
    pub fn coarse(&self) -> Option<FieldType> {
        match self.0.get("type").and_then(Value::as_str)? {
            "string" => Some(FieldType::String),
            "integer" => Some(FieldType::Integer),
            "number" => Some(FieldType::Number),
            "boolean" => Some(FieldType::Boolean),
            "array" => Some(FieldType::List),
            "object" => Some(FieldType::Object),
            _ => None,
        }
    }

    pub fn admits(&self, value: &Value) -> bool {
        self.refusal(value).is_none()
    }

    /// The finitely many values every instance is one of, when the schema bounds them with
    /// `const`, `enum`, or `type: "boolean"`.
    pub fn finite_values(&self) -> Option<Vec<Value>> {
        finite_values(&self.0, &self.0, 0)
    }

    /// Why an item of the array this schema admits may not be a string; `None` when every item,
    /// `prefixItems` included, provably is one.
    pub fn item_refusal(&self) -> Option<String> {
        let root = self.0.as_ref();
        for (at, prefix) in subschemas(root, "prefixItems").iter().enumerate() {
            if !provably_strings(root, prefix, 0) {
                return Some(format!(
                    "its `prefixItems[{at}]` is {}, which does not make every item a string",
                    quoted(prefix)
                ));
            }
        }
        match root.get("items") {
            None if root.get("prefixItems").is_some() => Some(
                "it declares no `items`, so an item past its `prefixItems` may be anything"
                    .to_owned(),
            ),
            None => Some("it declares no `items`, so an item may be anything".to_owned()),
            Some(Value::Bool(false)) => None,
            Some(items) if provably_strings(root, items, 0) => None,
            Some(items) => Some(format!(
                "its `items` is {}, which does not make every item a string",
                quoted(items)
            )),
        }
    }

    /// Why `value` does not satisfy the schema: the first [`MAX_SCHEMA_ERRORS`] errors, each as
    /// its instance path and message. `None` when it does.
    ///
    /// This becomes a task's fail note, which is persisted and served, so messages are masked and
    /// never repeat the value they refused.
    pub fn refusal(&self, value: &Value) -> Option<String> {
        let validator = match jsonschema::draft202012::new(&self.0) {
            Ok(validator) => validator,
            Err(error) => return Some(format!("the schema does not compile: {error}")),
        };
        let mut errors = validator.iter_errors(value);
        let shown: Vec<String> = errors
            .by_ref()
            .take(MAX_SCHEMA_ERRORS)
            .map(|error| {
                let at = error.instance_path().as_str();
                let at = if at.is_empty() { "/" } else { at };
                format!("{at}: {}", error.masked())
            })
            .collect();
        if shown.is_empty() {
            return None;
        }
        let more = if errors.next().is_some() {
            "; and more"
        } else {
            ""
        };
        Some(format!("{}{more}", shown.join("; ")))
    }
}

impl std::fmt::Display for JsonSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.coarse() {
            Some(ty) => write!(f, "schema ({ty})"),
            None => f.write_str("schema with no single top-level type"),
        }
    }
}

/// On the wire a schema is its JSON text, so a document holding `null` survives TOML.
impl Serialize for JsonSchema {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for JsonSchema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        JsonSchema::parse(&text).map_err(de::Error::custom)
    }
}

/// One path a task declares in `emits_files`, with the schema its JSON content must satisfy when
/// the declaration gave one.
///
/// Written as the bare path, or as `{"path": ..., "schema": "<JSON Schema text>"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredFile {
    pub path: String,
    pub schema: Option<JsonSchema>,
}

impl From<&str> for DeclaredFile {
    fn from(path: &str) -> Self {
        DeclaredFile {
            path: path.to_owned(),
            schema: None,
        }
    }
}

impl From<String> for DeclaredFile {
    fn from(path: String) -> Self {
        DeclaredFile { path, schema: None }
    }
}

impl DeclaredFile {
    /// Why `bytes`, this file's content after a passing attempt, break its declaration. `None`
    /// for a file with no schema and for content the schema admits.
    pub fn refusal(&self, bytes: &[u8]) -> Option<String> {
        let schema = self.schema.as_ref()?;
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(error) => {
                return Some(format!(
                    "declared file {:?} is not JSON: {error}",
                    self.path
                ));
            }
        };
        schema.refusal(&value).map(|why| {
            format!(
                "declared file {:?} does not match its schema: {why}",
                self.path
            )
        })
    }
}

#[derive(Serialize)]
struct TypedFileRef<'a> {
    path: &'a str,
    schema: &'a JsonSchema,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedFileRepr {
    path: String,
    schema: JsonSchema,
}

impl Serialize for DeclaredFile {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.schema {
            None => serializer.serialize_str(&self.path),
            Some(schema) => TypedFileRef {
                path: &self.path,
                schema,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for DeclaredFile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DeclaredFileVisitor;

        impl<'de> Visitor<'de> for DeclaredFileVisitor {
            type Value = DeclaredFile;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a workspace-relative path, or a table of path and schema")
            }

            fn visit_str<E: de::Error>(self, path: &str) -> Result<DeclaredFile, E> {
                Ok(DeclaredFile::from(path))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<DeclaredFile, A::Error> {
                let repr = TypedFileRepr::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(DeclaredFile {
                    path: repr.path,
                    schema: Some(repr.schema),
                })
            }
        }

        deserializer.deserialize_any(DeclaredFileVisitor)
    }
}

/// The JSON type of a value, in the tokens [`FieldType`] uses, for saying what arrived instead.
pub fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::OneOf(labels) => {
                let labels: Vec<&str> = labels.iter().map(Label::as_str).collect();
                write!(f, "one of {}", labels.join("|"))
            }
            FieldType::Schema(schema) => write!(f, "{schema}"),
            scalar => f.write_str(scalar.token().unwrap_or_default()),
        }
    }
}

impl Serialize for FieldType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            FieldType::OneOf(labels) => {
                let mut seq = serializer.serialize_seq(Some(labels.len()))?;
                for label in labels {
                    seq.serialize_element(label)?;
                }
                seq.end()
            }
            FieldType::Schema(schema) => SchemaRef { schema }.serialize(serializer),
            scalar => serializer.serialize_str(scalar.token().unwrap_or_default()),
        }
    }
}

impl<'de> Deserialize<'de> for FieldType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldTypeVisitor;

        impl<'de> Visitor<'de> for FieldTypeVisitor {
            type Value = FieldType;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a field type: \"string\", \"integer\", \"number\", \"boolean\", \"list\", \
                     \"object\", \"link\", \"links\", a list of labels, or a schema table",
                )
            }

            fn visit_str<E: de::Error>(self, token: &str) -> Result<FieldType, E> {
                FieldType::scalar(token)
                    .ok_or_else(|| E::invalid_value(de::Unexpected::Str(token), &self))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FieldType, A::Error> {
                let mut labels = Vec::new();
                while let Some(label) = seq.next_element::<Label>()? {
                    labels.push(label);
                }
                let ty = FieldType::OneOf(labels);
                ty.validate().map_err(de::Error::custom)?;
                Ok(ty)
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<FieldType, A::Error> {
                let repr = SchemaRepr::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(FieldType::Schema(repr.schema))
            }
        }

        deserializer.deserialize_any(FieldTypeVisitor)
    }
}

#[derive(Serialize)]
struct SchemaRef<'a> {
    schema: &'a JsonSchema,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRepr {
    schema: JsonSchema,
}

/// One declared output field on the wire: its name, and its type when the declaration gave one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmitWire {
    pub field: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<FieldType>,
}

#[cfg(test)]
mod tests {
    use crate::decision::{Label, QuestionKind};
    use crate::emits::{
        DRAFT_2020_12, DeclaredFile, EmitWire, FieldType, FieldTypeError, JsonSchema,
        MAX_SCHEMA_ERRORS, SchemaError, value_type,
    };
    use crate::link::{LinkKind, LinkProvider};
    use serde_json::json;

    fn labels(names: &[&str]) -> FieldType {
        FieldType::OneOf(names.iter().map(|n| Label::new(*n).unwrap()).collect())
    }

    #[test]
    fn each_type_admits_exactly_its_json_values() {
        let values = [
            json!(null),
            json!(true),
            json!(3),
            json!(-3),
            json!(3.0),
            json!(3.5),
            json!("high"),
            json!("other"),
            json!([1]),
            json!({"a": 1}),
        ];
        let cases: [(FieldType, [bool; 10]); 9] = [
            (
                FieldType::Link,
                [
                    false, false, false, false, false, false, false, false, false, false,
                ],
            ),
            (
                FieldType::Links,
                [
                    false, false, false, false, false, false, false, false, false, false,
                ],
            ),
            (
                FieldType::String,
                [
                    false, false, false, false, false, false, true, true, false, false,
                ],
            ),
            (
                FieldType::Integer,
                [
                    false, false, true, true, true, false, false, false, false, false,
                ],
            ),
            (
                FieldType::Number,
                [
                    false, false, true, true, true, true, false, false, false, false,
                ],
            ),
            (
                FieldType::Boolean,
                [
                    false, true, false, false, false, false, false, false, false, false,
                ],
            ),
            (
                FieldType::List,
                [
                    false, false, false, false, false, false, false, false, true, false,
                ],
            ),
            (
                FieldType::Object,
                [
                    false, false, false, false, false, false, false, false, false, true,
                ],
            ),
            (
                labels(&["high", "low"]),
                [
                    false, false, false, false, false, false, true, false, false, false,
                ],
            ),
        ];
        for (ty, expected) in cases {
            for (value, admitted) in values.iter().zip(expected) {
                assert_eq!(ty.admits(value), admitted, "{ty} vs {value}");
            }
        }
    }

    #[test]
    fn value_type_names_what_arrived() {
        assert_eq!(value_type(&json!(null)), "null");
        assert_eq!(value_type(&json!(false)), "boolean");
        assert_eq!(value_type(&json!(7)), "integer");
        assert_eq!(value_type(&json!(7.5)), "number");
        assert_eq!(value_type(&json!("x")), "string");
        assert_eq!(value_type(&json!([])), "list");
        assert_eq!(value_type(&json!({})), "object");
    }

    #[test]
    fn only_integer_and_number_are_numeric() {
        for (token, ty) in FieldType::SCALARS {
            assert_eq!(
                ty.is_numeric(),
                matches!(token, "integer" | "number"),
                "{token}"
            );
        }
        assert!(!labels(&["a"]).is_numeric());
    }

    #[test]
    fn a_label_list_must_name_distinct_labels() {
        assert_eq!(labels(&[]).validate(), Err(FieldTypeError::NoLabels));
        assert_eq!(
            labels(&["a", "b", "a"]).validate(),
            Err(FieldTypeError::DuplicateLabel {
                label: Label::new("a").unwrap()
            })
        );
        assert_eq!(labels(&["a"]).validate(), Ok(()));
        assert_eq!(FieldType::String.validate(), Ok(()));
    }

    #[test]
    fn types_round_trip_as_tokens_and_label_lists() {
        for (token, ty) in FieldType::SCALARS {
            let text = serde_json::to_string(&ty).unwrap();
            assert_eq!(text, format!("{token:?}"));
            assert_eq!(serde_json::from_str::<FieldType>(&text).unwrap(), ty);
            assert_eq!(ty.to_string(), token);
        }
        let ty = labels(&["high", "low"]);
        assert_eq!(serde_json::to_string(&ty).unwrap(), r#"["high","low"]"#);
        assert_eq!(
            serde_json::from_str::<FieldType>(r#"["high","low"]"#).unwrap(),
            ty
        );
        assert_eq!(ty.to_string(), "one of high|low");
    }

    #[test]
    fn a_malformed_type_does_not_decode() {
        for (text, needle) in [
            (r#""strng""#, "a field type"),
            (r#"[]"#, "names no labels"),
            (r#"["a", "a"]"#, "listed twice"),
            (r#"["not a label"]"#, "not an identifier"),
            (r#"3"#, "a field type"),
        ] {
            let err = serde_json::from_str::<FieldType>(text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn a_link_field_admits_one_http_url_and_a_links_field_a_list_of_them() {
        let pr = json!("https://github.com/neuralmagic/crucible/pull/9");
        assert!(FieldType::Link.admits(&pr));
        assert!(FieldType::Links.admits(&json!([pr, "https://example.com/x"])));
        assert!(FieldType::Links.admits(&json!([])));
        assert!(!FieldType::Links.admits(&pr));
        assert!(!FieldType::Link.admits(&json!(["https://example.com"])));
    }

    #[test]
    fn a_link_field_refuses_a_url_that_is_not_http_and_says_why() {
        let cases = [
            (
                FieldType::Link,
                json!("javascript:alert(1)"),
                "is not linked",
            ),
            (FieldType::Link, json!("data:text/html,x"), "is not linked"),
            (FieldType::Link, json!("not a url"), "is not a url"),
            (FieldType::Link, json!("https://"), "is not a url"),
            (FieldType::Link, json!(7), "integer is not a url string"),
            (FieldType::Link, json!(null), "null is not a url string"),
            (
                FieldType::Link,
                json!("https://user:pw@github.com/a/b"),
                "must not carry credentials",
            ),
            (FieldType::Links, json!("https://example.com"), "not a list"),
            (
                FieldType::Links,
                json!(["https://example.com", "ftp://example.com"]),
                "item 1:",
            ),
        ];
        for (ty, value, needle) in cases {
            let why = ty
                .link_refusal(&value)
                .unwrap_or_else(|| panic!("{ty} admitted {value}"));
            assert!(why.contains(needle), "{value}: {why}");
            assert!(!ty.admits(&value), "{value}");
        }
        assert_eq!(FieldType::String.link_refusal(&json!("x")), None);
    }

    /// The note this produces is persisted and served, and `output` is not, so it is the one
    /// place a token in a refused url could come to rest.
    #[test]
    fn a_refusal_does_not_repeat_the_url_it_refused() {
        for value in [
            json!("https://user:hunter2@github.com/a/b"),
            json!("http://user:hunter2@/a/b"),
            json!("https://hunter2@nope"),
        ] {
            let why = FieldType::Link
                .link_refusal(&value)
                .unwrap_or_else(|| panic!("admitted {value}"));
            assert!(!why.contains("hunter2"), "{value}: {why}");
        }
    }

    #[test]
    fn links_reads_the_parsed_links_out_of_a_declared_value() {
        let links = FieldType::Links.links(&json!([
            "https://github.com/neuralmagic/crucible/pull/9",
            "https://github.com/neuralmagic/crucible/tree/topic",
        ]));
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].provider, LinkProvider::GitHub);
        assert_eq!(links[0].kind, LinkKind::PullRequest);
        assert_eq!(links[0].label, "#9");
        assert_eq!(links[1].kind, LinkKind::Branch);
        assert_eq!(links[1].label, "topic");

        let one = FieldType::Link.links(&json!("https://acme.atlassian.net/browse/ABC-1"));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].provider, LinkProvider::Jira);

        assert!(
            FieldType::String
                .links(&json!("https://example.com"))
                .is_empty()
        );
        assert!(FieldType::Link.links(&json!("nope")).is_empty());
    }

    #[test]
    fn an_untyped_wire_field_omits_its_type() {
        let untyped = EmitWire {
            field: "lines".into(),
            ty: None,
        };
        assert_eq!(
            serde_json::to_string(&untyped).unwrap(),
            r#"{"field":"lines"}"#
        );
        let typed = EmitWire {
            field: "severity".into(),
            ty: Some(labels(&["high", "low"])),
        };
        let text = serde_json::to_string(&typed).unwrap();
        assert_eq!(text, r#"{"field":"severity","type":["high","low"]}"#);
        assert_eq!(serde_json::from_str::<EmitWire>(&text).unwrap(), typed);
    }

    fn schema(document: serde_json::Value) -> JsonSchema {
        JsonSchema::new(document).unwrap()
    }

    fn lanes() -> JsonSchema {
        schema(json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["name", "width"],
                "properties": {
                    "name": {"type": "string"},
                    "width": {"type": "integer", "minimum": 1}
                }
            }
        }))
    }

    #[test]
    fn a_schema_field_admits_exactly_what_its_schema_admits() {
        let ty = FieldType::Schema(lanes());
        assert!(ty.admits(&json!([])));
        assert!(ty.admits(&json!([{"name": "a", "width": 2}])));
        assert!(!ty.admits(&json!([{"name": "a", "width": 0}])));
        assert!(!ty.admits(&json!([{"name": "a"}])));
        assert!(!ty.admits(&json!({"name": "a", "width": 2})));
        assert!(!ty.admits(&json!(null)));
    }

    #[test]
    fn a_schema_refusal_names_each_instance_path_and_its_error() {
        let why = lanes()
            .refusal(&json!([{"name": "a", "width": 2}, {"name": 7, "width": 0}]))
            .unwrap();
        assert!(why.contains("/1/name: "), "{why}");
        assert!(why.contains("/1/width: "), "{why}");
        assert!(!why.contains("/0"), "{why}");
        let root = lanes().refusal(&json!("x")).unwrap();
        assert!(root.starts_with("/: "), "{root}");
        assert_eq!(lanes().refusal(&json!([{"name": "a", "width": 1}])), None);
    }

    #[test]
    fn a_schema_refusal_stops_after_a_few_errors() {
        let many: Vec<_> = (0..10).map(|_| json!({"name": 1, "width": 0})).collect();
        let why = lanes().refusal(&json!(many)).unwrap();
        assert_eq!(why.matches(": ").count(), MAX_SCHEMA_ERRORS, "{why}");
        assert!(why.ends_with("; and more"), "{why}");
    }

    /// The note this produces is persisted and served, and `output` is not.
    #[test]
    fn a_schema_refusal_does_not_repeat_the_value_it_refused() {
        let why = schema(json!({"type": "object", "properties": {"token": {"type": "integer"}}}))
            .refusal(&json!({"token": "hunter2"}))
            .unwrap();
        assert!(why.contains("/token"), "{why}");
        assert!(!why.contains("hunter2"), "{why}");
        let why = schema(json!({"enum": ["a", "b"]}))
            .refusal(&json!("hunter2"))
            .unwrap();
        assert!(!why.contains("hunter2"), "{why}");
    }

    #[test]
    fn a_schema_takes_its_coarse_type_from_its_top_level_type() {
        for (ty, coarse) in [
            ("string", Some(FieldType::String)),
            ("integer", Some(FieldType::Integer)),
            ("number", Some(FieldType::Number)),
            ("boolean", Some(FieldType::Boolean)),
            ("array", Some(FieldType::List)),
            ("object", Some(FieldType::Object)),
            ("null", None),
        ] {
            assert_eq!(schema(json!({"type": ty})).coarse(), coarse, "{ty}");
        }
        assert_eq!(schema(json!({"type": ["string", "null"]})).coarse(), None);
        assert_eq!(schema(json!({"minimum": 1})).coarse(), None);
        assert_eq!(schema(json!(true)).coarse(), None);

        assert!(FieldType::Schema(schema(json!({"type": "integer"}))).is_numeric());
        assert!(FieldType::Schema(schema(json!({"type": "number"}))).is_numeric());
        assert!(!FieldType::Schema(schema(json!({"minimum": 0}))).is_numeric());
        assert!(FieldType::Schema(lanes()).is_list());
        assert!(!FieldType::Schema(schema(json!({"items": {}}))).is_list());
        assert!(FieldType::List.is_list());
        assert!(!FieldType::Object.is_list());
    }

    #[test]
    fn a_schema_displays_its_coarse_type() {
        assert_eq!(FieldType::Schema(lanes()).to_string(), "schema (list)");
        assert_eq!(
            FieldType::Schema(schema(json!({}))).to_string(),
            "schema with no single top-level type"
        );
    }

    #[test]
    fn only_a_valid_2020_12_schema_is_a_schema() {
        assert!(matches!(
            JsonSchema::new(json!({"type": "lane"})),
            Err(SchemaError::Invalid { .. })
        ));
        assert!(matches!(
            JsonSchema::new(json!({"minLength": -1})),
            Err(SchemaError::Invalid { .. })
        ));
        assert_eq!(
            JsonSchema::new(json!({"$schema": "http://json-schema.org/draft-07/schema#"})),
            Err(SchemaError::OtherDialect {
                declared: "http://json-schema.org/draft-07/schema#".into()
            })
        );
        assert!(JsonSchema::new(json!({"$schema": DRAFT_2020_12, "type": "string"})).is_ok());
        assert!(matches!(
            JsonSchema::parse("{\"type\": "),
            Err(SchemaError::NotJson { .. })
        ));
    }

    #[test]
    fn a_remote_ref_is_refused_rather_than_fetched() {
        let refused = JsonSchema::new(json!({"$ref": "https://example.com/lanes.json"}));
        assert!(
            matches!(refused, Err(SchemaError::Invalid { .. })),
            "{refused:?}"
        );
        assert!(
            JsonSchema::new(json!({
                "$defs": {"lane": {"type": "string"}},
                "type": "array",
                "items": {"$ref": "#/$defs/lane"}
            }))
            .is_ok()
        );
    }

    #[test]
    fn a_schema_is_pinned_with_sorted_keys_at_every_depth() {
        let ty = FieldType::Schema(schema(json!({
            "type": "object",
            "properties": {"z": {"type": "string"}, "a": {"type": "integer"}},
            "required": ["z", "a"]
        })));
        assert_eq!(
            serde_json::to_string(&ty).unwrap(),
            r#"{"schema":"{\"properties\":{\"a\":{\"type\":\"integer\"},\"z\":{\"type\":\"string\"}},\"required\":[\"z\",\"a\"],\"type\":\"object\"}"}"#
        );
    }

    #[test]
    fn a_schema_type_round_trips_as_a_table_holding_its_text() {
        let ty = FieldType::Schema(schema(json!({"type": "array", "default": null})));
        let text = serde_json::to_string(&ty).unwrap();
        assert_eq!(
            text,
            r#"{"schema":"{\"default\":null,\"type\":\"array\"}"}"#
        );
        assert_eq!(serde_json::from_str::<FieldType>(&text).unwrap(), ty);

        let wire = EmitWire {
            field: "lanes".into(),
            ty: Some(ty),
        };
        let text = serde_json::to_string(&wire).unwrap();
        assert_eq!(serde_json::from_str::<EmitWire>(&text).unwrap(), wire);
    }

    #[test]
    fn a_malformed_schema_type_does_not_decode() {
        for (text, needle) in [
            (
                r#"{"schema": "{\"type\": \"lane\"}"}"#,
                "not a valid 2020-12",
            ),
            (r#"{"schema": "{"}"#, "not JSON"),
            (r#"{"schema": {"type": "array"}}"#, "string"),
            (r#"{"schema": "{}", "extra": 1}"#, "unknown field"),
            (r#"{}"#, "missing field"),
        ] {
            let err = serde_json::from_str::<FieldType>(text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn a_declared_file_is_a_bare_path_until_it_carries_a_schema() {
        let bare = DeclaredFile::from("REPORT.md");
        assert_eq!(serde_json::to_string(&bare).unwrap(), r#""REPORT.md""#);
        assert_eq!(
            serde_json::from_str::<DeclaredFile>(r#""REPORT.md""#).unwrap(),
            bare
        );
        let typed = DeclaredFile {
            path: "RESULT.json".into(),
            schema: Some(schema(json!({"type": "object"}))),
        };
        let text = serde_json::to_string(&typed).unwrap();
        assert_eq!(
            text,
            r#"{"path":"RESULT.json","schema":"{\"type\":\"object\"}"}"#
        );
        assert_eq!(serde_json::from_str::<DeclaredFile>(&text).unwrap(), typed);
        for bad in [
            r#"{"path": "a.json"}"#,
            r#"{"path": "a.json", "schema": "{}", "other": 1}"#,
            r#"7"#,
        ] {
            assert!(serde_json::from_str::<DeclaredFile>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_declared_file_refuses_content_that_is_not_json_or_breaks_its_schema() {
        let typed = DeclaredFile {
            path: "RESULT.json".into(),
            schema: Some(schema(json!({"type": "object", "required": ["ok"]}))),
        };
        assert_eq!(typed.refusal(br#"{"ok": true}"#), None);
        let why = typed.refusal(b"not json").unwrap();
        assert!(why.contains("\"RESULT.json\" is not JSON"), "{why}");
        let why = typed.refusal(br#"{"other": 1}"#).unwrap();
        assert!(
            why.contains("\"RESULT.json\" does not match its schema: /: "),
            "{why}"
        );
        assert_eq!(DeclaredFile::from("REPORT.md").refusal(b"anything"), None);
    }

    #[test]
    fn finite_values_read_const_enum_boolean_refs_and_all_of() {
        assert_eq!(
            schema(json!({"const": "a"})).finite_values(),
            Some(vec![json!("a")])
        );
        assert_eq!(
            schema(json!({"type": "string", "enum": ["a", "b"]})).finite_values(),
            Some(vec![json!("a"), json!("b")])
        );
        assert_eq!(
            schema(json!({"type": "boolean"})).finite_values(),
            Some(vec![json!(true), json!(false)])
        );
        assert_eq!(
            schema(json!({"$defs": {"t": {"enum": [1, 2]}}, "$ref": "#/$defs/t"})).finite_values(),
            Some(vec![json!(1), json!(2)])
        );
        assert_eq!(
            schema(json!({"allOf": [{"type": "string"}, {"const": "x"}]})).finite_values(),
            Some(vec![json!("x")])
        );
        assert_eq!(schema(json!({"type": "string"})).finite_values(), None);
        assert_eq!(
            schema(json!({"anyOf": [{"const": 1}, {"const": 2}]})).finite_values(),
            None
        );
    }

    #[test]
    fn items_are_proven_strings_or_the_refusal_says_what_was_found() {
        for ok in [
            json!({"type": "array", "items": {"type": "string"}}),
            json!({"type": "array", "items": {"type": ["string"]}}),
            json!({"type": "array", "items": {"enum": ["a", "b"]}}),
            json!({"type": "array", "items": {"const": "a"}}),
            json!({"type": "array", "items": {"allOf": [{"minLength": 1}, {"type": "string"}]}}),
            json!({"type": "array", "items": {"oneOf": [{"const": "a"}, {"enum": ["b"]}]}}),
            json!({"type": "array", "$defs": {"l": {"$ref": "#/$defs/m"}, "m": {"type": "string"}}, "items": {"$ref": "#/$defs/l"}}),
            json!({"type": "array", "prefixItems": [{"const": "head"}], "items": {"type": "string"}}),
            json!({"type": "array", "items": false}),
        ] {
            assert_eq!(schema(ok.clone()).item_refusal(), None, "{ok}");
        }
        for (bad, why) in [
            (
                json!({"type": "array"}),
                "it declares no `items`, so an item may be anything",
            ),
            (
                json!({"type": "array", "items": true}),
                "its `items` is true, which does not make every item a string",
            ),
            (
                json!({"type": "array", "items": {}}),
                "its `items` is {}, which",
            ),
            (
                json!({"type": "array", "items": {"type": ["string", "null"]}}),
                "does not make every item a string",
            ),
            (
                json!({"type": "array", "items": {"enum": []}}),
                "does not make every item a string",
            ),
            (
                json!({"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "integer"}]}}),
                "does not make every item a string",
            ),
            (
                json!({"type": "array", "prefixItems": [{"type": "integer"}], "items": {"type": "string"}}),
                "its `prefixItems[0]` is {\"type\":\"integer\"}",
            ),
        ] {
            let got = schema(bad.clone())
                .item_refusal()
                .unwrap_or_else(|| panic!("{bad} passed"));
            assert!(got.contains(why), "{bad}: {got}");
        }
        assert_eq!(
            FieldType::List.item_refusal(),
            None,
            "a plain list says nothing about its items"
        );
    }

    #[test]
    fn an_items_refusal_cuts_a_long_schema_short() {
        let long: Vec<i32> = (0..100).collect();
        let why = schema(json!({"type": "array", "items": {"enum": long}}))
            .item_refusal()
            .unwrap();
        assert!(why.contains('…'), "{why}");
        assert!(why.len() < 200, "{why}");
    }

    #[test]
    fn route_answers_are_the_labels_a_value_can_be() {
        let set = |names: &[&str]| -> Option<std::collections::BTreeSet<Label>> {
            Some(names.iter().map(|n| Label::new(*n).unwrap()).collect())
        };
        assert_eq!(labels(&["a", "b"]).route_answers(), set(&["a", "b"]));
        assert_eq!(FieldType::Boolean.route_answers(), set(&["no", "yes"]));
        assert_eq!(
            FieldType::Schema(schema(json!({"type": "boolean"}))).route_answers(),
            set(&["no", "yes"])
        );
        assert_eq!(
            FieldType::Schema(schema(json!({"enum": ["a", true]}))).route_answers(),
            set(&["a", "yes"])
        );
        assert_eq!(
            FieldType::Schema(schema(json!({"const": "a"}))).route_answers(),
            set(&["a"])
        );
        for none in [
            FieldType::String,
            FieldType::Integer,
            FieldType::Schema(schema(json!({"type": "string"}))),
            FieldType::Schema(schema(json!({"enum": ["a", 1]}))),
            FieldType::Schema(schema(json!({"enum": ["not a label"]}))),
        ] {
            assert_eq!(none.route_answers(), None, "{none}");
        }
    }

    #[test]
    fn a_score_reads_no_answers_off_a_type_that_may_hold_a_boolean() {
        use crate::decision::ChoiceOption;
        let levels = |names: &[&str]| QuestionKind::Score {
            levels: names
                .iter()
                .map(|n| ChoiceOption {
                    label: Label::new(*n).unwrap(),
                    description: None,
                })
                .collect(),
        };
        let score = levels(&["no", "yes"]);
        assert_eq!(FieldType::Boolean.answers_to(&score), None);
        assert_eq!(
            FieldType::Schema(schema(json!({"type": "boolean"}))).answers_to(&score),
            None
        );
        assert_eq!(
            FieldType::Schema(schema(json!({"enum": ["no", true]}))).answers_to(&score),
            None
        );
        assert_eq!(
            labels(&["no", "yes"]).answers_to(&score),
            labels(&["no", "yes"]).route_answers()
        );
        assert_eq!(
            FieldType::Schema(schema(json!({"enum": ["no"]}))).answers_to(&score),
            Some([Label::new("no").unwrap()].into_iter().collect())
        );
        assert_eq!(
            FieldType::Boolean.answers_to(&QuestionKind::Noul),
            FieldType::Boolean.route_answers()
        );
    }

    #[test]
    fn a_numeric_schema_must_not_enumerate_a_non_number() {
        assert!(FieldType::Schema(schema(json!({"type": "integer", "enum": [1, 2]}))).is_numeric());
        assert!(
            !FieldType::Schema(schema(json!({"type": "number", "enum": [1, "x"]}))).is_numeric()
        );
        assert!(!FieldType::Schema(schema(json!({"type": "number", "const": "x"}))).is_numeric());
    }
}
