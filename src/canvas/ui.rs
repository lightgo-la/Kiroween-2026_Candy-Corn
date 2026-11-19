//! Node canvas GUI: central canvas (pan and zoom) / inspector on the right / prompt test panel at the bottom.
//! Graphs run on a worker thread, and only events are passed to the UI through an mpsc channel.
//! Messages carry the generation number of their run, so events that arrive late after New, Open or the next run
//! are dropped. That is why, after Stop is pressed, the graph can be changed without waiting for the running node
//! (an LLM or MCP call) to finish.
//! File dialogs open at the end of the frame, after the widgets are processed (the same rule as app.rs).

use super::exec::{self, Event, RunOptions, RunReport};
use super::graph::{Edge, Graph, Node, NodeKind, PromptFormat, PromptTarget, Provider, TestSpec};
use crate::i18n::{self, Lang};
use crate::keys::KeysWindow;
use eframe::egui::{self, Align2, Color32, FontId, Id, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

const NODE_W: f32 = 200.0;
const NODE_H: f32 = 78.0;
const PORT_R: f32 = 6.0;
/// Maximum number of characters on the extra line of a node card (LLM note, error). The full text shows on hover.
const DETAIL_CHARS: usize = 30;

const AMBER: Color32 = Color32::from_rgb(230, 180, 60);
const RED: Color32 = Color32::from_rgb(230, 80, 80);
const GREEN: Color32 = Color32::from_rgb(80, 180, 100);

/// Type of the constructor functions in the add node menu.
type NodeCtor = fn() -> NodeKind;

#[derive(Clone, Debug, Default, PartialEq)]
enum RunState {
    #[default]
    Idle,
    Running,
    /// `note`: token usage and truncation warnings of LLM nodes
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

/// Messages the worker sends to the UI.
enum Msg {
    Node(Event),
    Test(usize, TestResult),
    Finished(Done),
}

/// How a run ended.
enum Done {
    Flow(Box<RunReport>),
    Tests { cancelled: bool },
    Error(String),
}

/// File dialogs, graph replacement and language switches are not done inside the UI pass but deferred to the end of
/// the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Deferred {
    New,
    Open,
    Save,
    SaveAs,
    /// Switched at the end of the frame so that the whole screen is drawn in one language from the next frame on.
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

/// Progress of one run. It does not depend on the screen, so it can be unit tested.
/// Every worker message carries the generation number received at the start, and is dropped if it differs from the
/// current generation.
#[derive(Default)]
struct RunBoard {
    generation: u64,
    running: bool,
    /// Stop was pressed and the run has not finished yet
    cancelling: bool,
    runs: HashMap<u32, NodeRun>,
    final_text: String,
    /// Same length as the graph's test cases
    test_results: Vec<Option<TestResult>>,
}

impl RunBoard {
    /// Starts a new run. None if a run is already going.
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

    /// New or Open: clears all results of the previous graph; messages still on their way are dropped by their
    /// generation number.
    fn reset(&mut self, tests: usize) {
        self.generation += 1;
        self.running = false;
        self.cancelling = false;
        self.runs.clear();
        self.final_text.clear();
        self.test_results = vec![None; tests];
    }

    /// Applies a message. When the run has finished, returns (text, is error) for the status line.
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

/// The word "node" next to a count. English uses the singular for 1.
fn nodes_word(n: usize) -> &'static str {
    if n == 1 { tr!("node") } else { tr!("nodes") }
}

fn flow_status(rep: &RunReport) -> (String, bool) {
    let failed = rep.errors.len();
    let failed_text = || trf!("{failed} {nodes} failed", nodes = nodes_word(failed), failed = failed);
    if rep.cancelled {
        let skipped = rep.skipped.len();
        let mut s = trf!(
            "Stopped · {skipped} {nodes} skipped",
            nodes = nodes_word(skipped),
            skipped = skipped
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
        let word = if warns == 1 { tr!("warning") } else { tr!("warnings") };
        (
            trf!(
                "Done ({ms} ms) · ⚠ {warns} {word} (see the node status)",
                ms = ms,
                warns = warns,
                word = word
            ),
            false,
        )
    } else {
        (trf!("Done ({ms} ms)", ms = ms), false)
    }
}

fn tests_status(results: &[Option<TestResult>], cancelled: bool) -> (String, bool) {
    let pass = results.iter().flatten().filter(|r| r.pass).count();
    let done = results.iter().flatten().count();
    let text = if cancelled {
        trf!("Tests stopped ({pass}/{done} passed)", pass = pass, done = done)
    } else {
        trf!("Tests: {pass}/{done} passed", pass = pass, done = done)
    };
    (text, pass != done)
}

/// Status line text when opening is refused during a run (shared by the open dialog and opening a file).
fn open_while_running() -> &'static str {
    tr!("Cannot open a file while running (press ■ Stop to open it right away)")
}

/// Status line text after a language switch (written in the new language). A failure to save the settings file is
/// reported as an error.
/// Even if saving fails, this session continues in the new language.
fn language_status(lang: Lang, saved: Result<(), String>) -> (String, bool) {
    let name = lang.native_name();
    match saved {
        Ok(()) => (trf!("Language: {name}", name = name), false),
        Err(e) => (
            trf!("Language: {name} · could not save the setting: {e}", name = name, e = e),
            true,
        ),
    }
}

/// The channel through which the worker thread sends messages to the UI.
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

/// Makes sure `Finished` is sent even if the job panics, so that no running state is left behind.
fn run_guarded(rep: &Reporter, job: impl FnOnce(&Reporter)) {
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(rep))) {
        rep.send(Msg::Finished(Done::Error(trf!(
            "Internal error during the run: {}",
            exec::panic_text(e)
        ))));
    }
}

/// Job that runs the whole graph.
fn flow_job(rep: &Reporter, g: &Graph, opts: &RunOptions) {
    let done = match exec::run_graph(g, opts, |e| rep.send(Msg::Node(e))) {
        Ok(r) => Done::Flow(Box::new(r)),
        Err(e) => Done::Error(e),
    };
    rep.send(Msg::Finished(done));
}

/// Job that runs the test cases one after another. When stopped, the remaining cases are not run and leave no results.
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

/// Whether one test case passed, and the text to show.
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
        error = trf!("The output does not contain '{expect}'", expect = expect);
    }
    TestResult {
        pass,
        output: r.final_text.clone(),
        error,
        ms: r.total_ms,
    }
}

/// Clears egui's own undo history left in every input field.
/// This keeps the text of the previous graph from coming back after a new graph is opened (the same approach as
/// app.rs).
fn clear_text_undo(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_by_type::<egui::text_edit::TextEditState>());
}

/// Saves by writing a temporary file and then renaming it. Even if interrupted, the original file is never left half
/// written.
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

/// Sample graph shown at startup. Its content is written in the language in use when it is created, and the second
/// test case asks in the other language.
fn sample_graph() -> Graph {
    const QUESTION_KO: &str = crate::lexicon::GRAPH_SAMPLE_QUESTION_KO;
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
                name: tr!("Basic").into(),
                input: question.into(),
                expect: "dry-run".into(),
            },
            TestSpec {
                name: tr!("Korean input").into(),
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

/// The area that encloses all nodes (used to fit the view). None if there are no nodes.
fn graph_bounds(g: &Graph) -> Option<Rect> {
    g.nodes.iter().map(|n| node_rect(n.pos)).reduce(|a, b| a.union(b))
}

/// Run state shown on a node card.
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
            badge: tr!("Running…").into(),
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
            badge: tr!("✖ Failed").into(),
            detail: clip(e.lines().next().unwrap_or_default(), DETAIL_CHARS),
            detail_color: RED,
            tooltip: Some(e.clone()),
        },
        RunState::Skipped => StatusView {
            border: Color32::from_gray(110),
            badge: tr!("Skipped").into(),
            ..idle()
        },
    }
}

pub struct CanvasApp {
    graph: Graph,
    path: Option<PathBuf>,
    selected: Option<u32>,
    /// Scene area the canvas shows (the Scene changes it when panning and zooming)
    scene_rect: Rect,
    /// Fit the view to all nodes in the next frame
    want_fit: bool,
    link_from: Option<u32>,
    board: RunBoard,
    /// Stop signal of the current run
    cancel: Arc<AtomicBool>,
    allow_mcp: bool,
    allow_custom_endpoint: bool,
    /// 🔑 API keys window and the cached state of the saved keys
    keys: KeysWindow,
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
            keys: KeysWindow::load(),
            status: tr!("Ready").into(),
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

    /// Makes the test input node point to a valid Text node.
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

    /// Keeps the number of result slots equal to the number of test cases.
    fn sync_test_results(&mut self) {
        let n = self.graph.tests.len();
        if self.board.test_results.len() != n {
            self.board.test_results.resize(n, None);
        }
    }

    // ---------- Files ----------

    /// Replaces the whole graph: resets the selection, the wire being dragged, run results, test results, pan and
    /// zoom, and the trust settings.
    fn install(&mut self, g: Graph, path: Option<PathBuf>, ctx: &egui::Context) {
        self.graph = g;
        self.path = path;
        self.selected = None;
        self.link_from = None;
        self.allow_mcp = false; // MCP commands and endpoints of a loaded graph must be allowed again every time
        self.allow_custom_endpoint = false;
        self.board.reset(self.graph.tests.len());
        self.fix_test_node();
        self.scene_rect = default_scene_rect();
        self.want_fit = true;
        clear_text_undo(ctx);
    }

    /// Whether New or Open may replace the graph. During a run this is allowed once Stop was pressed: "■ Stop" only
    /// halts after the running node (an LLM or MCP call that can take minutes) finishes, but replacing the graph
    /// raises the generation number, so all events the old worker sends late are dropped (RunBoard::apply).
    fn can_replace_graph(&self) -> bool {
        !self.board.running || self.board.cancelling
    }

    /// Text appended to the status line after replacing the graph. Mentions it when a run being stopped was discarded.
    fn abandoned_note(&self) -> &'static str {
        if self.board.running {
            tr!(" · results of the run being stopped are discarded")
        } else {
            ""
        }
    }

    fn new_graph(&mut self, ctx: &egui::Context) {
        if !self.can_replace_graph() {
            self.set_status(
                tr!("Cannot create a new graph while running (press ■ Stop to do it right away)"),
                true,
            );
            return;
        }
        let note = self.abandoned_note();
        self.install(Graph::default(), None, ctx);
        self.set_status(trf!("New graph{note}", note = note), false);
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
                let msg = trf!("Opened: {}{}", p.display(), self.abandoned_note());
                self.install(g, Some(p), ctx);
                self.set_status(msg, false);
            }
            Err(e) => self.set_status(trf!("Failed to open: {e}", e = e), true),
        }
    }

    /// Open and save dialog. Starts in the folder of the open file.
    #[cfg(not(test))]
    fn file_dialog(&self) -> rfd::FileDialog {
        let d = rfd::FileDialog::new().add_filter("canvas", &["json"]);
        match self.path.as_deref().and_then(Path::parent) {
            Some(dir) if !dir.as_os_str().is_empty() => d.set_directory(dir),
            _ => d,
        }
    }

    /// Picks the file to open. None if the user cancels. Unit tests open no window and treat it as cancelled.
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

    /// Picks the file to save to. None if the user cancels. Unit tests open no window and treat it as cancelled.
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
                self.set_status(trf!("Saved: {}", p.display()), false);
                self.path = Some(p);
            }
            Err(e) => self.set_status(trf!("Failed to save: {e}", e = e), true),
        }
    }

    /// Called at the end of the frame. Dialogs are opened only here.
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

    /// Switches the display language and saves it in the settings file (the same setting as the studio, so the next
    /// start uses it too).
    /// Fixed text is drawn in the new language from the next frame on. Text in the nodes is user content and stays as
    /// it is.
    fn switch_language(&mut self, lang: Lang, ctx: &egui::Context) {
        if lang == i18n::current() {
            return;
        }
        i18n::set(lang);
        let (text, err) = language_status(lang, i18n::save_default(lang));
        self.set_status(text, err);
        ctx.request_repaint();
    }

    // ---------- Editing ----------

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
                        "Duplicated node {id} as {copy} (connections are not copied)",
                        id = id,
                        copy = copy
                    ),
                    false,
                );
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn delete_node(&mut self, id: u32) {
        if self.board.running {
            self.set_status(tr!("Cannot delete nodes while running"), true);
            return;
        }
        self.graph.remove_node(id);
        self.board.runs.remove(&id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        if self.link_from == Some(id) {
            self.link_from = None; // if the node a wire is dragged from disappears, the link state is released
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
                self.set_status(tr!("Nodes rearranged"), false);
            }
            Err(e) => self.set_status(e, true),
        }
    }

    // ---------- Running ----------

    fn begin_run(&mut self) -> Option<(u64, Arc<AtomicBool>)> {
        if self.board.running {
            return None;
        }
        if self.graph.nodes.is_empty() {
            self.set_status(tr!("There are no nodes to run"), true);
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
            // A relative data_path starts from the folder of the graph file that was opened or saved.
            base_dir: self.path.as_deref().and_then(exec::base_dir_of),
            cancel: Some(cancel),
            saved_keys: true,
        }
    }

    /// Runs a job on a worker thread. If the thread cannot be created, the running state is reset right away.
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
            self.set_status(trf!("Could not start the run thread: {e}", e = e), true);
        }
    }

    fn run_flow(&mut self, ctx: &egui::Context) {
        let Some((generation, cancel)) = self.begin_run() else {
            return;
        };
        self.set_status(tr!("Running…"), false);
        let (g, opts) = (self.graph.clone(), self.run_options(cancel));
        self.spawn(ctx, generation, move |rep| flow_job(rep, &g, &opts));
    }

    /// If `only` is Some, runs only that case; if None, runs all cases in order.
    fn run_tests(&mut self, only: Option<usize>, ctx: &egui::Context) {
        let Some(entry) = self.graph.test_node else {
            self.set_status(tr!("There is no Text node to take the test input"), true);
            return;
        };
        let count = self.graph.tests.len();
        let idxs: Vec<usize> = match only {
            Some(i) if i < count => vec![i],
            Some(_) => return,
            None => (0..count).collect(),
        };
        if idxs.is_empty() {
            self.set_status(tr!("There are no test cases"), true);
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
        self.set_status(tr!("Running tests…"), false);
        let cases: Vec<(usize, TestSpec)> = idxs.iter().map(|&i| (i, self.graph.tests[i].clone())).collect();
        let (g, opts) = (self.graph.clone(), self.run_options(cancel));
        self.spawn(ctx, generation, move |rep| tests_job(rep, &g, entry, &cases, &opts));
    }

    fn request_cancel(&mut self) {
        if self.board.running && !self.board.cancelling {
            self.cancel.store(true, Ordering::Relaxed);
            self.board.cancelling = true;
            self.set_status(
                tr!("Stopping… the run halts when the running node finishes (New and Open work right away)"),
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

    // ---------- Screen ----------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.keys.open {
            return; // no shortcuts while text is being typed or the API keys window is open
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
        let busy = tr!("Not available while running. Press ■ Stop to use it right away");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(replaceable, egui::Button::new(tr!("New")))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.deferred = Some(Deferred::New);
            }
            if ui
                .add_enabled(replaceable, egui::Button::new(tr!("Open")))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.deferred = Some(Deferred::Open);
            }
            if ui.button(tr!("Save")).clicked() {
                self.deferred = Some(Deferred::Save);
            }
            if ui.button(tr!("Save as")).clicked() {
                self.deferred = Some(Deferred::SaveAs);
            }
            ui.separator();
            ui.menu_button(tr!("＋ Add node"), |ui| {
                let kinds: [(&str, NodeCtor); 5] = [
                    (tr!("Text (input)"), NodeKind::new_text),
                    (tr!("Prompt (compiler)"), NodeKind::new_prompt),
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
                .add_enabled(self.selected.is_some(), egui::Button::new(tr!("Duplicate")))
                .on_hover_text(tr!("Duplicates the selected node (Ctrl+D). Connections are not copied"))
                .clicked()
            {
                self.duplicate_selected();
            }
            if ui
                .button(tr!("Auto layout"))
                .on_hover_text(tr!("Rearranges the nodes from left to right in connection order"))
                .clicked()
            {
                self.auto_layout();
            }
            if ui
                .button(tr!("Fit view"))
                .on_hover_text(tr!("Zooms and pans so that every node is visible (wheel: pan, Ctrl+wheel: zoom)"))
                .clicked()
            {
                self.want_fit = true;
            }
            ui.separator();
            ui.checkbox(&mut self.allow_mcp, tr!("Allow MCP execution"))
                .on_hover_text(tr!("MCP nodes run their command on this computer. Turn this on only for graphs you trust."));
            ui.checkbox(
                &mut self.allow_custom_endpoint,
                tr!("Allow custom endpoint"),
            )
            .on_hover_text(tr!("If an LLM node's Base URL differs from the provider's default address, the API key is sent to that address. Turn this on only for graphs you trust. Only https is allowed, plus http for localhost."));
            if running {
                if ui
                    .add_enabled(!self.board.cancelling, egui::Button::new(tr!("■ Stop")))
                    .on_hover_text(tr!("Stops once the running node finishes and skips the remaining nodes. After pressing it you can use New and Open without waiting"))
                    .clicked()
                {
                    self.request_cancel();
                }
                ui.spinner();
            } else if ui.button(tr!("▶ Run all")).clicked() {
                self.run_flow(ctx);
            }
            ui.separator();
            if ui
                .button(tr!("🔑 API keys"))
                .on_hover_text(tr!("Save the API keys for the LLM nodes. Saved keys are used in this window, not by --run-graph"))
                .clicked()
            {
                self.keys.show_window();
            }
            self.language_picker(ui);
            ui.separator();
            let color = if self.status_err {
                RED
            } else {
                ui.visuals().text_color()
            };
            // Squeeze the status line into one line so it does not grow; the full text shows on hover.
            let one_line = self.status.split_whitespace().collect::<Vec<_>>().join(" ");
            let shown = clip(&one_line, 140);
            let label = ui.label(RichText::new(&shown).color(color));
            if shown != self.status {
                label.on_hover_text(&self.status);
            }
        });
    }

    /// Language picker in the top bar. A choice is applied at the end of the frame (`Deferred::Language`).
    fn language_picker(&mut self, ui: &mut egui::Ui) {
        let current = i18n::current();
        let mut picked = current;
        ui.label(tr!("Language"));
        egui::ComboBox::from_id_salt("canvas_lang")
            .selected_text(current.native_name())
            .show_ui(ui, |ui| {
                for lang in Lang::ALL {
                    ui.selectable_value(&mut picked, lang, lang.native_name());
                }
            })
            .response
            .on_hover_text(tr!("Display language (saved in the settings for next time)"));
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
                tr!("Start with ‘＋ Add node’"),
                FontId::proportional(16.0),
                Color32::from_gray(120),
            );
        }
    }

    /// Draws the nodes, edges and ports inside the Scene (scene coordinates) and handles their interaction.
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

        // 1) Node bodies: drag to move, click to select, right-click for the menu.
        for (idx, view) in views.iter().enumerate() {
            let id = self.graph.nodes[idx].id;
            let r = ui.interact(
                node_rect(self.graph.nodes[idx].pos),
                Id::unique(("cc-node", id)),
                Sense::click_and_drag(),
            );
            if r.dragged() {
                let d = r.drag_delta(); // movement in scene coordinates, with the zoom already applied
                let n = &mut self.graph.nodes[idx];
                n.pos = [n.pos[0] + d.x, n.pos[1] + d.y];
            }
            if r.clicked() || r.drag_started() {
                self.selected = Some(id);
            }
            r.context_menu(|ui| {
                if ui.button(tr!("Duplicate")).clicked() {
                    duplicate = Some(id);
                    ui.close();
                }
                if ui.add_enabled(!running, egui::Button::new(tr!("Delete"))).clicked() {
                    delete = Some(id);
                    ui.close();
                }
            });
            if let Some(tip) = view.tooltip.as_deref() {
                r.on_hover_text(tip);
            }
        }

        // 2) Draw the edges and nodes at their new positions, and handle the ports and the edge delete buttons.
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
            if r.on_hover_text(tr!("Delete connection")).clicked() {
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
                // While a wire is being dragged, highlight the node it can be dropped on.
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

        // 3) Change the structure after the loops.
        // If the button was released but the link state remains, or its source node is gone, release it (otherwise
        // the state would get stuck).
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
            ui.label(tr!("Select a node to see its settings.\n\n• Drag an output port (●) onto a node to connect them\n• Click the × in the middle of an edge to delete it\n• Delete with the right-click menu or Delete, duplicate with Ctrl+D\n• Drag empty space or use the wheel to pan, Ctrl+wheel to zoom\n• Tidy up with the 'Fit view' and 'Auto layout' buttons"));
            if !self.board.final_text.is_empty() {
                ui.separator();
                ui.label(RichText::new(tr!("Final output")).strong());
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
        let key_states = Provider::ALL.map(|p| self.keys.state(p).clone());
        let Some(node) = self.graph.nodes.iter_mut().find(|n| n.id == id) else {
            return;
        };
        // Each node needs its own input field IDs. Otherwise all nodes would share one undo history, and pressing
        // Ctrl+Z in another node would bring back that node's text.
        ui.push_id(id, |ui| {
            egui::ScrollArea::vertical().id_salt("inspector").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr!("Name"));
                    ui.text_edit_singleline(&mut node.title);
                });
                ui.label(
                    RichText::new(trf!("Kind: {}   Inputs: {}", node.kind.label(), if ups.is_empty() {
                            tr!("none").into()
                        } else {
                            ups.join(", ")
                        }))
                    .weak(),
                );
                ui.separator();
                match &mut node.kind {
                    NodeKind::Text { text } => {
                        ui.label(tr!("Text"));
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
                        ui.label(tr!("Instruction ({{input}} = upstream output)"));
                        ui.add(
                            egui::TextEdit::multiline(role)
                                .desired_rows(5)
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(tr!("Constraints (one per line)"));
                        ui.add(
                            egui::TextEdit::multiline(constraints)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            ui.label(tr!("Required fields"));
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
                            ui.label(tr!("Data file"));
                            ui.text_edit_singleline(data_path);
                        });
                        ui.label(
                            RichText::new(tr!("Relative paths start from the graph file's folder"))
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
                            // Each provider accepts a different temperature range (Anthropic takes 0~1).
                            if let Some(t) = temperature.as_mut() {
                                *t = provider.clamp_temperature(*t);
                            }
                        }
                        ui.horizontal(|ui| {
                            ui.label(tr!("Model"));
                            ui.text_edit_singleline(model);
                        });
                        ui.horizontal(|ui| {
                            ui.label(tr!("Key env var"));
                            ui.text_edit_singleline(key_env);
                        });
                        if !exec::valid_key_env(key_env.trim()) {
                            ui.colored_label(
                                RED,
                                trf!("The key variable name must use uppercase letters, digits and underscores and end with _API_KEY (e.g. {})", provider.default_key_env()),
                            );
                        } else if key_env.trim() == provider.default_key_env() {
                            let st = &key_states[Provider::ALL.iter().position(|p| *p == *provider).unwrap_or(0)];
                            let line = match &st.saved_tail {
                                Some(t) => trf!("Key: saved · {tail}", tail = t),
                                None if st.env => tr!("Key: from the environment variable").into(),
                                None => tr!("No key yet · add one in 🔑 API keys").into(),
                            };
                            ui.label(RichText::new(line).small());
                        } else {
                            ui.label(
                                RichText::new(tr!("A custom variable name reads only the environment variable")).small(),
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
                        ui.checkbox(&mut use_t, tr!("Set temperature"));
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
                            tr!("Dry-run (no network calls)"),
                        );
                        if !*dry_run {
                            ui.colored_label(
                                AMBER,
                                tr!("The real API will be called and you will be charged."),
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
                            ui.label(tr!("Command"));
                            ui.text_edit_singleline(command);
                        });
                        ui.horizontal(|ui| {
                            ui.label(tr!("Arguments"));
                            ui.text_edit_singleline(args);
                        });
                        ui.label(
                            RichText::new(tr!("e.g. command cmd / arguments /c npx -y package-name (npx on Windows)"))
                            .weak(),
                        );
                        ui.horizontal(|ui| {
                            ui.label(tr!("Tool name"));
                            ui.text_edit_singleline(tool);
                        });
                        ui.label(tr!("arguments (JSON, {{input}} is replaced)"));
                        ui.add(
                            egui::TextEdit::multiline(arguments)
                                .code_editor()
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                    }
                    NodeKind::Output => {
                        ui.label(tr!("Collects the connected inputs as the final result."));
                    }
                }
                if let Some(run) = self.board.runs.get(&id) {
                    ui.separator();
                    match &run.state {
                        RunState::Failed(e) => {
                            ui.colored_label(RED, e);
                        }
                        RunState::Done { ms, note } => {
                            ui.label(RichText::new(trf!("Output ({ms} ms)", ms = ms)).strong());
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
            ui.label(RichText::new(tr!("Prompt tests")).strong());
            ui.separator();
            ui.label(tr!("Input node:"));
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
                .map_or(tr!("(no Text node)").to_string(), |(_, t)| t.clone());
            egui::ComboBox::from_id_salt("test_node")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (i, t) in &texts {
                        ui.selectable_value(&mut self.graph.test_node, Some(*i), t);
                    }
                });
            if ui
                .add_enabled(!running, egui::Button::new(tr!("▶ Run all tests")))
                .clicked()
            {
                self.run_tests(None, ctx);
            }
            if ui.button(tr!("＋ Case")).clicked() {
                let n = self.graph.tests.len() + 1;
                self.graph.tests.push(TestSpec {
                    name: trf!("Case {n}", n = n),
                    ..TestSpec::default()
                });
                self.board.test_results.push(None);
            }
            let pass = self.board.test_results.iter().flatten().filter(|r| r.pass).count();
            let done = self.board.test_results.iter().flatten().count();
            if done > 0 {
                ui.label(trf!("{pass}/{done} passed", pass = pass, done = done));
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
                                    .on_hover_text(tr!("Run only this case"))
                                    .clicked()
                                {
                                    run_one = Some(i);
                                }
                                if ui
                                    .add_enabled(!running, egui::Button::new("🗑"))
                                    .on_hover_text(tr!("Delete the case (not possible while running)"))
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
                                    .hint_text(tr!("Test prompt input")),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut t.expect)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(tr!("Pass condition: text the final output must contain (optional)")),
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

    /// The whole screen for one frame. eframe and the unit tests use the same code.
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
        self.keys.show(&ctx);
        // File dialogs, graph replacement and language switches run after all widgets are processed.
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

/// Converts the mouse position inside the Scene to scene coordinates.
fn pointer_in_scene(ui: &egui::Ui) -> Option<Pos2> {
    let p = ui.input(|i| i.pointer.interact_pos())?;
    Some(match ui.ctx().layer_transform_from_global(ui.layer_id()) {
        Some(t) => t * p,
        None => p,
    })
}

/// Draws the dot grid only in the visible area. When zoomed far out, the dot spacing grows to limit the count.
/// Dot positions are computed from integer indices. A loop with `x += step` would never end for very large
/// coordinates (when the f32 spacing gets larger than step), because the value would stop growing.
fn paint_grid(ui: &egui::Ui) {
    let vis = ui.clip_rect();
    let mut step = 28.0_f32;
    while (vis.width() / step) * (vis.height() / step) > 6000.0 {
        step *= 2.0;
    }
    let (x0, y0) = ((vis.left() / step).floor() * step, (vis.top() / step).floor() * step);
    // NaN or infinity becomes 0, so nothing is drawn.
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

    // ---------- Run board ----------

    #[test]
    fn stale_generation_messages_are_dropped() {
        let mut b = RunBoard::default();
        let g1 = b.begin().unwrap();
        assert!(b.begin().is_none(), "a new run cannot start while one is running");
        assert!(b.apply(g1 + 1, Msg::Node(done(1))).is_none());
        assert!(b.runs.is_empty(), "events of another generation are not applied");
        b.apply(g1, Msg::Node(done(1)));
        assert_eq!(b.runs[&1].output, "out1");
        // A late Finished from an earlier generation must not end the current run.
        assert!(b.apply(g1 - 1, Msg::Finished(Done::Error("old run".into()))).is_none());
        assert!(b.running);
        let (text, err) = b.apply(g1, Msg::Finished(Done::Error("end".into()))).unwrap();
        assert_eq!((text.as_str(), err, b.running), ("end", true, false));
        assert!(b.begin().is_some(), "after it ends, a new run can start");
    }

    #[test]
    fn reset_clears_results_and_discards_the_old_run() {
        let mut b = RunBoard::default();
        let g1 = b.begin().unwrap();
        b.apply(g1, Msg::Node(done(1)));
        b.test_results = vec![Some(passed())];
        b.final_text = "previous result".into();
        b.reset(3);
        assert!(!b.running && !b.cancelling && b.runs.is_empty() && b.final_text.is_empty());
        assert_eq!(b.test_results.len(), 3);
        assert!(b.test_results.iter().all(Option::is_none));
        // Late events of the previous run are not applied to the new graph (its node ids start from 1 again).
        assert!(b.apply(g1, Msg::Node(done(1))).is_none() && b.runs.is_empty());
        assert!(b.apply(g1, Msg::Finished(Done::Error("late".into()))).is_none());
        let g2 = b.begin().unwrap();
        assert!(g2 > g1);
        assert!(b.apply(g1, Msg::Finished(Done::Tests { cancelled: false })).is_none());
        assert!(
            b.running,
            "the end signal of an old generation must not stop the new run"
        );
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
                output: "result".into(),
                ms: 7,
                note: Some("tokens 1→2".into()),
            },
        );
        assert_eq!(
            runs[&1].state,
            RunState::Done {
                ms: 7,
                note: Some("tokens 1→2".into())
            }
        );
        assert_eq!(runs[&1].output, "result");
        apply_event(
            &mut runs,
            Event::Failed {
                id: 1,
                error: "failure".into(),
            },
        );
        assert_eq!(runs[&1].state, RunState::Failed("failure".into()));
        assert!(runs[&1].output.is_empty());
        apply_event(&mut runs, Event::Skipped(2));
        assert_eq!(runs[&2].state, RunState::Skipped);
    }

    #[test]
    fn status_lines() {
        with_lang(Lang::Ko, || {
            let mut rep = RunReport {
                total_ms: 12,
                ..RunReport::default()
            };
            let done = format!(ko_text!("Done ({ms} ms)"), ms = 12);
            assert_eq!(flow_status(&rep), (done.clone(), false));
            rep.notes.insert(3, "tokens 1→2".into());
            assert_eq!(
                flow_status(&rep).0,
                done,
                "notes that are not warnings do not go to the status line"
            );
            rep.notes
                .insert(4, "⚠ Truncated at the output token limit (10) (length)".into());
            let (text, err) = flow_status(&rep);
            let warned = format!(
                ko_text!("Done ({ms} ms) · ⚠ {warns} {word} (see the node status)"),
                ms = 12,
                warns = 1,
                word = ko_text!("warning")
            );
            assert!(text == warned && !err, "{text}");
            rep.errors.insert(2, "x".into());
            let failed = format!(
                ko_text!("{failed} {nodes} failed"),
                failed = 1,
                nodes = ko_text!("node")
            );
            assert_eq!(flow_status(&rep), (failed.clone(), true));
            rep.cancelled = true;
            rep.skipped = vec![5, 6];
            let stopped = format!(
                ko_text!("Stopped · {skipped} {nodes} skipped"),
                skipped = 2,
                nodes = ko_text!("nodes")
            );
            assert_eq!(flow_status(&rep), (format!("{stopped} · {failed}"), true));
            rep.errors.clear();
            assert_eq!(flow_status(&rep), (stopped, false));

            let bad = TestResult {
                pass: false,
                ..passed()
            };
            let results = vec![Some(passed()), Some(bad), None];
            let passed_1_of_2 = format!(ko_text!("Tests: {pass}/{done} passed"), pass = 1, done = 2);
            assert_eq!(tests_status(&results, false), (passed_1_of_2, true));
            let stopped_1_of_1 = format!(ko_text!("Tests stopped ({pass}/{done} passed)"), pass = 1, done = 1);
            assert_eq!(tests_status(&results[..1], true), (stopped_1_of_1, false));
        });
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
        let warn_note = "⚠ Truncated at the output token limit (1024) (max_tokens) · tokens 812→403";
        let warned = status_view(Some(&run(RunState::Done {
            ms: 480,
            note: Some(warn_note.into()),
        })));
        assert_eq!(warned.border, AMBER, "a warning makes the border yellow");
        assert_eq!(
            warned.tooltip.as_deref(),
            Some(warn_note),
            "the full text shows on hover"
        );
        assert!(warned.detail.chars().count() <= DETAIL_CHARS + 1 && warned.detail.ends_with('…'));
        let usage = status_view(Some(&run(RunState::Done {
            ms: 5,
            note: Some("tokens 812→403".into()),
        })));
        assert_eq!((usage.border, usage.detail.as_str()), (GREEN, "tokens 812→403"));
        let failed = status_view(Some(&run(RunState::Failed("first line error\nsecond line".into()))));
        assert_eq!(failed.badge, "✖ Failed");
        assert_eq!(failed.detail, "first line error");
        assert_eq!(failed.tooltip.as_deref(), Some("first line error\nsecond line"));
        assert_eq!(status_view(Some(&run(RunState::Skipped))).badge, "Skipped");
        assert_eq!(status_view(Some(&run(RunState::Running))).border, AMBER);
    }

    #[test]
    fn test_case_verdicts() {
        let mut g = Graph::default();
        let a = g.add(NodeKind::new_mcp(), [0.0; 2]);
        let b = g.add(NodeKind::new_mcp(), [0.0; 2]);
        g.nodes[0].title = "first".into();
        g.nodes[1].title = "second".into();
        let mut rep = RunReport {
            final_text: "the result is dry-run".into(),
            total_ms: 9,
            ..RunReport::default()
        };
        let r = judge_case(&g, &rep, "dry-run");
        assert!(r.pass && r.error.is_empty() && r.ms == 9 && r.output == rep.final_text);
        assert!(
            judge_case(&g, &rep, "").pass,
            "with an empty pass condition, a case passes unless something failed"
        );
        let r = judge_case(&g, &rep, "missing text");
        assert!(
            !r.pass && r.error == "The output does not contain 'missing text'",
            "{}",
            r.error
        );
        // Failed nodes are reported with their names in id order (HashMap order does not matter).
        rep.errors.insert(b, "later".into());
        rep.errors.insert(a, "earlier".into());
        let r = judge_case(&g, &rep, "dry-run");
        assert!(!r.pass);
        assert_eq!(r.error, "first: earlier | second: later");
    }

    // ---------- Worker ----------

    #[test]
    fn a_panicking_job_still_finishes_the_run() {
        let (rep, rx) = reporter(5);
        run_guarded(&rep, |r| {
            r.send(Msg::Node(Event::Started(1)));
            panic!("boom");
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
        let (text, err) = status.expect("Finished must arrive");
        assert!(
            text.contains("Internal error") && text.contains("boom") && err,
            "{text}"
        );
        assert!(!board.running, "no running state may be left after a panic");
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
        assert!(status.unwrap().0.starts_with("Done"));
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

        // With the stop signal set, no case runs and the job ends as stopped.
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

    // ---------- Files ----------

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = TempDir::new("atomic");
        let p = dir.0.join("g.canvas.json");
        write_atomic(&p, "first").unwrap();
        write_atomic(&p, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");
        let names: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["g.canvas.json".to_string()]);
        // A missing folder fails and leaves no temporary file either.
        assert!(write_atomic(&dir.0.join("no-such-folder").join("x.json"), "x").is_err());
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

    // ---------- App state ----------

    fn app_with_sample() -> (CanvasApp, egui::Context) {
        (CanvasApp::from_graph(sample_graph()), egui::Context::default())
    }

    #[test]
    fn new_open_and_delete_are_refused_while_running() {
        let (mut app, ctx) = app_with_sample();
        app.begin_run().expect("a run must be able to start");
        assert!(app.board.running);
        let before = app.graph.nodes.len();

        app.new_graph(&ctx);
        assert_eq!(app.graph.nodes.len(), before, "no new graph while running");
        assert!(app.status_err && app.status.contains("while running"));
        app.set_status("", false);
        app.load(PathBuf::from(SAMPLE_FILE), &ctx);
        assert!(
            app.path.is_none() && app.status_err,
            "no opening while running: {}",
            app.status
        );
        app.set_status("", false);
        app.delete_node(2);
        assert_eq!(app.graph.nodes.len(), before);
        assert!(app.status_err);
        let tests = app.graph.tests.len();
        app.remove_test(0);
        assert_eq!(
            app.graph.tests.len(),
            tests,
            "test cases cannot be deleted while running"
        );
        assert!(app.begin_run().is_none(), "no second start while already running");
        // After the run ends it works again.
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
        app.board.final_text = "previous output".into();
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
            "the trust settings are turned on again for each document"
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
        assert_eq!(
            opts.base_dir,
            Some(base),
            "a relative data_path starts from the graph file's folder"
        );
        // A broken file leaves the current graph alone.
        let dir = TempDir::new("bad");
        let bad = dir.0.join("bad.json");
        std::fs::write(&bad, r#"{"nodes":[],"edges":[{"from":1,"to":2}]}"#).unwrap();
        app.load(bad, &ctx);
        assert!(app.status_err && app.status.contains("Failed to open"));
        assert_eq!(app.graph.nodes.len(), 4);
    }

    #[test]
    fn save_writes_nodes_edges_and_tests() {
        let dir = TempDir::new("save");
        let (mut app, _ctx) = app_with_sample();
        let p = dir.0.join("out.canvas.json");
        app.path = Some(p.clone()); // with a path, it saves without a dialog
        app.graph.tests[1].name = "renamed".into();
        app.save(false);
        assert!(!app.status_err, "{}", app.status);
        let back = Graph::from_json(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back.nodes.len(), 4);
        assert_eq!(back.edges.len(), 3);
        assert_eq!(back.tests, app.graph.tests, "the test cases are written to the file");
        assert_eq!(back.test_node, Some(1));
    }

    #[test]
    fn deleting_the_wire_source_releases_the_link() {
        let (mut app, _ctx) = app_with_sample();
        app.link_from = Some(2);
        app.selected = Some(2);
        app.board.runs.insert(2, NodeRun::default());
        app.delete_node(2);
        assert_eq!(
            app.link_from, None,
            "if the node a wire is dragged from disappears, the link state is released"
        );
        assert_eq!(app.selected, None);
        assert!(!app.board.runs.contains_key(&2));
        assert!(app.graph.edges.iter().all(|e| e.from != 2 && e.to != 2));
        // Deleting another node keeps the link in progress.
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
            title: "end".into(),
            pos: [0.0; 2],
            kind: NodeKind::new_text(),
        });
        let mut app = CanvasApp::from_graph(g);
        app.add_node(NodeKind::Output);
        assert!(app.status_err && app.status.contains("limit"), "{}", app.status);
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
            "a node that is not Text does not become the test input node"
        );
        app.add_node(NodeKind::new_text());
        assert_eq!(app.graph.test_node, Some(2));
    }

    // ---------- Running egui frames without a screen ----------

    /// Runs egui frames without a screen. The real `show()` code receives the mouse and keyboard events.
    /// Buttons that open a file dialog (Open, Save, Save as) are never pressed.
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

        /// Draws one frame and returns all text drawn in it.
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
            self.ctx.read_response(id).expect("the widget must have been drawn")
        }

        /// Converts the center of a widget from scene coordinates to screen coordinates.
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
            panic!("the run did not finish within 5 seconds");
        }

        fn text_states(&self) -> usize {
            self.ctx.data(|d| d.count::<egui::text_edit::TextEditState>())
        }
    }

    /// A point on the empty canvas (it overlaps no node or panel).
    const BACKGROUND: Pos2 = pos2(40.0, 600.0);

    /// Collects the text from the drawn shapes.
    fn collect_texts(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    fn two_nodes() -> (Graph, u32, u32) {
        let mut g = Graph::default();
        let a = g.add(NodeKind::Text { text: "input".into() }, [0.0, 0.0]);
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
        assert_eq!(h.app.selected, None, "clicking empty space clears the selection");
    }

    #[test]
    fn dragging_a_node_moves_it_in_scene_units() {
        let (g, a, _) = two_nodes();
        let mut h = Harness::new(g);
        let scale = h.scale(Id::unique(("cc-node", a)));
        assert!(scale > 0.25 && scale <= 1.6, "zoom {scale}");
        let start = h.app.graph.nodes[0].pos;
        let from = h.node(a);
        let screen_delta = vec2(80.0, 40.0);
        h.drag(from, from + screen_delta);
        let pos = h.app.graph.nodes[0].pos;
        let (dx, dy) = (pos[0] - start[0], pos[1] - start[1]);
        // In the scene it moves by the dragged screen distance divided by the zoom.
        assert!(
            (dx - screen_delta.x / scale).abs() < 1.0,
            "dx={dx}, expected={}",
            screen_delta.x / scale
        );
        assert!(
            (dy - screen_delta.y / scale).abs() < 1.0,
            "dy={dy}, expected={}",
            screen_delta.y / scale
        );
        assert_eq!(h.app.selected, Some(a));
        // The move is stored in the graph data only, and the other node stays put.
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
        // Making the same connection again is refused as a duplicate and reported in the status line.
        h.drag(h.port(a), h.node(b));
        assert_eq!(h.app.graph.edges.len(), 1);
        assert!(
            h.app.status_err && h.app.status.contains("Already connected"),
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
        assert_eq!(
            h.app.link_from, None,
            "dropping on empty space leaves no link state behind"
        );
        assert!(
            h.ctx.read_response(Id::unique(("cc-out", b))).is_none(),
            "Output nodes have no output port"
        );
        h.drag(h.port(a), h.node(t2));
        assert!(h.app.graph.edges.is_empty(), "Text has no input port");
        h.drag(h.port(a), h.node(a));
        assert!(h.app.graph.edges.is_empty(), "a node is not connected to itself");
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
        assert_eq!(h.app.graph.nodes.len(), 2, "the nodes stay");
    }

    #[test]
    fn delete_key_and_ctrl_d_work_on_the_selected_node() {
        let (g, a, b) = two_nodes();
        let mut h = Harness::new(g);
        h.click(h.node(a));
        h.frame(key(egui::Key::D, egui::Modifiers::COMMAND));
        assert_eq!(h.app.graph.nodes.len(), 3, "Ctrl+D duplicates");
        let copy = h.app.selected.expect("the copy is selected");
        assert!(copy != a && copy != b);
        assert_eq!(h.app.graph.node(copy).unwrap().pos, [32.0, 32.0]);
        h.frame(key(egui::Key::Delete, egui::Modifiers::NONE));
        assert!(h.app.graph.node(copy).is_none(), "Delete deletes");
        assert_eq!(h.app.selected, None);
        assert_eq!(h.app.graph.nodes.len(), 2);
        // Without a selection nothing happens.
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
        assert!(egui::Popup::is_any_open(&h.ctx), "a right click opens the menu");
        assert_eq!(h.app.graph.nodes.len(), 2);
        // The menu items are stacked vertically below the pointer. Scan that column and press 'Duplicate'.
        for step in 1..12 {
            if h.app.graph.nodes.len() > 2 {
                break;
            }
            h.click(on_node + vec2(16.0, 6.0 + 8.0 * step as f32));
            if !egui::Popup::is_any_open(&h.ctx) && h.app.graph.nodes.len() == 2 {
                // If a click elsewhere closed it, open it again.
                h.click_with(on_node, egui::PointerButton::Secondary);
            }
        }
        assert_eq!(h.app.graph.nodes.len(), 3, "Duplicate in the menu adds a node");
        assert!(h.app.selected.is_some_and(|id| id != a), "the copy is selected");
        assert!(h.app.graph.edges.is_empty());
    }

    #[test]
    fn deferred_actions_run_at_the_end_of_the_frame() {
        let mut h = Harness::new(sample_graph());
        // Open and Save as: cancelling the dialog changes nothing (tests open no window and treat it as cancelled).
        for d in [Deferred::Open, Deferred::SaveAs, Deferred::Save] {
            h.app.deferred = Some(d);
            h.frame(vec![]);
            assert!(h.app.deferred.is_none(), "{d:?} runs only once per frame");
            assert_eq!(h.app.graph.nodes.len(), 4, "{d:?}");
            assert!(h.app.path.is_none() && !h.app.status_err, "{d:?}: {}", h.app.status);
        }
        // New empties the graph and resets the visible area.
        h.app.board.runs.insert(1, NodeRun::default());
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert!(h.app.graph.nodes.is_empty() && h.app.board.runs.is_empty());
        assert_eq!(h.app.status, "New graph");
        // New is blocked while running too.
        h.app.install(sample_graph(), None, &h.ctx.clone());
        h.app.board.running = true;
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert_eq!(h.app.graph.nodes.len(), 4);
        assert!(h.app.status_err);
    }

    #[test]
    fn huge_coordinates_do_not_hang_the_canvas() {
        // Even with huge coordinates in the file (the f32 spacing gets larger than the grid spacing), drawing the
        // grid must finish.
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
        assert_eq!(h.app.graph.nodes.len(), 2, "nothing is deleted while running");
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
        assert_eq!(fitted, bounds, "at first the view fits all nodes");
        // Two nodes are small, so the view starts at the maximum zoom (1.6).
        assert!((h.scale(node) - 1.6).abs() < 1e-3, "{}", h.scale(node));
        h.frame(vec![moved(pos2(500.0, 300.0))]);
        h.frame(vec![egui::Event::Zoom(0.5)]);
        let zoomed_out = h.scale(node);
        assert!(zoomed_out < 1.0, "zooming out lowers the zoom: {zoomed_out}");
        assert!(
            h.app.scene_rect.width() > fitted.width() * 1.2,
            "zooming out shows a larger scene area: {fitted:?} -> {:?}",
            h.app.scene_rect
        );
        // Zooming in and out has limits.
        h.frame(vec![egui::Event::Zoom(100.0)]);
        assert!(
            (h.scale(node) - 1.6).abs() < 1e-3,
            "it never goes above the maximum zoom: {}",
            h.scale(node)
        );
        h.frame(vec![egui::Event::Zoom(0.0001)]);
        assert!(
            (h.scale(node) - 0.25).abs() < 1e-3,
            "it never goes below the minimum zoom: {}",
            h.scale(node)
        );
        h.settle();
        h.app.want_fit = true;
        h.frame(vec![]);
        assert_eq!(h.app.scene_rect, bounds, "Fit view goes back to showing all nodes");
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
        // If every node used the same input field IDs, all nodes would share one undo history (TextEditState).
        let mut g = Graph::default();
        let a = g.add(NodeKind::Text { text: "a".into() }, [0.0, 0.0]);
        let b = g.add(NodeKind::Text { text: "b".into() }, [300.0, 0.0]);
        let mut h = Harness::new(g);
        h.app.selected = Some(a);
        h.settle();
        let with_a = h.text_states();
        h.app.selected = Some(b);
        h.settle();
        let with_b = h.text_states();
        // The inspector has two input fields: the name and the text.
        assert_eq!(
            with_b - with_a,
            2,
            "another node must have its own input field IDs (= undo history)"
        );
        h.app.selected = Some(a);
        h.settle();
        assert_eq!(
            h.text_states(),
            with_b,
            "selecting a node again continues its own history"
        );
    }

    #[test]
    fn opening_clears_the_input_undo_history() {
        let (mut g, a, _) = two_nodes();
        g.nodes[0].title = "title".into();
        let mut h = Harness::new(g);
        h.app.selected = Some(a);
        h.settle();
        assert!(h.text_states() > 0);
        let ctx = h.ctx.clone();
        h.app.new_graph(&ctx);
        assert_eq!(
            h.text_states(),
            0,
            "a new graph keeps no undo history of the previous input"
        );
    }

    #[test]
    fn full_run_through_the_worker_thread() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_flow(&ctx);
        assert!(h.app.board.running, "starting a run sets the running state right away");
        h.wait_until_idle();
        assert!(!h.app.status_err, "{}", h.app.status);
        assert!(h.app.status.starts_with("Done"), "{}", h.app.status);
        assert!(h.app.board.final_text.starts_with("[dry-run]"));
        assert_eq!(h.app.board.runs.len(), 4);
        assert!(
            h.app
                .board
                .runs
                .values()
                .all(|r| matches!(r.state, RunState::Done { .. }))
        );
        // After the run the inspector shows the final output, and the graph can run again.
        h.app.selected = None;
        h.settle();
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert!(h.app.status.starts_with("Done"));
    }

    #[test]
    fn full_test_run_reports_pass_counts() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_tests(None, &ctx);
        h.wait_until_idle();
        assert_eq!(h.app.status, "Tests: 2/2 passed");
        assert!(!h.app.status_err);
        assert!(
            h.app
                .board
                .test_results
                .iter()
                .all(|r| r.as_ref().is_some_and(|r| r.pass))
        );
        // Run again only one case, whose pass condition cannot be met.
        h.app.graph.tests[1].expect = "a phrase that never appears".into();
        h.app.run_tests(Some(1), &ctx);
        h.wait_until_idle();
        assert_eq!(h.app.status, "Tests: 1/2 passed");
        assert!(h.app.status_err);
        let r = h.app.board.test_results[1].as_ref().unwrap();
        assert!(!r.pass && r.error.contains("a phrase that never appears"));
        assert!(
            h.app.board.test_results[0].as_ref().unwrap().pass,
            "the result of the other case stays"
        );
        // Without a Text node, tests do not start.
        h.app.graph.test_node = None;
        h.app.run_tests(None, &ctx);
        assert!(h.app.status_err && !h.app.board.running);
    }

    #[test]
    fn late_results_of_a_replaced_graph_are_ignored() {
        // When the running graph is replaced (the new graph's node ids also start from 1), late events must be
        // dropped.
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        h.app.run_flow(&ctx);
        h.app.install(Graph::default(), None, &ctx);
        assert!(!h.app.board.running);
        std::thread::sleep(std::time::Duration::from_millis(300)); // the worker sends all its messages
        h.settle();
        assert!(
            h.app.board.runs.is_empty(),
            "node states of the old run must not attach to the new graph"
        );
        assert!(h.app.board.final_text.is_empty());
        assert!(!h.app.board.running);
        assert!(!h.app.status.starts_with("Done"), "{}", h.app.status);
        // A new run after the replacement works normally.
        let mut g = Graph::default();
        g.add(
            NodeKind::Text {
                text: "new graph".into(),
            },
            [0.0; 2],
        );
        h.app.install(g, None, &ctx);
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert_eq!(h.app.board.final_text, "new graph");
    }

    /// Review repro: "■ Stop" only halts between nodes, so New and Open stayed blocked during a hanging LLM or MCP
    /// call (up to 120~900 s). After Stop is pressed the graph can be replaced right away, and events the old worker
    /// sends late never reach the new graph.
    #[test]
    fn new_and_open_are_allowed_once_stop_is_requested() {
        let mut h = Harness::new(sample_graph());
        let ctx = h.ctx.clone();
        // Simulate a hanging call: only start the run, and send the old worker's messages later by hand with its
        // generation number.
        let (old, flag) = h.app.begin_run().unwrap();
        assert!(!h.app.can_replace_graph());
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert_eq!(h.app.graph.nodes.len(), 4, "no new graph before Stop is pressed");
        assert!(h.app.status_err && h.app.status.contains("■ Stop"), "{}", h.app.status);

        h.app.request_cancel();
        assert!(flag.load(Ordering::Relaxed) && h.app.can_replace_graph());
        h.frame(vec![]); // the screen is drawn while stopping too (New and Open are enabled)
        h.app.deferred = Some(Deferred::New);
        h.frame(vec![]);
        assert!(h.app.graph.nodes.is_empty(), "after Stop is pressed, New works");
        assert!(!h.app.board.running && !h.app.board.cancelling);
        assert!(
            !h.app.status_err && h.app.status.starts_with("New graph"),
            "{}",
            h.app.status
        );
        assert!(h.app.status.contains("discarded"), "{}", h.app.status);

        // The new graph's node ids also start from 1. Late events, test results and the end signal of the old worker
        // are all dropped.
        h.app.add_node(NodeKind::Text {
            text: "new input".into(),
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
            final_text: "old result".into(),
            cancelled: true,
            ..RunReport::default()
        }))));
        h.settle();
        assert!(h.app.board.runs.is_empty() && h.app.board.final_text.is_empty());
        assert_eq!(
            h.app.status, status,
            "the end signal of the old run must not change the status line"
        );
        assert!(!h.app.board.running);
        // The new graph can run right away (the old stop signal does not carry over).
        h.app.run_flow(&ctx);
        h.wait_until_idle();
        assert_eq!(h.app.board.final_text, "new input", "{}", h.app.status);

        // Open works the same way: blocked before Stop, and it opens right away once Stop is pressed.
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
        late.send(Msg::Finished(Done::Error("old run".into())));
        h.settle();
        assert!(h.app.board.runs.is_empty() && !h.app.status_err, "{}", h.app.status);
    }

    #[test]
    fn stop_button_logic_sets_the_cancel_flag_once() {
        let (mut app, _ctx) = app_with_sample();
        app.request_cancel();
        assert!(!app.board.cancelling, "nothing happens when no run is going");
        let (_, flag) = app.begin_run().unwrap();
        assert!(!flag.load(Ordering::Relaxed));
        app.request_cancel();
        assert!(flag.load(Ordering::Relaxed) && app.board.cancelling);
        assert!(app.status.contains("Stopping"));
        // When the run ends, the stopping state goes away.
        let g = app.board.generation;
        app.board.apply(g, Msg::Finished(Done::Error("x".into())));
        assert!(!app.board.cancelling && !app.board.running);
        // The next run uses a new signal (the previous stop does not carry over).
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
        assert!(app.status.contains("cycle"), "{}", app.status);
    }

    // ---------- English ----------

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
            (format!(ko_text!("Language: {name}"), name = ko_text!("Korean")), false)
        );
        assert_eq!(
            with_lang(Lang::En, || language_status(Lang::En, Err("access denied".into()))),
            (
                "Language: English · could not save the setting: access denied".to_string(),
                true
            )
        );
        // Picking the language already in use changes nothing (neither the global language nor the settings file).
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
            // The sample graph is written in the language in use when it is created. The second case is input in the
            // other language (Korean).
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

            // The text of a saved graph (user content) stays as it is when opened in English.
            app.load(PathBuf::from(SAMPLE_FILE), &ctx);
            assert!(app.status.starts_with("Opened: "), "{}", app.status);
            assert!(matches!(&app.graph.nodes[1].kind, NodeKind::Prompt { role, .. } if role == "Analyze: {{input}}"));
            assert_eq!(app.graph.tests[0].name, "Basic");
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

    /// The English screen shows no Korean apart from user content. Opens the inspector for every node kind and also
    /// draws run results, test results, the right-click menu, the top bar while running and the empty canvas.
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
            assert!(egui::Popup::is_any_open(&h.ctx), "the right-click menu must be open");
            seen.extend(h.frame_texts(vec![]));
            let ctx = h.ctx.clone();
            h.app.install(Graph::default(), None, &ctx);
            h.settle();
            seen.extend(h.frame_texts(vec![]));

            let korean: Vec<&String> = seen.iter().filter(|s| has_hangul(s)).collect();
            assert!(korean.is_empty(), "Korean left on the English screen: {korean:?}");
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
                assert!(seen.iter().any(|s| s.contains(want)), "{want:?} was not drawn");
            }
        });
    }
}
