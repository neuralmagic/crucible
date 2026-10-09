//! Caller identity: who a request comes from ([`Identity`], from its access token in
//! [`crate::auth`]) and who it acts as in one namespace ([`Caller`]). Nothing a tool receives in its
//! arguments can widen that scope.

use crucible_contract::kueue::TenantRole;
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
    #[error("this token is bound to namespace {pinned:?}, not {requested:?}")]
    NamespacePinned { pinned: String, requested: String },
    #[error("name a namespace: this token has no default one")]
    NamespaceRequired,
    #[error(transparent)]
    Namespace(NameError),
    #[error("role {role:?} may not call {tool}; it needs the submitter role")]
    Role {
        role: TenantRole,
        tool: &'static str,
    },
    #[error("local queue {queue:?} is not in this run's allowlist {allowed:?}")]
    Queue { queue: String, allowed: Vec<String> },
}

/// Who a request comes from, before it names a namespace. A run's or tenant's identity is pinned
/// to one namespace; a person's picks one per call (authorized by the tool policy's access checks)
/// and may have a default.
#[derive(Debug, Clone)]
pub struct Identity {
    pub run_id: RunId,
    pub namespace: Option<Namespace>,
    pub pinned: bool,
    pub role: TenantRole,
    pub queues: Vec<QueueName>,
    pub launched_by: Option<Launcher>,
    /// The party presenting a run's token (the sandbox's SPIFFE ID), stamped on what it creates.
    pub actor: Option<String>,
}

impl Identity {
    /// A caller fixed to `caller`'s namespace.
    pub fn pinned(caller: Caller) -> Self {
        Self {
            run_id: caller.run_id,
            namespace: Some(caller.namespace),
            pinned: true,
            role: caller.role,
            queues: caller.queues,
            launched_by: caller.launched_by,
            actor: None,
        }
    }

    /// The caller acting in `requested`, or in its own namespace when none is named.
    pub fn in_namespace(&self, requested: Option<&str>) -> Result<Caller, Refusal> {
        let namespace = match (requested, &self.namespace, self.pinned) {
            (None, Some(own), _) => own.clone(),
            (None, None, _) => return Err(Refusal::NamespaceRequired),
            (Some(r), Some(own), true) if r != own.as_str() => {
                return Err(Refusal::NamespacePinned {
                    pinned: own.to_string(),
                    requested: r.to_string(),
                });
            }
            (Some(r), _, _) => Namespace::parse(r).map_err(Refusal::Namespace)?,
        };
        Ok(Caller {
            run_id: self.run_id.clone(),
            namespace,
            role: self.role,
            queues: self.queues.clone(),
            launched_by: self.launched_by.clone(),
        })
    }
}

/// An [`Identity`] resolved to the one namespace a call acts in.
#[derive(Debug, Clone)]
pub struct Caller {
    pub run_id: RunId,
    pub namespace: Namespace,
    pub role: TenantRole,
    /// Empty means every LocalQueue in the namespace.
    pub queues: Vec<QueueName>,
    /// The human a delegated token acts for, stamped on everything this caller creates. Static
    /// tenant tokens carry no launcher.
    pub launched_by: Option<Launcher>,
}

impl Caller {
    pub fn require_submitter(&self, tool: &'static str) -> Result<(), Refusal> {
        match self.role {
            TenantRole::Submitter => Ok(()),
            TenantRole::Viewer => Err(Refusal::Role {
                role: self.role,
                tool,
            }),
        }
    }

    pub fn authorize_queue(&self, queue: &QueueName) -> Result<(), Refusal> {
        if self.queues.is_empty() || self.queues.contains(queue) {
            return Ok(());
        }
        Err(Refusal::Queue {
            queue: queue.to_string(),
            allowed: self.queues.iter().map(ToString::to_string).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::caller::*;

    #[test]
    fn a_viewer_cannot_submit_and_the_refusal_names_the_tool() {
        let caller = Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("ns").unwrap(),
            role: TenantRole::Viewer,
            queues: vec![],
            launched_by: None,
        };
        let err = caller.require_submitter("workload_submit").unwrap_err();
        assert!(err.to_string().contains("workload_submit"));
    }

    #[test]
    fn a_pinned_identity_acts_only_in_its_own_namespace() {
        let identity = Identity::pinned(Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("team-a").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![],
            launched_by: None,
        });
        assert_eq!(
            identity.in_namespace(None).unwrap().namespace.as_str(),
            "team-a"
        );
        assert_eq!(
            identity
                .in_namespace(Some("team-a"))
                .unwrap()
                .namespace
                .as_str(),
            "team-a"
        );
        assert_eq!(
            identity.in_namespace(Some("team-b")).unwrap_err(),
            Refusal::NamespacePinned {
                pinned: "team-a".into(),
                requested: "team-b".into()
            }
        );
    }

    #[test]
    fn an_unpinned_identity_still_needs_a_valid_namespace() {
        let identity = Identity {
            run_id: RunId::parse("p").unwrap(),
            namespace: None,
            pinned: false,
            role: TenantRole::Submitter,
            queues: vec![],
            launched_by: None,
            actor: None,
        };
        assert!(matches!(
            identity.in_namespace(Some("Not_Valid")),
            Err(Refusal::Namespace(_))
        ));
        assert_eq!(
            identity.in_namespace(None).unwrap_err(),
            Refusal::NamespaceRequired
        );
    }

    #[test]
    fn the_queue_allowlist_is_enforced_and_empty_means_any() {
        let mut caller = Caller {
            run_id: RunId::parse("r").unwrap(),
            namespace: Namespace::parse("ns").unwrap(),
            role: TenantRole::Submitter,
            queues: vec![QueueName::parse("gpu").unwrap()],
            launched_by: None,
        };
        assert!(
            caller
                .authorize_queue(&QueueName::parse("gpu").unwrap())
                .is_ok()
        );
        let err = caller
            .authorize_queue(&QueueName::parse("cpu").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("\"cpu\""));
        caller.queues.clear();
        assert!(
            caller
                .authorize_queue(&QueueName::parse("cpu").unwrap())
                .is_ok()
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
