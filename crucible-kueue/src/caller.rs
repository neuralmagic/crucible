//! Caller identity: who a request comes from ([`Identity`], from its access token in
//! [`crate::auth`]) and who it acts as in one namespace ([`Caller`]). What it may do there is
//! Kubernetes RBAC's to decide, for the user the token names.

use crate::namespaces::Namespaces;
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
dns_label_newtype!(Manager, "manager name");

impl Default for Manager {
    /// `crucible-kueue`.
    fn default() -> Self {
        Self(crate::config::DEFAULT_MANAGER.to_string())
    }
}

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

/// Which kind of caller owns what it creates, so a person whose subject reads like a run id never
/// owns that run's workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    Run,
    Person,
}

impl OwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Person => "person",
        }
    }
}

/// Who a request comes from, before it names a namespace: a run (acting for its launcher) or a
/// person. Either may name any namespace per call, its token's `namespace` claim being the
/// default; the access checks decide whether it may act there.
#[derive(Debug, Clone)]
pub struct Identity {
    pub owner: OwnerKind,
    pub run_id: RunId,
    pub namespace: Option<Namespace>,
    /// The namespaces the token confines the caller to (its `namespaces` claim); `None` when it
    /// names none, so only the served set and RBAC decide.
    pub namespaces: Option<Vec<Namespace>>,
    /// The human the request acts for, stamped on everything it creates.
    pub launched_by: Launcher,
    /// The Kubernetes user the request's access checks run as.
    pub kube_user: String,
    /// The party presenting a run's token (the sandbox's SPIFFE ID), stamped on what it creates.
    pub actor: Option<String>,
}

/// Why an [`Identity`] cannot act in a namespace.
#[derive(Debug, PartialEq, Eq)]
pub enum ActError {
    Refused(Refusal),
    /// The server does not serve it, or the token's `namespaces` claim leaves it out.
    NotServed(Namespace),
}

impl Identity {
    /// The caller acting in `requested`, or in its default namespace when none is named, when
    /// `namespaces` serves it and the token's `namespaces` claim (if any) names it. This is the
    /// only way to get a [`Caller`], so no tool acts in a namespace that skipped the check.
    pub async fn act_in(
        &self,
        requested: Option<&str>,
        namespaces: &Namespaces,
    ) -> Result<Caller, ActError> {
        let namespace = match (requested, &self.namespace) {
            (Some(r), _) => Namespace::parse(r)
                .map_err(Refusal::Namespace)
                .map_err(ActError::Refused)?,
            (None, Some(default)) => default.clone(),
            (None, None) => return Err(ActError::Refused(Refusal::NamespaceRequired)),
        };
        let claimed = self
            .namespaces
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&namespace));
        if !claimed || !namespaces.serves(namespace.as_str()).await {
            return Err(ActError::NotServed(namespace));
        }
        Ok(Caller {
            owner: self.owner,
            run_id: self.run_id.clone(),
            namespace,
            launched_by: self.launched_by.clone(),
            kube_user: self.kube_user.clone(),
            served: Served(()),
        })
    }
}

/// Proof that a [`Caller`]'s namespace passed [`Identity::act_in`]; only this module makes one.
#[derive(Debug, Clone)]
pub struct Served(());

#[cfg(test)]
impl Served {
    pub fn for_test() -> Self {
        Self(())
    }
}

/// An [`Identity`] resolved to the one namespace a call acts in.
#[derive(Debug, Clone)]
pub struct Caller {
    pub owner: OwnerKind,
    pub run_id: RunId,
    pub namespace: Namespace,
    pub launched_by: Launcher,
    pub kube_user: String,
    pub served: Served,
}

#[cfg(test)]
mod tests {
    use crate::caller::*;

    fn served(names: &[&str]) -> Namespaces {
        crate::install_tls_provider();
        let config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        Namespaces::new(
            kube::Client::try_from(config).unwrap(),
            crate::kueue::KueueVersion::default(),
            names.iter().map(|n| n.to_string()).collect(),
            None,
        )
    }

    fn ns(name: &str) -> Namespace {
        Namespace::parse(name).unwrap()
    }

    #[tokio::test]
    async fn an_identity_acts_in_the_named_namespace_or_its_default_when_served() {
        let served = served(&["team-a", "team-b"]);
        let mut identity = Identity {
            owner: OwnerKind::Run,
            run_id: RunId::parse("r").unwrap(),
            namespace: Some(ns("team-a")),
            namespaces: None,
            launched_by: Launcher::parse("alice").unwrap(),
            kube_user: "alice-k8s".into(),
            actor: None,
        };
        let act = |i: &Identity, r: Option<&'static str>| {
            let i = i.clone();
            let served = &served;
            async move { i.act_in(r, served).await }
        };
        assert_eq!(act(&identity, None).await.unwrap().namespace, ns("team-a"));
        assert_eq!(
            act(&identity, Some("team-b")).await.unwrap().namespace,
            ns("team-b")
        );
        assert_eq!(
            act(&identity, Some("kube-system")).await.unwrap_err(),
            ActError::NotServed(ns("kube-system"))
        );
        assert!(matches!(
            act(&identity, Some("Not_Valid")).await,
            Err(ActError::Refused(Refusal::Namespace(_)))
        ));
        identity.namespace = None;
        assert_eq!(
            act(&identity, None).await.unwrap_err(),
            ActError::Refused(Refusal::NamespaceRequired)
        );
    }

    /// A token's `namespaces` claim confines it: a served namespace it leaves out is not served
    /// to this caller, and a claim that shares nothing with the served set leaves nothing.
    #[tokio::test]
    async fn the_namespaces_claim_confines_a_caller_within_the_served_set() {
        let served = served(&["team-a", "team-b"]);
        let identity = Identity {
            owner: OwnerKind::Run,
            run_id: RunId::parse("r").unwrap(),
            namespace: Some(ns("team-a")),
            namespaces: Some(vec![ns("team-a"), ns("elsewhere")]),
            launched_by: Launcher::parse("alice").unwrap(),
            kube_user: "alice-k8s".into(),
            actor: None,
        };
        assert!(identity.act_in(None, &served).await.is_ok());
        assert_eq!(
            identity.act_in(Some("team-b"), &served).await.unwrap_err(),
            ActError::NotServed(ns("team-b"))
        );
        assert_eq!(
            identity
                .act_in(Some("elsewhere"), &served)
                .await
                .unwrap_err(),
            ActError::NotServed(ns("elsewhere")),
            "claimed but not served"
        );
        let disjoint = Identity {
            namespace: Some(ns("other")),
            namespaces: Some(vec![ns("other")]),
            ..identity
        };
        assert_eq!(
            disjoint.act_in(None, &served).await.unwrap_err(),
            ActError::NotServed(ns("other"))
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
