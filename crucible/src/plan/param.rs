use serde::{Deserialize, Serialize};

/// A bound parameter value.
///
/// Stores and reloads as the JSON the value already is (`42`, `true`, `["a"]`), not as a tagged
/// wrapper: see the hand-written codec below.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Int(i32),
    Number(f64),
    Bool(bool),
    StringList(Vec<String>),
}

impl Serialize for ParamValue {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.json().serialize(serializer)
    }
}

/// Decoded by inspecting the value rather than by trying variants in order: `serde(untagged)`
/// cannot read a float back into an `f64` variant, and an integer must stay an integer instead of
/// widening to `42.0`.
impl<'de> Deserialize<'de> for ParamValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(s) => Ok(ParamValue::String(s)),
            serde_json::Value::Bool(b) => Ok(ParamValue::Bool(b)),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => i32::try_from(i)
                    .map(ParamValue::Int)
                    .map_err(|_| D::Error::custom(format!("{i} does not fit in 32 bits"))),
                None => n
                    .as_f64()
                    .map(ParamValue::Number)
                    .ok_or_else(|| D::Error::custom("a parameter number must be finite")),
            },
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|i| match i {
                    serde_json::Value::String(s) => Ok(s),
                    other => Err(D::Error::custom(format!(
                        "a list parameter holds strings, got {other}"
                    ))),
                })
                .collect::<std::result::Result<Vec<String>, _>>()
                .map(ParamValue::StringList),
            other => Err(D::Error::custom(format!(
                "a parameter value is a string, int, number, bool, or list of strings, got {other}"
            ))),
        }
    }
}

impl ParamValue {
    pub fn json(&self) -> serde_json::Value {
        match self {
            ParamValue::String(s) => serde_json::Value::String(s.clone()),
            ParamValue::Int(n) => serde_json::json!(n),
            ParamValue::Number(n) => serde_json::json!(n),
            ParamValue::Bool(b) => serde_json::Value::Bool(*b),
            ParamValue::StringList(items) => serde_json::json!(items),
        }
    }
}
