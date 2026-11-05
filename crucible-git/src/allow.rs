//! The run's `[outputs.draft-pr]` bound, which decides every repository `crucible-git` touches:
//! a fetch, a push, or a pull request outside its target is refused, and each new pull request
//! spends one of its count. The loop writes the bound at setup; it is read once and kept, so
//! nothing the run does afterwards can widen it. No bound, no access.

use crucible_contract::git::Bounds;
use crucible_contract::outputs::{BoundViolation, OutputKind, ResolvedOutput, resolve_target};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::sync::OnceCell;

#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("the run's output bounds are not readable yet at {path}: {reason}")]
    NotYet { path: String, reason: String },
    #[error("this run declares no draft-pr bound, so crucible-git reaches no repository")]
    NoBound,
    #[error(transparent)]
    Bound(#[from] BoundViolation),
}

impl Refused {
    pub fn status(&self) -> axum::http::StatusCode {
        match self {
            Refused::NotYet { .. } => axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Refused::NoBound | Refused::Bound(_) => axum::http::StatusCode::FORBIDDEN,
        }
    }
}

struct Loaded {
    bound: ResolvedOutput,
    params: BTreeMap<String, String>,
}

pub struct Allow {
    file: PathBuf,
    loaded: OnceCell<Option<Loaded>>,
    opened: Mutex<u32>,
    created: Mutex<Created>,
}

/// The pull requests this run opened, the only ones its follow-up writes may touch.
#[derive(Default)]
struct Created {
    numbers: HashSet<(String, u64)>,
    nodes: HashSet<String>,
}

impl Allow {
    pub fn new(file: PathBuf) -> Self {
        Self {
            file,
            loaded: OnceCell::new(),
            opened: Mutex::new(0),
            created: Mutex::new(Created::default()),
        }
    }

    async fn load(&self) -> Result<&Loaded, Refused> {
        let loaded =
            self.loaded
                .get_or_try_init(|| async {
                    let raw = tokio::fs::read(&self.file)
                        .await
                        .map_err(|e| self.not_yet(e.to_string()))?;
                    let bounds: Bounds =
                        serde_json::from_slice(&raw).map_err(|e| self.not_yet(e.to_string()))?;
                    Ok::<_, Refused>(bounds.outputs.get(OutputKind::DraftPr).cloned().map(
                        |bound| Loaded {
                            bound,
                            params: bounds.params,
                        },
                    ))
                })
                .await?;
        loaded.as_ref().ok_or(Refused::NoBound)
    }

    fn not_yet(&self, reason: String) -> Refused {
        Refused::NotYet {
            path: self.file.display().to_string(),
            reason,
        }
    }

    /// Whether the run may reach `repo` (`owner/name`).
    pub async fn repo(&self, repo: &str) -> Result<(), Refused> {
        let loaded = self.load().await?;
        resolve_target(&loaded.bound, Some(repo), &loaded.params)?;
        Ok(())
    }

    /// The single repository the bound names, when it names one.
    pub async fn target(&self) -> Result<Option<String>, Refused> {
        let loaded = self.load().await?;
        match resolve_target(&loaded.bound, None, &loaded.params) {
            Ok(target) => Ok(target),
            Err(BoundViolation::NoTarget { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Spend one pull request of the bound's count on `repo`.
    pub async fn open(&self, repo: &str) -> Result<(), Refused> {
        let loaded = self.load().await?;
        resolve_target(&loaded.bound, Some(repo), &loaded.params)?;
        let mut opened = match self.opened.lock() {
            Ok(g) => g,
            Err(_) => {
                return Err(BoundViolation::CountExhausted {
                    kind: OutputKind::DraftPr,
                    count: loaded.bound.count,
                }
                .into());
            }
        };
        if *opened >= loaded.bound.count {
            return Err(BoundViolation::CountExhausted {
                kind: OutputKind::DraftPr,
                count: loaded.bound.count,
            }
            .into());
        }
        *opened += 1;
        Ok(())
    }

    /// Remember a pull request this run opened on `repo`.
    pub fn record(&self, repo: &str, number: Option<u64>, node: Option<&str>) {
        if let Ok(mut created) = self.created.lock() {
            if let Some(n) = number {
                created.numbers.insert((repo.to_ascii_lowercase(), n));
            }
            if let Some(id) = node {
                created.nodes.insert(id.to_string());
            }
        }
    }

    /// Whether this run opened pull request `number` on `repo`.
    pub fn opened_number(&self, repo: &str, number: u64) -> bool {
        self.created
            .lock()
            .is_ok_and(|c| c.numbers.contains(&(repo.to_ascii_lowercase(), number)))
    }

    /// Whether this run opened the pull request with GraphQL node id `id`.
    pub fn opened_node(&self, id: &str) -> bool {
        self.created.lock().is_ok_and(|c| c.nodes.contains(id))
    }
}
