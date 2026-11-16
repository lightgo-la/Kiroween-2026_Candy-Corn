//! 캔버스 그래프 데이터 모델. JSON(`.canvas.json`)으로 저장/로드한다.
//! API 키는 저장하지 않고 환경 변수 이름만 저장한다.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

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
            Provider::Anthropic => "claude-sonnet-4-5",
            Provider::OpenAi => "gpt-4o",
            Provider::Gemini => "gemini-2.5-flash",
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

/// 노드 종류별 설정. `type` 필드로 구분한다.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// 고정 텍스트 입력
    Text { text: String },
    /// 프롬프트 컴파일러(린트 + 모델별 직렬화). 지시문 안의 `{{input}}`은 업스트림 출력으로 치환된다.
    Prompt {
        role: String,
        constraints: String,
        fields: String,
        target: PromptTarget,
        format: PromptFormat,
        #[serde(default)]
        data_path: String,
    },
    /// LLM API 호출
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
        /// true면 네트워크 호출 없이 요청 요약만 반환한다(비용 방지).
        #[serde(default = "yes")]
        dry_run: bool,
    },
    /// MCP 서버(stdio) 도구 호출. `arguments`는 JSON 템플릿이며 `{{input}}`이 치환된다.
    Mcp {
        command: String,
        #[serde(default)]
        args: String,
        tool: String,
        arguments: String,
    },
    /// 최종 결과 수집
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

    pub fn new_prompt() -> Self {
        NodeKind::Prompt {
            role: "다음 내용을 분석해줘:\n{{input}}".into(),
            constraints: "데이터에 없는 내용은 추론하지 마".into(),
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn add_node(&mut self, kind: NodeKind, pos: [f32; 2]) -> u32 {
        let id = self.nodes.iter().map(|n| n.id).max().map_or(1, |m| m + 1);
        let title = format!("{} {}", kind.label(), id);
        self.nodes.push(Node { id, title, pos, kind });
        id
    }

    pub fn remove_node(&mut self, id: u32) {
        self.nodes.retain(|n| n.id != id);
        self.edges.retain(|e| e.from != id && e.to != id);
    }

    /// 간선을 추가한다. 포트가 없거나, 중복이거나, 순환을 만들면 사유를 반환한다.
    pub fn connect(&mut self, from: u32, to: u32) -> Result<(), String> {
        if from == to {
            return Err("자기 자신에는 연결할 수 없습니다".into());
        }
        let (Some(a), Some(b)) = (self.node(from), self.node(to)) else {
            return Err("존재하지 않는 노드".into());
        };
        if !a.kind.has_output() {
            return Err(format!("{} 노드에는 출력 포트가 없습니다", a.kind.label()));
        }
        if !b.kind.has_input() {
            return Err(format!("{} 노드에는 입력 포트가 없습니다", b.kind.label()));
        }
        let e = Edge { from, to };
        if self.edges.contains(&e) {
            return Err("이미 연결되어 있습니다".into());
        }
        self.edges.push(e);
        if self.topo_order().is_err() {
            self.edges.pop();
            return Err("순환 연결은 허용되지 않습니다".into());
        }
        Ok(())
    }

    /// 입력 간선 순서(추가된 순서)대로 업스트림 노드 id.
    pub fn upstream(&self, id: u32) -> Vec<u32> {
        self.edges.iter().filter(|e| e.to == id).map(|e| e.from).collect()
    }

    /// Kahn 알고리즘. 순환이면 Err. 같은 단계에서는 노드 목록 순서를 유지한다.
    pub fn topo_order(&self) -> Result<Vec<u32>, String> {
        let ids: HashSet<u32> = self.nodes.iter().map(|n| n.id).collect();
        let mut indeg: HashMap<u32, usize> = self.nodes.iter().map(|n| (n.id, 0)).collect();
        for e in &self.edges {
            if !ids.contains(&e.from) || !ids.contains(&e.to) {
                return Err(format!("존재하지 않는 노드를 가리키는 간선 {}->{}", e.from, e.to));
            }
            *indeg.get_mut(&e.to).expect("checked") += 1;
        }
        let mut queue: VecDeque<u32> = self.nodes.iter().filter(|n| indeg[&n.id] == 0).map(|n| n.id).collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id);
            for e in self.edges.iter().filter(|e| e.from == id) {
                let d = indeg.get_mut(&e.to).expect("checked");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(e.to);
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            Err("그래프에 순환이 있습니다".into())
        }
    }

    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn from_json(s: &str) -> Result<Graph, String> {
        let g: Graph =
            serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|e| format!("그래프 JSON 오류: {e}"))?;
        let mut seen = HashSet::new();
        if let Some(n) = g.nodes.iter().find(|n| !seen.insert(n.id)) {
            return Err(format!("중복된 노드 id {}", n.id));
        }
        g.topo_order()?;
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> (Graph, u32, u32, u32) {
        let mut g = Graph::default();
        let a = g.add_node(NodeKind::new_text(), [0.0, 0.0]);
        let b = g.add_node(NodeKind::new_prompt(), [0.0, 0.0]);
        let c = g.add_node(NodeKind::Output, [0.0, 0.0]);
        (g, a, b, c)
    }

    #[test]
    fn connect_rules() {
        let (mut g, a, b, c) = chain();
        assert!(g.connect(a, b).is_ok());
        assert!(g.connect(a, b).is_err(), "중복");
        assert!(g.connect(b, b).is_err(), "자기 연결");
        assert!(g.connect(b, a).is_err(), "Text는 입력 포트 없음");
        assert!(g.connect(c, b).is_err(), "Output은 출력 포트 없음");
        assert!(g.connect(b, c).is_ok());
        assert_eq!(g.topo_order().unwrap(), vec![a, b, c]);
    }

    #[test]
    fn rejects_cycle() {
        let mut g = Graph::default();
        let a = g.add_node(NodeKind::new_prompt(), [0.0, 0.0]);
        let b = g.add_node(NodeKind::new_prompt(), [0.0, 0.0]);
        g.connect(a, b).unwrap();
        assert!(g.connect(b, a).is_err());
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn json_roundtrip_and_validation() {
        let (mut g, a, b, _) = chain();
        g.connect(a, b).unwrap();
        let g2 = Graph::from_json(&g.to_json().unwrap()).unwrap();
        assert_eq!(g2.nodes.len(), 3);
        assert_eq!(g2.edges, g.edges);
        assert!(Graph::from_json(r#"{"nodes":[],"edges":[{"from":1,"to":2}]}"#).is_err());
    }

    #[test]
    fn remove_node_drops_edges() {
        let (mut g, a, b, c) = chain();
        g.connect(a, b).unwrap();
        g.connect(b, c).unwrap();
        g.remove_node(b);
        assert!(g.edges.is_empty());
    }
}
