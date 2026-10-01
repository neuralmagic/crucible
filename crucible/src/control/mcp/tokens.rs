//! The engine's half of the per-sandbox token file (see [`crucible_contract::mcp`]): mint a token
//! for a sandbox, grant it, revoke it at teardown.

use anyhow::{Context, Result};
use crucible_contract::mcp::{TokenHolder, TokenMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One server's token file.
#[derive(Debug, Clone)]
pub struct TokenRegistry {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl TokenRegistry {
    /// Start an empty registry at `path`.
    pub fn create(path: PathBuf) -> Result<Self> {
        let registry = Self {
            path,
            lock: Arc::new(Mutex::new(())),
        };
        registry.write(&TokenMap::default())?;
        Ok(registry)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a fresh token for `holder`'s sandbox, replacing any it held, and return it.
    pub fn grant(&self, holder: &TokenHolder) -> Result<String> {
        let token = mint_token()?;
        self.update(|map| map.grant(holder.clone(), &token))?;
        Ok(token)
    }

    /// Drop `sandbox`'s token.
    pub fn revoke(&self, sandbox: &str) -> Result<()> {
        self.update(|map| map.revoke(sandbox))
    }

    fn update(&self, change: impl FnOnce(&mut TokenMap)) -> Result<()> {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let mut map =
            TokenMap::parse(&text).with_context(|| format!("parsing {}", self.path.display()))?;
        change(&mut map);
        self.write(&map)
    }

    /// Replace the file in one rename, so the server never reads half of it.
    fn write(&self, map: &TokenMap) -> Result<()> {
        use std::io::Write;
        let dir = self
            .path
            .parent()
            .with_context(|| format!("{} has no parent directory", self.path.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("staging a token file in {}", dir.display()))?;
        tmp.write_all(map.render().as_bytes())
            .context("writing the token file")?;
        tmp.persist(&self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

/// A fresh random bearer token: 24 bytes of OS entropy, hex-encoded. Read from `/dev/urandom`
/// directly so no rand crate is pulled in (linux pods + macOS both have it).
pub fn mint_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use crate::control::mcp::tokens::{TokenRegistry, mint_token};
    use crucible_contract::mcp::{TokenHolder, TokenMap};

    fn holder(sandbox: &str) -> TokenHolder {
        TokenHolder::new(sandbox, &format!("/sandbox/{sandbox}")).unwrap()
    }

    fn read(registry: &TokenRegistry) -> TokenMap {
        TokenMap::parse(&std::fs::read_to_string(registry.path()).unwrap()).unwrap()
    }

    /// Real entropy read: 48 hex chars, and two mints never collide.
    #[test]
    fn mint_token_is_hex_and_unique() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), 48);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_sandbox_holds_one_token_that_names_only_it() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TokenRegistry::create(dir.path().join("jira.tokens")).unwrap();
        assert!(read(&registry).is_empty());

        let a = registry.grant(&holder("ci-a")).unwrap();
        let b = registry.grant(&holder("ci-b")).unwrap();
        let map = read(&registry);
        assert_eq!(map.holder_for(&a), Some(&holder("ci-a")));
        assert_eq!(map.holder_for(&b), Some(&holder("ci-b")));

        let a2 = registry.grant(&holder("ci-a")).unwrap();
        let map = read(&registry);
        assert_eq!(
            map.holder_for(&a),
            None,
            "a new grant retires the old token"
        );
        assert_eq!(map.holder_for(&a2), Some(&holder("ci-a")));

        registry.revoke("ci-a").unwrap();
        let map = read(&registry);
        assert_eq!(map.holder_for(&a2), None);
        assert_eq!(map.holder_for(&b), Some(&holder("ci-b")));
    }

    /// A file written before the workdir field keeps its lines through a grant and a revoke.
    #[test]
    fn a_two_field_file_survives_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TokenRegistry::create(dir.path().join("t.tokens")).unwrap();
        std::fs::write(registry.path(), "tok-old ci-old\n").unwrap();

        let fresh = registry.grant(&holder("ci-new")).unwrap();
        assert_eq!(
            std::fs::read_to_string(registry.path()).unwrap(),
            format!("tok-old ci-old\n{fresh} ci-new /sandbox/ci-new\n")
        );
        let map = read(&registry);
        let old = map.holder_for("tok-old").unwrap();
        assert_eq!((old.sandbox(), old.workdir()), ("ci-old", None));

        registry.revoke("ci-old").unwrap();
        assert_eq!(read(&registry).holder_for("tok-old"), None);
        assert_eq!(read(&registry).holder_for(&fresh), Some(&holder("ci-new")));
    }

    #[test]
    fn concurrent_grants_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TokenRegistry::create(dir.path().join("t.tokens")).unwrap();
        let tokens: Vec<(String, String)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let registry = registry.clone();
                    scope.spawn(move || {
                        let sandbox = format!("ci-{i}");
                        let token = registry.grant(&holder(&sandbox)).unwrap();
                        (sandbox, token)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let map = read(&registry);
        for (sandbox, token) in &tokens {
            assert_eq!(map.holder_for(token), Some(&holder(sandbox)));
        }
    }
}
