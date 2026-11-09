//! A pack compiled the way a run would admit it, before anything runs: the linked engine
//! compiles its workflow source with the params' defaults (or stand-ins for required ones), and
//! the admitted plan folds like the first line of a run's session, so the graph, tasks and
//! declared fields show what a launch would start. A pack that does not compile folds the tasks
//! the engine got to, with the ones it could not build named in `broken`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crucible::plan::starlark::{check_file_with, declared_params, parent_or_cwd, placeholder};
use serde::Deserialize;
use serde_json::Value;

use crate::session::RunFold;

/// What compiling a pack produced: its plan as a run's first event leaves it, and why it did not
/// compile when it did not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub fold: RunFold,
    pub params_schema: Option<Value>,
    /// The engine's refusals, verbatim, each with the first `file:line` it names.
    pub problems: Vec<Problem>,
    /// Tasks in `fold` standing in for ones the engine could not build.
    pub broken: Vec<String>,
    /// The workflow source the pack names, when it names one.
    pub source: Option<PathBuf>,
}

impl Preview {
    /// Where `task` is defined: its source and the 1-based line of its `name = "…"`, or the first
    /// line when the name is built rather than written out.
    pub fn definition(&self, task: &str) -> Option<(PathBuf, usize)> {
        let source = self.source.clone()?;
        let text = std::fs::read_to_string(&source).unwrap_or_default();
        Some((source, definition_line(&text, task)))
    }
}

/// The 1-based line naming `task` in `text`: a `name = "task"` (spaced or not, either quote),
/// else any line carrying `"task"`, else 1.
fn definition_line(text: &str, task: &str) -> usize {
    let quoted = [format!("\"{task}\""), format!("'{task}'")];
    let named = |line: &str| {
        let squeezed: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        quoted
            .iter()
            .any(|q| squeezed.contains(&format!("name={q}")))
    };
    let mentioned = |line: &str| quoted.iter().any(|q| line.contains(q.as_str()));
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .position(|line| named(line))
        .or_else(|| lines.iter().position(|line| mentioned(line)))
        .map_or(1, |i| i + 1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub message: String,
    pub at: Option<(PathBuf, u32)>,
}

impl Problem {
    fn new(message: String, pack: &Path) -> Self {
        let at = location(&message, pack);
        Problem { message, at }
    }
}

#[derive(Deserialize)]
struct Manifest {
    workflow: Option<WorkflowTable>,
}

#[derive(Deserialize)]
struct WorkflowTable {
    file: Option<String>,
}

/// Compile the pack at `pack`. Blocking: the engine reads and evaluates its source.
pub fn compile(pack: &Path) -> Preview {
    let mut preview = Preview::default();
    let refuse = |preview: &mut Preview, message: String| {
        preview.problems.push(Problem::new(message, pack));
    };
    let manifest = match std::fs::read_to_string(pack.join("crucible.toml")) {
        Ok(text) => text,
        Err(e) => {
            refuse(&mut preview, format!("reading crucible.toml: {e}"));
            return preview;
        }
    };
    let file = match toml::from_str::<Manifest>(&manifest) {
        Ok(parsed) => parsed
            .workflow
            .and_then(|w| w.file)
            .filter(|f| !f.is_empty()),
        Err(e) => {
            refuse(&mut preview, format!("crucible.toml: {e}"));
            return preview;
        }
    };
    let Some(file) = file else {
        refuse(
            &mut preview,
            "crucible.toml names no [workflow] file".into(),
        );
        return preview;
    };
    let source = pack.join(file);
    preview.source = Some(source.clone());
    let text = match std::fs::read_to_string(&source) {
        Ok(text) => text,
        Err(e) => {
            refuse(&mut preview, format!("reading {}: {e}", source.display()));
            return preview;
        }
    };
    let schema = match declared_params(&text, &source) {
        Ok(schema) => schema,
        Err(e) => {
            refuse(&mut preview, crucible::errors::report(&e));
            return preview;
        }
    };
    let params = defaults(&schema);
    preview.params_schema = Some(schema);
    let checked = check_file_with(&source, parent_or_cwd(&source), &params);
    let compiled = match checked.outcome {
        Ok(compiled) => compiled,
        Err(e) => {
            for error in e.each() {
                refuse(&mut preview, crucible::errors::report(error));
            }
            if let Some(sketch) = checked.sketch {
                preview
                    .fold
                    .apply(crucible::plan::admitted::plan_sketched(&sketch.tasks));
                preview.broken = sketch.broken.into_iter().map(|name| name.0).collect();
            }
            return preview;
        }
    };
    match compiled.workflow.playbook_plan() {
        Ok(plan) => {
            preview.fold.apply(crucible::plan::admitted::plan_admitted(
                &plan,
                compiled.workflow.history_record.as_ref(),
            ));
        }
        Err(e) => refuse(&mut preview, format!("{e:#}")),
    }
    preview
}

/// The value each declared param compiles with: its declared default, else for a required one a
/// stand-in derived from its declaration, so a pack nobody has filled a form for still compiles.
fn defaults(schema: &Value) -> BTreeMap<String, String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut out: BTreeMap<String, String> = properties
        .into_iter()
        .flatten()
        .filter_map(|(name, property)| {
            let default = property.get("default")?.as_str()?;
            Some((name.clone(), default.to_string()))
        })
        .collect();
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for name in required {
        if out.contains_key(name) {
            continue;
        }
        if let Some(value) = properties
            .and_then(|p| p.get(name))
            .and_then(|property| placeholder::placeholder(name, property))
        {
            out.insert(name.to_string(), value);
        }
    }
    out
}

/// The first `path:line` in `message` naming a file under `pack`.
fn location(message: &str, pack: &Path) -> Option<(PathBuf, u32)> {
    message.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | '`' | '\''));
        let mut parts = word.split(':');
        let file = parts.next()?;
        let line: u32 = parts.next()?.parse().ok()?;
        let path = if Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            pack.join(file)
        };
        path.starts_with(pack).then_some((path, line))
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::preview::{compile, definition_line};

    #[test]
    fn a_task_is_found_where_its_name_is_written() {
        let text = "scribe = session(name = \"scribe\")\n\ndraft = agent(\n    name = \"draft\",\n    prompt = \"see draft\",\n)\nshape = command(name='shape', run = \"x\")\n";
        assert_eq!(definition_line(text, "draft"), 4);
        assert_eq!(definition_line(text, "shape"), 7);
        assert_eq!(definition_line(text, "scribe"), 1);
        assert_eq!(
            definition_line(text, "audit-headings"),
            1,
            "a built name lands at the top"
        );
    }
    use crate::session::TaskState;

    fn pack(dir: &Path, workflow: &str) {
        std::fs::write(
            dir.join("crucible.toml"),
            "[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("workflow.star"), workflow).unwrap();
    }

    #[test]
    fn a_pack_compiles_to_the_plan_a_run_would_admit() {
        let dir = tempfile::tempdir().unwrap();
        pack(
            dir.path(),
            concat!(
                "scan = command(name = \"scan\", run = \"true\", emits = {\"cves\": \"list\"})\n",
                "fix = agent(name = \"fix\", prompt = \"p\", depends_on = [scan], over = scan.cves, max_fanout = 4)\n",
                "workflow(type = \"playbook\", tasks = [scan, fix])\n",
            ),
        );
        let preview = compile(dir.path());
        assert!(preview.problems.is_empty(), "{:?}", preview.problems);
        let names: Vec<&str> = preview.fold.tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["scan", "fix"]);
        let fix = &preview.fold.tasks[1];
        assert_eq!(fix.depends_on, ["scan"]);
        assert_eq!(fix.over, "scan.cves");
        assert_eq!(fix.state(), TaskState::Pending);
        assert_eq!(preview.fold.tasks[0].emits[0].field, "cves");
    }

    #[test]
    fn the_example_playbook_compiles_with_its_param_defaults() {
        let pack = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/playbook");
        let preview = compile(&pack);
        assert!(preview.problems.is_empty(), "{:?}", preview.problems);
        let names: Vec<&str> = preview.fold.tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.first(), Some(&"draft"));
        assert_eq!(names.last(), Some(&"publish-report"), "{names:?}");
        let fips = compile(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/fips-watch"));
        assert!(fips.problems.is_empty(), "{:?}", fips.problems);
        assert!(fips.params_schema.is_some());
    }

    #[test]
    fn a_pack_that_does_not_compile_says_where() {
        let dir = tempfile::tempdir().unwrap();
        pack(
            dir.path(),
            "workflow(type = \"playbook\", tasks = [missing])\n",
        );
        let preview = compile(dir.path());
        assert!(preview.fold.tasks.is_empty());
        let problem = preview.problems.first().expect("a refusal");
        assert!(problem.message.contains("missing"), "{problem:?}");
        assert_eq!(
            problem
                .at
                .as_ref()
                .map(|(file, line)| (file.ends_with("workflow.star"), *line)),
            Some((true, 1)),
            "{problem:?}"
        );
        assert!(!compile(&dir.path().join("nope")).problems.is_empty());
    }

    #[test]
    fn a_pack_that_does_not_compile_still_shows_its_graph_and_every_problem() {
        let dir = tempfile::tempdir().unwrap();
        pack(
            dir.path(),
            concat!(
                "scan = command(name = \"scan\", run = \"true\", emits = {\"cves\": \"list\"})\n",
                "fix = agent(name = \"fix\", prompt = \"p\", over = each(scan.cves, max = 4), effort = \"hgih\")\n",
                "sum = command(name = \"sum\", run = \"true\", depends_on = [fix], join = \"some\")\n",
                "done = command(name = \"done\", run = \"true\", depends_on = [sum])\n",
                "workflow(type = \"playbook\", tasks = [scan, fix, sum, done])\n",
            ),
        );
        let preview = compile(dir.path());
        let said: Vec<(Option<u32>, bool)> = preview
            .problems
            .iter()
            .map(|problem| {
                (
                    problem.at.as_ref().map(|(_, line)| *line),
                    problem.message.contains("hgih") || problem.message.contains("some"),
                )
            })
            .collect();
        assert_eq!(
            said,
            [(Some(2), true), (Some(3), true)],
            "{:?}",
            preview.problems
        );
        let drawn: Vec<(&str, &[String])> = preview
            .fold
            .tasks
            .iter()
            .map(|task| (task.name.as_str(), task.depends_on.as_slice()))
            .collect();
        assert_eq!(drawn.len(), 4, "{drawn:?}");
        assert_eq!(drawn[0], ("scan", [].as_slice()));
        assert_eq!(drawn[2], ("sum", ["fix".to_owned()].as_slice()));
        assert_eq!(drawn[3], ("done", ["sum".to_owned()].as_slice()));
        assert_eq!(preview.broken, ["fix", "sum"]);
    }
}
