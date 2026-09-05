//! The DSL's own description of itself: every constructor, its lane, and its keyword arguments.
//!
//! This table is the source the published reference is generated from
//! (`crucible plan dsl-reference`). Tests below tie it to the registered globals and to
//! [`crate::plan::starlark::known_kwargs`], so a constructor cannot be added, renamed, or given a
//! new argument without the reference following it.

use crate::plan::exec::DeclaredStatus;
use crate::plan::ir::KEPT_INPUT;
use crate::plan::ir::MAX_FANOUT_CEILING;
use crate::plan::ir::{ITEM_INPUT, OUTCOME_INPUT};
#[cfg(test)]
use crate::plan::workflow::WorkflowType;

/// Which lanes see a constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Every workflow type, playbooks included.
    Common,
    /// The scored types (`autoresearch` and `custom`) only.
    Scored,
}

impl Lane {
    #[cfg(test)]
    fn includes(self, workflow: WorkflowType) -> bool {
        self == Lane::Common || workflow != WorkflowType::Playbook
    }
}

pub struct Kwarg {
    pub name: &'static str,
    pub ty: &'static str,
    pub purpose: String,
}

impl Kwarg {
    fn new(name: &'static str, ty: &'static str, purpose: impl Into<String>) -> Self {
        Kwarg {
            name,
            ty,
            purpose: purpose.into(),
        }
    }
}

pub struct Function {
    pub name: &'static str,
    pub lane: Lane,
    pub purpose: &'static str,
    /// The single positional argument a historically-positional constructor takes.
    pub positional: Option<&'static str>,
    pub kwargs: Vec<Kwarg>,
}

/// Knobs shared by every constructor that produces a graph task.
fn task_knobs() -> Vec<Kwarg> {
    vec![
        Kwarg::new(
            "depends_on",
            "list[task]",
            "Dependencies. Readiness decides execution order; declaration order does not.",
        ),
        Kwarg::new(
            "needs",
            "\"any\" | \"all\"",
            "How many dependencies must be admitted before the task is ready.",
        ),
        Kwarg::new(
            "join",
            "\"all\" | \"passed\" | \"settled\"",
            "Which dependencies must have passed: `all` every one, `passed` at least one and \
             only those are forwarded, `settled` none — it dispatches once every dependency is \
             terminal, whatever it settled as, unless the run has already halted, and forwards \
             each one as {status, note, output, files}.",
        ),
        Kwarg::new(
            "required",
            "bool",
            "False makes the task advisory: it blocks dependents but cannot invalidate the run.",
        ),
        Kwarg::new(
            "isolated",
            "bool",
            "Run in a disposable worktree. File changes are discarded; only JSON output continues.",
        ),
        Kwarg::new(
            "emits",
            "list[str]",
            "Result fields the task promises in its JSON output.",
        ),
        Kwarg::new(
            "emits_files",
            "list[str]",
            "Workspace files the task produces. A dependent is staged with the declared files of \
             every dependency that passed.",
        ),
        Kwarg::new(
            "over",
            "producer.field",
            "Map the task over a dependency's emitted list, one instance per item.",
        ),
        Kwarg::new(
            "max_fanout",
            "int",
            format!(
                "Instance cap for `over`, within the engine's ceiling of {MAX_FANOUT_CEILING}."
            ),
        ),
        Kwarg::new(
            "stage",
            "\"iteration\" | \"epilogue\"",
            "`epilogue` runs once after the loop concludes, and only if the run kept a candidate.",
        ),
    ]
}

/// Agent knobs shared by `agent()` and `skill()`.
fn agent_knobs() -> Vec<Kwarg> {
    vec![
        Kwarg::new("harness", "str", "Agent harness, overriding `[agent]`."),
        Kwarg::new("model", "str", "Model, overriding `[agent]`."),
        Kwarg::new("effort", "str", "Reasoning effort, overriding `[agent]`."),
        Kwarg::new(
            "session",
            "session | str",
            "Join a durable conversation. A task in a session cannot be isolated.",
        ),
    ]
}

fn name_kwarg() -> Kwarg {
    Kwarg::new("name", "str", "Task identity, unique within the workflow.")
}

fn with(head: Vec<Kwarg>, tail: Vec<Kwarg>) -> Vec<Kwarg> {
    let mut all = head;
    all.extend(tail);
    all
}

/// Every constructor the DSL registers, in the order the reference renders them.
pub fn functions() -> Vec<Function> {
    vec![
        Function {
            name: "agent",
            lane: Lane::Common,
            purpose: "An agent turn driven by a prompt.",
            positional: None,
            kwargs: with(
                with(
                    vec![
                        name_kwarg(),
                        Kwarg::new("prompt", "str", "The turn's prompt."),
                    ],
                    agent_knobs(),
                ),
                task_knobs(),
            ),
        },
        Function {
            name: "skill",
            lane: Lane::Common,
            purpose: "An agent turn whose prompt is a skill's instructions plus its arguments.",
            positional: None,
            kwargs: with(
                with(
                    vec![
                        name_kwarg(),
                        Kwarg::new(
                            "skill",
                            "str",
                            "Skill directory below the pack; its instructions become the prompt.",
                        ),
                        Kwarg::new("args", "dict", "Arguments appended to the instructions."),
                    ],
                    agent_knobs(),
                ),
                task_knobs(),
            ),
        },
        Function {
            name: "command",
            lane: Lane::Common,
            purpose: "A deterministic shell task in the candidate workspace.",
            positional: None,
            kwargs: with(
                vec![
                    name_kwarg(),
                    Kwarg::new("run", "str", "The command, run through `sh -c`."),
                ],
                task_knobs(),
            ),
        },
        Function {
            name: "evaluate",
            lane: Lane::Common,
            purpose: "A measurement command. Its last non-empty stdout line is a JSON object; \
                      `pass = false` vetoes the result and numeric `score` feeds `grade()` and \
                      `top_k()`.",
            positional: None,
            kwargs: with(
                vec![
                    name_kwarg(),
                    Kwarg::new("run", "str", "The command, run through `sh -c`."),
                    Kwarg::new(
                        "threshold",
                        "number",
                        "Grade the emitted score against this bound. An explicit `pass` wins.",
                    ),
                    Kwarg::new(
                        "direction",
                        "\"lower\" | \"higher\"",
                        "Which side of the threshold passes.",
                    ),
                ],
                task_knobs(),
            ),
        },
        Function {
            name: "report",
            lane: Lane::Common,
            purpose: "Publish a rendered template to a controller-configured destination. The \
                      workflow selects a destination key, never an endpoint or a credential.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("destination", "str", "The configured sink to publish to."),
                Kwarg::new("template", "str", "The template rendered into the message."),
                Kwarg::new(
                    "result",
                    "task",
                    "The task whose result the template renders.",
                ),
                Kwarg::new("required", "bool", "False makes the report advisory."),
            ],
        },
        Function {
            name: "session",
            lane: Lane::Common,
            purpose: "Declare a durable agent conversation. Tasks that share one run serially \
                      under one agent config, across dependency order and loop iterations.",
            positional: None,
            kwargs: vec![
                Kwarg::new(
                    "name",
                    "str",
                    "Session identity, referenced by `session =`.",
                ),
                Kwarg::new(
                    "harness",
                    "str",
                    "Default harness for tasks in the session.",
                ),
                Kwarg::new("model", "str", "Default model for tasks in the session."),
                Kwarg::new("effort", "str", "Default effort for tasks in the session."),
            ],
        },
        Function {
            name: "param",
            lane: Lane::Common,
            purpose: "Read a launch parameter. The `params` block must be the source's first \
                      statement, and a source that declares one compiles per run.",
            positional: Some("name"),
            kwargs: vec![],
        },
        Function {
            name: "prompt_file",
            lane: Lane::Common,
            purpose: "Embed a UTF-8 file below the pack directory. Absolute paths, `..`, \
                      symlinks, non-files, and oversized inputs are refused.",
            positional: Some("path"),
            kwargs: vec![],
        },
        Function {
            name: "workflow",
            lane: Lane::Common,
            purpose: "The source's final expression: the lane, the tasks that ship, and the \
                      result. A task constructed but not listed is a compile error.",
            positional: Some("tasks"),
            kwargs: vec![
                Kwarg::new(
                    "type",
                    "\"autoresearch\" | \"custom\" | \"playbook\"",
                    "The lane, which decides which constructors exist.",
                ),
                Kwarg::new("tasks", "list[task]", "Every task that ships."),
                Kwarg::new(
                    "result",
                    "task",
                    "The task whose output is the workflow's result.",
                ),
            ],
        },
        Function {
            name: "propose",
            lane: Lane::Scored,
            purpose: "The loop's candidate-producing agent turn.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new(
                    "session",
                    "session | str",
                    "The conversation the turn belongs to.",
                ),
                Kwarg::new("depends_on", "list[task]", "Dependencies."),
            ],
        },
        Function {
            name: "apply",
            lane: Lane::Scored,
            purpose: "Make the candidate live through the configured world. A failure means \
                      unscoreable, not worse.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("depends_on", "list[task]", "Dependencies."),
            ],
        },
        Function {
            name: "measure",
            lane: Lane::Scored,
            purpose: "Run the manifest's frozen judge as one opaque measurement task.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("depends_on", "list[task]", "Dependencies."),
            ],
        },
        Function {
            name: "grade",
            lane: Lane::Scored,
            purpose: "Fold evaluation evidence into a measurement. Evidence includes tasks that \
                      failed or never ran, which is what the score source alone cannot see.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("score", "task", "The task whose score the decision uses."),
                Kwarg::new(
                    "tiebreak",
                    "task",
                    "Secondary score that breaks primary-score ties.",
                ),
                Kwarg::new(
                    "evidence",
                    "list[task]",
                    "Tasks folded into the measurement.",
                ),
                Kwarg::new(
                    "join",
                    "\"all\" | \"passed\"",
                    "Which evidence must have passed.",
                ),
            ],
        },
        Function {
            name: "decide",
            lane: Lane::Scored,
            purpose: "Apply the engine's keep-or-discard rule to a measurement. An `autoresearch` \
                      workflow must end here.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("measurement", "task", "The measurement being ruled on."),
                Kwarg::new(
                    "depends_on",
                    "list[task]",
                    "Dependencies, defaulting to the measurement.",
                ),
            ],
        },
        Function {
            name: "top_k",
            lane: Lane::Scored,
            purpose: "Engine-owned reducer: the best `k` dependency outputs by numeric score.",
            positional: None,
            kwargs: vec![
                name_kwarg(),
                Kwarg::new("k", "int", "How many dependencies survive."),
                Kwarg::new("direction", "\"lower\" | \"higher\"", "Which score wins."),
                Kwarg::new("depends_on", "list[task]", "The candidates being reduced."),
                Kwarg::new("required", "bool", "False makes the reducer advisory."),
            ],
        },
        Function {
            name: "default_autoresearch",
            lane: Lane::Scored,
            purpose: "Expand the built-in propose/apply/measure/decide loop into visible nodes, \
                      plus the tasks passed to it.",
            positional: Some("extra_tasks"),
            kwargs: vec![],
        },
    ]
}

/// A name the engine reads or writes for itself, and what it means there.
pub struct Reserved {
    pub name: &'static str,
    pub ty: String,
    pub purpose: &'static str,
}

impl Reserved {
    fn new(name: &'static str, ty: impl Into<String>, purpose: &'static str) -> Self {
        Reserved {
            name,
            ty: ty.into(),
            purpose,
        }
    }
}

/// The tokens the engine acts on in a task's own `status` field, as a union type.
fn declared_status_type() -> String {
    DeclaredStatus::ALL
        .iter()
        .map(|s| format!("\"{}\"", s.as_str()))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Fields the engine reads out of a task's own JSON output.
pub fn reserved_result_fields() -> Vec<Reserved> {
    vec![Reserved::new(
        "status",
        declared_status_type(),
        "Settles the task, overriding an exit code or `pass`. Any other value is ignored.",
    )]
}

/// Keys the engine writes into a task's inputs. None of them is ever wrapped in a settled
/// join's per-dependency entry.
pub fn reserved_inputs() -> Vec<Reserved> {
    vec![
        Reserved::new(
            ITEM_INPUT,
            "str",
            "This mapped instance's key, one per item of the list `over` names.",
        ),
        Reserved::new(
            KEPT_INPUT,
            "object",
            "The kept candidate, in an epilogue task only.",
        ),
        Reserved::new(
            OUTCOME_INPUT,
            "object",
            "How the main graph ended and what each of its tasks settled as, as \
             `{\"exit\": str, \"tasks\": {name: {\"status\", \"note\"}}}`, in an epilogue \
             task only.",
        ),
    ]
}

/// The dialect's authoring surface, read off the dialect the compiler actually installs.
fn dialect_sentence() -> String {
    let dialect = crate::plan::starlark::dialect();
    let mut allowed = vec!["assignments", "`if`", "`for`", "comprehensions"];
    if dialect.enable_def {
        allowed.push("`def`");
    }
    if dialect.enable_lambda {
        allowed.push("`lambda`");
    }
    if dialect.enable_load {
        allowed.push("`load()`");
    }
    format!(
        "The dialect is Starlark with {}{}. Every one of them runs at compile time: the compiled \
         plan is a static graph, so a loop in the source unrolls into tasks rather than becoming \
         a cycle.",
        allowed.join(", "),
        if dialect.enable_top_level_stmt {
            ", at the top level or inside a `def`"
        } else {
            ""
        }
    )
}

/// A pipe ends a table cell, including inside a code span, so a union type has to escape it.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// The published reference page.
pub fn markdown() -> String {
    let mut out = String::new();
    out.push_str("# Workflow DSL reference\n\n");
    out.push_str(
        "<!-- Generated by `crucible plan dsl-reference`. Edit the table in \
         `crucible/src/plan/starlark/reference.rs`, not this file. -->\n\n",
    );
    out.push_str(&dialect_sentence());
    out.push_str(
        "\n\n`prompt_file()` and `load()` are the only file access, both confined below the pack \
         directory, and a loaded module cannot re-export what it loaded. The surface has no \
         processes, network access, clock, or randomness.\n\n\
         For what the engine does with the compiled graph, see [Work graphs](./work-graphs.md); \
         for the normative rules, see the [implementation contract](./crucible-contract.md).\n\n",
    );

    for (lane, heading, blurb) in [
        (
            Lane::Common,
            "Every lane",
            "Available in every workflow type, playbooks included.",
        ),
        (
            Lane::Scored,
            "Scored lanes only",
            "Available to `type = \"autoresearch\"` and `type = \"custom\"`. A playbook does not \
             have these in scope at all, so naming one is an unknown-name error and a \
             did-you-mean never offers one.",
        ),
    ] {
        out.push_str(&format!("## {heading}\n\n{blurb}\n\n"));
        for function in functions().iter().filter(|f| f.lane == lane) {
            out.push_str(&format!(
                "### `{}()`\n\n{}\n\n",
                function.name, function.purpose
            ));
            if let Some(positional) = function.positional {
                out.push_str(&format!(
                    "Takes one positional argument, `{positional}`.\n\n"
                ));
            }
            if function.kwargs.is_empty() {
                continue;
            }
            out.push_str("| Argument | Type | Purpose |\n| --- | --- | --- |\n");
            for kwarg in &function.kwargs {
                out.push_str(&format!(
                    "| `{}` | `{}` | {} |\n",
                    cell(kwarg.name),
                    cell(kwarg.ty),
                    cell(&kwarg.purpose)
                ));
            }
            out.push('\n');
        }
    }

    out.push_str(
        "## Reserved fields\n\nNames the engine reads and writes for itself. They are not \
         constructor arguments; they appear in a task's own JSON output and in the inputs it \
         receives.\n\n",
    );
    for (heading, blurb, rows) in [
        (
            "A task's own JSON output",
            "Read out of the object the task returns.",
            reserved_result_fields(),
        ),
        (
            "Inputs the engine writes",
            "Present alongside the dependency entries, never wrapped in one.",
            reserved_inputs(),
        ),
    ] {
        out.push_str(&format!("### {heading}\n\n{blurb}\n\n"));
        out.push_str("| Field | Type | Meaning |\n| --- | --- | --- |\n");
        for row in rows {
            out.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                cell(row.name),
                cell(&row.ty),
                cell(row.purpose)
            ));
        }
        out.push('\n');
    }
    out
}

/// The same surface as data, for tooling that generates or validates a workflow source.
pub fn json() -> serde_json::Value {
    serde_json::json!({
        "functions": functions()
            .iter()
            .map(|function| serde_json::json!({
                "name": function.name,
                "lane": match function.lane {
                    Lane::Common => "common",
                    Lane::Scored => "scored",
                },
                "purpose": function.purpose,
                "positional": function.positional,
                "kwargs": function.kwargs
                    .iter()
                    .map(|kwarg| serde_json::json!({
                        "name": kwarg.name,
                        "type": kwarg.ty,
                        "purpose": kwarg.purpose,
                    }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "reserved_result_fields": reserved_rows(reserved_result_fields()),
        "reserved_inputs": reserved_rows(reserved_inputs()),
    })
}

fn reserved_rows(rows: Vec<Reserved>) -> serde_json::Value {
    rows.iter()
        .map(|row| {
            serde_json::json!({
                "name": row.name,
                "type": row.ty,
                "purpose": row.purpose,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::plan::exec::{DeclaredStatus, TaskStatus};
    use crate::plan::starlark::reference::functions;
    use crate::plan::starlark::{dsl_functions, known_kwargs, lane_globals};
    use crate::plan::workflow::WorkflowType;

    /// The reference is the global surface, per lane, in both directions: a constructor the
    /// evaluator registers but the table omits is undocumented, and one the table names but the
    /// evaluator does not register is a ghost nobody can call.
    #[test]
    fn documented_functions_match_the_registered_globals() {
        for lane in [
            WorkflowType::Autoresearch,
            WorkflowType::Custom,
            WorkflowType::Playbook,
        ] {
            let documented: BTreeSet<&str> = functions()
                .iter()
                .filter(|function| function.lane.includes(lane))
                .map(|function| function.name)
                .collect();
            let callable: BTreeSet<&str> = dsl_functions(lane).into_iter().collect();
            assert_eq!(documented, callable, "lane {lane:?}");

            let registered: BTreeSet<String> = lane_globals(lane)
                .names()
                .map(|name| name.as_str().to_owned())
                .collect();
            for function in &documented {
                assert!(
                    registered.contains(*function),
                    "{function} is documented but not registered for {lane:?}"
                );
            }
        }
    }

    /// The reference's arguments are the arguments the constructors take. A positional-only
    /// constructor documents no keyword arguments, which is what its empty table says.
    #[test]
    fn documented_kwargs_match_known_kwargs() {
        for function in functions() {
            let documented: BTreeSet<&str> =
                function.kwargs.iter().map(|kwarg| kwarg.name).collect();
            let known: BTreeSet<&str> = known_kwargs(function.name).iter().copied().collect();
            assert_eq!(documented, known, "{}()", function.name);
        }
    }

    /// A union type (`"any" | "all"`) carries the character that ends a table cell. Every row
    /// the page renders has to survive as three cells regardless.
    #[test]
    fn every_rendered_table_row_has_three_cells() {
        for line in super::markdown().lines() {
            if !line.starts_with('|') {
                continue;
            }
            let separators = line
                .char_indices()
                .filter(|(i, c)| *c == '|' && (*i == 0 || !line[..*i].ends_with('\\')))
                .count();
            assert_eq!(separators, 4, "{line}");
        }
    }

    /// The reserved-fields table is the engine's own vocabulary, not prose beside it: the status
    /// tokens are the ones the engine acts on, and the input names are the constants it writes.
    #[test]
    fn the_reserved_fields_table_names_the_engines_own_constants() {
        let results = super::reserved_result_fields();
        assert_eq!(
            results.iter().map(|row| row.name).collect::<Vec<_>>(),
            ["status"],
            "the table documents a field no engine code reads"
        );
        for declared in DeclaredStatus::ALL {
            assert!(
                results[0]
                    .ty
                    .contains(&format!("\"{}\"", declared.as_str())),
                "{} is acted on but undocumented: {}",
                declared.as_str(),
                results[0].ty
            );
            assert_eq!(
                declared.as_str().parse::<TaskStatus>().map(|s| s.as_str()),
                Ok(declared.as_str()),
                "a declared status has no terminal status to settle as"
            );
        }

        assert_eq!(
            super::reserved_inputs()
                .iter()
                .map(|row| row.name)
                .collect::<Vec<_>>(),
            crate::plan::ir::RESERVED_INPUTS,
            "an input the engine writes is undocumented, or a documented one is invented"
        );
    }

    /// The page carries the tables, not just the tables' source.
    #[test]
    fn the_rendered_page_documents_every_reserved_name() {
        let page = super::markdown();
        for row in super::reserved_result_fields()
            .iter()
            .chain(super::reserved_inputs().iter())
        {
            assert!(
                page.contains(&format!("| `{}` |", row.name)),
                "{} is missing from the page",
                row.name
            );
        }
    }
}
