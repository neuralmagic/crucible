//! The catalog sweep: list each watched repository's channel tags, describe the digests not yet
//! catalogued, and record the repository's poll outcome.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crucible_capability::{CAPABILITIES_LABEL, CapabilityDoc};

use crate::client::Db;
use crate::clock::now_rfc3339;
use crate::images::model::{CatalogImage, RepositoryStatus};
use crate::images::registry::{DigestDescription, RegistryReader, is_channel_tag, is_pattern};
use crate::images::store;

const INTRO_DIGEST_LABEL: &str = "io.crucible.intro.digest";

/// What the catalog watches and how often.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    pub repositories: Vec<String>,
    pub interval: Duration,
}

/// One sweep's outcome per repository: how many images it catalogued, or why it failed.
#[derive(Debug, Default, PartialEq)]
pub struct SweepReport {
    pub catalogued: BTreeMap<String, usize>,
    pub failed: BTreeMap<String, String>,
}

/// Sweep every configured entry once. A pattern expands to the repositories it names right now;
/// a repository whose read fails keeps its cached rows and records the error; the others still
/// refresh.
pub async fn sweep(db: &Db, reader: &dyn RegistryReader, entries: &[String]) -> SweepReport {
    let mut report = SweepReport::default();
    let mut repositories: Vec<String> = Vec::new();
    for entry in entries {
        if !is_pattern(entry) {
            repositories.push(entry.clone());
            continue;
        }
        match reader.discover(entry).await {
            Ok(found) => repositories.extend(found),
            Err(e) => record_failure(db, &mut report, entry, e.to_string()).await,
        }
    }
    repositories.sort();
    repositories.dedup();
    for repository in &repositories {
        let polled = now_rfc3339();
        match sweep_repository(db, reader, repository, &polled).await {
            Ok(count) => {
                report.catalogued.insert(repository.clone(), count);
                let status = RepositoryStatus {
                    repository: repository.clone(),
                    last_polled: polled.clone(),
                    last_ok: Some(polled),
                    last_error: None,
                };
                if let Err(e) = store::upsert_repository(db.pool(), &status).await {
                    tracing::warn!(repository, error = %format!("{e:#}"), "catalog: recording the poll failed");
                }
            }
            Err(e) => record_failure(db, &mut report, repository, format!("{e:#}")).await,
        }
    }
    report
}

async fn record_failure(db: &Db, report: &mut SweepReport, entry: &str, error: String) {
    tracing::warn!(entry, error = %error, "catalog: sweep failed; keeping the cached rows");
    report.failed.insert(entry.to_string(), error.clone());
    let status = RepositoryStatus {
        repository: entry.to_string(),
        last_polled: now_rfc3339(),
        last_ok: None,
        last_error: Some(error),
    };
    if let Err(e) = store::upsert_repository(db.pool(), &status).await {
        tracing::warn!(entry, error = %format!("{e:#}"), "catalog: recording the poll failed");
    }
}

async fn sweep_repository(
    db: &Db,
    reader: &dyn RegistryReader,
    repository: &str,
    now: &str,
) -> anyhow::Result<usize> {
    let tags = reader.tags(repository).await?;
    let mut by_digest: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tag in tags.into_iter().filter(|t| is_channel_tag(t)) {
        let digest = reader.resolve(repository, &tag).await?;
        by_digest.entry(digest).or_default().push(tag);
    }
    let known = store::known_digests(db.pool(), repository).await?;
    for (digest, tags) in &by_digest {
        let mut tags = tags.clone();
        tags.sort();
        if known.contains(digest) {
            store::touch_image(db.pool(), repository, digest, &tags, now).await?;
        } else {
            let description = reader.describe(repository, digest).await?;
            let image = catalog_image(repository, digest.clone(), tags, description, now);
            store::upsert_image(db.pool(), &image).await?;
        }
    }
    let keep: Vec<String> = by_digest.keys().cloned().collect();
    store::prune_images(db.pool(), repository, &keep).await?;
    Ok(keep.len())
}

/// Build the catalog row for a freshly described digest.
pub fn catalog_image(
    repository: &str,
    digest: String,
    tags: Vec<String>,
    description: DigestDescription,
    now: &str,
) -> CatalogImage {
    let (capabilities, capability_digest) = match description.labels.get(CAPABILITIES_LABEL) {
        None => (None, None),
        Some(raw) => match serde_json::from_str::<CapabilityDoc>(raw) {
            Ok(doc) => {
                use sha2::Digest as _;
                let digest = format!("sha256:{:x}", sha2::Sha256::digest(raw.as_bytes()));
                (Some(doc), Some(digest))
            }
            Err(e) => {
                tracing::warn!(repository, %digest, error = %e, "catalog: capability label does not parse; cataloguing as unverified");
                (None, None)
            }
        },
    };
    CatalogImage {
        repository: repository.to_string(),
        digest,
        tags,
        arches: description.arches,
        created_at: description.created_at,
        capabilities,
        capability_digest,
        intro_digest: description.labels.get(INTRO_DIGEST_LABEL).cloned(),
        first_seen: now.to_string(),
        last_seen: now.to_string(),
    }
}

/// The resident watcher: sweep on the interval, and immediately when `refresh` is notified.
pub async fn watch_loop(
    db: Db,
    reader: Arc<dyn RegistryReader>,
    cfg: CatalogConfig,
    refresh: Arc<tokio::sync::Notify>,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let mut timer = tokio::time::interval(cfg.interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let signal = shutdown.notified();
    tokio::pin!(signal);
    signal.as_mut().enable();
    loop {
        tokio::select! {
            _ = timer.tick() => {}
            _ = refresh.notified() => {}
            _ = signal.as_mut() => break,
        }
        let report = sweep(&db, reader.as_ref(), &cfg.repositories).await;
        tracing::info!(
            catalogued = ?report.catalogued,
            failed = ?report.failed,
            "catalog: sweep complete"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;

    use anyhow::Result;
    use crucible_capability::{CAPABILITIES_LABEL, CAPABILITIES_SCHEMA};
    use sqlx::PgPool;

    use crate::client::Db;
    use crate::images::registry::{DigestDescription, TableRegistry};
    use crate::images::store;
    use crate::images::sweep::sweep;

    const REPO: &str = "ghcr.io/acme/sandbox-go-cc";
    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn labelled(predicates: &[(&str, &str)]) -> DigestDescription {
        let doc = serde_json::json!({
            "features": ["base", "go", "claude-code"],
            "image": "sandbox-go-cc",
            "predicates": predicates.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
            "schema": CAPABILITIES_SCHEMA,
        });
        let mut labels = BTreeMap::new();
        labels.insert(CAPABILITIES_LABEL.to_string(), doc.to_string());
        labels.insert(
            "io.crucible.intro.digest".to_string(),
            "sha256:abc".to_string(),
        );
        DigestDescription {
            arches: vec!["amd64".into(), "arm64".into()],
            created_at: Some("2026-09-12T06:40:00Z".into()),
            labels,
        }
    }

    fn db(pool: PgPool) -> Db {
        Db::new(pool)
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_sweep_catalogues_channel_tags_grouped_by_digest(pool: PgPool) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag(REPO, "latest", D1);
        registry.tag(REPO, "fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e", D1);
        registry.tag(REPO, "fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e-amd64", D1);
        registry.tag(REPO, "buildcache-amd64", D2);
        registry.tag(REPO, "sha256-1111.sig", D2);
        registry.digest(
            D1,
            labelled(&[
                ("toolchain.go", "1.25.11"),
                ("agent.claude-code", "2.1.270"),
            ]),
        );

        let report = sweep(&db, &registry, &[REPO.to_string()]).await;
        assert_eq!(report.catalogued.get(REPO), Some(&1));
        assert!(report.failed.is_empty());

        let images = store::list_images(db.pool()).await?;
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!(image.digest, D1);
        assert_eq!(
            image.tags,
            vec!["fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e", "latest"]
        );
        assert_eq!(image.arches, vec!["amd64", "arm64"]);
        assert_eq!(image.name(), "sandbox-go-cc");
        let doc = image
            .capabilities
            .as_ref()
            .expect("labelled image is verified");
        assert_eq!(doc.predicates["toolchain.go"], "1.25.11");
        assert!(
            image
                .capability_digest
                .as_deref()
                .unwrap()
                .starts_with("sha256:")
        );
        assert_eq!(image.intro_digest.as_deref(), Some("sha256:abc"));

        let repos = store::list_repositories(db.pool()).await?;
        assert_eq!(repos.len(), 1);
        assert!(repos[0].last_ok.is_some());
        assert!(repos[0].last_error.is_none());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_known_digest_is_not_described_again_and_moved_tags_follow(
        pool: PgPool,
    ) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag(REPO, "latest", D1);
        registry.tag(REPO, "aaaa", D1);
        registry.digest(D1, labelled(&[]));
        registry.digest(D2, labelled(&[("toolchain.go", "1.26.0")]));
        sweep(&db, &registry, &[REPO.to_string()]).await;
        assert_eq!(registry.describes.load(Ordering::SeqCst), 1);

        // A new build: latest moves to D2, the sha tag stays on D1.
        registry.tag(REPO, "latest", D2);
        registry.tag(REPO, "bbbb", D2);
        sweep(&db, &registry, &[REPO.to_string()]).await;
        assert_eq!(
            registry.describes.load(Ordering::SeqCst),
            2,
            "only D2 is new"
        );

        let images = store::list_images(db.pool()).await?;
        let by_digest: BTreeMap<_, _> = images.iter().map(|i| (i.digest.as_str(), i)).collect();
        assert_eq!(by_digest[D1].tags, vec!["aaaa"]);
        assert_eq!(by_digest[D2].tags, vec!["bbbb", "latest"]);
        assert_eq!(
            by_digest[D1].arches,
            vec!["amd64", "arm64"],
            "the cached description survives"
        );

        // The old sha tag is deleted upstream: its digest leaves the catalog.
        registry.untag(REPO, "aaaa");
        sweep(&db, &registry, &[REPO.to_string()]).await;
        let images = store::list_images(db.pool()).await?;
        assert_eq!(
            images.iter().map(|i| i.digest.as_str()).collect::<Vec<_>>(),
            vec![D2]
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_registry_outage_keeps_the_cached_rows_and_records_the_error(
        pool: PgPool,
    ) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag(REPO, "latest", D1);
        registry.digest(D1, labelled(&[]));
        sweep(&db, &registry, &[REPO.to_string()]).await;

        registry.fail(REPO, "502 from ghcr.io");
        let report = sweep(&db, &registry, &[REPO.to_string()]).await;
        assert_eq!(
            report.failed.get(REPO).map(String::as_str),
            Some("502 from ghcr.io")
        );

        let images = store::list_images(db.pool()).await?;
        assert_eq!(images.len(), 1, "the cached catalog survives the outage");
        let repos = store::list_repositories(db.pool()).await?;
        assert_eq!(repos[0].last_error.as_deref(), Some("502 from ghcr.io"));
        assert!(
            repos[0].last_ok.is_some(),
            "the last completed poll is kept"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pattern_expands_to_the_matching_repositories(pool: PgPool) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag("ghcr.io/acme/sandbox-go-cc", "latest", D1);
        registry.tag("ghcr.io/acme/sandbox-rust-cc", "latest", D2);
        registry.tag("ghcr.io/acme/loop-base", "latest", D1);
        registry.digest(D1, labelled(&[]));
        registry.digest(D2, labelled(&[]));
        let report = sweep(&db, &registry, &["ghcr.io/acme/sandbox-*".to_string()]).await;
        let mut swept: Vec<_> = report.catalogued.keys().cloned().collect();
        swept.sort();
        assert_eq!(
            swept,
            vec!["ghcr.io/acme/sandbox-go-cc", "ghcr.io/acme/sandbox-rust-cc"]
        );
        assert!(report.failed.is_empty());

        registry.fail("ghcr.io/acme/sandbox-*", "GitHub answered 401");
        let report = sweep(&db, &registry, &["ghcr.io/acme/sandbox-*".to_string()]).await;
        assert_eq!(
            report
                .failed
                .get("ghcr.io/acme/sandbox-*")
                .map(String::as_str),
            Some("GitHub answered 401")
        );
        assert_eq!(
            store::list_images(db.pool()).await?.len(),
            2,
            "cached rows survive"
        );
        let repos = store::list_repositories(db.pool()).await?;
        assert!(
            repos
                .iter()
                .any(|r| r.repository == "ghcr.io/acme/sandbox-*" && r.last_error.is_some())
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_unlabelled_image_is_catalogued_unverified(pool: PgPool) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag(REPO, "latest", D1);
        registry.digest(
            D1,
            DigestDescription {
                arches: vec!["amd64".into()],
                created_at: None,
                labels: BTreeMap::new(),
            },
        );
        sweep(&db, &registry, &[REPO.to_string()]).await;
        let images = store::list_images(db.pool()).await?;
        assert_eq!(images.len(), 1);
        assert!(images[0].capabilities.is_none());
        assert!(images[0].capability_digest.is_none());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_malformed_capability_label_is_catalogued_unverified(pool: PgPool) -> Result<()> {
        let db = db(pool);
        let registry = TableRegistry::new();
        registry.tag(REPO, "latest", D1);
        let mut labels = BTreeMap::new();
        labels.insert(CAPABILITIES_LABEL.to_string(), "not json".to_string());
        registry.digest(
            D1,
            DigestDescription {
                arches: vec![],
                created_at: None,
                labels,
            },
        );
        sweep(&db, &registry, &[REPO.to_string()]).await;
        let images = store::list_images(db.pool()).await?;
        assert!(images[0].capabilities.is_none());
        Ok(())
    }
}
