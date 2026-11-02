//! Diagnostics from parsing a document and compiling its pack.

use std::collections::BTreeMap;
use std::ops::Range as Bytes;
use std::path::{Path, PathBuf};

use crucible::plan::starlark::error::{CompileError, SourceAnchor};
use crucible::plan::starlark::placeholder::placeholder;
use crucible::plan::starlark::{compile_source_with, dialect, read_params, reject_deep_nesting};
use starlark_syntax::syntax::AstModule;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Position, Range,
};

use crate::index::bytes;
use crate::text::Text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Compiler,
    Crucible,
    Starlark,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At {
    Bytes(Bytes<usize>),
    /// 0-based lines and character columns, as the compiler's anchors carry them.
    Chars {
        begin: (usize, usize),
        end: (usize, usize),
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub at: At,
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub origin: Origin,
    pub message: String,
    /// Renders faded: the code it covers can be deleted.
    pub unnecessary: bool,
}

impl Finding {
    pub fn range(&self, text: &Text) -> Range {
        match &self.at {
            At::Bytes(bytes) => text.range(bytes.clone()),
            At::Chars { begin, end } => {
                let begin = text.char_offset(begin.0, begin.1);
                let end = text.char_offset(end.0, end.1).max(begin);
                text.range(begin..end)
            }
        }
    }

    pub fn diagnostic(&self, text: &Text) -> Diagnostic {
        Diagnostic {
            range: self.range(text),
            severity: Some(self.severity),
            code: Some(NumberOrString::String(self.code.clone())),
            source: Some(
                match self.origin {
                    Origin::Compiler | Origin::Crucible => "crucible",
                    Origin::Starlark => "starlark",
                }
                .to_owned(),
            ),
            message: self.message.clone(),
            tags: self.unnecessary.then(|| vec![DiagnosticTag::UNNECESSARY]),
            ..Diagnostic::default()
        }
    }
}

/// Parse with the compiler's dialect and nesting guard.
pub fn parse(source: &str, path: &Path) -> Result<AstModule, Finding> {
    reject_deep_nesting(source, path).map_err(|error| compile_finding(&error, None))?;
    AstModule::parse(&path.display().to_string(), source.to_owned(), &dialect()).map_err(|error| {
        let at = error
            .span()
            .map_or(At::Bytes(0..0), |at| At::Bytes(bytes(at.span)));
        Finding {
            at,
            severity: DiagnosticSeverity::ERROR,
            code: "parse".to_owned(),
            origin: Origin::Compiler,
            message: error.without_diagnostic().to_string(),
            unnecessary: false,
        }
    })
}

/// Compile `entry`, with placeholders for required parameters, and file the errors by path.
pub fn compile(entry: &Path, source: &str) -> BTreeMap<PathBuf, Vec<Finding>> {
    let pack_dir = entry.parent().unwrap_or(Path::new("."));
    match compile_source_with(source, entry, pack_dir, &placeholders(source, entry)) {
        Ok(_) => BTreeMap::new(),
        Err(error) => findings(&error, entry),
    }
}

fn placeholders(source: &str, entry: &Path) -> BTreeMap<String, String> {
    let Ok(params) = read_params(source, entry) else {
        return BTreeMap::new();
    };
    let schema = params.json_schema();
    params
        .specs()
        .iter()
        .filter(|spec| spec.required)
        .filter_map(|spec| {
            let property = schema.get("properties")?.get(&spec.name)?;
            Some((spec.name.clone(), placeholder(&spec.name, property)?))
        })
        .collect()
}

/// One finding per anchor, by file. An unanchored error lands at the top of the entry.
pub fn findings(error: &CompileError, entry: &Path) -> BTreeMap<PathBuf, Vec<Finding>> {
    let anchors = error.anchors();
    let mut out: BTreeMap<PathBuf, Vec<Finding>> = BTreeMap::new();
    if anchors.is_empty() {
        out.entry(entry.to_path_buf())
            .or_default()
            .push(compile_finding(error, None));
    }
    for anchor in &anchors {
        out.entry(PathBuf::from(&anchor.file))
            .or_default()
            .push(compile_finding(error, Some(anchor)));
    }
    out
}

fn compile_finding(error: &CompileError, anchor: Option<&SourceAnchor>) -> Finding {
    let at = match (anchor, error) {
        (Some(anchor), _) => At::Chars {
            begin: (
                anchor.span.begin_line.saturating_sub(1) as usize,
                anchor.span.begin_column.saturating_sub(1) as usize,
            ),
            end: (
                anchor.span.end_line.saturating_sub(1) as usize,
                anchor.span.end_column.saturating_sub(1) as usize,
            ),
        },
        (None, CompileError::SourceTooDeep { line, .. }) => At::Chars {
            begin: (line.saturating_sub(1), 0),
            end: (*line, 0),
        },
        (None, _) => At::Bytes(0..0),
    };
    Finding {
        at,
        severity: DiagnosticSeverity::ERROR,
        code: "compile".to_owned(),
        origin: Origin::Compiler,
        message: message(error),
        unnecessary: false,
    }
}

/// The error and its causes, without the location prefix.
fn message(error: &CompileError) -> String {
    match error {
        CompileError::At { inner, .. } => message(inner),
        other => crucible::errors::report(other),
    }
}

/// Whether two LSP ranges share any position.
pub fn overlaps(a: &Range, b: &Range) -> bool {
    let before = |x: &Position, y: &Position| (x.line, x.character) < (y.line, y.character);
    !before(&a.end, &b.start) && !before(&b.end, &a.start)
}

#[cfg(test)]
mod tests {
    use crate::check::*;

    fn pack(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        for (path, body) in files {
            let path = root.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("dirs");
            }
            std::fs::write(path, body).expect("write");
        }
        root
    }

    fn entry(root: &tempfile::TempDir) -> PathBuf {
        root.path()
            .canonicalize()
            .expect("canonical")
            .join("workflow.star")
    }

    #[test]
    fn a_clean_pack_has_no_compiler_findings() {
        let source = "a = command(name = \"a\", run = \"true\")\nworkflow(type = \"playbook\", tasks = [a])\n";
        let root = pack(&[("workflow.star", source)]);
        assert!(compile(&entry(&root), source).is_empty());
    }

    #[test]
    fn a_required_param_with_no_value_still_compiles() {
        let source = "params = {\"repo\": {\"type\": \"string\", \"required\": True, \"pattern\": \"^[a-z]+/[a-z]+$\"}}\n\
                      a = agent(name = \"a\", prompt = \"look at \" + param(\"repo\"))\n\
                      workflow(type = \"playbook\", tasks = [a])\n";
        let root = pack(&[("workflow.star", source)]);
        assert_eq!(compile(&entry(&root), source), BTreeMap::new());
    }

    #[test]
    fn each_dropped_task_is_reported_where_it_was_built() {
        let source = "a = command(name = \"a\", run = \"true\")\n\
                      b = command(name = \"b\", run = \"true\")\n\
                      c = command(name = \"c\", run = \"true\")\n\
                      workflow(type = \"playbook\", tasks = [a])\n";
        let root = pack(&[("workflow.star", source)]);
        let entry = entry(&root);
        let found = compile(&entry, source);
        let lines: Vec<usize> = found
            .get(&entry)
            .expect("entry findings")
            .iter()
            .map(|f| match f.at {
                At::Chars { begin, .. } => begin.0,
                At::Bytes(_) => usize::MAX,
            })
            .collect();
        assert_eq!(lines, [1, 2]);
    }

    #[test]
    fn an_error_in_a_loaded_module_is_filed_under_that_module() {
        let source =
            "load(\"lib.star\", \"make\")\nworkflow(type = \"playbook\", tasks = [make()])\n";
        let lib = "def make():\n    return command(name = \"x\", run = \"true\", bogus = 1)\n";
        let root = pack(&[("workflow.star", source), ("lib.star", lib)]);
        let entry = entry(&root);
        let found = compile(&entry, source);
        let lib_path = entry.with_file_name("lib.star");
        let in_lib = found.get(&lib_path).expect("lib findings");
        assert_eq!(in_lib.len(), 1);
        assert!(in_lib[0].message.contains("bogus"), "{}", in_lib[0].message);
        let text = Text::new(lib.to_owned());
        let range = in_lib[0].range(&text);
        assert_eq!(range.start.line, 1);
        assert_eq!(
            text.slice(text.offset(range.start)..text.offset(range.end)),
            "command(name = \"x\", run = \"true\", bogus = 1)",
            "a call evaluated from another module is anchored at the call, not the argument"
        );
    }

    #[test]
    fn a_parse_error_keeps_its_span() {
        let finding = parse("x = (\n", Path::new("w.star")).expect_err("broken");
        assert_eq!(finding.code, "parse");
        assert!(matches!(finding.at, At::Bytes(_)));
        assert!(!finding.message.contains("w.star:"), "{}", finding.message);
    }

    #[test]
    fn a_source_nested_past_the_guard_is_refused_before_parsing() {
        let source = format!("x = {}1{}\n", "(".repeat(200), ")".repeat(200));
        let finding = parse(&source, Path::new("w.star")).expect_err("too deep");
        assert!(finding.message.contains("nests"), "{}", finding.message);
    }

    #[test]
    fn ranges_overlap_when_they_share_a_position() {
        let r =
            |a: u32, b: u32, c: u32, d: u32| Range::new(Position::new(a, b), Position::new(c, d));
        assert!(overlaps(&r(0, 0, 0, 5), &r(0, 5, 0, 9)));
        assert!(overlaps(&r(0, 0, 2, 0), &r(1, 3, 1, 4)));
        assert!(!overlaps(&r(0, 0, 0, 4), &r(0, 5, 0, 9)));
    }
}
