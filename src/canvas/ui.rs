//! 노드 캔버스 GUI: 중앙 캔버스(이동·확대) / 우측 인스펙터 / 하단 프롬프트 테스트 창.
//! 그래프 실행은 워커 스레드에서 하고 mpsc 채널로 이벤트만 UI에 전달한다.
//! 메시지에는 실행 세대 번호를 붙여, 새로 만들기·열기·다음 실행 뒤에 늦게 도착한 이벤트는 버린다.
//! 그래서 중지를 누른 뒤에는 실행 중인 노드(LLM·MCP 호출)가 끝나기를 기다리지 않고 그래프를 바꿀 수 있다.
//! 파일 대화상자는 위젯 처리가 끝난 프레임 끝에서 연다(app.rs와 같은 규칙).

use super::exec::{self, Event, RunOptions, RunReport};
use super::graph::{Edge, Graph, Node, NodeKind, PromptFormat, PromptTarget, Provider, TestSpec};
use crate::i18n::{self, Lang};
use eframe::egui::{self, Align2, Color32, FontId, Id, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

const NODE_W: f32 = 200.0;
const NODE_H: f32 = 78.0;
const PORT_R: f32 = 6.0;
/// 노드 카드의 보조 줄(LLM 메모, 오류)에 보여 줄 최대 글자 수. 전체는 마우스를 올리면 보인다.
const DETAIL_CHARS: usize = 30;

const AMBER: Color32 = Color32::from_rgb(230, 180, 60);
const RED: Color32 = Color32::from_rgb(230, 80, 80);
const GREEN: Color32 = Color32::from_rgb(80, 180, 100);

/// 노드 추가 메뉴의 생성 함수 타입.
type NodeCtor = fn() -> NodeKind;

#[derive(Clone, Debug, Default, PartialEq)]
enum RunState {
    #[default]
    Idle,
    Running,
    /// `note`: LLM 노드의 토큰 사용량·잘림 경고
    Done {
        ms: u128,
        note: Option<String>,
    },
    Failed(String),
    Skipped,
}

#[derive(Clone, Debug, Default)]
struct NodeRun {
    state: RunState,
    output: String,
}

#[derive(Clone, Debug)]
struct TestResult {
    pass: bool,
    output: String,
    error: String,
    ms: u128,
}

/// 워커가 UI로 보내는 메시지.
enum Msg {
    Node(Event),
    Test(usize, TestResult),
    Finished(Done),
}

/// 실행이 끝난 모습.
enum Done {
    Flow(Box<RunReport>),
    Tests { cancelled: bool },
    Error(String),
}

/// 파일 대화상자나 그래프 교체, 언어 전환은 UI 패스 안에서 하지 않고 프레임 끝으로 미룬다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Deferred {
    New,
    Open,
    Save,
    SaveAs,
    /// 화면 전체가 다음 프레임부터 한 언어로 그려지도록 프레임 끝에서 바꾼다.
    Language(Lang),
}

fn apply_event(runs: &mut HashMap<u32, NodeRun>, e: Event) {
    let (id, run) = match e {
        Event::Started(id) => (
            id,
            NodeRun {
                state: RunState::Running,
                output: String::new(),
            },
        ),
        Event::Done { id, output, ms, note } => (
            id,
            NodeRun {
                state: RunState::Done { ms, note },
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
    runs.insert(id, run);
}

/// 실행 한 번의 진행 상태. 화면과 무관해서 단위 테스트할 수 있다.
/// 모든 워커 메시지는 시작할 때 받은 세대 번호를 달고 오며, 지금 세대와 다르면 버린다.
#[derive(Default)]
struct RunBoard {
    generation: u64,
    running: bool,
    /// 중지를 눌렀고 아직 끝나지 않음
    cancelling: bool,
    runs: HashMap<u32, NodeRun>,
    final_text: String,
    /// 그래프의 테스트 케이스와 같은 길이
    test_results: Vec<Option<TestResult>>,
}

impl RunBoard {
    /// 새 실행을 시작한다. 이미 실행 중이면 None.
    fn begin(&mut self) -> Option<u64> {
        if self.running {
            return None;
        }
        self.generation += 1;
        self.running = true;
        self.cancelling = false;
        self.runs.clear();
        self.final_text.clear();
        Some(self.generation)
    }

    /// 새로 만들기·열기: 이전 그래프의 결과를 모두 지우고, 아직 오는 중인 메시지는 세대 번호로 버린다.
    fn reset(&mut self, tests: usize) {
        self.generation += 1;
        self.running = false;
        self.cancelling = false;
        self.runs.clear();
        self.final_text.clear();
        self.test_results = vec![None; tests];
    }

    /// 메시지를 반영한다. 실행이 끝났으면 상태 줄에 쓸 (문구, 오류 여부)를 돌려준다.
    fn apply(&mut self, generation: u64, msg: Msg) -> Option<(String, bool)> {
        if generation != self.generation {
            return None;
        }
        match msg {
            Msg::Node(e) => {
                apply_event(&mut self.runs, e);
                None
            }
            Msg::Test(i, r) => {
                if let Some(slot) = self.test_results.get_mut(i) {
                    *slot = Some(r);
                }
                None
            }
            Msg::Finished(done) => {
                self.running = false;
                self.cancelling = false;
                Some(match done {
                    Done::Flow(rep) => {
                        self.final_text = rep.final_text.clone();
                        flow_status(&rep)
                    }
                    Done::Tests { cancelled } => tests_status(&self.test_results, cancelled),
                    Done::Error(e) => (e, true),
                })
            }
        }
    }
}

/// 개수 앞뒤에 붙는 "노드". 영어는 1개면 단수형이다.
fn nodes_word(n: usize) -> &'static str {
    if n == 1 {
        tr!("노드", "node")
    } else {
        tr!("노드", "nodes")
    }
}

fn flow_status(rep: &RunReport) -> (String, bool) {
    let failed = rep.errors.len();
    let failed_text = || {
        trf!(
            "{failed}개 {nodes} 실패",
            "{failed} {nodes} failed",
            nodes = nodes_word(failed)
        )
    };
    if rep.cancelled {
        let skipped = rep.skipped.len();
        let mut s = trf!(
            "중지됨 · {nodes} {skipped}개 건너뜀",
            "Stopped · {skipped} {nodes} skipped",
            nodes = nodes_word(skipped)
        );
        if failed > 0 {
            s += " · ";
            s += &failed_text();
        }
        return (s, failed > 0);
    }
    if failed > 0 {
        return (failed_text(), true);
    }
    let warns = rep.notes.values().filter(|n| n.starts_with('⚠')).count();
    let ms = rep.total_ms;
    if warns > 0 {
        let word = if warns == 1 {
            tr!("경고", "warning")
        } else {
            tr!("경고", "warnings")
        };
        (
            trf!(
                "완료 ({ms} ms) · ⚠ {word} {warns}건 (노드 상태 참고)",
                "Done ({ms} ms) · ⚠ {warns} {word} (see the node status)"
            ),
            false,
        )
    } else {
        (trf!("완료 ({ms} ms)", "Done ({ms} ms)"), false)
    }
}

fn tests_status(results: &[Option<TestResult>], cancelled: bool) -> (String, bool) {
    let pass = results.iter().flatten().filter(|r| r.pass).count();
    let done = results.iter().flatten().count();
    let text = if cancelled {
        trf!(
            "테스트 중지됨 ({pass}/{done} 통과)",
            "Tests stopped ({pass}/{done} passed)"
        )
    } else {
        trf!("테스트 {pass}/{done} 통과", "Tests: {pass}/{done} passed")
    };
    (text, pass != done)
}

/// 실행 중에 열기를 막을 때의 상태 줄 문구(열기 대화상자와 파일 열기가 같이 쓴다).
fn open_while_running() -> &'static str {
    tr!(
        "실행 중에는 파일을 열 수 없습니다 (■ 중지를 누르면 바로 열 수 있습니다)",
        "Cannot open a file while running (press ■ Stop to open it right away)"
    )
}

/// 언어를 바꾼 뒤 상태 줄 문구(바뀐 언어로 쓴다). 설정 파일 저장에 실패하면 오류로 알린다.
/// 저장에 실패해도 이번 실행은 바뀐 언어로 계속한다.
fn language_status(lang: Lang, saved: Result<(), String>) -> (String, bool) {
    let name = lang.native_name();
    match saved {
        Ok(()) => (trf!("언어: {name}", "Language: {name}"), false),
        Err(e) => (
            trf!(
                "언어: {name} · 설정을 저장하지 못했습니다: {e}",
                "Language: {name} · could not save the setting: {e}"
            ),
            true,
        ),
    }
}

/// 워커 스레드가 UI로 메시지를 보내는 통로.
struct Reporter {
    tx: Sender<(u64, Msg)>,
    ctx: egui::Context,
    generation: u64,
}

impl Reporter {
    fn send(&self, msg: Msg) {
        let _ = self.tx.send((self.generation, msg));
        self.ctx.request_repaint();
    }
}

/// 작업이 패닉해도 `Finished`가 반드시 가게 해서 실행 중 상태가 남지 않게 한다.
fn run_guarded(rep: &Reporter, job: impl FnOnce(&Reporter)) {
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(rep))) {
        rep.send(Msg::Finished(Done::Error(trf!(
            "실행 중 내부 오류: {}",
            "Internal error during the run: {}",
            exec::panic_text(e)
        ))));
    }
}

/// 그래프 전체 실행 작업.
fn flow_job(rep: &Reporter, g: &Graph, opts: &RunOptions) {
    let done = match exec::run_graph(g, opts, |e| rep.send(Msg::Node(e))) {
        Ok(r) => Done::Flow(Box::new(r)),
        Err(e) => Done::Error(e),
    };
    rep.send(Msg::Finished(done));
}

/// 테스트 케이스를 차례로 실행하는 작업. 중지하면 남은 케이스는 실행하지 않고 결과도 남기지 않는다.
fn tests_job(rep: &Reporter, g: &Graph, entry: u32, cases: &[(usize, TestSpec)], base: &RunOptions) {
    let stopped = || base.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    let mut cancelled = false;
    for (i, case) in cases {
        if stopped() {
            cancelled = true;
            break;
        }
        let opts = RunOptions {
            input_override: Some((entry, case.input.clone())),
            ..base.clone()
        };
        match exec::run_graph(g, &opts, |e| rep.send(Msg::Node(e))) {
            Ok(r) if r.cancelled => {
                cancelled = true;
                break;
            }
            Ok(r) => rep.send(Msg::Test(*i, judge_case(g, &r, &case.expect))),
            Err(error) => rep.send(Msg::Test(
                *i,
                TestResult {
                    pass: false,
                    output: String::new(),
                    error,
                    ms: 0,
                },
            )),
        }
    }
    rep.send(Msg::Finished(Done::Tests { cancelled }));
}

/// 한 테스트 케이스의 통과 여부와 표시 문구.
fn judge_case(g: &Graph, r: &RunReport, expect: &str) -> TestResult {
    let mut errs: Vec<(&u32, &String)> = r.errors.iter().collect();
    errs.sort_by_key(|(id, _)| **id);
    let mut error = errs
        .iter()
        .map(|(id, e)| format!("{}: {e}", g.node(**id).map_or("?", |n| n.title.as_str())))
        .collect::<Vec<_>>()
        .join(" | ");
    let ok = r.ok();
    let pass = ok && (expect.is_empty() || r.final_text.contains(expect));
    if ok && !pass {
        error = trf!(
            "출력에 '{expect}' 가 없습니다",
            "The output does not contain '{expect}'"
        );
    }
    TestResult {
        pass,
        output: r.final_text.clone(),
        error,
        ms: r.total_ms,
    }
}

/// 입력칸마다 남아 있는 egui 자체 실행 취소 기록을 지운다.
/// 새 그래프를 열었을 때 이전 그래프의 글자가 되살아나는 것을 막는다(app.rs와 같은 방식).
fn clear_text_undo(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_by_type::<egui::text_edit::TextEditState>());
}

/// 임시 파일에 쓴 뒤 이름을 바꿔 저장한다. 도중에 멈춰도 원래 파일이 반쯤 쓰인 채로 남지 않는다.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 처음 열 때 보여 주는 예제 그래프. 내용은 만들 때의 언어로 쓰고, 두 번째 테스트 케이스는 다른 언어로 묻는다.
fn sample_graph() -> Graph {
    const QUESTION_KO: &str = "3분기 매출이 급감한 원인을 분석해줘"; // i18n-data
    const QUESTION_EN: &str = "Why did Q3 revenue drop?";
    let node = |id: u32, kind: NodeKind| Node {
        id,
        title: format!("{} {}", kind.label(), id),
        pos: [0.0; 2],
        kind,
    };
    let (question, other) = if i18n::is_en() {
        (QUESTION_EN, QUESTION_KO)
    } else {
        (QUESTION_KO, QUESTION_EN)
    };
    let mut g = Graph {
        nodes: vec![
            node(1, NodeKind::Text { text: question.into() }),
            node(2, NodeKind::new_prompt()),
            node(3, NodeKind::new_llm()),
            node(4, NodeKind::Output),
        ],
        edges: vec![
            Edge { from: 1, to: 2 },
            Edge { from: 2, to: 3 },
            Edge { from: 3, to: 4 },
        ],
        tests: vec![
            TestSpec {
                name: tr!("기본", "Basic").into(),
                input: question.into(),
                expect: "dry-run".into(),
            },
            TestSpec {
                name: tr!("영문 입력", "Korean input").into(),
                input: other.into(),
                expect: String::new(),
            },
        ],
        test_node: Some(1),
    };
    let _ = g.auto_layout();
    g
}

fn default_scene_rect() -> Rect {
    Rect::from_min_size(pos2(-40.0, -40.0), vec2(1300.0, 700.0))
}

fn node_rect(pos: [f32; 2]) -> Rect {
    Rect::from_min_size(pos2(pos[0], pos[1]), vec2(NODE_W, NODE_H))
}

/// 모든 노드를 감싸는 영역(화면 맞춤에 쓴다). 노드가 없으면 None.
fn graph_bounds(g: &Graph) -> Option<Rect> {
    g.nodes.iter().map(|n| node_rect(n.pos)).reduce(|a, b| a.union(b))
}

/// 노드 카드에 보여 줄 실행 상태.
struct StatusView {
    border: Color32,
    badge: String,
    detail: String,
    detail_color: Color32,
    tooltip: Option<String>,
}

fn status_view(run: Option<&NodeRun>) -> StatusView {
    let idle = || StatusView {
        border: Color32::from_gray(90),
        badge: String::new(),
        detail: String::new(),
        detail_color: Color32::from_gray(150),
        tooltip: None,
    };
    let Some(run) = run else { return idle() };
    match &run.state {
        RunState::Idle => idle(),
        RunState::Running => StatusView {
            border: AMBER,
            badge: tr!("실행 중…", "Running…").into(),
            ..idle()
        },
        RunState::Done { ms, note } => {
            let warn = note.as_deref().is_some_and(|n| n.starts_with('⚠'));
            StatusView {
                border: if warn { AMBER } else { GREEN },
                badge: format!("✔ {ms} ms"),
                detail: note.as_deref().map(|n| clip(n, DETAIL_CHARS)).unwrap_or_default(),
                detail_color: if warn { AMBER } else { Color32::from_gray(150) },
                tooltip: note.clone(),
            }
        }
        RunState::Failed(e) => StatusView {
            border: Color32::from_rgb(220, 70, 70),
            badge: tr!("✖ 실패", "✖ Failed").into(),
            detail: clip(e.lines().next().unwrap_or_default(), DETAIL_CHARS),
            detail_color: RED,
            tooltip: Some(e.clone()),
        },
        RunState::Skipped => StatusView {
            border: Color32::from_gray(110),
            badge: tr!("건너뜀", "Skipped").into(),
            ..idle()
        },
    }
}

pub struct CanvasApp {
    graph: Graph,
    path: Option<PathBuf>,
    selected: Option<u32>,
    /// 캔버스가 보여 주는 장면 영역(Scene이 이동·확대에 따라 바꾼다)
    scene_rect: Rect,
    /// 다음 프레임에 모든 노드가 보이게 맞춘다
    want_fit: bool,
    link_from: Option<u32>,
    board: RunBoard,
    /// 현재 실행의 중지 신호
    cancel: Arc<AtomicBool>,
    allow_mcp: bool,
    allow_custom_endpoint: bool,
    status: String,
    status_err: bool,
    deferred: Option<Deferred>,
    tx: Sender<(u64, Msg)>,
    rx: Receiver<(u64, Msg)>,
}

impl CanvasApp {
    pub fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>) -> Self {
        crate::app::install_korean_font(&cc.egui_ctx);
        let mut app = CanvasApp::from_graph(sample_graph());
        if let Some(p) = file {
            app.load(p, &cc.egui_ctx);
        }
        app
    }

    fn from_graph(graph: Graph) -> Self {
        let (tx, rx) = channel();
        let mut app = CanvasApp {
            board: RunBoard {
                test_results: vec![None; graph.tests.len()],
                ..RunBoard::default()
            },
            graph,
            path: None,
            selected: None,
            scene_rect: default_scene_rect(),
            want_fit: true,
            link_from: None,
            cancel: Arc::new(AtomicBool::new(false)),
            allow_mcp: false,
            allow_custom_endpoint: false,
            status: tr!("준비됨", "Ready").into(),
            status_err: false,
            deferred: None,
            tx,
            rx,
        };
        app.fix_test_node();
        app
    }

    fn set_status(&mut self, s: impl Into<String>, err: bool) {
        self.status = s.into();
        self.status_err = err;
    }

    /// 테스트 입력 노드가 유효한 Text 노드를 가리키도록 보정한다.
    fn fix_test_node(&mut self) {
        let is_text = |g: &Graph, id: u32| g.node(id).is_some_and(|n| matches!(n.kind, NodeKind::Text { .. }));
        if !self.graph.test_node.is_some_and(|id| is_text(&self.graph, id)) {
            self.graph.test_node = self
                .graph
                .nodes
                .iter()
                .find(|n| matches!(n.kind, NodeKind::Text { .. }))
                .map(|n| n.id);
        }
    }

    /// 테스트 케이스 수와 결과 칸 수를 맞춘다.
    fn sync_test_results(&mut self) {
        let n = self.graph.tests.len();
        if self.board.test_results.len() != n {
            self.board.test_results.resize(n, None);
        }
    }

    // ---------- 파일 ----------

    /// 그래프를 통째로 교체한다: 선택·연결 중인 선, 실행 결과, 테스트 결과, 이동·확대, 신뢰 설정을 모두 초기화한다.
    fn install(&mut self, g: Graph, path: Option<PathBuf>, ctx: &egui::Context) {
        self.graph = g;
        self.path = path;
        self.selected = None;
        self.link_from = None;
        self.allow_mcp = false; // 불러온 그래프의 MCP 명령·엔드포인트는 매번 다시 허용해야 한다
        self.allow_custom_endpoint = false;
        self.board.reset(self.graph.tests.len());
        self.fix_test_node();
        self.scene_rect = default_scene_rect();
        self.want_fit = true;
        clear_text_undo(ctx);
    }

    /// 새로 만들기·열기로 그래프를 바꿀 수 있는가. 실행 중이어도 중지를 눌렀으면 된다: "■ 중지"는 실행 중인 노드
    /// (최대 수 분 걸리는 LLM·MCP 호출)가 끝나야 멈추지만, 그래프를 바꾸면 세대 번호가 올라가 옛 워커가 늦게 보내는
    /// 이벤트는 모두 버려진다(RunBoard::apply).
    fn can_replace_graph(&self) -> bool {
        !self.board.running || self.board.cancelling
    }

    /// 그래프를 바꾼 뒤 상태 줄에 덧붙일 말. 중지 중이던 실행을 버렸으면 알린다.
    fn abandoned_note(&self) -> &'static str {
        if self.board.running {
            tr!(
                " · 중지 중이던 실행의 결과는 버립니다",
                " · results of the run being stopped are discarded"
            )
        } else {
            ""
        }
    }

    fn new_graph(&mut self, ctx: &egui::Context) {
        if !self.can_replace_graph() {
            self.set_status(
                tr!(
                    "실행 중에는 새로 만들 수 없습니다 (■ 중지를 누르면 바로 할 수 있습니다)",
                    "Cannot create a new graph while running (press ■ Stop to do it right away)"
                ),
                true,
            );
            return;
        }
        let note = self.abandoned_note();
        self.install(Graph::default(), None, ctx);
        self.set_status(trf!("새 그래프{note}", "New graph{note}"), false);
    }

    fn load(&mut self, p: PathBuf, ctx: &egui::Context) {
        if !self.can_replace_graph() {
            self.set_status(open_while_running(), true);
            return;
        }
        match std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|s| Graph::from_json(&s))
        {
            Ok(g) => {
                let msg = trf!("불러옴: {}{}", "Opened: {}{}", p.display(), self.abandoned_note());
                self.install(g, Some(p), ctx);
                self.set_status(msg, false);
            }
            Err(e) => self.set_status(trf!("불러오기 실패: {e}", "Failed to open: {e}"), true),
        }
    }

    /// 열기·저장 대화상자. 열려 있는 파일의 폴더에서 시작한다.
    #[cfg(not(test))]
    fn file_dialog(&self) -> rfd::FileDialog {
        let d = rfd::FileDialog::new().add_filter("canvas", &["json"]);
        match self.path.as_deref().and_then(Path::parent) {
            Some(dir) if !dir.as_os_str().is_empty() => d.set_directory(dir),
            _ => d,
        }
    }

    /// 열 파일을 고른다. 사용자가 취소하면 None. 단위 테스트에서는 창을 띄우지 않고 취소로 본다.
    fn pick_open_path(&self) -> Option<PathBuf> {
        #[cfg(test)]
        {
            None
        }
        #[cfg(not(test))]
        {
            self.file_dialog().pick_file()
        }
    }

    /// 저장할 파일을 고른다. 사용자가 취소하면 None. 단위 테스트에서는 창을 띄우지 않고 취소로 본다.
    fn pick_save_path(&self) -> Option<PathBuf> {
        #[cfg(test)]
        {
            None
        }
        #[cfg(not(test))]
        {
            self.file_dialog().set_file_name("flow.canvas.json").save_file()
        }
    }

    fn save(&mut self, as_new: bool) {
        let target = if as_new || self.path.is_none() {
            self.pick_save_path()
        } else {
            self.path.clone()
        };
        let Some(p) = target else { return };
        match self.graph.to_json().and_then(|s| write_atomic(&p, &s)) {
            Ok(()) => {
                self.set_status(trf!("저장됨: {}", "Saved: {}", p.display()), false);
                self.path = Some(p);
            }
            Err(e) => self.set_status(trf!("저장 실패: {e}", "Failed to save: {e}"), true),
        }
    }

    /// 프레임 끝에서 호출된다. 대화상자는 여기서만 연다.
    fn run_deferred(&mut self, d: Deferred, ctx: &egui::Context) {
        match d {
            Deferred::New => self.new_graph(ctx),
            Deferred::Open => {
                if !self.can_replace_graph() {
                    self.set_status(open_while_running(), true);
                } else if let Some(p) = self.pick_open_path() {
                    self.load(p, ctx);
                }
            }
            Deferred::Save => self.save(false),
            Deferred::SaveAs => self.save(true),
            Deferred::Language(lang) => self.switch_language(lang, ctx),
        }
    }

    /// 화면 언어를 바꾸고 설정 파일에 저장한다(스튜디오와 같은 설정이라 다음 실행에도 쓴다).
    /// 고정 문구는 다음 프레임부터 새 언어로 그려진다. 노드에 적힌 글자는 사용자 내용이라 그대로 둔다.
    fn switch_language(&mut self, lang: Lang, ctx: &egui::Context) {
        if lang == i18n::current() {
            return;
        }
        i18n::set(lang);
        let (text, err) = language_status(lang, i18n::save_default(lang));
        self.set_status(text, err);
        ctx.request_repaint();
    }

    // ---------- 편집 ----------

    fn add_node(&mut self, kind: NodeKind) {
        let off = (self.graph.nodes.len() % 8) as f32 * 24.0;
        let c = self.scene_rect.center();
        let pos = [c.x - NODE_W / 2.0 + off, c.y - NODE_H / 2.0 + off];
        match self.graph.add_node(kind, pos) {
            Ok(id) => {
                self.selected = Some(id);
                self.fix_test_node();
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn duplicate_selected(&mut self) {
        let Some(id) = self.selected else { return };
        match self.graph.duplicate_node(id) {
            Ok(copy) => {
                self.selected = Some(copy);
                self.set_status(
                    trf!(
                        "노드 {id}을(를) {copy}(으)로 복제했습니다 (연결은 복사되지 않습니다)",
                        "Duplicated node {id} as {copy} (connections are not copied)"
                    ),
                    false,
                );
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn delete_node(&mut self, id: u32) {
        if self.board.running {
            self.set_status(
                tr!(
                    "실행 중에는 노드를 삭제할 수 없습니다",
                    "Cannot delete nodes while running"
                ),
                true,
            );
            return;
        }
        self.graph.remove_node(id);
        self.board.runs.remove(&id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        if self.link_from == Some(id) {
            self.link_from = None; // 선을 끌던 노드가 사라지면 연결 상태를 놓는다
        }
        self.fix_test_node();
    }

    fn remove_test(&mut self, i: usize) {
        if self.board.running || i >= self.graph.tests.len() {
            return;
        }
        self.graph.tests.remove(i);
        if i < self.board.test_results.len() {
            self.board.test_results.remove(i);
        }
    }

    fn auto_layout(&mut self) {
        match self.graph.auto_layout() {
            Ok(()) => {
                self.want_fit = true;
                self.set_status(tr!("자동 정렬했습니다", "Nodes rearranged"), false);
            }
            Err(e) => self.set_status(e, true),
        }
    }

    // ---------- 실행 ----------

    fn begin_run(&mut self) -> Option<(u64, Arc<AtomicBool>)> {
        if self.board.running {
            return None;
        }
        if self.graph.nodes.is_empty() {
            self.set_status(tr!("실행할 노드가 없습니다", "There are no nodes to run"), true);
            return None;
        }
        if let Err(e) = self.graph.topo_order() {
            self.set_status(e, true);
            return None;
        }
        let generation = self.board.begin()?;
        self.cancel = Arc::new(AtomicBool::new(false));
        Some((generation, Arc::clone(&self.cancel)))
    }

    fn run_options(&self, cancel: Arc<AtomicBool>) -> RunOptions {
        RunOptions {
            allow_mcp: self.allow_mcp,
            allow_custom_endpoint: self.allow_custom_endpoint,
            input_override: None,
            // 상대 data_path는 열었거나 저장한 그래프 파일의 폴더 기준이다.
            base_dir: self.path.as_deref().and_then(exec::base_dir_of),
            cancel: Some(cancel),
        }
    }

    /// 워커 스레드에서 작업을 돌린다. 스레드를 못 만들면 실행 중 상태를 바로 되돌린다.
    fn spawn(&mut self, ctx: &egui::Context, generation: u64, job: impl FnOnce(&Reporter) + Send + 'static) {
        let rep = Reporter {
            tx: self.tx.clone(),
            ctx: ctx.clone(),
            generation,
        };
        let started = std::thread::Builder::new()
            .name("canvas-run".into())
            .spawn(move || run_guarded(&rep, job));
        if let Err(e) = started {
            self.board.running = false;
            self.set_status(
                trf!(
                    "실행 스레드를 만들지 못했습니다: {e}",
                    "Could not start the run thread: {e}"
                ),
                true,
            );
        }
    }

    fn run_flow(&mut self, ctx: &egui::Context) {
        let Some((generation, cancel)) = self.begin_run() else {
            return;
        };
        self.set_status(tr!("실행 중…", "Running…"), false);
        let (g, opts) = (self.graph.clone(), self.run_options(cancel));
        self.spawn(ctx, generation, move |rep| flow_job(rep, &g, &opts));
    }

    /// `only`가 Some이면 해당 케이스만, None이면 전부 순차 실행한다.
    fn run_tests(&mut self, only: Option<usize>, ctx: &egui::Context) {
        let Some(entry) = self.graph.test_node else {
            self.set_status(
                tr!(
                    "테스트 입력으로 쓸 Text 노드가 없습니다",
                    "There is no Text node to take the test input"
                ),
                true,
            );
            return;
        };
        let count = self.graph.tests.len();
        let idxs: Vec<usize> = match only {
            Some(i) if i < count => vec![i],
            Some(_) => return,
            None => (0..count).collect(),
        };
        if idxs.is_empty() {
            self.set_status(tr!("테스트 케이스가 없습니다", "There are no test cases"), true);
            return;
        }
        let Some((generation, cancel)) = self.begin_run() else {
            return;
        };
        for &i in &idxs {
            if let Some(slot) = self.board.test_results.get_mut(i) {
                *slot = None;
            }
        }
        self.set_status(tr!("테스트 실행 중…", "Running tests…"), false);
        let cases: Vec<(usize, TestSpec)> = idxs.iter().map(|&i| (i, self.graph.tests[i].clone())).collect();
        let (g, opts) = (self.graph.clone(), self.run_options(cancel));
        self.spawn(ctx, generation, move |rep| tests_job(rep, &g, entry, &cases, &opts));
    }

    fn request_cancel(&mut self) {
        if self.board.running && !self.board.cancelling {
            self.cancel.store(true, Ordering::Relaxed);
            self.board.cancelling = true;
            self.set_status(
                tr!(
                    "중지하는 중… 실행 중인 노드가 끝나면 멈춥니다 (새로 만들기·열기는 바로 할 수 있습니다)",
                    "Stopping… the run halts when the running node finishes (New and Open work right away)"
                ),
                false,
            );
        }
    }

    fn poll(&mut self) {
        while let Ok((generation, msg)) = self.rx.try_recv() {
            if let Some((text, err)) = self.board.apply(generation, msg) {
                self.set_status(text, err);
            }
        }
    }

    // ---------- 화면 ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return; // 글자를 입력하는 중에는 단축키를 쓰지 않는다
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)) {
            self.duplicate_selected();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Delete))
            && let Some(id) = self.selected
        {
            self.delete_node(id);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.board.running;
        let replaceable = self.can_replace_graph();
        let busy = tr!(
            "실행 중에는 쓸 수 없습니다. ■ 중지를 누르면 바로 쓸 수 있습니다",
            "Not available while running. Press ■ Stop to use it right away"
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(replaceable, egui::Button::new(tr!("새로 만들기", "New")))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.deferred = Some(Deferred::New);
            }
            if ui
                .add_enabled(replaceable, egui::Button::new(tr!("열기", "Open")))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.deferred = Some(Deferred::Open);
            }
            if ui.button(tr!("저장", "Save")).clicked() {
                self.deferred = Some(Deferred::Save);
            }
            if ui.button(tr!("다른 이름으로", "Save as")).clicked() {
                self.deferred = Some(Deferred::SaveAs);
            }
            ui.separator();
            ui.menu_button(tr!("＋ 노드 추가", "＋ Add node"), |ui| {
                let kinds: [(&str, NodeCtor); 5] = [
                    (tr!("Text (입력)", "Text (input)"), NodeKind::new_text),
                    (tr!("Prompt (컴파일러)", "Prompt (compiler)"), NodeKind::new_prompt),
                    ("LLM API", NodeKind::new_llm),
                    ("MCP Tool", NodeKind::new_mcp),
                    ("Output", || NodeKind::Output),
                ];
                for (label, make) in kinds {
                    if ui.button(label).clicked() {
                        self.add_node(make());
                        ui.close();
                    }
                }
            });
            if ui
                .add_enabled(self.selected.is_some(), egui::Button::new(tr!("복제", "Duplicate")))
                .on_hover_text(tr!(
                    "선택한 노드를 복제합니다 (Ctrl+D). 연결은 복사되지 않습니다",
                    "Duplicates the selected node (Ctrl+D). Connections are not copied"
                ))
                .clicked()
            {
                self.duplicate_selected();
            }
            if ui
                .button(tr!("자동 정렬", "Auto layout"))
                .on_hover_text(tr!(
                    "연결 순서대로 왼쪽에서 오른쪽으로 노드를 다시 배치합니다",
                    "Rearranges the nodes from left to right in connection order"
                ))
                .clicked()
            {
                self.auto_layout();
            }
            if ui
                .button(tr!("화면 맞춤", "Fit view"))
                .on_hover_text(tr!(
                    "모든 노드가 보이게 확대·이동합니다 (휠: 이동, Ctrl+휠: 확대/축소)",
                    "Zooms and pans so that every node is visible (wheel: pan, Ctrl+wheel: zoom)"
                ))
                .clicked()
            {
                self.want_fit = true;
            }
            ui.separator();
            ui.checkbox(&mut self.allow_mcp, tr!("MCP 실행 허용", "Allow MCP execution"))
                .on_hover_text(tr!(
                    "MCP 노드는 지정한 명령을 로컬에서 실행합니다. 신뢰하는 그래프에서만 켜세요.",
                    "MCP nodes run their command on this computer. Turn this on only for graphs you trust."
                ));
            ui.checkbox(
                &mut self.allow_custom_endpoint,
                tr!("사용자 지정 엔드포인트 허용", "Allow custom endpoint"),
            )
            .on_hover_text(tr!(
                "LLM 노드의 Base URL이 제공자 기본 주소와 다르면 API 키가 그 주소로 전송됩니다. \
                 신뢰하는 그래프에서만 켜세요. https만 허용하며 localhost는 http도 됩니다.",
                "If an LLM node's Base URL differs from the provider's default address, the API key is sent \
                 to that address. Turn this on only for graphs you trust. Only https is allowed, plus http \
                 for localhost."
            ));
            if running {
                if ui
                    .add_enabled(!self.board.cancelling, egui::Button::new(tr!("■ 중지", "■ Stop")))
                    .on_hover_text(tr!(
                        "실행 중인 노드가 끝나면 남은 노드를 건너뛰고 멈춥니다. 누른 뒤에는 기다리지 않고 \
                         새로 만들기·열기를 할 수 있습니다",
                        "Stops once the running node finishes and skips the remaining nodes. After pressing \
                         it you can use New and Open without waiting"
                    ))
                    .clicked()
                {
                    self.request_cancel();
                }
                ui.spinner();
            } else if ui.button(tr!("▶ 전체 실행", "▶ Run all")).clicked() {
                self.run_flow(ctx);
            }
            ui.separator();
            self.language_picker(ui);
            ui.separator();
            let color = if self.status_err {
                RED
            } else {
                ui.visuals().text_color()
            };
            // 상태 줄이 여러 줄로 커지지 않게 한 줄로 줄이고, 전체 문구는 마우스를 올리면 보여 준다.
            let one_line = self.status.split_whitespace().collect::<Vec<_>>().join(" ");
            let shown = clip(&one_line, 140);
            let label = ui.label(RichText::new(&shown).color(color));
            if shown != self.status {
                label.on_hover_text(&self.status);
            }
        });
    }

    /// 상단 막대의 언어 선택. 고르면 프레임 끝에서 바꾼다(`Deferred::Language`).
    fn language_picker(&mut self, ui: &mut egui::Ui) {
        let current = i18n::current();
        let mut picked = current;
        ui.label(tr!("언어", "Language"));
        egui::ComboBox::from_id_salt("canvas_lang")
            .selected_text(current.native_name())
            .show_ui(ui, |ui| {
                for lang in Lang::ALL {
                    ui.selectable_value(&mut picked, lang, lang.native_name());
                }
            })
            .response
            .on_hover_text(tr!(
                "화면 언어 (다음 실행에도 쓰도록 설정에 저장합니다)",
                "Display language (saved in the settings for next time)"
            ));
        if picked != current {
            self.deferred = Some(Deferred::Language(picked));
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(24));
        if self.want_fit {
            self.want_fit = false;
            if let Some(b) = graph_bounds(&self.graph) {
                self.scene_rect = b.expand(40.0);
            }
        }
        let mut rect = self.scene_rect;
        let out = egui::Scene::new()
            .zoom_range(0.25..=1.6)
            .show(ui, &mut rect, |ui| self.draw(ui));
        self.scene_rect = rect;
        if out.response.clicked() {
            self.selected = None;
        }
        if self.graph.nodes.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                tr!("‘＋ 노드 추가’로 시작하세요", "Start with ‘＋ Add node’"),
                FontId::proportional(16.0),
                Color32::from_gray(120),
            );
        }
    }

    /// Scene 안(장면 좌표)에서 노드·간선·포트를 그리고 상호작용을 처리한다.
    fn draw(&mut self, ui: &mut egui::Ui) {
        paint_grid(ui);
        let running = self.board.running;
        let views: Vec<StatusView> = self
            .graph
            .nodes
            .iter()
            .map(|n| status_view(self.board.runs.get(&n.id)))
            .collect();
        let mut delete = None;
        let mut duplicate = None;

        // 1) 노드 몸통: 끌어서 옮기고, 눌러서 선택하고, 우클릭 메뉴를 연다.
        for (idx, view) in views.iter().enumerate() {
            let id = self.graph.nodes[idx].id;
            let r = ui.interact(
                node_rect(self.graph.nodes[idx].pos),
                Id::unique(("cc-node", id)),
                Sense::click_and_drag(),
            );
            if r.dragged() {
                let d = r.drag_delta(); // 확대 배율이 이미 반영된 장면 좌표 이동량
                let n = &mut self.graph.nodes[idx];
                n.pos = [n.pos[0] + d.x, n.pos[1] + d.y];
            }
            if r.clicked() || r.drag_started() {
                self.selected = Some(id);
            }
            r.context_menu(|ui| {
                if ui.button(tr!("복제", "Duplicate")).clicked() {
                    duplicate = Some(id);
                    ui.close();
                }
                if ui
                    .add_enabled(!running, egui::Button::new(tr!("삭제", "Delete")))
                    .clicked()
                {
                    delete = Some(id);
                    ui.close();
                }
            });
            if let Some(tip) = view.tooltip.as_deref() {
                r.on_hover_text(tip);
            }
        }

        // 2) 옮긴 위치로 간선과 노드를 그리고, 포트와 간선 삭제 버튼의 상호작용을 처리한다.
        let rects: HashMap<u32, Rect> = self.graph.nodes.iter().map(|n| (n.id, node_rect(n.pos))).collect();
        let pointer = pointer_in_scene(ui);
        let painter = ui.painter().clone();

        let mut remove_edge = None;
        for (i, e) in self.graph.edges.iter().enumerate() {
            let (Some(a), Some(b)) = (rects.get(&e.from), rects.get(&e.to)) else {
                continue;
            };
            let (p0, p3) = (a.right_center(), b.left_center());
            let dx = ((p3.x - p0.x).abs() * 0.5).max(40.0);
            let pts = [p0, p0 + vec2(dx, 0.0), p3 - vec2(dx, 0.0), p3];
            let on_selected = self.selected.is_some_and(|s| s == e.from || s == e.to);
            let (width, color) = if on_selected {
                (2.5, Color32::from_rgb(170, 205, 255))
            } else {
                (2.0, Color32::from_rgb(120, 160, 220))
            };
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                pts,
                false,
                Color32::TRANSPARENT,
                Stroke::new(width, color),
            ));
            let mid = bezier_mid(pts);
            let r = ui.interact(
                Rect::from_center_size(mid, vec2(16.0, 16.0)),
                Id::unique(("cc-edge", e.from, e.to)),
                Sense::click(),
            );
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
            if r.on_hover_text(tr!("연결 삭제", "Delete connection")).clicked() {
                remove_edge = Some(i);
            }
        }

        let mut connect = None;
        for (idx, node) in self.graph.nodes.iter().enumerate() {
            let rect = rects[&node.id];
            let view = &views[idx];
            let selected = self.selected == Some(node.id);
            painter.rect_filled(rect, 8.0, Color32::from_gray(40));
            let header = Rect::from_min_size(rect.min, vec2(NODE_W, 22.0));
            painter.rect_filled(header, 8.0, kind_color(node.kind.label()));
            painter.rect_stroke(
                rect,
                8.0,
                Stroke::new(
                    if selected { 3.0 } else { 1.5 },
                    if selected { Color32::WHITE } else { view.border },
                ),
                StrokeKind::Middle,
            );
            painter.text(
                header.left_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                node.kind.label(),
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            painter.text(
                rect.left_top() + vec2(10.0, 36.0),
                Align2::LEFT_CENTER,
                clip(&node.title, 26),
                FontId::proportional(13.0),
                Color32::from_gray(230),
            );
            painter.text(
                rect.left_top() + vec2(10.0, 52.0),
                Align2::LEFT_CENTER,
                &view.badge,
                FontId::proportional(11.0),
                view.border,
            );
            painter.text(
                rect.left_top() + vec2(10.0, 66.0),
                Align2::LEFT_CENTER,
                &view.detail,
                FontId::proportional(10.0),
                view.detail_color,
            );

            if node.kind.has_input() {
                painter.circle_filled(rect.left_center(), PORT_R, Color32::from_gray(200));
                // 선을 끄는 중이면 놓을 수 있는 노드를 표시한다.
                if let (Some(from), Some(p)) = (self.link_from, pointer)
                    && from != node.id
                    && rect.expand(10.0).contains(p)
                {
                    painter.rect_stroke(
                        rect.expand(4.0),
                        10.0,
                        Stroke::new(2.0, Color32::from_rgb(120, 200, 255)),
                        StrokeKind::Outside,
                    );
                }
            }
            if node.kind.has_output() {
                let c = rect.right_center();
                let pr = ui.interact(
                    Rect::from_center_size(c, vec2(18.0, 18.0)),
                    Id::unique(("cc-out", node.id)),
                    Sense::drag(),
                );
                let hot = pr.hovered() || self.link_from == Some(node.id);
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
                    self.link_from = Some(node.id);
                }
                if pr.drag_stopped() && self.link_from == Some(node.id) {
                    if let Some(p) = pointer {
                        connect = self
                            .graph
                            .nodes
                            .iter()
                            .find(|n| n.kind.has_input() && n.id != node.id && rects[&n.id].expand(10.0).contains(p))
                            .map(|n| (node.id, n.id));
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

        // 3) 반복문이 끝난 뒤에 구조를 바꾼다.
        // 버튼을 뗐는데 연결 상태가 남았거나 출발 노드가 없어졌다면 놓는다(그대로 두면 상태가 굳는다).
        if self.link_from.is_some_and(|id| !rects.contains_key(&id)) || !ui.input(|i| i.pointer.any_down()) {
            self.link_from = None;
        }
        if let Some(i) = remove_edge
            && i < self.graph.edges.len()
        {
            self.graph.edges.remove(i);
        }
        if let Some((a, b)) = connect
            && let Err(e) = self.graph.connect(a, b)
        {
            self.set_status(e, true);
        }
        if let Some(id) = duplicate {
            self.selected = Some(id);
            self.duplicate_selected();
        }
        if let Some(id) = delete {
            self.delete_node(id);
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected else {
            ui.label(tr!(
                "노드를 선택하면 설정이 표시됩니다.\n\n\
                 • 출력 포트(●)를 입력 쪽 노드로 드래그해 연결\n\
                 • 간선 중앙의 × 클릭으로 연결 삭제\n\
                 • 우클릭 메뉴 또는 Delete로 삭제, Ctrl+D로 복제\n\
                 • 빈 곳 드래그·휠로 이동, Ctrl+휠로 확대/축소\n\
                 • '화면 맞춤', '자동 정렬' 버튼으로 배치 정리",
                "Select a node to see its settings.\n\n\
                 • Drag an output port (●) onto a node to connect them\n\
                 • Click the × in the middle of an edge to delete it\n\
                 • Delete with the right-click menu or Delete, duplicate with Ctrl+D\n\
                 • Drag empty space or use the wheel to pan, Ctrl+wheel to zoom\n\
                 • Tidy up with the 'Fit view' and 'Auto layout' buttons"
            ));
            if !self.board.final_text.is_empty() {
                ui.separator();
                ui.label(RichText::new(tr!("최종 출력", "Final output")).strong());
                readonly_box(ui, "final_out", &self.board.final_text);
            }
            return;
        };
        let upstream = self.graph.upstream(id);
        let ups: Vec<String> = upstream
            .iter()
            .filter_map(|u| self.graph.node(*u).map(|n| n.title.clone()))
            .collect();
        let allow_custom = self.allow_custom_endpoint;
        let Some(node) = self.graph.nodes.iter_mut().find(|n| n.id == id) else {
            return;
        };
        // 노드마다 입력칸 ID가 달라야 한다. 같으면 모든 노드가 하나의 실행 취소 기록을 공유해
        // 다른 노드에서 Ctrl+Z를 누르면 그 노드의 글자가 되살아난다.
        ui.push_id(id, |ui| {
            egui::ScrollArea::vertical().id_salt("inspector").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr!("이름", "Name"));
                    ui.text_edit_singleline(&mut node.title);
                });
                ui.label(
                    RichText::new(trf!(
                        "종류: {}   입력: {}",
                        "Kind: {}   Inputs: {}",
                        node.kind.label(),
                        if ups.is_empty() {
                            tr!("없음", "none").into()
                        } else {
                            ups.join(", ")
                        }
                    ))
                    .weak(),
                );
                ui.separator();
                match &mut node.kind {
                    NodeKind::Text { text } => {
                        ui.label(tr!("텍스트", "Text"));
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
                        ui.label(tr!(
                            "지시문 ({{input}} = 업스트림 출력)",
                            "Instruction ({{input}} = upstream output)"
                        ));
                        ui.add(
                            egui::TextEdit::multiline(role)
                                .desired_rows(5)
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(tr!("제약 조건 (한 줄에 하나)", "Constraints (one per line)"));
                        ui.add(
                            egui::TextEdit::multiline(constraints)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            ui.label(tr!("필수 필드", "Required fields"));
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
                            ui.label(tr!("데이터 파일", "Data file"));
                            ui.text_edit_singleline(data_path);
                        });
                        ui.label(
                            RichText::new(tr!(
                                "상대 경로는 그래프 파일이 있는 폴더 기준입니다",
                                "Relative paths start from the graph file's folder"
                            ))
                            .weak(),
                        );
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
                            // 제공자마다 받는 temperature 범위가 다르다(Anthropic은 0~1).
                            if let Some(t) = temperature.as_mut() {
                                *t = provider.clamp_temperature(*t);
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label(tr!("모델", "Model"));
                            ui.text_edit_singleline(model);
                        });
                        ui.horizontal(|ui| {
                            ui.label(tr!("키 환경변수", "Key env var"));
                            ui.text_edit_singleline(key_env);
                        });
                        if !exec::valid_key_env(key_env.trim()) {
                            ui.colored_label(
                                RED,
                                trf!(
                                    "키 환경변수 이름은 영문 대문자·숫자·밑줄이고 _API_KEY로 끝나야 합니다 (예: {})",
                                    "The key variable name must use uppercase letters, digits and underscores \
                                     and end with _API_KEY (e.g. {})",
                                    provider.default_key_env()
                                ),
                            );
                        }
                        ui.horizontal(|ui| {
                            ui.label("Base URL");
                            ui.add(egui::TextEdit::singleline(base_url).hint_text(provider.default_base_url()));
                        });
                        if !*dry_run && let Err(e) = exec::check_endpoint(*provider, base_url, allow_custom) {
                            ui.colored_label(AMBER, e);
                        }
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
                        ui.checkbox(&mut use_t, tr!("temperature 지정", "Set temperature"));
                        match (use_t, temperature.is_some()) {
                            (true, false) => *temperature = Some(0.7),
                            (false, true) => *temperature = None,
                            _ => {}
                        }
                        if let Some(t) = temperature.as_mut() {
                            ui.add(egui::Slider::new(t, 0.0..=provider.temperature_max()));
                        }
                        ui.checkbox(
                            dry_run,
                            tr!("Dry-run (네트워크 호출 안 함)", "Dry-run (no network calls)"),
                        );
                        if !*dry_run {
                            ui.colored_label(
                                AMBER,
                                tr!(
                                    "실제 API가 호출되며 비용이 발생합니다.",
                                    "The real API will be called and you will be charged."
                                ),
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
                            ui.label(tr!("명령", "Command"));
                            ui.text_edit_singleline(command);
                        });
                        ui.horizontal(|ui| {
                            ui.label(tr!("인자", "Arguments"));
                            ui.text_edit_singleline(args);
                        });
                        ui.label(
                            RichText::new(tr!(
                                "예: 명령 cmd / 인자 /c npx -y 패키지명 (Windows의 npx)",
                                "e.g. command cmd / arguments /c npx -y package-name (npx on Windows)"
                            ))
                            .weak(),
                        );
                        ui.horizontal(|ui| {
                            ui.label(tr!("도구 이름", "Tool name"));
                            ui.text_edit_singleline(tool);
                        });
                        ui.label(tr!(
                            "arguments (JSON, {{input}} 치환)",
                            "arguments (JSON, {{input}} is replaced)"
                        ));
                        ui.add(
                            egui::TextEdit::multiline(arguments)
                                .code_editor()
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                    }
                    NodeKind::Output => {
                        ui.label(tr!(
                            "연결된 입력을 최종 결과로 수집합니다.",
                            "Collects the connected inputs as the final result."
                        ));
                    }
                }
                if let Some(run) = self.board.runs.get(&id) {
                    ui.separator();
                    match &run.state {
                        RunState::Failed(e) => {
                            ui.colored_label(RED, e);
                        }
                        RunState::Done { ms, note } => {
                            ui.label(RichText::new(trf!("출력 ({ms} ms)", "Output ({ms} ms)")).strong());
                            if let Some(n) = note {
                                ui.colored_label(
                                    if n.starts_with('⚠') {
                                        AMBER
                                    } else {
                                        Color32::from_gray(150)
                                    },
                                    n,
                                );
                            }
                            readonly_box(ui, "node_out", &run.output);
                        }
                        _ => {}
                    }
                }
            });
        });
    }

    fn test_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.board.running;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("프롬프트 테스트", "Prompt tests")).strong());
            ui.separator();
            ui.label(tr!("입력 주입 노드:", "Input node:"));
            let texts: Vec<(u32, String)> = self
                .graph
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Text { .. }))
                .map(|n| (n.id, n.title.clone()))
                .collect();
            let cur = texts
                .iter()
                .find(|(i, _)| Some(*i) == self.graph.test_node)
                .map_or(tr!("(Text 노드 없음)", "(no Text node)").to_string(), |(_, t)| {
                    t.clone()
                });
            egui::ComboBox::from_id_salt("test_node")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (i, t) in &texts {
                        ui.selectable_value(&mut self.graph.test_node, Some(*i), t);
                    }
                });
            if ui
                .add_enabled(!running, egui::Button::new(tr!("▶ 전체 테스트", "▶ Run all tests")))
                .clicked()
            {
                self.run_tests(None, ctx);
            }
            if ui.button(tr!("＋ 케이스", "＋ Case")).clicked() {
                let n = self.graph.tests.len() + 1;
                self.graph.tests.push(TestSpec {
                    name: trf!("케이스 {n}", "Case {n}"),
                    ..TestSpec::default()
                });
                self.board.test_results.push(None);
            }
            let pass = self.board.test_results.iter().flatten().filter(|r| r.pass).count();
            let done = self.board.test_results.iter().flatten().count();
            if done > 0 {
                ui.label(trf!("{pass}/{done} 통과", "{pass}/{done} passed"));
            }
        });
        ui.separator();
        let mut remove = None;
        let mut run_one = None;
        egui::ScrollArea::both().id_salt("tests").show(ui, |ui| {
            for (i, t) in self.graph.tests.iter_mut().enumerate() {
                let result = self.board.test_results.get(i).and_then(Option::as_ref);
                ui.push_id(i, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(430.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(140.0));
                                if ui
                                    .add_enabled(!running, egui::Button::new("▶"))
                                    .on_hover_text(tr!("이 케이스만 실행", "Run only this case"))
                                    .clicked()
                                {
                                    run_one = Some(i);
                                }
                                if ui
                                    .add_enabled(!running, egui::Button::new("🗑"))
                                    .on_hover_text(tr!(
                                        "케이스 삭제 (실행 중에는 삭제할 수 없습니다)",
                                        "Delete the case (not possible while running)"
                                    ))
                                    .clicked()
                                {
                                    remove = Some(i);
                                }
                                match result {
                                    Some(r) if r.pass => {
                                        ui.colored_label(GREEN, format!("PASS {} ms", r.ms));
                                    }
                                    Some(r) => {
                                        ui.colored_label(RED, format!("FAIL {} ms", r.ms));
                                    }
                                    None => {
                                        ui.label("");
                                    }
                                }
                            });
                            ui.add(
                                egui::TextEdit::multiline(&mut t.input)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(tr!("테스트 프롬프트 입력", "Test prompt input")),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut t.expect)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(tr!(
                                        "통과 조건: 최종 출력에 포함될 문자열 (선택)",
                                        "Pass condition: text the final output must contain (optional)"
                                    )),
                            );
                        });
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width().max(300.0));
                            if let Some(r) = result {
                                if !r.error.is_empty() {
                                    ui.colored_label(RED, &r.error);
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
            self.remove_test(i);
        }
        if let Some(i) = run_one {
            self.run_tests(Some(i), ctx);
        }
    }

    /// 한 프레임의 전체 화면. eframe과 단위 테스트가 같은 코드를 쓴다.
    fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll();
        self.sync_test_results();
        self.shortcuts(&ctx);
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
        // 파일 대화상자, 그래프 교체, 언어 전환은 모든 위젯 처리가 끝난 뒤 실행한다.
        if let Some(d) = self.deferred.take() {
            self.run_deferred(d, &ctx);
        }
    }
}

impl eframe::App for CanvasApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// Scene 안에서 마우스 위치를 장면 좌표로 바꾼다.
fn pointer_in_scene(ui: &egui::Ui) -> Option<Pos2> {
    let p = ui.input(|i| i.pointer.interact_pos())?;
    Some(match ui.ctx().layer_transform_from_global(ui.layer_id()) {
        Some(t) => t * p,
        None => p,
    })
}

/// 보이는 영역에만 점 격자를 그린다. 멀리 줄이면 점 간격을 넓혀 개수를 제한한다.
/// 점 위치는 정수 번호로 계산한다. `x += step`로 돌리면 좌표가 아주 클 때(f32 간격이 step보다 커질 때)
/// 값이 더 늘지 않아 영원히 도는 문제가 생긴다.
fn paint_grid(ui: &egui::Ui) {
    let vis = ui.clip_rect();
    let mut step = 28.0_f32;
    while (vis.width() / step) * (vis.height() / step) > 6000.0 {
        step *= 2.0;
    }
    let (x0, y0) = ((vis.left() / step).floor() * step, (vis.top() / step).floor() * step);
    // 숫자가 아니거나 무한대면 0이 되어 아무것도 그리지 않는다.
    let count = |from: f32, to: f32| ((to - from) / step).ceil().max(0.0) as usize;
    let painter = ui.painter();
    let dot = Color32::from_gray(48);
    for j in 0..count(y0, vis.bottom()) {
        for i in 0..count(x0, vis.right()) {
            painter.circle_filled(pos2(x0 + i as f32 * step, y0 + j as f32 * step), 1.0, dot);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::with_lang;

    const SAMPLE_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json");

    fn done(id: u32) -> Event {
        Event::Done {
            id,
            output: format!("out{id}"),
            ms: 3,
            note: None,
        }
    }

    fn passed() -> TestResult {
        TestResult {
            pass: true,
            output: String::new(),
            error: String::new(),
            ms: 1,
        }
    }

    fn reporter(generation: u64) -> (Reporter, Receiver<(u64, Msg)>) {
        let (tx, rx) = channel();
        let rep = Reporter {
            tx,
            ctx: egui::Context::default(),
            generation,
        };
        (rep, rx)
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!("cc_ui_{}_{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---------- 실행 상태 판 ----------

    #[test]
    fn stale_generation_messages_are_dropped() {
        let mut b = RunBoard::default();
        let g1 = b.begin().unwrap();
        assert!(b.begin().is_none(), "실행 중에는 새 실행을 시작할 수 없다");
        assert!(b.apply(g1 + 1, Msg::Node(done(1))).is_none());
        assert!(b.runs.is_empty(), "다른 세대의 이벤트는 반영하지 않는다");
        b.apply(g1, Msg::Node(done(1)));
        assert_eq!(b.runs[&1].output, "out1");
        // 늦게 온 이전 세대의 Finished가 지금 실행을 끝내면 안 된다.
        assert!(b.apply(g1 - 1, Msg::Finished(Done::Error("옛 실행".into()))).is_none());
        assert!(b.running);
        let (text, err) = b.apply(g1, Msg::Finished(Done::Error("끝".into()))).unwrap();
        assert_eq!((text.as_str(), err, b.running), ("끝", true, false));
        assert!(b.begin().is_some(), "끝난 뒤에는 다시 시작할 수 있다");
    }

    #[test]
    fn reset_clears_results_and_discards_the_old_run() {
        let mut b = RunBoard::default();
        let g1 = b.begin().unwrap();
        b.apply(g1, Msg::Node(done(1)));
        b.test_results = vec![Some(passed())];
        b.final_text = "이전 결과".into();
        b.reset(3);
        assert!(!b.running && !b.cancelling && b.runs.is_empty() && b.final_text.is_empty());
        assert_eq!(b.test_results.len(), 3);
        assert!(b.test_results.iter().all(Option::is_none));
        // 이전 실행의 늦은 이벤트는 새 그래프에 적용되지 않는다(새 그래프는 노드 id가 1부터 다시 시작한다).
        assert!(b.apply(g1, Msg::Node(done(1))).is_none() && b.runs.is_empty());
        assert!(b.apply(g1, Msg::Finished(Done::Error("늦음".into()))).is_none());
        let g2 = b.begin().unwrap();
        assert!(g2 > g1);
        assert!(b.apply(g1, Msg::Finished(Done::Tests { cancelled: false })).is_none());
        assert!(b.running, "옛 세대의 끝 신호가 새 실행을 멈추면 안 된다");
    }

    #[test]
    fn events_map_to_node_states() {
        let mut runs = HashMap::new();
        apply_event(&mut runs, Event::Started(1));
        assert_eq!(runs[&1].state, RunState::Running);
        apply_event(
            &mut runs,
            Event::Done {
                id: 1,
                output: "결과".into(),
                ms: 7,
                note: Some("토큰 1→2".into()),
            },
        );
        assert_eq!(
            runs[&1].state,
            RunState::Done {
                ms: 7,
                note: Some("토큰 1→2".into())
            }
        );
        assert_eq!(runs[&1].output, "결과");
        apply_event(
            &mut runs,
            Event::Failed {
                id: 1,
                error: "실패".into(),
            },
        );
        assert_eq!(runs[&1].state, RunState::Failed("실패".into()));
        assert!(runs[&1].output.is_empty());
        apply_event(&mut runs, Event::Skipped(2));
        assert_eq!(runs[&2].state, RunState::Skipped);
    }

    #[test]
    fn status_lines() {
        let mut rep = RunReport {
            total_ms: 12,
            ..RunReport::default()
        };
        assert_eq!(flow_status(&rep), ("완료 (12 ms)".to_string(), false));
        rep.notes.insert(3, "토큰 1→2".into());
        assert_eq!(
            flow_status(&rep).0,
            "완료 (12 ms)",
            "경고가 아닌 메모는 상태 줄에 올리지 않는다"
        );
        rep.notes.insert(4, "⚠ 출력 토큰 한도(10)에서 잘림 (length)".into());
        let (text, err) = flow_status(&rep);
        assert!(text.contains("경고 1건") && !err, "{text}");
        rep.errors.insert(2, "x".into());
        assert_eq!(flow_status(&rep), ("1개 노드 실패".to_string(), true));
        rep.cancelled = true;
        rep.skipped = vec![5, 6];
        assert_eq!(
            flow_status(&rep),
            ("중지됨 · 노드 2개 건너뜀 · 1개 노드 실패".to_string(), true)
        );
        rep.errors.clear();
        assert_eq!(flow_status(&rep), ("중지됨 · 노드 2개 건너뜀".to_string(), false));

        let bad = TestResult {
            pass: false,
            ..passed()
        };
        let results = vec![Some(passed()), Some(bad), None];
        assert_eq!(tests_status(&results, false), ("테스트 1/2 통과".to_string(), true));
        assert_eq!(
            tests_status(&results[..1], true),
            ("테스트 중지됨 (1/1 통과)".to_string(), false)
        );
    }

    #[test]
    fn node_cards_show_latency_and_the_llm_note() {
        let idle = status_view(None);
        assert!(idle.badge.is_empty() && idle.detail.is_empty() && idle.tooltip.is_none());
        let run = |state| NodeRun {
            state,
            output: String::new(),
        };
        let ok = status_view(Some(&run(RunState::Done { ms: 12, note: None })));
        assert_eq!((ok.badge.as_str(), ok.border, ok.tooltip), ("✔ 12 ms", GREEN, None));
        let warn_note = "⚠ 출력 토큰 한도(1024)에서 잘림 (max_tokens) · 토큰 812→403";
        let warned = status_view(Some(&run(RunState::Done {
            ms: 480,
            note: Some(warn_note.into()),
        })));
        assert_eq!(warned.border, AMBER, "경고가 있으면 테두리가 노란색");
        assert_eq!(
            warned.tooltip.as_deref(),
            Some(warn_note),
            "전체 문구는 마우스를 올리면 보인다"
        );
        assert!(warned.detail.chars().count() <= DETAIL_CHARS + 1 && warned.detail.ends_with('…'));
        let usage = status_view(Some(&run(RunState::Done {
            ms: 5,
            note: Some("토큰 812→403".into()),
        })));
        assert_eq!((usage.border, usage.detail.as_str()), (GREEN, "토큰 812→403"));
        let failed = status_view(Some(&run(RunState::Failed("첫 줄 오류\n둘째 줄".into()))));
        assert_eq!(failed.badge, "✖ 실패");
        assert_eq!(failed.detail, "첫 줄 오류");
        assert_eq!(failed.tooltip.as_deref(), Some("첫 줄 오류\n둘째 줄"));
        assert_eq!(status_view(Some(&run(RunState::Skipped))).badge, "건너뜀");
        assert_eq!(status_view(Some(&run(RunState::Running))).border, AMBER);
    }

    #[test]
    fn test_case_verdicts() {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_mcp(), [0.0; 2]);
        let b = g.add(NodeKind::new_mcp(), [0.0; 2]);
        g.nodes[0].title = "첫째".into();
        g.nodes[1].title = "둘째".into();
        let mut rep = RunReport {
            final_text: "결과 dry-run 입니다".into(),
            total_ms: 9,
            ..RunReport::default()
        };
        let r = judge_case(&g, &rep, "dry-run");
        assert!(r.pass && r.error.is_empty() && r.ms == 9 && r.output == rep.final_text);
        assert!(judge_case(&g, &rep, "").pass, "통과 조건이 비면 실패만 아니면 통과");
        let r = judge_case(&g, &rep, "없는 문구");
        assert!(!r.pass && r.error == "출력에 '없는 문구' 가 없습니다", "{}", r.error);
        // 노드가 실패하면 id 순서대로 노드 이름과 함께 알린다(HashMap 순서에 흔들리지 않는다).
        rep.errors.insert(b, "나중".into());
        rep.errors.insert(a, "먼저".into());
        let r = judge_case(&g, &rep, "dry-run");
        assert!(!r.pass);
        assert_eq!(r.error, "첫째: 먼저 | 둘째: 나중");
    }

    // ---------- 워커 ----------

    #[test]
    fn a_panicking_job_still_finishes_the_run() {
        let (rep, rx) = reporter(5);
        run_guarded(&rep, |r| {
            r.send(Msg::Node(Event::Started(1)));
            panic!("터짐");
        });
        let got: Vec<(u64, Msg)> = rx.try_iter().collect();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(g, _)| *g == 5));
        let mut board = RunBoard {
            generation: 5,
            running: true,
            ..RunBoard::default()
        };
        let mut status = None;
        for (g, m) in got {
            status = board.apply(g, m).or(status);
        }
        let (text, err) = status.expect("Finished가 도착해야 한다");
        assert!(text.contains("내부 오류") && text.contains("터짐") && err, "{text}");
        assert!(!board.running, "패닉 뒤에도 실행 중 상태가 남으면 안 된다");
    }

    #[test]
    fn flow_job_reports_every_node_then_finishes() {
        let (rep, rx) = reporter(3);
        flow_job(&rep, &sample_graph(), &RunOptions::default());
        let mut board = RunBoard {
            generation: 3,
            running: true,
            ..RunBoard::default()
        };
        let mut status = None;
        for (g, m) in rx.try_iter() {
            assert_eq!(g, 3);
            status = board.apply(g, m).or(status);
        }
        assert!(!board.running);
        assert_eq!(board.runs.len(), 4);
        assert!(board.runs.values().all(|r| matches!(r.state, RunState::Done { .. })));
        assert!(board.final_text.starts_with("[dry-run]"), "{}", board.final_text);
        assert!(status.unwrap().0.starts_with("완료"));
    }

    #[test]
    fn tests_job_runs_cases_and_stops_when_cancelled() {
        let g = sample_graph();
        let cases: Vec<(usize, TestSpec)> = g.tests.iter().cloned().enumerate().collect();
        let (rep, rx) = reporter(1);
        tests_job(&rep, &g, 1, &cases, &RunOptions::default());
        let msgs: Vec<(u64, Msg)> = rx.try_iter().collect();
        let verdicts: Vec<(usize, bool)> = msgs
            .iter()
            .filter_map(|(_, m)| {
                if let Msg::Test(i, r) = m {
                    Some((*i, r.pass))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(verdicts, vec![(0, true), (1, true)]);
        assert!(matches!(
            msgs.last().unwrap().1,
            Msg::Finished(Done::Tests { cancelled: false })
        ));

        // 중지 신호가 켜져 있으면 케이스를 하나도 실행하지 않고 중지로 끝낸다.
        let (rep, rx) = reporter(2);
        let stop = RunOptions {
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..RunOptions::default()
        };
        tests_job(&rep, &g, 1, &cases, &stop);
        let msgs: Vec<(u64, Msg)> = rx.try_iter().collect();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].1, Msg::Finished(Done::Tests { cancelled: true })));
    }

    // ---------- 파일 ----------

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = TempDir::new("atomic");
        let p = dir.0.join("g.canvas.json");
        write_atomic(&p, "첫 번째").unwrap();
        write_atomic(&p, "두 번째").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "두 번째");
        let names: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["g.canvas.json".to_string()]);
        // 폴더가 없으면 실패하고 임시 파일도 남기지 않는다.
        assert!(write_atomic(&dir.0.join("없는폴더").join("x.json"), "x").is_err());
    }

    #[test]
    fn clear_text_undo_forgets_every_input_history() {
        let ctx = egui::Context::default();
        let state = egui::text_edit::TextEditState::default();
        ctx.data_mut(|d| {
            d.insert_temp(Id::unique("cc-undo-a"), state.clone());
            d.insert_temp(Id::unique("cc-undo-b"), state);
        });
        assert_eq!(ctx.data(|d| d.count::<egui::text_edit::TextEditState>()), 2);
        clear_text_undo(&ctx);
        assert_eq!(ctx.data(|d| d.count::<egui::text_edit::TextEditState>()), 0);
    }

    #[test]
    fn the_sample_graph_is_valid_and_runs() {
        let g = sample_graph();
        g.validate().unwrap();
        assert_eq!(g.test_node, Some(1));
        let r = exec::run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
        assert!(r.ok() && r.final_text.starts_with("[dry-run]"), "{:?}", r.errors);
    }

    #[test]
    fn graph_bounds_cover_every_node() {
        assert!(graph_bounds(&Graph::default()).is_none());
        let mut g = Graph::default();
        g.add(NodeKind::new_text(), [10.0, 20.0]);
        g.add(NodeKind::Output, [500.0, -30.0]);
        let b = graph_bounds(&g).unwrap();
        assert_eq!(b.min, pos2(10.0, -30.0));
        assert_eq!(b.max, pos2(500.0 + NODE_W, 20.0 + NODE_H));
    }

    // ---------- 앱 상태 ----------

    fn app_with_sample() -> (CanvasApp, egui::Context) {
        (CanvasApp::from_graph(sample_graph()), egui::Context::default())
    }

    #[test]
    fn new_open_and_delete_are_refused_while_running() {
        let (mut app, ctx) = app_with_sample();
        app.begin_run().expect("실행을 시작할 수 있어야 한다");
        assert!(app.board.running);
        let before = app.graph.nodes.len();

        app.new_graph(&ctx);
        assert_eq!(app.graph.nodes.len(), before, "실행 중에는 새로 만들 수 없다");
        assert!(app.status_err && app.status.contains("실행 중"));
        app.set_status("", false);
        app.load(PathBuf::from(SAMPLE_FILE), &ctx);
        assert!(
            app.path.is_none() && app.status_err,
            "실행 중에는 열 수 없다: {}",
            app.status
        );
        app.set_status("", false);
        app.delete_node(2);
        assert_eq!(app.graph.nodes.len(), before);
        assert!(app.status_err);
        let tests = app.graph.tests.len();
        app.remove_test(0);
        assert_eq!(app.graph.tests.len(), tests, "실행 중에는 테스트 케이스를 지울 수 없다");
        assert!(app.begin_run().is_none(), "이미 실행 중이면 또 시작하지 않는다");
        // 끝나면 다시 할 수 있다.
        app.board.running = false;
        app.delete_node(2);
        assert_eq!(app.graph.nodes.len(), before - 1);
        app.remove_test(0);
        assert_eq!(app.graph.tests.len(), tests - 1);
        assert_eq!(app.board.test_results.len(), tests - 1);
    }

    #[test]
    fn install_resets_results_view_and_trust() {
        let (mut app, ctx) = app_with_sample();
        app.board.runs.insert(1, NodeRun::default());
        app.board.final_text = "이전 출력".into();
        app.board.test_results = vec![Some(passed()), Some(passed())];
        app.selected = Some(2);
        app.link_from = Some(1);
        app.allow_mcp = true;
        app.allow_custom_endpoint = true;
        app.scene_rect = Rect::from_min_size(pos2(5000.0, 5000.0), vec2(10.0, 10.0));
        app.want_fit = false;

        app.new_graph(&ctx);
        assert!(app.graph.nodes.is_empty() && app.graph.tests.is_empty() && app.path.is_none());
        assert!(app.board.runs.is_empty() && app.board.final_text.is_empty() && app.board.test_results.is_empty());
        assert_eq!((app.selected, app.link_from), (None, None));
        assert!(
            !app.allow_mcp && !app.allow_custom_endpoint,
            "신뢰 설정은 문서마다 다시 켠다"
        );
        assert_eq!(app.scene_rect, default_scene_rect());
        assert!(app.want_fit);
    }

    #[test]
    fn opening_a_file_restores_saved_tests() {
        let (mut app, ctx) = (CanvasApp::from_graph(Graph::default()), egui::Context::default());
        app.board.runs.insert(9, NodeRun::default());
        app.load(PathBuf::from(SAMPLE_FILE), &ctx);
        assert!(!app.status_err, "{}", app.status);
        assert_eq!(app.graph.nodes.len(), 4);
        assert_eq!(app.graph.tests.len(), 2);
        assert_eq!(app.graph.tests[0].expect, "dry-run");
        assert_eq!(app.board.test_results.len(), 2);
        assert_eq!(app.graph.test_node, Some(1));
        assert!(app.board.runs.is_empty());
        let base = exec::base_dir_of(app.path.as_deref().unwrap()).unwrap();
        assert_eq!(base, Path::new(env!("CARGO_MANIFEST_DIR")).join("samples"));
        let opts = app.run_options(Arc::new(AtomicBool::new(false)));
        assert_eq!(opts.base_dir, Some(base), "상대 data_path는 그래프 파일 폴더 기준");
        // 깨진 파일은 현재 그래프를 건드리지 않는다.
        let dir = TempDir::new("bad");
        let bad = dir.0.join("bad.json");
        std::fs::write(&bad, r#"{"nodes":[],"edges":[{"from":1,"to":2}]}"#).unwrap();
        app.load(bad, &ctx);
        assert!(app.status_err && app.status.contains("불러오기 실패"));
        assert_eq!(app.graph.nodes.len(), 4);
    }

    #[test]
    fn save_writes_nodes_edges_and_tests() {
        let dir = TempDir::new("save");
        let (mut app, _ctx) = app_with_sample();
        let p = dir.0.join("out.canvas.json");
        app.path = Some(p.clone()); // 경로가 있으면 대화상자 없이 저장한다
        app.graph.tests[1].name = "고친 이름".into();
        app.save(false);
        assert!(!app.status_err, "{}", app.status);
        let back = Graph::from_json(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back.nodes.len(), 4);
        assert_eq!(back.edges.len(), 3);
        assert_eq!(back.tests, app.graph.tests, "테스트 케이스가 파일에 들어간다");
        assert_eq!(back.test_node, Some(1));
    }

    #[test]
    fn deleting_the_wire_source_releases_the_link() {
        let (mut app, _ctx) = app_with_sample();
        app.link_from = Some(2);
        app.selected = Some(2);
        app.board.runs.insert(2, NodeRun::default());
        app.delete_node(2);
        assert_eq!(app.link_from, None, "선을 끌던 노드가 사라지면 연결 상태를 놓는다");
        assert_eq!(app.selected, None);
        assert!(!app.board.runs.contains_key(&2));
        assert!(app.graph.edges.iter().all(|e| e.from != 2 && e.to != 2));
        // 다른 노드를 지워도 진행 중인 연결은 유지한다.
        app.link_from = Some(1);
        app.delete_node(4);
        assert_eq!(app.link_from, Some(1));
    }

    #[test]
    fn deleting_the_test_input_node_picks_another_text_node() {
        let mut g = sample_graph();
        let second = g.add(NodeKind::new_text(), [0.0; 2]);
        let mut app = CanvasApp::from_graph(g);
        assert_eq!(app.graph.test_node, Some(1));
        app.delete_node(1);
        assert_eq!(app.graph.test_node, Some(second));
        app.delete_node(second);
        assert_eq!(app.graph.test_node, None);
    }

    #[test]
    fn duplicate_and_add_report_id_exhaustion() {
        let mut g = Graph::default();
        g.nodes.push(Node {
            id: u32::MAX,
            title: "끝".into(),
            pos: [0.0; 2],
            kind: NodeKind::new_text(),
        });
        let mut app = CanvasApp::from_graph(g);
        app.add_node(NodeKind::Output);
        assert!(app.status_err && app.status.contains("한계"), "{}", app.status);
        assert_eq!(app.graph.nodes.len(), 1);
        app.selected = Some(u32::MAX);
        app.duplicate_selected();
        assert!(app.status_err && app.graph.nodes.len() == 1);
    }

    #[test]
    fn added_nodes_land_in_the_middle_of_the_view() {
        let mut app = CanvasApp::from_graph(Graph::default());
        app.scene_rect = Rect::from_min_size(pos2(1000.0, 2000.0), vec2(400.0, 200.0));
        app.add_node(NodeKind::new_prompt());
        let n = &app.graph.nodes[0];
        assert_eq!(n.pos, [1200.0 - NODE_W / 2.0, 2100.0 - NODE_H / 2.0]);
        assert_eq!(app.selected, Some(n.id));
        assert_eq!(
            app.graph.test_node, None,
            "Text가 아닌 노드는 테스트 입력 노드가 되지 않는다"
        );
        app.add_node(NodeKind::new_text());
        assert_eq!(app.graph.test_node, Some(2));
    }

    // ---------- 화면 없이 egui 프레임 돌리기 ----------

    /// 화면 없이 egui 프레임을 돌리는 도구. 실제 `show()` 코드가 마우스·키보드 이벤트를 받는다.
    /// 파일 대화상자가 열리는 버튼(열기·저장·다른 이름으로)은 누르지 않는다.
    struct Harness {
        ctx: egui::Context,
        app: CanvasApp,
        time: f64,
    }

    fn moved(p: Pos2) -> egui::Event {
        egui::Event::PointerMoved(p)
    }

    fn button(p: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn key(k: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key: k,
                physical_key: Some(k),
                pressed,
                repeat: false,
                modifiers,
            })
            .collect()
    }

    impl Harness {
        fn new(graph: Graph) -> Harness {
            let mut h = Harness {
                ctx: egui::Context::default(),
                app: CanvasApp::from_graph(graph),
                time: 0.0,
            };
            h.settle();
            h
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.frame_texts(events);
        }

        /// 한 프레임을 그리고 그 프레임에 그려진 글자를 모두 돌려준다.
        fn frame_texts(&mut self, events: Vec<egui::Event>) -> Vec<String> {
            self.time += 0.1;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1500.0, 900.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let app = &mut self.app;
            let out = self.ctx.run_ui(input, |ui| app.show(ui));
            let mut texts = Vec::new();
            for clipped in &out.shapes {
                collect_texts(&clipped.shape, &mut texts);
            }
            out.drop_without_applying_deltas();
            texts
        }

        fn settle(&mut self) {
            for _ in 0..4 {
                self.frame(vec![]);
            }
        }

        fn widget(&self, id: Id) -> egui::Response {
            self.ctx.read_response(id).expect("위젯이 그려져 있어야 한다")
        }

        /// 장면 좌표의 위젯 중심을 화면 좌표로 바꾼다.
        fn screen(&self, id: Id) -> Pos2 {
            let r = self.widget(id);
            match self.ctx.layer_transform_to_global(r.layer_id) {
                Some(t) => t * r.rect.center(),
                None => r.rect.center(),
            }
        }

        fn scale(&self, id: Id) -> f32 {
            let r = self.widget(id);
            self.ctx
                .layer_transform_to_global(r.layer_id)
                .map_or(1.0, |t| t.scaling)
        }

        fn node(&self, id: u32) -> Pos2 {
            self.screen(Id::unique(("cc-node", id)))
        }

        fn port(&self, id: u32) -> Pos2 {
            self.screen(Id::unique(("cc-out", id)))
        }

        fn click(&mut self, p: Pos2) {
            self.click_with(p, egui::PointerButton::Primary);
        }

        fn click_with(&mut self, p: Pos2, which: egui::PointerButton) {
            let ev = |pressed| egui::Event::PointerButton {
                pos: p,
                button: which,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![moved(p)]);
            self.frame(vec![ev(true)]);
            self.frame(vec![ev(false)]);
            self.frame(vec![]);
        }

        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.frame(vec![moved(from)]);
            self.frame(vec![button(from, true)]);
            self.frame(vec![moved(from + (to - from) * 0.5)]);
            self.frame(vec![moved(to)]);
            self.frame(vec![button(to, false)]);
            self.frame(vec![]);
        }

        fn wait_until_idle(&mut self) {
            for _ in 0..500 {
                self.frame(vec![]);
                if !self.app.board.running {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("5초 안에 실행이 끝나지 않았다");
        }

        fn text_states(&self) -> usize {
            self.ctx.data(|d| d.count::<egui::text_edit::TextEditState>())
        }
    }

    /// 빈 캔버스 위의 한 점(노드·패널과 겹치지 않는다).
    const BACKGROUND: Pos2 = pos2(40.0, 600.0);

    /// 그려진 모양에서 글자를 모은다.
    fn collect_texts(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    fn two_nodes() -> (Graph, u32, u32) {
        let mut g = Graph::default();
        let a = g.add(NodeKind::Text { text: "입력".into() }, [0.0, 0.0]);
        let b = g.add(NodeKind::Output, [300.0, 0.0]);
        (g, a, b)
    }

    #[test]
    fn clicking_selects_a_node_and_the_background_clears_it() {
        let (g, a, b) = two_nodes();
        let mut h = Harness::new(g);
        assert_eq!(h.app.selected, None);
        h.click(h.node(a));
        assert_eq!(h.app.selected, Some(a));
        h.click(h.node(b));
        assert_eq!(h.app.selected, Some(b));
        h.click(BACKGROUND);
        assert_eq!(h.app.selected, None, "빈 곳을 누르면 선택이 풀린다");
    }

    #[test]
    fn dragging_a_node_moves_it_in_scene_units() {
        let (g, a, _) = two_nodes();
        let mut h = Harness::new(g);
        let scale = h.scale(Id::unique(("cc-node", a)));
        assert!(scale > 0.25 && scale <= 1.6, "확대 배율 {scale}");
        let start = h.app.graph.nodes[0].pos;
        let from = h.node(a);
        let screen_delta = vec2(80.0, 40.0);
        h.drag(from, from + screen_delta);
        let pos = h.app.graph.nodes[0].pos;
        let (dx, dy) = (pos[0] - start[0], pos[1] - start[1]);
        // 화면에서 끈 거리를 확대 배율로 나눈 만큼 장면에서 움직인다.
        assert!(
            (dx - screen_delta.x / scale).abs() < 1.0,
            "dx={dx}, 기대={}",
            screen_delta.x / scale
        );
        assert!(
            (dy - screen_delta.y / scale).abs() < 1.0,
            "dy={dy}, 기대={}",
            screen_delta.y / scale
        );
        assert_eq!(h.app.selected, Some(a));
        // 이동은 그래프 데이터에만 남고 다른 노드는 그대로다.
        assert_eq!(h.app.graph.nodes[1].pos, [300.0, 0.0]);
    }

    #[test]
    fn dragging_from_an_output_port_connects_nodes() {
        let (g, a, b) = two_nodes();
        let mut h = Harness::new(g);
        h.drag(h.port(a), h.node(b));
        assert_eq!(h.app.graph.edges, vec![Edge { from: a, to: b }]);
        assert_eq!(h.app.link_from, None);
        assert!(!h.app.status_err, "{}", h.app.status);
        // 같은 연결을 또 만들면 중복으로 거절하고 상태 줄에 알린다.
        h.drag(h.port(a), h.node(b));
        assert_eq!(h.app.graph.edges.len(), 1);
        assert!(
            h.app.status_err && h.app.status.contains("이미 연결"),
            "{}",
            h.app.status
        );
    }

    #[test]
    fn dropping_a_wire_on_nothing_or_on_a_text_node_does_nothing() {
        let (mut g, a, b) = two_nodes();
        let t2 = g.add(NodeKind::new_text(), [0.0, 200.0]);
        let mut h = Harness::new(g);
        h.drag(h.port(a), BACKGROUND);
        assert!(h.app.graph.edges.is_empty());
        assert_eq!(h.app.link_from, None, "빈 곳에 놓아도 연결 상태가 남지 않는다");
        assert!(
            h.ctx.read_response(Id::unique(("cc-out", b))).is_none(),
            "Output 노드에는 출력 포트가 없다"
        );
        h.drag(h.port(a), h.node(t2));
        assert!(h.app.graph.edges.is_empty(), "Text에는 입력 포트가 없다");
        h.drag(h.port(a), h.node(a));
        assert!(h.app.graph.edges.is_empty(), "자기 자신에는 연결하지 않는다");
        assert_eq!(h.app.link_from, None);
    }

    #[test]
    fn the_cross_button_deletes_an_edge() {
        let (mut g, a, b) = two_nodes();
        g.connect(a, b).unwrap();
        let mut h = Harness::new(g);
        let cross = h.screen(Id::unique(("cc-edge", a, b)));
        h.click(cross);
        assert!(h.app.graph.edges.is_empty());
        assert_eq!(h.app.graph.nodes.len(), 2, "노드는 남는다");
    }

    #[test]
    fn delete_key_and_ctrl_d_work_on_the_selected_node() {
        let (g, a, b) = two_nodes();
        let mut h = Harness::new(g);
        h.click(h.node(a));
        h.frame(key(egui::Key::D, egui::Modifiers::COMMAND));
        assert_eq!(h.app.graph.nodes.len(), 3, "Ctrl+D로 복제");
        let copy = h.app.selected.expect("복제본이 선택된다");
        assert!(copy != a && copy != b);
        assert_eq!(h.app.graph.node(copy).unwrap().pos, [32.0, 32.0]);
        h.frame(key(egui::Key::Delete, egui::Modifiers::NONE));
        assert!(h.app.graph.node(copy).is_none(), "Delete로 삭제");
        assert_eq!(h.app.selected, None);
        assert_eq!(h.app.graph.nodes.len(), 2);
        // 선택이 없으면 아무 일도 없다.
        h.frame(key(egui::Key::Delete, egui::Modifiers::NONE));
        h.frame(key(egui::Key::D, egui::Modifiers::COMMAND));
        assert_eq!(h.app.graph.nodes.len(), 2);
    }

    #[test]
    fn right_click_menu_duplicates_the_node() {
        let (g, a, _) = two_nodes();
        let mut h = Harness::new(g);
        let on_node = h.node(a);
        h.click_with(on_node, egui::PointerButton::Secondary);
        assert!(egui::Popup::is_any_open(&h.ctx), "우클릭하면 메뉴가 열린다");
        assert_eq!(h.app.graph.nodes.len(), 2);
        // 메뉴 항목은 포인터 아래쪽에 세로로 놓인다. 그 줄을 훑어 '복제'를 누른다.
        for step in 1..12 {
            if h.app.graph.nodes.len() > 2 {
                break;
            }
            h.click(on_node + vec2(16.0, 6.0 + 8.0 * step as f32));
            if !egui::Popup::is_any_open(&h.ctx) && h.app.graph.nodes.len() == 2 {
                // 다른 곳을 눌러 닫혔다면 다시 연다.
                h.click_with(on_node, egui::PointerButton::Secondary);
            }
        }
        assert_eq!(h.app.graph.nodes.len(), 3, "메뉴의 복제가 노드를 늘린다");
        assert!(h.app.selected.is_some_and(|id| id != a), "복제본이 선택된다");
        assert!(h.app.graph.edges.is_empty());
    }

    #[test]
    fn deferred_actions_run_at_the_end_of_the_frame() {
        let mut h = Harness::new(sample_graph());
        // 열기·다른 이름으로: 대화상자를 취소하면 아무것도 바뀌지 않는다(테스트에서는 창을 띄우지 않고 취소로 본다).
        for d in [Deferred::Open, Deferred::SaveAs, Deferred::Save] {
            h.app.deferred = Some(d);
            h.frame(vec![]);
            assert!(h.app.deferred.is_none(), "{d:?}는 한 프레임에 한 번만 실행된다");
            assert_eq!(h.app.graph.nodes.len(), 4, "{d:?}");
            assert!(h.app.path.is_none() && !h.app.status_err, "{d:?}: {}", h.app.status);
        }
        // 새로 만들기는 그래프를 비우고 보이는 영역을 처음으로 돌린다.
        h.app.board.runs.insert(1, NodeRun::default());
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert!(h.app.graph.nodes.is_empty() && h.app.board.runs.is_empty());
        assert_eq!(h.app.status, "새 그래프");
        // 실행 중에는 새로 만들기도 막힌다.
        h.app.install(sample_graph(), None, &h.ctx.clone());
        h.app.board.running = true;
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert_eq!(h.app.graph.nodes.len(), 4);
        assert!(h.app.status_err);
    }

    #[test]
    fn huge_coordinates_do_not_hang_the_canvas() {
        // 파일에 아주 큰 좌표가 있어도(f32 간격이 격자 간격보다 커진다) 격자 그리기가 끝나야 한다.
        let mut g = Graph::default();
        g.add(NodeKind::new_text(), [1.0e9, 1.0e9]);
        g.add(NodeKind::Output, [-3.0e9, 2.0e9]);
        let mut h = Harness::new(g);
        h.settle();
        h.app.want_fit = true;
        h.settle();
        assert!(h.app.scene_rect.is_finite());
    }

    #[test]
    fn delete_key_is_ignored_while_running() {
        let (g, a, _) = two_nodes();
        let mut h = Harness::new(g);
        h.click(h.node(a));
        h.app.board.running = true;
        h.frame(key(egui::Key::Delete, egui::Modifiers::NONE));
        assert_eq!(h.app.graph.nodes.len(), 2, "실행 중에는 삭제하지 않는다");
        assert!(h.app.status_err);
        h.app.board.running = false;
        h.frame(key(egui::Key::Delete, egui::Modifiers::NONE));
        assert_eq!(h.app.graph.nodes.len(), 1);
    }

    #[test]
    fn zoom_changes_the_view_and_fit_restores_it() {
        let (g, a, _) = two_nodes();
        let mut h = Harness::new(g);
        let node = Id::unique(("cc-node", a));
        let fitted = h.app.scene_rect;
        let bounds = graph_bounds(&h.app.graph).unwrap().expand(40.0);
        assert_eq!(fitted, bounds, "처음에는 모든 노드가 보이게 맞춘다");
        // 노드 두 개는 작아서 처음부터 최대 배율(1.6)로 맞춰진다.
        assert!((h.scale(node) - 1.6).abs() < 1e-3, "{}", h.scale(node));
        h.frame(vec![moved(pos2(500.0, 300.0))]);
        h.frame(vec![egui::Event::Zoom(0.5)]);
        let zoomed_out = h.scale(node);
        assert!(zoomed_out < 1.0, "축소하면 배율이 줄어든다: {zoomed_out}");
        assert!(
            h.app.scene_rect.width() > fitted.width() * 1.2,
            "축소하면 보이는 장면 영역이 늘어난다: {fitted:?} -> {:?}",
            h.app.scene_rect
        );
        // 확대·축소에는 한도가 있다.
        h.frame(vec![egui::Event::Zoom(100.0)]);
        assert!(
            (h.scale(node) - 1.6).abs() < 1e-3,
            "최대 배율을 넘지 않는다: {}",
            h.scale(node)
        );
        h.frame(vec![egui::Event::Zoom(0.0001)]);
        assert!(
            (h.scale(node) - 0.25).abs() < 1e-3,
            "최소 배율 아래로 내려가지 않는다: {}",
            h.scale(node)
        );
        h.settle();
        h.app.want_fit = true;
        h.frame(vec![]);
        assert_eq!(
            h.app.scene_rect, bounds,
            "화면 맞춤은 모든 노드가 보이는 상태로 되돌린다"
        );
        assert!(!h.app.want_fit);
        assert!((h.scale(node) - 1.6).abs() < 1e-3);
    }

    #[test]
    fn auto_layout_button_logic_rearranges_and_refits() {
        let mut g = Graph::default();
        let t = g.add(NodeKind::Text { text: "a".into() }, [900.0, 900.0]);
        let o = g.add(NodeKind::Output, [-500.0, -500.0]);
        g.connect(t, o).unwrap();
        let mut h = Harness::new(g);
        h.app.auto_layout();
        assert!(h.app.want_fit && !h.app.status_err);
        assert_eq!(h.app.graph.node(t).unwrap().pos, [0.0, 0.0]);
        assert_eq!(h.app.graph.node(o).unwrap().pos, [super::super::graph::LAYOUT_COL, 0.0]);
        h.frame(vec![]);
        assert_eq!(h.app.scene_rect, graph_bounds(&h.app.graph).unwrap().expand(40.0));
    }

    #[test]
    fn inspector_inputs_have_a_separate_id_per_node() {
        // 입력칸 ID가 노드마다 같으면 실행 취소 기록(TextEditState)을 모든 노드가 공유한다.
        let mut g = Graph::default();
        let a = g.add(NodeKind::Text { text: "가".into() }, [0.0, 0.0]);
        let b = g.add(NodeKind::Text { text: "나".into() }, [300.0, 0.0]);
        let mut h = Harness::new(g);
        h.app.selected = Some(a);
        h.settle();
        let with_a = h.text_states();
        h.app.selected = Some(b);
        h.settle();
        let with_b = h.text_states();
        // 인스펙터의 입력칸은 이름과 텍스트 둘이다.
        assert_eq!(
            with_b - with_a,
            2,
            "다른 노드는 자기 입력칸 ID(=실행 취소 기록)를 가져야 한다"
        );
        h.app.selected = Some(a);
        h.settle();
        assert_eq!(h.text_states(), with_b, "다시 선택하면 그 노드의 기록을 이어 쓴다");
    }

    #[test]
    fn opening_clears_the_input_undo_history() {
        let (mut g, a, _) = two_nodes();
        g.nodes[0].title = "제목".into();
        let mut h = Harness::new(g);
        h.app.selected = Some(a);
        h.settle();
        assert!(h.text_states() > 0);
        let ctx = h.ctx.clone();
        h.app.new_graph(&ctx);
        assert_eq!(
            h.text_states(),
            0,
            "새 그래프에는 이전 입력의 실행 취소 기록이 남지 않는다"
        );
    }

    #[test]
    fn full_run_through_the_worker_thread() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_flow(&ctx);
        assert!(h.app.board.running, "실행을 시작하면 바로 실행 중 상태가 된다");
        h.wait_until_idle();
        assert!(!h.app.status_err, "{}", h.app.status);
        assert!(h.app.status.starts_with("완료"), "{}", h.app.status);
        assert!(h.app.board.final_text.starts_with("[dry-run]"));
        assert_eq!(h.app.board.runs.len(), 4);
        assert!(
            h.app
                .board
                .runs
                .values()
                .all(|r| matches!(r.state, RunState::Done { .. }))
        );
        // 끝난 뒤에는 인스펙터가 최종 출력을 보여 주고 다시 실행할 수 있다.
        h.app.selected = None;
        h.settle();
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert!(h.app.status.starts_with("완료"));
    }

    #[test]
    fn full_test_run_reports_pass_counts() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_tests(None, &ctx);
        h.wait_until_idle();
        assert_eq!(h.app.status, "테스트 2/2 통과");
        assert!(!h.app.status_err);
        assert!(
            h.app
                .board
                .test_results
                .iter()
                .all(|r| r.as_ref().is_some_and(|r| r.pass))
        );
        // 통과 조건을 못 맞추는 케이스 하나만 다시 실행한다.
        h.app.graph.tests[1].expect = "절대 없는 문구".into();
        h.app.run_tests(Some(1), &ctx);
        h.wait_until_idle();
        assert_eq!(h.app.status, "테스트 1/2 통과");
        assert!(h.app.status_err);
        let r = h.app.board.test_results[1].as_ref().unwrap();
        assert!(!r.pass && r.error.contains("절대 없는 문구"));
        assert!(
            h.app.board.test_results[0].as_ref().unwrap().pass,
            "다른 케이스의 결과는 그대로"
        );
        // Text 노드가 없으면 테스트를 시작하지 않는다.
        h.app.graph.test_node = None;
        h.app.run_tests(None, &ctx);
        assert!(h.app.status_err && !h.app.board.running);
    }

    #[test]
    fn late_results_of_a_replaced_graph_are_ignored() {
        // 실행 중이던 그래프가 교체되면(새 그래프도 노드 id가 1부터 시작한다) 늦게 오는 이벤트를 버려야 한다.
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_flow(&ctx);
        h.app.install(Graph::default(), None, &ctx);
        assert!(!h.app.board.running);
        std::thread::sleep(std::time::Duration::from_millis(300)); // 워커가 끝까지 메시지를 보낸다
        h.settle();
        assert!(
            h.app.board.runs.is_empty(),
            "옛 실행의 노드 상태가 새 그래프에 붙으면 안 된다"
        );
        assert!(h.app.board.final_text.is_empty());
        assert!(!h.app.board.running);
        assert!(!h.app.status.starts_with("완료"), "{}", h.app.status);
        // 교체 뒤의 새 실행은 정상으로 돈다.
        let mut g = Graph::default();
        g.add(
            NodeKind::Text {
                text: "새 그래프".into(),
            },
            [0.0; 2],
        );
        h.app.install(g, None, &ctx);
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert_eq!(h.app.board.final_text, "새 그래프");
    }

    /// 리뷰 재현: "■ 중지"는 노드 사이에서만 멈추므로, 막혀 있는 LLM·MCP 호출(최대 120~900초) 동안 새로 만들기·열기가
    /// 계속 막혀 있었다. 중지를 누르면 바로 그래프를 바꿀 수 있고, 옛 워커가 늦게 보낸 이벤트는 새 그래프에 닿지 않는다.
    #[test]
    fn new_and_open_are_allowed_once_stop_is_requested() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        // 막혀 있는 호출을 흉내 낸다: 실행을 시작만 하고, 옛 워커의 메시지는 나중에 그 세대 번호로 직접 보낸다.
        let (old, flag) = h.app.begin_run().unwrap();
        assert!(!h.app.can_replace_graph());
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert_eq!(h.app.graph.nodes.len(), 4, "중지를 누르기 전에는 새로 만들 수 없다");
        assert!(h.app.status_err && h.app.status.contains("■ 중지"), "{}", h.app.status);

        h.app.request_cancel();
        assert!(flag.load(Ordering::Relaxed) && h.app.can_replace_graph());
        h.frame(vec![]); // 중지 중에도 화면은 그려진다(새로 만들기·열기 버튼이 켜진다)
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert!(h.app.graph.nodes.is_empty(), "중지를 누른 뒤에는 새로 만들 수 있다");
        assert!(!h.app.board.running && !h.app.board.cancelling);
        assert!(
            !h.app.status_err && h.app.status.starts_with("새 그래프"),
            "{}",
            h.app.status
        );
        assert!(h.app.status.contains("버립니다"), "{}", h.app.status);

        // 새 그래프도 노드 id가 1부터 시작한다. 옛 워커가 늦게 보낸 이벤트·테스트 결과·끝 신호는 모두 버린다.
        h.app.add_node(NodeKind::Text {
            text: "새 입력".into()
        });
        let status = h.app.status.clone();
        let late = Reporter {
            tx: h.app.tx.clone(),
            ctx: ctx.clone(),
            generation: old,
        };
        late.send(Msg::Node(Event::Started(1)));
        late.send(Msg::Node(done(1)));
        late.send(Msg::Test(0, passed()));
        late.send(Msg::Finished(Done::Flow(Box::new(RunReport {
            final_text: "옛 결과".into(),
            cancelled: true,
            ..RunReport::default()
        }))));
        h.settle();
        assert!(h.app.board.runs.is_empty() && h.app.board.final_text.is_empty());
        assert_eq!(h.app.status, status, "옛 실행의 끝 신호가 상태 줄을 바꾸면 안 된다");
        assert!(!h.app.board.running);
        // 새 그래프는 바로 실행할 수 있다(옛 중지 신호가 이어지지 않는다).
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert_eq!(h.app.board.final_text, "새 입력", "{}", h.app.status);

        // 열기도 같다: 중지 전에는 막히고, 중지를 누르면 바로 열린다.
        let (old, _) = h.app.begin_run().unwrap();
        h.app.load(PathBuf::from(SAMPLE_FILE), &ctx);
        assert!(h.app.status_err && h.app.path.is_none(), "{}", h.app.status);
        h.app.request_cancel();
        h.app.load(PathBuf::from(SAMPLE_FILE), &ctx);
        assert!(!h.app.status_err && h.app.path.is_some(), "{}", h.app.status);
        assert_eq!(h.app.graph.nodes.len(), 4);
        let late = Reporter {
            tx: h.app.tx.clone(),
            ctx: ctx.clone(),
            generation: old,
        };
        late.send(Msg::Node(done(1)));
        late.send(Msg::Finished(Done::Error("옛 실행".into())));
        h.settle();
        assert!(h.app.board.runs.is_empty() && !h.app.status_err, "{}", h.app.status);
    }

    #[test]
    fn stop_button_logic_sets_the_cancel_flag_once() {
        let (mut app, _ctx) = app_with_sample();
        app.request_cancel();
        assert!(!app.board.cancelling, "실행 중이 아니면 아무 일도 없다");
        let (_, flag) = app.begin_run().unwrap();
        assert!(!flag.load(Ordering::Relaxed));
        app.request_cancel();
        assert!(flag.load(Ordering::Relaxed) && app.board.cancelling);
        assert!(app.status.contains("중지"));
        // 끝나면 중지 표시는 사라진다.
        let g = app.board.generation;
        app.board.apply(g, Msg::Finished(Done::Error("x".into())));
        assert!(!app.board.cancelling && !app.board.running);
        // 다음 실행은 새 신호를 쓴다(이전 중지가 이어지지 않는다).
        let (_, next) = app.begin_run().unwrap();
        assert!(!next.load(Ordering::Relaxed));
    }

    #[test]
    fn empty_and_cyclic_graphs_do_not_start_a_run() {
        let mut app = CanvasApp::from_graph(Graph::default());
        assert!(app.begin_run().is_none() && !app.board.running);
        assert!(app.status_err);
        let mut g = Graph::default();
        let x = g.add(NodeKind::new_prompt(), [0.0; 2]);
        let y = g.add(NodeKind::new_prompt(), [0.0; 2]);
        g.edges = vec![Edge { from: x, to: y }, Edge { from: y, to: x }];
        let mut app = CanvasApp::from_graph(g);
        assert!(app.begin_run().is_none() && !app.board.running);
        assert!(app.status.contains("순환"), "{}", app.status);
    }

    // ---------- 영어 ----------

    fn has_hangul(s: &str) -> bool {
        s.chars()
            .any(|c| matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}'))
    }

    #[test]
    fn status_lines_cards_and_verdicts_in_english() {
        with_lang(Lang::En, || {
            let mut rep = RunReport {
                total_ms: 12,
                ..RunReport::default()
            };
            assert_eq!(flow_status(&rep), ("Done (12 ms)".to_string(), false));
            rep.notes.insert(3, "tokens 1→2".into());
            assert_eq!(flow_status(&rep).0, "Done (12 ms)");
            rep.notes
                .insert(4, "⚠ Truncated at the output token limit (10) (length)".into());
            assert_eq!(flow_status(&rep).0, "Done (12 ms) · ⚠ 1 warning (see the node status)");
            rep.notes.insert(5, "⚠ x".into());
            assert_eq!(flow_status(&rep).0, "Done (12 ms) · ⚠ 2 warnings (see the node status)");
            rep.errors.insert(2, "x".into());
            assert_eq!(flow_status(&rep), ("1 node failed".to_string(), true));
            rep.errors.insert(6, "y".into());
            assert_eq!(flow_status(&rep).0, "2 nodes failed");
            rep.cancelled = true;
            rep.skipped = vec![7];
            assert_eq!(
                flow_status(&rep),
                ("Stopped · 1 node skipped · 2 nodes failed".to_string(), true)
            );
            rep.errors.clear();
            rep.skipped = vec![7, 8];
            assert_eq!(flow_status(&rep), ("Stopped · 2 nodes skipped".to_string(), false));

            let bad = TestResult {
                pass: false,
                ..passed()
            };
            let results = vec![Some(passed()), Some(bad), None];
            assert_eq!(tests_status(&results, false), ("Tests: 1/2 passed".to_string(), true));
            assert_eq!(
                tests_status(&results[..1], true),
                ("Tests stopped (1/1 passed)".to_string(), false)
            );

            let run = |state| NodeRun {
                state,
                output: String::new(),
            };
            assert_eq!(status_view(Some(&run(RunState::Running))).badge, "Running…");
            assert_eq!(status_view(Some(&run(RunState::Failed("e".into())))).badge, "✖ Failed");
            assert_eq!(status_view(Some(&run(RunState::Skipped))).badge, "Skipped");
            let r = judge_case(
                &Graph::default(),
                &RunReport {
                    final_text: "out".into(),
                    ..RunReport::default()
                },
                "missing",
            );
            assert_eq!(r.error, "The output does not contain 'missing'");
        });
    }

    #[test]
    fn language_switch_status_line() {
        assert_eq!(
            with_lang(Lang::En, || language_status(Lang::En, Ok(()))),
            ("Language: English".to_string(), false)
        );
        assert_eq!(
            with_lang(Lang::Ko, || language_status(Lang::Ko, Ok(()))),
            ("언어: 한국어".to_string(), false)
        );
        assert_eq!(
            with_lang(Lang::En, || language_status(Lang::En, Err("access denied".into()))),
            (
                "Language: English · could not save the setting: access denied".to_string(),
                true
            )
        );
        // 이미 쓰는 언어를 고르면 아무것도 바꾸지 않는다(전역 언어도, 설정 파일도 건드리지 않는다).
        let mut h = Harness::new(sample_graph());
        let before = h.app.status.clone();
        h.app.deferred = Some(Deferred::Language(i18n::current()));
        h.frame(vec![]);
        assert!(h.app.deferred.is_none());
        assert_eq!((h.app.status.as_str(), h.app.status_err), (before.as_str(), false));
    }

    #[test]
    fn app_messages_and_the_sample_graph_in_english() {
        with_lang(Lang::En, || {
            let (mut app, ctx) = app_with_sample();
            assert_eq!(app.status, "Ready");
            // 예제 그래프는 만들 때의 언어로 쓴다. 두 번째 케이스는 다른 언어(한국어) 입력이다.
            assert!(matches!(&app.graph.nodes[0].kind, NodeKind::Text { text } if text == "Why did Q3 revenue drop?"));
            assert_eq!(app.graph.tests[0].name, "Basic");
            assert_eq!(app.graph.tests[1].name, "Korean input");
            assert!(has_hangul(&app.graph.tests[1].input));
            let r = exec::run_graph(&app.graph, &RunOptions::default(), |_| {}).unwrap();
            assert!(r.ok(), "{:?}", r.errors);
            assert!(
                r.final_text
                    .starts_with("[dry-run] Anthropic/claude-sonnet-4-5 max_tokens=1024 prompt "),
                "{}",
                r.final_text
            );

            app.begin_run().unwrap();
            app.new_graph(&ctx);
            assert_eq!(
                app.status,
                "Cannot create a new graph while running (press ■ Stop to do it right away)"
            );
            app.load(PathBuf::from(SAMPLE_FILE), &ctx);
            assert_eq!(
                app.status,
                "Cannot open a file while running (press ■ Stop to open it right away)"
            );
            app.delete_node(2);
            assert_eq!(app.status, "Cannot delete nodes while running");
            app.request_cancel();
            assert!(app.status.starts_with("Stopping…"), "{}", app.status);
            app.new_graph(&ctx);
            assert_eq!(app.status, "New graph · results of the run being stopped are discarded");
            assert!(app.begin_run().is_none());
            assert_eq!(app.status, "There are no nodes to run");
            app.run_tests(None, &ctx);
            assert_eq!(app.status, "There is no Text node to take the test input");

            // 저장된 그래프의 글자(사용자 내용)는 영어로 열어도 그대로다.
            app.load(PathBuf::from(SAMPLE_FILE), &ctx);
            assert!(app.status.starts_with("Opened: "), "{}", app.status);
            assert!(matches!(&app.graph.nodes[1].kind, NodeKind::Prompt { role, .. } if role == "분석: {{input}}"));
            assert_eq!(app.graph.tests[0].name, "기본");
            app.selected = Some(2);
            app.duplicate_selected();
            assert_eq!(app.status, "Duplicated node 2 as 5 (connections are not copied)");
            assert_eq!(app.graph.node(5).unwrap().title, "p copy");
            app.auto_layout();
            assert_eq!(app.status, "Nodes rearranged");
            app.graph.tests.clear();
            app.run_tests(None, &ctx);
            assert_eq!(app.status, "There are no test cases");
        });
    }

    /// 영어 화면에는 사용자 내용 말고는 한국어가 남지 않는다. 노드 종류마다 인스펙터를 열고,
    /// 실행 결과·테스트 결과·우클릭 메뉴·실행 중 막대·빈 캔버스까지 그려 본다.
    #[test]
    fn the_english_screen_shows_no_korean() {
        with_lang(Lang::En, || {
            let mut g = Graph::default();
            let t = g.add(NodeKind::Text { text: "hello".into() }, [0.0, 0.0]);
            let p = g.add(NodeKind::new_prompt(), [300.0, 0.0]);
            let l = g.add(
                NodeKind::Llm {
                    provider: Provider::OpenAi,
                    model: "m".into(),
                    system: String::new(),
                    key_env: "bad".into(),
                    base_url: String::new(),
                    max_tokens: 10,
                    temperature: Some(0.5),
                    dry_run: true,
                },
                [600.0, 0.0],
            );
            let m = g.add(NodeKind::new_mcp(), [900.0, 0.0]);
            let o = g.add(NodeKind::Output, [1200.0, 0.0]);
            g.tests = vec![
                TestSpec {
                    name: "first".into(),
                    ..TestSpec::default()
                },
                TestSpec {
                    name: "second".into(),
                    ..TestSpec::default()
                },
            ];
            let mut h = Harness::new(g);
            for e in [
                Event::Done {
                    id: t,
                    output: "hello".into(),
                    ms: 1,
                    note: Some("tokens 1→2".into()),
                },
                Event::Started(l),
                Event::Failed {
                    id: m,
                    error: "MCP execution is not allowed".into(),
                },
                Event::Skipped(o),
            ] {
                apply_event(&mut h.app.board.runs, e);
            }
            h.app.board.final_text = "result".into();
            let failed = judge_case(&h.app.graph, &RunReport::default(), "zzz");
            h.app.board.test_results = vec![
                Some(passed()),
                Some(TestResult {
                    ms: 2,
                    output: "x".into(),
                    ..failed
                }),
            ];
            let mut seen = Vec::new();
            for sel in [None, Some(t), Some(p), Some(l), Some(m), Some(o)] {
                h.app.selected = sel;
                h.settle();
                seen.extend(h.frame_texts(vec![]));
            }
            h.app.board.running = true;
            seen.extend(h.frame_texts(vec![]));
            h.app.board.running = false;
            h.app.selected = None;
            h.settle();
            let on_node = h.node(t);
            h.click_with(on_node, egui::PointerButton::Secondary);
            assert!(egui::Popup::is_any_open(&h.ctx), "우클릭 메뉴가 열려야 한다");
            seen.extend(h.frame_texts(vec![]));
            let ctx = h.ctx.clone();
            h.app.install(Graph::default(), None, &ctx);
            h.settle();
            seen.extend(h.frame_texts(vec![]));

            let korean: Vec<&String> = seen.iter().filter(|s| has_hangul(s)).collect();
            assert!(korean.is_empty(), "영어 화면에 남은 한국어: {korean:?}");
            for want in [
                "New",
                "＋ Add node",
                "Allow MCP execution",
                "Allow custom endpoint",
                "▶ Run all",
                "■ Stop",
                "Language",
                "English",
                "Ready",
                "Select a node to see its settings.",
                "Final output",
                "Kind: LLM API   Inputs: none",
                "Instruction ({{input}} = upstream output)",
                "Analyze the following:\n{{input}}",
                "The key variable name must use uppercase letters",
                "Dry-run (no network calls)",
                "arguments (JSON, {{input}} is replaced)",
                "Collects the connected inputs as the final result.",
                "Running…",
                "✖ Failed",
                "Skipped",
                "Prompt tests",
                "Input node:",
                "▶ Run all tests",
                "PASS 1 ms",
                "FAIL 2 ms",
                "The output does not contain 'zzz'",
                "Duplicate",
                "Delete",
                "(no Text node)",
                "Start with ‘＋ Add node’",
            ] {
                assert!(seen.iter().any(|s| s.contains(want)), "{want:?} 가 그려지지 않음");
            }
        });
    }
}
