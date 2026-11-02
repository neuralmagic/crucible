//! Every workflow source checked into the repository is clean under the language server: no
//! compile error, no lint at error severity. A false positive here would greet every author who
//! opens a known-good pack.

use std::path::{Path, PathBuf};

use crucible_lsp::analyze::analyze;
use crucible_lsp::check::{self, Finding};
use crucible_lsp::docs::Reference;
use crucible_lsp::text::Text;
use tower_lsp_server::ls_types::DiagnosticSeverity;

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "star") {
            out.push(path);
        }
    }
}

fn render(path: &Path, findings: &[Finding]) -> Vec<String> {
    let text = Text::new(std::fs::read_to_string(path).unwrap_or_default());
    findings
        .iter()
        .map(|f| {
            let at = f.range(&text).start;
            format!(
                "{}:{}:{} {} {}",
                path.display(),
                at.line + 1,
                at.character + 1,
                f.code,
                f.message
            )
        })
        .collect()
}

#[test]
fn every_checked_in_pack_is_clean() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    sources(&root.join("examples"), &mut files);
    sources(&root.join("e2e"), &mut files);
    files.sort();
    assert!(files.len() >= 10, "found only {files:?}");
    let reference = Reference::new();
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let mut errors = Vec::new();
    for file in &files {
        let file = file.canonicalize().expect("canonical");
        let source = std::fs::read_to_string(&file).expect("read");
        let analysis = analyze(&reference, Some(&file), &source, &read);
        let local: Vec<Finding> = analysis
            .local
            .into_iter()
            .filter(|f| f.severity == DiagnosticSeverity::ERROR)
            .collect();
        errors.extend(render(&file, &local));
        if let Some(compile) = analysis.compile {
            for (path, found) in check::compile(&compile.entry, &compile.source) {
                errors.extend(render(&path, &found));
            }
        }
    }
    assert!(errors.is_empty(), "\n{}", errors.join("\n"));
}
