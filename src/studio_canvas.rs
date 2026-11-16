//! 노드 캔버스: 데이터/역할/제약/출력 블록을 끌어 배치하고 컴파일러 노드로 연결해 결과까지 한 화면에서 본다.
//! 이동(드래그)·확대(Ctrl+휠, 핀치)는 egui::Scene이 처리한다.

use crate::ast::{ConstraintNode, ConstraintType, OutputFormat, OutputSchemaAst, TargetModel};
use crate::pipeline::{self, CompileResult, LoadedData};
use crate::theme::{self, Palette};
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Shape,
    Stroke, Ui, UiBuilder, epaint::CubicBezierShape, epaint::Shadow,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeKey {
    Data,
    Role,
    Output,
    Root,
    Result,
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
            NodeKey::Goal(_) => None,
        }
    }
    pub const FIXED: [NodeKey; 5] = [
        NodeKey::Data,
        NodeKey::Role,
        NodeKey::Output,
        NodeKey::Root,
        NodeKey::Result,
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
}

/// 캔버스가 앱에 요청하는 동작(소유권 충돌을 피하려고 반환값으로 전달한다).
pub enum Action {
    PickFile,
    Compile,
    RemoveGoal(u64),
    AddGoal(String),
    Recipe(usize),
    SaveProject,
    LoadProject,
    SaveOutput,
}

pub struct Recipe {
    pub name: &'static str,
    pub role: &'static str,
    pub goals: &'static [&'static str],
    pub fields: &'static str,
}

pub const RECIPES: [Recipe; 4] = [
    Recipe {
        name: "매출 이상치 분석",
        role: "매출 데이터를 분석해 근본 원인을 찾는 데이터 분석가",
        goals: &[
            "결측치 0.2% 제외",
            "이상치 우선 탐지",
            "데이터에 없는 내용은 추론하지 마",
        ],
        fields: "root_causes,outliers,recommendations",
    },
    Recipe {
        name: "데이터 품질 점검",
        role: "데이터 품질 감사관",
        goals: &[
            "결측치 비율을 컬럼별로 보고",
            "중복 행 여부를 확인",
            "데이터에 없는 내용은 추론하지 마",
        ],
        fields: "quality_issues,severity,fixes",
    },
    Recipe {
        name: "경영진 요약 리포트",
        role: "경영진에게 보고하는 비즈니스 분석가",
        goals: &[
            "요약은 3문장 이내",
            "숫자는 소수점 둘째 자리까지",
            "핵심 지표를 먼저 제시",
        ],
        fields: "headline,key_metrics,next_steps",
    },
    Recipe {
        name: "고객 세그먼트 분류",
        role: "고객 데이터를 세그먼트로 분류하는 분석가",
        goals: &[
            "금액 4M 기준으로 분류",
            "분류 근거를 한 줄로 설명",
            "분류할 수 없으면 unknown으로 표기",
        ],
        fields: "segment,reason,confidence",
    },
];

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

const GOAL_STEP: f32 = 112.0;

pub struct Canvas {
    scene_rect: Rect,
    pos: HashMap<NodeKey, Pos2>,
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

const ROLE_COLOR: Color32 = Color32::from_rgb(0xc0, 0x8c, 0xff);
const DATA_COLOR: Color32 = Color32::from_rgb(0x4f, 0xd1, 0xc5);
const OUT_COLOR: Color32 = Color32::from_rgb(0xf2, 0xd4, 0x6b);
const RESULT_COLOR: Color32 = Color32::from_rgb(0xff, 0x8f, 0xb8);

impl Canvas {
    pub fn pos_of(&self, key: NodeKey) -> Option<Pos2> {
        self.pos.get(&key).copied()
    }

    pub fn set_pos(&mut self, key: NodeKey, p: Pos2) {
        self.pos.insert(key, p);
        self.want_layout = false;
    }

    pub fn request_fit(&mut self) {
        self.want_fit = true;
        self.user_view = false;
    }

    /// 노드 배치를 기본값으로 되돌린다(스냅·애니메이션 설정은 유지).
    pub fn reset_layout(&mut self) {
        self.pos.clear();
        self.rects.clear();
        self.selected = None;
        self.want_layout = true;
        self.user_view = false;
    }

    /// 삭제된 노드의 위치·선택 상태를 정리한다.
    /// 위치(pos)는 남겨 두어 실행 취소로 되살리면 같은 자리에 나타난다.
    pub fn forget(&mut self, key: NodeKey) {
        self.rects.remove(&key);
        if self.selected == Some(key) {
            self.selected = None;
        }
    }

    fn layout(&mut self, goals: &[Goal]) {
        self.pos.insert(NodeKey::Data, Pos2::new(0.0, 0.0));
        self.pos.insert(NodeKey::Role, Pos2::new(0.0, 210.0));
        self.pos.insert(NodeKey::Output, Pos2::new(0.0, 390.0));
        for (i, g) in goals.iter().enumerate() {
            self.pos
                .insert(NodeKey::Goal(g.id), Pos2::new(290.0, i as f32 * GOAL_STEP));
        }
        let mid = (goals.len() as f32 * GOAL_STEP / 2.0 - 110.0).max(120.0);
        self.pos.insert(NodeKey::Root, Pos2::new(590.0, mid));
        self.pos.insert(NodeKey::Result, Pos2::new(590.0, mid + 190.0));
    }

    fn fit(&mut self) {
        let mut it = self.rects.values();
        if let Some(first) = it.next() {
            let all = it.fold(*first, |a, r| a.union(*r));
            self.scene_rect = all.expand(24.0);
            self.want_fit = false;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, m: &mut Model, p: &Palette) -> Vec<Action> {
        let mut actions = Vec::new();
        for (i, g) in m.goals.iter().enumerate() {
            self.pos
                .entry(NodeKey::Goal(g.id))
                .or_insert(Pos2::new(290.0, i as f32 * GOAL_STEP));
        }
        self.toolbar(ui, p, &mut actions);
        if self.want_layout {
            self.layout(m.goals);
            self.want_layout = false;
            self.want_fit = true;
        }
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
        // 창이 포커스를 잃으면 애니메이션용 다시 그리기를 멈춘다.
        if self.animate && ctx.input(|i| i.focused) {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
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
            ui.menu_button("🎃 레시피", |ui| {
                ui.label(
                    RichText::new("역할·제약·출력 필드를 한 번에 채웁니다")
                        .small()
                        .color(p.sub),
                );
                for (i, r) in RECIPES.iter().enumerate() {
                    if ui.button(r.name).clicked() {
                        actions.push(Action::Recipe(i));
                        ui.close();
                    }
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
            ui.checkbox(&mut self.animate, "흐름 애니메이션");
            ui.separator();
            if ui.button("프로젝트 저장").clicked() {
                actions.push(Action::SaveProject);
            }
            if ui.button("불러오기").clicked() {
                actions.push(Action::LoadProject);
            }
            if ui.button("결과 저장").clicked() {
                actions.push(Action::SaveOutput);
            }
        });
        ui.label(
            RichText::new(
                "노드 제목줄을 끌어 이동 · 빈 곳을 끌어 화면 이동 · Ctrl+휠 확대 · Delete로 선택한 제약 삭제",
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
        let pos = self.pos.get(&key).copied().unwrap_or(Pos2::ZERO);
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
            self.pos.insert(key, pos + drag.drag_delta());
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
        // 격자 점
        let vis = ui.clip_rect();
        let mut step = 40.0;
        while (vis.width() / step) * (vis.height() / step) > 6000.0 {
            step *= 2.0;
        }
        let dot = p.line.gamma_multiply(0.7);
        let mut y = (vis.top() / step).floor() * step;
        while y < vis.bottom() {
            let mut x = (vis.left() / step).floor() * step;
            while x < vis.right() {
                ui.painter().circle_filled(Pos2::new(x, y), 1.2, dot);
                x += step;
            }
            y += step;
        }

        let wire_slot = ui.painter().add(Shape::Noop);
        let live_ids: Vec<NodeKey> = m.goals.iter().map(|g| NodeKey::Goal(g.id)).collect();
        self.rects
            .retain(|k, _| matches!(k, NodeKey::Goal(_)) && live_ids.contains(k) || !matches!(k, NodeKey::Goal(_)));

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
        for g in m.goals.iter_mut() {
            let kind = ConstraintNode::classify(String::new(), &g.text).rule_type;
            let color = goal_color(&kind);
            let related = !g.text.trim().is_empty()
                && m.result.is_some_and(|r| {
                    r.ast
                        .lint_warnings
                        .iter()
                        .any(|d| d.severity != "INFO" && d.message.contains(g.text.trim()))
                });
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
