//! 캔버스 그래프 데이터 모델. JSON(`.canvas.json`)으로 저장/로드한다.
//! API 키는 저장하지 않고 환경 변수 이름만 저장한다.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// 자동 정렬 간격: 층(왼쪽→오른쪽) 사이의 가로 간격과 같은 층 안의 세로 간격.
pub const LAYOUT_COL: f32 = 280.0;
pub const LAYOUT_ROW: f32 = 100.0;
/// 복제한 노드를 원본에서 띄우는 거리.
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

    /// 제공자가 받는 temperature 상한. Anthropic은 0~1, 나머지는 0~2다.
    pub fn temperature_max(self) -> f32 {
        match self {
            Provider::Anthropic => 1.0,
            Provider::OpenAi | Provider::Gemini => 2.0,
        }
    }

    /// 제공자가 받는 범위로 temperature를 맞춘다.
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

/// 노드 종류별 설정. `type` 필드로 구분한다.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// 고정 텍스트 입력
    Text { text: String },
    /// 프롬프트 컴파일러(린트 + 모델별 직렬화). 지시문 안의 `{{input}}`은 업스트림 출력으로 치환된다.
    /// `data_path`가 상대 경로면 그래프 파일이 있는 폴더 기준이다.
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

/// 저장되는 프롬프트 테스트 케이스. 실행 결과는 저장하지 않는다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: String,
    /// 비어 있지 않으면 최종 출력에 이 문자열이 포함되어야 통과
    #[serde(default)]
    pub expect: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// 프롬프트 테스트 케이스. 이 항목이 없는 옛 파일도 열린다.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestSpec>,
    /// 테스트 입력을 넣을 Text 노드 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_node: Option<u32>,
}

impl Graph {
    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// 다음 노드 id. 가장 큰 id가 u32 한계면 오류를 돌려준다(덧셈이 넘치지 않게 검사한다).
    fn next_id(&self) -> Result<u32, String> {
        match self.nodes.iter().map(|n| n.id).max() {
            None => Ok(1),
            Some(m) => m
                .checked_add(1)
                .ok_or_else(|| format!("노드 id가 한계({})에 도달해 더 추가할 수 없습니다", u32::MAX)),
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
        self.add_node(kind, pos).expect("테스트용 노드 추가")
    }

    /// 노드를 복제한다: 새 id, 원본 옆으로 띄운 위치, 같은 설정. 연결은 복사하지 않는다.
    pub fn duplicate_node(&mut self, id: u32) -> Result<u32, String> {
        let src = self.node(id).ok_or_else(|| "존재하지 않는 노드".to_string())?;
        let (kind, title, pos) = (src.kind.clone(), src.title.clone(), src.pos);
        let new_id = self.next_id()?;
        let title = self.unique_title(&format!("{title} 복사"));
        let pos = [pos[0] + DUPLICATE_OFFSET[0], pos[1] + DUPLICATE_OFFSET[1]];
        self.nodes.push(Node {
            id: new_id,
            title,
            pos,
            kind,
        });
        Ok(new_id)
    }

    /// 같은 이름의 노드가 있으면 뒤에 번호를 붙여 겹치지 않는 이름을 만든다.
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
    /// 간선 수가 많아도 느려지지 않게 나가는 간선을 미리 모아 둔다(O(노드+간선)).
    pub fn topo_order(&self) -> Result<Vec<u32>, String> {
        let ids: HashSet<u32> = self.nodes.iter().map(|n| n.id).collect();
        let mut indeg: HashMap<u32, usize> = self.nodes.iter().map(|n| (n.id, 0)).collect();
        let mut outgoing: HashMap<u32, Vec<u32>> = HashMap::new();
        for e in &self.edges {
            if !ids.contains(&e.from) || !ids.contains(&e.to) {
                return Err(format!("존재하지 않는 노드를 가리키는 간선 {}->{}", e.from, e.to));
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
            Err("그래프에 순환이 있습니다".into())
        }
    }

    /// 왼쪽에서 오른쪽으로 층을 나눠 노드를 다시 배치한다.
    /// 층은 가장 긴 업스트림 경로의 길이이고, 같은 층 안에서는 업스트림 노드의 평균 높이에 맞춰
    /// 위아래 순서를 정한다(겹치면 아래로 민다). 위상 정렬이 되지 않는 그래프(순환)는 Err.
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
            // 목표 높이: 업스트림 노드 높이의 평균. 첫 층은 목록 순서대로 쌓는다.
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
            // 안정 정렬이라 목표가 같거나 없으면 목록 순서가 유지된다.
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

    /// 구조 검사: 중복 노드 id, 없는 노드를 가리키는 간선, 포트 규칙 위반(Output에서 나가는 간선,
    /// Text로 들어오는 간선), 자기 연결, 중복 간선, 순환.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        if let Some(n) = self.nodes.iter().find(|n| !seen.insert(n.id)) {
            return Err(format!("중복된 노드 id {}", n.id));
        }
        let index: HashMap<u32, &Node> = self.nodes.iter().map(|n| (n.id, n)).collect();
        if let Some(n) = self.nodes.iter().find(|n| !n.pos.iter().all(|v| v.is_finite())) {
            return Err(format!(
                "노드 {}의 위치 값이 올바르지 않습니다 (유한한 숫자여야 합니다)",
                n.id
            ));
        }
        let mut edges = HashSet::new();
        for e in &self.edges {
            let (Some(a), Some(b)) = (index.get(&e.from), index.get(&e.to)) else {
                return Err(format!("존재하지 않는 노드를 가리키는 간선 {}->{}", e.from, e.to));
            };
            if e.from == e.to {
                return Err(format!("자기 자신에 연결된 간선 {}->{}", e.from, e.to));
            }
            if !a.kind.has_output() {
                return Err(format!(
                    "{} 노드(id {})에는 출력 포트가 없어 간선 {}->{}을 둘 수 없습니다",
                    a.kind.label(),
                    a.id,
                    e.from,
                    e.to
                ));
            }
            if !b.kind.has_input() {
                return Err(format!(
                    "{} 노드(id {})에는 입력 포트가 없어 간선 {}->{}을 둘 수 없습니다",
                    b.kind.label(),
                    b.id,
                    e.from,
                    e.to
                ));
            }
            if !edges.insert((e.from, e.to)) {
                return Err(format!("중복된 간선 {}->{}", e.from, e.to));
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
            serde_json::from_str(s.trim_start_matches('\u{feff}')).map_err(|e| format!("그래프 JSON 오류: {e}"))?;
        g.validate()?;
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> (Graph, u32, u32, u32) {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_text(), [0.0, 0.0]);
        let b = g.add(NodeKind::new_prompt(), [0.0, 0.0]);
        let c = g.add(NodeKind::Output, [0.0, 0.0]);
        (g, a, b, c)
    }

    /// 검사 없이 만든(저장 가능한) 그래프를 JSON으로 거쳐 로드해 본다.
    fn load(g: &Graph) -> Result<Graph, String> {
        Graph::from_json(&g.to_json().unwrap())
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
        assert!(err.contains("중복된 간선"), "{err}");
        let err = with_edges(&[(c, b)]).unwrap_err();
        assert!(err.contains("Output") && err.contains("출력 포트"), "{err}");
        let err = with_edges(&[(b, a)]).unwrap_err();
        assert!(err.contains("Text") && err.contains("입력 포트"), "{err}");
        let err = with_edges(&[(b, b)]).unwrap_err();
        assert!(err.contains("자기 자신"), "{err}");
        let err = with_edges(&[(a, 99)]).unwrap_err();
        assert!(err.contains("존재하지 않는"), "{err}");
        let mut g2 = g.clone();
        let extra = g2.add(NodeKind::new_prompt(), [0.0, 0.0]);
        g2.edges = vec![Edge { from: b, to: extra }, Edge { from: extra, to: b }];
        assert!(load(&g2).unwrap_err().contains("순환"));
        let mut dup = g.clone();
        dup.nodes[1].id = dup.nodes[0].id;
        assert!(load(&dup).unwrap_err().contains("중복된 노드 id"));
    }

    #[test]
    fn id_overflow_is_an_error_not_a_panic() {
        let mut g = Graph::default();
        g.nodes.push(Node {
            id: u32::MAX,
            title: "끝".into(),
            pos: [0.0, 0.0],
            kind: NodeKind::new_text(),
        });
        let mut g = load(&g).expect("id가 u32::MAX여도 로드는 된다");
        assert!(g.add_node(NodeKind::Output, [0.0, 0.0]).unwrap_err().contains("한계"));
        assert!(g.duplicate_node(u32::MAX).is_err());
        assert_eq!(g.nodes.len(), 1, "실패하면 아무것도 추가되지 않는다");

        let mut near = Graph::default();
        near.nodes.push(Node {
            id: u32::MAX - 1,
            title: "직전".into(),
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
        g.nodes[1].title = "요약기".into();
        let copy = g.duplicate_node(b).unwrap();
        assert!(copy > c, "새 id");
        assert_eq!(g.edges.len(), 2, "연결은 복사하지 않는다");
        assert!(g.upstream(copy).is_empty() && !g.edges.iter().any(|e| e.from == copy));
        let (orig, dup) = (g.node(b).unwrap(), g.node(copy).unwrap());
        assert_eq!(dup.pos, [100.0 + DUPLICATE_OFFSET[0], 50.0 + DUPLICATE_OFFSET[1]]);
        assert_eq!(dup.title, "요약기 복사");
        assert_eq!(format!("{:?}", orig.kind), format!("{:?}", dup.kind));
        // 또 복제하면 이름이 겹치지 않는다.
        let again = g.duplicate_node(b).unwrap();
        assert_eq!(g.node(again).unwrap().title, "요약기 복사 2");
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
            assert_eq!(pos_of(&g, *id), [i as f32 * LAYOUT_COL, 0.0], "노드 {id}");
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
        assert_eq!(pos_of(&cyc, x), [1.0, 2.0], "실패하면 위치를 건드리지 않는다");
    }

    /// 무작위 DAG에서도 간선은 항상 오른쪽으로 향하고 노드끼리 겹치지 않는다.
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
                assert!(pos_of(&g, e.to)[0] > pos_of(&g, e.from)[0], "라운드 {round}: {e:?}");
            }
            for (i, a) in g.nodes.iter().enumerate() {
                for b in &g.nodes[i + 1..] {
                    let apart = (a.pos[0] - b.pos[0]).abs() >= LAYOUT_COL || (a.pos[1] - b.pos[1]).abs() >= LAYOUT_ROW;
                    assert!(apart, "라운드 {round}: {} 와 {} 가 겹침", a.id, b.id);
                }
            }
        }
    }

    #[test]
    fn old_files_without_tests_still_load() {
        let old = r#"{"nodes":[{"id":1,"title":"in","pos":[0,0],"kind":{"type":"text","text":"x"}}],"edges":[]}"#;
        let g = Graph::from_json(old).unwrap();
        assert!(g.tests.is_empty() && g.test_node.is_none());
        // 테스트가 없으면 파일에도 적지 않아 옛 형식 그대로 저장된다.
        let json = g.to_json().unwrap();
        assert!(!json.contains("tests") && !json.contains("test_node"), "{json}");
    }

    #[test]
    fn saved_tests_roundtrip() {
        let (mut g, a, _, _) = chain();
        g.test_node = Some(a);
        g.tests = vec![
            TestSpec {
                name: "기본".into(),
                input: "줄1\n줄2 <b> & \"따옴표\"".into(),
                expect: "dry-run".into(),
            },
            TestSpec {
                name: "비어 있음".into(),
                ..TestSpec::default()
            },
        ];
        let back = load(&g).unwrap();
        assert_eq!(back.tests, g.tests);
        assert_eq!(back.test_node, Some(a));
        // 일부 항목만 있는 케이스도 열린다.
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
                "{} -> {} 가로 간격이 좁음",
                a.title,
                b.title
            );
        }
        for (i, a) in g.nodes.iter().enumerate() {
            for b in &g.nodes[i + 1..] {
                assert_ne!(a.pos, b.pos, "{} 와 {} 의 위치가 같음", a.title, b.title);
            }
        }
    }

    #[test]
    fn large_graphs_load_quickly() {
        // 간선 하나마다 노드 목록을 훑는 O(N^2) 처리라면 수 초가 걸리는 크기다.
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
        assert!(t.elapsed().as_secs_f32() < 3.0, "로드 {:?}", t.elapsed());
        assert_eq!(back.topo_order().unwrap().len(), 20_000);
    }

    #[test]
    fn non_finite_positions_are_rejected() {
        let one = |pos: &str| {
            format!(r#"{{"nodes":[{{"id":1,"title":"t","pos":{pos},"kind":{{"type":"output"}}}}],"edges":[]}}"#)
        };
        assert!(Graph::from_json(&one("[1.5, -2]")).is_ok());
        // f32 범위를 넘는 숫자는 무한대가 된다. 캔버스가 그릴 수 없으므로 읽을 때 거절한다.
        let err = Graph::from_json(&one("[1e39, 0]")).unwrap_err();
        assert!(err.contains("위치 값"), "{err}");
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
}
