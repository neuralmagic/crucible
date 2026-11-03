//! Moving a pack to a newer edition (ADR-0071). Each edition after the first has one step that
//! rewrites a pack written against the edition before it. A step is deterministic, and running
//! it on its own output changes nothing. What a step cannot decide from the pack's text it
//! reports for review instead of guessing.

mod document;
mod edition_two;
mod scan;

use crate::host_env::{EnvDecl, HostTable};
use crate::manifest::edition::{Edition, UnknownEdition};
use crate::manifest::variant::VariantName;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// A pack's text files by pack-relative path, and which of them is its manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackFiles {
    manifest: String,
    files: BTreeMap<String, String>,
}

impl PackFiles {
    /// `files` with `manifest`, a file at the pack root, as the manifest.
    pub fn new(
        manifest: impl Into<String>,
        files: BTreeMap<String, String>,
    ) -> Result<PackFiles, MigrateError> {
        let manifest = manifest.into();
        for path in files.keys() {
            path.parse::<crucible_contract::pack_tree::PackFilePath>()
                .map_err(|_| MigrateError::BadPath { path: path.clone() })?;
        }
        if manifest.contains('/') || !files.contains_key(&manifest) {
            return Err(MigrateError::NoManifest { path: manifest });
        }
        Ok(PackFiles { manifest, files })
    }

    /// The pack rooted at the manifest's directory. Files that are not UTF-8 text are left out:
    /// a migration reads scripts and rewrites manifests, and neither is binary.
    pub fn read(manifest_path: &Path) -> Result<PackFiles, MigrateError> {
        let name = manifest_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| MigrateError::NoManifest {
                path: manifest_path.display().to_string(),
            })?
            .to_owned();
        let root = crate::manifest::manifest_dir(manifest_path);
        let read =
            crucible_contract::pack_tree::walk_dir(&root).map_err(|e| MigrateError::Read {
                path: root.display().to_string(),
                message: e.to_string(),
            })?;
        let mut files = BTreeMap::new();
        for (path, bytes) in read.tree.into_files() {
            let path = path.as_str().to_owned();
            match String::from_utf8(bytes) {
                Ok(text) => {
                    files.insert(path, text);
                }
                Err(_) if path == name => return Err(MigrateError::NotText { path }),
                Err(_) => {}
            }
        }
        PackFiles::new(name, files)
    }

    /// The manifest's pack-relative path.
    pub fn manifest_path(&self) -> &str {
        &self.manifest
    }

    /// The manifest's text.
    pub fn manifest(&self) -> &str {
        self.files
            .get(&self.manifest)
            .map(String::as_str)
            .unwrap_or_default()
    }

    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.files
    }

    pub fn into_files(self) -> BTreeMap<String, String> {
        self.files
    }

    fn with_manifest(&self, text: String) -> PackFiles {
        let mut files = self.files.clone();
        files.insert(self.manifest.clone(), text);
        PackFiles {
            manifest: self.manifest.clone(),
            files,
        }
    }

    /// Write every file under `root`, for the compilers that read a pack from disk.
    fn materialize(&self, root: &Path) -> Result<(), MigrateError> {
        for (path, text) in &self.files {
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| MigrateError::Scratch {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
            }
            std::fs::write(&target, text).map_err(|e| MigrateError::Scratch {
                path: path.clone(),
                message: e.to_string(),
            })?;
        }
        Ok(())
    }
}

/// Why a pack could not be migrated.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MigrateError {
    #[error("the pack has no manifest {path:?} at its root")]
    NoManifest { path: String },
    #[error("{path:?} is not a relative path inside the pack")]
    BadPath { path: String },
    #[error("reading the pack at {path}: {message}")]
    Read { path: String, message: String },
    #[error("{path} is not UTF-8 text, so it cannot be migrated")]
    NotText { path: String },
    #[error("writing {path} to a scratch copy of the pack: {message}")]
    Scratch { path: String, message: String },
    #[error("{manifest} does not parse, so it cannot be migrated: {message}")]
    Parse { manifest: String, message: String },
    #[error(
        "{manifest} is edition {from}; no step lowers an edition, so it cannot move to edition \
         {to}"
    )]
    Downgrade {
        manifest: String,
        from: Edition,
        to: Edition,
    },
    #[error("compiling {file} to find its commands: {message}")]
    Compile { file: String, message: String },
    #[error("{manifest} has no {table} table to declare its commands' variables in")]
    NoTable { manifest: String, table: String },
    #[error("the migrated {manifest} does not parse: {message}")]
    Unparseable { manifest: String, message: String },
    #[error(transparent)]
    UnknownEdition(#[from] UnknownEdition),
}

impl MigrateError {
    /// This error with `root/` dropped from its message, so the scratch copy a step compiles
    /// from never names itself to the caller.
    fn relative_to(self, root: &Path) -> MigrateError {
        let prefix = format!("{}/", root.display());
        let strip = |message: String| message.replace(&prefix, "");
        match self {
            MigrateError::Parse { manifest, message } => MigrateError::Parse {
                manifest,
                message: strip(message),
            },
            MigrateError::Compile { file, message } => MigrateError::Compile {
                file,
                message: strip(message),
            },
            MigrateError::Unparseable { manifest, message } => MigrateError::Unparseable {
                manifest,
                message: strip(message),
            },
            other => other,
        }
    }
}

/// A pack moved to a newer edition: its files, what changed, and what a reviewer should check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    pub from: Edition,
    pub to: Edition,
    pub files: PackFiles,
    pub changes: Vec<Change>,
    pub needs_review: Vec<Finding>,
}

impl Migration {
    /// True when the pack was already at the target edition.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty()
    }

    /// A unified diff of each file this migration changed against `before`, by path.
    pub fn diffs(&self, before: &PackFiles) -> Vec<FileDiff> {
        self.files
            .files
            .iter()
            .filter_map(|(path, after)| {
                let old = before
                    .files
                    .get(path)
                    .map(String::as_str)
                    .unwrap_or_default();
                (old != after).then(|| FileDiff {
                    path: path.clone(),
                    unified: similar::TextDiff::from_lines(old, after)
                        .unified_diff()
                        .context_radius(3)
                        .header(&format!("a/{path}"), &format!("b/{path}"))
                        .to_string(),
                })
            })
            .collect()
    }
}

/// One changed file's unified diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub unified: String,
}

/// A manifest table a migration writes to: the base manifest's, or a variant's own copy.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableRef {
    pub variant: Option<VariantName>,
    pub table: HostTable,
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.variant {
            Some(variant) => write!(f, "[variant.{variant}.{}]", self.table.key()),
            None => write!(f, "{}", self.table),
        }
    }
}

/// A host command, by where the manifest or workflow defines it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandName {
    MeasureCmd,
    ApplyCmd,
    SnapshotCmd,
    RestoreCmd,
    Task(String),
}

impl fmt::Display for CommandName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandName::MeasureCmd => f.write_str("measure_cmd"),
            CommandName::ApplyCmd => f.write_str("apply_cmd"),
            CommandName::SnapshotCmd => f.write_str("snapshot_cmd"),
            CommandName::RestoreCmd => f.write_str("restore_cmd"),
            CommandName::Task(task) => write!(f, "task {task:?}"),
        }
    }
}

/// Where a read was found: a line of a pack file, or a host command's own text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReadSite {
    File { path: String, line: usize },
    Command(CommandName),
}

impl fmt::Display for ReadSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadSite::File { path, line } => write!(f, "{path}:{line}"),
            ReadSite::Command(name) => write!(f, "{name}"),
        }
    }
}

/// One edit a migration made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// The manifest's top-level edition.
    Edition { from: Edition, to: Edition },
    /// A variable `table`'s commands read, declared in its `env`.
    Declared {
        table: TableRef,
        decl: EnvDecl,
        read_at: Vec<ReadSite>,
    },
}

/// How many read sites a change names before summarizing the rest.
const SHOWN_SITES: usize = 3;

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Edition { from, to } => write!(f, "edition = {to} (was {from})"),
            Change::Declared {
                table,
                decl,
                read_at,
            } => {
                let shown: Vec<String> = read_at
                    .iter()
                    .take(SHOWN_SITES)
                    .map(ToString::to_string)
                    .collect();
                write!(
                    f,
                    "{table}.env declares \"{decl}\", read at {}",
                    shown.join(", ")
                )?;
                match read_at.len().saturating_sub(SHOWN_SITES) {
                    0 => Ok(()),
                    more => write!(f, " and {more} more"),
                }
            }
        }
    }
}

/// Why a host command's code could not be read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unresolved {
    /// The command hands code to an interpreter inline.
    InlineCode { interpreter: String },
    /// The program or script is named by an expansion only the running shell resolves.
    Indirect { word: String },
    /// The command runs a path the pack does not carry.
    NotInPack { path: String },
    /// The command runs a pack file that is not shell, Python, or nushell.
    Unreadable { path: String },
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unresolved::InlineCode { interpreter } => write!(
                f,
                "runs inline {interpreter} code, read only as far as the command text goes"
            ),
            Unresolved::Indirect { word } => {
                write!(f, "runs `{word}`, which only the running shell resolves")
            }
            Unresolved::NotInPack { path } => {
                write!(f, "runs {path}, which the pack does not carry")
            }
            Unresolved::Unreadable { path } => write!(
                f,
                "runs {path}, which is not shell, Python, or nushell, so its reads are unknown"
            ),
        }
    }
}

/// What runs a pack file outside the host commands an edition governs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Runner {
    /// An agent turn, in its sandbox.
    AgentTurn,
    /// `[workspace].setup_cmd`, which has the engine's environment in every edition.
    SetupCmd,
    /// The `[agent].agent_cmd` stand-in, which has the engine's environment in every edition.
    AgentCmd,
}

impl fmt::Display for Runner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Runner::AgentTurn => "an agent turn in its sandbox",
            Runner::SetupCmd => "setup_cmd",
            Runner::AgentCmd => "the agent_cmd stand-in",
        })
    }
}

/// Something a migration could not settle from the pack's text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finding {
    /// A host command whose code was not fully read.
    UnresolvedCommand {
        table: TableRef,
        command: CommandName,
        text: String,
        reason: Unresolved,
    },
    /// A pack file reads variables, and no host command was found to run it.
    UnattributedReads { path: String, names: Vec<String> },
    /// A pack file reads variables, and only code the edition does not govern runs it.
    NotAffected {
        path: String,
        names: Vec<String>,
        runner: Runner,
    },
    /// A read whose variable name is computed at run time.
    ComputedRead { site: ReadSite },
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Finding::UnresolvedCommand {
                table,
                command,
                text,
                reason,
            } => write!(f, "{table} {command} `{text}` {reason}"),
            Finding::UnattributedReads { path, names } => write!(
                f,
                "{path} reads {}, and no host command was found to run it; if one does, declare \
                 them in that command's table",
                names.join(", ")
            ),
            Finding::NotAffected {
                path,
                names,
                runner,
            } => write!(
                f,
                "{path} reads {}, and only {runner} runs it, which the edition does not change",
                names.join(", ")
            ),
            Finding::ComputedRead { site } => {
                write!(f, "{site} reads a variable whose name it computes")
            }
        }
    }
}

/// What one edition's step produced.
struct Stepped {
    files: PackFiles,
    changes: Vec<Change>,
    needs_review: Vec<Finding>,
}

/// One edition's step: what moves a pack written against the edition before `to` to `to`.
struct Step {
    to: Edition,
    run: fn(&PackFiles) -> Result<Stepped, MigrateError>,
}

/// Every step, oldest edition first. The next edition adds its step here.
const STEPS: &[Step] = &[Step {
    to: Edition::Two,
    run: edition_two::step,
}];

/// The edition `pack`'s manifest declares.
fn edition_of(pack: &PackFiles) -> Result<Edition, MigrateError> {
    crate::manifest::edition::declared(pack.manifest()).map_err(|e| MigrateError::Parse {
        manifest: pack.manifest_path().to_owned(),
        message: e.to_string(),
    })
}

/// Move `pack` to edition `to`, one step per edition between its own and `to`. A pack already
/// at `to` comes back unchanged.
pub fn migrate(pack: &PackFiles, to: Edition) -> Result<Migration, MigrateError> {
    let from = edition_of(pack)?;
    if to < from {
        return Err(MigrateError::Downgrade {
            manifest: pack.manifest_path().to_owned(),
            from,
            to,
        });
    }
    let mut files = pack.clone();
    let mut changes = Vec::new();
    let mut needs_review = Vec::new();
    for step in STEPS.iter().filter(|s| s.to > from && s.to <= to) {
        let stepped = (step.run)(&files)?;
        files = stepped.files;
        changes.extend(stepped.changes);
        needs_review.extend(stepped.needs_review);
    }
    Ok(Migration {
        from,
        to,
        files,
        changes,
        needs_review,
    })
}

#[cfg(test)]
mod tests;
