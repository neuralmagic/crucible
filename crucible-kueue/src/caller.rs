//! Caller identity: who a request comes from ([`Identity`], from its access token in
//! [`crate::auth`]) and who it acts as in one namespace ([`Caller`]). What it may do there is
//! Kubernetes RBAC's to decide, for the user the token names.

use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error(
        "{what} {value:?} is not a DNS-1123 label (lowercase alphanumerics and '-', 1-63 chars)"
    )]
    NotDnsLabel { what: &'static str, value: String },
    #[error(
        "{what} {value:?} is not a label value (alphanumerics, '-', '_', '.', 1-63 chars, alphanumeric at both ends)"
    )]
    NotLabelValue { what: &'static str, value: String },
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn is_label_value(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
}

macro_rules! dns_label_newtype {
    ($name:ident, $what:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, NameError> {
                if is_dns_label(value) {
                    Ok(Self(value.to_string()))
                } else {
                    Err(NameError::NotDnsLabel {
                        what: $what,
                        value: value.to_string(),
                    })
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

dns_label_newtype!(Namespace, "namespace");
dns_label_newtype!(QueueName, "local queue");

/// Run identifier stamped as a label on everything a caller creates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    pub fn parse(value: &str) -> Result<Self, NameError> {
        if is_label_value(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(NameError::NotLabelValue {
                what: "run id",
                value: value.to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who launched the run a delegated token acts for: the token's `sub`, kept as a label value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Launcher(String);

impl Launcher {
    pub fn parse(value: &str) -> Result<Self, NameError> {
        if is_label_value(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(NameError::NotLabelValue {
                what: "launcher",
                value: value.to_string(),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Refusal {
    #[error("name a namespace: this token has no default one")]
    NamespaceRequired,
    #[error(transparent)]
    Namespace(NameError),
}

/// Who a request comes from, before it names a namespace: a run (acting for its launcher) or a
/// person. Either may name any namespace per call, its token's `namespace` claim being the
/// default; the access checks decide whether it may act there.
#[derive(Debug, Clone)]
pub struct Identity {
    pub run_id: RunId,
    pub namespace: Option<Namespace>,
    /// The human the request acts for, stamped on everything it creates.
    pub launched_by: Launcher,
    /// The party presenting a run's token (the sandbox's SPIFFE ID), stamped on what it creates.
    pub actor: Option<String>,
}

impl Identity {
    /// The caller acting in `requested`, or in its default namespace when none is named.
    pub fn in_namespace(&self, requested: Option<&str>) -> Result<Caller, Refusal> {
        let namespace = match (requested, &self.namespace) {
            (Some(r), _) => Namespace::parse(r).map_err(Refusal::Namespace)?,
            (None, Some(default)) => default.clone(),
            (None, None) => return Err(Refusal::NamespaceRequired),
        };
        Ok(Caller {
            run_id: self.run_id.clone(),
            namespace,
            launched_by: self.launched_by.clone(),
        })
    }
}

/// An [`Identity`] resolved to the one namespace a call acts in.
#[derive(Debug, Clone)]
pub struct Caller {
    pub run_id: RunId,
    pub namespace: Namespace,
    pub launched_by: Launcher,
}

#[cfg(test)]
mod tests {
    use crate::caller::*;

    #[test]
    fn an_identity_acts_in_the_named_namespace_or_its_default() {
        let mut identity = Identity {
            run_id: RunId::parse("r").unwrap(),
            namespace: Some(Namespace::parse("team-a").unwrap()),
            launched_by: Launcher::parse("alice").unwrap(),
            actor: None,
        };
        assert_eq!(
            identity.in_namespace(None).unwrap().namespace.as_str(),
            "team-a"
        );
        assert_eq!(
            identity
                .in_namespace(Some("team-b"))
                .unwrap()
                .namespace
                .as_str(),
            "team-b"
        );
        assert!(matches!(
            identity.in_namespace(Some("Not_Valid")),
            Err(Refusal::Namespace(_))
        ));
        identity.namespace = None;
        assert_eq!(
            identity.in_namespace(None).unwrap_err(),
            Refusal::NamespaceRequired
        );
    }

    #[test]
    fn label_and_name_validation_matches_kubernetes_rules() {
        assert!(Namespace::parse("a-1").is_ok());
        assert!(Namespace::parse("-a").is_err());
        assert!(Namespace::parse(&"a".repeat(64)).is_err());
        assert!(RunId::parse("Run_1.x").is_ok());
        assert!(RunId::parse("_run").is_err());
        assert!(RunId::parse("").is_err());
    }
}
