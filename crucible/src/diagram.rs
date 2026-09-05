//! Control flow as data: a transition table, a cursor that walks it, and the renderers that
//! draw it, shared by the loop driver and the plan executor so every machine in the engine
//! is explained the same way and cannot drift from the code that runs it.

use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;

/// A transition the table does not list: a bug in the code walking it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("control bug in {machine}: no transition from {from} on {event}")]
pub struct IllegalTransition {
    pub machine: &'static str,
    pub from: String,
    pub event: String,
}

/// A position in a transition table.
pub struct Cursor<S: 'static, E: 'static> {
    machine: &'static str,
    table: &'static [(S, E, S)],
    state: S,
}

impl<S: Copy + Eq + Debug, E: Copy + Eq + Debug> Cursor<S, E> {
    pub fn new(machine: &'static str, table: &'static [(S, E, S)], start: S) -> Self {
        Self {
            machine,
            table,
            state: start,
        }
    }

    pub fn state(&self) -> S {
        self.state
    }

    pub fn advance(&mut self, event: E) -> Result<S, IllegalTransition> {
        let to = self
            .table
            .iter()
            .find(|(from, ev, _)| *from == self.state && *ev == event)
            .map(|(_, _, to)| *to)
            .ok_or_else(|| IllegalTransition {
                machine: self.machine,
                from: format!("{:?}", self.state),
                event: format!("{event:?}"),
            })?;
        self.state = to;
        Ok(to)
    }
}

/// `PreflightPassed` -> `preflight passed`: an enum variant as an edge label.
pub fn words(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 4);
    for (i, c) in camel.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// One node of a rendered table.
pub struct Node {
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Plain,
    /// The state waits on something outside the machine.
    Idle,
    /// The state runs a whole graph of its own.
    Nested,
    /// A named way the machine ends, one of several.
    Outcome,
    /// The single end marker.
    Terminal,
}

pub struct Cluster {
    pub label: &'static str,
    pub nodes: Vec<Node>,
}

pub struct Edge {
    pub from: String,
    pub to: String,
    pub label: String,
    /// The edge leaves the machine's main flow (an exit or a failure); drawn in the exit color.
    pub exit: bool,
}

/// A table laid out for drawing: nodes grouped into clusters, edges in table order.
pub struct Digraph {
    pub name: &'static str,
    pub start: String,
    pub clusters: Vec<Cluster>,
    pub edges: Vec<Edge>,
}

impl Digraph {
    /// Graphviz source: clusters dashed, idle states dashed, terminal states filled black,
    /// exit edges in the exit color.
    pub fn dot(&self) -> String {
        let mut out = format!(
            "digraph {} {{\n    graph [rankdir=TB, fontname=\"Helvetica\", fontsize=11, \
             fontcolor=\"#55606a\", pad=0.4, nodesep=0.5, ranksep=0.7, splines=true, \
             newrank=true];\n    node [shape=box, style=\"rounded,filled\", fillcolor=\"#e4ebe7\", \
             color=\"#2b6f62\", fontname=\"Helvetica\", fontsize=12, fontcolor=\"#1d2329\", \
             margin=\"0.18,0.1\"];\n    edge [fontname=\"Helvetica\", fontsize=10, \
             color=\"#55606a\", fontcolor=\"#55606a\", arrowsize=0.8];\n    start [shape=point, \
             width=0.14, color=\"#1d2329\"];\n",
            self.name
        );
        for (i, cluster) in self.clusters.iter().enumerate() {
            out.push_str(&format!(
                "    subgraph cluster_{i} {{\n        label=\"{}\"; style=dashed; \
                 color=\"#c8d0cc\"; labeljust=l;\n",
                cluster.label
            ));
            for node in &cluster.nodes {
                let attrs = match node.kind {
                    NodeKind::Plain => "",
                    NodeKind::Idle => " [style=\"rounded,filled,dashed\"]",
                    NodeKind::Nested => " [peripheries=2]",
                    NodeKind::Outcome => " [fillcolor=\"#f5ebe1\", color=\"#8a4b1c\"]",
                    NodeKind::Terminal => {
                        " [shape=doublecircle, label=\"\", width=0.22, fillcolor=\"#1d2329\", \
                         color=\"#1d2329\"]"
                    }
                };
                out.push_str(&format!("        {}{attrs};\n", node.name));
            }
            out.push_str("    }\n");
        }
        out.push_str(&format!("    start -> {};\n", self.start));
        for e in &self.edges {
            let style = if e.exit {
                ", color=\"#8a4b1c\", fontcolor=\"#8a4b1c\""
            } else {
                ""
            };
            out.push_str(&format!(
                "    {} -> {} [label=\"{}\"{style}];\n",
                e.from,
                e.to,
                e.label.replace('\n', "\\n")
            ));
        }
        out.push_str("}\n");
        out
    }

    /// The same table as a mermaid `stateDiagram-v2`.
    pub fn mermaid(&self) -> String {
        let mut out = format!("stateDiagram-v2\n    [*] --> {}\n", self.start);
        for e in &self.edges {
            out.push_str(&format!(
                "    {} --> {}: {}\n",
                e.from,
                e.to,
                e.label.replace('\n', " ")
            ));
        }
        for node in self.clusters.iter().flat_map(|c| &c.nodes) {
            if matches!(node.kind, NodeKind::Terminal | NodeKind::Outcome) {
                out.push_str(&format!("    {} --> [*]\n", node.name));
            }
        }
        out
    }
}

/// The label for an edge that ends the machine: the event, then the token it reports, unless
/// they read the same.
pub fn exit_label(event: &str, token: &str) -> String {
    if event == token {
        event.to_string()
    } else {
        format!("{event}\n→ {token}")
    }
}

/// What every table must satisfy: one target per (state, event), every state reachable from
/// `start`, and no dead ends except the terminal states. Returns the violations.
pub fn table_problems<S: Copy + Eq + Hash + Debug, E: Copy + Eq + Hash + Debug>(
    table: &[(S, E, S)],
    start: S,
    terminal: impl Fn(S) -> bool,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut pairs = HashSet::new();
    for (from, ev, _) in table {
        if !pairs.insert((*from, *ev)) {
            problems.push(format!("{from:?} on {ev:?} is listed twice"));
        }
    }
    let states: HashSet<S> = table.iter().flat_map(|(a, _, b)| [*a, *b]).collect();
    let mut seen = HashSet::from([start]);
    let mut frontier = vec![start];
    while let Some(s) = frontier.pop() {
        for (from, _, to) in table {
            if *from == s && seen.insert(*to) {
                frontier.push(*to);
            }
        }
    }
    for s in &states {
        if !seen.contains(s) {
            problems.push(format!("{s:?} is unreachable from {start:?}"));
        }
        let out = table.iter().filter(|(from, _, _)| from == s).count();
        if terminal(*s) && out > 0 {
            problems.push(format!("{s:?} is terminal but has {out} outgoing edge(s)"));
        }
        if !terminal(*s) && out == 0 {
            problems.push(format!("{s:?} is a dead end"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum S {
        A,
        B,
        End,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum E {
        Go,
        Stop,
    }
    const T: &[(S, E, S)] = &[(S::A, E::Go, S::B), (S::B, E::Stop, S::End)];

    #[test]
    fn a_cursor_walks_the_table_and_names_what_it_refuses() {
        let mut c = Cursor::new("t", T, S::A);
        assert_eq!(c.advance(E::Go), Ok(S::B));
        let err = c.advance(E::Go).unwrap_err();
        assert_eq!(err.machine, "t");
        assert_eq!(
            err.to_string(),
            "control bug in t: no transition from B on Go"
        );
        assert_eq!(c.state(), S::B);
    }

    #[test]
    fn the_checks_find_duplicates_unreachable_states_and_dead_ends() {
        assert!(table_problems(T, S::A, |s| s == S::End).is_empty());
        let bad: &[(S, E, S)] = &[(S::A, E::Go, S::A), (S::A, E::Go, S::B)];
        let problems = table_problems(bad, S::A, |s| s == S::End);
        assert!(
            problems.iter().any(|p| p.contains("listed twice")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("dead end")),
            "{problems:?}"
        );
        let orphan: &[(S, E, S)] = &[(S::A, E::Go, S::End), (S::B, E::Stop, S::End)];
        let problems = table_problems(orphan, S::A, |s| s == S::End);
        assert!(
            problems.iter().any(|p| p.contains("unreachable")),
            "{problems:?}"
        );
    }

    #[test]
    fn words_and_exit_labels_read_as_prose() {
        assert_eq!(words("PreflightPassed"), "preflight passed");
        assert_eq!(exit_label("stopped", "stopped"), "stopped");
        assert_eq!(exit_label("over budget", "budget"), "over budget\n→ budget");
    }

    #[test]
    fn dot_and_mermaid_carry_every_edge_and_the_terminal_marker() {
        let g = Digraph {
            name: "t",
            start: "A".into(),
            clusters: vec![Cluster {
                label: "all",
                nodes: vec![
                    Node {
                        name: "A".into(),
                        kind: NodeKind::Plain,
                    },
                    Node {
                        name: "B".into(),
                        kind: NodeKind::Idle,
                    },
                    Node {
                        name: "End".into(),
                        kind: NodeKind::Terminal,
                    },
                ],
            }],
            edges: vec![
                Edge {
                    from: "A".into(),
                    to: "B".into(),
                    label: "go".into(),
                    exit: false,
                },
                Edge {
                    from: "B".into(),
                    to: "End".into(),
                    label: "stop\n→ done".into(),
                    exit: true,
                },
            ],
        };
        let dot = g.dot();
        assert!(dot.contains("A -> B [label=\"go\"];"));
        assert!(dot.contains("B -> End [label=\"stop\\n→ done\", color=\"#8a4b1c\""));
        assert!(dot.contains("End [shape=doublecircle"));
        let mmd = g.mermaid();
        assert!(mmd.contains("B --> End: stop → done"));
        assert!(mmd.contains("End --> [*]"));
    }
}
