//! Quantities flowing between named stages, drawn as ribbons (a Sankey diagram), and which
//! column each stage is drawn in.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{MAGNITUDE, Problem, Tone};

/// A stage: a bar as tall as what flows through it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowNode {
    /// Unique within its flow.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub tone: Tone,
}

/// A ribbon from one stage to a later one, as wide as its value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowLink {
    pub from: String,
    pub to: String,
    /// Greater than zero, and at most [`MAGNITUDE`].
    pub value: f64,
    /// The ribbon's colour; the tone of the stage it reaches when not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

/// The column each node is drawn in, in the nodes' order: 0 for a node nothing flows into,
/// otherwise one past the furthest column that flows into it.
pub fn flow_columns(nodes: &[FlowNode], links: &[FlowLink]) -> Result<Vec<usize>, Problem> {
    let mut index = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if index.insert(node.id.as_str(), i).is_some() {
            return Err(Problem::FlowDuplicate(node.id.clone()));
        }
    }
    let mut ends = Vec::with_capacity(links.len());
    for link in links {
        let end = |id: &String| {
            index
                .get(id.as_str())
                .copied()
                .ok_or_else(|| Problem::FlowUnknown(id.clone()))
        };
        if !(link.value > 0.0 && link.value <= MAGNITUDE) {
            return Err(Problem::FlowValue(link.from.clone(), link.to.clone()));
        }
        ends.push((end(&link.from)?, end(&link.to)?));
    }
    let mut columns = vec![0; nodes.len()];
    for _ in 0..=nodes.len() {
        let mut moved = false;
        for &(from, to) in &ends {
            if columns[to] <= columns[from] {
                columns[to] = columns[from] + 1;
                moved = true;
            }
        }
        if !moved {
            return Ok(columns);
        }
    }
    Err(Problem::FlowCycle)
}

#[cfg(test)]
mod tests {
    use crate::{FlowLink, FlowNode, Limits, Node, Problem, Tone, View, flow_columns};

    fn node(id: &str) -> FlowNode {
        FlowNode {
            id: id.into(),
            label: id.to_uppercase(),
            tone: Tone::Neutral,
        }
    }

    fn link(from: &str, to: &str, value: f64) -> FlowLink {
        FlowLink {
            from: from.into(),
            to: to.into(),
            value,
            tone: None,
        }
    }

    fn checked(nodes: Vec<FlowNode>, links: Vec<FlowLink>) -> Result<(), Problem> {
        View {
            title: "T".into(),
            body: Node::Flow { nodes, links },
            controls: vec![],
        }
        .check(&Limits::DEFAULT)
    }

    #[test]
    fn a_node_sits_one_column_past_the_furthest_that_flows_into_it() {
        let nodes = ["go", "python", "reach", "scope", "fix", "close", "alone"].map(node);
        let links = vec![
            link("go", "reach", 20.0),
            link("python", "reach", 20.0),
            link("python", "scope", 10.0),
            link("reach", "fix", 8.0),
            link("reach", "close", 32.0),
            link("scope", "close", 10.0),
            link("go", "close", 1.0),
        ];
        assert_eq!(flow_columns(&nodes, &links), Ok(vec![0, 0, 1, 1, 2, 2, 0]));
        assert_eq!(checked(nodes.to_vec(), links), Ok(()));
        assert_eq!(flow_columns(&[], &[]), Ok(vec![]));
    }

    #[test]
    fn a_flow_that_cannot_be_drawn_is_refused() {
        let nodes = || ["a", "b", "c"].map(node).to_vec();
        assert_eq!(
            checked(nodes(), vec![link("a", "b", 1.0), link("b", "a", 1.0)]),
            Err(Problem::FlowCycle)
        );
        assert_eq!(
            checked(nodes(), vec![link("a", "a", 1.0)]),
            Err(Problem::FlowCycle)
        );
        assert_eq!(
            checked(nodes(), vec![link("a", "z", 1.0)]),
            Err(Problem::FlowUnknown("z".into()))
        );
        assert_eq!(checked(nodes(), vec![link("a", "b", 1e15)]), Ok(()));
        assert_eq!(checked(nodes(), vec![link("a", "b", 1e-300)]), Ok(()));
        for value in [0.0, -2.0, f64::NAN, f64::INFINITY, 1.1e15] {
            assert_eq!(
                checked(nodes(), vec![link("a", "b", value)]),
                Err(Problem::FlowValue("a".into(), "b".into()))
            );
        }
        assert_eq!(
            checked(vec![node("a"), node("a")], vec![]),
            Err(Problem::FlowDuplicate("a".into()))
        );
        let many: Vec<FlowNode> = (0..61).map(|i| node(&i.to_string())).collect();
        assert_eq!(checked(many, vec![]), Err(Problem::FlowNodes(60)));
        assert_eq!(
            checked(nodes(), vec![link("a", "b", 1.0); 401]),
            Err(Problem::FlowLinks(400))
        );
    }

    #[test]
    fn a_flow_reads_back_as_written() {
        let flow = Node::Flow {
            nodes: vec![node("a"), node("b")],
            links: vec![FlowLink {
                tone: Some(Tone::Bad),
                ..link("a", "b", 3.0)
            }],
        };
        let json = serde_json::to_string(&flow).unwrap();
        assert_eq!(
            json,
            r#"{"flow":{"nodes":[{"id":"a","label":"A","tone":"neutral"},{"id":"b","label":"B","tone":"neutral"}],"links":[{"from":"a","to":"b","value":3.0,"tone":"bad"}]}}"#
        );
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), flow);
        let bare = r#"{"flow":{"nodes":[{"id":"a","label":"A"}],"links":[]}}"#;
        assert!(serde_json::from_str::<Node>(bare).is_ok());
    }
}
