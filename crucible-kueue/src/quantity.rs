//! Resource quantity arithmetic on top of `kube_quantity`.

use kube_quantity::ParsedQuantity;
use std::collections::BTreeMap;

pub fn zero() -> ParsedQuantity {
    ParsedQuantity::try_from("0").unwrap_or_default()
}

pub fn serialize<S: serde::Serializer>(q: &ParsedQuantity, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&q.to_string())
}

pub fn add_into(map: &mut BTreeMap<String, ParsedQuantity>, key: &str, value: ParsedQuantity) {
    let next = match map.remove(key) {
        Some(prev) => prev + value,
        None => value,
    };
    map.insert(key.to_string(), next);
}

#[cfg(test)]
mod tests {
    use crate::quantity::*;

    fn q(s: &str) -> ParsedQuantity {
        ParsedQuantity::try_from(s).unwrap()
    }

    #[test]
    fn mixed_format_arithmetic_is_exact() {
        assert_eq!(zero() + q("1Gi"), q("1Gi"));
        assert_eq!(q("1Gi") + zero(), q("1Gi"));
        assert_eq!(zero() + q("500m") + q("500m"), q("1"));
        assert_eq!(q("500m") * 2u32, q("1"));
        assert_eq!(q("1Gi") * 2u32, q("2Gi"));
        assert_eq!(q("64Gi") - q("6748Mi"), q("58788Mi"));
        assert_eq!(q("4230m") + q("2"), q("6230m"));
        assert!(q("1G") < q("1Gi"));
        assert!(q("16Ti") >= q("6748Mi"));
        assert!(q("9951709180Ki") > zero());
        assert_eq!(zero() + q("9951709180Ki"), q("9951709180Ki"));
        assert_eq!(q("47") - q("6"), q("41"));
        assert!(q("100m") <= q("1722"));
        assert!(q("64Mi") <= q("9951709180Ki") - zero());
    }

    #[test]
    fn add_into_starts_from_the_first_value() {
        let mut m = BTreeMap::new();
        add_into(&mut m, "memory", q("1Gi"));
        add_into(&mut m, "memory", q("1Gi"));
        add_into(&mut m, "cpu", q("500m"));
        add_into(&mut m, "cpu", q("500m"));
        assert_eq!(m["memory"], q("2Gi"));
        assert_eq!(m["cpu"], q("1"));
    }
}
