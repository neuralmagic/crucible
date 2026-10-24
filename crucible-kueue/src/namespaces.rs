//! Which namespaces a caller can use: those this server is installed in (configured, or found as
//! the namespaces holding a LocalQueue), narrowed to those where the caller's own RBAC lets it
//! list LocalQueues.

use crate::config::NAMESPACES_CLAIM;
use crate::kueue::{self, KueueVersion, LocalQueue};
use crucible_mcp_auth::access::{AccessCheck, AccessReview};
use crucible_mcp_auth::kube::{GROUPS_CLAIM, USER_CLAIM};
use crucible_mcp_auth::principal::Principal;
use kube::api::ListParams;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DISCOVERY_TTL: Duration = Duration::from_secs(60);
/// How long a caller's usable namespaces are reused; a new RoleBinding shows within this.
const USABLE_TTL: Duration = Duration::from_secs(30);

pub struct Namespaces {
    client: kube::Client,
    kueue: KueueVersion,
    configured: Vec<String>,
    reviewer: Option<Arc<dyn AccessReview>>,
    discovered: Mutex<Option<(Instant, Vec<String>)>>,
    /// Each identity's usable namespaces, so a call that names none does not review every served
    /// namespace again.
    usable: moka::future::Cache<String, Vec<String>>,
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
            usable: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(USABLE_TTL)
                .build(),
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

    /// Whether this server serves `namespace`. A call anywhere else is refused before it reaches
    /// the API server, whatever the caller's own RBAC there.
    pub async fn serves(&self, namespace: &str) -> bool {
        self.served().await.iter().any(|n| n == namespace)
    }

    /// The served namespaces the token's `namespaces` claim (if any) names and the caller may list
    /// LocalQueues in. A caller without a Kubernetes user, or a server without a reviewer, can use
    /// every such namespace. Cached [`USABLE_TTL`] per identity (subject, user, groups and claim).
    pub async fn usable(&self, principal: &Principal) -> Vec<String> {
        let claimed = principal.claims.get(NAMESPACES_CLAIM).map(|v| match v {
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            _ => Vec::new(),
        });
        let mut served = self.served().await;
        if let Some(claimed) = &claimed {
            served.retain(|n| claimed.contains(&n.as_str()));
        }
        let reviewed = self.reviewer.is_some() && principal.claims.contains_key(USER_CLAIM);
        if !reviewed {
            return served;
        }
        let key = serde_json::json!([
            principal.subject,
            principal.claims.get(USER_CLAIM),
            principal.claims.get(GROUPS_CLAIM),
            claimed,
        ])
        .to_string();
        if let Some(hit) = self.usable.get(&key).await {
            return hit;
        }
        let found = usable(served, self.reviewer.as_deref(), principal).await;
        self.usable.insert(key, found.clone()).await;
        found
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

    /// A configured server serves exactly its namespaces; nothing else is asked of the API server.
    #[tokio::test]
    async fn a_configured_server_serves_its_namespaces_and_no_other() {
        crate::install_tls_provider();
        let config = kube::Config::new("http://127.0.0.1:9".parse().unwrap());
        let client = kube::Client::try_from(config).unwrap();
        let namespaces = Namespaces::new(
            client,
            KueueVersion::default(),
            vec!["team-a".into(), "team-b".into()],
            None,
        );
        assert!(namespaces.serves("team-a").await);
        assert!(namespaces.serves("team-b").await);
        assert!(!namespaces.serves("kube-system").await);
        assert!(!namespaces.serves("team-a-other").await);
    }

    /// A reviewer that counts its reviews.
    struct Counting(std::sync::atomic::AtomicUsize);

    impl AccessReview for Counting {
        fn review<'a>(&'a self, _: &'a Principal, check: &'a AccessCheck) -> ReviewFuture<'a> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ok = check.namespace == "team-a";
            Box::pin(async move { Ok(ok) })
        }
    }

    /// One identity's usable namespaces are reviewed once and reused; another identity is
    /// reviewed on its own.
    #[tokio::test]
    async fn usable_namespaces_are_reviewed_once_per_identity() {
        crate::install_tls_provider();
        let config = kube::Config::new("http://127.0.0.1:9".parse().unwrap());
        let client = kube::Client::try_from(config).unwrap();
        let counting = Arc::new(Counting(Default::default()));
        let namespaces = Namespaces::new(
            client,
            KueueVersion::default(),
            vec!["team-a".into(), "team-b".into()],
            Some(counting.clone()),
        );
        let alice = principal(json!({"sub": "a", "kube_user": "alice"}));
        assert_eq!(namespaces.usable(&alice).await, ["team-a"]);
        assert_eq!(namespaces.usable(&alice).await, ["team-a"]);
        assert_eq!(counting.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        namespaces
            .usable(&principal(json!({"sub": "b", "kube_user": "bob"})))
            .await;
        assert_eq!(counting.0.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

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
