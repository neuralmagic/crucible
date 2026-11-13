//! `cargo xtask ci plan [--changed FILE]` prints, as GitHub outputs, the CI jobs a change needs
//! and the packages they build. It fails on an include it cannot resolve to a tracked file.
//!
//! The crate graph comes from `cargo metadata`. The files each crate reads come from its own
//! sources: `include_str!`, `include_bytes!` and `include!` arguments, and string literals that
//! name a tracked path outside the crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use proc_macro2::{TokenStream, TokenTree};
use serde::Deserialize;

const ENGINE: &str = "crucible";
const CONTROLLER: &str = "crucible-controller";
const LSP: &str = "crucible-lsp";
const KUEUE_CLIENT: &str = "crucible-kueue-client";
/// What test-default-features and the default-feature clippy pass build.
const DEFAULT_FEATURE_CRATES: &[&str] = &[ENGINE, CONTROLLER, "crux"];
/// What scripts/kind-e2e.sh builds.
const KIND_E2E_CRATES: &[&str] = &[ENGINE, CONTROLLER, "crux"];

const UI: &str = "crucible-controller/ui/";
const SQLX_CACHE: &str = ".sqlx/";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{file}: {message}")]
    Tokenize { file: String, message: String },
    #[error("a source scan thread panicked")]
    ScanPanicked,
    #[error("xtask/src/ci.rs names {0}, which is not a workspace crate")]
    UnknownCrate(&'static str),
    #[error("includes the CI plan cannot follow:\n  {}", .0.join("\n  "))]
    Unresolved(Vec<String>),
    #[error("{command} failed: {stderr}")]
    Command { command: String, stderr: String },
}

struct Paths {
    files: &'static [&'static str],
    prefixes: &'static [&'static str],
}

impl Paths {
    fn has(&self, path: &str) -> bool {
        self.files.contains(&path) || self.prefixes.iter().any(|p| path.starts_with(p))
    }
}

const EVERYTHING: Paths = Paths {
    files: &[
        ".github/workflows/ci.yml",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "clippy.toml",
        "scripts/ci-jobs-test.sh",
    ],
    prefixes: &[".github/actions/", "xtask/", ".cargo/", ".config/"],
};

const GOV: Paths = Paths {
    files: &["docs/SUMMARY.md"],
    prefixes: &["gov/", "docs/rfc/", "docs/adr/"],
};

/// Pages scripts/dsl-docs.sh and scripts/state-docs.sh generate from the engine.
const GENERATED_DOCS: Paths = Paths {
    files: &[
        "docs/dsl-reference.md",
        "docs/loop-states.md",
        "docs/plan-states.md",
        "docs/img/loop-states.dot",
        "docs/img/plan-states.dot",
        "docs/img/plan-task-states.dot",
        "scripts/dsl-docs.sh",
        "scripts/state-docs.sh",
    ],
    prefixes: &[],
};

const KIND_E2E: Paths = Paths {
    files: &["scripts/kind-e2e.sh", "tools/fake-agent.py"],
    prefixes: &["e2e/kind/", "scripts/kind-e2e/", "examples/schema-repair/"],
};

/// Trees crucible-lsp/tests/packs.rs walks by joining path components.
const PACK_TREES: Paths = Paths {
    files: &[],
    prefixes: &["examples/", "e2e/"],
};

const KUEUE_PYTHON: Paths = Paths {
    files: &["scripts/crucible-kueue-py-stage.sh"],
    prefixes: &["crucible-kueue-py/"],
};

const LINT: Paths = Paths {
    files: &[
        "tools/check-dep-dups.sh",
        "tools/check-heavy-dep-allowlist.sh",
        "tools/dep-dups.snapshot",
    ],
    prefixes: &[],
};

const ELSEWHERE: Paths = Paths {
    files: &["book.toml", "mermaid-init.js", "justfile"],
    prefixes: &[
        ".github/workflows/",
        "crucible-desktop/",
        "formal/",
        "images/",
        "docker/",
        "editors/",
        "docs/",
        "skills/",
        "scripts/",
    ],
};

fn under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Paths no ci.yml job reads.
fn elsewhere(path: &str) -> bool {
    let top_level = !path.contains('/');
    ELSEWHERE.has(path)
        || (top_level && (path.ends_with(".md") || path.starts_with("LICENSE-")))
        || (path.starts_with("tools/") && path.ends_with(".nu"))
}

/// `base/rel` with `.` and `..` resolved; `None` when it leaves the repo or names its root.
fn resolve(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in base.split('/').chain(rel.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[derive(Debug, Clone)]
struct Crate {
    name: String,
    dir: String,
    lib: bool,
    sqlx: bool,
    /// Directories of its path dependencies, workspace members or not.
    deps: Vec<String>,
}

#[derive(Debug)]
struct Workspace {
    crates: Vec<Crate>,
}

#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    packages: Vec<MetaPackage>,
}

#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<MetaDependency>,
    targets: Vec<MetaTarget>,
}

#[derive(Deserialize)]
struct MetaDependency {
    name: String,
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct MetaTarget {
    kind: Vec<String>,
}

fn repo_relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.iter().map(|part| part.to_str()).collect();
    Some(parts?.join("/"))
}

impl Workspace {
    fn from_metadata(meta: &Metadata) -> Result<Self> {
        let mut crates = Vec::new();
        for package in &meta.packages {
            let dir = package
                .manifest_path
                .parent()
                .and_then(|dir| repo_relative(&meta.workspace_root, dir))
                .with_context(|| format!("{} is outside the workspace", package.name))?;
            crates.push(Crate {
                name: package.name.clone(),
                dir,
                lib: package
                    .targets
                    .iter()
                    .any(|target| target.kind.iter().any(|kind| kind == "lib")),
                sqlx: package.dependencies.iter().any(|dep| dep.name == "sqlx"),
                deps: package
                    .dependencies
                    .iter()
                    .filter_map(|dep| repo_relative(&meta.workspace_root, dep.path.as_deref()?))
                    .collect(),
            });
        }
        Ok(Self { crates })
    }

    fn named(&self, name: &str) -> Option<&Crate> {
        self.crates.iter().find(|krate| krate.name == name)
    }

    fn crate_at(&self, path: &str) -> Option<&Crate> {
        self.crates
            .iter()
            .filter(|krate| under(path, &krate.dir))
            .max_by_key(|krate| krate.dir.len())
    }

    /// Crates with a path dependency outside the workspace in the same top-level directory as
    /// `path`.
    fn vendored_into(&self, path: &str) -> impl Iterator<Item = &Crate> {
        let top = path.split('/').next().unwrap_or(path);
        self.crates.iter().filter(move |krate| {
            krate
                .deps
                .iter()
                .any(|dep| self.crate_at(dep).is_none() && dep.split('/').next() == Some(top))
        })
    }

    /// `changed` plus every crate that depends on one of them, dev-dependencies included.
    fn affected(&self, changed: &BTreeSet<String>) -> BTreeSet<String> {
        let mut affected = changed.clone();
        loop {
            let before = affected.len();
            for krate in &self.crates {
                let depends = krate.deps.iter().any(|dep| {
                    self.crates
                        .iter()
                        .any(|other| other.dir == *dep && affected.contains(&other.name))
                });
                if depends {
                    affected.insert(krate.name.clone());
                }
            }
            if affected.len() == before {
                return affected;
            }
        }
    }
}

/// The tracked files and the directories that hold them.
#[derive(Debug, Default)]
struct Tree {
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
}

impl Tree {
    fn new(files: impl IntoIterator<Item = String>) -> Self {
        let mut tree = Self::default();
        for file in files {
            let mut dir = file.as_str();
            while let Some((parent, _)) = dir.rsplit_once('/') {
                if !tree.dirs.insert(parent.to_string()) {
                    break;
                }
                dir = parent;
            }
            tree.files.insert(file);
        }
        tree
    }
}

enum Found {
    Include(String),
    /// An include whose argument is not a string literal, as source text.
    ComputedInclude(String),
    Literal(String),
}

fn string_value(token: &TokenTree) -> Option<String> {
    let TokenTree::Literal(literal) = token else {
        return None;
    };
    let text = literal.to_string();
    if let Some(plain) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        && !plain.contains('\\')
    {
        return Some(plain.to_string());
    }
    if !text.starts_with(['"', 'r']) {
        return None;
    }
    syn::parse2::<syn::LitStr>(TokenStream::from(token.clone()))
        .ok()
        .map(|literal| literal.value())
}

fn is_include(tokens: &[TokenTree], group_at: usize) -> bool {
    let Some(at) = group_at.checked_sub(2) else {
        return false;
    };
    matches!(&tokens[at + 1], TokenTree::Punct(bang) if bang.as_char() == '!')
        && matches!(&tokens[at], TokenTree::Ident(name)
            if name == "include_str" || name == "include_bytes" || name == "include")
}

fn find(stream: TokenStream, found: &mut Vec<Found>) {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    for (at, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) if is_include(&tokens, at) => {
                match group
                    .stream()
                    .into_iter()
                    .next()
                    .as_ref()
                    .and_then(string_value)
                {
                    Some(path) => found.push(Found::Include(path)),
                    None => found.push(Found::ComputedInclude(group.stream().to_string())),
                }
            }
            TokenTree::Group(group) => find(group.stream(), found),
            token => {
                if let Some(value) = string_value(token) {
                    found.push(Found::Literal(value));
                }
            }
        }
    }
}

fn found_in(file: &str, source: &str) -> Result<Vec<Found>> {
    let stream: TokenStream =
        source
            .parse()
            .map_err(|err: proc_macro2::LexError| Error::Tokenize {
                file: file.into(),
                message: err.to_string(),
            })?;
    let mut found = Vec::new();
    find(stream, &mut found);
    Ok(found)
}

/// What each of `files` holds, in the same order, tokenized on every core.
fn found_in_files(root: &Path, files: &[&str]) -> Result<Vec<Vec<Found>>> {
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let next = AtomicUsize::new(0);
    let work = || {
        let mut done = Vec::new();
        while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
            let source = std::fs::read_to_string(root.join(file))
                .with_context(|| format!("reading {file}"))?;
            done.push((*file, found_in(file, &source)?));
        }
        Ok(done)
    };
    let mut found: BTreeMap<&str, Vec<Found>> = BTreeMap::new();
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..threads).map(|_| scope.spawn(work)).collect();
        for worker in workers {
            let done: Result<Vec<_>> = worker.join().map_err(|_| Error::ScanPanicked)?;
            found.extend(done?);
        }
        Ok(())
    })?;
    Ok(files
        .iter()
        .map(|file| found.remove(file).unwrap_or_default())
        .collect())
}

fn looks_like_path(literal: &str) -> bool {
    literal.len() <= 200
        && literal.contains('/')
        && literal
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// The tracked paths each crate's sources name.
#[derive(Debug, Default)]
struct Reads {
    by_target: BTreeMap<String, BTreeSet<String>>,
    /// Includes that resolve to no tracked file.
    unresolved: Vec<String>,
}

impl Reads {
    #[cfg(test)]
    fn scan(&mut self, krate: &Crate, file: &str, source: &str, tree: &Tree) -> Result<()> {
        self.record(krate, file, found_in(file, source)?, tree);
        Ok(())
    }

    fn record(&mut self, krate: &Crate, file: &str, found: Vec<Found>, tree: &Tree) {
        let source_dir = file.rsplit_once('/').map_or("", |(dir, _)| dir);
        for item in found {
            match item {
                Found::Include(path) => match resolve(source_dir, &path) {
                    Some(target) if tree.files.contains(&target) => self.add(target, krate),
                    _ => self
                        .unresolved
                        .push(format!("{file}: \"{path}\" is not a tracked file")),
                },
                Found::ComputedInclude(argument) => {
                    if !argument.contains("OUT_DIR") {
                        self.unresolved
                            .push(format!("{file}: include of `{argument}` is not a literal"));
                    }
                }
                Found::Literal(literal) => {
                    if !looks_like_path(&literal) {
                        continue;
                    }
                    for base in [krate.dir.as_str(), ""] {
                        if let Some(target) = resolve(base, &literal)
                            && (tree.files.contains(&target)
                                || (tree.dirs.contains(&target) && target.contains('/')))
                            && !under(&target, &krate.dir)
                        {
                            self.add(target, krate);
                        }
                    }
                }
            }
        }
    }

    fn add(&mut self, target: String, krate: &Crate) {
        self.by_target
            .entry(target)
            .or_default()
            .insert(krate.name.clone());
    }

    /// Crates that name `path`, or with `inside` a directory holding it.
    fn readers(&self, path: &str, inside: bool) -> BTreeSet<&str> {
        self.by_target
            .iter()
            .filter(|(target, _)| *target == path || (inside && under(path, target)))
            .flat_map(|(_, crates)| crates.iter().map(String::as_str))
            .collect()
    }
}

#[derive(Debug, Default)]
struct Selection {
    everything: bool,
    crates: BTreeSet<String>,
    lint: bool,
    gov: bool,
    kind_e2e: bool,
    ui: bool,
    kueue_python: bool,
    audit: bool,
}

#[derive(Debug, PartialEq)]
pub struct Jobs {
    /// Package arguments for the all-features build, tests and clippy; empty for none.
    packages: String,
    /// The same for doctests, which only library crates have.
    doc_packages: String,
    /// The same for the default-feature tests and clippy.
    default_packages: String,
    /// The controller is built: its tests need Postgres, Keycloak and Vault, and the sqlx check.
    controller: bool,
    /// The engine is built: the pages generated from it are checked.
    generated_docs: bool,
    lint: bool,
    gov: bool,
    kind_e2e: bool,
    ui: bool,
    kueue_python: bool,
    audit: bool,
}

impl fmt::Display for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "packages={}", self.packages)?;
        writeln!(f, "doc_packages={}", self.doc_packages)?;
        writeln!(f, "default_packages={}", self.default_packages)?;
        writeln!(f, "controller={}", self.controller)?;
        writeln!(f, "generated_docs={}", self.generated_docs)?;
        writeln!(f, "lint={}", self.lint)?;
        writeln!(f, "gov={}", self.gov)?;
        writeln!(f, "kind_e2e={}", self.kind_e2e)?;
        writeln!(f, "ui={}", self.ui)?;
        writeln!(f, "kueue_python={}", self.kueue_python)?;
        writeln!(f, "audit={}", self.audit)
    }
}

fn package_args<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    names
        .into_iter()
        .map(|name| format!("-p {name}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Planner {
    workspace: Workspace,
    reads: Reads,
}

impl Planner {
    pub fn load() -> Result<Self> {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let meta =
            output(Command::new(cargo).args(["metadata", "--format-version", "1", "--no-deps"]))?;
        let meta: Metadata = serde_json::from_slice(&meta).context("parsing cargo metadata")?;
        let workspace = Workspace::from_metadata(&meta)?;
        let root = &meta.workspace_root;
        let listed = output(
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["ls-files", "-z"]),
        )?;
        let listed = String::from_utf8(listed).context("git ls-files printed a non-UTF-8 path")?;
        let tree = Tree::new(
            listed
                .split('\0')
                .filter(|f| !f.is_empty())
                .map(String::from),
        );
        let sources: Vec<(&Crate, &str)> = tree
            .files
            .iter()
            .filter(|file| file.ends_with(".rs"))
            .filter_map(|file| Some((workspace.crate_at(file)?, file.as_str())))
            .collect();
        let files: Vec<&str> = sources.iter().map(|(_, file)| *file).collect();
        let mut reads = Reads::default();
        for ((krate, file), found) in sources.iter().zip(found_in_files(root, &files)?) {
            reads.record(krate, file, found, &tree);
        }
        let planner = Self { workspace, reads };
        planner.validate()?;
        Ok(planner)
    }

    /// The jobs `changed` needs and why, one line per path; `None` is every job.
    pub fn plan(&self, changed: Option<&[String]>) -> (Jobs, Vec<String>) {
        let mut selection = Selection::default();
        let mut why = Vec::new();
        match changed {
            None => selection.everything = true,
            Some(paths) => {
                for path in paths {
                    let reasons = self.select(path, &mut selection);
                    why.push(format!("{path}: {}", reasons.join(", ")));
                }
            }
        }
        (self.jobs(&selection), why)
    }

    fn select(&self, path: &str, selection: &mut Selection) -> Vec<String> {
        if EVERYTHING.has(path) {
            selection.everything = true;
            return vec!["everything".into()];
        }
        let mut reasons = Vec::new();
        let readme = file_name(path) == "README.md";
        for reader in self.reads.readers(path, !readme) {
            selection.crates.insert(reader.to_string());
            reasons.push(format!("read by {reader}"));
        }
        if readme {
            reasons.push("documentation".into());
            return reasons;
        }
        if path.starts_with(UI) {
            selection.ui = true;
            reasons.push("ui".into());
            return reasons;
        }
        if GOV.has(path) {
            selection.gov = true;
            reasons.push("gov".into());
        }
        if GENERATED_DOCS.has(path) {
            selection.lint = true;
            selection.crates.insert(ENGINE.into());
            reasons.push(format!("generated from {ENGINE}"));
        }
        if KIND_E2E.has(path) {
            selection.kind_e2e = true;
            reasons.push("kind-e2e".into());
        }
        if PACK_TREES.has(path) {
            selection.crates.insert(LSP.into());
            reasons.push(format!("walked by {LSP}"));
        }
        if KUEUE_PYTHON.has(path) {
            selection.kueue_python = true;
            reasons.push("kueue-python".into());
        }
        if path == "deny.toml" {
            selection.audit = true;
            reasons.push("audit".into());
        }
        let name = file_name(path);
        if LINT.has(path) || name.contains("Containerfile") || name.contains("Dockerfile") {
            selection.lint = true;
            reasons.push("lint".into());
        }
        if let Some(krate) = self.workspace.crate_at(path) {
            selection.crates.insert(krate.name.clone());
            reasons.push(format!("in {}", krate.name));
            if name == "Cargo.toml" {
                selection.audit = true;
                reasons.push("audit".into());
            }
        }
        if path.starts_with(SQLX_CACHE) {
            for krate in self.workspace.crates.iter().filter(|krate| krate.sqlx) {
                selection.crates.insert(krate.name.clone());
                reasons.push(format!("query cache of {}", krate.name));
            }
        }
        for krate in self.workspace.vendored_into(path) {
            selection.crates.insert(krate.name.clone());
            reasons.push(format!("vendored into {}", krate.name));
        }
        if reasons.is_empty() && elsewhere(path) {
            reasons.push("nothing".into());
        }
        if reasons.is_empty() {
            selection.everything = true;
            reasons.push("unknown path, everything".into());
        }
        reasons
    }

    fn jobs(&self, selection: &Selection) -> Jobs {
        let crates = &self.workspace.crates;
        let affected: BTreeSet<String> = if selection.everything {
            crates.iter().map(|krate| krate.name.clone()).collect()
        } else {
            self.workspace.affected(&selection.crates)
        };
        let has = |name: &str| affected.contains(name);
        let all = selection.everything;
        let libs = affected
            .iter()
            .map(String::as_str)
            .filter(|name| self.workspace.named(name).is_some_and(|krate| krate.lib));
        Jobs {
            packages: if all {
                "--workspace".into()
            } else {
                package_args(affected.iter().map(String::as_str))
            },
            doc_packages: if all {
                "--workspace".into()
            } else {
                package_args(libs)
            },
            default_packages: package_args(
                DEFAULT_FEATURE_CRATES
                    .iter()
                    .copied()
                    .filter(|name| has(name)),
            ),
            controller: has(CONTROLLER),
            generated_docs: has(ENGINE),
            lint: all || selection.lint || !affected.is_empty(),
            gov: all || selection.gov,
            kind_e2e: all || selection.kind_e2e || KIND_E2E_CRATES.iter().any(|name| has(name)),
            ui: all || selection.ui || has(CONTROLLER),
            kueue_python: all || selection.kueue_python || has(KUEUE_CLIENT),
            audit: all || selection.audit,
        }
    }

    /// Fails when a rule names a crate that is gone or an include cannot be resolved.
    fn validate(&self) -> Result<(), Error> {
        let named = [ENGINE, CONTROLLER, LSP, KUEUE_CLIENT]
            .into_iter()
            .chain(DEFAULT_FEATURE_CRATES.iter().copied())
            .chain(KIND_E2E_CRATES.iter().copied());
        for name in named {
            if self.workspace.named(name).is_none() {
                return Err(Error::UnknownCrate(name));
            }
        }
        if !self.reads.unresolved.is_empty() {
            return Err(Error::Unresolved(self.reads.unresolved.clone()));
        }
        Ok(())
    }
}

fn output(command: &mut Command) -> Result<Vec<u8>> {
    let out = command
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if !out.status.success() {
        return Err(Error::Command {
            command: format!("{command:?}"),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
        .into());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use crate::ci::*;

    fn krate(name: &str, dir: &str, lib: bool, deps: &[&str]) -> Crate {
        Crate {
            name: name.into(),
            dir: dir.into(),
            lib,
            sqlx: name == CONTROLLER,
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            crates: vec![
                krate("crucible-contract", "crucible-contract", true, &[]),
                krate(
                    ENGINE,
                    "crucible",
                    true,
                    &["crucible-contract", "third_party/mermaid"],
                ),
                krate("crux", "crux", true, &["crucible-contract"]),
                krate(
                    CONTROLLER,
                    "crucible-controller",
                    true,
                    &["crucible", "crux"],
                ),
                krate(LSP, "crucible-lsp", true, &["crucible"]),
                krate(
                    KUEUE_CLIENT,
                    "crucible-kueue-client",
                    true,
                    &["crucible-contract"],
                ),
                krate("buildit", "buildit", false, &[]),
                krate("forge", "forge", true, &[]),
            ],
        }
    }

    const TREE: &[&str] = &[
        "README.md",
        "docs/crucible-contract.md",
        "docs/operators.md",
        "examples/counter/crucible.toml",
        "examples/counter/README.md",
        "examples/counter/prompts/goal.md",
        "buildit/README.md",
        "buildit/deploy/policy.toml",
        "buildit/src/main.rs",
        "crucible/README.md",
        "crucible/src/lib.rs",
        "crucible/src/scope/transcript.rs",
        "crucible/src/prompts/judge.md",
        "forge/src/github.rs",
        ".github/workflows/crucible-build.yml",
    ];

    fn tree() -> Tree {
        Tree::new(TREE.iter().map(|file| file.to_string()))
    }

    fn scanned(krate_name: &str, file: &str, source: &str) -> Reads {
        let workspace = workspace();
        let mut reads = Reads::default();
        reads
            .scan(workspace.named(krate_name).unwrap(), file, source, &tree())
            .unwrap();
        reads
    }

    fn planner() -> Planner {
        let workspace = workspace();
        let tree = tree();
        let mut reads = Reads::default();
        let sources = [
            (
                ENGINE,
                "crucible/src/scope/transcript.rs",
                r#"const A: &str = include_str!("../../../docs/crucible-contract.md");
                   const B: &str = include_str!("../prompts/judge.md");
                   fn pack() -> PathBuf { root().join("../examples/counter") }
                   const P: &str = include_str!("../../../buildit/deploy/policy.toml");"#,
            ),
            (
                "forge",
                "forge/src/github.rs",
                r#"const W: &str = include_str!("../../.github/workflows/crucible-build.yml");"#,
            ),
        ];
        for (name, file, source) in sources {
            reads
                .scan(workspace.named(name).unwrap(), file, source, &tree)
                .unwrap();
        }
        Planner { workspace, reads }
    }

    fn plan(paths: &[&str]) -> Jobs {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        planner().plan(Some(&paths)).0
    }

    fn nothing() -> Jobs {
        Jobs {
            packages: String::new(),
            doc_packages: String::new(),
            default_packages: String::new(),
            controller: false,
            generated_docs: false,
            lint: false,
            gov: false,
            kind_e2e: false,
            ui: false,
            kueue_python: false,
            audit: false,
        }
    }

    fn everything() -> Jobs {
        Jobs {
            packages: "--workspace".into(),
            doc_packages: "--workspace".into(),
            default_packages: "-p crucible -p crucible-controller -p crux".into(),
            controller: true,
            generated_docs: true,
            lint: true,
            gov: true,
            kind_e2e: true,
            ui: true,
            kueue_python: true,
            audit: true,
        }
    }

    fn engine_and_dependents() -> Jobs {
        Jobs {
            packages: "-p crucible -p crucible-controller -p crucible-lsp".into(),
            doc_packages: "-p crucible -p crucible-controller -p crucible-lsp".into(),
            default_packages: "-p crucible -p crucible-controller".into(),
            controller: true,
            generated_docs: true,
            lint: true,
            kind_e2e: true,
            ui: true,
            ..nothing()
        }
    }

    #[test]
    fn resolve_follows_dot_segments() {
        assert_eq!(resolve("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(
            resolve("a", "/../docs/./x.md").as_deref(),
            Some("docs/x.md")
        );
        assert_eq!(resolve("", "docs/x.md").as_deref(), Some("docs/x.md"));
        assert_eq!(resolve("a", "../.."), None);
        assert_eq!(resolve("a", ".."), None);
    }

    #[test]
    fn under_matches_whole_components() {
        assert!(under("crucible/src/lib.rs", "crucible"));
        assert!(under("crucible", "crucible"));
        assert!(!under("crucible-controller/src/lib.rs", "crucible"));
    }

    #[test]
    fn the_longest_crate_directory_owns_a_path() {
        let mut workspace = workspace();
        workspace
            .crates
            .push(krate("nested", "crucible/nested", true, &[]));
        assert_eq!(
            workspace
                .crate_at("crucible/nested/src/lib.rs")
                .unwrap()
                .name,
            "nested"
        );
        assert_eq!(
            workspace.crate_at("crucible/src/lib.rs").unwrap().name,
            ENGINE
        );
        assert!(workspace.crate_at("docs/x.md").is_none());
    }

    #[test]
    fn affected_follows_reverse_dependencies_to_a_fixed_point() {
        let workspace = workspace();
        let of = |name: &str| {
            workspace
                .affected(&BTreeSet::from([name.to_string()]))
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(of("buildit"), ["buildit"]);
        assert_eq!(of(CONTROLLER), [CONTROLLER]);
        assert_eq!(of("crux"), [CONTROLLER, "crux"]);
        assert_eq!(of(ENGINE), [ENGINE, CONTROLLER, LSP]);
        assert_eq!(
            of("crucible-contract"),
            [
                ENGINE,
                "crucible-contract",
                CONTROLLER,
                KUEUE_CLIENT,
                LSP,
                "crux"
            ]
        );
    }

    #[test]
    fn metadata_gives_directories_libraries_and_path_dependencies() {
        let meta: Metadata = serde_json::from_value(serde_json::json!({
            "workspace_root": "/repo",
            "packages": [
                {
                    "name": "crucible-capability",
                    "manifest_path": "/repo/capability/Cargo.toml",
                    "dependencies": [],
                    "targets": [{"kind": ["lib"]}]
                },
                {
                    "name": "crucible-controller",
                    "manifest_path": "/repo/crucible-controller/Cargo.toml",
                    "dependencies": [
                        {"name": "crucible-capability", "path": "/repo/capability"},
                        {"name": "sqlx", "path": null},
                        {"name": "elsewhere", "path": "/other/elsewhere"}
                    ],
                    "targets": [{"kind": ["bin"]}, {"kind": ["custom-build"]}]
                }
            ]
        }))
        .unwrap();
        let workspace = Workspace::from_metadata(&meta).unwrap();
        let capability = workspace.named("crucible-capability").unwrap();
        assert_eq!(capability.dir, "capability");
        assert!(capability.lib && !capability.sqlx);
        let controller = workspace.named(CONTROLLER).unwrap();
        assert!(!controller.lib && controller.sqlx);
        assert_eq!(controller.deps, ["capability"]);
        assert_eq!(
            workspace
                .affected(&BTreeSet::from(["crucible-capability".to_string()]))
                .len(),
            2
        );
    }

    #[test]
    fn a_package_outside_the_workspace_root_is_an_error() {
        let meta: Metadata = serde_json::from_value(serde_json::json!({
            "workspace_root": "/repo",
            "packages": [{
                "name": "stray",
                "manifest_path": "/other/stray/Cargo.toml",
                "dependencies": [],
                "targets": []
            }]
        }))
        .unwrap();
        let err = Workspace::from_metadata(&meta).unwrap_err().to_string();
        assert!(err.contains("stray is outside the workspace"), "{err}");
    }

    #[test]
    fn includes_resolve_against_the_source_file() {
        let reads = scanned(
            ENGINE,
            "crucible/src/scope/transcript.rs",
            r##"
            const A: &str = include_str!("../../../docs/crucible-contract.md");
            const B: &[u8] = include_bytes!(
                "../prompts/judge.md"
            );
            const C: &str = include_str!(r#"../../README.md"#);
            fn nested() { assert_eq!(include_str!("../lib.rs"), ""); }
            "##,
        );
        assert!(reads.unresolved.is_empty(), "{:?}", reads.unresolved);
        assert_eq!(
            reads.by_target.keys().collect::<Vec<_>>(),
            [
                "crucible/README.md",
                "crucible/src/lib.rs",
                "crucible/src/prompts/judge.md",
                "docs/crucible-contract.md"
            ]
        );
    }

    #[test]
    fn an_include_the_plan_cannot_follow_is_reported() {
        let reads = scanned(
            ENGINE,
            "crucible/src/lib.rs",
            r#"
            const GONE: &str = include_str!("../missing.md");
            const BUILT: &str = include_str!(concat!(env!("OUT_DIR"), "/built.rs"));
            const NAMED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
            include!("generated.rs");
            "#,
        );
        assert!(reads.by_target.is_empty());
        assert_eq!(reads.unresolved.len(), 3, "{:?}", reads.unresolved);
        assert!(reads.unresolved[0].contains("\"../missing.md\" is not a tracked file"));
        assert!(reads.unresolved[1].contains("is not a literal"));
        assert!(reads.unresolved[2].contains("\"generated.rs\" is not a tracked file"));
    }

    #[test]
    fn path_literals_resolve_against_the_crate_and_the_repo_root() {
        let reads = scanned(
            ENGINE,
            "crucible/tests/example.rs",
            r#"
            fn a() { root().join("../examples/counter"); }
            fn b() { concat!(env!("CARGO_MANIFEST_DIR"), "/../buildit/deploy/policy.toml"); }
            fn c() { repo().join("docs/operators.md"); }
            "#,
        );
        assert_eq!(
            reads.by_target.keys().collect::<Vec<_>>(),
            [
                "buildit/deploy/policy.toml",
                "docs/operators.md",
                "examples/counter"
            ]
        );
    }

    #[test]
    fn literals_that_are_not_paths_to_another_tracked_place_are_ignored() {
        let reads = scanned(
            ENGINE,
            "crucible/src/lib.rs",
            r#"
            /// docs/operators.md
            //! see docs/operators.md for more
            fn a() {
                let _ = "docs";
                let _ = "docs/";
                let _ = "/examples";
                let _ = "src/lib.rs";
                let _ = "../crucible/src/lib.rs";
                let _ = "docs/missing.md";
                let _ = "../../outside/x";
                let _ = format!("docs/{}.md", 1);
                let _ = "see docs/operators.md";
                let _ = b"docs/operators.md";
                let _ = 'd';
                let _ = 42;
            }
            "#,
        );
        assert!(reads.by_target.is_empty(), "{:?}", reads.by_target);
    }

    #[test]
    fn a_source_that_does_not_tokenize_is_an_error() {
        let workspace = workspace();
        let err = Reads::default()
            .scan(
                workspace.named(ENGINE).unwrap(),
                "crucible/src/lib.rs",
                "fn a() { \"unterminated }",
                &tree(),
            )
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("crucible/src/lib.rs: "), "{err}");
    }

    #[test]
    fn documentation_selects_nothing() {
        for path in [
            "README.md",
            "CONTRIBUTING.md",
            "LICENSE-MIT",
            "buildit/README.md",
            "crucible/README.md",
            "crucible-controller/ui/README.md",
            "examples/counter/README.md",
            "docs/operators.md",
            "docs/img/controller-run.png",
            "docs/theme/crucible.css",
            "skills/draft-a-playbook/SKILL.md",
        ] {
            assert_eq!(plan(&[path]), nothing(), "{path}");
        }
    }

    #[test]
    fn a_compiled_in_document_selects_the_crate_that_includes_it() {
        assert_eq!(
            plan(&["docs/crucible-contract.md"]),
            engine_and_dependents()
        );
        assert_eq!(
            plan(&["crucible/src/prompts/judge.md"]),
            engine_and_dependents()
        );
    }

    #[test]
    fn a_readme_selects_only_a_crate_that_names_it() {
        let mut planner = planner();
        let engine = planner.workspace.named(ENGINE).unwrap().clone();
        planner.reads.add("crucible/README.md".into(), &engine);
        let jobs = |path: &str| planner.plan(Some(&[path.to_string()])).0;
        assert_eq!(jobs("crucible/README.md"), engine_and_dependents());
        assert_eq!(jobs("examples/counter/README.md"), nothing());
    }

    #[test]
    fn a_leaf_crate_selects_only_itself() {
        let jobs = Jobs {
            packages: "-p buildit".into(),
            lint: true,
            ..nothing()
        };
        assert_eq!(plan(&["buildit/src/main.rs"]), jobs);
        assert_eq!(plan(&["buildit/src/main.rs", "buildit/README.md"]), jobs);
        assert_eq!(
            plan(&["buildit/Cargo.toml"]),
            Jobs {
                packages: "-p buildit".into(),
                lint: true,
                audit: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn a_file_another_crate_reads_selects_both() {
        assert_eq!(
            plan(&["buildit/deploy/policy.toml"]),
            Jobs {
                packages: "-p buildit -p crucible -p crucible-controller -p crucible-lsp".into(),
                ..engine_and_dependents()
            }
        );
    }

    #[test]
    fn the_controller_selects_its_services_ui_and_kind_e2e() {
        let jobs = Jobs {
            packages: "-p crucible-controller".into(),
            doc_packages: "-p crucible-controller".into(),
            default_packages: "-p crucible-controller".into(),
            controller: true,
            lint: true,
            kind_e2e: true,
            ui: true,
            ..nothing()
        };
        assert_eq!(plan(&["crucible-controller/src/spa.rs"]), jobs);
        assert_eq!(plan(&["crucible-controller/migrations/0049_x.sql"]), jobs);
        assert_eq!(plan(&[".sqlx/query-abc.json"]), jobs);
    }

    #[test]
    fn a_crate_the_controller_depends_on_selects_the_controller_too() {
        assert_eq!(
            plan(&["crux/src/lib.rs"]),
            Jobs {
                packages: "-p crucible-controller -p crux".into(),
                doc_packages: "-p crucible-controller -p crux".into(),
                default_packages: "-p crucible-controller -p crux".into(),
                controller: true,
                lint: true,
                kind_e2e: true,
                ui: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn a_widely_used_crate_selects_every_dependent() {
        let packages = "-p crucible -p crucible-contract -p crucible-controller \
                        -p crucible-kueue-client -p crucible-lsp -p crux";
        assert_eq!(
            plan(&["crucible-contract/src/lib.rs"]),
            Jobs {
                packages: packages.into(),
                doc_packages: packages.into(),
                default_packages: "-p crucible -p crucible-controller -p crux".into(),
                controller: true,
                generated_docs: true,
                lint: true,
                kind_e2e: true,
                ui: true,
                kueue_python: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn a_vendored_dependency_selects_the_crates_that_use_it() {
        assert_eq!(
            plan(&["third_party/mermaid/src/lib.rs"]),
            engine_and_dependents()
        );
        assert_eq!(
            plan(&["third_party/dagre/src/lib.rs"]),
            engine_and_dependents()
        );
        assert_eq!(plan(&["third_party/NOTICE"]), engine_and_dependents());
    }

    #[test]
    fn the_ui_selects_only_the_ui_job() {
        assert_eq!(
            plan(&["crucible-controller/ui/src/App.tsx", "README.md"]),
            Jobs {
                ui: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn governance_sources_select_only_the_gov_job() {
        let jobs = Jobs {
            gov: true,
            ..nothing()
        };
        assert_eq!(
            plan(&["gov/rfc/RFC-0006.toml", "docs/rfc/RFC-0006.md"]),
            jobs
        );
        assert_eq!(plan(&["docs/adr/ADR-0075.md"]), jobs);
        assert_eq!(plan(&["docs/SUMMARY.md"]), jobs);
    }

    #[test]
    fn generated_pages_select_the_engine() {
        for path in [
            "docs/dsl-reference.md",
            "docs/img/plan-states.dot",
            "scripts/state-docs.sh",
        ] {
            assert_eq!(plan(&[path]), engine_and_dependents(), "{path}");
        }
    }

    #[test]
    fn kind_fixtures_select_kind_e2e() {
        let lsp = Jobs {
            packages: "-p crucible-lsp".into(),
            doc_packages: "-p crucible-lsp".into(),
            lint: true,
            kind_e2e: true,
            ..nothing()
        };
        assert_eq!(plan(&["e2e/kind/cluster.yaml"]), lsp);
        assert_eq!(plan(&["examples/schema-repair/workflow.star"]), lsp);
        let only = Jobs {
            kind_e2e: true,
            ..nothing()
        };
        assert_eq!(plan(&["scripts/kind-e2e.sh"]), only);
        assert_eq!(plan(&["scripts/kind-e2e/store.sh"]), only);
        assert_eq!(plan(&["tools/fake-agent.py"]), only);
    }

    #[test]
    fn pack_trees_select_the_lsp_and_their_readers() {
        assert_eq!(
            plan(&["e2e/kueue/pack/workflow.star"]),
            Jobs {
                packages: "-p crucible-lsp".into(),
                doc_packages: "-p crucible-lsp".into(),
                lint: true,
                ..nothing()
            }
        );
        assert_eq!(
            plan(&["examples/counter/crucible.toml"]),
            engine_and_dependents()
        );
        assert_eq!(
            plan(&["examples/counter/prompts/goal.md"]),
            engine_and_dependents()
        );
    }

    #[test]
    fn the_python_client_selects_kueue_python() {
        let jobs = Jobs {
            kueue_python: true,
            ..nothing()
        };
        assert_eq!(plan(&["crucible-kueue-py/src/lib.rs"]), jobs);
        assert_eq!(plan(&["scripts/crucible-kueue-py-stage.sh"]), jobs);
        assert_eq!(
            plan(&["crucible-kueue-client/src/lib.rs"]),
            Jobs {
                packages: "-p crucible-kueue-client".into(),
                doc_packages: "-p crucible-kueue-client".into(),
                lint: true,
                kueue_python: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn the_deny_policy_selects_audit() {
        assert_eq!(
            plan(&["deny.toml"]),
            Jobs {
                audit: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn lint_inputs_select_lint_without_a_build() {
        let jobs = Jobs {
            lint: true,
            ..nothing()
        };
        assert_eq!(plan(&["tools/dep-dups.snapshot"]), jobs);
        assert_eq!(plan(&["tools/check-dep-dups.sh"]), jobs);
        assert_eq!(plan(&["docker/Containerfile.controller"]), jobs);
        assert_eq!(
            plan(&["e2e/kind/Containerfile.loop"]),
            Jobs {
                packages: "-p crucible-lsp".into(),
                doc_packages: "-p crucible-lsp".into(),
                lint: true,
                kind_e2e: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn paths_other_workflows_own_select_nothing() {
        for path in [
            ".github/workflows/desktop.yml",
            "crucible-desktop/src/lib.rs",
            "crucible-desktop/Cargo.toml",
            "formal/Plan.tla",
            "images/matrix.toml",
            "docker/entrypoint.sh",
            "editors/vscode/extension.js",
            "scripts/optimize-loop.sh",
            "tools/steer.nu",
            "book.toml",
            "justfile",
        ] {
            assert_eq!(plan(&[path]), nothing(), "{path}");
        }
    }

    #[test]
    fn a_workflow_a_crate_compiles_in_selects_that_crate() {
        assert_eq!(
            plan(&[".github/workflows/crucible-build.yml"]),
            Jobs {
                packages: "-p forge".into(),
                doc_packages: "-p forge".into(),
                lint: true,
                ..nothing()
            }
        );
    }

    #[test]
    fn build_inputs_and_the_plan_itself_select_everything() {
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "clippy.toml",
            ".cargo/config.toml",
            ".config/nextest.toml",
            ".github/workflows/ci.yml",
            ".github/actions/postgres/action.yml",
            "xtask/src/ci.rs",
            "scripts/ci-jobs-test.sh",
        ] {
            assert_eq!(plan(&[path]), everything(), "{path}");
            assert_eq!(plan(&["README.md", path]), everything(), "{path}");
        }
    }

    #[test]
    fn an_unknown_path_selects_everything() {
        for path in [
            ".gitignore",
            "vendor/rune/src/lib.rs",
            "new-dir/file.txt",
            "tools/new-tool.py",
            "notes.txt",
        ] {
            assert_eq!(plan(&[path]), everything(), "{path}");
        }
    }

    #[test]
    fn no_change_list_selects_everything_and_an_empty_one_nothing() {
        assert_eq!(planner().plan(None).0, everything());
        assert_eq!(plan(&[]), nothing());
    }

    #[test]
    fn the_plan_says_why_each_path_selects_what_it_does() {
        let paths = [
            "docs/crucible-contract.md".to_string(),
            "docs/operators.md".to_string(),
            "notes.txt".to_string(),
        ];
        assert_eq!(
            planner().plan(Some(&paths)).1,
            [
                "docs/crucible-contract.md: read by crucible",
                "docs/operators.md: nothing",
                "notes.txt: unknown path, everything",
            ]
        );
    }

    #[test]
    fn jobs_print_as_github_outputs() {
        assert_eq!(
            plan(&["crux/src/lib.rs"]).to_string(),
            "packages=-p crucible-controller -p crux\n\
             doc_packages=-p crucible-controller -p crux\n\
             default_packages=-p crucible-controller -p crux\n\
             controller=true\n\
             generated_docs=false\n\
             lint=true\n\
             gov=false\n\
             kind_e2e=true\n\
             ui=true\n\
             kueue_python=false\n\
             audit=false\n"
        );
    }

    #[test]
    fn a_tree_the_plan_can_follow_is_valid() {
        planner().validate().unwrap();
    }

    #[test]
    fn an_include_the_plan_cannot_follow_is_invalid() {
        let mut planner = planner();
        planner
            .reads
            .unresolved
            .push("crucible/src/lib.rs: \"../gone.md\" is not a tracked file".into());
        let err = planner.validate().unwrap_err().to_string();
        assert!(err.contains("includes the CI plan cannot follow"), "{err}");
        assert!(err.contains("../gone.md"), "{err}");
    }

    #[test]
    fn a_rule_naming_a_crate_that_is_gone_is_invalid() {
        let mut planner = planner();
        planner.workspace.crates.retain(|krate| krate.name != LSP);
        let err = planner.validate().unwrap_err().to_string();
        assert!(
            err.contains("crucible-lsp, which is not a workspace crate"),
            "{err}"
        );
    }

    /// The outputs that are set: `name` for a true one, `name=value` for a package list.
    fn set(jobs: &Jobs) -> String {
        jobs.to_string()
            .lines()
            .filter_map(|line| match line.split_once('=')? {
                (_, "" | "false") => None,
                (name, "true") => Some(name),
                _ => Some(line),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn this_repository_selects_the_jobs_its_paths_need() {
        let planner = Planner::load().unwrap();
        let engine = "-p crucible -p crucible-controller -p crucible-lsp";
        let engine_jobs = "default_packages=-p crucible -p crucible-controller \
                           controller generated_docs lint kind_e2e ui";
        let engine_and_dependents =
            format!("packages={engine} doc_packages={engine} {engine_jobs}");
        let engine_sources =
            format!("packages={engine} -p xtask doc_packages={engine} {engine_jobs}");
        let controller = "packages=-p crucible-controller doc_packages=-p crucible-controller \
                          default_packages=-p crucible-controller controller lint kind_e2e ui";
        let forge =
            "-p crucible -p crucible-broker -p crucible-controller -p crucible-lsp -p forge";
        let forge_and_dependents = format!("packages={forge} doc_packages={forge} {engine_jobs}");
        let contract = "packages=-p buildit -p crucible -p crucible-broker -p crucible-contract \
                        -p crucible-controller -p crucible-git -p crucible-harness -p crucible-kueue \
                        -p crucible-kueue-client -p crucible-lsp -p crucible-mcp-auth -p crux \
                        doc_packages=-p crucible -p crucible-broker -p crucible-contract \
                        -p crucible-controller -p crucible-harness -p crucible-kueue \
                        -p crucible-kueue-client -p crucible-lsp -p crucible-mcp-auth -p crux \
                        default_packages=-p crucible -p crucible-controller -p crux \
                        controller generated_docs lint kind_e2e ui kueue_python";
        let lsp = "packages=-p crucible-lsp doc_packages=-p crucible-lsp lint";
        let everything = "packages=--workspace doc_packages=--workspace \
                          default_packages=-p crucible -p crucible-controller -p crux \
                          controller generated_docs lint gov kind_e2e ui kueue_python audit";
        let cases: &[(&str, &[&str])] = &[
            ("", &["README.md"]),
            ("", &["buildit/README.md"]),
            (
                "",
                &["docs/architecture.md", "CONTRIBUTING.md", "LICENSE-MIT"],
            ),
            (
                "",
                &[
                    "crucible-desktop/src/lib.rs",
                    ".github/workflows/desktop.yml",
                ],
            ),
            ("", &[]),
            (&engine_and_dependents, &["docs/crucible-contract.md"]),
            (&engine_and_dependents, &["docs/dsl-reference.md"]),
            (&engine_and_dependents, &["examples/counter/crucible.toml"]),
            (&engine_and_dependents, &["third_party/NOTICE"]),
            (&engine_sources, &["crucible/src/prompts/scope-refine.md"]),
            (&engine_sources, &["crucible/src/plan/exec.rs"]),
            ("packages=-p buildit lint", &["buildit/src/main.rs"]),
            (
                "packages=-p buildit lint",
                &["buildit/src/main.rs", "buildit/README.md"],
            ),
            ("packages=-p buildit lint audit", &["buildit/Cargo.toml"]),
            (
                "packages=-p crucible-git lint",
                &["crucible-git/src/main.rs"],
            ),
            (controller, &["crucible-controller/src/spa.rs"]),
            (controller, &["crucible-controller/migrations/0049_x.sql"]),
            (controller, &[".sqlx/query-0.json"]),
            (contract, &["crucible-contract/src/lib.rs"]),
            (
                &forge_and_dependents,
                &[".github/workflows/crucible-build.yml"],
            ),
            ("ui", &["crucible-controller/ui/src/App.tsx", "README.md"]),
            (
                "gov",
                &[
                    "gov/rfc/RFC-0006.toml",
                    "docs/rfc/RFC-0006.md",
                    "docs/adr/ADR-0075.md",
                ],
            ),
            ("audit", &["deny.toml"]),
            ("kueue_python", &["crucible-kueue-py/src/lib.rs"]),
            (
                "lint",
                &["tools/dep-dups.snapshot", "docker/Containerfile.controller"],
            ),
            ("kind_e2e", &["scripts/kind-e2e/store.sh"]),
            (&format!("{lsp} kind_e2e"), &["e2e/kind/cluster.yaml"]),
            (lsp, &["examples/route/workflow.star"]),
            (everything, &["Cargo.lock"]),
            (everything, &[".github/workflows/ci.yml"]),
            (everything, &[".github/actions/postgres/action.yml"]),
            (everything, &["xtask/src/ci.rs"]),
            (everything, &["scripts/ci-jobs-test.sh"]),
            (everything, &["vendor/rune/src/lib.rs"]),
            (everything, &["README.md", "new-dir/file.txt"]),
        ];
        for (want, paths) in cases {
            let changed: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
            assert_eq!(set(&planner.plan(Some(&changed)).0), *want, "{paths:?}");
        }
        assert_eq!(set(&planner.plan(None).0), everything);
    }
}
