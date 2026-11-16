//! 노드 캔버스 GUI: 중앙 캔버스 / 우측 인스펙터 / 하단 프롬프트 테스트 창.
//! 그래프 실행은 워커 스레드에서 하고 mpsc 채널로 이벤트만 UI에 전달한다.

use super::exec::{self, Event, RunOptions, RunReport};
use super::graph::{Graph, NodeKind, PromptFormat, PromptTarget, Provider};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

const NODE_W: f32 = 200.0;
const NODE_H: f32 = 60.0;
const PORT_R: f32 = 6.0;

/// 노드 추가 메뉴의 생성 함수 타입.
type NodeCtor = fn() -> NodeKind;

#[derive(Clone, Default)]
enum RunState {
    #[default]
    Idle,
    Running,
    Done(u128),
    Failed(String),
    Skipped,
}

#[derive(Clone, Default)]
struct NodeRun {
    state: RunState,
    output: String,
}

#[derive(Clone)]
struct TestCase {
    name: String,
    input: String,
    /// 비어 있지 않으면 최종 출력에 이 문자열이 포함되어야 통과
    expect: String,
    result: Option<TestResult>,
}

#[derive(Clone)]
struct TestResult {
    pass: bool,
    output: String,
    error: String,
    ms: u128,
}

enum Msg {
    Node(Event),
    Test(usize, TestResult),
    Finished(Result<Option<RunReport>, String>),
}

pub struct CanvasApp {
    graph: Graph,
    path: Option<PathBuf>,
    selected: Option<u32>,
    pan: Vec2,
    link_from: Option<u32>,
    runs: HashMap<u32, NodeRun>,
    final_text: String,
    allow_mcp: bool,
    status: String,
    status_err: bool,
    running: bool,
    tests: Vec<TestCase>,
    test_node: Option<u32>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

fn sample_graph() -> Graph {
    let mut g = Graph::default();
    let t = g.add_node(
        NodeKind::Text {
            text: "3분기 매출이 급감한 원인을 분석해줘".into(),
        },
        [40.0, 60.0],
    );
    let p = g.add_node(NodeKind::new_prompt(), [300.0, 60.0]);
    let l = g.add_node(NodeKind::new_llm(), [560.0, 60.0]);
    let o = g.add_node(NodeKind::Output, [820.0, 60.0]);
    for (a, b) in [(t, p), (p, l), (l, o)] {
        let _ = g.connect(a, b);
    }
    g
}

impl CanvasApp {
    pub fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>) -> Self {
        crate::app::install_korean_font(&cc.egui_ctx);
        let (tx, rx) = channel();
        let mut app = CanvasApp {
            graph: sample_graph(),
            path: None,
            selected: None,
            pan: Vec2::ZERO,
            link_from: None,
            runs: HashMap::new(),
            final_text: String::new(),
            allow_mcp: false,
            status: "준비됨".into(),
            status_err: false,
            running: false,
            tests: vec![
                TestCase {
                    name: "기본".into(),
                    input: "3분기 매출이 급감한 원인을 분석해줘".into(),
                    expect: "dry-run".into(),
                    result: None,
                },
                TestCase {
                    name: "영문 입력".into(),
                    input: "Why did Q3 revenue drop?".into(),
                    expect: String::new(),
                    result: None,
                },
            ],
            test_node: None,
            tx,
            rx,
        };
        if let Some(p) = file {
            app.load(p);
        }
        app.fix_test_node();
        app
    }

    fn set_status(&mut self, s: impl Into<String>, err: bool) {
        self.status = s.into();
        self.status_err = err;
    }

    /// 테스트 입력 노드가 유효한 Text 노드를 가리키도록 보정한다.
    fn fix_test_node(&mut self) {
        let valid = self.test_node.is_some_and(|id| {
            self.graph
                .node(id)
                .is_some_and(|n| matches!(n.kind, NodeKind::Text { .. }))
        });
        if !valid {
            self.test_node = self
                .graph
                .nodes
                .iter()
                .find(|n| matches!(n.kind, NodeKind::Text { .. }))
                .map(|n| n.id);
        }
    }

    fn load(&mut self, p: PathBuf) {
        match std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|s| Graph::from_json(&s))
        {
            Ok(g) => {
                self.graph = g;
                self.runs.clear();
                self.selected = None;
                self.allow_mcp = false; // 불러온 그래프의 MCP 명령은 매번 다시 허용해야 한다
                self.set_status(format!("불러옴: {}", p.display()), false);
                self.path = Some(p);
                self.fix_test_node();
            }
            Err(e) => self.set_status(format!("불러오기 실패: {e}"), true),
        }
    }

    fn save(&mut self, as_new: bool) {
        let target = if as_new || self.path.is_none() {
            rfd::FileDialog::new()
                .add_filter("canvas", &["json"])
                .set_file_name("flow.canvas.json")
                .save_file()
        } else {
            self.path.clone()
        };
        let Some(p) = target else { return };
        match self
            .graph
            .to_json()
            .and_then(|s| std::fs::write(&p, s).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.set_status(format!("저장됨: {}", p.display()), false);
                self.path = Some(p);
            }
            Err(e) => self.set_status(format!("저장 실패: {e}"), true),
        }
    }

    // ---------- 실행 ----------

    fn begin_run(&mut self) -> bool {
        if self.running {
            return false;
        }
        if let Err(e) = self.graph.topo_order() {
            self.set_status(e, true);
            return false;
        }
        self.running = true;
        self.runs.clear();
        self.final_text.clear();
        true
    }

    fn run_flow(&mut self, ctx: &egui::Context) {
        if !self.begin_run() {
            return;
        }
        self.set_status("실행 중…", false);
        let (g, opts, tx, ctx) = (
            self.graph.clone(),
            RunOptions {
                allow_mcp: self.allow_mcp,
                input_override: None,
            },
            self.tx.clone(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            let r = exec::run_graph(&g, &opts, |e| {
                let _ = tx.send(Msg::Node(e));
                ctx.request_repaint();
            });
            let _ = tx.send(Msg::Finished(r.map(Some)));
            ctx.request_repaint();
        });
    }

    /// `only`가 Some이면 해당 케이스만, None이면 전부 순차 실행한다.
    fn run_tests(&mut self, only: Option<usize>, ctx: &egui::Context) {
        let Some(entry) = self.test_node else {
            self.set_status("테스트 입력으로 쓸 Text 노드가 없습니다", true);
            return;
        };
        if !self.begin_run() {
            return;
        }
        let idxs: Vec<usize> = match only {
            Some(i) => vec![i],
            None => (0..self.tests.len()).collect(),
        };
        for &i in &idxs {
            self.tests[i].result = None;
        }
        self.set_status("테스트 실행 중…", false);
        let cases: Vec<(usize, String, String)> = idxs
            .iter()
            .map(|&i| (i, self.tests[i].input.clone(), self.tests[i].expect.clone()))
            .collect();
        let (g, allow, tx, ctx) = (self.graph.clone(), self.allow_mcp, self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            for (i, input, expect) in cases {
                let opts = RunOptions {
                    allow_mcp: allow,
                    input_override: Some((entry, input)),
                };
                let res = match exec::run_graph(&g, &opts, |e| {
                    let _ = tx.send(Msg::Node(e));
                    ctx.request_repaint();
                }) {
                    Ok(r) => {
                        let error = r.errors.values().cloned().collect::<Vec<_>>().join(" | ");
                        let pass = r.ok() && (expect.is_empty() || r.final_text.contains(&expect));
                        let error = if r.ok() && !pass {
                            format!("출력에 '{expect}' 가 없습니다")
                        } else {
                            error
                        };
                        TestResult {
                            pass,
                            output: r.final_text,
                            error,
                            ms: r.total_ms,
                        }
                    }
                    Err(e) => TestResult {
                        pass: false,
                        output: String::new(),
                        error: e,
                        ms: 0,
                    },
                };
                let _ = tx.send(Msg::Test(i, res));
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::Finished(Ok(None)));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Node(e) => {
                    let (id, run) = match e {
                        Event::Started(id) => (
                            id,
                            NodeRun {
                                state: RunState::Running,
                                output: String::new(),
                            },
                        ),
                        Event::Done { id, output, ms } => (
                            id,
                            NodeRun {
                                state: RunState::Done(ms),
                                output,
                            },
                        ),
                        Event::Failed { id, error } => (
                            id,
                            NodeRun {
                                state: RunState::Failed(error),
                                output: String::new(),
                            },
                        ),
                        Event::Skipped(id) => (
                            id,
                            NodeRun {
                                state: RunState::Skipped,
                                output: String::new(),
                            },
                        ),
                    };
                    self.runs.insert(id, run);
                }
                Msg::Test(i, r) => {
                    if let Some(t) = self.tests.get_mut(i) {
                        t.result = Some(r);
                    }
                }
                Msg::Finished(r) => {
                    self.running = false;
                    match r {
                        Ok(Some(rep)) => {
                            self.final_text = rep.final_text.clone();
                            if rep.ok() {
                                self.set_status(format!("완료 ({} ms)", rep.total_ms), false);
                            } else {
                                self.set_status(format!("{}개 노드 실패", rep.errors.len()), true);
                            }
                        }
                        Ok(None) => {
                            let pass = self
                                .tests
                                .iter()
                                .filter(|t| t.result.as_ref().is_some_and(|r| r.pass))
                                .count();
                            let done = self.tests.iter().filter(|t| t.result.is_some()).count();
                            self.set_status(format!("테스트 {pass}/{done} 통과"), pass != done);
                        }
                        Err(e) => self.set_status(e, true),
                    }
                }
            }
        }
    }

    // ---------- 화면 ----------

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("새로 만들기").clicked() {
                self.graph = Graph::default();
                self.path = None;
                self.runs.clear();
                self.selected = None;
                self.fix_test_node();
            }
            if ui.button("열기").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("canvas", &["json"]).pick_file()
            {
                self.load(p);
            }
            if ui.button("저장").clicked() {
                self.save(false);
            }
            if ui.button("다른 이름으로").clicked() {
                self.save(true);
            }
            ui.separator();
            ui.menu_button("＋ 노드 추가", |ui| {
                let kinds: [(&str, NodeCtor); 5] = [
                    ("Text (입력)", NodeKind::new_text),
                    ("Prompt (컴파일러)", NodeKind::new_prompt),
                    ("LLM API", NodeKind::new_llm),
                    ("MCP Tool", NodeKind::new_mcp),
                    ("Output", || NodeKind::Output),
                ];
                for (label, make) in kinds {
                    if ui.button(label).clicked() {
                        let off = (self.graph.nodes.len() % 8) as f32 * 24.0;
                        let id = self
                            .graph
                            .add_node(make(), [60.0 + off - self.pan.x, 200.0 + off - self.pan.y]);
                        self.selected = Some(id);
                        self.fix_test_node();
                        ui.close();
                    }
                }
            });
            ui.separator();
            ui.checkbox(&mut self.allow_mcp, "MCP 실행 허용")
                .on_hover_text("MCP 노드는 지정한 명령을 로컬에서 실행합니다. 신뢰하는 그래프에서만 켜세요.");
            if ui
                .add_enabled(!self.running, egui::Button::new("▶ 전체 실행"))
                .clicked()
            {
                self.run_flow(ctx);
            }
            if self.running {
                ui.spinner();
            }
            ui.separator();
            let color = if self.status_err {
                Color32::from_rgb(230, 80, 80)
            } else {
                ui.visuals().text_color()
            };
            ui.label(RichText::new(&self.status).color(color));
        });
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        painter.rect_filled(resp.rect, 0.0, Color32::from_gray(24));
        // 점 격자
        let step = 28.0;
        let origin = resp.rect.min + self.pan;
        let off = vec2(self.pan.x.rem_euclid(step), self.pan.y.rem_euclid(step));
        let mut y = resp.rect.min.y + off.y;
        while y < resp.rect.max.y {
            let mut x = resp.rect.min.x + off.x;
            while x < resp.rect.max.x {
                painter.circle_filled(pos2(x, y), 1.0, Color32::from_gray(48));
                x += step;
            }
            y += step;
        }
        if resp.dragged() && self.link_from.is_none() {
            self.pan += resp.drag_delta();
        }
        if resp.clicked() {
            self.selected = None;
        }

        let rect_of = |pos: [f32; 2]| Rect::from_min_size(origin + vec2(pos[0], pos[1]), vec2(NODE_W, NODE_H));
        let rects: HashMap<u32, Rect> = self.graph.nodes.iter().map(|n| (n.id, rect_of(n.pos))).collect();

        // 간선
        let mut remove_edge = None;
        for (i, e) in self.graph.edges.iter().enumerate() {
            let (Some(a), Some(b)) = (rects.get(&e.from), rects.get(&e.to)) else {
                continue;
            };
            let (p0, p3) = (a.right_center(), b.left_center());
            let dx = ((p3.x - p0.x).abs() * 0.5).max(40.0);
            let pts = [p0, p0 + vec2(dx, 0.0), p3 - vec2(dx, 0.0), p3];
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                pts,
                false,
                Color32::TRANSPARENT,
                Stroke::new(2.0, Color32::from_rgb(120, 160, 220)),
            ));
            let mid = bezier_mid(pts);
            let hit = Rect::from_center_size(mid, vec2(16.0, 16.0));
            let r = ui.interact(hit, ui.scope_id().with(("edge", i)), Sense::click());
            let hot = r.hovered();
            painter.circle_filled(
                mid,
                7.0,
                if hot {
                    Color32::from_rgb(200, 70, 70)
                } else {
                    Color32::from_gray(60)
                },
            );
            painter.text(
                mid,
                Align2::CENTER_CENTER,
                "×",
                FontId::proportional(11.0),
                Color32::WHITE,
            );
            if r.clicked() {
                remove_edge = Some(i);
            }
        }
        if let Some(i) = remove_edge {
            self.graph.edges.remove(i);
        }

        // 노드
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let mut connect: Option<(u32, u32)> = None;
        let mut delete: Option<u32> = None;
        for idx in 0..self.graph.nodes.len() {
            let (id, has_in, has_out, label, title) = {
                let n = &self.graph.nodes[idx];
                (
                    n.id,
                    n.kind.has_input(),
                    n.kind.has_output(),
                    n.kind.label(),
                    n.title.clone(),
                )
            };
            let rect = rects[&id];
            let r = ui.interact(rect, ui.scope_id().with(("node", id)), Sense::click_and_drag());
            if r.dragged() {
                let d = r.drag_delta();
                self.graph.nodes[idx].pos[0] += d.x;
                self.graph.nodes[idx].pos[1] += d.y;
            }
            if r.clicked() || r.drag_started() {
                self.selected = Some(id);
            }
            r.context_menu(|ui| {
                if ui.button("삭제").clicked() {
                    delete = Some(id);
                    ui.close();
                }
            });
            let run = self.runs.get(&id).cloned().unwrap_or_default();
            let (border, badge) = match &run.state {
                RunState::Idle => (Color32::from_gray(90), String::new()),
                RunState::Running => (Color32::from_rgb(230, 180, 60), "실행 중".into()),
                RunState::Done(ms) => (Color32::from_rgb(80, 180, 100), format!("✔ {ms} ms")),
                RunState::Failed(_) => (Color32::from_rgb(220, 70, 70), "✖ 실패".into()),
                RunState::Skipped => (Color32::from_gray(110), "건너뜀".into()),
            };
            let sel = self.selected == Some(id);
            painter.rect_filled(rect, 8.0, Color32::from_gray(40));
            let header = Rect::from_min_size(rect.min, vec2(NODE_W, 22.0));
            painter.rect_filled(header, 8.0, kind_color(label));
            painter.rect_stroke(
                rect,
                8.0,
                Stroke::new(if sel { 3.0 } else { 1.5 }, if sel { Color32::WHITE } else { border }),
                StrokeKind::Middle,
            );
            painter.text(
                header.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            painter.text(
                rect.left_top() + vec2(10.0, 32.0),
                Align2::LEFT_CENTER,
                clip(&title, 26),
                FontId::proportional(13.0),
                Color32::from_gray(230),
            );
            painter.text(
                rect.left_top() + vec2(10.0, 48.0),
                Align2::LEFT_CENTER,
                badge,
                FontId::proportional(11.0),
                border,
            );

            if has_in {
                let c = rect.left_center();
                painter.circle_filled(c, PORT_R, Color32::from_gray(200));
            }
            if has_out {
                let c = rect.right_center();
                let pr = ui.interact(
                    Rect::from_center_size(c, vec2(18.0, 18.0)),
                    ui.scope_id().with(("out", id)),
                    Sense::drag(),
                );
                let hot = pr.hovered() || self.link_from == Some(id);
                painter.circle_filled(
                    c,
                    PORT_R,
                    if hot {
                        Color32::from_rgb(120, 200, 255)
                    } else {
                        Color32::from_gray(200)
                    },
                );
                if pr.drag_started() {
                    self.link_from = Some(id);
                }
                if pr.drag_stopped() && self.link_from == Some(id) {
                    if let Some(p) = pointer {
                        let target = self
                            .graph
                            .nodes
                            .iter()
                            .find(|n| n.kind.has_input() && n.id != id && rects[&n.id].expand(10.0).contains(p))
                            .map(|n| n.id);
                        if let Some(t) = target {
                            connect = Some((id, t));
                        }
                    }
                    self.link_from = None;
                }
            }
        }
        if let (Some(from), Some(p)) = (self.link_from, pointer)
            && let Some(a) = rects.get(&from)
        {
            painter.line_segment(
                [a.right_center(), p],
                Stroke::new(2.0, Color32::from_rgb(120, 200, 255)),
            );
        }
        if let Some((a, b)) = connect
            && let Err(e) = self.graph.connect(a, b)
        {
            self.set_status(e, true);
        }
        if delete.is_none()
            && self.selected.is_some()
            && !ui.memory(|m| m.focused().is_some())
            && ui.input(|i| i.key_pressed(egui::Key::Delete))
        {
            delete = self.selected;
        }
        if let Some(id) = delete {
            self.graph.remove_node(id);
            self.runs.remove(&id);
            self.selected = None;
            self.fix_test_node();
        }
        if self.graph.nodes.is_empty() {
            painter.text(
                resp.rect.center(),
                Align2::CENTER_CENTER,
                "‘＋ 노드 추가’로 시작하세요",
                FontId::proportional(16.0),
                Color32::from_gray(120),
            );
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label("노드를 선택하면 설정이 표시됩니다.\n\n• 출력 포트(●)를 입력 쪽 노드로 드래그해 연결\n• 간선 중앙의 × 클릭으로 연결 삭제\n• 우클릭 또는 Delete로 노드 삭제\n• 빈 곳 드래그로 화면 이동");
            if !self.final_text.is_empty() {
                ui.separator();
                ui.label(RichText::new("최종 출력").strong());
                readonly_box(ui, "final_out", &self.final_text);
            }
            return;
        };
        let upstream = self.graph.upstream(id);
        let ups: Vec<String> = upstream
            .iter()
            .filter_map(|u| self.graph.node(*u).map(|n| n.title.clone()))
            .collect();
        let Some(node) = self.graph.nodes.iter_mut().find(|n| n.id == id) else {
            return;
        };
        egui::ScrollArea::vertical().id_salt("inspector").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("이름");
                ui.text_edit_singleline(&mut node.title);
            });
            ui.label(
                RichText::new(format!(
                    "종류: {}   입력: {}",
                    node.kind.label(),
                    if ups.is_empty() {
                        "없음".into()
                    } else {
                        ups.join(", ")
                    }
                ))
                .weak(),
            );
            ui.separator();
            match &mut node.kind {
                NodeKind::Text { text } => {
                    ui.label("텍스트");
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .desired_rows(6)
                            .desired_width(f32::INFINITY),
                    );
                }
                NodeKind::Prompt {
                    role,
                    constraints,
                    fields,
                    target,
                    format,
                    data_path,
                } => {
                    ui.label("지시문 ({{input}} = 업스트림 출력)");
                    ui.add(
                        egui::TextEdit::multiline(role)
                            .desired_rows(5)
                            .desired_width(f32::INFINITY),
                    );
                    ui.label("제약 조건 (한 줄에 하나)");
                    ui.add(
                        egui::TextEdit::multiline(constraints)
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        ui.label("필수 필드");
                        ui.text_edit_singleline(fields);
                    });
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("pt")
                            .selected_text(format!("{target:?}"))
                            .show_ui(ui, |ui| {
                                for t in [PromptTarget::Claude, PromptTarget::Gpt, PromptTarget::Gemini] {
                                    ui.selectable_value(target, t, format!("{t:?}"));
                                }
                            });
                        egui::ComboBox::from_id_salt("pf")
                            .selected_text(format!("{format:?}"))
                            .show_ui(ui, |ui| {
                                for f in [PromptFormat::Json, PromptFormat::Table] {
                                    ui.selectable_value(format, f, format!("{f:?}"));
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("데이터 파일");
                        ui.text_edit_singleline(data_path);
                    });
                }
                NodeKind::Llm {
                    provider,
                    model,
                    system,
                    key_env,
                    base_url,
                    max_tokens,
                    temperature,
                    dry_run,
                } => {
                    let before = *provider;
                    egui::ComboBox::from_id_salt("prov")
                        .selected_text(provider.label())
                        .show_ui(ui, |ui| {
                            for p in Provider::ALL {
                                ui.selectable_value(provider, p, p.label());
                            }
                        });
                    if *provider != before {
                        *model = provider.default_model().into();
                        *key_env = provider.default_key_env().into();
                    }
                    ui.horizontal(|ui| {
                        ui.label("모델");
                        ui.text_edit_singleline(model);
                    });
                    ui.horizontal(|ui| {
                        ui.label("키 환경변수");
                        ui.text_edit_singleline(key_env);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Base URL");
                        ui.add(egui::TextEdit::singleline(base_url).hint_text(provider.default_base_url()));
                    });
                    ui.label("System");
                    ui.add(
                        egui::TextEdit::multiline(system)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add(
                        egui::DragValue::new(max_tokens)
                            .range(1..=200_000)
                            .prefix("max_tokens "),
                    );
                    let mut use_t = temperature.is_some();
                    ui.checkbox(&mut use_t, "temperature 지정");
                    match (use_t, temperature.is_some()) {
                        (true, false) => *temperature = Some(0.7),
                        (false, true) => *temperature = None,
                        _ => {}
                    }
                    if let Some(t) = temperature {
                        ui.add(egui::Slider::new(t, 0.0..=2.0));
                    }
                    ui.checkbox(dry_run, "Dry-run (네트워크 호출 안 함)");
                    if !*dry_run {
                        ui.colored_label(
                            Color32::from_rgb(230, 180, 60),
                            "실제 API가 호출되며 비용이 발생합니다.",
                        );
                    }
                }
                NodeKind::Mcp {
                    command,
                    args,
                    tool,
                    arguments,
                } => {
                    ui.horizontal(|ui| {
                        ui.label("명령");
                        ui.text_edit_singleline(command);
                    });
                    ui.horizontal(|ui| {
                        ui.label("인자");
                        ui.text_edit_singleline(args);
                    });
                    ui.label(RichText::new("예: 명령 cmd / 인자 /c npx -y 패키지명 (Windows의 npx)").weak());
                    ui.horizontal(|ui| {
                        ui.label("도구 이름");
                        ui.text_edit_singleline(tool);
                    });
                    ui.label("arguments (JSON, {{input}} 치환)");
                    ui.add(
                        egui::TextEdit::multiline(arguments)
                            .code_editor()
                            .desired_rows(4)
                            .desired_width(f32::INFINITY),
                    );
                }
                NodeKind::Output => {
                    ui.label("연결된 입력을 최종 결과로 수집합니다.");
                }
            }
            if let Some(run) = self.runs.get(&id) {
                ui.separator();
                match &run.state {
                    RunState::Failed(e) => {
                        ui.colored_label(Color32::from_rgb(230, 80, 80), e);
                    }
                    RunState::Done(ms) => {
                        ui.label(RichText::new(format!("출력 ({ms} ms)")).strong());
                        readonly_box(ui, "node_out", &run.output);
                    }
                    _ => {}
                }
            }
        });
    }

    fn test_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("프롬프트 테스트").strong());
            ui.separator();
            ui.label("입력 주입 노드:");
            let texts: Vec<(u32, String)> = self
                .graph
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Text { .. }))
                .map(|n| (n.id, n.title.clone()))
                .collect();
            let cur = texts
                .iter()
                .find(|(i, _)| Some(*i) == self.test_node)
                .map_or("(Text 노드 없음)".to_string(), |(_, t)| t.clone());
            egui::ComboBox::from_id_salt("test_node")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (i, t) in &texts {
                        ui.selectable_value(&mut self.test_node, Some(*i), t);
                    }
                });
            if ui
                .add_enabled(!self.running, egui::Button::new("▶ 전체 테스트"))
                .clicked()
            {
                self.run_tests(None, ctx);
            }
            if ui.button("＋ 케이스").clicked() {
                self.tests.push(TestCase {
                    name: format!("케이스 {}", self.tests.len() + 1),
                    input: String::new(),
                    expect: String::new(),
                    result: None,
                });
            }
            let pass = self
                .tests
                .iter()
                .filter(|t| t.result.as_ref().is_some_and(|r| r.pass))
                .count();
            let done = self.tests.iter().filter(|t| t.result.is_some()).count();
            if done > 0 {
                ui.label(format!("{pass}/{done} 통과"));
            }
        });
        ui.separator();
        let mut remove = None;
        let mut run_one = None;
        egui::ScrollArea::both().id_salt("tests").show(ui, |ui| {
            for (i, t) in self.tests.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(430.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(140.0));
                                if ui
                                    .add_enabled(!self.running, egui::Button::new("▶"))
                                    .on_hover_text("이 케이스만 실행")
                                    .clicked()
                                {
                                    run_one = Some(i);
                                }
                                if ui.button("🗑").clicked() {
                                    remove = Some(i);
                                }
                                match &t.result {
                                    Some(r) if r.pass => {
                                        ui.colored_label(Color32::from_rgb(80, 180, 100), format!("PASS {} ms", r.ms))
                                    }
                                    Some(r) => {
                                        ui.colored_label(Color32::from_rgb(230, 80, 80), format!("FAIL {} ms", r.ms))
                                    }
                                    None => ui.label(""),
                                };
                            });
                            ui.add(
                                egui::TextEdit::multiline(&mut t.input)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("테스트 프롬프트 입력"),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut t.expect)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("통과 조건: 최종 출력에 포함될 문자열 (선택)"),
                            );
                        });
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width().max(300.0));
                            if let Some(r) = &t.result {
                                if !r.error.is_empty() {
                                    ui.colored_label(Color32::from_rgb(230, 80, 80), &r.error);
                                }
                                readonly_box(ui, "res", &r.output);
                            }
                        });
                    });
                });
                ui.separator();
            }
        });
        if let Some(i) = remove {
            self.tests.remove(i);
        }
        if let Some(i) = run_one {
            self.run_tests(Some(i), ctx);
        }
    }
}

impl eframe::App for CanvasApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        egui::Panel::top("ctop").show(ui, |ui| self.top_bar(ui, &ctx));
        egui::Panel::bottom("ctests")
            .resizable(true)
            .default_size(230.0)
            .size_range(120.0..=520.0)
            .show(ui, |ui| self.test_panel(ui, &ctx));
        egui::Panel::right("cinspect")
            .resizable(true)
            .default_size(360.0)
            .size_range(260.0..=620.0)
            .show(ui, |ui| self.inspector(ui));
        egui::CentralPanel::default().show(ui, |ui| self.canvas(ui));
    }
}

fn readonly_box(ui: &mut egui::Ui, id: &str, text: &str) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(160.0)
        .show(ui, |ui| {
            let mut s = text;
            ui.add(
                egui::TextEdit::multiline(&mut s)
                    .code_editor()
                    .desired_width(f32::INFINITY),
            );
        });
}

fn kind_color(label: &str) -> Color32 {
    match label {
        "Text" => Color32::from_rgb(90, 90, 120),
        "Prompt" => Color32::from_rgb(120, 80, 160),
        "LLM API" => Color32::from_rgb(40, 110, 170),
        "MCP Tool" => Color32::from_rgb(30, 130, 110),
        _ => Color32::from_rgb(150, 100, 40),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn bezier_mid(p: [Pos2; 4]) -> Pos2 {
    // t = 0.5
    let mix = |a: Pos2, b: Pos2| pos2((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
    let (ab, bc, cd) = (mix(p[0], p[1]), mix(p[1], p[2]), mix(p[2], p[3]));
    mix(mix(ab, bc), mix(bc, cd))
}
