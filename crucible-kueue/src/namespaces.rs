//! Which namespaces a caller can use: those this server is installed in (configured, or found as
//! the namespaces holding a LocalQueue), narrowed to those where the caller's own RBAC lets it
//! list LocalQueues.

use crate::kueue::{self, KueueVersion, LocalQueue};
use crucible_mcp_auth::access::{AccessCheck, AccessReview};
use crucible_mcp_auth::kube::USER_CLAIM;
use crucible_mcp_auth::principal::Principal;
use kube::api::ListParams;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DISCOVERY_TTL: Duration = Duration::from_secs(60);

pub struct Namespaces {
    client: kube::Client,
    kueue: KueueVersion,
    configured: Vec<String>,
    reviewer: Option<Arc<dyn AccessReview>>,
    discovered: Mutex<Option<(Instant, Vec<String>)>>,
}

impl Namespaces {
    pub fn new(
        client: kube::Client,
        kueue: KueueVersion,
        configured: Vec<String>,
        reviewer: Option<Arc<dyn AccessReview>>,
    ) -> Self {
        Self {
            client,
            kueue,
            configured,
            reviewer,
            discovered: Mutex::new(None),
        }
    }

    /// The namespaces this server serves: the configured ones, else every namespace holding a
    /// LocalQueue, if the server may list them cluster-wide (cached a minute). Empty when neither
    /// is known.
    pub async fn served(&self) -> Vec<String> {
        if !self.configured.is_empty() {
            return self.configured.clone();
        }
        let mut cached = self.discovered.lock().await;
        if let Some((at, found)) = cached.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return found.clone();
        }
        let found = match kueue::list::<LocalQueue>(
            &self.client,
            &self.kueue.local_queues(),
            None,
            &ListParams::default(),
        )
        .await
        {
            Ok(queues) => {
                let mut ns: Vec<String> = queues
                    .into_iter()
                    .filter_map(|q| q.metadata.namespace)
                    .collect();
                ns.sort();
                ns.dedup();
                ns
            }
            Err(error) => {
                tracing::debug!(%error, "cannot discover served namespaces; configure --namespace");
                Vec::new()
            }
        };
        *cached = Some((Instant::now(), found.clone()));
        found
    }

    /// The served namespaces the caller may list LocalQueues in. A caller without a Kubernetes
    /// user, or a server without a reviewer, can use every served namespace.
    pub async fn usable(&self, principal: &Principal) -> Vec<String> {
        usable(self.served().await, self.reviewer.as_deref(), principal).await
    }
}

pub async fn usable(
    served: Vec<String>,
    reviewer: Option<&dyn AccessReview>,
    principal: &Principal,
) -> Vec<String> {
    let Some(reviewer) = reviewer.filter(|_| principal.claims.contains_key(USER_CLAIM)) else {
        return served;
    };
    let mut out = Vec::new();
    for namespace in served {
        let check = AccessCheck {
            tool: "namespaces".into(),
            rule: "queues-list".into(),
            message: "you may not list LocalQueues in this namespace".into(),
            group: kueue::GROUP.into(),
            resource: "localqueues".into(),
            subresource: String::new(),
            verb: "list".into(),
            namespace: namespace.clone(),
            name: String::new(),
        };
        if matches!(reviewer.review(principal, &check).await, Ok(true)) {
            out.push(namespace);
        }
    }
    out
}

/// `namespaces you can use: a, b`, or that there are none.
pub fn describe(usable: &[String]) -> String {
    match usable.is_empty() {
        true => "there is no namespace you can use with this server".into(),
        false => format!("namespaces you can use: {}", usable.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use crate::namespaces::*;
    use crucible_mcp_auth::access::ReviewFuture;
    use crucible_mcp_auth::principal::Callers;
    use serde_json::{Value, json};

    /// RBAC that lets the caller list LocalQueues in `allowed` only.
    struct Rbac(&'static [&'static str]);

    impl AccessReview for Rbac {
        fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
            let ok = check.verb == "list"
                && check.resource == "localqueues"
                && self.0.contains(&check.namespace.as_str());
            Box::pin(async move { Ok(ok) })
        }
    }

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::People).unwrap(),
            _ => unreachable!(),
        }
    }

    fn served() -> Vec<String> {
        ["ml-a", "ml-b", "ops"].map(String::from).to_vec()
    }

    #[tokio::test]
    async fn a_caller_can_use_the_served_namespaces_its_rbac_lets_it_list_queues_in() {
        let alice = principal(json!({"sub": "a", "kube_user": "alice"}));
        let rbac = Rbac(&["ml-b", "elsewhere"]);
        assert_eq!(usable(served(), Some(&rbac), &alice).await, ["ml-b"]);
        let nobody = Rbac(&[]);
        assert!(usable(served(), Some(&nobody), &alice).await.is_empty());
    }

    #[tokio::test]
    async fn without_a_reviewer_or_a_kubernetes_user_every_served_namespace_is_usable() {
        let alice = principal(json!({"sub": "a", "kube_user": "alice"}));
        assert_eq!(usable(served(), None, &alice).await, served());
        let run = principal(json!({"sub": "r"}));
        assert_eq!(usable(served(), Some(&Rbac(&[])), &run).await, served());
    }

    #[test]
    fn the_usable_namespaces_read_as_one_clause() {
        assert_eq!(
            describe(&["ml-a".into(), "ml-b".into()]),
            "namespaces you can use: ml-a, ml-b"
        );
        assert_eq!(
            describe(&[]),
            "there is no namespace you can use with this server"
        );
    }
}
