//! Remote Starlark dependencies of stored packs. A fetched tree is stored in `pack_trees` like any
//! pack, `pack_tree_deps` pins it to the pack trees whose lock names it, and it is written beside a
//! pack's scratch directory before a compile.

use anyhow::Context as _;
use crucible::plan::deps::{Auth, Cache, LockedDep};
use crucible_contract::pack_tree::PackTree;
use sqlx::PgPool;

use crate::secrets::github_app::{GithubAppTokenSource, GithubOwner};

const GITHUB_HOST: &str = "github.com";

#[derive(Debug, thiserror::Error)]
pub(crate) enum ResolveError {
    /// The lock or a remote is at fault; the text is for the author.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Which remotes a dependency may be fetched from, and with what credentials.
#[derive(Debug, Clone)]
pub(crate) enum Remotes {
    /// `https://` URLs only. A github.com URL is fetched with a token from the App's installation
    /// on the repository's owner when the App is configured and installed there.
    Https {
        github_app: Option<GithubAppTokenSource>,
    },
    #[cfg(test)]
    Any,
}

impl Remotes {
    /// How to fetch `url`, which `what` names in a refusal.
    pub(crate) async fn auth(&self, what: &str, url: &str) -> Result<Auth, ResolveError> {
        let github_app = match self {
            Remotes::Https { github_app } => github_app,
            #[cfg(test)]
            Remotes::Any => return Ok(Auth::Ambient),
        };
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(ResolveError::Refused(format!(
                "{what}: {url:?} is not an https:// URL"
            )));
        };
        let Some(app) = github_app else {
            return Ok(Auth::Ambient);
        };
        let owner = github_owner(rest).and_then(|owner| GithubOwner::parse(owner).ok());
        let Some(owner) = owner else {
            return Ok(Auth::Ambient);
        };
        if app.installation_of(&owner).await?.is_none() {
            return Ok(Auth::Ambient);
        }
        Ok(Auth::Token {
            host: GITHUB_HOST.to_owned(),
            token: app.token_for_one_consumer(Some(&owner)).await?,
        })
    }
}

/// The owner segment of a github.com URL with its scheme removed: `github.com/<owner>/...`, or
/// the API's `api.github.com/repos/<owner>/...`.
fn github_owner(rest: &str) -> Option<&str> {
    let (authority, path) = rest.split_once('/')?;
    let mut segments = path.split('/');
    if authority == GITHUB_HOST {
        segments.next()
    } else if authority.strip_prefix("api.") == Some(GITHUB_HOST) {
        segments.next().filter(|first| *first == "repos")?;
        segments.next()
    } else {
        None
    }
}

/// The dependencies `tree`'s lock pins.
pub(crate) fn locked(tree: &PackTree) -> Vec<LockedDep> {
    crate::playbooks::lock::lock_of(tree).map_or_else(Vec::new, |lock| lock.deps)
}

/// Store every dependency in `deps` that is not stored yet, fetching it at its locked commit.
pub(crate) async fn resolve(
    pool: &PgPool,
    deps: &[LockedDep],
    remotes: &Remotes,
) -> Result<(), ResolveError> {
    for dep in deps {
        if crate::playbooks::pack_trees::get_tree(pool, &dep.tree)
            .await?
            .is_some()
        {
            continue;
        }
        let auth = remotes
            .auth(&format!("dependency \"{}\"", dep.name), &dep.git)
            .await?;
        let fetched = {
            let dep = dep.clone();
            tokio::task::spawn_blocking(move || {
                let scratch = tempfile::tempdir().context("dependency fetch scratch dir")?;
                Ok::<_, anyhow::Error>(crucible::plan::deps::fetch_dep(
                    &dep,
                    &Cache::at(scratch.path()),
                    &auth,
                ))
            })
            .await
            .context("joining the dependency fetch worker")??
        };
        let tree = fetched.map_err(|error| ResolveError::Refused(error.to_string()))?;
        let pack = crate::playbooks::pack_trees::EncodedPack::new(tree)
            .with_context(|| format!("encoding dependency {}", dep.name))?;
        let mut conn = pool
            .acquire()
            .await
            .context("acquiring a connection to store a dependency")?;
        crate::playbooks::pack_trees::put_tree(&mut conn, &pack).await?;
    }
    Ok(())
}

/// The stored trees of `deps`. One that is not stored is left out, and a compile that needs it
/// says so.
pub(crate) async fn load(pool: &PgPool, deps: &[LockedDep]) -> anyhow::Result<Vec<PackTree>> {
    let mut trees = Vec::new();
    for dep in deps {
        trees.extend(crate::playbooks::pack_trees::get_tree(pool, &dep.tree).await?);
    }
    Ok(trees)
}

/// Write `deps` where a compile of the pack at `pack_root` finds them. Blocking.
pub(crate) fn write_beside(pack_root: &std::path::Path, deps: &[PackTree]) -> anyhow::Result<()> {
    if deps.is_empty() {
        return Ok(());
    }
    let cache = Cache::beside(pack_root)
        .with_context(|| format!("{} has no parent directory", pack_root.display()))?;
    for tree in deps {
        cache
            .put(tree)
            .context("writing a dependency beside its pack")?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::playbooks::deps::*;
    use crate::playbooks::pack_trees::{EncodedPack, collect, get_tree, put_tree};
    use crucible::plan::deps::LOCK_FILE;
    use std::path::Path;
    use std::process::Command;

    pub(crate) fn locked_in(lock: Option<&[u8]>) -> Vec<LockedDep> {
        crate::playbooks::lock::parse_lock(lock).map_or_else(Vec::new, |lock| lock.deps)
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A git repository under `root` holding a playbook helper, and the text of a lock that pins
    /// it as `gates`. `tag` goes into the helper so each caller's tree has its own digest.
    pub(crate) fn locked_library(root: &Path, tag: &str) -> String {
        let repo = root.join("library");
        std::fs::create_dir_all(&repo).expect("library dir");
        git(&repo, &["init", "--quiet", "--initial-branch", "main"]);
        std::fs::write(
            repo.join("gates.star"),
            format!("def hello(name):\n    return command(name = name, run = \"echo {tag}\")\n"),
        )
        .expect("library file");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "one"]);

        let pack = root.join("locking-pack");
        std::fs::create_dir_all(&pack).expect("pack dir");
        std::fs::write(
            pack.join("crucible.toml"),
            format!(
                "[deps.gates]\ngit = \"file://{}\"\nrev = \"main\"\n",
                repo.display()
            ),
        )
        .expect("manifest");
        crucible::plan::deps::lock(&pack, &Cache::at(root.join("author-cache")), &Auth::Ambient)
            .expect("lock");
        std::fs::read_to_string(pack.join(LOCK_FILE)).expect("lock text")
    }

    async fn backdate(pool: &PgPool) {
        sqlx::query("UPDATE pack_trees SET created_at = '2000-01-01T00:00:00Z'")
            .execute(pool)
            .await
            .expect("backdate");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_resolved_dependency_is_stored_cached_and_pinned_by_the_pack_that_locks_it(
        pool: PgPool,
    ) {
        let root = tempfile::tempdir().expect("root");
        let lock = locked_library(root.path(), "pinned");
        let deps = locked_in(Some(lock.as_bytes()));
        let [dep] = deps.as_slice() else {
            panic!("one dep: {deps:?}")
        };
        resolve(&pool, &deps, &Remotes::Any).await.expect("resolve");
        let stored = get_tree(&pool, &dep.tree)
            .await
            .expect("read")
            .expect("stored");
        assert_eq!(stored.digest(), dep.tree);

        std::fs::remove_dir_all(root.path().join("library")).expect("drop the remote");
        resolve(&pool, &deps, &Remotes::Any)
            .await
            .expect("a stored dependency is not fetched again");
        let loaded = load(&pool, &deps).await.expect("load");
        assert_eq!(loaded, [stored]);
        let pack_root = root.path().join("scratch/pack");
        std::fs::create_dir_all(&pack_root).expect("pack root");
        write_beside(&pack_root, &loaded).expect("write");
        assert_eq!(
            crucible_contract::pack_tree::walk_dir(
                &Cache::at(root.path().join("scratch/pack.deps")).dir(&dep.tree)
            )
            .expect("walk")
            .tree
            .digest(),
            dep.tree
        );

        let pack = PackTree::from_pairs(&[(LOCK_FILE, lock.as_bytes())]).expect("pack");
        let mut conn = pool.acquire().await.expect("conn");
        let pack_digest = put_tree(&mut conn, &EncodedPack::new(pack).expect("encode"))
            .await
            .expect("put");
        let pinned: Vec<(String, String)> =
            sqlx::query_as("SELECT name, dep_digest FROM pack_tree_deps WHERE tree_digest = $1")
                .bind(pack_digest.as_str())
                .fetch_all(&pool)
                .await
                .expect("pins");
        assert_eq!(pinned, [("gates".to_string(), dep.tree.to_string())]);

        backdate(&pool).await;
        assert_eq!(collect(&pool).await.expect("collect").trees, 1);
        assert!(get_tree(&pool, &dep.tree).await.expect("read").is_some());
        assert_eq!(collect(&pool).await.expect("collect").trees, 1);
        assert!(get_tree(&pool, &dep.tree).await.expect("read").is_none());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pack_whose_dependency_is_not_stored_is_stored_unpinned_and_loads_nothing(
        pool: PgPool,
    ) {
        let root = tempfile::tempdir().expect("root");
        let lock = locked_library(root.path(), "unresolved");
        let deps = locked_in(Some(lock.as_bytes()));
        let pack = PackTree::from_pairs(&[(LOCK_FILE, lock.as_bytes())]).expect("pack");
        let mut conn = pool.acquire().await.expect("conn");
        put_tree(&mut conn, &EncodedPack::new(pack).expect("encode"))
            .await
            .expect("put");
        let pins: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_tree_deps")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(pins, 0);

        assert!(load(&pool, &deps).await.expect("load").is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn only_https_remotes_are_fetched_and_a_changed_tree_is_refused(pool: PgPool) {
        let root = tempfile::tempdir().expect("root");
        let lock = locked_library(root.path(), "refused");
        let deps = locked_in(Some(lock.as_bytes()));
        let https_only = Remotes::Https { github_app: None };
        let error = resolve(&pool, &deps, &https_only)
            .await
            .expect_err("a file URL");
        assert!(
            matches!(&error, ResolveError::Refused(message) if message.contains("is not an https:// URL")),
            "{error}"
        );

        let mut tampered = deps.clone();
        tampered[0].tree = PackTree::from_pairs(&[("other", b"x".as_slice())])
            .expect("tree")
            .digest();
        let error = resolve(&pool, &tampered, &Remotes::Any)
            .await
            .expect_err("a digest mismatch");
        assert!(
            matches!(&error, ResolveError::Refused(message) if message.contains("pins")),
            "{error}"
        );
        assert!(
            get_tree(&pool, &tampered[0].tree)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            get_tree(&pool, &deps[0].tree)
                .await
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn a_lock_that_does_not_parse_pins_nothing() {
        assert!(locked_in(None).is_empty());
        assert!(locked_in(Some(b"version = 1\n")).is_empty());
        assert!(locked_in(Some(b"not toml [")).is_empty());
        assert!(locked_in(Some(&[0xff, 0xfe])).is_empty());
    }
}
