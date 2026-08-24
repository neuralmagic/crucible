//! Deserializing the wire types from text.

use serde::de::DeserializeOwned;

/// Parse `text` as one of the wire types.
///
/// serde's internally-tagged enums and `#[serde(flatten)]` structs replay their input through
/// `serde::__private::de::Content`, which cannot carry a `serde_json` arbitrary-precision number:
/// a float behind either construct fails as `invalid type: map, expected f64`. The `starlark`
/// dependency turns `serde_json/arbitrary_precision` on for the whole binary, so every decoder for
/// a tagged or flattened type goes through `Value`, whose own `Deserializer` reads those numbers
/// natively.
pub fn from_str<T: DeserializeOwned>(text: &str) -> serde_json::Result<T> {
    serde_json::from_value(serde_json::from_str(text)?)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Tagged {
        Priced { usd: f64 },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Flattened {
        v: u8,
        #[serde(flatten)]
        event: Tagged,
    }

    #[test]
    fn a_float_survives_a_tag_and_a_flatten() {
        let value = Flattened {
            v: 1,
            event: Tagged::Priced { usd: 5.25 },
        };
        let text = serde_json::to_string(&value).unwrap();
        assert_eq!(super::from_str::<Flattened>(&text).unwrap(), value);
    }
}
