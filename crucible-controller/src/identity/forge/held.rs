//! The forge tokens a controller acting with the person's own access holds in memory.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::identity::forge::{Forge, ForgeHost};

/// The forge tokens the person last linked, by forge and host, on a controller that acts as that
/// person. Clones share one map.
#[derive(Clone, Default)]
pub struct SignedIn(Arc<RwLock<BTreeMap<(Forge, ForgeHost), String>>>);

impl SignedIn {
    pub(crate) fn set(&self, forge: Forge, host: &ForgeHost, token: Option<String>) {
        if let Ok(mut held) = self.0.write() {
            match token {
                Some(token) => held.insert((forge, host.clone()), token),
                None => held.remove(&(forge, host.clone())),
            };
        }
    }

    pub(crate) fn token(&self, forge: Forge, host: &ForgeHost) -> Option<String> {
        self.0
            .read()
            .ok()
            .and_then(|held| held.get(&(forge, host.clone())).cloned())
    }
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.0.read().map(|held| held.len()).unwrap_or(0);
        write!(f, "SignedIn({held} held)")
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::forge::ForgeHost;
    use crate::identity::forge::held::*;

    #[test]
    fn signed_in_tokens_are_kept_per_forge_host_and_never_formatted() {
        let held = SignedIn::default();
        let other = held.clone();
        let gl = ForgeHost::parse("gitlab.com").unwrap();
        held.set(
            Forge::Github,
            &ForgeHost::github(),
            Some("ghu_secret".into()),
        );
        held.set(Forge::Gitlab, &gl, Some("glpat_secret".into()));
        assert_eq!(
            other.token(Forge::Gitlab, &gl).as_deref(),
            Some("glpat_secret")
        );
        assert_eq!(other.token(Forge::Github, &gl), None);
        let shown = format!("{other:?}");
        assert_eq!(shown, "SignedIn(2 held)");
        held.set(Forge::Gitlab, &gl, None);
        assert_eq!(other.token(Forge::Gitlab, &gl), None);
    }
}
