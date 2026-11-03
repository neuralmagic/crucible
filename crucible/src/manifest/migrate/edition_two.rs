//! The step to edition 2, where a host command receives only the base, what the engine sets for
//! it, and what its table declares (RFC-0001:C-COMMANDS). Every variable a host command's code
//! reads without setting it first is declared optional in that command's table: an optional
//! name reaches the command whenever the engine has it, as under edition 1, and never refuses a
//! run when it does not, so the step cannot add a refusal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::host_env::{BASE, EnvDecl, EnvName, HostTable};
use crate::manifest::edition::Edition;
use crate::manifest::migrate::scan::{Event, Language, SimpleCommand, Word, language_sets};
use crate::manifest::migrate::{
    Change, CommandName, Finding, MigrateError, PackFiles, ReadSite, Runner, Stepped, TableRef,
    Unresolved, document,
};
use crate::manifest::variant::VariantName;
use crate::manifest::{CompositeManifest, Inject, JudgeCfg, Manifest, Workspace, WorldCfg};
use crate::plan::ir::{Task, TaskKind};
use crate::plan::workflow::WorkflowCfg;

/// The file a skill's instructions live in; its directory is the skill.
const SKILL_FILE: &str = "SKILL.md";

pub(crate) fn step(pack: &PackFiles) -> Result<Stepped, MigrateError> {
    let scratch = tempfile::tempdir().map_err(|e| MigrateError::Scratch {
        path: ".".to_owned(),
        message: e.to_string(),
    })?;
    pack.materialize(scratch.path())?;
    let manifest_path = scratch.path().join(pack.manifest_path());
    let scopes = scopes(pack, &manifest_path).map_err(|e| e.relative_to(scratch.path()))?;

    let mut analysis = Analysis::new(pack);
    for scope in &scopes {
        for command in &scope.commands {
            analysis.command(scope, command);
        }
    }
    analysis.unattributed(&scopes);

    let mut changes = vec![Change::Edition {
        from: Edition::One,
        to: Edition::Two,
    }];
    let mut text = document::set_edition(pack.manifest(), Edition::Two, pack.manifest_path())?;
    for (table, reads) in &analysis.reads {
        let mut decls = Vec::new();
        for (name, sites) in reads {
            let decl = EnvDecl {
                name: name.clone(),
                required: false,
            };
            changes.push(Change::Declared {
                table: table.clone(),
                decl: decl.clone(),
                read_at: sites.iter().cloned().collect(),
            });
            decls.push(decl);
        }
        text = document::declare(&text, table, &decls, pack.manifest_path())?;
    }
    verify(&text, &manifest_path, pack.manifest_path())
        .map_err(|e| e.relative_to(scratch.path()))?;
    Ok(Stepped {
        files: pack.with_manifest(text),
        changes,
        needs_review: analysis.findings.into_iter().collect(),
    })
}

/// The migrated manifest parses as the engine reads it, every variant included.
fn verify(text: &str, manifest_path: &Path, manifest: &str) -> Result<(), MigrateError> {
    let unparseable = |message: String| MigrateError::Unparseable {
        manifest: manifest.to_owned(),
        message,
    };
    std::fs::write(manifest_path, text).map_err(|e| MigrateError::Scratch {
        path: manifest.to_owned(),
        message: e.to_string(),
    })?;
    let parsed = if crate::manifest::is_composite(manifest_path) {
        CompositeManifest::load(manifest_path).map(|_| ())
    } else {
        Manifest::load(manifest_path).map(|_| ())
    };
    parsed.map_err(|e| unparseable(format!("{e:#}")))
}

/// One host command, by the table that declares what it receives.
struct HostCommand {
    table: HostTable,
    name: CommandName,
    text: String,
}

/// The base manifest or one variant of it, as a run of it would read the pack.
struct Scope {
    variant: Option<VariantName>,
    /// The host tables this variant holds its own copy of.
    replaces: BTreeSet<HostTable>,
    commands: Vec<HostCommand>,
    workspace_dir: String,
    injects: Vec<Inject>,
    /// Pack directories whose files only agent turns use.
    agent_roots: Vec<String>,
    /// Commands that run with the engine's environment in every edition.
    unaffected: Vec<(Runner, String)>,
}

impl Scope {
    /// The table a command of `table` declares its variables in under this scope.
    fn owner(&self, table: HostTable) -> TableRef {
        TableRef {
            variant: self
                .variant
                .clone()
                .filter(|_| self.replaces.contains(&table)),
            table,
        }
    }
}

/// The parts of a manifest a scope is built from.
struct Tables<'m> {
    judge: Option<&'m JudgeCfg>,
    world: &'m WorldCfg,
    workflow: Option<&'m WorkflowCfg>,
    workspace: &'m Workspace,
    toolbox_dir: Option<&'m str>,
    agent_cmd: Option<&'m str>,
}

fn scopes(pack: &PackFiles, manifest_path: &Path) -> Result<Vec<Scope>, MigrateError> {
    let parse = |e: anyhow::Error| MigrateError::Parse {
        manifest: pack.manifest_path().to_owned(),
        message: format!("{e:#}"),
    };
    let manifest_dir = crate::manifest::manifest_dir(manifest_path);
    if crate::manifest::is_composite(manifest_path) {
        let m = CompositeManifest::load(manifest_path).map_err(parse)?;
        let tables = Tables {
            judge: Some(&m.judge),
            world: &m.world,
            workflow: m.workflow.as_ref(),
            workspace: &m.workspace,
            toolbox_dir: m.agent.toolbox_dir.as_deref(),
            agent_cmd: m.agent.agent_cmd.as_deref(),
        };
        return Ok(vec![scope(
            None,
            BTreeSet::new(),
            &tables,
            &manifest_dir,
            pack.manifest_path(),
        )?]);
    }
    let document: toml::Table = toml::from_str(pack.manifest()).map_err(|e| parse(e.into()))?;
    let variants =
        crate::manifest::variant::Variants::split(document.clone()).map_err(|e| parse(e.into()))?;
    let mut out = Vec::new();
    for variant in std::iter::once(None).chain(variants.names().cloned().map(Some)) {
        let m = Manifest::load_variant(manifest_path, variant.as_ref()).map_err(parse)?;
        let replaces = variant
            .as_ref()
            .and_then(|v| {
                document
                    .get(crate::manifest::variant::VARIANT_TABLE)?
                    .get(v.as_str())?
                    .as_table()
            })
            .map(|own| {
                HostTable::ALL
                    .into_iter()
                    .filter(|t| own.contains_key(t.key()))
                    .collect()
            })
            .unwrap_or_default();
        let tables = Tables {
            judge: m.judge.as_ref(),
            world: &m.world,
            workflow: m.workflow.as_ref(),
            workspace: &m.workspace,
            toolbox_dir: m.agent.toolbox_dir.as_deref(),
            agent_cmd: m.agent.agent_cmd.as_deref(),
        };
        out.push(scope(
            variant,
            replaces,
            &tables,
            &manifest_dir,
            pack.manifest_path(),
        )?);
    }
    Ok(out)
}

fn scope(
    variant: Option<VariantName>,
    replaces: BTreeSet<HostTable>,
    tables: &Tables<'_>,
    manifest_dir: &Path,
    manifest: &str,
) -> Result<Scope, MigrateError> {
    let mut commands = Vec::new();
    if let Some(judge) = tables.judge.filter(|j| !j.measure_cmd.trim().is_empty()) {
        commands.push(HostCommand {
            table: HostTable::Judge,
            name: CommandName::MeasureCmd,
            text: judge.measure_cmd.clone(),
        });
    }
    for (name, text) in [
        (CommandName::ApplyCmd, &tables.world.apply_cmd),
        (CommandName::SnapshotCmd, &tables.world.snapshot_cmd),
        (CommandName::RestoreCmd, &tables.world.restore_cmd),
    ] {
        if let Some(text) = text {
            commands.push(HostCommand {
                table: HostTable::World,
                name,
                text: text.clone(),
            });
        }
    }
    let mut agent_roots: Vec<String> = tables.toolbox_dir.and_then(normalize).into_iter().collect();
    if let Some(workflow) = tables.workflow {
        let (tasks, prompt_files) = workflow_tasks(workflow, manifest_dir)?;
        for task in tasks {
            if let TaskKind::Command { command } | TaskKind::Evaluate { command, .. } = task.task {
                commands.push(HostCommand {
                    table: HostTable::Workflow,
                    name: CommandName::Task(task.name.0),
                    text: command,
                });
            }
        }
        agent_roots.extend(prompt_files.iter().filter_map(|p| {
            let p = p.to_str()?;
            let skill = p.strip_suffix(SKILL_FILE)?.trim_end_matches('/');
            normalize(skill)
        }));
    }
    let injects = tables
        .workspace
        .injects(manifest_dir)
        .map_err(|e| MigrateError::Parse {
            manifest: manifest.to_owned(),
            message: format!("[workspace].inject: {e}"),
        })?;
    let unaffected = [
        (Runner::SetupCmd, tables.workspace.setup_cmd.as_deref()),
        (Runner::AgentCmd, tables.agent_cmd),
    ]
    .into_iter()
    .filter_map(|(runner, cmd)| Some((runner, cmd?.to_owned())))
    .collect();
    Ok(Scope {
        variant,
        replaces,
        commands,
        workspace_dir: tables.workspace.dir.clone(),
        injects,
        agent_roots,
        unaffected,
    })
}

/// The workflow's tasks, compiling `[workflow].file` when it names one, and the pack-relative
/// prompt files the compile read. A required parameter without a default compiles with a
/// stand-in derived from its declaration, as compile-on-save does.
fn workflow_tasks(
    workflow: &WorkflowCfg,
    manifest_dir: &Path,
) -> Result<(Vec<Task>, Vec<std::path::PathBuf>), MigrateError> {
    if !workflow.is_unresolved() {
        return Ok((workflow.tasks.clone(), Vec::new()));
    }
    let file = workflow.file.clone().unwrap_or_default();
    let source = manifest_dir.join(&file);
    let compile = |message: String| MigrateError::Compile {
        file: file.clone(),
        message,
    };
    let text = std::fs::read_to_string(&source).map_err(|e| compile(e.to_string()))?;
    let schema = crate::plan::starlark::declared_params(&text, &source)
        .map_err(|e| compile(crate::errors::report(&e)))?;
    let compiled =
        crate::plan::starlark::compile_file_with(&source, manifest_dir, &stand_ins(&schema))
            .map_err(|e| compile(crate::errors::report(&e)))?;
    Ok((compiled.workflow.tasks, compiled.prompt_files))
}

/// A value for every required parameter that declares no default.
fn stand_ins(schema: &serde_json::Value) -> BTreeMap<String, String> {
    let properties = schema.get("properties").and_then(|p| p.as_object());
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|name| {
            let name = name.as_str()?;
            let property = properties?.get(name)?;
            if property.get("default").is_some() {
                return None;
            }
            let value = crate::plan::starlark::placeholder::placeholder(name, property)?;
            Some((name.to_owned(), value))
        })
        .collect()
}

/// Where text being walked came from, so a read names its site.
#[derive(Clone, Copy)]
enum Origin<'a> {
    Command(&'a CommandName),
    File(&'a str),
}

impl Origin<'_> {
    fn site(self, line: usize) -> ReadSite {
        match self {
            Origin::Command(name) => ReadSite::Command(name.clone()),
            Origin::File(path) => ReadSite::File {
                path: path.to_owned(),
                line,
            },
        }
    }
}

/// How one command runs its code.
enum Invocation<'w> {
    /// An interpreter handed code in an argument.
    Inline {
        interpreter: &'w str,
        language: Option<Language>,
        code: &'w Word,
    },
    /// An interpreter reading its program from stdin.
    Stdin { interpreter: &'w str },
    /// An interpreter given a script.
    Script { script: &'w Word },
    /// `python -m`.
    Module { module: &'w Word },
    /// A program that is not an interpreter.
    Program(&'w Word),
}

/// Programs that run the command after their own options.
const WRAPPERS: &[&str] = &[
    "exec", "command", "nohup", "time", "nice", "stdbuf", "sudo", "env", "timeout", "xargs",
];

/// Interpreters whose code may come inline, besides the ones the scanner reads.
const OTHER_INTERPRETERS: &[&str] = &["node", "deno", "bun", "perl", "ruby", "php"];

fn invocation(cmd: &SimpleCommand) -> Option<Invocation<'_>> {
    let mut words = cmd.words.as_slice();
    while let Some(first) = words.first().filter(|w| w.literal) {
        let name = basename(&first.text);
        if !WRAPPERS.contains(&name) {
            break;
        }
        words = &words[1..];
        while words
            .first()
            .is_some_and(|w| w.text.starts_with('-') || (name == "env" && w.text.contains('=')))
        {
            words = &words[1..];
        }
        if name == "timeout" && !words.is_empty() {
            words = &words[1..];
        }
    }
    let program = words.first()?;
    if !program.literal {
        return Some(Invocation::Program(program));
    }
    let interpreter = basename(&program.text);
    let language = Language::of_interpreter(interpreter);
    if language.is_none() && !OTHER_INTERPRETERS.contains(&interpreter) {
        return Some(Invocation::Program(program));
    }
    let mut args = words[1..].iter();
    while let Some(arg) = args.next() {
        if arg.literal && arg.text.starts_with('-') && arg.text != "-" {
            if inline_flag(interpreter, language, &arg.text) {
                return Some(match args.next() {
                    Some(code) => Invocation::Inline {
                        interpreter,
                        language,
                        code,
                    },
                    None => Invocation::Stdin { interpreter },
                });
            }
            if language == Some(Language::Python) && arg.text == "-m" {
                return args.next().map(|module| Invocation::Module { module });
            }
            if takes_value(&arg.text) {
                args.next();
            }
            continue;
        }
        if arg.literal && arg.text == "-" {
            return Some(Invocation::Stdin { interpreter });
        }
        return Some(Invocation::Script { script: arg });
    }
    Some(Invocation::Stdin { interpreter })
}

fn inline_flag(interpreter: &str, language: Option<Language>, flag: &str) -> bool {
    match language {
        Some(Language::Shell) => !flag.starts_with("--") && flag.contains('c'),
        Some(Language::Python) => flag == "-c",
        Some(Language::Nu) => flag == "-c" || flag == "--commands",
        None => match interpreter {
            "node" | "deno" | "bun" => matches!(flag, "-e" | "--eval" | "-p" | "--print"),
            _ => matches!(flag, "-e" | "-E" | "-r"),
        },
    }
}

fn takes_value(flag: &str) -> bool {
    matches!(flag, "-o" | "+o" | "-W" | "-X" | "--require")
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// `path` with `.` and `..` resolved, or `None` when it climbs out of the pack or is absolute.
fn normalize(path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_owned()
    } else {
        format!("{dir}/{rel}")
    }
}

/// What the edition does not govern, or a language sets for itself.
fn excluded(table: HostTable, language: Language, name: &str) -> bool {
    BASE.contains(&name) || table.engine_sets().contains(&name) || language_sets(language, name)
}

/// Every pack walk: which files host commands reach, what they read, and what needs review.
struct Analysis<'p> {
    pack: &'p PackFiles,
    scans: BTreeMap<String, Option<(Language, Vec<Event>)>>,
    /// Directories holding a top-level Python package, which an absolute import resolves from.
    python_roots: Vec<String>,
    reads: BTreeMap<TableRef, BTreeMap<EnvName, BTreeSet<ReadSite>>>,
    visited: BTreeSet<(TableRef, String)>,
    reached: BTreeSet<String>,
    findings: BTreeSet<Finding>,
}

impl<'p> Analysis<'p> {
    fn new(pack: &'p PackFiles) -> Analysis<'p> {
        let files = pack.files();
        let mut roots = BTreeSet::new();
        for path in files.keys() {
            let Some(package) = path.strip_suffix("__init__.py") else {
                continue;
            };
            let mut package = package.trim_end_matches('/');
            if package.is_empty() {
                continue;
            }
            while !parent(package).is_empty()
                && files.contains_key(&join(parent(package), "__init__.py"))
            {
                package = parent(package);
            }
            roots.insert(parent(package).to_owned());
        }
        Analysis {
            pack,
            scans: BTreeMap::new(),
            python_roots: roots.into_iter().collect(),
            reads: BTreeMap::new(),
            visited: BTreeSet::new(),
            reached: BTreeSet::new(),
            findings: BTreeSet::new(),
        }
    }

    fn scan(&mut self, path: &str) -> Option<(Language, Vec<Event>)> {
        if let Some(scanned) = self.scans.get(path) {
            return scanned.clone();
        }
        let scanned = self.pack.files().get(path).and_then(|text| {
            let language = Language::of(path, text)?;
            Some((language, language.scan(text)))
        });
        self.scans.insert(path.to_owned(), scanned.clone());
        scanned
    }

    fn language(&self, path: &str) -> Option<Language> {
        Language::of(path, self.pack.files().get(path)?)
    }

    /// Walk one host command: its own text, then every pack file it runs.
    fn command(&mut self, scope: &Scope, command: &HostCommand) {
        let owner = scope.owner(command.table);
        let events = Language::Shell.scan(&command.text);
        for event in &events {
            if let Event::Command(cmd) = event
                && let Some(reason) = self.unresolved(scope, cmd)
            {
                self.findings.insert(Finding::UnresolvedCommand {
                    table: owner.clone(),
                    command: command.name.clone(),
                    text: command.text.clone(),
                    reason,
                });
            }
        }
        self.walk(
            scope,
            &owner,
            Origin::Command(&command.name),
            None,
            Language::Shell,
            events,
            BTreeSet::new(),
        );
    }

    /// Record the reads in `events` that nothing set first, and visit what they run.
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        scope: &Scope,
        owner: &TableRef,
        origin: Origin<'_>,
        file: Option<&str>,
        language: Language,
        events: Vec<Event>,
        inherited: BTreeSet<String>,
    ) {
        let mut local = BTreeSet::new();
        let mut exported = inherited;
        let mut children: Vec<(String, BTreeSet<String>)> = Vec::new();
        for event in events {
            match event {
                Event::Read { name, line } => {
                    if local.contains(&name)
                        || exported.contains(&name)
                        || excluded(owner.table, language, &name)
                    {
                        continue;
                    }
                    let Ok(name) = EnvName::try_from(name) else {
                        continue;
                    };
                    self.reads
                        .entry(owner.clone())
                        .or_default()
                        .entry(name)
                        .or_default()
                        .insert(origin.site(line));
                }
                Event::Set {
                    name,
                    exported: passed,
                    ..
                } => {
                    if passed {
                        exported.insert(name.clone());
                    }
                    local.insert(name);
                }
                Event::Export { name, .. } => {
                    if local.contains(&name) {
                        exported.insert(name);
                    }
                }
                Event::Computed { line } => {
                    self.findings.insert(Finding::ComputedRead {
                        site: origin.site(line),
                    });
                }
                Event::Command(cmd) => {
                    match invocation(&cmd) {
                        Some(Invocation::Inline {
                            language: Some(inline),
                            code,
                            ..
                        }) if code.literal => {
                            let events = inline.scan(&code.text);
                            self.walk(scope, owner, origin, file, inline, events, exported.clone());
                        }
                        Some(Invocation::Module { module }) if module.literal => {
                            for path in self.module(file, 0, &module.text, &[]) {
                                children.push((path, exported.clone()));
                            }
                        }
                        _ => {}
                    }
                    for word in &cmd.words {
                        if let Some(path) = self.resolve(scope, word, file)
                            && self.language(&path).is_some()
                        {
                            children.push((path, exported.clone()));
                        }
                    }
                }
                Event::Path { text, .. } => {
                    let word = Word {
                        text,
                        literal: true,
                    };
                    if let Some(path) = self.resolve(scope, &word, None)
                        && self.language(&path).is_some()
                    {
                        children.push((path, exported.clone()));
                    }
                }
                Event::Import {
                    level,
                    module,
                    names,
                    ..
                } => {
                    if language == Language::Python {
                        for path in self.module(file, level, &module, &names) {
                            children.push((path, exported.clone()));
                        }
                    }
                }
            }
        }
        for (path, inherited) in children {
            if !self.visited.insert((owner.clone(), path.clone())) {
                continue;
            }
            self.reached.insert(path.clone());
            if let Some((language, events)) = self.scan(&path) {
                self.walk(
                    scope,
                    owner,
                    Origin::File(&path),
                    Some(&path),
                    language,
                    events,
                    inherited,
                );
            }
        }
    }

    /// Why a host command's code cannot be read, when it cannot.
    fn unresolved(&self, scope: &Scope, cmd: &SimpleCommand) -> Option<Unresolved> {
        let script = |word: &Word| -> Option<Unresolved> {
            if !word.literal {
                return Some(Unresolved::Indirect {
                    word: word.text.clone(),
                });
            }
            match self.resolve(scope, word, None) {
                None => Some(Unresolved::NotInPack {
                    path: word.text.clone(),
                }),
                Some(path) if self.language(&path).is_none() => {
                    Some(Unresolved::Unreadable { path })
                }
                Some(_) => None,
            }
        };
        match invocation(cmd)? {
            Invocation::Inline { interpreter, .. } | Invocation::Stdin { interpreter } => {
                Some(Unresolved::InlineCode {
                    interpreter: interpreter.to_owned(),
                })
            }
            Invocation::Script { script: word } => script(word),
            Invocation::Module { module } => {
                if !module.literal {
                    return Some(Unresolved::Indirect {
                        word: module.text.clone(),
                    });
                }
                self.module(None, 0, &module.text, &[])
                    .is_empty()
                    .then(|| Unresolved::NotInPack {
                        path: module.text.clone(),
                    })
            }
            Invocation::Program(word) if !word.literal => script(word),
            Invocation::Program(word) => {
                let relative = word.text.contains('/') && !word.text.starts_with('/');
                if relative { script(word) } else { None }
            }
        }
    }

    /// The pack file `word` names, read as a command run from the workspace sees it: through
    /// `[workspace].inject`, then under the workspace directory, then as a pack path, then
    /// beside the file the word appears in.
    fn resolve(&self, scope: &Scope, word: &Word, file: Option<&str>) -> Option<String> {
        if !word.literal {
            return None;
        }
        let text = word.text.as_str();
        if text.is_empty()
            || text.starts_with('-')
            || text.starts_with('/')
            || text.contains(['=', '*', '?', '[', '\n', '$'])
        {
            return None;
        }
        let files = self.pack.files();
        let exists = |path: Option<String>| path.filter(|p| files.contains_key(p));
        let rel = normalize(text);
        if let Some(rel) = &rel {
            for inject in &scope.injects {
                let (Some(dst), Some(src)) = (normalize(&inject.dst), normalize(&inject.src))
                else {
                    continue;
                };
                let mapped = if *rel == dst {
                    Some(src)
                } else {
                    rel.strip_prefix(&format!("{dst}/"))
                        .map(|rest| join(&src, rest))
                };
                if let Some(found) = exists(mapped) {
                    return Some(found);
                }
            }
        }
        exists(normalize(&join(&scope.workspace_dir, text)))
            .or_else(|| exists(rel))
            .or_else(|| exists(file.and_then(|f| normalize(&join(parent(f), text)))))
    }

    /// The pack files a Python import executes: each package's `__init__.py` on the way, the
    /// module, and any names imported from it that are submodules.
    fn module(
        &self,
        importer: Option<&str>,
        level: usize,
        module: &str,
        names: &[String],
    ) -> Vec<String> {
        let importer_dir = importer.map_or("", parent);
        let bases: Vec<String> = if level > 0 {
            let mut base = importer_dir;
            for _ in 1..level {
                base = parent(base);
            }
            vec![base.to_owned()]
        } else {
            let mut bases = vec![importer_dir.to_owned(), String::new()];
            bases.extend(self.python_roots.iter().cloned());
            bases.dedup();
            bases
        };
        let parts: Vec<&str> = module.split('.').filter(|p| !p.is_empty()).collect();
        let files = self.pack.files();
        for base in bases {
            let path_of = |segments: &[&str]| join(&base, &segments.join("/"));
            let mut found = Vec::new();
            for n in 1..=parts.len() {
                let init = join(&path_of(&parts[..n]), "__init__.py");
                if files.contains_key(&init) {
                    found.push(init);
                }
            }
            let file = format!("{}.py", path_of(&parts));
            let package = join(&path_of(&parts), "__init__.py");
            let hit = if parts.is_empty() {
                level > 0
            } else if files.contains_key(&file) {
                found.push(file);
                true
            } else {
                files.contains_key(&package)
            };
            if !hit {
                continue;
            }
            for name in names {
                let mut segments = parts.clone();
                segments.push(name);
                let sub = path_of(&segments);
                for candidate in [format!("{sub}.py"), join(&sub, "__init__.py")] {
                    if files.contains_key(&candidate) {
                        found.push(candidate);
                    }
                }
            }
            return found;
        }
        Vec::new()
    }

    /// Note the pack files that read variables and that no host command reached.
    fn unattributed(&mut self, scopes: &[Scope]) {
        let mut runners: BTreeMap<String, Runner> = BTreeMap::new();
        for scope in scopes {
            for (runner, cmd) in &scope.unaffected {
                for event in Language::Shell.scan(cmd) {
                    let Event::Command(cmd) = event else {
                        continue;
                    };
                    let runs = match invocation(&cmd) {
                        Some(Invocation::Script { script: word } | Invocation::Program(word)) => {
                            self.resolve(scope, word, None).into_iter().collect()
                        }
                        Some(Invocation::Module { module }) => {
                            self.module(None, 0, &module.text, &[])
                        }
                        _ => Vec::new(),
                    };
                    for path in runs {
                        runners.entry(path).or_insert(*runner);
                    }
                }
            }
        }
        let agent_roots: BTreeSet<&str> = scopes
            .iter()
            .flat_map(|s| s.agent_roots.iter().map(String::as_str))
            .collect();
        let engine_sets: BTreeSet<&str> = HostTable::ALL
            .into_iter()
            .flat_map(|t| t.engine_sets().iter().copied())
            .collect();
        let paths: Vec<String> = self.pack.files().keys().cloned().collect();
        for path in paths {
            if self.reached.contains(&path) {
                continue;
            }
            let Some((language, events)) = self.scan(&path) else {
                continue;
            };
            let mut set = BTreeSet::new();
            let mut names = BTreeSet::new();
            for event in events {
                match event {
                    Event::Read { name, .. }
                        if !set.contains(&name)
                            && !BASE.contains(&name.as_str())
                            && !engine_sets.contains(name.as_str())
                            && !language_sets(language, &name) =>
                    {
                        names.insert(name);
                    }
                    Event::Set { name, .. } => {
                        set.insert(name);
                    }
                    _ => {}
                }
            }
            if names.is_empty() {
                continue;
            }
            let names: Vec<String> = names.into_iter().collect();
            let runner = runners.get(&path).copied().or_else(|| {
                agent_roots
                    .iter()
                    .any(|root| !root.is_empty() && path.starts_with(&format!("{root}/")))
                    .then_some(Runner::AgentTurn)
            });
            self.findings.insert(match runner {
                Some(runner) => Finding::NotAffected {
                    path,
                    names,
                    runner,
                },
                None => Finding::UnattributedReads { path, names },
            });
        }
    }
}
