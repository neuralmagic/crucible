use crucible_capability::CapabilityDoc;

/// One catalogued image: a manifest digest in a watched repository, with the channel tags that
/// currently point at it and the capability document read off its config.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogImage {
    /// The repository reference without tag or digest, e.g. `ghcr.io/neuralmagic/sandbox-go-cc`.
    pub repository: String,
    /// The digest a tag resolves to: the index digest for a multi-arch image.
    pub digest: String,
    /// Channel tags pointing at this digest, sorted.
    pub tags: Vec<String>,
    /// Architectures the image is built for, sorted.
    pub arches: Vec<String>,
    /// The image config's `created` timestamp, when it carries one.
    pub created_at: Option<String>,
    /// `None` when the image carries no capability label: catalogued, unverified.
    pub capabilities: Option<CapabilityDoc>,
    /// sha256 of the label bytes as stamped, for run provenance.
    pub capability_digest: Option<String>,
    /// The `io.crucible.intro.digest` label, when present.
    pub intro_digest: Option<String>,
    pub first_seen: String,
    pub last_seen: String,
}

impl CatalogImage {
    /// The image's short name: the last path segment of the repository.
    pub fn name(&self) -> &str {
        self.repository
            .rsplit('/')
            .next()
            .unwrap_or(&self.repository)
    }
}

/// The last poll of one watched repository.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryStatus {
    pub repository: String,
    pub last_polled: String,
    /// The last poll that completed.
    pub last_ok: Option<String>,
    /// The error of the most recent poll, cleared by a completed one.
    pub last_error: Option<String>,
}
