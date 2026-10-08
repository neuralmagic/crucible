//! The authenticated caller, as handlers and policy see it.

use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Deepest `act` nesting accepted. Each level is one delegation hop.
const MAX_ACTOR_DEPTH: usize = 8;

/// Who is calling, derived from a validated token's claims.
///
/// `subject` is on whose behalf the call is made. `actor_chain` is RFC 8693's `act` claim
/// flattened: the first entry is the party presenting the token (a sandbox's SPIFFE ID), each
/// later entry the party that delegated to the one before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub subject: String,
    pub actor_chain: Vec<String>,
    /// `azp`: the OAuth client the token was issued to.
    pub client_id: Option<String>,
    pub namespace: Option<String>,
    pub scopes: BTreeSet<String>,
    /// Every claim, as policy rules see them under `claims`.
    pub claims: Map<String, Value>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClaimsError {
    #[error("claim {0} is missing or not a string")]
    NotString(&'static str),
    #[error("claim act at depth {0} has no string sub")]
    ActorSubject(usize),
    #[error("claim act nests deeper than {MAX_ACTOR_DEPTH}")]
    ActorDepth,
    #[error("claim scope must be a space-separated string")]
    Scope,
}

impl Principal {
    pub fn from_claims(claims: Map<String, Value>) -> Result<Self, ClaimsError> {
        let subject = claims
            .get("sub")
            .and_then(Value::as_str)
            .ok_or(ClaimsError::NotString("sub"))?
            .to_string();
        let actor_chain = actor_chain(claims.get("act"))?;
        let client_id = optional_string(&claims, "azp")?;
        let namespace = optional_string(&claims, "namespace")?;
        let scopes = match claims.get("scope") {
            None => BTreeSet::new(),
            Some(Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
            Some(_) => return Err(ClaimsError::Scope),
        };
        Ok(Self {
            subject,
            actor_chain,
            client_id,
            namespace,
            scopes,
            claims,
        })
    }

    /// The party presenting the token: the innermost actor when delegated, else the subject.
    pub fn presenter(&self) -> &str {
        self.actor_chain.first().unwrap_or(&self.subject)
    }

    /// The scopes in `required` this principal lacks.
    pub fn missing_scopes<'a>(&self, required: &'a [String]) -> Vec<&'a str> {
        required
            .iter()
            .filter(|s| !self.scopes.contains(*s))
            .map(String::as_str)
            .collect()
    }
}

fn optional_string(
    claims: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<String>, ClaimsError> {
    match claims.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(ClaimsError::NotString(key)),
    }
}

fn actor_chain(mut act: Option<&Value>) -> Result<Vec<String>, ClaimsError> {
    let mut chain = Vec::new();
    while let Some(level) = act {
        if chain.len() == MAX_ACTOR_DEPTH {
            return Err(ClaimsError::ActorDepth);
        }
        let sub = level
            .get("sub")
            .and_then(Value::as_str)
            .ok_or(ClaimsError::ActorSubject(chain.len()))?;
        chain.push(sub.to_string());
        act = level.get("act");
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use crate::principal::*;
    use serde_json::json;

    fn claims(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn a_delegated_token_names_the_human_as_subject_and_the_sandbox_as_presenter() {
        let p = Principal::from_claims(claims(json!({
            "sub": "alice",
            "azp": "run-auth-spike",
            "namespace": "team-e2e",
            "scope": "kueue.read  kueue.submit",
            "act": {"sub": "spiffe://example.crucible/openshell/sandbox/s1",
                    "act": {"sub": "spiffe://example.crucible/controller"}},
        })))
        .unwrap();
        assert_eq!(p.subject, "alice");
        assert_eq!(
            p.actor_chain,
            [
                "spiffe://example.crucible/openshell/sandbox/s1",
                "spiffe://example.crucible/controller"
            ]
        );
        assert_eq!(
            p.presenter(),
            "spiffe://example.crucible/openshell/sandbox/s1"
        );
        assert_eq!(p.client_id.as_deref(), Some("run-auth-spike"));
        assert_eq!(p.namespace.as_deref(), Some("team-e2e"));
        assert_eq!(
            p.scopes.iter().map(String::as_str).collect::<Vec<_>>(),
            ["kueue.read", "kueue.submit"]
        );
        assert_eq!(p.claims["azp"], "run-auth-spike");
    }

    #[test]
    fn an_undelegated_token_presents_as_its_subject() {
        let p = Principal::from_claims(claims(json!({"sub": "alice"}))).unwrap();
        assert!(p.actor_chain.is_empty());
        assert_eq!(p.presenter(), "alice");
        assert!(p.scopes.is_empty());
        assert_eq!(p.namespace, None);
    }

    #[test]
    fn malformed_identity_claims_are_refused() {
        let cases = [
            (json!({}), ClaimsError::NotString("sub")),
            (json!({"sub": 7}), ClaimsError::NotString("sub")),
            (
                json!({"sub": "a", "act": {"iss": "x"}}),
                ClaimsError::ActorSubject(0),
            ),
            (
                json!({"sub": "a", "act": {"sub": "b", "act": "c"}}),
                ClaimsError::ActorSubject(1),
            ),
            (json!({"sub": "a", "scope": ["x"]}), ClaimsError::Scope),
            (json!({"sub": "a", "azp": 1}), ClaimsError::NotString("azp")),
            (
                json!({"sub": "a", "namespace": {}}),
                ClaimsError::NotString("namespace"),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(
                Principal::from_claims(claims(input.clone())),
                Err(want),
                "{input}"
            );
        }
    }

    #[test]
    fn actor_nesting_is_bounded() {
        let mut act = json!({"sub": "leaf"});
        for i in 0..MAX_ACTOR_DEPTH {
            act = json!({"sub": format!("a{i}"), "act": act});
        }
        let err = Principal::from_claims(claims(json!({"sub": "s", "act": act}))).unwrap_err();
        assert_eq!(err, ClaimsError::ActorDepth);
    }

    #[test]
    fn missing_scopes_lists_only_what_is_absent() {
        let p = Principal::from_claims(claims(json!({"sub": "a", "scope": "kueue.read"}))).unwrap();
        let required = ["kueue.read".to_string(), "kueue.submit".to_string()];
        assert_eq!(p.missing_scopes(&required), ["kueue.submit"]);
    }
}
