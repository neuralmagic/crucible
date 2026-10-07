//! The typed graph document a compiled plan reduces to: nodes, edges, and the per-task metadata a
//! renderer needs to draw them (kind, advisory tasks, join mode, isolation, fan-out, routing).
//!
//! The linked engine's compiled plan (`canonical_json`, what `crucible plan compile-workflow`
//! prints) is the input; nothing else in the controller
//! interprets a plan's semantics, and nothing here does either — the reduction is structural, so a
//! kind, join or needs value this build does not know still rides to the client as itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What executes a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Agent,
    Command,
    Engine,
    Route,
    #[serde(other)]
    Other,
}

/// Which of a task's dependencies have to be admitted before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Needs {
    Any,
    All,
    #[serde(other)]
    Other,
}

/// Which of a task's dependencies have to have passed before it runs. `Settled` waits for every
/// dependency to reach a terminal status and runs whatever they settled as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Join {
    All,
    Passed,
    Settled,
    #[serde(other)]
    Other,
}

/// A task mapped over a producer's emitted field: one instance per item, capped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FanOutDto {
    /// The task whose emitted field is mapped over.
    pub over_task: String,
    pub over_field: String,
    /// The instance cap the pack declared, if any.
    pub max_fanout: Option<u32>,
}

/// A task that runs only on some answers to one question of a route it depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WhenDto {
    pub route: String,
    pub question: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GraphNodeDto {
    pub name: String,
    pub kind: TaskKind,
    /// False for an advisory task: the graph runs on without it.
    pub required: bool,
    pub needs: Needs,
    pub join: Join,
    /// The workspace the task runs in, as the engine resolved it; null for the shared one.
    pub isolation: Option<String>,
    /// Result fields the task declares.
    pub emits: Vec<String>,
    /// Workspace files the task declares.
    pub emits_files: Vec<String>,
    /// Set when this task is mapped over a producer's field.
    pub fanout: Option<FanOutDto>,
    /// `producer.field` references each instance receives narrowed to its own entry.
    pub keyed: Vec<String>,
    /// Set when the task runs only on some answers of a route.
    pub when: Option<WhenDto>,
    /// A route's question ids; empty for any other task.
    pub questions: Vec<String>,
    /// The conversation an agent turn belongs to; null when the turn stands alone.
    pub session: Option<String>,
    /// The agent knobs the task set, null where it inherits the pack's defaults.
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// An agent task's prompt, as the engine assembled it: a `skill()` task's is its skill's
    /// instructions plus the arguments the pack passed.
    pub prompt: Option<String>,
    /// What a command task runs.
    pub command: Option<String>,
}

/// One dependency, from producer to consumer. The join and required flags are the consumer's:
/// they are what the edge means, so a renderer can label the edge without a node lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GraphEdgeDto {
    pub from: String,
    pub to: String,
    pub join: Join,
    pub required: bool,
    /// The consumer's `when`, on the edge from the route it names.
    pub when: Option<WhenDto>,
    /// Both ends map over the same `producer.field`: instance k reads the producer's instance k.
    pub aligned: bool,
    /// The producer's fields the consumer narrows to each instance's entry.
    pub keyed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorkflowGraphDto {
    /// The workflow type the pack declares, `playbook` for anything the registry takes.
    pub workflow_type: String,
    /// The task whose output is the workflow's result, when one is named.
    pub result: Option<String>,
    pub nodes: Vec<GraphNodeDto>,
    pub edges: Vec<GraphEdgeDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct CompiledOver {
    task: String,
    field: String,
}

#[derive(Debug, Deserialize)]
struct CompiledWhen {
    task: String,
    question: String,
    #[serde(default)]
    is: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CompiledTask {
    name: String,
    kind: TaskKind,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default = "any")]
    needs: Needs,
    #[serde(default = "yes")]
    required: bool,
    #[serde(default = "all")]
    join: Join,
    #[serde(default)]
    isolation: Option<String>,
    #[serde(default)]
    emits: crucible::plan::ir::Emits,
    #[serde(default)]
    emits_files: Vec<crucible_contract::emits::DeclaredFile>,
    #[serde(default)]
    over: Option<CompiledOver>,
    #[serde(default)]
    max_fanout: Option<u32>,
    #[serde(default)]
    keyed: Vec<CompiledOver>,
    #[serde(default)]
    when: Option<CompiledWhen>,
    #[serde(default)]
    questions: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

fn any() -> Needs {
    Needs::Any
}

fn all() -> Join {
    Join::All
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct CompiledPlan {
    #[serde(rename = "type")]
    workflow_type: String,
    #[serde(default)]
    result: Option<String>,
    #[serde(default, rename = "task")]
    tasks: Vec<CompiledTask>,
}

fn when_dto(when: &CompiledWhen) -> WhenDto {
    WhenDto {
        route: when.task.clone(),
        question: when.question.clone(),
        labels: when.is.clone(),
    }
}

/// Reduce a compiled plan's canonical JSON to the graph document. `Err` carries a message fit
/// for a preview's diagnostics: a plan that will not parse is the engine's output changing shape,
/// not a caller's mistake.
pub fn graph_from_compiled(compiled: &[u8]) -> Result<WorkflowGraphDto, String> {
    let plan: CompiledPlan = serde_json::from_slice(compiled)
        .map_err(|e| format!("the engine's compiled plan did not parse as JSON: {e}"))?;

    let over: std::collections::HashMap<&str, Option<&CompiledOver>> = plan
        .tasks
        .iter()
        .map(|t| (t.name.as_str(), t.over.as_ref()))
        .collect();
    let mut edges = Vec::new();
    for task in &plan.tasks {
        for dep in &task.depends_on {
            let Some(dep_over) = over.get(dep.as_str()) else {
                continue;
            };
            edges.push(GraphEdgeDto {
                from: dep.clone(),
                to: task.name.clone(),
                join: task.join,
                required: task.required,
                when: task
                    .when
                    .as_ref()
                    .filter(|when| when.task == *dep)
                    .map(when_dto),
                aligned: task.over.is_some() && task.over.as_ref() == *dep_over,
                keyed: task
                    .keyed
                    .iter()
                    .filter(|keyed| keyed.task == *dep)
                    .map(|keyed| keyed.field.clone())
                    .collect(),
            });
        }
    }

    let nodes = plan
        .tasks
        .into_iter()
        .map(|task| GraphNodeDto {
            name: task.name,
            kind: task.kind,
            required: task.required,
            needs: task.needs,
            join: task.join,
            isolation: task.isolation,
            emits: task.emits.names(),
            emits_files: task.emits_files.into_iter().map(|file| file.path).collect(),
            fanout: task.over.map(|over| FanOutDto {
                over_task: over.task,
                over_field: over.field,
                max_fanout: task.max_fanout,
            }),
            keyed: task
                .keyed
                .iter()
                .map(|keyed| format!("{}.{}", keyed.task, keyed.field))
                .collect(),
            when: task.when.as_ref().map(when_dto),
            questions: task.questions.into_keys().collect(),
            session: task.session,
            harness: task.harness,
            model: task.model,
            effort: task.effort,
            prompt: task.prompt,
            command: task.command,
        })
        .collect();

    Ok(WorkflowGraphDto {
        workflow_type: plan.workflow_type,
        result: plan.result,
        nodes,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use crate::playbooks::plan_graph::*;

    const PLAN: &[u8] = br#"{
      "type": "playbook",
      "result": "final",
      "task": [
        {"name":"seed","kind":"command","command":"./seed.sh --all","depends_on":[],"needs":"any",
         "required":true,"isolation":null,"join":"all"},
        {"name":"fan","kind":"agent","prompt":"go","harness":"claude","model":"opus",
         "effort":"high","session":"scribe","depends_on":["seed"],"needs":"any",
         "required":true,"isolation":"worktree","join":"all","emits":["idea"]},
        {"name":"work","kind":"command","command":"echo","depends_on":["fan"],"needs":"any",
         "required":true,"isolation":null,"join":"all","over":{"task":"fan","field":"idea"},
         "max_fanout":4},
        {"name":"check","kind":"command","command":"true","depends_on":["work"],"needs":"all",
         "required":false,"isolation":null,"join":"passed","emits_files":["out.json",
         {"path":"result.json","schema":"{\"type\":\"object\"}"}],
         "emits":{"rows":{"schema":"{\"type\":\"array\"}"}}},
        {"name":"final","kind":"command","command":"true","depends_on":["check","ghost"],
         "needs":"any","required":true,"isolation":null,"join":"passed"}
      ]
    }"#;

    fn node<'a>(graph: &'a WorkflowGraphDto, name: &str) -> &'a GraphNodeDto {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("{name} is in the graph"))
    }

    #[test]
    fn a_compiled_plan_reduces_to_nodes_and_edges() {
        let graph = graph_from_compiled(PLAN).expect("reduces");
        assert_eq!(graph.workflow_type, "playbook");
        assert_eq!(graph.result.as_deref(), Some("final"));
        assert_eq!(graph.nodes.len(), 5);
        assert_eq!(node(&graph, "fan").kind, TaskKind::Agent);
        assert_eq!(node(&graph, "fan").emits, vec!["idea".to_string()]);
        assert_eq!(node(&graph, "fan").isolation.as_deref(), Some("worktree"));
        assert!(!node(&graph, "check").required, "an advisory task says so");
        assert_eq!(node(&graph, "check").join, Join::Passed);
        assert_eq!(node(&graph, "check").needs, Needs::All);
        assert_eq!(
            node(&graph, "check").emits_files,
            vec!["out.json".to_string(), "result.json".to_string()]
        );
        assert_eq!(node(&graph, "check").emits, vec!["rows".to_string()]);
    }

    /// What a task actually runs: the agent knobs and prompt, or the command line.
    #[test]
    fn a_task_carries_the_source_it_runs() {
        let graph = graph_from_compiled(PLAN).expect("reduces");
        let fan = node(&graph, "fan");
        assert_eq!(fan.model.as_deref(), Some("opus"));
        assert_eq!(fan.effort.as_deref(), Some("high"));
        assert_eq!(fan.harness.as_deref(), Some("claude"));
        assert_eq!(fan.session.as_deref(), Some("scribe"));
        assert_eq!(fan.prompt.as_deref(), Some("go"));
        assert_eq!(fan.command, None);

        let seed = node(&graph, "seed");
        assert_eq!(seed.command.as_deref(), Some("./seed.sh --all"));
        assert_eq!(seed.prompt, None);
        assert_eq!(seed.model, None, "a command has no agent knobs");
    }

    #[test]
    fn a_mapped_task_carries_its_producer_field_and_cap() {
        let graph = graph_from_compiled(PLAN).expect("reduces");
        assert_eq!(
            node(&graph, "work").fanout,
            Some(FanOutDto {
                over_task: "fan".to_string(),
                over_field: "idea".to_string(),
                max_fanout: Some(4),
            })
        );
        assert_eq!(node(&graph, "seed").fanout, None);
    }

    #[test]
    fn a_settled_join_reaches_the_client_as_itself() {
        let plan = String::from_utf8_lossy(PLAN).replace(
            r#""join":"passed","emits_files""#,
            r#""join":"settled","emits_files""#,
        );
        let graph = graph_from_compiled(plan.as_bytes()).expect("reduces");
        assert_eq!(node(&graph, "check").join, Join::Settled);
        assert_eq!(
            serde_json::to_value(Join::Settled).expect("serializes"),
            serde_json::json!("settled")
        );
    }

    /// The edge carries the consumer's join and required flags, and a dependency on a name the
    /// plan does not carry is dropped rather than drawn to nothing.
    #[test]
    fn edges_carry_the_consumers_join_and_drop_unknown_dependencies() {
        let graph = graph_from_compiled(PLAN).expect("reduces");
        assert_eq!(
            graph.edges,
            vec![
                GraphEdgeDto {
                    from: "seed".to_string(),
                    to: "fan".to_string(),
                    join: Join::All,
                    required: true,
                    when: None,
                    aligned: false,
                    keyed: Vec::new(),
                },
                GraphEdgeDto {
                    from: "fan".to_string(),
                    to: "work".to_string(),
                    join: Join::All,
                    required: true,
                    when: None,
                    aligned: false,
                    keyed: Vec::new(),
                },
                GraphEdgeDto {
                    from: "work".to_string(),
                    to: "check".to_string(),
                    join: Join::Passed,
                    required: false,
                    when: None,
                    aligned: false,
                    keyed: Vec::new(),
                },
                GraphEdgeDto {
                    from: "check".to_string(),
                    to: "final".to_string(),
                    join: Join::Passed,
                    required: true,
                    when: None,
                    aligned: false,
                    keyed: Vec::new(),
                },
            ]
        );
    }

    /// A vocabulary this build does not know rides as `other` instead of refusing the preview.
    #[test]
    fn an_unknown_kind_join_or_needs_still_reduces() {
        let graph = graph_from_compiled(
            br#"{"type":"playbook","task":[
                 {"name":"a","kind":"oracle","needs":"quorum","join":"majority"}]}"#,
        )
        .expect("reduces");
        assert_eq!(node(&graph, "a").kind, TaskKind::Other);
        assert_eq!(node(&graph, "a").needs, Needs::Other);
        assert_eq!(node(&graph, "a").join, Join::Other);
        assert!(node(&graph, "a").required, "a task is required by default");
    }

    #[test]
    fn a_typed_emits_reduces_to_its_field_names() {
        let graph = graph_from_compiled(
            br#"{"type":"playbook","task":[
                 {"name":"a","kind":"command","emits":{"tier":["high","low"],"count":"integer"}}]}"#,
        )
        .expect("reduces");
        assert_eq!(node(&graph, "a").emits, ["count", "tier"]);
    }

    const ROUTED: &[u8] = br#"{
      "type": "playbook",
      "task": [
        {"name":"scan","kind":"command","command":"scan","emits":["targets","notes"]},
        {"name":"triage","kind":"route","depends_on":["scan"],"needs":"systemone",
         "over":{"task":"scan","field":"targets"},"max_fanout":120,
         "keyed":[{"task":"scan","field":"notes"}],
         "questions":{"tier":{"instructions":"how bad","type":"choice","options":[]},
                      "scope":{"instructions":"how wide","type":"noul"}},
         "decider":{"kind":"model","min_confidence":0.6}},
        {"name":"fix","kind":"agent","prompt":"fix","depends_on":["scan","triage"],
         "over":{"task":"scan","field":"targets"},"max_fanout":120,
         "keyed":[{"task":"scan","field":"notes"}],
         "when":{"task":"triage","question":"tier","is":["high","low"]}},
        {"name":"roll","kind":"command","command":"roll","depends_on":["fix"]}
      ]
    }"#;

    #[test]
    fn a_route_reduces_to_a_route_node_with_its_question_ids() {
        let graph = graph_from_compiled(ROUTED).expect("reduces");
        let triage = node(&graph, "triage");
        assert_eq!(triage.kind, TaskKind::Route);
        assert_eq!(triage.questions, ["scope", "tier"]);
        assert_eq!(triage.keyed, ["scan.notes"]);
        assert!(node(&graph, "fix").questions.is_empty());
        assert_eq!(
            node(&graph, "fix").when,
            Some(WhenDto {
                route: "triage".to_string(),
                question: "tier".to_string(),
                labels: vec!["high".to_string(), "low".to_string()],
            })
        );
    }

    #[test]
    fn edges_carry_the_when_alignment_and_keyed_fields_they_are_read_through() {
        let graph = graph_from_compiled(ROUTED).expect("reduces");
        let edge = |from: &str, to: &str| {
            graph
                .edges
                .iter()
                .find(|e| e.from == from && e.to == to)
                .unwrap_or_else(|| panic!("{from} -> {to} is drawn"))
        };
        let into_route = edge("scan", "triage");
        assert!(
            !into_route.aligned,
            "the producer of the list is not mapped"
        );
        assert_eq!(into_route.keyed, ["notes"]);
        assert_eq!(into_route.when, None);

        let routed = edge("triage", "fix");
        assert!(routed.aligned, "both map over scan.targets");
        assert!(routed.keyed.is_empty());
        assert_eq!(
            routed.when.as_ref().map(|w| w.question.as_str()),
            Some("tier")
        );

        let narrowed = edge("scan", "fix");
        assert_eq!(narrowed.when, None, "the when is on the route's edge only");
        assert_eq!(narrowed.keyed, ["notes"]);

        let fold = edge("fix", "roll");
        assert!(!fold.aligned, "an unmapped consumer reads the fold");
    }

    #[test]
    fn a_plan_that_will_not_parse_is_an_error_not_an_empty_graph() {
        assert!(graph_from_compiled(b"not json").is_err());
        assert!(graph_from_compiled(b"[]").is_err());
        assert!(
            graph_from_compiled(br#"{"task":[]}"#).is_err(),
            "a plan with no workflow type is not a plan"
        );
    }
}
