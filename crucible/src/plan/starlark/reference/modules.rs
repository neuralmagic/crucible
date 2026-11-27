//! The built-in modules a source imports with `load("@crucible//<name>", ...)`.

use crate::plan::starlark::reference::{Function, Kwarg, Lane};

/// A built-in module and the names it exports.
pub struct Module {
    pub label: &'static str,
    pub purpose: &'static str,
    pub functions: Vec<Function>,
    pub enums: Vec<(&'static str, &'static [&'static str], &'static str)>,
}

fn outputs_kwargs(first: Kwarg) -> Vec<Kwarg> {
    vec![
        first,
        Kwarg::new(
            "fields",
            "dict[str, type]",
            "The typed fields of the JSON object the run writes as its result.",
        ),
        Kwarg::new(
            "files",
            "list[str]",
            "Further paths the call stages for its dependents.",
        ),
    ]
}

pub fn modules() -> Vec<Module> {
    vec![Module {
        label: crate::plan::starlark::targets::MODULE,
        purpose: "The targets `call()` and `watch()` take. Each is written in `workflow.star`, \
                  inline in the call or bound to a top-level name, from literals and other \
                  values of this module, so the controller can pin it in `calls.lock` without \
                  running the source. An inline target is pinned under its task's name, a bound \
                  one under the name it is bound to, which lets several calls share one pin.",
        functions: vec![
            Function {
                name: "playbook",
                lane: Lane::Routed,
                purpose: "Another playbook, run as a child of this one.",
                positional: Some("ref"),
                kwargs: vec![Kwarg::new(
                    "ref",
                    "str",
                    "A registered playbook id, or `draft:<id>` for a draft's head (draft packs only).",
                )],
            },
            Function {
                name: "github_workflow",
                lane: Lane::Routed,
                purpose: "A GitHub Actions workflow, dispatched with its inputs.",
                positional: None,
                kwargs: vec![
                    Kwarg::new("repo", "str", "`owner/name`."),
                    Kwarg::new(
                        "workflow",
                        "str",
                        "The workflow file, under `.github/workflows/`.",
                    ),
                    Kwarg::new("ref", "str", "The branch or tag it runs on."),
                    Kwarg::new(
                        "outputs",
                        "artifact_outputs(...)",
                        "Where the run's result is read from.",
                    ),
                ],
            },
            Function {
                name: "gitlab_pipeline",
                lane: Lane::Routed,
                purpose: "A GitLab pipeline, created with typed `spec:inputs`.",
                positional: None,
                kwargs: vec![
                    Kwarg::new(
                        "host",
                        "str",
                        "The GitLab host; it must be one the controller serves.",
                    ),
                    Kwarg::new("project", "str", "The project path, `group/project`."),
                    Kwarg::new("ref", "str", "The branch or tag the pipeline runs on."),
                    Kwarg::new(
                        "outputs",
                        "job_outputs(...)",
                        "Where the pipeline's result is read from. Unset, the call reads none.",
                    ),
                ],
            },
            Function {
                name: "tekton",
                lane: Lane::Routed,
                purpose: "Pipelines-as-Code runs on a Tekton cluster, for `watch()` only.",
                positional: None,
                kwargs: vec![
                    Kwarg::new(
                        "cluster",
                        "str",
                        "A cluster the controller is configured to watch.",
                    ),
                    Kwarg::new("namespace", "str", "The namespace the runs start in."),
                    Kwarg::new(
                        "repository",
                        "str",
                        "The Pipelines-as-Code Repository name.",
                    ),
                    Kwarg::new(
                        "event",
                        "event.push | event.pull_request",
                        "Which runs to follow. Unset, both.",
                    ),
                    Kwarg::new(
                        "expected",
                        "list[pipeline_run(...)]",
                        "The runs the watch waits for, and the results each declares.",
                    ),
                ],
            },
            Function {
                name: "artifact_outputs",
                lane: Lane::Routed,
                purpose: "A GitHub run's result: an uploaded artifact holding `result.json`.",
                positional: None,
                kwargs: outputs_kwargs(Kwarg::new("artifact", "str", "The artifact name.")),
            },
            Function {
                name: "job_outputs",
                lane: Lane::Routed,
                purpose: "A GitLab pipeline's result: a job's artifacts holding `result.json`.",
                positional: None,
                kwargs: outputs_kwargs(Kwarg::new(
                    "job",
                    "str",
                    "The job whose artifacts hold it.",
                )),
            },
            Function {
                name: "pipeline_run",
                lane: Lane::Routed,
                purpose: "One PipelineRun a Tekton watch waits for.",
                positional: Some("name"),
                kwargs: vec![
                    Kwarg::new("name", "str", "The PipelineRun's name."),
                    Kwarg::new(
                        "results",
                        "list[str]",
                        "Task results the watch outputs for it.",
                    ),
                ],
            },
        ],
        enums: vec![(
            "event",
            &["push", "pull_request"],
            "The Pipelines-as-Code event a Tekton watch follows.",
        )],
    }]
}

pub(crate) fn markdown(out: &mut String) {
    out.push_str(
        "## Built-in modules\n\nImported with `load()`, like a pack's own modules. A name a \
         module exports is not in scope until it is loaded, and it can be renamed on import: \
         `load(\"@crucible//calls\", gl = \"gitlab_pipeline\")`.\n\n",
    );
    for module in modules() {
        out.push_str(&format!("### `{}`\n\n{}\n\n", module.label, module.purpose));
        for function in &module.functions {
            out.push_str(&format!(
                "#### `{}()`\n\n{}\n\n",
                function.name, function.purpose
            ));
            if let Some(positional) = function.positional {
                out.push_str(&format!(
                    "Takes `{positional}` positionally or by name.\n\n"
                ));
            }
            out.push_str("| Argument | Type | Purpose |\n| --- | --- | --- |\n");
            for kwarg in &function.kwargs {
                out.push_str(&format!(
                    "| `{}` | `{}` | {} |\n",
                    crate::plan::starlark::reference::cell(kwarg.name),
                    crate::plan::starlark::reference::cell(kwarg.ty),
                    crate::plan::starlark::reference::cell(&kwarg.purpose)
                ));
            }
            out.push('\n');
        }
        for (name, members, purpose) in &module.enums {
            let shown: Vec<String> = members.iter().map(|m| format!("`{name}.{m}`")).collect();
            out.push_str(&format!(
                "#### `{name}`\n\n{purpose} Members: {}.\n\n",
                shown.join(", ")
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::starlark::reference::modules::modules;
    use crate::plan::starlark::targets;

    #[test]
    fn the_calls_module_documents_exactly_what_it_exports() {
        let calls = &modules()[0];
        let mut documented: Vec<&str> = calls
            .functions
            .iter()
            .map(|f| f.name)
            .chain(calls.enums.iter().map(|(name, _, _)| *name))
            .collect();
        let mut exported: Vec<&str> = targets::exported().collect();
        documented.sort_unstable();
        exported.sort_unstable();
        assert_eq!(documented, exported);
        for function in &calls.functions {
            let documented: Vec<&str> = function.kwargs.iter().map(|k| k.name).collect();
            assert_eq!(
                documented,
                targets::arguments(function.name),
                "{}",
                function.name
            );
            assert_eq!(
                function.positional,
                targets::leading(function.name),
                "{}",
                function.name
            );
        }
        for (name, members, _) in &calls.enums {
            assert!(
                targets::ENUMS.contains(&(*name, *members)),
                "{name} documents its members"
            );
        }
    }
}
