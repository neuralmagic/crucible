//! The clusters a Tekton target may name, as the controller is configured with them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crucible_ci::tekton::Namespace;

use crate::authz::model::Principal;

/// One cluster a Tekton target may name: the kubeconfig whose current context reaches it, read at
/// each operation and held in memory only, and its Tekton Results address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TektonCluster {
    pub kubeconfig: PathBuf,
    pub results: Option<String>,
    pub grants: BTreeMap<String, Vec<Principal>>,
}

impl TektonCluster {
    pub(crate) fn grants(&self, namespace: &Namespace, who: &Principal) -> bool {
        let who = Principal::parse(&who.to_string()).unwrap_or_else(|_| who.clone());
        [namespace.as_str(), ANY_NAMESPACE]
            .iter()
            .filter_map(|ns| self.grants.get(*ns))
            .any(|granted| granted.contains(&who))
    }
}

const ANY_NAMESPACE: &str = "*";

pub(crate) fn clusters(
    kubeconfigs: &[String],
    results: &[String],
    grants: &[String],
) -> Result<BTreeMap<String, TektonCluster>, String> {
    let pair = |entry: &str, what: &str| -> Result<(String, String), String> {
        match entry.split_once('=') {
            Some((name, value)) if !name.trim().is_empty() && !value.trim().is_empty() => {
                Ok((name.trim().to_owned(), value.trim().to_owned()))
            }
            _ => Err(format!("{entry:?} is not <cluster>={what}")),
        }
    };
    let mut out = BTreeMap::new();
    for entry in kubeconfigs.iter().filter(|e| !e.trim().is_empty()) {
        let (name, path) = pair(entry, "<kubeconfig path>")?;
        out.insert(
            name,
            TektonCluster {
                kubeconfig: PathBuf::from(path),
                results: None,
                grants: BTreeMap::new(),
            },
        );
    }
    for entry in results.iter().filter(|e| !e.trim().is_empty()) {
        let (name, url) = pair(entry, "<results url>")?;
        let cluster = out.get_mut(&name).ok_or_else(|| {
            format!("Tekton Results names cluster {name:?}, which has no kubeconfig")
        })?;
        cluster.results = Some(url);
    }
    for entry in grants.iter().filter(|e| !e.trim().is_empty()) {
        let (place, principals) = pair(entry, "<principal>|<principal>")?;
        let (name, namespace) = place
            .split_once('/')
            .filter(|(c, n)| !c.is_empty() && !n.is_empty())
            .ok_or_else(|| format!("{entry:?} is not <cluster>/<namespace or *>=<principals>"))?;
        if namespace != ANY_NAMESPACE {
            Namespace::parse(namespace).map_err(|e| format!("{entry:?}: {e}"))?;
        }
        let principals = principals
            .split('|')
            .map(|p| Principal::parse(p).map_err(|e| format!("{entry:?}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let cluster = out.get_mut(name).ok_or_else(|| {
            format!("Tekton grant names cluster {name:?}, which has no kubeconfig")
        })?;
        cluster
            .grants
            .entry(namespace.to_owned())
            .or_default()
            .extend(principals);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crucible_ci::tekton::Namespace;

    use crate::authz::model::Principal;
    use crate::calls::tekton::cluster::clusters;

    #[test]
    fn cluster_entries_parse() {
        let c = clusters(
            &["konflux=/tmp/kubeconfig".to_owned()],
            &["konflux=https://results.example.com".to_owned()],
            &[],
        )
        .expect("parses");
        assert_eq!(
            c["konflux"].results.as_deref(),
            Some("https://results.example.com")
        );
        assert!(clusters(&["bad".to_owned()], &[], &[]).is_err());
        assert!(clusters(&[], &["other=https://r".to_owned()], &[]).is_err());
    }

    #[test]
    fn a_watch_needs_a_grant_on_its_namespace() {
        let c = clusters(
            &["konflux=/tmp/kubeconfig".to_owned()],
            &[],
            &[
                "konflux/tenant-a=user:Wren|team:builders".to_owned(),
                "konflux/*=user:ops".to_owned(),
            ],
        )
        .expect("parses");
        let konflux = &c["konflux"];
        let a = Namespace::parse("tenant-a").unwrap();
        let b = Namespace::parse("tenant-b").unwrap();
        let user = |l: &str| Principal::User(l.into());
        assert!(konflux.grants(&a, &user("wren")));
        assert!(konflux.grants(&a, &user("WREN")));
        assert!(konflux.grants(&a, &Principal::parse("team:builders").unwrap()));
        assert!(!konflux.grants(&b, &user("wren")));
        assert!(konflux.grants(&b, &user("ops")));
        assert!(!konflux.grants(&a, &user("mallory")));
        for bad in [
            "other/tenant-a=user:wren",
            "konflux=user:wren",
            "konflux/tenant-a=wren",
            "konflux/Not_A_NS=user:wren",
            "konflux/=user:wren",
        ] {
            assert!(
                clusters(&["konflux=/k".to_owned()], &[], &[bad.to_owned()]).is_err(),
                "{bad:?}"
            );
        }
    }
}
