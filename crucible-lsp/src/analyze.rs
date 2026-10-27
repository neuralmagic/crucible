//! Parse and lint one document, and find the entry its pack compiles from.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crucible::plan::starlark::read_params;

use crate::check::{self, Finding};
use crate::docs::Reference;
use crate::index::Index;
use crate::lint::{self, LintContext};
use crate::pack::Pack;
use crate::snapshot::PackContext;

/// Modules a `param()` read is looked for in, past the entry.
const MAX_MODULES: usize = 32;

pub struct Analysis {
    /// The parse error, or the lints.
    pub local: Vec<Finding>,
    /// `None` when the text did not parse.
    pub index: Option<Index>,
    pub context: PackContext,
    pub compile: Option<Compile>,
}

/// The entry to compile for this document, and the pack it belongs to.
pub struct Compile {
    pub pack: PathBuf,
    pub entry: PathBuf,
    pub source: String,
}

pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A document's entry: itself when it calls `workflow()`, else its pack's `workflow.star`.
struct Entry {
    path: PathBuf,
    source: String,
    /// The entry's own index, when the entry is another file.
    index: Option<Index>,
}

pub fn analyze(
    reference: &Reference,
    path: Option<&Path>,
    text: &str,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Analysis {
    let pack = path.and_then(Pack::of);
    let named = path.map_or_else(|| PathBuf::from("untitled.star"), Path::to_path_buf);
    let module = match check::parse(text, &named) {
        Ok(module) => module,
        Err(finding) => {
            return Analysis {
                local: pack.iter().map(|_| finding.clone()).collect(),
                index: None,
                context: PackContext {
                    pack,
                    ..PackContext::default()
                },
                compile: None,
            };
        }
    };
    let index = Index::build(&module);
    let pack = pack.or_else(|| index.lane.and(path).map(Pack::standalone));
    let entry = match (&pack, index.lane.is_some()) {
        (_, true) => path.map(|path| Entry {
            path: path.to_path_buf(),
            source: text.to_owned(),
            index: None,
        }),
        (Some(pack), false) => {
            let entry = pack.entry();
            let is_self = path.map(canonical) == Some(canonical(&entry));
            read(&entry).filter(|_| !is_self).map(|source| Entry {
                index: check::parse(&source, &entry).ok().map(|m| Index::build(&m)),
                path: entry,
                source,
            })
        }
        (None, false) => None,
    };
    let entry_index = entry.as_ref().map(|e| e.index.as_ref().unwrap_or(&index));
    let context = PackContext {
        sandboxes: pack.as_ref().and_then(Pack::sandboxes),
        lane: entry_index.and_then(|i| i.lane),
        param_decls: entry_index.map(|i| i.params.clone()).unwrap_or_default(),
        params: entry
            .as_ref()
            .and_then(|e| read_params(&e.source, &e.path).ok())
            .map(|params| params.specs().to_vec())
            .unwrap_or_default(),
        entry: entry.as_ref().map(|e| e.path.clone()),
        pack,
    };
    let local = match &context.pack {
        Some(pack) => {
            let reads = (!index.params.is_empty())
                .then(|| param_reads(&index, pack, read))
                .flatten();
            lint::run(
                &module,
                &index,
                &LintContext {
                    reference,
                    lane: context.lane,
                    pack,
                    sandboxes: context.sandboxes.as_ref(),
                    param_reads: reads.as_ref(),
                },
            )
        }
        None => Vec::new(),
    };
    let compile = match (&context.pack, entry) {
        (Some(pack), Some(entry)) => Some(Compile {
            pack: pack.dir.clone(),
            entry: canonical(&entry.path),
            source: entry.source,
        }),
        _ => None,
    };
    Analysis {
        local,
        index: Some(index),
        context,
        compile,
    }
}

/// Every parameter the entry and its loaded modules read; `None` when that cannot be known.
fn param_reads(
    entry: &Index,
    pack: &Pack,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Option<BTreeSet<String>> {
    let mut names = lint::literal_param_reads(entry)?;
    let mut queue: Vec<String> = entry.loads.iter().map(|l| l.module.clone()).collect();
    let mut seen = BTreeSet::new();
    while let Some(module) = queue.pop() {
        let path = pack.resolve(&module)?;
        if !seen.insert(canonical(&path)) {
            continue;
        }
        if seen.len() > MAX_MODULES {
            return None;
        }
        let source = read(&path)?;
        let index = Index::build(&check::parse(&source, &path).ok()?);
        names.extend(lint::literal_param_reads(&index)?);
        queue.extend(index.loads.iter().map(|l| l.module.clone()));
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use tower_lsp_server::ls_types::DiagnosticSeverity;

    use crate::analyze::*;

    fn disk(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn pack(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().canonicalize().expect("canonical");
        std::fs::write(dir.join("crucible.toml"), "").expect("manifest");
        for (path, body) in files {
            std::fs::write(dir.join(path), body).expect("write");
        }
        (root, dir)
    }

    #[test]
    fn a_module_compiles_through_its_packs_entry_and_takes_its_lane() {
        let entry =
            "load(\"lib.star\", \"make\")\nworkflow(type = \"playbook\", tasks = [make()])\n";
        let lib = "def make():\n    return command(name = \"m\", run = \"true\")\n";
        let (_root, dir) = pack(&[("workflow.star", entry), ("lib.star", lib)]);
        let analysis = analyze(&Reference::new(), Some(&dir.join("lib.star")), lib, &disk);
        let compile = analysis.compile.expect("compiles the entry");
        assert_eq!(compile.entry, dir.join("workflow.star"));
        assert_eq!(compile.source, entry);
        assert_eq!(
            analysis.context.lane,
            Some(crucible::plan::workflow::WorkflowType::Playbook)
        );
    }

    #[test]
    fn a_param_read_only_in_a_loaded_module_is_still_read() {
        let entry = "params = {\"repo\": {\"type\": \"string\", \"required\": True}}\n\
                     load(\"lib.star\", \"make\")\nworkflow(type = \"playbook\", tasks = [make()])\n";
        let lib = "def make():\n    return agent(name = \"m\", prompt = \"p\" + param(\"repo\"))\n";
        let (_root, dir) = pack(&[("workflow.star", entry), ("lib.star", lib)]);
        let analysis = analyze(
            &Reference::new(),
            Some(&dir.join("workflow.star")),
            entry,
            &disk,
        );
        assert!(
            analysis.local.iter().all(|f| f.code != "unused-param"),
            "{:?}",
            analysis.local
        );
        std::fs::write(
            dir.join("lib.star"),
            lib.replace("param(\"repo\")", "\"x\""),
        )
        .expect("lib");
        let analysis = analyze(
            &Reference::new(),
            Some(&dir.join("workflow.star")),
            entry,
            &disk,
        );
        assert!(
            analysis.local.iter().any(|f| f.code == "unused-param"),
            "{:?}",
            analysis.local
        );
    }

    #[test]
    fn a_parse_error_is_the_only_finding_and_nothing_compiles() {
        let (_root, dir) = pack(&[]);
        let analysis = analyze(
            &Reference::new(),
            Some(&dir.join("workflow.star")),
            "x = (",
            &disk,
        );
        assert_eq!(analysis.local.len(), 1);
        assert_eq!(analysis.local[0].severity, DiagnosticSeverity::ERROR);
        assert!(analysis.index.is_none() && analysis.compile.is_none());
        assert_eq!(analysis.context.pack.map(|p| p.dir), Some(dir));
    }

    #[test]
    fn starlark_outside_any_pack_is_left_alone() {
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let path = elsewhere.path().join("BUILD.star");
        let bazel = "cc_library(name = \"x\", srcs = glob([\"*.cc\"]))\nagent = 1\n";
        let analysis = analyze(&Reference::new(), Some(&path), bazel, &disk);
        assert!(analysis.local.is_empty(), "{:?}", analysis.local);
        assert!(analysis.compile.is_none() && analysis.context.pack.is_none());
        let broken = analyze(&Reference::new(), Some(&path), "x = (", &disk);
        assert!(
            broken.local.is_empty(),
            "another dialect's syntax is not ours to judge"
        );
        let untitled = analyze(&Reference::new(), None, "x = 1\n", &disk);
        assert!(untitled.compile.is_none() && untitled.context.pack.is_none());
    }

    #[test]
    fn a_workflow_outside_any_manifest_is_its_own_pack() {
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let path = elsewhere.path().join("workflow.star");
        let source = "a = command(name = \"a\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [a])\n";
        let analysis = analyze(&Reference::new(), Some(&path), source, &disk);
        assert_eq!(
            analysis.context.pack.map(|p| p.dir),
            Some(elsewhere.path().to_path_buf())
        );
        assert!(analysis.compile.is_some());
    }
}
