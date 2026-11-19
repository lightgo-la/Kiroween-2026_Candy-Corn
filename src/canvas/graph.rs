//! Canvas graph data model. Saved and loaded as JSON (`.canvas.json`).
//! API keys are never saved, only the names of their environment variables.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Auto layout spacing: the horizontal gap between layers (left→right) and the vertical gap within a layer.
pub const LAYOUT_COL: f32 = 280.0;
pub const LAYOUT_ROW: f32 = 100.0;
/// How far a duplicated node is placed from the original.
pub const DUPLICATE_OFFSET: [f32; 2] = [32.0, 32.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    OpenAi,
    Gemini,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Anthropic, Provider::OpenAi, Provider::Gemini];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::OpenAi => "OpenAI",
            Provider::Gemini => "Gemini",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Anthropic => "claude-sonnet-5-5",
            Provider::OpenAi => "gpt-6.2-sol",
            Provider::Gemini => "gemini-4-neon",
        }
    }

    /// Current models offered by the model pickers as (model ID, display name), most capable first. The list is from
    /// the providers' model pages (November 2026); any other model ID can still be typed into the model field.
    pub fn models(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Provider::Anthropic => &[
                ("claude-fable-5-2", "Claude Fable 5.2"),
                ("claude-opus-5-5", "Claude Opus 5.5"),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                ("claude-haiku-5-5", "Claude Haiku 5.5"),
            ],
            Provider::OpenAi => &[
                ("gpt-6.2-astra", "GPT-6.2 Astra"),
                ("gpt-6.2-sol", "GPT-6.2 Sol"),
                ("gpt-6.1-terra", "GPT-6.1 Terra"),
                ("gpt-6-luna", "GPT-6 Luna"),
            ],
            Provider::Gemini => &[
                ("gemini-4-carbon", "Gemini 4 Carbon"),
                ("gemini-4-argon", "Gemini 4 Argon"),
                ("gemini-4-neon", "Gemini 4 Neon"),
                ("gemini-4-hydrogen", "Gemini 4 Hydrogen"),
            ],
        }
    }

    pub fn default_key_env(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAi => "https://api.openai.com",
            Provider::Gemini => "https://generativelanguage.googleapis.com",
        }
    }

    /// Upper temperature limit the provider accepts. Anthropic takes 0~1, the others 0~2.
    pub fn temperature_max(self) -> f32 {
        match self {
            Provider::Anthropic => 1.0,
            Provider::OpenAi | Provider::Gemini => 2.0,
        }
    }

    /// Fits the temperature into the range the provider accepts.
    pub fn clamp_temperature(self, t: f32) -> f32 {
        if t.is_nan() {
            0.0
        } else {
            t.clamp(0.0, self.temperature_max())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptTarget {
    Claude,
    Gpt,
    Gemini,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptFormat {
    Json,
    Table,
}

fn yes() -> bool {
    true
}

/// Settings per node kind, told apart by the `type` field.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// Fixed text input
    Text { text: String },
    /// Prompt compiler (lint + per-model serialization). `{{input}}` in the instruction is replaced with the
    /// upstream output.
    /// A relative `data_path` starts from the folder that contains the graph file.
    Prompt {
        role: String,
        constraints: String,
        fields: String,
        target: PromptTarget,
        format: PromptFormat,
        #[serde(default)]
        data_path: String,
    },
    /// LLM API call
    Llm {
        provider: Provider,
        model: String,
        #[serde(default)]
        system: String,
        key_env: String,
        #[serde(default)]
        base_url: String,
        max_tokens: u32,
        #[serde(default)]
        temperature: Option<f32>,
        /// If true, only a summary of the request is returned, without a network call (avoids charges).
        #[serde(default = "yes")]
        dry_run: bool,
    },
    /// MCP server (stdio) tool call. `arguments` is a JSON template in which `{{input}}` is replaced.
    Mcp {
        command: String,
        #[serde(default)]
        args: String,
        tool: String,
        arguments: String,
    },
    /// Collects the final result
    Output,
}

impl NodeKind {
    pub fn label(&self) -> &'static str {
        match self {
            NodeKind::Text { .. } => "Text",
            NodeKind::Prompt { .. } => "Prompt",
            NodeKind::Llm { .. } => "LLM API",
            NodeKind::Mcp { .. } => "MCP Tool",
            NodeKind::Output => "Output",
        }
    }

    pub fn has_input(&self) -> bool {
        !matches!(self, NodeKind::Text { .. })
    }

    pub fn has_output(&self) -> bool {
        !matches!(self, NodeKind::Output)
    }

    pub fn new_text() -> Self {
        NodeKind::Text { text: String::new() }
    }

    /// The default instruction and constraint are written in the language in use when the node is created. After
    /// that they are user text, so they stay the same when the language changes.
    pub fn new_prompt() -> Self {
        NodeKind::Prompt {
            role: tr!("Analyze the following:\n{{input}}").into(),
            constraints: tr!("Do not infer anything that is not in the data").into(),
            fields: "summary,recommendations".into(),
            target: PromptTarget::Claude,
            format: PromptFormat::Json,
            data_path: String::new(),
        }
    }

    pub fn new_llm() -> Self {
        let p = Provider::Anthropic;
        NodeKind::Llm {
            provider: p,
            model: p.default_model().into(),
            system: String::new(),
            key_env: p.default_key_env().into(),
            base_url: String::new(),
            max_tokens: 1024,
            temperature: None,
            dry_run: true,
        }
    }

    pub fn new_mcp() -> Self {
        NodeKind::Mcp {
            command: String::new(),
            args: String::new(),
            tool: String::new(),
            arguments: "{\"text\": \"{{input}}\"}".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: u32,
    pub title: String,
    pub pos: [f32; 2],
    pub kind: NodeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: u32,
    pub to: u32,
}

/// A saved prompt test case. Run results are not saved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: String,
    /// If not empty, the final output must contain this string to pass
    #[serde(default)]
    pub expect: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Prompt test cases. Old files without this entry still open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestSpec>,
    /// Id of the Text node that receives the test input
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_node: Option<u32>,
}

impl Graph {
    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Next node id. Returns an error if the largest id is at the u32 limit (checked so the addition cannot
    /// overflow).
    fn next_id(&self) -> Result<u32, String> {
        match self.nodes.iter().map(|n| n.id).max() {
            None => Ok(1),
            Some(m) => m.checked_add(1).ok_or_else(|| {
                trf!(
                    "Node ids have reached the limit ({}); no more nodes can be added",
                    u32::MAX
                )
            }),
        }
    }

    pub fn add_node(&mut self, kind: NodeKind, pos: [f32; 2]) -> Result<u32, String> {
        let id = self.next_id()?;
        let title = format!("{} {}", kind.label(), id);
        self.nodes.push(Node { id, title, pos, kind });
        Ok(id)
    }

    #[cfg(test)]
    pub fn add(&mut self, kind: NodeKind, pos: [f32; 2]) -> u32 {
        self.add_node(kind, pos).expect("add a test node")
    }

    /// Duplicates a node: a new id, a position offset from the original, the same settings. Connections are not
    /// copied.
    pub fn duplicate_node(&mut self, id: u32) -> Result<u32, String> {
        let src = self.node(id).ok_or_else(|| tr!("No such node").to_string())?;
        let (kind, title, pos) = (src.kind.clone(), src.title.clone(), src.pos);
        let new_id = self.next_id()?;
        // The copy is named in the language in use when duplicating.
        let title = self.unique_title(&trf!("{title} copy", title = title));
        let pos = [pos[0] + DUPLICATE_OFFSET[0], pos[1] + DUPLICATE_OFFSET[1]];
        self.nodes.push(Node {
            id: new_id,
            title,
            pos,
            kind,
        });
        Ok(new_id)
    }

    /// If a node with the same name exists, appends a number to make a name that is not taken.
    fn unique_title(&self, base: &str) -> String {
        let taken = |t: &str| self.nodes.iter().any(|n| n.title == t);
        if !taken(base) {
            return base.to_string();
        }
        let mut n = 2u64;
        loop {
            let t = format!("{base} {n}");
            if !taken(&t) {
                return t;
            }
            n += 1;
        }
    }

    pub fn remove_node(&mut self, id: u32) {
        self.nodes.retain(|n| n.id != id);
        self.edges.retain(|e| e.from != id && e.to != id);
        if self.test_node == Some(id) {
            self.test_node = None;
        }
    }

    /// Adds an edge. Returns the reason if a port is missing, the edge is a duplicate or it would create a cycle.
    pub fn connect(&mut self, from: u32, to: u32) -> Result<(), String> {
        if from == to {
            return Err(tr!("A node cannot be connected to itself").into());
        }
        let (Some(a), Some(b)) = (self.node(from), self.node(to)) else {
            return Err(tr!("No such node").into());
        };
        if !a.kind.has_output() {
            return Err(trf!("The {} node has no output port", a.kind.label()));
        }
        if !b.kind.has_input() {
            return Err(trf!("The {} node has no input port", b.kind.label()));
        }
        let e = Edge { from, to };
        if self.edges.contains(&e) {
            return Err(tr!("Already connected").into());
        }
        self.edges.push(e);
        if self.topo_order().is_err() {
            self.edges.pop();
            return Err(tr!("Cyclic connections are not allowed").into());
        }
        Ok(())
    }

    /// Upstream node ids in input edge order (the order the edges were added).
    pub fn upstream(&self, id: u32) -> Vec<u32> {
        self.edges.iter().filter(|e| e.to == id).map(|e| e.from).collect()
    }

    /// Kahn's algorithm. Err if there is a cycle. Within the same step the node list order is kept.
    /// Outgoing edges are collected up front so that many edges do not slow it down (O(nodes + edges)).
    pub fn topo_order(&self) -> Result<Vec<u32>, String> {
        let ids: HashSet<u32> = self.nodes.iter().map(|n| n.id).collect();
        let mut indeg: HashMap<u32, usize> = self.nodes.iter().map(|n| (n.id, 0)).collect();
        let mut outgoing: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in &self.edges {
            if !ids.contains(&e.from) || !ids.contains(&e.to) {
                return Err(missing_node_edge(e));
            }
            *indeg.get_mut(&e.to).expect("checked") += 1;
            outgoing.entry(e.from).or_default().push(e.to);
        }
        let mut queue: VecDeque<u32> = self.nodes.iter().filter(|n| indeg[&n.id] == 0).map(|n| n.id).collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id);
            for &to in outgoing.get(&id).map(Vec::as_slice).unwrap_or_default() {
                let d = indeg.get_mut(&to).expect("checked");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(to);
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            Err(tr!("The graph has a cycle").into())
        }
    }

    /// Rearranges the nodes in layers from left to right.
    /// A node's layer is the length of its longest upstream path; within a layer the top-to-bottom order follows
    /// the average height of the upstream nodes (overlapping nodes are pushed down). A graph that cannot be sorted
    /// topologically (a cycle) is an Err.
    pub fn auto_layout(&mut self) -> Result<(), String> {
        let order = self.topo_order()?;
        let mut layer: HashMap<u32, usize> = HashMap::new();
        for &id in &order {
            let l = self
                .upstream(id)
                .iter()
                .map(|u| layer.get(u).map_or(0, |l| l + 1))
                .max()
                .unwrap_or(0);
            layer.insert(id, l);
        }
        let last = layer.values().copied().max().unwrap_or(0);
        let mut ys: HashMap<u32, f32> = HashMap::new();
        let mut placed: HashMap<u32, [f32; 2]> = HashMap::new();
        for l in 0..=last {
            // Target height: the average height of the upstream nodes. The first layer is stacked in list order.
            let mut members: Vec<(u32, Option<f32>)> = self
                .nodes
                .iter()
                .filter(|n| layer.get(&n.id) == Some(&l))
                .map(|n| {
                    let ups = self.upstream(n.id);
                    let want =
                        (!ups.is_empty()).then(|| ups.iter().filter_map(|u| ys.get(u)).sum::<f32>() / ups.len() as f32);
                    (n.id, want)
                })
                .collect();
            // The sort is stable, so the list order is kept when the targets are equal or missing.
            members.sort_by(|a, b| match (a.1, b.1) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => std::cmp::Ordering::Equal,
            });
            let mut next_free = f32::NEG_INFINITY;
            for (i, (id, want)) in members.into_iter().enumerate() {
                let y = match want {
                    Some(w) => w.max(next_free),
                    None => i as f32 * LAYOUT_ROW,
                };
                next_free = y + LAYOUT_ROW;
                ys.insert(id, y);
                placed.insert(id, [l as f32 * LAYOUT_COL, y]);
            }
        }
        for n in &mut self.nodes {
            if let Some(p) = placed.get(&n.id) {
                n.pos = *p;
            }
        }
        Ok(())
    }

    /// Structure check: duplicate node ids, edges to missing nodes, port rule violations (edges leaving Output,
    /// edges entering Text), self-connections, duplicate edges, cycles.
    pub fn validate(&self) -> Result<(), String> {
        const POS_MAX: f32 = 1e12;
        let mut seen = HashSet::new();
        if let Some(n) = self.nodes.iter().find(|n| !seen.insert(n.id)) {
            return Err(trf!("Duplicate node id {}", n.id));
        }
        let index: HashMap<u32, &Node> = self.nodes.iter().map(|n| (n.id, n)).collect();
        if let Some(n) = self.nodes.iter().find(|n| !n.pos.iter().all(|v| v.is_finite())) {
            return Err(trf!(
                "Node {} has an invalid position (coordinates must be finite numbers)",
                n.id
            ));
        }
        // Coordinates this large overflow the canvas math (the view becomes infinite and a save writes null)
        if let Some(n) = self.nodes.iter().find(|n| n.pos.iter().any(|v| v.abs() > POS_MAX)) {
            return Err(trf!(
                "Node {} is too far from the origin (coordinates must stay within ±1e12)",
                n.id
            ));
        }
        let mut edges = HashSet::new();
        for e in &self.edges {
            let (Some(a), Some(b)) = (index.get(&e.from), index.get(&e.to)) else {
                return Err(missing_node_edge(e));
            };
            if e.from == e.to {
                return Err(trf!("Edge {}->{} connects a node to itself", e.from, e.to));
            }
            if !a.kind.has_output() {
                return Err(trf!(
                    "The {} node (id {}) has no output port, so edge {}->{} is not allowed",
                    a.kind.label(),
                    a.id,
                    e.from,
                    e.to
                ));
            }
            if !b.kind.has_input() {
                return Err(trf!(
                    "The {} node (id {}) has no input port, so edge {}->{} is not allowed",
                    b.kind.label(),
                    b.id,
                    e.from,
                    e.to
                ));
            }
            if !edges.insert((e.from, e.to)) {
                return Err(trf!("Duplicate edge {}->{}", e.from, e.to));
            }
        }
        self.topo_order()?;
        Ok(())
    }

    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn from_json(s: &str) -> Result<Graph, String> {
        let g: Graph =
            serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|e| trf!("Graph JSON error: {e}", e = e))?;
        g.validate()?;
        Ok(g)
    }
}

/// Error message for an edge that points to a missing node (shared by the topological sort and the structure
/// check).
fn missing_node_edge(e: &Edge) -> String {
    trf!("Edge {}->{} points to a node that does not exist", e.from, e.to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    fn chain() -> (Graph, u32, u32, u32) {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_text(), [0.0, 0.0]);
        let b = g.add(NodeKind::new_prompt(), [0.0, 0.0]);
        let c = g.add(NodeKind::Output, [0.0, 0.0]);
        (g, a, b, c)
    }

    /// Loads a graph built without checks (it can still be saved) by going through JSON.
    fn load(g: &Graph) -> Result<Graph, String> {
        Graph::from_json(&g.to_json().unwrap())
    }

    #[test]
    fn connect_rules() {
        let (mut g, a, b, c) = chain();
        assert!(g.connect(a, b).is_ok());
        assert!(g.connect(a, b).is_err(), "duplicate");
        assert!(g.connect(b, b).is_err(), "self-connection");
        assert!(g.connect(b, a).is_err(), "Text has no input port");
        assert!(g.connect(c, b).is_err(), "Output has no output port");
        assert!(g.connect(b, c).is_ok());
        assert_eq!(g.topo_order().unwrap(), vec![a, b, c]);
    }

    #[test]
    fn rejects_cycle() {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_prompt(), [0.0, 0.0]);
        let b = g.add(NodeKind::new_prompt(), [0.0, 0.0]);
        g.connect(a, b).unwrap();
        assert!(g.connect(b, a).is_err());
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn json_roundtrip_and_validation() {
        let (mut g, a, b, _) = chain();
        g.connect(a, b).unwrap();
        let g2 = load(&g).unwrap();
        assert_eq!(g2.nodes.len(), 3);
        assert_eq!(g2.edges, g.edges);
        assert!(Graph::from_json(r#"{"nodes":[],"edges":[{"from":1,"to":2}]}"#).is_err());
    }

    #[test]
    fn remove_node_drops_edges_and_test_node() {
        let (mut g, a, b, c) = chain();
        g.connect(a, b).unwrap();
        g.connect(b, c).unwrap();
        g.test_node = Some(b);
        g.remove_node(b);
        assert!(g.edges.is_empty());
        assert_eq!(g.test_node, None);
    }

    #[test]
    fn load_rejects_invalid_edges() {
        let (g, a, b, c) = chain();
        let with_edges = |edges: &[(u32, u32)]| {
            let mut g = g.clone();
            g.edges = edges.iter().map(|&(from, to)| Edge { from, to }).collect();
            load(&g)
        };
        assert!(with_edges(&[(a, b), (b, c)]).is_ok());
        let err = with_edges(&[(a, b), (a, b)]).unwrap_err();
        assert!(err.contains("Duplicate edge"), "{err}");
        let err = with_edges(&[(c, b)]).unwrap_err();
        assert!(err.contains("Output") && err.contains("output port"), "{err}");
        let err = with_edges(&[(b, a)]).unwrap_err();
        assert!(err.contains("Text") && err.contains("input port"), "{err}");
        let err = with_edges(&[(b, b)]).unwrap_err();
        assert!(err.contains("itself"), "{err}");
        let err = with_edges(&[(a, 99)]).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        let mut g2 = g.clone();
        let extra = g2.add(NodeKind::new_prompt(), [0.0, 0.0]);
        g2.edges = vec![Edge { from: b, to: extra }, Edge { from: extra, to: b }];
        assert!(load(&g2).unwrap_err().contains("cycle"));
        let mut dup = g.clone();
        dup.nodes[1].id = dup.nodes[0].id;
        assert!(load(&dup).unwrap_err().contains("Duplicate node id"));
    }

    #[test]
    fn id_overflow_is_an_error_not_a_panic() {
        let mut g = Graph::default();
        g.nodes.push(Node {
            id: u32::MAX,
            title: "end".into(),
            pos: [0.0, 0.0],
            kind: NodeKind::new_text(),
        });
        let mut g = load(&g).expect("a graph with id u32::MAX still loads");
        assert!(g.add_node(NodeKind::Output, [0.0, 0.0]).unwrap_err().contains("limit"));
        assert!(g.duplicate_node(u32::MAX).is_err());
        assert_eq!(g.nodes.len(), 1, "nothing is added on failure");

        let mut near = Graph::default();
        near.nodes.push(Node {
            id: u32::MAX - 1,
            title: "almost".into(),
            pos: [0.0, 0.0],
            kind: NodeKind::new_text(),
        });
        assert_eq!(near.add_node(NodeKind::Output, [0.0, 0.0]), Ok(u32::MAX));
        assert!(near.add_node(NodeKind::Output, [0.0, 0.0]).is_err());
    }

    #[test]
    fn duplicate_copies_settings_without_edges() {
        let (mut g, a, b, c) = chain();
        g.connect(a, b).unwrap();
        g.connect(b, c).unwrap();
        g.nodes[1].pos = [100.0, 50.0];
        g.nodes[1].title = "Summarizer".into();
        let copy = g.duplicate_node(b).unwrap();
        assert!(copy > c, "new id");
        assert_eq!(g.edges.len(), 2, "connections are not copied");
        assert!(g.upstream(copy).is_empty() && !g.edges.iter().any(|e| e.from == copy));
        let (orig, dup) = (g.node(b).unwrap(), g.node(copy).unwrap());
        assert_eq!(dup.pos, [100.0 + DUPLICATE_OFFSET[0], 50.0 + DUPLICATE_OFFSET[1]]);
        assert_eq!(dup.title, "Summarizer copy");
        assert_eq!(format!("{:?}", orig.kind), format!("{:?}", dup.kind));
        // Duplicating again does not repeat the name.
        let again = g.duplicate_node(b).unwrap();
        assert_eq!(g.node(again).unwrap().title, "Summarizer copy 2");
        assert!(g.duplicate_node(999).is_err());
        g.validate().unwrap();
    }

    fn pos_of(g: &Graph, id: u32) -> [f32; 2] {
        g.node(id).unwrap().pos
    }

    #[test]
    fn auto_layout_chain_goes_left_to_right() {
        let mut g = Graph::default();
        let ids = [
            g.add(NodeKind::new_text(), [500.0, 500.0]),
            g.add(NodeKind::new_prompt(), [-30.0, 9.0]),
            g.add(NodeKind::new_llm(), [7.0, 70.0]),
            g.add(NodeKind::Output, [0.0, 0.0]),
        ];
        for w in ids.windows(2) {
            g.connect(w[0], w[1]).unwrap();
        }
        g.auto_layout().unwrap();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(pos_of(&g, *id), [i as f32 * LAYOUT_COL, 0.0], "node {id}");
        }
    }

    #[test]
    fn auto_layout_fan_in_is_centered_and_diamond_is_layered() {
        let mut g = Graph::default();
        let t1 = g.add(NodeKind::new_text(), [0.0; 2]);
        let t2 = g.add(NodeKind::new_text(), [0.0; 2]);
        let p = g.add(NodeKind::new_prompt(), [0.0; 2]);
        g.connect(t1, p).unwrap();
        g.connect(t2, p).unwrap();
        g.auto_layout().unwrap();
        assert_eq!(pos_of(&g, t1), [0.0, 0.0]);
        assert_eq!(pos_of(&g, t2), [0.0, LAYOUT_ROW]);
        assert_eq!(pos_of(&g, p), [LAYOUT_COL, LAYOUT_ROW / 2.0]);

        let mut d = Graph::default();
        let t = d.add(NodeKind::new_text(), [0.0; 2]);
        let p1 = d.add(NodeKind::new_prompt(), [0.0; 2]);
        let p2 = d.add(NodeKind::new_prompt(), [0.0; 2]);
        let o = d.add(NodeKind::Output, [0.0; 2]);
        for (a, b) in [(t, p1), (t, p2), (p1, o), (p2, o)] {
            d.connect(a, b).unwrap();
        }
        d.auto_layout().unwrap();
        assert_eq!(pos_of(&d, p1), [LAYOUT_COL, 0.0]);
        assert_eq!(pos_of(&d, p2), [LAYOUT_COL, LAYOUT_ROW]);
        assert_eq!(pos_of(&d, o), [2.0 * LAYOUT_COL, LAYOUT_ROW / 2.0]);
    }

    #[test]
    fn auto_layout_isolated_nodes_and_cycles() {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_text(), [9.0, 9.0]);
        let b = g.add(NodeKind::Output, [9.0, 9.0]);
        g.auto_layout().unwrap();
        assert_eq!(pos_of(&g, a), [0.0, 0.0]);
        assert_eq!(pos_of(&g, b), [0.0, LAYOUT_ROW]);
        Graph::default().auto_layout().unwrap();

        let mut cyc = Graph::default();
        let x = cyc.add(NodeKind::new_prompt(), [1.0, 2.0]);
        let y = cyc.add(NodeKind::new_prompt(), [3.0, 4.0]);
        cyc.edges = vec![Edge { from: x, to: y }, Edge { from: y, to: x }];
        assert!(cyc.auto_layout().is_err());
        assert_eq!(pos_of(&cyc, x), [1.0, 2.0], "positions are untouched on failure");
    }

    /// Even in random DAGs, edges always point to the right and nodes never overlap.
    #[test]
    fn auto_layout_never_overlaps_nodes() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for round in 0..200 {
            let n = 1 + next(14);
            let mut g = Graph::default();
            let ids: Vec<u32> = (0..n).map(|_| g.add(NodeKind::new_prompt(), [0.0; 2])).collect();
            for _ in 0..next(2 * n + 1) {
                let (i, j) = (next(n), next(n));
                if i < j {
                    let _ = g.connect(ids[i], ids[j]);
                }
            }
            g.auto_layout().unwrap();
            for e in &g.edges {
                assert!(pos_of(&g, e.to)[0] > pos_of(&g, e.from)[0], "round {round}: {e:?}");
            }
            for (i, a) in g.nodes.iter().enumerate() {
                for b in &g.nodes[i + 1..] {
                    let apart = (a.pos[0] - b.pos[0]).abs() >= LAYOUT_COL || (a.pos[1] - b.pos[1]).abs() >= LAYOUT_ROW;
                    assert!(apart, "round {round}: {} and {} overlap", a.id, b.id);
                }
            }
        }
    }

    #[test]
    fn old_files_without_tests_still_load() {
        let old = r#"{"nodes":[{"id":1,"title":"in","pos":[0,0],"kind":{"type":"text","text":"x"}}],"edges":[]}"#;
        let g = Graph::from_json(old).unwrap();
        assert!(g.tests.is_empty() && g.test_node.is_none());
        // Without tests nothing is written to the file either, so it is saved in the old format.
        let json = g.to_json().unwrap();
        assert!(!json.contains("tests") && !json.contains("test_node"), "{json}");
    }

    #[test]
    fn saved_tests_roundtrip() {
        let (mut g, a, _, _) = chain();
        g.test_node = Some(a);
        g.tests = vec![
            TestSpec {
                name: "basic".into(),
                input: ko_fixture!("graph.model.saved_test_input").into(),
                expect: "dry-run".into(),
            },
            TestSpec {
                name: "empty".into(),
                ..TestSpec::default()
            },
        ];
        let back = load(&g).unwrap();
        assert_eq!(back.tests, g.tests);
        assert_eq!(back.test_node, Some(a));
        // Cases with only some of the fields open too.
        let partial = r#"{"nodes":[],"edges":[],"tests":[{"name":"n"},{"input":"i"}]}"#;
        let g = Graph::from_json(partial).unwrap();
        assert_eq!(g.tests[0].name, "n");
        assert_eq!(g.tests[1].input, "i");
    }

    #[test]
    fn sample_file_has_readable_layout() {
        let g = Graph::from_json(include_str!("../../samples/flow.canvas.json")).unwrap();
        assert_eq!(g.nodes.len(), 4);
        for e in &g.edges {
            let (a, b) = (g.node(e.from).unwrap(), g.node(e.to).unwrap());
            assert!(
                b.pos[0] >= a.pos[0] + 200.0 + 40.0,
                "{} -> {}: the horizontal gap is too small",
                a.title,
                b.title
            );
        }
        for (i, a) in g.nodes.iter().enumerate() {
            for b in &g.nodes[i + 1..] {
                assert_ne!(a.pos, b.pos, "{} and {} have the same position", a.title, b.title);
            }
        }
    }

    #[test]
    fn large_graphs_load_quickly() {
        // At this size, O(N^2) work that scans the node list for every edge would take seconds.
        let mut g = Graph::default();
        let mut prev = None;
        for i in 0..20_000u32 {
            let kind = if i == 0 {
                NodeKind::new_text()
            } else {
                NodeKind::new_prompt()
            };
            let id = g.add(kind, [i as f32, 0.0]);
            if let Some(p) = prev {
                g.edges.push(Edge { from: p, to: id });
            }
            prev = Some(id);
        }
        let json = g.to_json().unwrap();
        let t = std::time::Instant::now();
        let back = Graph::from_json(&json).unwrap();
        assert!(t.elapsed().as_secs_f32() < 3.0, "load {:?}", t.elapsed());
        assert_eq!(back.topo_order().unwrap().len(), 20_000);
    }

    #[test]
    fn non_finite_positions_are_rejected() {
        let one = |pos: &str| {
            format!(r#"{{"nodes":[{{"id":1,"title":"t","pos":{pos},"kind":{{"type":"output"}}}}],"edges":[]}}"#)
        };
        assert!(Graph::from_json(&one("[1.5, -2]")).is_ok());
        // Numbers beyond the f32 range become infinite. The canvas cannot draw them, so they are rejected on load.
        let err = Graph::from_json(&one("[1e39, 0]")).unwrap_err();
        assert!(err.contains("invalid position"), "{err}");
    }

    #[test]
    fn temperature_is_clamped_per_provider() {
        assert_eq!(Provider::Anthropic.clamp_temperature(1.7), 1.0);
        assert_eq!(Provider::OpenAi.clamp_temperature(1.7), 1.7);
        assert_eq!(Provider::Gemini.clamp_temperature(2.5), 2.0);
        assert_eq!(Provider::Anthropic.clamp_temperature(-0.5), 0.0);
        assert_eq!(Provider::OpenAi.clamp_temperature(f32::NAN), 0.0);
        for p in Provider::ALL {
            assert_eq!(p.clamp_temperature(p.temperature_max()), p.temperature_max());
        }
    }

    // ---------- English ----------

    #[test]
    fn validation_and_connect_errors_in_english() {
        with_lang(Lang::En, || {
            let (g, a, b, c) = chain();
            let with_edges = |edges: &[(u32, u32)]| {
                let mut g = g.clone();
                g.edges = edges.iter().map(|&(from, to)| Edge { from, to }).collect();
                load(&g).unwrap_err()
            };
            assert_eq!(with_edges(&[(a, b), (a, b)]), format!("Duplicate edge {a}->{b}"));
            assert_eq!(
                with_edges(&[(c, b)]),
                format!("The Output node (id {c}) has no output port, so edge {c}->{b} is not allowed")
            );
            assert_eq!(
                with_edges(&[(b, a)]),
                format!("The Text node (id {a}) has no input port, so edge {b}->{a} is not allowed")
            );
            assert_eq!(
                with_edges(&[(b, b)]),
                format!("Edge {b}->{b} connects a node to itself")
            );
            assert_eq!(
                with_edges(&[(a, 99)]),
                format!("Edge {a}->99 points to a node that does not exist")
            );
            let mut cyc = g.clone();
            let extra = cyc.add(NodeKind::new_prompt(), [0.0, 0.0]);
            cyc.edges = vec![Edge { from: b, to: extra }, Edge { from: extra, to: b }];
            assert_eq!(load(&cyc).unwrap_err(), "The graph has a cycle");
            let mut dup = g.clone();
            dup.nodes[1].id = dup.nodes[0].id;
            assert_eq!(load(&dup).unwrap_err(), format!("Duplicate node id {a}"));
            let err = Graph::from_json("{").unwrap_err();
            assert!(err.starts_with("Graph JSON error: "), "{err}");
            let far = r#"{"nodes":[{"id":1,"title":"t","pos":[1e39,0],"kind":{"type":"output"}}],"edges":[]}"#;
            assert_eq!(
                Graph::from_json(far).unwrap_err(),
                "Node 1 has an invalid position (coordinates must be finite numbers)"
            );

            let mut g = g.clone();
            g.connect(a, b).unwrap();
            assert_eq!(g.connect(a, b).unwrap_err(), "Already connected");
            assert_eq!(g.connect(b, b).unwrap_err(), "A node cannot be connected to itself");
            assert_eq!(g.connect(b, a).unwrap_err(), "The Text node has no input port");
            assert_eq!(g.connect(c, b).unwrap_err(), "The Output node has no output port");
            assert_eq!(g.connect(a, 99).unwrap_err(), "No such node");
            let p2 = g.add(NodeKind::new_prompt(), [0.0, 0.0]);
            g.connect(b, p2).unwrap();
            assert_eq!(g.connect(p2, b).unwrap_err(), "Cyclic connections are not allowed");
            assert_eq!(g.duplicate_node(999).unwrap_err(), "No such node");

            let mut full = Graph::default();
            full.nodes.push(Node {
                id: u32::MAX,
                title: "end".into(),
                pos: [0.0, 0.0],
                kind: NodeKind::new_text(),
            });
            assert_eq!(
                full.add_node(NodeKind::Output, [0.0, 0.0]).unwrap_err(),
                format!(
                    "Node ids have reached the limit ({}); no more nodes can be added",
                    u32::MAX
                )
            );
        });
    }

    #[test]
    fn new_node_defaults_follow_the_language_but_saved_text_does_not() {
        let prompt_text = |k: &NodeKind| match k {
            NodeKind::Prompt { role, constraints, .. } => (role.clone(), constraints.clone()),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            prompt_text(&with_lang(Lang::En, NodeKind::new_prompt)),
            (
                "Analyze the following:\n{{input}}".to_string(),
                "Do not infer anything that is not in the data".to_string()
            )
        );
        assert_eq!(
            prompt_text(&with_lang(Lang::Ko, NodeKind::new_prompt)),
            (
                ko_text!("Analyze the following:\n{{input}}").to_string(),
                ko_text!("Do not infer anything that is not in the data").to_string()
            )
        );
        // A node created and saved in Korean keeps its text when opened in English (user content).
        let mut g = Graph::default();
        let p = with_lang(Lang::Ko, || g.add(NodeKind::new_prompt(), [0.0, 0.0]));
        let json = g.to_json().unwrap();
        let back = with_lang(Lang::En, || Graph::from_json(&json)).unwrap();
        assert_eq!(
            prompt_text(&back.node(p).unwrap().kind).0,
            ko_text!("Analyze the following:\n{{input}}")
        );
        // The copy is named in the language in use when duplicating.
        g.nodes[0].title = "Summarizer".into();
        let copy = with_lang(Lang::En, || g.duplicate_node(p)).unwrap();
        assert_eq!(g.node(copy).unwrap().title, "Summarizer copy");
        let again = with_lang(Lang::En, || g.duplicate_node(p)).unwrap();
        assert_eq!(g.node(again).unwrap().title, "Summarizer copy 2");
    }

    #[test]
    fn positions_that_would_break_the_canvas_are_refused() {
        let graph = |pos: &str| {
            format!(r#"{{"nodes":[{{"id":1,"title":"t","pos":{pos},"kind":{{"type":"output"}}}}],"edges":[]}}"#)
        };
        assert!(Graph::from_json(&graph("[3e38,3e38]")).is_err());
        assert!(Graph::from_json(&graph("[0.0,-2e12]")).is_err());
        assert!(Graph::from_json(&graph("[3e9,-3e9]")).is_ok());
    }
}
