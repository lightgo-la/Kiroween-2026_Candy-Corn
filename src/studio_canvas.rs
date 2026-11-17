//! 블루프린트 캔버스: 설정·데이터·역할·제약·출력 블록을 컴파일러 노드에 연결하고 결과·LLM 실행까지 한 화면에서 구성한다.
//! 캔버스의 모든 노드 값이 곧 블루프린트(.kiro) 파일의 내용이다.
//! 이동(드래그)·확대(Ctrl+휠, 핀치)는 egui::Scene이 처리한다.

use crate::ast::{ConstraintNode, ConstraintType, LintDiagnostic, OutputFormat, OutputSchemaAst, TargetModel};
use crate::blueprint::{self, LlmOptions, MAX_SAMPLE_ROWS, PRESETS};
use crate::pipeline::{self, CompileResult, LoadedData};
use crate::theme::{self, Palette, THEMES};
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Shape,
    Stroke, Ui, UiBuilder, epaint::CubicBezierShape, epaint::Shadow,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeKey {
    Data,
    Role,
    Output,
    Root,
    Result,
    Llm,
    Settings,
    Goal(u64),
}

impl NodeKey {
    pub fn fixed_name(self) -> Option<&'static str> {
        match self {
            NodeKey::Data => Some("data"),
            NodeKey::Role => Some("role"),
            NodeKey::Output => Some("output"),
            NodeKey::Root => Some("root"),
            NodeKey::Result => Some("result"),
            NodeKey::Llm => Some("llm"),
            NodeKey::Settings => Some("settings"),
            NodeKey::Goal(_) => None,
        }
    }

    /// 블루프린트 `positions`의 이름으로 고정 노드를 찾는다.
    pub fn from_fixed_name(name: &str) -> Option<NodeKey> {
        NodeKey::FIXED.into_iter().find(|k| k.fixed_name() == Some(name))
    }

    pub const FIXED: [NodeKey; 7] = [
        NodeKey::Data,
        NodeKey::Role,
        NodeKey::Output,
        NodeKey::Root,
        NodeKey::Result,
        NodeKey::Llm,
        NodeKey::Settings,
    ];
}

/// 제약 한 줄. 캔버스 노드와 왼쪽 폼이 같은 목록을 편집한다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: u64,
    pub text: String,
    pub on: bool,
}

pub struct Model<'a> {
    pub role: &'a mut String,
    pub goals: &'a mut Vec<Goal>,
    pub format: &'a mut OutputFormat,
    pub fields_text: &'a mut String,
    pub target: &'a mut TargetModel,
    pub loaded: Option<&'a LoadedData>,
    pub result: Option<&'a CompileResult>,
    pub loading: bool,
    pub compiling: bool,
    pub stale: bool,
    pub project: &'a mut String,
    /// 마지막으로 열거나 저장한 블루프린트와 다른지 (이름 옆 ● 표시)
    pub dirty: bool,
    pub theme_idx: &'a mut usize,
    /// 안개 테마의 움직이는 안개 사용 여부
    pub fog: &'a mut bool,
    /// 현재 테마가 안개 테마인지 (아니면 안개 효과 설정을 끈 상태로 보여 준다)
    pub fog_theme: bool,
    pub sample_rows: &'a mut usize,
    pub llm: LlmView<'a>,
}

/// LLM 실행 노드에 보여 줄 상태.
pub struct LlmView<'a> {
    pub provider: &'static str,
    pub key_env: &'static str,
    pub model: &'a mut String,
    pub model_hint: &'static str,
    pub has_key: bool,
    pub running: bool,
    /// (통과 여부, 한 줄 요약)
    pub verdict: Option<(bool, String)>,
    /// 출력 한도·온도·시스템 프롬프트 (오른쪽 VIII과 같은 값)
    pub opts: &'a mut LlmOptions,
    /// 현재 제공자의 온도 상한
    pub temp_max: f32,
    /// 지난 실행의 응답 시간·토큰 사용량 한 줄
    pub details: Option<String>,
    /// 지난 응답이 출력 한도에서 잘렸으면 두 배로 늘린 한도 (다시 실행 버튼)
    pub retry_limit: Option<u32>,
}

/// 캔버스가 앱에 요청하는 동작(소유권 충돌을 피하려고 반환값으로 전달한다).
pub enum Action {
    PickFile,
    Compile,
    RemoveGoal(u64),
    AddGoal(String),
    /// 내장 블루프린트 적용 (blueprint::PRESETS 번호)
    Preset(usize),
    /// 다른 이름으로 저장 (항상 위치를 묻는다)
    SaveProject,
    /// 지금 블루프린트 파일에 저장 (Ctrl+S, 파일이 없으면 위치를 묻는다)
    QuickSave,
    LoadProject,
    SaveOutput,
    RunLlm,
    /// 진행 중인 LLM 실행을 버린다
    CancelLlm,
    /// 출력 한도를 이 값으로 늘려 다시 실행
    RetryLonger(u32),
    /// 샘플 행 수를 바꿨으니 데이터를 다시 프로파일링
    ReloadSample,
}

pub const TEMPLATES: [(&str, &[&str]); 5] = [
    ("결측치", &["결측치 0.2% 제외", "결측치 비율을 컬럼별로 보고"]),
    ("이상치", &["이상치 우선 탐지", "이상치는 z-score 3 기준으로 판단"]),
    (
        "사실성",
        &["데이터에 없는 내용은 추론하지 마", "근거가 되는 컬럼 이름을 함께 표기"],
    ),
    ("형식", &["요약은 3문장 이내", "숫자는 소수점 둘째 자리까지"]),
    ("분류", &["금액 4M 기준으로 분류", "분류할 수 없으면 unknown으로 표기"]),
];

/// 노드 사이 세로 간격
const GAP: f32 = 20.0;
const LEFT_X: f32 = 0.0;
const GOAL_X: f32 = 290.0;
const ROOT_X: f32 = 590.0;
const LLM_X: f32 = 900.0;
/// 격자 점 간격(확대가 작으면 두 배씩 늘린다)과 한 화면의 최대 점 수
const GRID_STEP: f32 = 40.0;
const MAX_GRID_DOTS: i64 = 6000;
/// 흐름 애니메이션 프레임 간격 (최대 20fps)
pub const FLOW_FRAME: Duration = Duration::from_millis(50);

/// 노드 높이 추정치. 지난 프레임에 잰 높이를 모를 때(처음 그리기 전) 쓴다.
fn default_height(key: NodeKey) -> f32 {
    match key {
        NodeKey::Settings => 150.0,
        NodeKey::Data => 210.0,
        NodeKey::Role => 160.0,
        NodeKey::Output => 150.0,
        NodeKey::Goal(_) => 92.0,
        NodeKey::Root => 170.0,
        NodeKey::Result => 300.0,
        NodeKey::Llm => 260.0,
    }
}

/// 제약 노드 높이 추정. 긴 문장은 입력칸 안에서 줄이 바뀌어 노드가 길어진다
/// (한 줄에 전각 약 17자, ASCII는 절반 폭으로 친다).
pub fn estimate_goal_height(text: &str) -> f32 {
    const PER_LINE: f32 = 17.0;
    const LINE_H: f32 = 17.0;
    let lines: usize = text
        .split('\n')
        .map(|l| {
            let w: f32 = l.chars().map(|c| if c.is_ascii() { 0.55 } else { 1.0 }).sum();
            ((w / PER_LINE).ceil() as usize).max(1)
        })
        .sum();
    default_height(NodeKey::Goal(0)) + lines.saturating_sub(1) as f32 * LINE_H
}

/// 기본 배치. 왼쪽 열(설정·데이터·역할·출력)과 제약 열은 노드 높이만큼 쌓고,
/// 컴파일러·결과·LLM 노드는 제약 열 가운데 높이에 둔다.
/// height는 노드 높이(지난 프레임에 잰 값이나 추정치)를 돌려주고, None이면 기본 추정치를 쓴다.
pub fn layout_positions(goal_ids: &[u64], height: impl Fn(NodeKey) -> Option<f32>) -> Vec<(NodeKey, Pos2)> {
    let h = |k: NodeKey| {
        height(k)
            .filter(|h| h.is_finite() && *h > 0.0)
            .unwrap_or_else(|| default_height(k))
    };
    let mut out = Vec::with_capacity(goal_ids.len() + NodeKey::FIXED.len());
    let mut y = 0.0;
    for k in [NodeKey::Settings, NodeKey::Data, NodeKey::Role, NodeKey::Output] {
        out.push((k, Pos2::new(LEFT_X, y)));
        y += h(k) + GAP;
    }
    let mut gy = 0.0;
    for &id in goal_ids {
        let k = NodeKey::Goal(id);
        out.push((k, Pos2::new(GOAL_X, gy)));
        gy += h(k) + GAP;
    }
    let mid = (gy / 2.0 - 110.0).max(120.0);
    let below_root = mid + h(NodeKey::Root) + GAP;
    out.push((NodeKey::Root, Pos2::new(ROOT_X, mid)));
    out.push((NodeKey::Result, Pos2::new(ROOT_X, below_root)));
    out.push((NodeKey::Llm, Pos2::new(LLM_X, below_root)));
    out
}

/// 새 제약 노드의 자리: 가장 아래에 있는 제약 노드 바로 밑(같은 x). 제약이 없으면 제약 열 맨 위.
/// nodes는 기존 제약 노드의 (위치, 높이).
pub fn place_below(nodes: impl IntoIterator<Item = (Pos2, f32)>) -> Pos2 {
    nodes
        .into_iter()
        .max_by(|a, b| (a.0.y + a.1).total_cmp(&(b.0.y + b.1)))
        .map_or(Pos2::new(GOAL_X, 0.0), |(p, h)| Pos2::new(p.x, p.y + h + GAP))
}

/// 좌표를 허용 범위(blueprint::POS_LIMIT) 안으로 자른다. NaN·무한대면 None.
fn clamp_pos(p: Pos2) -> Option<Pos2> {
    blueprint::sanitize_pos([p.x, p.y]).map(|[x, y]| Pos2::new(x, y))
}

/// 격자 점의 (간격, x칸 번호 범위, y칸 번호 범위). 점이 max_dots를 넘지 않게 간격을 두 배씩 늘린다.
/// 좌표가 아무리 커도 반드시 끝나도록 정수 칸 번호로 센다 (f32 누적 덧셈은 큰 좌표에서 더 나아가지 않는다).
/// 보이는 영역이 비정상(NaN·무한대·빈 영역)이면 None.
fn grid_cells(vis: Rect, base: f32, max_dots: i64) -> Option<(f32, Range<i64>, Range<i64>)> {
    if !vis.is_finite() || vis.width() <= 0.0 || vis.height() <= 0.0 || !base.is_finite() || base <= 0.0 {
        return None;
    }
    let mut step = base as f64;
    for _ in 0..64 {
        let span = |a: f32, b: f32| (a as f64 / step).floor() as i64..(b as f64 / step).ceil() as i64;
        let (xs, ys) = (span(vis.left(), vis.right()), span(vis.top(), vis.bottom()));
        if (xs.end - xs.start).saturating_mul(ys.end - ys.start) <= max_dots {
            return Some((step as f32, xs, ys));
        }
        step *= 2.0;
    }
    None
}

/// 흐름 애니메이션용 다시 그리기 간격. 컴파일·로드·LLM 실행 중이거나 포인터가 캔버스 위에 있을 때만
/// 최대 20fps로 움직이고, 그 밖에는 멈춘 화면을 그대로 두어 유휴 CPU를 쓰지 않는다.
pub fn flow_repaint(animate: bool, busy: bool, pointer_over: bool, focused: bool) -> Option<Duration> {
    (animate && focused && (busy || pointer_over)).then_some(FLOW_FRAME)
}

/// 실행 옵션 접기 제목에 붙는 요약: "2048 토큰 · 온도 기본값 · 시스템". 온도는 제공자 범위로 자른 실제 전송값이다.
pub fn options_summary(o: &LlmOptions, temp_max: f32) -> String {
    let temp = o.temperature.map_or_else(
        || "온도 기본값".to_string(),
        |t| format!("온도 {:.2}", t.clamp(0.0, temp_max)),
    );
    let system = if o.system.trim().is_empty() {
        ""
    } else {
        " · 시스템"
    };
    format!("{} 토큰 · {temp}{system}", o.max_tokens)
}

/// 이보다 큰 출력 한도는 응답이 길어져 LLM 호출 제한 시간에 걸릴 수 있어 안내한다.
const LONG_REPLY_TOKENS: u32 = 8192;

/// LLM 실행 옵션 입력. 캔버스 LLM 노드와 오른쪽 VIII이 같은 값을 편집한다 (salt로 입력칸 id를 나눈다).
pub fn llm_options_ui(ui: &mut Ui, o: &mut LlmOptions, temp_max: f32, salt: &str, p: &Palette) {
    ui.horizontal(|ui| {
        ui.label("출력 한도");
        ui.add(
            egui::DragValue::new(&mut o.max_tokens)
                .range(blueprint::MIN_LLM_MAX_TOKENS..=blueprint::MAX_LLM_MAX_TOKENS)
                .speed(16.0)
                .suffix(" 토큰"),
        )
        .on_hover_text(format!(
            "응답이 이 토큰 수에서 잘립니다 ({}~{})",
            blueprint::MIN_LLM_MAX_TOKENS,
            blueprint::MAX_LLM_MAX_TOKENS
        ));
    });
    if o.max_tokens > LONG_REPLY_TOKENS {
        ui.label(
            RichText::new("⚠ 한도가 크면 응답이 길어져 호출 제한 시간을 넘을 수 있고, 모델마다 허용 한도가 다릅니다")
                .small()
                .color(p.warn()),
        );
    }
    ui.horizontal(|ui| {
        ui.label("온도");
        let mut default = o.temperature.is_none();
        if ui
            .checkbox(&mut default, "기본값")
            .on_hover_text("제공자 기본 온도를 씁니다 (요청에 넣지 않음)")
            .changed()
        {
            o.temperature = (!default).then_some(1.0_f32.min(temp_max));
        }
    });
    if let Some(t) = &mut o.temperature {
        // 제공자 범위로 자른 값을 보여 주고 사용자가 움직일 때만 저장값을 바꾼다
        // (GPT에서 1.5로 둔 값이 Claude로 바꿨다가 돌아와도 남아 있게).
        let mut shown = t.clamp(0.0, temp_max);
        if ui
            .add(
                egui::Slider::new(&mut shown, 0.0..=temp_max)
                    .step_by(0.05)
                    .fixed_decimals(2),
            )
            .on_hover_text(format!("0~{temp_max:.0} (제공자마다 범위가 다릅니다)"))
            .changed()
        {
            *t = shown;
        }
    }
    ui.label(RichText::new("시스템 프롬프트 (선택)").small().color(p.sub));
    ui.add(
        egui::TextEdit::multiline(&mut o.system)
            .id_salt((salt, "llm-system"))
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .hint_text("비워 두면 보내지 않습니다"),
    );
}

pub struct Canvas {
    scene_rect: Rect,
    pos: HashMap<NodeKey, Pos2>,
    /// 지난 프레임에 그린 노드 영역 (높이는 배치·새 제약 자리 계산에 쓴다)
    rects: HashMap<NodeKey, Rect>,
    pub snap: bool,
    pub animate: bool,
    selected: Option<NodeKey>,
    want_layout: bool,
    want_fit: bool,
    /// 지난 프레임의 캔버스 화면 영역(배경 클릭/Delete 판정용).
    area: Rect,
    /// 사용자가 캔버스를 직접 이동·확대했는지. 아니면 창 크기가 바뀔 때 다시 화면 맞춤을 한다.
    user_view: bool,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas {
            scene_rect: Rect::from_min_size(Pos2::new(-40.0, -40.0), egui::vec2(1300.0, 700.0)),
            pos: HashMap::new(),
            rects: HashMap::new(),
            snap: true,
            animate: true,
            selected: None,
            want_layout: true,
            want_fit: true,
            area: Rect::NOTHING,
            user_view: false,
        }
    }
}

fn goal_color(kind: &ConstraintType) -> Color32 {
    match kind {
        ConstraintType::NegativeRule => theme::ERR,
        ConstraintType::BoundaryRule => Color32::from_rgb(0x6c, 0xb4, 0xff),
        ConstraintType::FactualityRule => theme::OK,
    }
}

/// 린트 진단이 컴파일 요청 안 위치 `i`의 제약을 가리키는지. 진단에 걸린 모든 입력 위치(제거는 지운 것·남긴 것,
/// 모순 의심은 두 제약)로만 찾는다. 경고 문구는 수식어를 지운 본문을 인용하므로 원문 비교로는 찾을 수 없다.
fn lint_mentions(diags: &[LintDiagnostic], i: usize) -> bool {
    diags
        .iter()
        .filter(|d| d.severity != "INFO")
        .any(|d| d.indices.contains(&i))
}

const ROLE_COLOR: Color32 = Color32::from_rgb(0xc0, 0x8c, 0xff);
const DATA_COLOR: Color32 = Color32::from_rgb(0x4f, 0xd1, 0xc5);
const OUT_COLOR: Color32 = Color32::from_rgb(0xf2, 0xd4, 0x6b);
const RESULT_COLOR: Color32 = Color32::from_rgb(0xff, 0x8f, 0xb8);
const LLM_COLOR: Color32 = Color32::from_rgb(0x9a, 0xe6, 0x6e);
const SETTINGS_COLOR: Color32 = Color32::from_rgb(0xc8, 0xb8, 0xa6);

impl Canvas {
    pub fn pos_of(&self, key: NodeKey) -> Option<Pos2> {
        self.pos.get(&key).copied()
    }

    /// 노드 위치를 정한다. 비정상 좌표(NaN·무한대)는 무시하고 먼 좌표는 허용 범위로 자른다.
    /// 자동 배치 여부는 바꾸지 않으므로 블루프린트 위치는 reset_layout(또는 load_layout) 뒤에 덮어쓴다.
    pub fn set_pos(&mut self, key: NodeKey, p: Pos2) {
        if let Some(p) = clamp_pos(p) {
            self.pos.insert(key, p);
        }
    }

    pub fn request_fit(&mut self) {
        self.want_fit = true;
        self.user_view = false;
    }

    /// 노드 배치를 기본값으로 되돌린다(스냅·애니메이션 설정은 유지). 기본 배치를 바로 계산하므로
    /// 이어서 set_pos로 덮은 위치는 그대로 남고, 위치가 없는 노드가 원점에 쌓이지 않는다.
    pub fn reset_layout(&mut self, goals: &[Goal]) {
        self.pos.clear();
        self.rects.clear();
        self.selected = None;
        self.layout(goals);
        self.want_layout = false;
        self.want_fit = true;
        self.user_view = false;
    }

    /// 블루프린트를 적용할 때: 기본 배치를 먼저 계산하고 저장된 위치를 그 위에 덮는다.
    /// 위치가 저장되지 않은 노드(v1 파일의 설정·데이터·LLM 노드 등)는 기본 자리에 남는다.
    pub fn load_layout(&mut self, goals: &[Goal], saved: impl IntoIterator<Item = (NodeKey, [f32; 2])>) {
        self.reset_layout(goals);
        for (k, [x, y]) in saved {
            self.set_pos(k, Pos2::new(x, y));
        }
    }

    /// 삭제된 노드의 위치·선택 상태를 정리한다.
    /// 위치(pos)는 남겨 두어 실행 취소로 되살리면 같은 자리에 나타난다.
    pub fn forget(&mut self, key: NodeKey) {
        self.rects.remove(&key);
        if self.selected == Some(key) {
            self.selected = None;
        }
    }

    /// 지난 프레임에 잰 노드 높이
    fn measured_height(&self, key: NodeKey) -> Option<f32> {
        self.rects
            .get(&key)
            .map(Rect::height)
            .filter(|h| h.is_finite() && *h > 0.0)
    }

    /// 제약 노드 높이: 지난 프레임에 잰 값, 없으면 문장 길이로 추정한 값.
    fn goal_height(&self, g: &Goal) -> f32 {
        self.measured_height(NodeKey::Goal(g.id))
            .unwrap_or_else(|| estimate_goal_height(&g.text))
    }

    /// 기본 배치. 지난 프레임에 잰 노드 높이를 쓰므로 줄이 바뀐 긴 제약도 겹치지 않는다.
    fn layout(&mut self, goals: &[Goal]) {
        let ids: Vec<u64> = goals.iter().map(|g| g.id).collect();
        let placed = layout_positions(&ids, |k| match k {
            NodeKey::Goal(id) => goals.iter().find(|g| g.id == id).map(|g| self.goal_height(g)),
            _ => self.measured_height(k),
        });
        self.pos.extend(placed);
    }

    /// 위치가 없는 노드에 자리를 준다: 고정 노드는 기본 배치, 새 제약은 가장 아래 제약 노드 밑.
    /// 앱이 매 프레임 호출하므로(캔버스를 보지 않을 때도) 저장할 블루프린트에 늘 위치가 들어간다.
    pub fn ensure_positions(&mut self, goals: &[Goal]) {
        if NodeKey::FIXED.iter().any(|k| !self.pos.contains_key(k)) {
            let ids: Vec<u64> = goals.iter().map(|g| g.id).collect();
            let defaults = layout_positions(&ids, |k| self.measured_height(k));
            for (k, p) in defaults {
                if !matches!(k, NodeKey::Goal(_)) {
                    self.pos.entry(k).or_insert(p);
                }
            }
        }
        for g in goals {
            let key = NodeKey::Goal(g.id);
            if self.pos.contains_key(&key) {
                continue;
            }
            let p = place_below(
                goals
                    .iter()
                    .filter_map(|o| Some((*self.pos.get(&NodeKey::Goal(o.id))?, self.goal_height(o)))),
            );
            self.pos.insert(key, p);
        }
    }

    /// 위치가 없는 노드의 자리 (show()가 미리 채우므로 보통 쓰이지 않는다). 원점에 겹쳐 쌓이지 않게 한다.
    fn fallback_pos(&self, key: NodeKey) -> Pos2 {
        match key {
            NodeKey::Goal(_) => place_below(
                self.rects
                    .iter()
                    .filter(|(k, _)| matches!(k, NodeKey::Goal(_)))
                    .map(|(k, r)| (self.pos.get(k).copied().unwrap_or(r.min), r.height())),
            ),
            _ => layout_positions(&[], |_| None)
                .into_iter()
                .find(|(k, _)| *k == key)
                .map_or(Pos2::new(LEFT_X, 0.0), |(_, p)| p),
        }
    }

    fn fit(&mut self) {
        if let Some(all) = self
            .rects
            .values()
            .copied()
            .reduce(|a, b| a.union(b))
            .filter(Rect::is_finite)
        {
            self.scene_rect = all.expand(24.0);
            self.want_fit = false;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, m: &mut Model, p: &Palette) -> Vec<Action> {
        let mut actions = Vec::new();
        self.toolbar(ui, p, &mut actions);
        if self.want_layout {
            self.layout(m.goals);
            self.want_layout = false;
            self.want_fit = true;
        }
        self.ensure_positions(m.goals);
        if self.want_fit {
            self.fit();
        }
        let ctx = ui.ctx().clone();
        let pointer_in = ctx.pointer_hover_pos().is_some_and(|p| self.area.contains(p));
        // 빈 곳을 클릭하면 선택 해제(노드 제목줄 클릭은 아래 node()에서 다시 선택한다)
        if pointer_in && ctx.input(|i| i.pointer.primary_clicked()) {
            self.selected = None;
        }
        // Delete는 포인터가 캔버스 위에 있고 글자 입력 중이 아닐 때만 동작한다.
        if let Some(NodeKey::Goal(id)) = self.selected
            && pointer_in
            && !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Delete))
        {
            actions.push(Action::RemoveGoal(id));
            self.selected = None;
        }
        // 흐름 애니메이션: 작업 중이거나 포인터가 캔버스 위에 있을 때만 다시 그린다.
        let busy = m.compiling || m.loading || m.llm.running;
        if let Some(after) = flow_repaint(self.animate, busy, pointer_in, ctx.input(|i| i.focused)) {
            ctx.request_repaint_after(after);
        }

        let mut rect = self.scene_rect;
        let framed = Frame::new()
            .fill(p.panel2)
            .stroke(Stroke::new(1.0, p.line))
            .corner_radius(CornerRadius::same(12))
            .show(ui, |ui| {
                let size = ui.available_size();
                ui.set_min_size(size);
                egui::Scene::new().zoom_range(0.3..=1.6).show(ui, &mut rect, |ui| {
                    self.draw(ui, m, p, &mut actions);
                });
            });
        let new_area = framed.response.rect;
        if pointer_in
            && ctx.input(|i| {
                i.pointer.is_decidedly_dragging() || i.smooth_scroll_delta != egui::Vec2::ZERO || i.zoom_delta() != 1.0
            })
        {
            self.user_view = true;
        }
        if !self.user_view && (new_area.size() - self.area.size()).length() > 1.0 {
            self.want_fit = true;
        }
        self.area = new_area;
        self.scene_rect = rect;
        actions
    }

    fn toolbar(&mut self, ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("＋ 제약 노드", |ui| {
                for (cat, items) in TEMPLATES {
                    ui.label(RichText::new(cat).color(p.accent).strong());
                    for t in items {
                        if ui.button(*t).clicked() {
                            actions.push(Action::AddGoal(t.to_string()));
                            ui.close();
                        }
                    }
                    ui.separator();
                }
                if ui.button("빈 제약 추가").clicked() {
                    actions.push(Action::AddGoal(String::new()));
                    ui.close();
                }
            });
            ui.menu_button("🧩 블루프린트", |ui| {
                ui.label(
                    RichText::new("타겟·역할·제약·출력·샘플 행 수·테마를 한 번에 설정합니다 (현재 데이터는 유지)")
                        .small()
                        .color(p.sub),
                );
                for (i, preset) in PRESETS.iter().enumerate() {
                    if ui.button(preset.name).on_hover_text(preset.summary).clicked() {
                        actions.push(Action::Preset(i));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("블루프린트 열기…").clicked() {
                    actions.push(Action::LoadProject);
                    ui.close();
                }
                if ui.button("현재 설정을 블루프린트로 저장…").clicked() {
                    actions.push(Action::SaveProject);
                    ui.close();
                }
            });
            if ui
                .button("자동 정렬")
                .on_hover_text("노드를 기본 배치로 되돌립니다")
                .clicked()
            {
                self.want_layout = true;
            }
            if ui.button("화면 맞춤").clicked() {
                self.request_fit();
            }
            ui.checkbox(&mut self.snap, "격자 스냅");
            ui.checkbox(&mut self.animate, "흐름 애니메이션")
                .on_hover_text("작업 중이거나 포인터가 캔버스 위에 있을 때만 움직입니다");
            ui.separator();
            if ui
                .button("블루프린트 저장")
                .on_hover_text(
                    "Ctrl+S · 지금 블루프린트 파일에 저장합니다 (처음이면 위치를 고릅니다)\n데이터 경로·역할·제약·출력·타겟·샘플 행 수·테마·LLM 모델·실행 옵션·노드 배치",
                )
                .clicked()
            {
                actions.push(Action::QuickSave);
            }
            if ui.button("블루프린트 열기").on_hover_text("Ctrl+O").clicked() {
                actions.push(Action::LoadProject);
            }
            if ui.button("결과 저장").clicked() {
                actions.push(Action::SaveOutput);
            }
        });
        ui.label(
            RichText::new(
                "노드 제목줄을 끌어 이동 · 빈 곳을 끌어 화면 이동 · Ctrl+휠 확대 · Delete로 선택한 제약 삭제 · 캔버스 전체가 블루프린트로 저장됩니다",
            )
            .small()
            .color(p.sub),
        );
        ui.add_space(2.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &mut self,
        ui: &mut Ui,
        key: NodeKey,
        title: &str,
        color: Color32,
        width: f32,
        p: &Palette,
        dim: bool,
        add: impl FnOnce(&mut Ui),
    ) {
        let pos = match self.pos.get(&key) {
            Some(p) => *p,
            None => {
                let p = self.fallback_pos(key);
                self.pos.insert(key, p);
                p
            }
        };
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(key)
                .max_rect(Rect::from_min_size(pos, egui::vec2(width, 4000.0)))
                .layout(Layout::top_down(Align::Min)),
        );
        let selected = self.selected == Some(key);
        let edge = if dim { color.gamma_multiply(0.4) } else { color };
        let out = Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(if selected { 2.5 } else { 1.5 }, edge))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(10))
            .shadow(Shadow {
                offset: [0, 6],
                blur: 18,
                spread: 0,
                color: Color32::from_black_alpha(if selected { 150 } else { 90 }),
            })
            .show(&mut child, |ui| {
                ui.set_width(width - 20.0);
                ui.label(RichText::new(title).color(edge).strong().size(14.0));
                ui.separator();
                add(ui);
            });
        let rect = out.response.rect;
        let header = Rect::from_min_size(rect.min, egui::vec2(rect.width(), 36.0));
        let drag = ui.interact(header, Id::unique(("canvas-node", key)), Sense::click_and_drag());
        if drag.clicked() || drag.drag_started() {
            self.selected = Some(key);
        }
        if drag.dragged() {
            if let Some(moved) = clamp_pos(pos + drag.drag_delta()) {
                self.pos.insert(key, moved);
            }
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        } else if drag.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
        if drag.drag_stopped()
            && self.snap
            && let Some(cur) = self.pos.get_mut(&key)
        {
            cur.x = (cur.x / 20.0).round() * 20.0;
            cur.y = (cur.y / 20.0).round() * 20.0;
        }
        self.rects.insert(key, rect);
    }

    fn draw(&mut self, ui: &mut Ui, m: &mut Model, p: &Palette, actions: &mut Vec<Action>) {
        // 격자 점 (정수 칸 번호로 세어 어떤 좌표에서도 반드시 끝난다)
        if let Some((step, xs, ys)) = grid_cells(ui.clip_rect(), GRID_STEP, MAX_GRID_DOTS) {
            let dot = p.line.gamma_multiply(0.7);
            let painter = ui.painter();
            for iy in ys {
                let y = (iy as f64 * step as f64) as f32;
                for ix in xs.clone() {
                    painter.circle_filled(Pos2::new((ix as f64 * step as f64) as f32, y), 1.2, dot);
                }
            }
        }

        let wire_slot = ui.painter().add(Shape::Noop);
        let live_ids: Vec<NodeKey> = m.goals.iter().map(|g| NodeKey::Goal(g.id)).collect();
        self.rects
            .retain(|k, _| matches!(k, NodeKey::Goal(_)) && live_ids.contains(k) || !matches!(k, NodeKey::Goal(_)));

        // 블루프린트 설정
        self.node(
            ui,
            NodeKey::Settings,
            "⚙ 블루프린트 설정",
            SETTINGS_COLOR,
            250.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label("이름");
                    // 저장하지 않은 변경 표시(●). 자리를 늘 잡아 두고 입력칸에 고정 id를 주어,
                    // 첫 글자를 칠 때 표시가 나타나도 입력칸의 포커스가 풀리지 않게 한다.
                    let (slot, resp) = ui.allocate_exact_size(egui::vec2(10.0, 18.0), Sense::hover());
                    if m.dirty {
                        ui.painter().text(
                            slot.center(),
                            egui::Align2::CENTER_CENTER,
                            "●",
                            egui::FontId::proportional(12.0),
                            p.warn(),
                        );
                        resp.on_hover_text("저장하지 않은 변경 사항이 있습니다 (Ctrl+S로 저장)");
                    }
                    ui.add(
                        egui::TextEdit::singleline(m.project)
                            .id_salt("canvas-project")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("테마");
                    egui::ComboBox::from_id_salt("canvas-theme")
                        .width(170.0)
                        .selected_text(THEMES[(*m.theme_idx).min(THEMES.len() - 1)].name)
                        .show_ui(ui, |ui| {
                            for (i, t) in THEMES.iter().enumerate() {
                                ui.selectable_value(m.theme_idx, i, t.name);
                            }
                        });
                });
                ui.add_enabled(m.fog_theme, egui::Checkbox::new(m.fog, "안개 효과"))
                    .on_hover_text("움직이는 안개를 그립니다. 끄면 다시 그리기가 줄어듭니다")
                    .on_disabled_hover_text("안개 테마(06·07)에서만 쓰입니다");
                ui.horizontal(|ui| {
                    if ui
                        .small_button("저장")
                        .on_hover_text("Ctrl+S · 지금 블루프린트 파일에 저장")
                        .clicked()
                    {
                        actions.push(Action::QuickSave);
                    }
                    if ui.small_button("다른 이름으로…").clicked() {
                        actions.push(Action::SaveProject);
                    }
                    if ui.small_button("열기").clicked() {
                        actions.push(Action::LoadProject);
                    }
                });
            },
        );

        // 데이터
        self.node(
            ui,
            NodeKey::Data,
            "◆ 데이터 소스",
            DATA_COLOR,
            250.0,
            p,
            false,
            |ui| {
                match m.loaded {
                    Some(d) => {
                        ui.label(RichText::new(&d.data.source_name).strong());
                        ui.label(format!("{} rows · {} columns", d.data.total_rows, d.data.columns.len()));
                        if !d.data.anomalies.is_empty() {
                            ui.label(
                                RichText::new(format!("이상치 {}건 감지", d.data.anomalies.len())).color(p.warn()),
                            );
                        }
                        ui.horizontal_wrapped(|ui| {
                            for c in d.data.columns.iter().take(6) {
                                let col = if c.dtype == "numeric" { DATA_COLOR } else { ROLE_COLOR };
                                ui.label(
                                    RichText::new(format!("{}·{}", c.name, c.dtype.chars().next().unwrap_or('?')))
                                        .small()
                                        .color(col),
                                );
                            }
                        });
                    }
                    None => {
                        ui.label(RichText::new("데이터 없음").color(p.sub));
                        ui.label(RichText::new("파일을 창에 끌어다 놓아도 됩니다").small().color(p.sub));
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("샘플 행");
                    let r = ui.add(egui::DragValue::new(m.sample_rows).range(0..=MAX_SAMPLE_ROWS));
                    if r.drag_stopped() || r.lost_focus() {
                        actions.push(Action::ReloadSample);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!m.loading, egui::Button::new("📂 로드")).clicked() {
                        actions.push(Action::PickFile);
                    }
                    if m.loading {
                        ui.spinner();
                    }
                });
            },
        );

        // 역할
        self.node(ui, NodeKey::Role, "◆ 역할", ROLE_COLOR, 250.0, p, false, |ui| {
            ui.add(
                egui::TextEdit::multiline(m.role)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("역할을 입력"),
            );
            ui.label(
                RichText::new("수식어(매우, please…)는 린트가 자동 압축")
                    .small()
                    .color(p.sub),
            );
        });

        // 출력
        self.node(
            ui,
            NodeKey::Output,
            "◆ 출력 포맷",
            OUT_COLOR,
            250.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(m.format, OutputFormat::StrictJson, "JSON");
                    ui.radio_value(m.format, OutputFormat::MarkdownTable, "표");
                });
                ui.add(
                    egui::TextEdit::singleline(m.fields_text)
                        .desired_width(f32::INFINITY)
                        .hint_text("필드1,필드2"),
                );
                let fields = pipeline::parse_fields(m.fields_text);
                if fields.is_empty() {
                    ui.label(RichText::new("필수 필드가 비어 있음").small().color(theme::ERR));
                } else {
                    let schema = OutputSchemaAst::render_schema(*m.format, &fields);
                    let short: String = schema.chars().take(70).collect();
                    ui.label(RichText::new(short).monospace().small().color(p.sub));
                }
            },
        );

        // 제약
        // 린트 경고 배지는 진단이 가리키는 위치(컴파일 요청에 들어간, 사용 중이고 비어 있지 않은 제약의 순서)로 찾는다.
        // 결과가 오래됐으면 위치가 어긋날 수 있으므로 표시하지 않는다.
        let mut active = 0usize;
        for g in m.goals.iter_mut() {
            let kind = ConstraintNode::classify(String::new(), &g.text).rule_type;
            let color = goal_color(&kind);
            let pos = (g.on && !g.text.trim().is_empty()).then(|| {
                active += 1;
                active - 1
            });
            let related =
                !m.stale && pos.is_some_and(|i| m.result.is_some_and(|r| lint_mentions(&r.ast.lint_warnings, i)));
            let title = format!("● 제약 · {}", kind.tag());
            let id = g.id;
            self.node(ui, NodeKey::Goal(id), &title, color, 260.0, p, !g.on, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut g.text)
                        .id_salt(("canvas-goal", id))
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .hint_text("제약 문장"),
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut g.on, "사용");
                    if ui.small_button("삭제").clicked() {
                        actions.push(Action::RemoveGoal(id));
                    }
                    if related {
                        ui.label(RichText::new("⚠ 린트 경고").small().color(p.warn()));
                    }
                });
            });
        }

        // 컴파일러(루트)
        let active = m.goals.iter().filter(|g| g.on).count();
        self.node(
            ui,
            NodeKey::Root,
            "▶ RootPromptAST 컴파일러",
            p.accent,
            230.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label("타겟");
                    egui::ComboBox::from_id_salt("canvas-target")
                        .selected_text(m.target.label())
                        .show_ui(ui, |ui| {
                            for t in TargetModel::ALL {
                                ui.selectable_value(m.target, t, t.label());
                            }
                        });
                });
                ui.label(format!("활성 제약 {active}개"));
                if theme::primary_button(ui, p, "⚡ Compile & Run", !m.compiling).clicked() {
                    actions.push(Action::Compile);
                }
                if m.compiling {
                    ui.spinner();
                } else if m.stale && m.result.is_some() {
                    ui.label(RichText::new("● 입력이 바뀜 · 다시 컴파일").small().color(p.warn()));
                }
            },
        );

        // 결과
        self.node(
            ui,
            NodeKey::Result,
            "★ 결과 · 토큰 절감",
            RESULT_COLOR,
            270.0,
            p,
            false,
            |ui| match m.result {
                Some(r) => {
                    let good = r.saving_pct >= 0.0;
                    ui.label(
                        RichText::new(format!(
                            "{:+.1}%",
                            if r.saving_pct == 0.0 { 0.0 } else { -r.saving_pct }
                        ))
                        .size(32.0)
                        .color(if good { p.hi } else { theme::ERR })
                        .strong(),
                    );
                    let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), Sense::hover());
                    ui.painter().rect_filled(bar, 4.0, p.line.gamma_multiply(0.5));
                    let frac = (r.saving_pct / 100.0).clamp(0.0, 1.0) as f32;
                    let fill = Rect::from_min_size(bar.min, egui::vec2(bar.width() * frac, bar.height()));
                    ui.painter().rect_filled(fill, 4.0, p.accent);
                    ui.label(format!("원본 {} → 압축 {} tokens", r.orig_tokens, r.comp_tokens));
                    ui.label(RichText::new(format!("절감 ${:.4}", r.saved_usd)).color(p.sub));
                    let warns = r.ast.lint_warnings.iter().filter(|d| d.severity == "WARN").count();
                    let errs = r.ast.lint_warnings.iter().filter(|d| d.severity == "ERROR").count();
                    ui.label(
                        RichText::new(format!("린트 경고 {warns} · 오류 {errs}")).color(if errs > 0 {
                            theme::ERR
                        } else {
                            theme::OK
                        }),
                    );
                    for line in r.output.lines().take(5) {
                        ui.add(egui::Label::new(RichText::new(line).monospace().small().color(p.sub)).truncate());
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button("복사").clicked() {
                            ui.ctx().copy_text(r.output.clone());
                        }
                        if ui.small_button("저장").clicked() {
                            actions.push(Action::SaveOutput);
                        }
                    });
                }
                None => {
                    ui.label(RichText::new("아직 컴파일하지 않았습니다").color(p.sub));
                }
            },
        );

        // LLM 실행
        let has_result = m.result.is_some();
        let stale = m.stale;
        let compiling = m.compiling;
        let llm = &mut m.llm;
        self.node(
            ui,
            NodeKey::Llm,
            "✦ LLM 실행",
            LLM_COLOR,
            250.0,
            p,
            !llm.has_key,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(llm.provider);
                    ui.add(
                        egui::TextEdit::singleline(llm.model)
                            .id_salt("canvas-llm-model")
                            .desired_width(f32::INFINITY)
                            .hint_text(llm.model_hint),
                    );
                });
                // 제목에 현재 값 요약이 붙어 바뀌므로 열림 상태는 고정 id로 기억한다.
                egui::CollapsingHeader::new(format!("실행 옵션 · {}", options_summary(llm.opts, llm.temp_max)))
                    .id_salt("canvas-llm-opts")
                    .show(ui, |ui| llm_options_ui(ui, llm.opts, llm.temp_max, "canvas", p));
                // 입력이 바뀐 뒤의 오래된 결과로는 실행하지 않는다.
                let enabled = llm.has_key && has_result && !stale && !compiling && !llm.running;
                ui.horizontal(|ui| {
                    let label = if llm.running { "실행 중" } else { "▶ 실제 실행" };
                    if theme::primary_button(ui, p, label, enabled).clicked() {
                        actions.push(Action::RunLlm);
                    }
                    if llm.running {
                        ui.spinner();
                        if ui
                            .small_button("■ 취소")
                            .on_hover_text("응답을 기다리지 않고 버립니다")
                            .clicked()
                        {
                            actions.push(Action::CancelLlm);
                        }
                    }
                });
                match (&llm.verdict, llm.has_key, has_result) {
                    _ if has_result && stale && llm.has_key && !llm.running => {
                        ui.label(
                            RichText::new("입력이 바뀜 · 다시 컴파일한 뒤 실행")
                                .small()
                                .color(p.warn()),
                        );
                    }
                    (Some((ok, text)), _, _) => {
                        ui.label(
                            RichText::new(text)
                                .small()
                                .color(if *ok { theme::OK } else { theme::ERR }),
                        );
                    }
                    (None, false, _) => {
                        ui.label(
                            RichText::new(format!("{} 설정 후 앱을 다시 시작하세요", llm.key_env))
                                .small()
                                .color(p.sub),
                        );
                    }
                    (None, true, false) => {
                        ui.label(RichText::new("먼저 컴파일하세요").small().color(p.sub));
                    }
                    (None, true, true) => {
                        ui.label(
                            RichText::new("응답은 출력 형식·필수 필드로 검증합니다")
                                .small()
                                .color(p.sub),
                        );
                    }
                }
                if let Some(d) = &llm.details {
                    ui.label(RichText::new(d).small().color(p.sub));
                }
                if let Some(limit) = llm.retry_limit
                    && ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(format!("한도 2배({limit})로 다시 실행")).small(),
                        )
                        .clicked()
                {
                    actions.push(Action::RetryLonger(limit));
                }
            },
        );

        // 연결선
        let time = ui.ctx().input(|i| i.time) as f32;
        let mut shapes = Vec::new();
        if let Some(root) = self.rects.get(&NodeKey::Root).copied() {
            let to = root.left_center();
            let mut sources: Vec<(NodeKey, Color32, bool)> = vec![
                (NodeKey::Data, DATA_COLOR, m.loaded.is_some()),
                (NodeKey::Role, ROLE_COLOR, !m.role.trim().is_empty()),
                (NodeKey::Output, OUT_COLOR, true),
            ];
            for g in m.goals.iter() {
                let kind = ConstraintNode::classify(String::new(), &g.text).rule_type;
                sources.push((NodeKey::Goal(g.id), goal_color(&kind), g.on));
            }
            for (k, color, live) in sources {
                if let Some(r) = self.rects.get(&k) {
                    wire(
                        &mut shapes,
                        r.right_center(),
                        to,
                        color,
                        time,
                        self.animate && live,
                        live,
                        p,
                    );
                }
            }
            if let Some(res) = self.rects.get(&NodeKey::Result) {
                wire(
                    &mut shapes,
                    root.right_center(),
                    res.left_center(),
                    RESULT_COLOR,
                    time,
                    self.animate && m.result.is_some(),
                    true,
                    p,
                );
                if let Some(l) = self.rects.get(&NodeKey::Llm) {
                    let live = m.llm.has_key && m.result.is_some();
                    wire(
                        &mut shapes,
                        res.right_center(),
                        l.left_center(),
                        LLM_COLOR,
                        time,
                        self.animate && m.llm.running,
                        live,
                        p,
                    );
                }
            }
        }
        ui.painter().set(wire_slot, Shape::Vec(shapes));
    }
}

#[allow(clippy::too_many_arguments)]
fn wire(out: &mut Vec<Shape>, a: Pos2, b: Pos2, color: Color32, time: f32, flow: bool, live: bool, p: &Palette) {
    let dx = ((b.x - a.x).abs() * 0.5).max(50.0);
    let pts = [a, a + egui::vec2(dx, 0.0), b - egui::vec2(dx, 0.0), b];
    let c = if live { color } else { color.gamma_multiply(0.3) };
    out.push(
        CubicBezierShape::from_points_stroke(
            pts,
            false,
            Color32::TRANSPARENT,
            Stroke::new(8.0, c.gamma_multiply(0.12)),
        )
        .into(),
    );
    let curve = CubicBezierShape::from_points_stroke(
        pts,
        false,
        Color32::TRANSPARENT,
        Stroke::new(2.5, c.gamma_multiply(0.9)),
    );
    if flow {
        for k in 0..3 {
            let t = (time * 0.35 + k as f32 / 3.0).fract();
            out.push(Shape::circle_filled(curve.sample(t), 3.6, p.hi));
        }
    }
    out.push(curve.into());
    out.push(Shape::circle_filled(a, 5.0, c));
    out.push(Shape::circle_filled(b, 5.0, c));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lint_badge_uses_constraint_position() {
        let req = |cs: &[&str]| pipeline::CompileRequest {
            target: TargetModel::Claude,
            role: "r".into(),
            constraints: cs.iter().map(|s| s.to_string()).collect(),
            format: OutputFormat::StrictJson,
            fields: vec!["a".into()],
            data: None,
        };
        // 앞 제약이 모순으로 지워진다: 배지는 지워진 제약(위치 0)과 남긴 제약(위치 1)에 달린다. 경고 문구에 글자만
        // 들어 있는 다른 제약(위치 2, "우선")에는 달리지 않는다(예전 부분 문자열 비교의 오탐).
        let r = pipeline::compile(&req(&["이상치 우선 탐지", "이상치 탐지하지 마", "우선"]));
        let d = &r.ast.lint_warnings;
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        assert!(!lint_mentions(d, 2), "{d:?}");
        // 같은 문장 중복: 지운 쪽(위치 0)과 남긴 쪽(위치 1, 나중 것) 모두 위치로 찾는다.
        let r = pipeline::compile(&req(&["금액   기준 정렬", "금액 기준 정렬"]));
        let d = &r.ast.lint_warnings;
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        // 리뷰 재현: 모순 의심 경고는 수식어를 지운 본문("값을 계산해")을 인용하므로, 원문("값을 정말 계산해")으로
        // 찾던 예전 방식에서는 앞 제약에 배지가 달리지 않았다. 두 제약 모두에 달린다.
        let r = pipeline::compile(&req(&["값을 정말 계산해", "값은 계산하지 마"]));
        let d = &r.ast.lint_warnings;
        assert!(
            d.iter()
                .any(|w| w.message.starts_with("모순 의심") && w.message.contains("\"값을 계산해\"")),
            "{d:?}"
        );
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        // 경고가 없는 제약에는 달리지 않는다
        let r = pipeline::compile(&req(&["이상치 우선 탐지", "결측치 제외"]));
        assert!(!lint_mentions(&r.ast.lint_warnings, 0) && !lint_mentions(&r.ast.lint_warnings, 1));
    }

    fn goals(ids: &[u64]) -> Vec<Goal> {
        ids.iter()
            .map(|&id| Goal {
                id,
                text: format!("제약 {id}"),
                on: true,
            })
            .collect()
    }

    fn rect_of(c: &Canvas, key: NodeKey, h: f32) -> Rect {
        Rect::from_min_size(c.pos_of(key).unwrap(), egui::vec2(260.0, h))
    }

    #[test]
    fn grid_loop_terminates_at_huge_and_bad_coordinates() {
        // y=1.5e9에서는 f32의 y += 40이 더 나아가지 않아 예전 루프가 멈추지 않았다.
        for top in [0.0_f32, 1.5e9, -3.0e9, 1.0e30] {
            let vis = Rect::from_min_size(Pos2::new(-200.0, top), egui::vec2(1300.0, 700.0));
            match grid_cells(vis, GRID_STEP, MAX_GRID_DOTS) {
                Some((step, xs, ys)) => {
                    let n = (xs.end - xs.start) * (ys.end - ys.start);
                    assert!(
                        n <= MAX_GRID_DOTS && step >= GRID_STEP,
                        "top={top}: {n} dots, step {step}"
                    );
                }
                // 1e30에서는 높이 700이 f32 정밀도에 묻혀 빈 영역이 된다: 점을 그리지 않는다
                None => assert!(top.abs() > 1.0e20, "top={top}"),
            }
        }
        // 아주 넓은 영역은 간격을 늘려 점 수를 제한한다
        let wide = Rect::from_min_size(Pos2::ZERO, egui::vec2(1.0e6, 1.0e6));
        let (step, xs, ys) = grid_cells(wide, GRID_STEP, MAX_GRID_DOTS).unwrap();
        assert!((xs.end - xs.start) * (ys.end - ys.start) <= MAX_GRID_DOTS && step > GRID_STEP);
        let bad = [
            Rect::from_min_max(Pos2::new(f32::NAN, 0.0), Pos2::new(10.0, 10.0)),
            Rect::from_min_max(Pos2::ZERO, Pos2::new(f32::INFINITY, 10.0)),
            Rect::NOTHING,
        ];
        for r in bad {
            assert!(grid_cells(r, GRID_STEP, MAX_GRID_DOTS).is_none(), "{r:?}");
        }
    }

    #[test]
    fn far_and_invalid_positions_are_clamped() {
        let mut c = Canvas::default();
        c.reset_layout(&[]);
        c.set_pos(NodeKey::Root, Pos2::new(0.0, 3.0e9));
        assert_eq!(c.pos_of(NodeKey::Root), Some(Pos2::new(0.0, blueprint::POS_LIMIT)));
        let before = c.pos_of(NodeKey::Data);
        c.set_pos(NodeKey::Data, Pos2::new(f32::NAN, 1.0));
        assert_eq!(c.pos_of(NodeKey::Data), before); // NaN은 무시
    }

    #[test]
    fn default_layout_matches_previous_fixed_spacing() {
        let placed: HashMap<NodeKey, Pos2> = layout_positions(&[1, 2, 3], |_| None).into_iter().collect();
        assert_eq!(placed[&NodeKey::Settings], Pos2::new(0.0, 0.0));
        assert_eq!(placed[&NodeKey::Data], Pos2::new(0.0, 170.0));
        assert_eq!(placed[&NodeKey::Role], Pos2::new(0.0, 400.0));
        assert_eq!(placed[&NodeKey::Output], Pos2::new(0.0, 580.0));
        assert_eq!(placed[&NodeKey::Goal(2)], Pos2::new(290.0, 112.0));
        assert_eq!(placed[&NodeKey::Root], Pos2::new(590.0, 120.0));
        assert_eq!(placed[&NodeKey::Result], Pos2::new(590.0, 310.0));
        assert_eq!(placed[&NodeKey::Llm], Pos2::new(900.0, 310.0));
    }

    #[test]
    fn layout_uses_measured_heights_so_long_goals_do_not_overlap() {
        let gs = goals(&[1, 2, 3]);
        let mut c = Canvas::default();
        c.reset_layout(&gs);
        // 지난 프레임: 1번 제약이 줄이 바뀌어 300px
        c.rects.insert(NodeKey::Goal(1), rect_of(&c, NodeKey::Goal(1), 300.0));
        c.rects.insert(NodeKey::Goal(2), rect_of(&c, NodeKey::Goal(2), 92.0));
        c.layout(&gs);
        assert_eq!(c.pos_of(NodeKey::Goal(2)), Some(Pos2::new(290.0, 320.0)));
        // 잰 높이가 없으면 문장 길이로 추정한다
        assert!(estimate_goal_height(&"가".repeat(60)) > estimate_goal_height("짧은 제약"));
        assert_eq!(estimate_goal_height("짧은 제약"), default_height(NodeKey::Goal(0)));
        assert!(estimate_goal_height("한 줄\n두 줄\n세 줄") > estimate_goal_height("한 줄"));
    }

    #[test]
    fn new_goal_goes_below_the_lowest_goal() {
        // 세 개 중 첫 번째를 지우고 하나 추가: 예전에는 index*112 = 224에 놓여 마지막 제약과 겹쳤다.
        let mut c = Canvas::default();
        c.reset_layout(&goals(&[1, 2, 3]));
        let mut live = goals(&[2, 3]);
        c.forget(NodeKey::Goal(1));
        live.push(Goal {
            id: 4,
            text: "새 제약".into(),
            on: true,
        });
        c.ensure_positions(&live);
        let new = c.pos_of(NodeKey::Goal(4)).unwrap();
        assert_eq!(new, Pos2::new(290.0, 224.0 + 92.0 + GAP));
        let new_rect = Rect::from_min_size(new, egui::vec2(260.0, 92.0));
        for id in [2, 3] {
            assert!(!new_rect.intersects(rect_of(&c, NodeKey::Goal(id), 92.0)), "{id}");
        }
        // 지난 프레임에 잰 높이(줄바꿈으로 길어진 노드)를 쓴다
        c.rects.insert(NodeKey::Goal(3), rect_of(&c, NodeKey::Goal(3), 200.0));
        live.push(Goal {
            id: 5,
            text: String::new(),
            on: true,
        });
        c.ensure_positions(&live);
        // 가장 아래 노드는 4번(336~428): 그 밑
        assert_eq!(c.pos_of(NodeKey::Goal(5)), Some(Pos2::new(290.0, 336.0 + 92.0 + GAP)));
        assert_eq!(place_below(std::iter::empty()), Pos2::new(GOAL_X, 0.0));
        // 사용자가 옮긴 열 아래에 붙는다
        assert_eq!(
            place_below([(Pos2::new(700.0, 40.0), 100.0), (Pos2::new(290.0, 0.0), 92.0)]),
            Pos2::new(700.0, 160.0)
        );
    }

    #[test]
    fn v1_layout_without_fixed_positions_does_not_pile_at_origin() {
        // v1 프로젝트 파일: 제약 하나와 root만 위치가 있다. 나머지는 기본 배치를 따라야 한다.
        let gs = goals(&[7, 8]);
        let mut c = Canvas::default();
        c.load_layout(&gs, [(NodeKey::Goal(7), [1.0, 2.0]), (NodeKey::Root, [1.0, 2.0])]);
        let defaults: HashMap<NodeKey, Pos2> = layout_positions(&[7, 8], |_| None).into_iter().collect();
        assert_eq!(c.pos_of(NodeKey::Root), Some(Pos2::new(1.0, 2.0)));
        assert_eq!(c.pos_of(NodeKey::Goal(7)), Some(Pos2::new(1.0, 2.0)));
        for k in [
            NodeKey::Data,
            NodeKey::Role,
            NodeKey::Output,
            NodeKey::Result,
            NodeKey::Llm,
            NodeKey::Settings,
            NodeKey::Goal(8),
        ] {
            assert_eq!(c.pos_of(k), Some(defaults[&k]), "{k:?}");
        }
        for k in [NodeKey::Data, NodeKey::Role, NodeKey::Llm, NodeKey::Result] {
            assert_ne!(c.pos_of(k), Some(Pos2::ZERO), "{k:?}");
        }
        // 화면을 그리기 전에 모든 노드 위치가 있으므로 저장해도 같은 배치가 남는다
        assert!(NodeKey::FIXED.iter().all(|k| c.pos_of(*k).is_some()));
        assert_eq!(c.fallback_pos(NodeKey::Llm), defaults[&NodeKey::Llm]);
    }

    #[test]
    fn fixed_names_roundtrip() {
        for k in NodeKey::FIXED {
            assert_eq!(NodeKey::from_fixed_name(k.fixed_name().unwrap()), Some(k));
        }
        assert_eq!(NodeKey::from_fixed_name("goal"), None);
    }

    #[test]
    fn flow_animation_repaints_only_when_needed() {
        assert_eq!(flow_repaint(true, false, false, true), None); // 유휴: 다시 그리지 않음
        assert_eq!(flow_repaint(true, true, false, true), Some(FLOW_FRAME)); // 작업 중
        assert_eq!(flow_repaint(true, false, true, true), Some(FLOW_FRAME)); // 포인터가 캔버스 위
        assert_eq!(flow_repaint(false, true, true, true), None); // 애니메이션 끔
        assert_eq!(flow_repaint(true, true, true, false), None); // 포커스 없음
        assert!(FLOW_FRAME >= Duration::from_millis(50)); // 최대 20fps
    }

    #[test]
    fn llm_options_summary() {
        let mut o = LlmOptions::default();
        assert_eq!(options_summary(&o, 1.0), "2048 토큰 · 온도 기본값");
        o.temperature = Some(1.5);
        o.system = "시스템".into();
        o.max_tokens = 4096;
        assert_eq!(options_summary(&o, 2.0), "4096 토큰 · 온도 1.50 · 시스템");
        // Anthropic(최대 1.0)에서는 실제로 보내는 값을 보여 준다
        assert_eq!(options_summary(&o, 1.0), "4096 토큰 · 온도 1.00 · 시스템");
    }
}
