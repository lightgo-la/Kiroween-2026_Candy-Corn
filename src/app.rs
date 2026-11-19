//! egui 3-pane UI (left: input form / center: canvas and AST X-Ray / right: output & benchmark).
//! Heavy work (profiling, compiling, LLM calls) runs on worker threads, and only the results come back over mpsc
//! channels.

use crate::ast::{ConstraintNode, OutputFormat, TargetModel};
use crate::blueprint::{self, Blueprint, GoalSpec, LlmOptions, PRESETS};
use crate::canvas::{graph::Provider, llm};
use crate::history::{self, History};
use crate::i18n::{self, Lang, Tr};
use crate::keys::{self, KeysWindow};
use crate::llm_view;
use crate::pipeline::{self, CompileRequest, CompileResult, LoadedData, TargetCost};
use crate::studio_canvas::{self, Action, Canvas, Goal, LlmView, Model, NodeKey};
use crate::theme::{self, Palette, THEMES};
use crate::tokenizer;
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

enum Msg {
    Loaded(u64, PathBuf, Result<LoadedData, String>),
}

enum CompileMsg {
    /// The compile result, and a comparison of the same input compiled for each target
    Done(u64, Box<CompileResult>, Vec<TargetCost>),
    Failed(u64, String),
}

/// Result of one real LLM run.
struct LlmRun {
    out: Result<pipeline::OutputCheck, String>,
    /// Time from request to reply (including retries after transient errors, ms)
    ms: u64,
    /// Output token limit used for this run
    max_tokens: u32,
}

/// Real LLM run result. The first value is the request generation (stale replies are discarded).
enum LlmMsg {
    Done(u64, LlmRun),
}

/// LLM run generations. A new run, a cancel, a recompile or applying a blueprint bumps the generation and discards the
/// reply in progress.
/// (The worker's HTTP call cannot be stopped, so cancelling means "discard the reply without waiting".)
#[derive(Default, Debug)]
struct LlmJobs {
    generation: u64,
    running: bool,
}

impl LlmJobs {
    fn start(&mut self) -> u64 {
        self.generation += 1;
        self.running = true;
        self.generation
    }

    /// Discards the run in progress. True if a run was in progress.
    fn cancel(&mut self) -> bool {
        self.generation += 1;
        std::mem::take(&mut self.running)
    }

    /// Accepts the reply if it belongs to the current run, and ends the running state.
    fn accept(&mut self, generation: u64) -> bool {
        let current = self.running && generation == self.generation;
        if current {
            self.running = false;
        }
        current
    }
}

/// The editing state that undo covers.
#[derive(Clone, PartialEq)]
struct Snapshot {
    target: TargetModel,
    role: String,
    goals: Vec<Goal>,
    format: OutputFormat,
    fields_text: String,
}

/// The LLM provider for a compile target.
fn provider_for(t: TargetModel) -> Provider {
    match t {
        TargetModel::Claude => Provider::Anthropic,
        TargetModel::Gpt => Provider::OpenAi,
        TargetModel::Gemini => Provider::Gemini,
    }
}

fn provider_index(p: Provider) -> usize {
    Provider::ALL.iter().position(|x| *x == p).unwrap_or(0)
}

/// Clears egui's own undo history kept in each text box.
/// This keeps old text from coming back after the app changed the text (undo, loading a project, applying a blueprint).
fn clear_text_undo(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_by_type::<egui::text_edit::TextEditState>());
}

/// Short text for the status line. A long error body is shown only on hover.
fn short(s: &str, n: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}

/// Status line text for an LLM run result. Cut-off or stopped replies are reported apart from format violations.
fn llm_status(out: &Result<pipeline::OutputCheck, String>) -> String {
    match out {
        Ok(c) if c.ok => tr!("LLM reply · output format and required fields passed").into(),
        Ok(c) => match &c.cut_off {
            Some(cut) => trf!("LLM reply · {cut} (see VIII on the right)", cut = cut),
            None => tr!("LLM reply · output format violation (see VIII on the right)").into(),
        },
        Err(_) => tr!("LLM run failed (details in VIII on the right)").into(),
    }
}

/// (passed, one-line summary) shown in the canvas LLM node.
fn llm_verdict(out: &Result<pipeline::OutputCheck, String>) -> (bool, String) {
    match out {
        Ok(c) if c.ok && c.table_rows == Some(0) => (true, tr!("✔ Format OK · ⚠ table has no data rows").to_string()),
        Ok(c) if c.ok => (true, tr!("✔ Format and required fields OK").to_string()),
        Ok(c) => (
            false,
            c.cut_off
                .clone()
                .or_else(|| c.error.clone())
                .unwrap_or_else(|| trf!("Missing fields: {}", c.missing.join(", "))),
        ),
        Err(e) => (false, short(e, 80)),
    }
}

/// Color category of a check result line in VIII.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Ok,
    Err,
    Warn,
    Muted,
}

fn tone_color(t: Tone, p: &Palette) -> egui::Color32 {
    match t {
        Tone::Ok => theme::OK,
        Tone::Err => theme::ERR,
        Tone::Warn => p.warn(),
        Tone::Muted => p.sub,
    }
}

/// Check result lines shown in VIII. For a cut-off reply, the cut-off is reported first and the format check result is
/// added only as one muted line (so a parse failure or missing fields in truncated JSON do not look like a format
/// violation). When parsing or finding the table fails, the missing fields are always the whole list, so they are
/// not listed.
fn check_lines(c: &pipeline::OutputCheck) -> Vec<(Tone, String)> {
    if let Some(cut) = &c.cut_off {
        return vec![
            (Tone::Err, format!("✖ {cut}")),
            (
                Tone::Muted,
                trf!("Format check (on the cut-off reply): {}", format_summary(c)),
            ),
        ];
    }
    let mut lines = Vec::new();
    if c.ok {
        lines.push((Tone::Ok, tr!("✔ Output format and required fields passed").to_string()));
        if !c.nulls.is_empty() {
            lines.push((Tone::Warn, trf!("⚠ Fields with empty values: {}", c.nulls.join(", "))));
        }
    } else if let Some(e) = &c.error {
        lines.push((Tone::Err, format!("✖ {e}")));
    } else if !c.missing.is_empty() {
        lines.push((Tone::Warn, trf!("⚠ Missing fields: {}", c.missing.join(", "))));
    }
    if c.table_rows == Some(0) {
        lines.push((
            Tone::Warn,
            tr!("⚠ The table has no data rows (header only)").to_string(),
        ));
    }
    lines
}

/// One-line summary of the format check (the muted line added to a cut-off reply).
fn format_summary(c: &pipeline::OutputCheck) -> String {
    if c.pretty.trim().is_empty() {
        return tr!("empty reply").into();
    }
    match &c.error {
        Some(e) => e.clone(),
        None if !c.missing.is_empty() => trf!("missing fields: {}", c.missing.join(", ")),
        None => tr!("format and required fields match").into(),
    }
}

/// Reply time, the token usage reported by the provider, and the estimated token count of the compiled prompt.
fn run_details(ms: u64, usage: Option<llm::Usage>, prompt_tokens: usize) -> String {
    match usage {
        Some(u) => trf!(
            "Reply in {ms} ms · input {} · output {} tokens (compressed prompt est. {prompt_tokens})",
            u.input,
            u.output,
            ms = ms,
            prompt_tokens = prompt_tokens
        ),
        None => trf!(
            "Reply in {ms} ms · no usage info (compressed prompt est. {prompt_tokens} tokens)",
            ms = ms,
            prompt_tokens = prompt_tokens
        ),
    }
}

fn run_line(run: &LlmRun, prompt_tokens: usize) -> String {
    match &run.out {
        Ok(c) => run_details(run.ms, c.usage, prompt_tokens),
        Err(_) => trf!("Failed after {} ms", run.ms),
    }
}

/// Doubles the output limit (capped at MAX_LLM_MAX_TOKENS). None if it is already at the cap.
fn doubled_limit(n: u32) -> Option<u32> {
    (n < blueprint::MAX_LLM_MAX_TOKENS).then(|| n.saturating_mul(2).min(blueprint::MAX_LLM_MAX_TOKENS))
}

/// For a reply cut off at the output token limit, the raised limit to use when running again.
fn retry_limit(run: &LlmRun) -> Option<u32> {
    match &run.out {
        Ok(c) if c.hit_max_tokens() => doubled_limit(run.max_tokens),
        _ => None,
    }
}

/// File extensions loaded as data (the file dialog and drag and drop use the same list).
const DATA_EXTS: [&str; 6] = ["csv", "tsv", "txt", "json", "jsonl", "ndjson"];

/// Kind of a dropped file (or of a file given as a startup argument).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Blueprint,
    Data,
    Unsupported,
}

/// Decides the file kind from the extension (case-insensitive): .kiro is a blueprint, data extensions go to the
/// profiler, the rest is rejected.
fn drop_kind(path: &Path) -> DropKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "kiro" {
        DropKind::Blueprint
    } else if DATA_EXTS.contains(&ext.as_str()) {
        DropKind::Data
    } else {
        DropKind::Unsupported
    }
}

/// When several files are dropped at once, picks the first one that can be handled (if none, the first file, so the
/// error is reported).
fn pick_drop(paths: &[PathBuf]) -> Option<&PathBuf> {
    paths
        .iter()
        .find(|p| drop_kind(p) != DropKind::Unsupported)
        .or(paths.first())
}

/// Status line hint while a file is being dragged.
fn drop_hint(kind: Option<DropKind>) -> &'static str {
    match kind {
        Some(DropKind::Blueprint) => tr!("Drop here to open the blueprint"),
        Some(DropKind::Data) => tr!("Drop here to load the data"),
        Some(DropKind::Unsupported) => {
            tr!("Unsupported file type (CSV, TSV, TXT, JSON, JSONL, NDJSON data or a .kiro blueprint)")
        }
        None => tr!("Drop here to open the file"),
    }
}

/// The constraints that are on and not empty, with whitespace normalized (used by both the compile request and the
/// stale key).
fn normalized_constraints(goals: &[Goal]) -> Vec<String> {
    goals
        .iter()
        .filter(|g| g.on && !g.text.trim().is_empty())
        .map(|g| g.text.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The edited-input part of a compile request (without the data). start_compile and the stale key use the same
/// normalization.
fn compile_inputs(
    target: TargetModel,
    role: &str,
    goals: &[Goal],
    format: OutputFormat,
    fields_text: &str,
) -> CompileRequest {
    CompileRequest {
        target,
        role: role.to_string(),
        constraints: normalized_constraints(goals),
        format,
        fields: pipeline::parse_fields(fields_text),
        data: None,
    }
}

/// Key that tells whether the compile result matches the current inputs. It is built from the normalized request that
/// is actually sent and the data fingerprint, so adding an empty constraint or only changing spaces or comma spacing
/// does not count as "inputs changed".
fn compile_key(req: &CompileRequest, data: Option<u64>) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        req.target, req.role, req.constraints, req.format, req.fields, data
    )
}

/// What to do with an existing compile result after the UI language changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LangRefresh {
    /// Leave it as is: no result, or a stale result of changed inputs (the next compile uses the new language).
    Keep,
    /// Recompile the same inputs on a worker so the lint log is in the new language.
    /// A compile in progress is restarted in the new language (left alone, it could mix text from both languages).
    Recompile,
}

/// Decides whether to recompile after a language switch. inputs_match tells whether the current inputs match those of
/// the shown result (or of the request in progress while compiling). If the inputs changed, there is no recompile: a
/// compile the user did not press does not replace the result, and the "inputs changed" mark stays.
fn lang_refresh(has_result: bool, compiling: bool, inputs_match: bool) -> LangRefresh {
    if (has_result || compiling) && inputs_match {
        LangRefresh::Recompile
    } else {
        LangRefresh::Keep
    }
}

/// Whether to keep the LLM result (received or still arriving) when a recompiled result comes in.
/// lang_switch_prompt is the previous result's prompt for a language-switch recompile. The compiled prompt does not
/// depend on the UI language (test `compiled_prompt_does_not_depend_on_ui_language`), so it is usually the same, and
/// then the reply was received for the same prompt and is kept. It is discarded if the prompt changed or the user
/// started the compile.
fn keep_llm_after_recompile(lang_switch_prompt: Option<&str>, new_prompt: &str) -> bool {
    lang_switch_prompt == Some(new_prompt)
}

/// Saves the chosen language to the settings file. The app passes `i18n::save_default`, tests pass a function that
/// writes to a temporary path.
/// On failure, returns the text to show in the status line (in the new language).
fn persist_lang(lang: Lang, save: impl FnOnce(Lang) -> Result<(), String>) -> Result<(), String> {
    save(lang).map_err(|e| {
        trf!(
            "Could not save the language setting (applies to this session only): {e}",
            e = e
        )
    })
}

/// A warning text when the data was read as one column whose name still contains tabs, semicolons, commas or pipes.
/// The profiler picks the delimiter (comma, semicolon, tab, pipe) from the header line, but if it guesses wrong, a
/// wrong one-column profile looks like a success and goes into the prompt, so the user is told.
fn delimiter_warning(d: &LoadedData) -> Option<&'static str> {
    let cols = &d.data.columns;
    (cols.len() == 1 && cols[0].name.contains(['\t', ';', ',', '|'])).then_some(tr!(
        "⚠ Read as a single column: the header still has tabs, semicolons, commas or pipes; check the delimiter"
    ))
}

/// Fingerprint of the data content that goes into the prompt (for the stale key). Reading the same file again with the
/// same settings gives the same value.
fn data_fingerprint(d: &LoadedData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    let s = &d.data;
    (
        &d.path,
        d.raw_tokens,
        &s.source_name,
        s.total_rows,
        &s.anomalies,
        &s.sample_rows,
    )
        .hash(&mut h);
    for c in &s.columns {
        (&c.name, &c.dtype, c.null_ratio.to_bits(), &c.summary_stats).hash(&mut h);
    }
    h.finish()
}

/// The last opened or saved blueprint. Comparing it with the serialized current settings reveals unsaved changes.
#[derive(Default, Debug)]
struct SavedState {
    fingerprint: String,
}

impl SavedState {
    fn mark(&mut self, bp: &Blueprint) {
        self.fingerprint = bp.fingerprint();
    }

    fn is_dirty(&self, bp: &Blueprint) -> bool {
        bp.fingerprint() != self.fingerprint
    }
}

/// Window title: a leading ● when there are unsaved changes, and the blueprint file name in parentheses when it
/// differs from the project name.
fn window_title(project: &str, dirty: bool, path: Option<&Path>) -> String {
    let mark = if dirty { "● " } else { "" };
    let name = match project.trim() {
        "" => tr!("Untitled"),
        n => n,
    };
    let file = path
        .and_then(Path::file_name)
        .map(|f| f.to_string_lossy())
        .filter(|f| f.as_ref() != name)
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();
    format!("{mark}{name}{file} — Candy-Corn")
}

/// An action that asks first when there are unsaved changes.
#[derive(Clone, Debug, PartialEq)]
enum Pending {
    /// The open-blueprint dialog
    OpenDialog,
    /// Open this blueprint file (drag and drop)
    OpenPath(PathBuf),
    /// Apply a built-in blueprint (index into PRESETS)
    Preset(usize),
    /// Close the window
    Close,
}

impl Pending {
    fn label(&self) -> &'static str {
        match self {
            Pending::OpenDialog | Pending::OpenPath(_) => tr!("open the blueprint"),
            Pending::Preset(_) => tr!("apply the built-in blueprint"),
            Pending::Close => tr!("close the window"),
        }
    }
}

/// Result of saving a blueprint. "Save" in the unsaved-changes prompt goes on with the action only on Saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    /// The save dialog was closed
    Cancelled,
    /// Writing failed (error in the status line)
    Failed,
}

/// File dialogs run at the end of the frame: opened inside the UI pass, they freeze the frame and garble input.
#[derive(Clone, Debug)]
enum Dialog {
    PickData,
    /// Save as (always asks for a location)
    SaveAs,
    /// Ctrl+S: save to the current blueprint file (asks for a location if there is no file yet)
    QuickSave,
    LoadProject,
    SaveOutput,
    /// Save the LLM reply body to a file
    SaveReply,
    /// Go on with the action after a successful save ("Save" in the unsaved-changes prompt)
    SaveThen(Pending),
}

/// Top bar buttons
enum TopAction {
    Compile,
    Undo,
    Redo,
}

fn undo_hint() -> &'static str {
    tr!(
        "Ctrl+Z · covers role, constraints, output format and fields, target\n(Not theme, data, sample rows, project name, node layout or LLM run options. While typing, undo works inside the text box)"
    )
}

/// First-frame estimate of the width needed to draw the top bar on one row (later frames use the width measured in the
/// last frame).
const TOPBAR_NEED_GUESS: f32 = 1240.0;

/// Minimum width always left for the center canvas.
const CENTER_MIN_W: f32 = 360.0;

/// Maximum (left, right) panel widths for a window width. The spare width is split 45:55 so that the center canvas
/// keeps at least CENTER_MIN_W even in a narrow window (1000 px minimum), while each panel keeps its minimum width
/// (280, 320) and its original maximum width (520, 700).
fn side_panel_max(window_w: f32) -> (f32, f32) {
    let spare = (window_w - CENTER_MIN_W).max(0.0);
    ((spare * 0.45).clamp(280.0, 520.0), (spare * 0.55).clamp(320.0, 700.0))
}

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| tr!("unknown error").into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Canvas,
    XRay,
}

/// Role and constraints of the first launch (filled in the startup language; after that they are user content and
/// stay as they are when the language changes).
const DEFAULT_ROLE: Tr = trp!("Data analyst who finds root causes in sales data");
const DEFAULT_GOALS: [(Tr, bool); 3] = [
    (trp!("Exclude missing values (0.2%)"), true),
    (trp!("Detect outliers first"), true),
    (trp!("Classify by the 4M threshold"), false),
];

pub struct StudioApp {
    project: String,
    target: TargetModel,
    role: String,
    goals: Vec<Goal>,
    next_id: u64,
    new_goal: String,
    format: OutputFormat,
    fields_text: String,
    sample_rows: usize,
    theme_idx: usize,
    tab: Tab,
    canvas: Canvas,

    loaded: Option<LoadedData>,
    /// Content fingerprint of the shown data (for the stale key; only meaningful when loaded is set)
    loaded_fp: u64,
    loading: bool,
    compiling: bool,
    status: String,
    status_err: bool,
    result: Option<CompileResult>,
    /// Token and cost comparison of the result's input compiled for each target (X-Ray tab)
    compare: Vec<TargetCost>,
    result_key: String,
    dialog: Option<Dialog>,
    load_gen: u64,
    /// Kind of the file being dragged over the window (Some(None) for a file whose path is unknown)
    hover_drop: Option<Option<DropKind>>,
    steering_rules: Vec<String>,
    /// The last applied theme (wherever it changes, in the canvas, the header or a blueprint, it is applied once at the
    /// start of a frame)
    applied_theme: usize,
    /// The last opened or saved blueprint file
    blueprint_path: Option<PathBuf>,
    /// Folder the blueprint dialogs open in (the folder of the last opened or saved file)
    last_dir: Option<PathBuf>,
    /// The save question was answered, so the window may close
    allow_close: bool,
    /// Data file reference (the shown data and a pending or missing request). Used for saving blueprints and for
    /// re-profiling.
    data_ref: DataRef,
    compile_gen: u64,
    /// (generation, previous result's prompt) of a compile restarted for a language switch. If the same prompt comes
    /// out, the LLM result is kept.
    lang_recompile: Option<(u64, String)>,
    /// Whether the compile result differs from the current inputs (computed once per frame)
    stale: bool,
    history: History<Snapshot>,
    logo: Option<egui::TextureHandle>,
    llm_models: [String; 3],
    /// Output limit, temperature, system prompt (edited by both the canvas LLM node and VIII; saved in blueprint v3)
    llm_opts: LlmOptions,
    llm: LlmJobs,
    llm_out: Option<LlmRun>,
    llm_tx: Sender<LlmMsg>,
    llm_rx: Receiver<LlmMsg>,
    /// 🔑 API keys window and the cached state of the saved keys (the keys themselves are read only to start a run)
    keys: KeysWindow,
    /// VIII (LLM run result) is shown in its own window instead of the right pane
    llm_window: bool,
    /// How the reply body is shown in VIII
    llm_tab: llm_view::ReplyTab,
    /// Recent finished runs (chart and table in VIII)
    llm_history: Vec<llm_view::RunRecord>,
    /// Provider and model of the run in progress, for its history entry
    llm_meta: Option<(Provider, String)>,
    /// Moving fog of the fog themes (an app setting, so it is not saved in blueprints)
    fog_on: bool,
    /// The last opened or saved blueprint
    saved: SavedState,
    /// Whether there are unsaved changes (computed every frame)
    dirty: bool,
    /// The action the save prompt is asking about
    confirm: Option<Pending>,
    /// The last window title sent
    title: String,
    /// Width needed to draw the top bar on one row (measured in the last frame)
    topbar_need: f32,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx_tx: Sender<CompileMsg>,
    ctx_rx: Receiver<CompileMsg>,
}

/// Registers a Windows Korean font as a fallback at the end of egui's font lists.
pub fn install_korean_font(ctx: &egui::Context) -> bool {
    const CANDIDATES: [&str; 3] = [
        "C:\\Windows\\Fonts\\malgun.ttf",
        "C:\\Windows\\Fonts\\NanumGothic.ttf",
        "C:\\Windows\\Fonts\\gulim.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "korean".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("korean".to_owned());
    }
    ctx.set_fonts(fonts);
    true
}

impl StudioApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        startup_file: Option<PathBuf>,
        startup_blueprint: Option<PathBuf>,
        theme_arg: Option<usize>,
    ) -> Self {
        // Preload the tokenizer vocabulary (~85 ms) so the first compile does not wait for it.
        std::thread::spawn(tokenizer::warm_up);
        let font_ok = install_korean_font(&cc.egui_ctx);
        let theme_idx = theme_arg.unwrap_or(3).min(THEMES.len() - 1); // default: 04 Premium Noir
        theme::apply(&cc.egui_ctx, &THEMES[theme_idx]);
        let (tx, rx) = channel();
        let (ctx_tx, ctx_rx) = channel();
        let (llm_tx, llm_rx) = channel();
        let logo = eframe::icon_data::from_png_bytes(theme::ICON_PNG).ok().map(|icon| {
            let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            cc.egui_ctx.load_texture("app-logo", img, egui::TextureOptions::LINEAR)
        });

        let mut app = StudioApp {
            project: "sales_analysis.kiro".into(),
            target: TargetModel::Claude,
            role: DEFAULT_ROLE.get().into(),
            goals: vec![],
            next_id: 1,
            new_goal: String::new(),
            format: OutputFormat::StrictJson,
            fields_text: pipeline::DEFAULT_FIELDS.into(),
            sample_rows: 8,
            theme_idx,
            tab: Tab::Canvas,
            canvas: Canvas::default(),
            loaded: None,
            loaded_fp: 0,
            loading: false,
            compiling: false,
            status: tr!("Load data, then press Compile & Run (Ctrl+Enter)").into(),
            status_err: false,
            result: None,
            compare: vec![],
            result_key: String::new(),
            dialog: None,
            load_gen: 0,
            hover_drop: None,
            steering_rules: vec![],
            applied_theme: theme_idx,
            blueprint_path: None,
            last_dir: None,
            allow_close: false,
            data_ref: DataRef::default(),
            compile_gen: 0,
            lang_recompile: None,
            stale: false,
            history: History::new(Snapshot {
                target: TargetModel::Claude,
                role: String::new(),
                goals: vec![],
                format: OutputFormat::StrictJson,
                fields_text: String::new(),
            }),
            logo,
            llm_models: Provider::ALL.map(|p| p.default_model().to_string()),
            llm_opts: LlmOptions::default(),
            llm: LlmJobs::default(),
            llm_out: None,
            llm_tx,
            llm_rx,
            keys: KeysWindow::load(),
            llm_window: false,
            llm_tab: llm_view::ReplyTab::default(),
            llm_history: Vec::new(),
            llm_meta: None,
            fog_on: true,
            saved: SavedState::default(),
            dirty: false,
            confirm: None,
            title: String::new(),
            topbar_need: TOPBAR_NEED_GUESS,
            tx,
            rx,
            ctx_tx,
            ctx_rx,
        };
        for (text, on) in DEFAULT_GOALS {
            app.add_goal(text.get().to_string(), on);
        }
        // Inject the working folder's steering rules automatically (UTF-8 or CP949, detected automatically)
        if let Some(md) = pipeline::find_steering().and_then(|p| pipeline::read_text_auto(&p).ok()) {
            app.steering_rules = pipeline::parse_steering_rules(&md);
            for r in app.steering_rules.clone() {
                app.add_goal(r, true);
            }
        }
        // Compute the default layout right away so the blueprint has node positions even before the first frame.
        app.canvas.reset_layout(&app.goals);
        app.history.reset(app.snapshot()); // the startup state is not something to undo
        if !font_ok {
            app.status = tr!("Korean font (malgun.ttf etc.) not found; Korean text may not display correctly").into();
            app.status_err = true;
        }
        if let Some(bp) = startup_blueprint {
            app.open_blueprint(&bp, &cc.egui_ctx);
            if let Some(t) = theme_arg {
                app.theme_idx = t.min(THEMES.len() - 1); // --theme takes precedence over the blueprint
            }
        }
        if let Some(p) = startup_file {
            app.open_path(p, &cc.egui_ctx);
        }
        // Treat the startup state (including the blueprint just opened, --theme and data given as a startup argument)
        // as saved.
        app.mark_saved();
        app
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            target: self.target,
            role: self.role.clone(),
            goals: self.goals.clone(),
            format: self.format,
            fields_text: self.fields_text.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot, what: &str) {
        self.target = s.target;
        self.role = s.role;
        self.format = s.format;
        self.fields_text = s.fields_text;
        self.goals = s.goals;
        // Keep the ids of restored and new constraints from colliding.
        let max_id = self.goals.iter().map(|g| g.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id + 1);
        self.status = what.to_string();
        self.status_err = false;
    }

    fn undo(&mut self, ctx: &egui::Context) {
        match self.history.undo() {
            Some(s) => {
                self.restore(s, tr!("Undone"));
                clear_text_undo(ctx);
            }
            None => self.status = tr!("Nothing to undo").into(),
        }
    }

    fn redo(&mut self, ctx: &egui::Context) {
        match self.history.redo() {
            Some(s) => {
                self.restore(s, tr!("Redone"));
                clear_text_undo(ctx);
            }
            None => self.status = tr!("Nothing to redo").into(),
        }
    }

    /// Provider shown in the LLM node and VIII (the compile result's target if there is one, else the current target).
    fn llm_provider(&self) -> Provider {
        provider_for(self.result.as_ref().map_or(self.target, |r| r.ast.target_model))
    }

    /// Whether the LLM can be run now (start_llm checks the API key separately).
    fn llm_ready(&self) -> bool {
        self.result.is_some() && !self.stale && !self.compiling && !self.llm.running
    }

    /// Sends the compiled prompt to the real LLM and checks the reply against the output schema (on a worker thread).
    fn start_llm(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.result else { return };
        let provider = provider_for(r.ast.target_model);
        // The key saved in the API keys window first, then the environment variable.
        let Some((key, _)) = keys::lookup(provider.default_key_env(), true) else {
            self.status = trf!(
                "No {} API key. Add one in 🔑 API keys or set {}",
                provider.label(),
                provider.default_key_env()
            );
            self.status_err = true;
            return;
        };
        // The custom endpoint saved in the API keys window (empty: the provider's own address).
        let base_url = match keys::base_url(provider) {
            Ok(u) => u,
            Err(e) => {
                self.status = e;
                self.status_err = true;
                return;
            }
        };
        let model = {
            let m = self.llm_models[provider_index(provider)].trim();
            if m.is_empty() {
                provider.default_model().to_string()
            } else {
                m.to_string()
            }
        };
        let prompt = r.output.clone();
        let format = r.ast.output_schema.format;
        let fields = r.ast.output_schema.required_fields.clone();
        let opts = self.llm_opts.clone().normalized();
        // Send the temperature clamped to the provider's range (the saved value is left alone).
        let (max_tokens, temperature) = (opts.max_tokens, opts.effective_temperature(provider));
        let system = opts.system.trim().to_string();
        let generation = self.llm.start();
        self.llm_meta = Some((provider, model.clone()));
        self.llm_out = None;
        self.status = trf!(
            "{} · {model} running... (max output {max_tokens})",
            provider.label(),
            model = model,
            max_tokens = max_tokens
        );
        self.status_err = false;
        let (tx, ctx) = (self.llm_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let req = llm::LlmRequest {
                provider,
                model: &model,
                system: &system,
                prompt: &prompt,
                api_key: &key,
                base_url: &base_url,
                max_tokens,
                temperature,
            };
            let started = Instant::now();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                llm::call(&req).map(|reply| pipeline::check_reply(format, &fields, &reply, max_tokens))
            }))
            .unwrap_or_else(|e| Err(trf!("LLM call internal error: {}", panic_text(e))));
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let _ = tx.send(LlmMsg::Done(generation, LlmRun { out, ms, max_tokens }));
            ctx.request_repaint();
        });
    }

    /// Discards the LLM run in progress (its reply is ignored if it still arrives).
    fn cancel_llm(&mut self) {
        if self.llm.cancel() {
            self.status = tr!("LLM run cancelled").into();
            self.status_err = false;
        }
    }

    /// Sends the same prompt again with a raised output limit ("Retry with 2× limit" for a cut-off reply).
    fn retry_longer(&mut self, limit: u32, ctx: &egui::Context) {
        self.llm_opts.max_tokens = limit.clamp(blueprint::MIN_LLM_MAX_TOKENS, blueprint::MAX_LLM_MAX_TOKENS);
        if self.llm_ready() {
            self.start_llm(ctx);
        }
    }

    fn add_goal(&mut self, text: String, on: bool) {
        self.goals.push(Goal {
            id: self.next_id,
            text,
            on,
        });
        self.next_id += 1;
    }

    fn palette(&self) -> Palette {
        THEMES[self.theme_idx.min(THEMES.len() - 1)]
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        // Only called at the end of the frame (run_dialog).
        let Some(path) = rfd::FileDialog::new()
            .add_filter(tr!("Data (CSV, TSV, TXT, JSON, JSONL, NDJSON)"), &DATA_EXTS)
            .pick_file()
        else {
            return;
        };
        self.spawn_load(path, ctx);
    }

    /// Opens a dropped file (or a file given as a startup argument) according to its extension:
    /// .kiro as a blueprint, data files through the profiler; any other file is reported as an error.
    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        match drop_kind(&path) {
            DropKind::Blueprint => self.request(Pending::OpenPath(path), ctx),
            DropKind::Data => self.spawn_load(path, ctx),
            DropKind::Unsupported => {
                self.status = trf!(
                    "Unsupported file type: {} (only CSV, TSV, TXT, JSON, JSONL, NDJSON data or .kiro blueprints can be opened)",
                    path.display()
                );
                self.status_err = true;
            }
        }
    }

    fn spawn_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.data_ref.requested(path.clone(), self.sample_rows);
        self.loading = true;
        self.status = trf!("Profiling: {}", path.display());
        self.status_err = false;
        self.load_gen += 1;
        let (tx, ctx, k, generation) = (self.tx.clone(), ctx.clone(), self.sample_rows, self.load_gen);
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| pipeline::load_data(&path, k))
                .unwrap_or_else(|e| Err(trf!("Profiler internal error: {}", panic_text(e))));
            let _ = tx.send(Msg::Loaded(generation, path, r));
            ctx.request_repaint();
        });
    }

    /// Profiles the same file again if the sample row count differs from the last profiling request.
    /// While a load is in progress, that file is the one used (it does not go back to the previous file).
    fn reprofile_if_needed(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.data_ref.reprofile_target(self.sample_rows) {
            self.spawn_load(path, ctx);
        }
    }

    /// The edited-input part of the compile request (without the data).
    fn request_inputs(&self) -> CompileRequest {
        compile_inputs(self.target, &self.role, &self.goals, self.format, &self.fields_text)
    }

    fn data_fp(&self) -> Option<u64> {
        self.loaded.as_ref().map(|_| self.loaded_fp)
    }

    /// Key for checking whether the compile result matches the current inputs (same normalization as the request that
    /// start_compile sends).
    fn input_key(&self) -> String {
        compile_key(&self.request_inputs(), self.data_fp())
    }

    fn start_compile(&mut self, ctx: &egui::Context) {
        self.start_compile_with(ctx, None);
    }

    /// Compiles the current inputs on a worker. keep_llm_for is the previous result's prompt when recompiling after a
    /// language switch (if the result has the same prompt, the LLM result is kept). None when the user started it.
    fn start_compile_with(&mut self, ctx: &egui::Context, keep_llm_for: Option<String>) {
        let inputs = self.request_inputs();
        self.result_key = compile_key(&inputs, self.data_fp());
        let req = CompileRequest {
            data: self.loaded.clone(),
            ..inputs
        };
        self.compiling = true;
        self.status = tr!("Compiling...").into();
        self.status_err = false;
        self.compile_gen += 1;
        self.lang_recompile = keep_llm_for.map(|prompt| (self.compile_gen, prompt));
        let (tx, ctx, generation) = (self.ctx_tx.clone(), ctx.clone(), self.compile_gen);
        std::thread::spawn(move || {
            let msg = match std::panic::catch_unwind(|| (pipeline::compile(&req), pipeline::compare_targets(&req))) {
                Ok((r, cmp)) => CompileMsg::Done(generation, Box::new(r), cmp),
                Err(e) => CompileMsg::Failed(generation, panic_text(e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(Msg::Loaded(generation, path, r)) = self.rx.try_recv() {
            if generation != self.load_gen {
                continue; // a newer load was started, so the stale result is discarded
            }
            self.loading = false;
            match r {
                Ok(d) => {
                    self.status = trf!("Loaded · {} ({} rows)", path.display(), d.data.total_rows);
                    if let Some(w) = delimiter_warning(&d) {
                        self.status = format!("{} · {w}", self.status);
                        self.status_err = true;
                    }
                    self.loaded_fp = data_fingerprint(&d);
                    self.loaded = Some(d);
                    self.data_ref.succeeded();
                }
                Err(e) => {
                    self.status = trf!("Load failed: {e}", e = e);
                    self.status_err = true;
                    // If the previous data is still shown, saving and re-profiling point at that data as well,
                    // and the sample row count is set back to match it (so the form and the actual data agree).
                    if let Some(rows) = self.data_ref.failed()
                        && rows != self.sample_rows
                    {
                        self.sample_rows = rows;
                        self.status
                            .push_str(&trf!(" · sample rows reset to {rows}", rows = rows));
                    }
                }
            }
        }
        while let Ok(msg) = self.ctx_rx.try_recv() {
            let generation = match &msg {
                CompileMsg::Done(g, ..) | CompileMsg::Failed(g, _) => *g,
            };
            if generation != self.compile_gen {
                continue;
            }
            self.compiling = false;
            // The previous result's prompt, if this compile was restarted for a language switch
            let lang_switch = self
                .lang_recompile
                .take()
                .filter(|(g, _)| *g == generation)
                .map(|(_, prompt)| prompt);
            match msg {
                CompileMsg::Done(_, r, cmp) => {
                    // A language-switch recompile does not overwrite the status line (the language notice or a settings
                    // save error).
                    if lang_switch.is_none() {
                        self.status = tr!("Compiled").into();
                        self.status_err = false;
                    }
                    // An LLM reply received (or still arriving) for the previous prompt no longer matches. If only the
                    // language changed and the prompt is the same, the reply is for the same prompt and is kept.
                    if !keep_llm_after_recompile(lang_switch.as_deref(), &r.output) {
                        self.llm_out = None;
                        self.llm.cancel();
                    }
                    self.result = Some(*r);
                    self.compare = cmp;
                }
                CompileMsg::Failed(_, e) => {
                    self.status = trf!("Compile failed: {e}", e = e);
                    self.status_err = true;
                    self.result_key.clear(); // mark the remaining previous result as "inputs changed"
                }
            }
        }
        while let Ok(LlmMsg::Done(generation, run)) = self.llm_rx.try_recv() {
            if !self.llm.accept(generation) {
                continue; // a reply discarded by a cancel, a new run or a recompile
            }
            self.status = llm_status(&run.out);
            self.status_err = !matches!(&run.out, Ok(c) if c.ok);
            if let Some((provider, model)) = self.llm_meta.take() {
                llm_view::push_record(
                    &mut self.llm_history,
                    llm_view::RunRecord {
                        provider,
                        model,
                        ms: run.ms,
                        usage: run.out.as_ref().ok().and_then(|c| c.usage),
                        passed: matches!(&run.out, Ok(c) if c.ok),
                        failed: run.out.is_err(),
                    },
                );
            }
            self.llm_out = Some(run);
        }
    }

    // ---------- project / result files ----------

    /// Builds a blueprint from all the current settings.
    fn to_blueprint(&self, save_to: Option<&Path>) -> Blueprint {
        let positions = NodeKey::FIXED
            .iter()
            .filter_map(|k| Some((k.fixed_name()?.to_string(), self.canvas.pos_of(*k).map(|p| [p.x, p.y])?)))
            .collect();
        // Data in the blueprint's folder (or below it) is saved as a relative path, so the folder still works when
        // moved as a whole.
        let data_path = self.data_ref.source().map(|d| match save_to {
            Some(bp_file) => Blueprint::portable_data_path(d, bp_file),
            None => d.display().to_string(),
        });
        Blueprint {
            version: blueprint::VERSION,
            project: self.project.clone(),
            target: self.target,
            role: self.role.clone(),
            goals: self
                .goals
                .iter()
                .map(|g| GoalSpec {
                    text: g.text.clone(),
                    on: g.on,
                    pos: self.canvas.pos_of(NodeKey::Goal(g.id)).map(|p| [p.x, p.y]),
                })
                .collect(),
            format: self.format,
            fields: self.fields_text.clone(),
            sample_rows: self.sample_rows,
            theme: self.theme_idx,
            data_path,
            positions,
            llm_models: Provider::ALL
                .iter()
                .copied()
                .zip(self.llm_models.iter().cloned())
                .collect(),
            llm_options: self.llm_opts.clone(),
        }
    }

    /// Remembers the current settings as the "saved state" (right after open/save). Later changes get the ● mark.
    fn mark_saved(&mut self) {
        self.saved.mark(&self.to_blueprint(None));
        self.dirty = false;
    }

    /// Applies all of the blueprint's settings. With keep_data the current data is kept (and profiled again if the
    /// sample row count changed); otherwise the blueprint's data path is loaded. Returns a warning text, if any.
    fn apply_blueprint(
        &mut self,
        bp: Blueprint,
        source: Option<&Path>,
        keep_data: bool,
        ctx: &egui::Context,
    ) -> Option<String> {
        let bp = bp.normalized();
        self.project = bp.project.clone();
        self.target = bp.target;
        self.role = bp.role.clone();
        self.format = bp.format;
        self.fields_text = bp.fields.clone();
        self.sample_rows = bp.sample_rows;
        self.theme_idx = bp.theme;
        self.goals.clear();
        let mut saved_pos = Vec::new();
        for g in &bp.goals {
            self.add_goal(g.text.clone(), g.on);
            if let (Some(p), Some(last)) = (g.pos, self.goals.last()) {
                saved_pos.push((NodeKey::Goal(last.id), p));
            }
        }
        saved_pos.extend(
            bp.positions
                .iter()
                .filter_map(|(name, p)| Some((NodeKey::from_fixed_name(name)?, *p))),
        );
        // Compute the default layout first and put the saved positions over it (nodes without a position stay in their
        // default place, not at the origin).
        self.canvas.load_layout(&self.goals, saved_pos);
        for (i, p) in Provider::ALL.iter().enumerate() {
            if let Some(m) = bp.llm_model(*p) {
                self.llm_models[i] = m.to_string();
            }
        }
        self.llm_opts = bp.llm_options.clone();
        self.canvas.request_fit();
        self.result = None;
        self.compare.clear();
        self.result_key.clear();
        self.llm_out = None;
        self.llm.cancel();
        clear_text_undo(ctx);
        // The result of a compile in progress does not match this blueprint, so it is discarded.
        self.compile_gen += 1;
        self.compiling = false;
        self.lang_recompile = None;
        if keep_data {
            self.reprofile_if_needed(ctx);
            return None;
        }
        // Discard the previous data and any load in progress.
        self.loaded = None;
        self.loading = false;
        self.load_gen += 1;
        self.data_ref = DataRef::default();
        match bp.data_path_from(source) {
            Some(dp) if dp.is_file() => {
                self.spawn_load(dp, ctx);
                None
            }
            Some(dp) => {
                let warning = trf!("Data file not found: {}", dp.display());
                self.data_ref.missing(dp, self.sample_rows); // so the reference survives saving again
                Some(warning)
            }
            None => None,
        }
    }

    fn with_steering(&self, bp: Blueprint) -> Blueprint {
        merge_steering(bp, &self.steering_rules)
    }

    fn open_blueprint(&mut self, path: &Path, ctx: &egui::Context) {
        let bp = match Blueprint::load(path) {
            Ok(b) => self.with_steering(b),
            Err(e) => {
                self.status = trf!("Failed to open blueprint: {e}", e = e);
                self.status_err = true;
                return;
            }
        };
        let warning = self.apply_blueprint(bp, Some(path), false, ctx);
        self.blueprint_path = Some(path.to_path_buf());
        self.last_dir = path.parent().map(Path::to_path_buf);
        self.history.reset(self.snapshot()); // clear the history so undo does not go back into another blueprint
        self.mark_saved();
        match warning {
            Some(w) => {
                self.status = trf!("Blueprint opened · {w}", w = w);
                self.status_err = true;
            }
            None => {
                self.status = trf!("Blueprint opened: {}", path.display());
                self.status_err = false;
            }
        }
    }

    /// Saves the blueprint to path. On success that file becomes the current blueprint and is marked as saved.
    fn save_to(&mut self, path: &Path) -> SaveOutcome {
        match self.to_blueprint(Some(path)).save(path) {
            Ok(()) => {
                self.status = trf!("Blueprint saved: {}", path.display());
                self.status_err = false;
                self.last_dir = path.parent().map(Path::to_path_buf);
                self.blueprint_path = Some(path.to_path_buf());
                self.mark_saved();
                SaveOutcome::Saved
            }
            Err(e) => {
                self.status = e;
                self.status_err = true;
                SaveOutcome::Failed
            }
        }
    }

    /// Save as (only called at the end of the frame). The dialog opens in the folder of the last opened or saved file.
    fn save_as(&mut self) -> SaveOutcome {
        let mut dialog = rfd::FileDialog::new()
            .add_filter(tr!("Candy-Corn blueprint"), &["kiro"])
            .set_file_name(self.project.clone());
        if let Some(dir) = &self.last_dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else {
            return SaveOutcome::Cancelled;
        };
        self.save_to(&path)
    }

    /// Ctrl+S: saves to the current blueprint file, or asks for a location if there is no file yet (only called at the
    /// end of the frame).
    fn quick_save(&mut self) -> SaveOutcome {
        match self.blueprint_path.clone() {
            Some(path) => self.save_to(&path),
            None => self.save_as(),
        }
    }

    fn load_project(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new().add_filter(tr!("Candy-Corn blueprint"), &["kiro"]);
        if let Some(dir) = &self.last_dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        self.open_blueprint(&path, ctx);
    }

    fn save_output(&mut self) {
        let Some(r) = &self.result else {
            self.status = tr!("No result to save. Compile first").into();
            self.status_err = true;
            return;
        };
        let ext = match r.ast.target_model {
            TargetModel::Claude => "xml",
            TargetModel::Gpt => "md",
            TargetModel::Gemini => "txt",
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(tr!("Prompt"), &[ext, "txt"])
            .set_file_name(format!("prompt.{ext}"))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, &r.output) {
            Ok(()) => {
                self.status = trf!("Result saved: {}", path.display());
                self.status_err = false;
            }
            Err(e) => {
                self.status = trf!("Save failed: {e}", e = e);
                self.status_err = true;
            }
        }
    }

    /// Saves the body of the last LLM reply (.json for JSON output, .md for a table).
    fn save_reply(&mut self) {
        let Some(Ok(c)) = self.llm_out.as_ref().map(|r| &r.out) else {
            return;
        };
        let json = self
            .result
            .as_ref()
            .is_none_or(|r| r.ast.output_schema.format == OutputFormat::StrictJson);
        let ext = if json { "json" } else { "md" };
        let Some(path) = rfd::FileDialog::new()
            .add_filter(tr!("LLM reply"), &[ext, "txt"])
            .set_file_name(format!("reply.{ext}"))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, &c.pretty) {
            Ok(()) => {
                self.status = trf!("Reply saved: {}", path.display());
                self.status_err = false;
            }
            Err(e) => {
                self.status = trf!("Save failed: {e}", e = e);
                self.status_err = true;
            }
        }
    }

    /// Applies a built-in blueprint. The current data, LLM models and run options are kept, and the edits can be
    /// undone. It starts fresh from a template, so the link to the current file is dropped (Ctrl+S asks for a location
    /// instead of overwriting the previous .kiro).
    fn apply_preset(&mut self, i: usize, ctx: &egui::Context) {
        let Some(preset) = PRESETS.get(i) else { return };
        let mut bp = self.with_steering(preset.to_blueprint());
        bp.llm_models = Provider::ALL
            .iter()
            .copied()
            .zip(self.llm_models.iter().cloned())
            .collect();
        bp.llm_options = self.llm_opts.clone();
        self.apply_blueprint(bp, None, true, ctx);
        self.blueprint_path = None;
        self.status = trf!("Blueprint applied: {} ({})", preset.name.get(), preset.summary.get());
        self.status_err = false;
    }

    /// Opens a blueprint, applies a built-in blueprint or closes the window. Asks first if there are unsaved changes.
    fn request(&mut self, p: Pending, ctx: &egui::Context) {
        if self.saved.is_dirty(&self.to_blueprint(None)) {
            self.confirm = Some(p);
        } else {
            self.perform(p, ctx);
        }
    }

    fn perform(&mut self, p: Pending, ctx: &egui::Context) {
        match p {
            Pending::OpenDialog => {
                // The dialog opens at the end of the frame (in the next frame if this already is the end of one).
                self.dialog = Some(Dialog::LoadProject);
                ctx.request_repaint();
            }
            Pending::OpenPath(path) => self.open_blueprint(&path, ctx),
            Pending::Preset(i) => self.apply_preset(i, ctx),
            Pending::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn remove_goal(&mut self, id: u64) {
        self.goals.retain(|g| g.id != id);
        self.canvas.forget(NodeKey::Goal(id));
    }

    fn run_dialog(&mut self, d: Dialog, ctx: &egui::Context) {
        match d {
            Dialog::PickData => self.pick_file(ctx),
            Dialog::SaveAs => {
                self.save_as();
            }
            Dialog::QuickSave => {
                self.quick_save();
            }
            Dialog::LoadProject => self.load_project(ctx),
            Dialog::SaveOutput => self.save_output(),
            Dialog::SaveReply => self.save_reply(),
            Dialog::SaveThen(p) => match self.quick_save() {
                SaveOutcome::Saved => self.perform(p, ctx),
                // Without a save, the follow-up action is not done either (so no changes are lost).
                SaveOutcome::Cancelled => {
                    self.status = trf!("Save cancelled; did not {}", p.label());
                    self.status_err = false;
                }
                SaveOutcome::Failed => self.status.push_str(&trf!(" · did not {}", p.label())),
            },
        }
    }

    fn handle(&mut self, actions: Vec<Action>, ctx: &egui::Context) {
        for a in actions {
            match a {
                Action::PickFile => self.dialog = Some(Dialog::PickData),
                Action::Compile => {
                    if !self.compiling {
                        self.start_compile(ctx)
                    }
                }
                Action::RemoveGoal(id) => self.remove_goal(id),
                Action::AddGoal(t) => self.add_goal(t, true),
                Action::Preset(i) => self.request(Pending::Preset(i), ctx),
                Action::RunLlm => {
                    if self.llm_ready() {
                        self.start_llm(ctx)
                    }
                }
                Action::CancelLlm => self.cancel_llm(),
                Action::RetryLonger(limit) => self.retry_longer(limit, ctx),
                Action::ReloadSample => self.reprofile_if_needed(ctx),
                Action::SaveProject => self.dialog = Some(Dialog::SaveAs),
                Action::QuickSave => self.dialog = Some(Dialog::QuickSave),
                Action::LoadProject => self.request(Pending::OpenDialog, ctx),
                Action::SaveOutput => self.dialog = Some(Dialog::SaveOutput),
                Action::SetLang(lang) => self.switch_lang(lang, ctx),
                Action::OpenKeys => self.keys.show_window(),
            }
        }
    }

    /// Changes the UI language (language picker in the top bar or the settings node). Typed content (role, constraints,
    /// steering rules) stays as it is. The choice is saved to the settings file, and since the existing lint log is in
    /// the old language, unchanged inputs are compiled again.
    fn switch_lang(&mut self, lang: Lang, ctx: &egui::Context) {
        if lang == i18n::current() {
            return;
        }
        i18n::set(lang);
        let refresh = lang_refresh(
            self.result.is_some(),
            self.compiling,
            self.result_key == self.input_key(),
        );
        if refresh == LangRefresh::Recompile {
            // A compile in progress is restarted in the new language. If the user started it, the LLM result is
            // discarded as usual; if it is a language-switch recompile, its reference prompt is carried over.
            let keep_llm_for = if self.compiling {
                self.lang_recompile
                    .take()
                    .filter(|(g, _)| *g == self.compile_gen)
                    .map(|(_, prompt)| prompt)
            } else {
                self.result.as_ref().map(|r| r.output.clone())
            };
            self.start_compile_with(ctx, keep_llm_for);
        }
        match persist_lang(lang, i18n::save_default) {
            Ok(()) => {
                self.status = trf!("Language: {}", lang.native_name());
                self.status_err = false;
            }
            Err(e) => {
                self.status = e;
                self.status_err = true;
            }
        }
        self.update_title(ctx); // title text such as "Untitled"
        ctx.request_repaint();
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = window_title(&self.project, self.dirty, self.blueprint_path.as_deref());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    /// Handles dropped files and shows a hint for a file being dragged.
    fn handle_drops(&mut self, ctx: &egui::Context, modal_open: bool) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        // A drop during a load starts a new load that wins (load_gen).
        if let Some(path) = pick_drop(&dropped).cloned() {
            if modal_open {
                self.status = tr!("Choose whether to save first").into();
                self.status_err = true;
            } else {
                self.open_path(path, ctx);
            }
        }
        self.hover_drop = ctx.input(|i| i.raw.hovered_files.first().map(|f| f.path.as_deref().map(drop_kind)));
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = egui::Modifiers::COMMAND;
        let shift = egui::Modifiers::SHIFT;
        if !ctx.text_edit_focused() {
            // Check Ctrl+Shift+Z first (the Ctrl+Z pattern also matches while Shift is held).
            if ctx.input_mut(|i| i.consume_key(cmd | shift, egui::Key::Z) || i.consume_key(cmd, egui::Key::Y)) {
                self.redo(ctx);
            } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Z)) {
                self.undo(ctx);
            }
        }
        // Ctrl+Enter compiles
        if !self.compiling && ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Enter)) {
            self.start_compile(ctx);
        }
        // Ctrl+Shift+S: save as, Ctrl+S: save to the current file (works even inside a text box)
        if ctx.input_mut(|i| i.consume_key(cmd | shift, egui::Key::S)) {
            self.dialog = Some(Dialog::SaveAs);
        } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::S)) {
            self.dialog = Some(Dialog::QuickSave);
        }
        // Ctrl+O: open a blueprint (asks first when there are unsaved changes)
        if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::O)) {
            self.request(Pending::OpenDialog, ctx);
        }
    }

    /// Asks before opening another blueprint or applying a built-in blueprint when there are unsaved changes.
    fn confirm_modal(&mut self, ctx: &egui::Context, p: &Palette) {
        enum Choice {
            Save,
            Discard,
            Cancel,
        }
        let Some(pending) = self.confirm.clone() else { return };
        let question = match &pending {
            Pending::Preset(i) => trf!(
                "Save your current changes before applying the built-in blueprint “{}”?",
                PRESETS.get(*i).map_or("", |x| x.name.get())
            ),
            Pending::OpenDialog | Pending::OpenPath(_) => {
                tr!("Save your current changes before opening another blueprint?").to_string()
            }
            Pending::Close => tr!("Save your current changes before closing the window?").to_string(),
        };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("unsaved-changes")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(RichText::new(tr!("Unsaved changes")).strong().size(16.0).color(p.hi));
            ui.add_space(4.0);
            ui.label(trf!("“{}” has unsaved changes.", self.project));
            ui.label(question);
            if let Pending::OpenPath(path) = &pending {
                ui.label(RichText::new(path.display().to_string()).small().color(p.sub));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, p, tr!("Save"), true).clicked() {
                    choice = Some(Choice::Save);
                }
                if ui.button(tr!("Don't save")).clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.button(tr!("Cancel")).clicked() {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(Choice::Cancel); // Esc or a click outside
        }
        match choice {
            Some(Choice::Save) => {
                self.confirm = None;
                self.dialog = Some(Dialog::SaveThen(pending));
            }
            Some(Choice::Discard) => {
                self.confirm = None;
                self.perform(pending, ctx);
            }
            Some(Choice::Cancel) => {
                self.confirm = None;
                self.status = tr!("Cancelled").into();
                self.status_err = false;
            }
            None => {}
        }
    }

    // ---------- panels ----------

    /// Top bar (drawn by top_bar_ui). Switches to the chosen language and handles the pressed button.
    fn top_bar(&mut self, ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context) {
        let (act, lang) = top_bar_ui(
            ui,
            p,
            TopBar {
                logo: self.logo.as_ref(),
                project: &mut self.project,
                dirty: self.dirty,
                path: self.blueprint_path.as_deref(),
                target: &mut self.target,
                theme_idx: &mut self.theme_idx,
                need: &mut self.topbar_need,
                compiling: self.compiling,
                can_undo: self.history.can_undo(),
                can_redo: self.history.can_redo(),
            },
        );
        if let Some(l) = lang {
            self.switch_lang(l, ctx);
        }
        match act {
            Some(TopAction::Compile) => {
                if !self.compiling {
                    self.start_compile(ctx);
                }
            }
            Some(TopAction::Undo) => self.undo(ctx),
            Some(TopAction::Redo) => self.redo(ctx),
            None => {}
        }
    }

    fn footer(&self, ui: &mut egui::Ui, p: &Palette) {
        egui::Sides::new().shrink_left().truncate().show(
            ui,
            |ui| match self.hover_drop {
                Some(kind) => {
                    let color = if kind == Some(DropKind::Unsupported) {
                        theme::ERR
                    } else {
                        p.hi
                    };
                    ui.label(RichText::new(drop_hint(kind)).small().color(color));
                }
                None => {
                    let color = if self.status_err { theme::ERR } else { p.sub };
                    let r = ui.label(RichText::new(short(&self.status, 200)).small().color(color));
                    if self.status.chars().count() > 40 {
                        r.on_hover_text(&self.status);
                    }
                }
            },
            // The right side is drawn first (a long status text is truncated on the left), so the "inputs changed"
            // warning is always visible.
            |ui| {
                ui.label(
                    RichText::new(tr!("Ctrl+Enter compile · Ctrl+S save · Ctrl+O open · drop files"))
                        .small()
                        .color(p.sub),
                );
                if self.stale && !self.compiling {
                    ui.label(
                        RichText::new(tr!("● Inputs changed: recompile"))
                            .small()
                            .color(p.warn()),
                    );
                }
            },
        );
    }

    fn left_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // AI API settings are at the top, so they can be set before anything else (keys and endpoints).
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(tr!("🔑 API keys"))
                .on_hover_text(tr!(
                    "Keys for the LLM run, saved for this Windows user (not in the blueprint)"
                ))
                .clicked()
            {
                self.keys.show_window();
            }
            if !self.keys.has_key(self.llm_provider()) {
                ui.label(RichText::new(tr!("No key yet · add one in 🔑 API keys")).color(p.warn()));
            }
        });
        ui.add_space(6.0);
        theme::section_title(ui, p, "I", tr!("Data source"));
        let mut reload = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.loading, egui::Button::new(tr!("📂 Load CSV/JSON")))
                .on_hover_text("CSV·TSV·TXT·JSON·JSONL·NDJSON")
                .clicked()
            {
                self.dialog = Some(Dialog::PickData);
            }
            if self.loading {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label(tr!("Sample rows"));
            let r = ui.add(egui::DragValue::new(&mut self.sample_rows).range(0..=blueprint::MAX_SAMPLE_ROWS));
            reload = r.drag_stopped() || r.lost_focus();
        });
        if reload {
            self.reprofile_if_needed(ui.ctx());
        }
        match &self.loaded {
            Some(d) => {
                ui.label(format!(
                    "{}  ·  {} rows  ·  {} columns",
                    d.data.source_name,
                    d.data.total_rows,
                    d.data.columns.len()
                ))
                .on_hover_text(d.path.display().to_string());
            }
            None => {
                ui.label(RichText::new(tr!("No data loaded (you can also drop a file on the window)")).color(p.sub));
            }
        }

        ui.add_space(10.0);
        theme::section_title(ui, p, "II", tr!("Goals & constraints"));
        ui.label(RichText::new(tr!("Role")).color(p.sub));
        ui.add(
            egui::TextEdit::multiline(&mut self.role)
                .desired_rows(3)
                .desired_width(ui.available_width()),
        );
        let row_w = (ui.available_width() - 70.0).max(80.0);
        let mut remove = None;
        for g in self.goals.iter_mut() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut g.on, "");
                ui.add(
                    egui::TextEdit::multiline(&mut g.text)
                        .id_salt(("goal-form", g.id))
                        .desired_rows(1)
                        .desired_width(row_w),
                );
                if ui
                    .small_button("×")
                    .on_hover_text(tr!("Delete this constraint"))
                    .clicked()
                {
                    remove = Some(g.id);
                }
            });
        }
        if let Some(id) = remove {
            self.remove_goal(id);
        }
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.new_goal)
                    .hint_text(tr!("Add a constraint (Enter)"))
                    .desired_width(row_w),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("＋").clicked() || enter) && !self.new_goal.trim().is_empty() {
                let t = self.new_goal.trim().to_string();
                self.add_goal(t, true);
                self.new_goal.clear();
            }
        });

        ui.add_space(10.0);
        theme::section_title(ui, p, "III", tr!("Enforced output format"));
        ui.radio_value(&mut self.format, OutputFormat::StrictJson, "Strict JSON");
        ui.radio_value(&mut self.format, OutputFormat::MarkdownTable, "Markdown Table");
        ui.label(RichText::new(tr!("Required fields (comma separated)")).color(p.sub));
        ui.add(egui::TextEdit::singleline(&mut self.fields_text).desired_width(ui.available_width()));
        if pipeline::parse_fields(&self.fields_text).is_empty() {
            ui.colored_label(theme::ERR, tr!("No required fields"));
        }
    }

    fn center_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Canvas, RichText::new(tr!("IV · Canvas")).size(15.0));
            ui.selectable_value(&mut self.tab, Tab::XRay, RichText::new("Multi-AST X-Ray").size(15.0));
        });
        ui.add_space(2.0);
        match self.tab {
            Tab::Canvas => {
                let stale = self.stale;
                let provider = self.llm_provider();
                let verdict = self.llm_out.as_ref().map(|run| llm_verdict(&run.out));
                let prompt_tokens = self.result.as_ref().map_or(0, |r| r.comp_tokens);
                let details = self.llm_out.as_ref().map(|run| run_line(run, prompt_tokens));
                let retry = self.llm_out.as_ref().and_then(retry_limit);
                let has_key = self.keys.has_key(provider);
                let fog_theme = self.palette().fog;
                let dirty = self.dirty;
                let actions = {
                    let mut model = Model {
                        role: &mut self.role,
                        goals: &mut self.goals,
                        format: &mut self.format,
                        fields_text: &mut self.fields_text,
                        target: &mut self.target,
                        loaded: self.loaded.as_ref(),
                        result: self.result.as_ref(),
                        loading: self.loading,
                        compiling: self.compiling,
                        stale,
                        project: &mut self.project,
                        dirty,
                        theme_idx: &mut self.theme_idx,
                        fog: &mut self.fog_on,
                        fog_theme,
                        sample_rows: &mut self.sample_rows,
                        llm: LlmView {
                            provider: provider.label(),
                            key_env: provider.default_key_env(),
                            model: &mut self.llm_models[provider_index(provider)],
                            model_hint: provider.default_model(),
                            models: provider.models(),
                            has_key,
                            running: self.llm.running,
                            verdict,
                            opts: &mut self.llm_opts,
                            temp_max: blueprint::temperature_max(provider),
                            details,
                            retry_limit: retry,
                        },
                    };
                    self.canvas.show(ui, &mut model, p)
                };
                let ctx = ui.ctx().clone();
                self.handle(actions, &ctx);
            }
            Tab::XRay => self.xray(ui, p),
        }
    }

    fn xray(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(r) = &self.result else {
            ui.label(RichText::new(tr!("1) Load data (CSV/JSON) or drop a file on the window")).color(p.sub));
            ui.label(RichText::new(tr!("2) Tick the constraints and press Compile & Run (Ctrl+Enter)")).color(p.sub));
            return;
        };
        let ast = &r.ast;
        // The titles contain changing values (row count, lint state, format), so a fixed id_salt keeps the open state.
        egui::ScrollArea::vertical()
            .id_salt("ast")
            .max_height(ui.available_height() * 0.6)
            .show(ui, |ui| {
                egui::CollapsingHeader::new("▼ RootPromptAST")
                    .id_salt("xray-root")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!("target: {}", ast.target_model.label()));
                        egui::CollapsingHeader::new(format!("RoleNode ({})", ast.role.name))
                            .id_salt("xray-role")
                            .show(ui, |ui| {
                                ui.label(&ast.role.instruction);
                            });
                        match &ast.data_sub_ast {
                            Some(d) => {
                                egui::CollapsingHeader::new(format!("DataSubAST (Compressed, {} rows)", d.total_rows))
                                    .id_salt("xray-data")
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for c in &d.columns {
                                            ui.label(format!(
                                                "{} : {} | null {:.1}% | {}",
                                                c.name,
                                                c.dtype,
                                                c.null_ratio * 100.0,
                                                c.summary_stats
                                            ));
                                        }
                                        if !d.anomalies.is_empty() {
                                            ui.colored_label(p.warn(), "Outliers");
                                            for a in &d.anomalies {
                                                ui.label(format!("  • {a}"));
                                            }
                                        }
                                    });
                            }
                            None => {
                                ui.label(RichText::new(tr!("DataSubAST: (no data)")).color(p.sub));
                            }
                        }
                        let lint_ok = !ast.lint_warnings.iter().any(|d| d.severity != "INFO");
                        egui::CollapsingHeader::new(format!(
                            "ConstraintSubAST ({})",
                            if lint_ok { "Lint OK" } else { tr!("Lint warnings") }
                        ))
                        .id_salt("xray-constraints")
                        .default_open(true)
                        .show(ui, |ui| {
                            for c in &ast.constraints {
                                constraint_row(ui, c);
                            }
                        });
                        egui::CollapsingHeader::new(format!(
                            "OutputSchemaAST ({})",
                            match ast.output_schema.format {
                                OutputFormat::StrictJson => "JSON",
                                OutputFormat::MarkdownTable => "Table",
                            }
                        ))
                        .id_salt("xray-schema")
                        .show(ui, |ui| {
                            ui.monospace(&ast.output_schema.raw_schema);
                        });
                    });
            });
        if !self.compare.is_empty() {
            ui.separator();
            compare_grid(ui, p, &self.compare, ast.target_model);
        }
        ui.separator();
        theme::section_title(ui, p, "V", tr!("Live lint log"));
        egui::ScrollArea::vertical().id_salt("lint").show(ui, |ui| {
            for d in &ast.lint_warnings {
                let (icon, color) = match d.severity.as_str() {
                    "INFO" => ("✔", theme::OK),
                    "WARN" => ("⚠", p.warn()),
                    _ => ("✖", theme::ERR),
                };
                ui.colored_label(color, format!("{icon} {}", d.message));
            }
        });
    }

    fn right_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        theme::section_title(ui, p, "VI", tr!("Token savings"));
        let Some(r) = &self.result else {
            ui.label(RichText::new(tr!("Compile to see the token savings and the final prompt here.")).color(p.sub));
            return;
        };
        let good = r.saving_pct >= 0.0;
        ui.label(
            RichText::new(format!(
                "{:+.1}%",
                if r.saving_pct == 0.0 { 0.0 } else { -r.saving_pct }
            ))
            .size(44.0)
            .color(if good { p.hi } else { theme::ERR }),
        );
        let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
        ui.painter().rect_filled(bar, 3.0, p.line.gamma_multiply(0.5));
        let fill = egui::Rect::from_min_size(
            bar.min,
            egui::vec2(
                bar.width() * (r.saving_pct / 100.0).clamp(0.0, 1.0) as f32,
                bar.height(),
            ),
        );
        theme::paint_h_gradient(ui.painter(), fill, p.hi, p.accent);
        ui.label(trf!("Original {} → compressed {} tokens", r.orig_tokens, r.comp_tokens));
        ui.label(
            RichText::new(trf!(
                "Saved ${:.4}  ·  o200k_base approximation, prices are estimates",
                r.saved_usd
            ))
            .small()
            .color(p.sub),
        );

        ui.add_space(8.0);
        theme::section_title(ui, p, "VII", tr!("Final prompt"));
        ui.horizontal(|ui| {
            if ui.button(tr!("Copy")).clicked() {
                ui.ctx().copy_text(r.output.clone());
            }
            if ui.button(tr!("Save to file")).clicked() {
                self.dialog = Some(Dialog::SaveOutput);
            }
        });
        let h = ui.available_height() * 0.55;
        egui::ScrollArea::both().id_salt("out").max_height(h).show(ui, |ui| {
            egui::Frame::new()
                .fill(p.panel2)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(&r.output).monospace().color(p.sub)).selectable(true));
                });
        });
        ui.add_space(6.0);
        if self.llm_window {
            self.llm_docked_note(ui, p);
        } else {
            self.llm_section(ui, p, false);
        }
    }

    /// What the right pane shows in place of VIII while it is in its own window.
    fn llm_docked_note(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.result.is_none() {
            return;
        }
        theme::section_title(ui, p, "VIII", tr!("LLM run result"));
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr!("Shown in a separate window")).color(p.sub));
            if ui.small_button(tr!("↙ Back to the pane")).clicked() {
                self.llm_window = false;
            }
        });
    }

    /// Draws VIII in its own OS window while `llm_window` is on. Closing the window puts VIII back in the right pane.
    fn llm_window_viewport(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.llm_window {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Candy-Corn · {}", tr!("LLM run result")))
            .with_inner_size([760.0, 720.0])
            .with_min_inner_size([420.0, 320.0]);
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("llm-result-window"),
            builder,
            |ui, _class| {
                if ui.input(|i| i.viewport().close_requested()) {
                    self.llm_window = false;
                    return;
                }
                egui::CentralPanel::default().frame(theme::backdrop(p)).show(ui, |ui| {
                    theme::card(p).show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        if self.result.is_none() {
                            ui.label(RichText::new(tr!("Compile first to run the prompt")).color(p.sub));
                            if ui.small_button(tr!("↙ Back to the pane")).clicked() {
                                self.llm_window = false;
                            }
                        } else {
                            self.llm_section(ui, p, true);
                        }
                    });
                });
            },
        );
    }

    fn llm_section(&mut self, ui: &mut egui::Ui, p: &Palette, in_window: bool) {
        enum Act {
            Run,
            Cancel,
            Retry(u32),
            Keys,
            Window(bool),
            SaveReply,
        }
        let Some((target, preview, has_data, prompt_tokens, fields)) = self.result.as_ref().map(|r| {
            (
                r.ast.target_model,
                r.preview.clone(),
                r.ast.data_sub_ast.is_some(),
                r.comp_tokens,
                r.ast.output_schema.required_fields.clone(),
            )
        }) else {
            return;
        };
        theme::section_title(ui, p, "VIII", tr!("LLM run result"));
        let provider = provider_for(target);
        let has_key = self.keys.has_key(provider);
        let stale = self.stale;
        let running = self.llm.running;
        let ready = has_key && self.llm_ready();
        let mut act = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(provider.label());
            keys::model_menu(ui, provider.models(), &mut self.llm_models[provider_index(provider)]);
            ui.add(
                egui::TextEdit::singleline(&mut self.llm_models[provider_index(provider)])
                    .desired_width(150.0)
                    .hint_text(provider.default_model()),
            );
            let label = if running { tr!("Running") } else { tr!("▶ Run live") };
            let btn = theme::primary_button(ui, p, label, ready);
            // Show the hint only when there is no key (not when the button is disabled during a run).
            let btn = if has_key {
                btn
            } else {
                btn.on_disabled_hover_text(trf!(
                    "Add your {} API key in 🔑 API keys (or set {})",
                    provider.label(),
                    provider.default_key_env()
                ))
            };
            if btn.clicked() {
                act = Some(Act::Run);
            }
            if ui.small_button(tr!("🔑 API keys")).clicked() {
                act = Some(Act::Keys);
            }
            if in_window {
                if ui
                    .small_button(tr!("↙ Back to the pane"))
                    .on_hover_text(tr!("Close this window and show the result in the right pane again"))
                    .clicked()
                {
                    act = Some(Act::Window(false));
                }
            } else if ui
                .small_button(tr!("↗ Separate window"))
                .on_hover_text(tr!("Open the LLM run result in its own window"))
                .clicked()
            {
                act = Some(Act::Window(true));
            }
            if running {
                ui.spinner();
                if ui
                    .button(tr!("■ Cancel"))
                    .on_hover_text(tr!("Discard the reply without waiting"))
                    .clicked()
                {
                    act = Some(Act::Cancel);
                }
            }
        });
        // The title includes a summary of the current values, so it changes; a fixed id keeps the open state.
        let temp_max = blueprint::temperature_max(provider);
        egui::CollapsingHeader::new(trf!(
            "Run options · {}",
            studio_canvas::options_summary(&self.llm_opts, temp_max)
        ))
        .id_salt("viii-llm-opts")
        .show(ui, |ui| {
            studio_canvas::llm_options_ui(ui, &mut self.llm_opts, temp_max, "viii", p)
        });
        let with_system = if self.llm_opts.system.trim().is_empty() {
            ""
        } else {
            tr!(" (including the system prompt)")
        };
        let note = match (has_key && !stale, has_data) {
            _ if has_key && stale => tr!("Inputs changed. Recompile before running").to_string(),
            (true, true) => trf!(
                "Running sends the whole final output (file name, column stats, outliers, sample rows){with_system} to {}",
                provider.label(),
                with_system = with_system
            ),
            (true, false) => trf!(
                "Running sends the final prompt{with_system} to {}",
                provider.label(),
                with_system = with_system
            ),
            (false, _) => trf!(
                "No {} API key, so only the schema skeleton (mock reply) is shown. Add one in 🔑 API keys",
                provider.label()
            ),
        };
        ui.label(RichText::new(note).small().color(p.sub));
        if !self.llm_history.is_empty() {
            let mut clear = false;
            egui::CollapsingHeader::new(trf!("Run history ({n})", n = self.llm_history.len()))
                .id_salt("viii-history")
                .show(ui, |ui| {
                    llm_view::latency_chart(ui, p, &self.llm_history);
                    llm_view::history_table(ui, p, &self.llm_history);
                    clear = ui.small_button(tr!("Clear history")).clicked();
                });
            if clear {
                self.llm_history.clear();
            }
        }
        egui::ScrollArea::vertical()
            .id_salt("preview")
            .show(ui, |ui| match &self.llm_out {
                None => {
                    ui.add(egui::Label::new(RichText::new(&preview).monospace().color(p.sub)).selectable(true));
                }
                Some(run) => {
                    let c = match &run.out {
                        Ok(c) => c,
                        Err(e) => {
                            ui.colored_label(theme::ERR, e);
                            ui.label(RichText::new(run_line(run, prompt_tokens)).small().color(p.sub));
                            return;
                        }
                    };
                    for (tone, line) in check_lines(c) {
                        ui.colored_label(tone_color(tone, p), line);
                    }
                    llm_view::field_chips(ui, p, &llm_view::field_states(&fields, &c.missing, &c.nulls));
                    if let Some(u) = c.usage {
                        ui.add_space(2.0);
                        llm_view::token_bars(ui, p, u, prompt_tokens, run.max_tokens);
                    }
                    ui.label(
                        RichText::new(trf!(
                            "Stop reason: {}",
                            c.stop_reason.as_deref().unwrap_or(tr!("(not in the reply)"))
                        ))
                        .small()
                        .color(p.sub),
                    );
                    ui.label(RichText::new(run_line(run, prompt_tokens)).small().color(p.sub));
                    let empty = c.pretty.trim().is_empty();
                    ui.horizontal_wrapped(|ui| {
                        if let Some(limit) = retry_limit(run)
                            && ui
                                .add_enabled(
                                    ready,
                                    egui::Button::new(trf!("Retry with 2× limit ({limit})", limit = limit)),
                                )
                                .on_hover_text(tr!("Doubles the output token limit and sends the same prompt again"))
                                .clicked()
                        {
                            act = Some(Act::Retry(limit));
                        }
                        if ui
                            .add_enabled(!empty, egui::Button::new(tr!("Copy reply")).small())
                            .clicked()
                        {
                            ui.ctx().copy_text(c.pretty.clone());
                        }
                        if ui
                            .add_enabled(!empty, egui::Button::new(tr!("Save reply…")).small())
                            .clicked()
                        {
                            act = Some(Act::SaveReply);
                        }
                    });
                    if empty {
                        ui.label(RichText::new(tr!("(empty reply)")).monospace().color(p.sub));
                    } else {
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut self.llm_tab,
                                llm_view::ReplyTab::Structured,
                                tr!("📊 Structured"),
                            );
                            ui.selectable_value(&mut self.llm_tab, llm_view::ReplyTab::Text, tr!("📝 Text"));
                        });
                        ui.separator();
                        let drawn = self.llm_tab == llm_view::ReplyTab::Structured && {
                            let ok = llm_view::structured_view(ui, p, &c.pretty);
                            if !ok {
                                ui.label(
                                    RichText::new(tr!("No JSON or table in the reply; showing the text"))
                                        .small()
                                        .color(p.sub),
                                );
                            }
                            ok
                        };
                        if !drawn {
                            ui.add(
                                egui::Label::new(RichText::new(&c.pretty).monospace().color(p.sub)).selectable(true),
                            );
                        }
                    }
                }
            });
        let ctx = ui.ctx().clone();
        match act {
            Some(Act::Run) => self.start_llm(&ctx),
            Some(Act::Cancel) => self.cancel_llm(),
            Some(Act::Retry(limit)) => self.retry_longer(limit, &ctx),
            Some(Act::Keys) => self.keys.show_window(),
            Some(Act::Window(on)) => self.llm_window = on,
            Some(Act::SaveReply) => self.dialog = Some(Dialog::SaveReply),
            None => {}
        }
    }
}

/// Values drawn in the top bar, and the width needed for one row.
struct TopBar<'a> {
    logo: Option<&'a egui::TextureHandle>,
    project: &'a mut String,
    dirty: bool,
    path: Option<&'a Path>,
    target: &'a mut TargetModel,
    theme_idx: &'a mut usize,
    /// Width needed to draw everything on one row (measured in the last frame, and measured again on every draw)
    need: &'a mut f32,
    compiling: bool,
    can_undo: bool,
    can_redo: bool,
}

/// Draws the top bar. If it does not fit on one row (at the minimum window width of 1000 px, for example), the target,
/// theme and language pickers move to a second row.
/// The one-row width comes from each group's width measured in the last frame, so it stays right for any font, theme
/// name or language.
/// Returns (pressed button, newly chosen language).
fn top_bar_ui(ui: &mut egui::Ui, p: &Palette, bar: TopBar<'_>) -> (Option<TopAction>, Option<Lang>) {
    let TopBar {
        logo,
        project,
        dirty,
        path,
        target,
        theme_idx,
        need,
        compiling,
        can_undo,
        can_redo,
    } = bar;
    let compact = ui.available_width() < *need;
    let gap = ui.spacing().item_spacing.x;
    let mut brand_w = 0.0;
    let mut sel_w = 0.0;
    let mut lang = None;
    let (left_w, (right_w, act)) = egui::Sides::new().height(46.0).show(
        ui,
        |ui| {
            let x0 = ui.cursor().left();
            brand(ui, logo, p);
            brand_w = ui.cursor().left() - x0;
            ui.add_space(16.0);
            project_field(ui, project, dirty, path, p);
            if !compact {
                (sel_w, lang) = selectors(ui, target, theme_idx);
            }
            ui.min_rect().width()
        },
        |ui| top_actions(ui, p, compiling, can_undo, can_redo),
    );
    if compact {
        ui.horizontal(|ui| {
            ui.add_space(brand_w + 16.0);
            (sel_w, lang) = selectors(ui, target, theme_idx);
        });
    }
    let one_row = if compact { left_w + gap + sel_w } else { left_w };
    *need = one_row + gap + right_w + 24.0;
    (act, lang)
}

/// Logo and name on the left of the top bar.
fn brand(ui: &mut egui::Ui, logo: Option<&egui::TextureHandle>, p: &Palette) {
    match logo {
        Some(tex) => {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), [38.0, 38.0])).corner_radius(9));
        }
        None => {
            let (logo, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
            theme::pumpkin_logo(ui.painter(), logo.center(), 38.0, p);
        }
    }
    ui.vertical(|ui| {
        ui.label(RichText::new("CANDY-CORN").size(20.0).strong().color(p.hi));
        ui.label(RichText::new(p.tag.get()).size(10.0).color(p.accent));
    });
}

/// Project name and the unsaved-changes mark (●). The mark's slot is always reserved so the top bar width does not
/// change.
fn project_field(ui: &mut egui::Ui, project: &mut String, dirty: bool, path: Option<&Path>, p: &Palette) {
    ui.label(tr!("Project"));
    ui.add(
        egui::TextEdit::singleline(project)
            .id_salt("top-project")
            .desired_width(150.0),
    )
    .on_hover_text(path.map_or_else(
        || tr!("Blueprint not saved to a file yet (Ctrl+S to save)").to_string(),
        |p| p.display().to_string(),
    ));
    let (slot, resp) = ui.allocate_exact_size(egui::vec2(12.0, 18.0), egui::Sense::hover());
    if dirty {
        ui.painter().text(
            slot.center(),
            egui::Align2::CENTER_CENTER,
            "●",
            egui::FontId::proportional(13.0),
            p.warn(),
        );
        resp.on_hover_text(tr!("Unsaved changes (Ctrl+S to save)"));
    }
}

/// Target, theme and language pickers. Returns (drawn width, newly chosen language).
fn selectors(ui: &mut egui::Ui, target: &mut TargetModel, theme_idx: &mut usize) -> (f32, Option<Lang>) {
    let x0 = ui.cursor().left();
    ui.label(tr!("Target"));
    egui::ComboBox::from_id_salt("target")
        .selected_text(target.label())
        .show_ui(ui, |ui| {
            for t in TargetModel::ALL {
                ui.selectable_value(target, t, t.label());
            }
        });
    ui.label(tr!("Theme"));
    egui::ComboBox::from_id_salt("theme")
        .width(190.0)
        .selected_text(THEMES[(*theme_idx).min(THEMES.len() - 1)].name.get())
        .show_ui(ui, |ui| {
            for (i, t) in THEMES.iter().enumerate() {
                ui.selectable_value(theme_idx, i, t.name.get());
            }
        });
    let lang = studio_canvas::lang_picker(ui, "top-lang");
    (ui.cursor().left() - x0, lang)
}

/// Buttons on the right of the top bar (laid out from right to left). Returns (drawn width, pressed button).
fn top_actions(
    ui: &mut egui::Ui,
    p: &Palette,
    compiling: bool,
    can_undo: bool,
    can_redo: bool,
) -> (f32, Option<TopAction>) {
    let mut act = None;
    if theme::primary_button(ui, p, "⚡ Compile & Run", !compiling)
        .on_hover_text("Ctrl+Enter")
        .clicked()
    {
        act = Some(TopAction::Compile);
    }
    // The spinner's slot is always reserved so the width does not change while compiling.
    let spin = egui::vec2(18.0, 18.0);
    if compiling {
        ui.add_sized(spin, egui::Spinner::new());
    } else {
        ui.allocate_exact_size(spin, egui::Sense::hover());
    }
    ui.add_space(8.0);
    if ui
        .add_enabled(can_redo, egui::Button::new(tr!("Redo")))
        .on_hover_text("Ctrl+Shift+Z / Ctrl+Y")
        .clicked()
    {
        act = Some(TopAction::Redo);
    }
    if ui
        .add_enabled(can_undo, egui::Button::new(tr!("Undo")))
        .on_hover_text(undo_hint())
        .clicked()
    {
        act = Some(TopAction::Undo);
    }
    (ui.min_rect().width(), act)
}

/// Target comparison table of the X-Ray tab: token counts and estimated input costs of the same input compiled for
/// the three targets.
fn compare_grid(ui: &mut egui::Ui, p: &Palette, rows: &[TargetCost], current: TargetModel) {
    ui.label(
        RichText::new(tr!("Target comparison · the same input compiled for each target"))
            .color(p.accent)
            .strong(),
    );
    let cheapest = rows.iter().map(|c| c.cost_usd).fold(f64::INFINITY, f64::min);
    egui::Grid::new("xray-compare")
        .striped(true)
        .num_columns(5)
        .show(ui, |ui| {
            for h in [
                tr!("Target"),
                tr!("Tokens"),
                tr!("vs. original"),
                tr!("Input cost (est.)"),
                "",
            ] {
                ui.label(RichText::new(h).small().color(p.sub));
            }
            ui.end_row();
            for c in rows {
                let name = RichText::new(c.target.label());
                ui.label(if c.target == current {
                    name.strong().color(p.hi)
                } else {
                    name
                });
                ui.label(c.comp_tokens.to_string());
                ui.label(format!(
                    "{:+.1}%",
                    if c.saving_pct == 0.0 { 0.0 } else { -c.saving_pct }
                ));
                ui.label(format!("${:.5}", c.cost_usd));
                let mut marks = Vec::new();
                if c.target == current {
                    marks.push(tr!("current"));
                }
                if c.cost_usd <= cheapest && rows.len() > 1 {
                    marks.push(tr!("cheapest"));
                }
                ui.label(RichText::new(marks.join(" · ")).small().color(p.sub));
                ui.end_row();
            }
        });
    ui.label(
        RichText::new(tr!(
            "o200k_base approximation · prices are estimates per 1M input tokens"
        ))
        .small()
        .color(p.sub),
    );
}

fn constraint_row(ui: &mut egui::Ui, c: &ConstraintNode) {
    ui.label(format!(
        "[{}] {} (p{}): {}",
        c.id,
        c.rule_type.tag(),
        c.priority,
        c.expression
    ));
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        if self.theme_idx != self.applied_theme {
            self.theme_idx = self.theme_idx.min(THEMES.len() - 1);
            theme::apply(&ctx, &THEMES[self.theme_idx]);
            self.applied_theme = self.theme_idx;
        }
        self.stale = self.result.is_some() && self.result_key != self.input_key();
        // Also give a place to constraints added while the canvas is not shown (X-Ray tab), so that going back to the
        // canvas after saving does not create new positions and turn the state into "unsaved changes".
        self.canvas.ensure_positions(&self.goals);
        self.dirty = self.saved.is_dirty(&self.to_blueprint(None));
        // Closing the window: with unsaved changes, the close is put off and the user is asked.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.dirty {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.confirm.is_none() {
                self.confirm = Some(Pending::Close);
            }
        }
        self.update_title(&ctx);
        let p = self.palette();
        theme::paint_backdrop(ui.painter(), ctx.content_rect(), &p);
        theme::paint_fog(&ctx, &p, self.fog_on);

        // While the save prompt or the API keys window is open, shortcuts and file drops are not accepted.
        let modal_open = self.confirm.is_some() || self.keys.open;
        self.handle_drops(&ctx, modal_open);
        if !modal_open {
            self.shortcuts(&ctx);
        }

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 8))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| self.top_bar(ui, &p, &ctx));

        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 5))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| self.footer(ui, &p));

        // Fit both side panels' maximum widths to the window width so the center canvas stays even in a narrow window.
        let (left_max, right_max) = side_panel_max(ctx.content_rect().width());

        egui::Panel::left("left")
            .resizable(true)
            .default_size(340.0)
            .size_range(280.0..=left_max)
            .frame(theme::backdrop(&p))
            .show(ui, |ui| {
                theme::card(&p).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    egui::ScrollArea::vertical()
                        .id_salt("left_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.left_pane(ui, &p));
                });
            });
        egui::Panel::right("right")
            .resizable(true)
            .default_size(440.0)
            .size_range(320.0..=right_max)
            .frame(theme::backdrop(&p))
            .show(ui, |ui| {
                theme::card(&p).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    self.right_pane(ui, &p);
                });
            });
        egui::CentralPanel::default().frame(theme::backdrop(&p)).show(ui, |ui| {
            theme::card(&p).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.center_pane(ui, &p);
            });
        });
        // Record one step once editing pauses briefly.
        let now = ctx.input(|i| i.time);
        if self.history.track(&self.snapshot(), now) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(history::SETTLE_SECS));
        }
        self.llm_window_viewport(&ctx, &p);
        self.confirm_modal(&ctx, &p);
        self.keys.show(&ctx);
        // File dialogs run after all widgets have been handled.
        if let Some(d) = self.dialog.take() {
            self.run_dialog(d, &ctx);
        }
    }
}

/// Appends the working folder's steering rules that are not in the blueprint, switched on.
/// Rules already in the blueprint (including switched-off ones) are left as they are.
fn merge_steering(mut bp: Blueprint, rules: &[String]) -> Blueprint {
    for rule in rules {
        if !bp.goals.iter().any(|g| g.text.trim() == rule.trim()) {
            bp.goals.push(GoalSpec {
                text: rule.clone(),
                on: true,
                pos: None,
            });
        }
    }
    bp
}

/// Data file reference. It keeps the data used for display and compiling (shown) apart from a request that is in
/// progress, failed or missing (pending), so a failed load still points at the previous data and a missing file does
/// not lose the blueprint's reference. Each entry is (path, the sample row count used when profiling that path).
#[derive(Default, Debug, Clone, PartialEq)]
struct DataRef {
    shown: Option<(PathBuf, usize)>,
    pending: Option<(PathBuf, usize)>,
}

impl DataRef {
    fn requested(&mut self, path: PathBuf, rows: usize) {
        self.pending = Some((path, rows));
    }

    /// When the file the blueprint points at does not exist: nothing is loaded, only the reference is kept.
    fn missing(&mut self, path: PathBuf, rows: usize) {
        self.pending = Some((path, rows));
    }

    fn succeeded(&mut self) {
        if let Some(p) = self.pending.take() {
            self.shown = Some(p);
        }
    }

    /// Failure: if data is shown, falls back to it (and returns that data's sample row count);
    /// otherwise the requested file stays as the reference.
    fn failed(&mut self) -> Option<usize> {
        let (_, rows) = self.shown.as_ref()?;
        self.pending = None;
        Some(*rows)
    }

    fn current(&self) -> Option<&(PathBuf, usize)> {
        self.pending.as_ref().or(self.shown.as_ref())
    }

    /// Data file to save in the blueprint
    fn source(&self) -> Option<&Path> {
        self.current().map(|(p, _)| p.as_path())
    }

    /// File to profile again when the sample row count changed (the pending request's file, if there is one)
    fn reprofile_target(&self, want_rows: usize) -> Option<PathBuf> {
        let (path, rows) = self.current()?;
        (want_rows != *rows && path.is_file()).then(|| path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_csv(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cc_dataref_{}_{name}.csv", std::process::id()));
        std::fs::write(&p, "a\n1\n").unwrap();
        p
    }

    #[test]
    fn data_ref_missing_blueprint_file_keeps_reference() {
        let missing = PathBuf::from("Z:/no/such/data.csv");
        let mut d = DataRef::default();
        d.missing(missing.clone(), 8);
        assert_eq!(d.source(), Some(missing.as_path())); // data_path survives saving again
        assert_eq!(d.reprofile_target(12), None); // a missing file is not profiled again
    }

    #[test]
    fn data_ref_request_b_while_a_shown_targets_b() {
        let (a, b) = (temp_csv("a1"), temp_csv("b1"));
        let mut d = DataRef::default();
        d.requested(a.clone(), 8);
        d.succeeded();
        d.requested(b.clone(), 8); // B is loading
        assert_eq!(d.source(), Some(b.as_path()));
        assert_eq!(d.reprofile_target(12), Some(b.clone()));
        assert_eq!(d.reprofile_target(8), None);
        d.succeeded();
        assert_eq!(d.shown, Some((b.clone(), 8)));
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }

    #[test]
    fn data_ref_failed_load_falls_back_to_shown_data() {
        let (a, b) = (temp_csv("a2"), temp_csv("b2"));
        let mut d = DataRef::default();
        d.requested(a.clone(), 8);
        d.succeeded();
        d.requested(b.clone(), 12);
        assert_eq!(d.failed(), Some(8)); // B is a broken file: fall back to the shown A (8 rows)
        assert_eq!(d.source(), Some(a.as_path()));
        assert_eq!(d.reprofile_target(12), Some(a.clone())); // A again, with 12 rows
        assert_eq!(d.reprofile_target(8), None); // A already has 8 rows
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }

    #[test]
    fn data_ref_first_load_failure_keeps_reference() {
        let b = temp_csv("b3");
        let mut d = DataRef::default();
        d.requested(b.clone(), 8);
        assert_eq!(d.failed(), None); // no data to fall back to
        assert_eq!(d.source(), Some(b.as_path()));
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn steering_rules_are_added_once_and_disabled_ones_stay_off() {
        let rule_a = ko_fixture!("studio.steering.rule_a");
        let other = ko_fixture!("studio.steering.other");
        let rule_b = ko_fixture!("studio.steering.rule_b");
        let bp = Blueprint {
            goals: vec![
                GoalSpec {
                    text: rule_a.into(),
                    on: false,
                    pos: None,
                },
                GoalSpec {
                    text: other.into(),
                    on: true,
                    pos: None,
                },
            ],
            ..Blueprint::default()
        };
        let merged = merge_steering(bp, &[format!("{rule_a} "), rule_b.to_string()]);
        let texts: Vec<(&str, bool)> = merged.goals.iter().map(|g| (g.text.as_str(), g.on)).collect();
        assert_eq!(texts, vec![(rule_a, false), (other, true), (rule_b, true)]);
    }

    #[test]
    fn provider_follows_target() {
        assert_eq!(provider_for(TargetModel::Claude), Provider::Anthropic);
        assert_eq!(provider_for(TargetModel::Gpt), Provider::OpenAi);
        assert_eq!(provider_for(TargetModel::Gemini), Provider::Gemini);
        for p in Provider::ALL {
            assert_eq!(Provider::ALL[provider_index(p)], p);
        }
    }

    #[test]
    fn short_status_collapses_whitespace_and_truncates() {
        assert_eq!(short("a\n  b", 10), "a b");
        assert_eq!(
            short(ko_fixture!("studio.short.text"), 3),
            ko_fixture!("studio.short.truncated")
        );
    }

    #[test]
    fn dropped_files_are_routed_by_extension() {
        for (name, kind) in [
            ("plan.kiro", DropKind::Blueprint),
            ("C:/a b/PLAN.KIRO", DropKind::Blueprint),
            ("sales.csv", DropKind::Data),
            ("sales.TSV", DropKind::Data),
            ("notes.txt", DropKind::Data),
            ("d.json", DropKind::Data),
            ("d.JSONL", DropKind::Data),
            ("d.ndjson", DropKind::Data),
            ("photo.png", DropKind::Unsupported),
            ("plan.kiro.bak", DropKind::Unsupported),
            ("README", DropKind::Unsupported),
            ("archive.csv.zip", DropKind::Unsupported),
        ] {
            assert_eq!(drop_kind(Path::new(name)), kind, "{name}");
        }
        let png = PathBuf::from("a.png");
        let csv = PathBuf::from("b.csv");
        assert_eq!(pick_drop(&[png.clone(), csv.clone()]), Some(&csv)); // picks the file that can be opened
        assert_eq!(pick_drop(std::slice::from_ref(&png)), Some(&png)); // otherwise the first file, to report the error
        assert_eq!(pick_drop(&[]), None);
        i18n::with_lang(Lang::Ko, || {
            assert_eq!(
                drop_hint(Some(DropKind::Unsupported)),
                ko_text!("Unsupported file type (CSV, TSV, TXT, JSON, JSONL, NDJSON data or a .kiro blueprint)")
            );
            assert_eq!(
                drop_hint(Some(DropKind::Blueprint)),
                ko_text!("Drop here to open the blueprint")
            );
        });
    }

    fn goal(id: u64, text: &str, on: bool) -> Goal {
        Goal {
            id,
            text: text.into(),
            on,
        }
    }

    fn key_of(goals: &[Goal], fields: &str, data: Option<u64>) -> String {
        let role = ko_fixture!("studio.stale.role");
        let req = compile_inputs(TargetModel::Claude, role, goals, OutputFormat::StrictJson, fields);
        compile_key(&req, data)
    }

    #[test]
    fn stale_key_uses_the_normalized_request() {
        let text = ko_fixture!("studio.stale.goal");
        let base = key_of(&[goal(1, text, true)], "a,b", Some(1));
        // An added empty constraint, spaces and line breaks, switched-off and blank constraints, and the comma spacing
        // of the fields do not change the request that is sent
        let same = [
            key_of(&[goal(1, text, true), goal(2, "", true)], "a,b", Some(1)),
            key_of(
                &[goal(1, ko_fixture!("studio.stale.goal_spaced"), true)],
                "a,b",
                Some(1),
            ),
            key_of(
                &[goal(1, text, true), goal(2, ko_fixture!("studio.goal_off"), false)],
                "a,b",
                Some(1),
            ),
            key_of(&[goal(1, text, true), goal(2, "   ", true)], " a , b ,", Some(1)),
        ];
        for k in same {
            assert_eq!(k, base);
        }
        // Inputs that really change give a different key
        let different = [
            key_of(&[goal(1, text, false)], "a,b", Some(1)),
            key_of(
                &[goal(1, ko_fixture!("studio.stale.goal_changed"), true)],
                "a,b",
                Some(1),
            ),
            key_of(&[goal(1, text, true)], "a,c", Some(1)),
            key_of(&[goal(1, text, true)], "a,b", Some(2)),
            key_of(&[goal(1, text, true)], "a,b", None),
        ];
        for k in different {
            assert_ne!(k, base);
        }
        let req = compile_inputs(
            TargetModel::Gpt,
            "r",
            &[goal(1, " a  b ", true), goal(2, "", true)],
            OutputFormat::MarkdownTable,
            "x, y",
        );
        assert_eq!(req.constraints, vec!["a b".to_string()]);
        assert_eq!(req.fields, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn data_fingerprint_changes_with_profile_content() {
        let dir = std::env::temp_dir().join(format!("cc_datafp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.csv");
        std::fs::write(&p, "id,amount\n1,10\n2,20\n3,30\n4,40\n").unwrap();
        let a = pipeline::load_data(&p, 2).unwrap();
        let again = pipeline::load_data(&p, 2).unwrap();
        let more = pipeline::load_data(&p, 3).unwrap();
        assert_eq!(data_fingerprint(&a), data_fingerprint(&again));
        assert_ne!(data_fingerprint(&a), data_fingerprint(&more));
        assert_eq!(delimiter_warning(&a), None);
        // A tab-separated file read as one column gets a warning (once the profiler detects the delimiter, the warning
        // goes away)
        let tsv = dir.join("t.tsv");
        std::fs::write(&tsv, "id\tamount\tregion\n1\t10\tA\n2\t20\tB\n").unwrap();
        let t = pipeline::load_data(&tsv, 2).unwrap();
        if t.data.columns.len() == 1 {
            assert!(delimiter_warning(&t).is_some());
        } else {
            assert_eq!(delimiter_warning(&t), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review repro: a comma-separated file named x.tsv was read as one column "id,region,amount", with no warning.
    /// The profiler now picks the comma, and if a single column still contains commas or pipes, there is a warning.
    #[test]
    fn delimiter_warning_flags_commas_and_pipes_in_a_single_column() {
        let dir = std::env::temp_dir().join(format!("cc_delim_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [
            ("x.tsv", "id,region,amount\n1,A,10\n2,B,20\n"),
            ("p.tsv", "id|region|amount\n1|A|10\n2|B|20\n"),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            let d = pipeline::load_data(&p, 2).unwrap();
            let names: Vec<&str> = d.data.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, ["id", "region", "amount"], "{name}");
            assert_eq!(delimiter_warning(&d), None, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let one = |name: &str| LoadedData {
            path: PathBuf::from("d.tsv"),
            data: crate::ast::DataSubAst {
                source_name: "d.tsv".into(),
                total_rows: 1,
                columns: vec![crate::ast::ColumnProfile {
                    name: name.into(),
                    dtype: "text".into(),
                    null_ratio: 0.0,
                    summary_stats: "distinct:1".into(),
                }],
                anomalies: vec![],
                sample_rows: vec![],
            },
            raw_tokens: 1,
        };
        for name in ["id,region,amount", "id|region", "a\tb", "a;b"] {
            let w = i18n::with_lang(Lang::Ko, || delimiter_warning(&one(name)));
            assert_eq!(
                w,
                Some(ko_text!(
                    "⚠ Read as a single column: the header still has tabs, semicolons, commas or pipes; check the delimiter"
                )),
                "{name:?}"
            );
        }
        assert_eq!(delimiter_warning(&one("amount")), None);
    }

    fn reply(text: &str, stop: Option<&str>) -> llm::LlmReply {
        llm::LlmReply {
            text: text.into(),
            stop_reason: stop.map(String::from),
            usage: None,
        }
    }

    fn fields(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The fixed text of a translated template in front of its first placeholder.
    fn lead(template: &'static str) -> &'static str {
        template.find('{').map_or(template, |i| &template[..i])
    }

    #[test]
    fn cut_off_reply_shows_one_muted_format_line() {
        i18n::with_lang(Lang::Ko, || {
            let f = fields(&["root_causes", "outliers"]);
            let c = pipeline::check_reply(
                OutputFormat::StrictJson,
                &f,
                &reply(ko_fixture!("studio.reply.cut_json"), Some("max_tokens")),
                2048,
            );
            let format_check = |summary: &str| format!(ko_text!("Format check (on the cut-off reply): {}"), summary);
            let lines = check_lines(&c);
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert_eq!(lines[0].0, Tone::Err);
            let cut = format!(
                ko_text!("Reached the output token limit ({max_tokens}); the response was cut off ({reason})"),
                max_tokens = 2048,
                reason = "max_tokens"
            );
            assert_eq!(lines[0].1, format!("✖ {cut}"));
            assert_eq!(lines[1].0, Tone::Muted);
            assert!(
                lines[1]
                    .1
                    .starts_with(&format_check(lead(ko_text!("JSON parse error: {e}")))),
                "{}",
                lines[1].1
            );
            let missing = lead(ko_text!("missing fields: {}"));
            assert!(!lines.iter().any(|(_, l)| l.contains(missing)), "{lines:?}");
            // A table in the right format, but cut off
            let t = ko_fixture!("studio.reply.cut_table");
            let c = pipeline::check_reply(OutputFormat::MarkdownTable, &f, &reply(t, Some("length")), 512);
            assert_eq!(
                check_lines(&c)[1],
                (Tone::Muted, format_check(ko_text!("format and required fields match")))
            );
            // A reply that ended without a body (Gemini used up the limit on thinking tokens)
            let c = pipeline::check_reply(OutputFormat::StrictJson, &f, &reply("", Some("MAX_TOKENS")), 2048);
            assert_eq!(check_lines(&c)[1], (Tone::Muted, format_check(ko_text!("empty reply"))));
            assert_eq!(format_summary(&c), ko_text!("empty reply"));
        });
    }

    #[test]
    fn complete_reply_lines() {
        i18n::with_lang(Lang::Ko, || {
            let f = fields(&["a", "b"]);
            let ok = pipeline::check_reply(
                OutputFormat::StrictJson,
                &f,
                &reply(r#"{"a":1,"b":null}"#, Some("stop")),
                2048,
            );
            assert_eq!(
                check_lines(&ok),
                vec![
                    (
                        Tone::Ok,
                        ko_text!("✔ Output format and required fields passed").to_string()
                    ),
                    (Tone::Warn, format!(ko_text!("⚠ Fields with empty values: {}"), "b"))
                ]
            );
            // A parse failure does not get "missing fields: all" appended
            let prose = reply(ko_fixture!("studio.reply.prose"), Some("stop"));
            let bad = pipeline::check_reply(OutputFormat::StrictJson, &f, &prose, 2048);
            let lines = check_lines(&bad);
            assert_eq!(lines.len(), 1);
            let parse_error = format!("✖ {}", lead(ko_text!("JSON parse error: {e}")));
            assert!(lines[0].0 == Tone::Err && lines[0].1.starts_with(&parse_error));
            let missing = pipeline::check_reply(OutputFormat::StrictJson, &f, &reply(r#"{"a":1}"#, None), 2048);
            assert_eq!(
                check_lines(&missing),
                vec![(Tone::Warn, format!(ko_text!("⚠ Missing fields: {}"), "b"))]
            );
            // A header-only table passes, with a warning
            let empty_table = pipeline::check_output(OutputFormat::MarkdownTable, &f, "| a | b |\n|---|---|");
            let lines = check_lines(&empty_table);
            assert_eq!(lines.last().unwrap().0, Tone::Warn);
            assert_eq!(
                lines.last().unwrap().1,
                ko_text!("⚠ The table has no data rows (header only)")
            );
            let (ok, text) = llm_verdict(&Ok(empty_table));
            assert!(
                ok && text == ko_text!("✔ Format OK · ⚠ table has no data rows"),
                "{text}"
            );
        });
    }

    #[test]
    fn retry_with_doubled_limit_only_for_max_tokens() {
        assert_eq!(doubled_limit(2048), Some(4096));
        assert_eq!(doubled_limit(256), Some(512));
        assert_eq!(doubled_limit(20_000), Some(blueprint::MAX_LLM_MAX_TOKENS));
        assert_eq!(doubled_limit(blueprint::MAX_LLM_MAX_TOKENS), None);
        let f = fields(&["a"]);
        let run = |stop: &str, max_tokens: u32| LlmRun {
            out: Ok(pipeline::check_reply(
                OutputFormat::StrictJson,
                &f,
                &reply(ko_fixture!("studio.reply.cut_short"), Some(stop)),
                max_tokens,
            )),
            ms: 10,
            max_tokens,
        };
        assert_eq!(retry_limit(&run("max_tokens", 2048)), Some(4096));
        assert_eq!(retry_limit(&run("MAX_TOKENS", 32_000)), None); // already at the cap
        assert_eq!(retry_limit(&run("content_filter", 2048)), None); // a stop unrelated to the limit
        let failed = LlmRun {
            out: Err("HTTP 500".into()),
            ms: 5,
            max_tokens: 2048,
        };
        assert_eq!(retry_limit(&failed), None);
        assert_eq!(
            i18n::with_lang(Lang::Ko, || run_line(&failed, 0)),
            format!(ko_text!("Failed after {} ms"), 5)
        );
    }

    #[test]
    fn cancelled_llm_reply_is_ignored() {
        let mut jobs = LlmJobs::default();
        let first = jobs.start();
        assert!(jobs.cancel()); // ■ Cancel
        assert!(!jobs.running);
        assert!(!jobs.accept(first)); // a late reply is discarded
        assert!(!jobs.cancel()); // when nothing is running, the status text is not changed
        let second = jobs.start();
        let third = jobs.start(); // run again: the earlier request is stale
        assert!(!jobs.accept(second));
        assert!(jobs.running);
        assert!(jobs.accept(third));
        assert!(!jobs.running);
        assert!(!jobs.accept(third)); // the same reply is not accepted twice
    }

    #[test]
    fn run_details_show_latency_and_usage() {
        i18n::with_lang(Lang::Ko, || {
            assert_eq!(
                run_details(
                    1234,
                    Some(llm::Usage {
                        input: 812,
                        output: 431
                    }),
                    790
                ),
                format!(
                    ko_text!("Reply in {ms} ms · input {} · output {} tokens (compressed prompt est. {prompt_tokens})"),
                    812,
                    431,
                    ms = 1234,
                    prompt_tokens = 790
                )
            );
            assert_eq!(
                run_details(50, None, 10),
                format!(
                    ko_text!("Reply in {ms} ms · no usage info (compressed prompt est. {prompt_tokens} tokens)"),
                    ms = 50,
                    prompt_tokens = 10
                )
            );
        });
    }

    #[test]
    fn dirty_detection_compares_with_last_saved_blueprint() {
        let mut saved = SavedState::default();
        let bp = PRESETS[0].to_blueprint();
        assert!(saved.is_dirty(&bp)); // nothing saved yet
        saved.mark(&bp);
        assert!(!saved.is_dirty(&bp.clone()));
        let mut edited = bp.clone();
        edited.role.push_str(ko_fixture!("studio.dirty.role_suffix"));
        assert!(saved.is_dirty(&edited));
        edited.role = bp.role.clone();
        assert!(!saved.is_dirty(&edited)); // reverting makes it clean again
        edited.positions = vec![("root".into(), [10.0, 20.0])]; // moving a node is a change to save too
        assert!(saved.is_dirty(&edited));
        let mut opts = bp.clone();
        opts.llm_options.max_tokens = 4096;
        assert!(saved.is_dirty(&opts));
        // Reopening the saved file gives the same fingerprint
        let dir = std::env::temp_dir().join(format!("cc_dirty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.kiro");
        bp.save(&path).unwrap();
        assert!(!saved.is_dirty(&Blueprint::load(&path).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_title_marks_unsaved_changes() {
        assert_eq!(window_title("plan.kiro", false, None), "plan.kiro — Candy-Corn");
        assert_eq!(window_title("plan.kiro", true, None), "● plan.kiro — Candy-Corn");
        assert_eq!(
            window_title("plan.kiro", true, Some(Path::new("C:/x/plan.kiro"))),
            "● plan.kiro — Candy-Corn"
        );
        let project = ko_fixture!("studio.title.project");
        assert_eq!(
            window_title(project, false, Some(Path::new("C:/x/a.kiro"))),
            format!("{project} (a.kiro) — Candy-Corn")
        );
        assert_eq!(
            i18n::with_lang(Lang::Ko, || window_title("  ", false, None)),
            format!("{} — Candy-Corn", ko_text!("Untitled"))
        );
    }

    #[test]
    fn side_panels_leave_room_for_the_canvas() {
        // Minimum window width 1000 px: shrink below the default widths (340, 440) to leave 360 px in the center
        let (l, r) = side_panel_max(1000.0);
        assert!(1000.0 - l - r >= CENTER_MIN_W - 0.5, "{l} {r}");
        assert!(l >= 280.0 && r >= 320.0);
        // At the default window width (1400 px), the default widths are not reduced
        let (l, r) = side_panel_max(1400.0);
        assert!(l >= 340.0 && r >= 440.0, "{l} {r}");
        // A wide window gets the original maximum widths
        assert_eq!(side_panel_max(2560.0), (520.0, 700.0));
        // Even a very narrow window does not go below the minimum widths
        assert_eq!(side_panel_max(500.0), (280.0, 320.0));
    }

    // ---------- language ----------

    #[test]
    fn language_switch_recompiles_only_results_of_unchanged_inputs() {
        use LangRefresh::{Keep, Recompile};
        // (has a result, compiling, inputs match the result or the request in progress) → what to do
        for (has_result, compiling, inputs_match, want) in [
            (true, false, true, Recompile), // result of the current inputs: rebuild the lint log in the new language
            (true, false, false, Keep),     // result of changed inputs: no compile the user did not press replaces it
            (false, false, true, Keep),     // no result (also after a failed compile)
            (false, false, false, Keep),
            (false, true, true, Recompile), // compile in progress: restart in the new language (so no text is mixed)
            (true, true, true, Recompile),
            (true, true, false, Keep), // inputs changed after the compile started: the result is stale anyway
            (false, true, false, Keep),
        ] {
            assert_eq!(
                lang_refresh(has_result, compiling, inputs_match),
                want,
                "{has_result} {compiling} {inputs_match}"
            );
        }
    }

    #[test]
    fn llm_result_is_kept_only_for_the_same_prompt_after_a_language_switch() {
        assert!(keep_llm_after_recompile(Some("<p>a</p>"), "<p>a</p>"));
        // If the prompt changed, the reply belongs to the previous prompt and is discarded
        assert!(!keep_llm_after_recompile(Some("<p>a</p>"), "<p>b</p>"));
        // A compile the user started discards it even when the prompt is the same (the old behavior)
        assert!(!keep_llm_after_recompile(None, "<p>a</p>"));
    }

    #[test]
    fn compiled_prompt_does_not_depend_on_ui_language() {
        // Why a language-switch recompile may keep the LLM result: for the same request (user content and data) only
        // the lint text follows the language, while the prompt body and the mock reply stay the same. A constraint that
        // triggers contradiction removal is added too.
        let data =
            pipeline::load_data(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/sales.csv")), 8).unwrap();
        for content in Lang::ALL {
            for p in &PRESETS {
                let extra = i18n::with_lang(content, || tr!("Do not detect outliers").to_string());
                let bp = i18n::with_lang(content, || p.to_blueprint());
                for target in TargetModel::ALL {
                    let mut req = bp.to_request(vec![extra.clone()], Some(data.clone()));
                    req.target = target;
                    let ko = i18n::with_lang(Lang::Ko, || pipeline::compile(&req));
                    let en = i18n::with_lang(Lang::En, || pipeline::compile(&req));
                    assert_eq!(ko.output, en.output, "{} {target:?} {content:?}", p.name.ko());
                    assert_eq!(ko.preview, en.preview);
                    assert!(keep_llm_after_recompile(Some(&ko.output), &en.output));
                }
            }
        }
    }

    #[test]
    fn language_choice_is_saved_through_persist_lang() {
        let dir = std::env::temp_dir().join(format!("cc_lang_setting_{}", std::process::id()));
        let path = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(persist_lang(Lang::En, |l| i18n::save(&path, l)), Ok(()));
        assert_eq!(i18n::load_saved(&path), Some(Lang::En));
        assert_eq!(persist_lang(Lang::Ko, |l| i18n::save(&path, l)), Ok(()));
        assert_eq!(i18n::load_saved(&path), Some(Lang::Ko));
        // A location that cannot be written (its parent is a file): the status line error is in the new language and
        // has the cause appended
        let blocked = path.join("settings.json");
        let en = i18n::with_lang(Lang::En, || persist_lang(Lang::En, |l| i18n::save(&blocked, l)));
        let en = en.unwrap_err();
        assert!(
            en.starts_with("Could not save the language setting") && en.len() > 70,
            "{en}"
        );
        let ko = i18n::with_lang(Lang::Ko, || persist_lang(Lang::Ko, |l| i18n::save(&blocked, l)));
        assert!(ko.unwrap_err().starts_with(lead(ko_text!(
            "Could not save the language setting (applies to this session only): {e}"
        ))));
        assert_eq!(i18n::load_saved(&path), Some(Lang::Ko)); // a failed save leaves the existing setting alone
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_and_verdict_helpers_follow_the_language() {
        let f = fields(&["a", "b"]);
        let ok = pipeline::check_output(OutputFormat::StrictJson, &f, r#"{"a":1,"b":2}"#);
        let missing = pipeline::check_output(OutputFormat::StrictJson, &f, r#"{"a":1}"#);
        let empty_table = pipeline::check_output(OutputFormat::MarkdownTable, &f, "| a | b |\n|---|---|");
        i18n::with_lang(Lang::En, || {
            assert_eq!(
                llm_status(&Ok(ok.clone())),
                "LLM reply · output format and required fields passed"
            );
            assert_eq!(
                llm_status(&Ok(missing.clone())),
                "LLM reply · output format violation (see VIII on the right)"
            );
            assert_eq!(
                llm_status(&Err("HTTP 500".into())),
                "LLM run failed (details in VIII on the right)"
            );
            assert_eq!(
                llm_verdict(&Ok(ok.clone())),
                (true, "✔ Format and required fields OK".to_string())
            );
            assert_eq!(
                llm_verdict(&Ok(missing.clone())),
                (false, "Missing fields: b".to_string())
            );
            let (pass, text) = llm_verdict(&Ok(empty_table.clone()));
            assert!(pass && text == "✔ Format OK · ⚠ table has no data rows", "{text}");
            assert_eq!(
                check_lines(&missing),
                vec![(Tone::Warn, "⚠ Missing fields: b".to_string())]
            );
            assert_eq!(format_summary(&missing), "missing fields: b");
            assert_eq!(drop_hint(Some(DropKind::Blueprint)), "Drop here to open the blueprint");
            assert_eq!(drop_hint(Some(DropKind::Data)), "Drop here to load the data");
            assert!(drop_hint(Some(DropKind::Unsupported)).starts_with("Unsupported file type"));
            assert_eq!(drop_hint(None), "Drop here to open the file");
            assert_eq!(
                run_details(50, None, 10),
                "Reply in 50 ms · no usage info (compressed prompt est. 10 tokens)"
            );
            let failed = LlmRun {
                out: Err("HTTP 500".into()),
                ms: 5,
                max_tokens: 2048,
            };
            assert_eq!(run_line(&failed, 0), "Failed after 5 ms");
            assert_eq!(window_title(" ", true, None), "● Untitled — Candy-Corn");
            assert_eq!(window_title("plan.kiro", false, None), "plan.kiro — Candy-Corn");
            assert_eq!(Pending::Close.label(), "close the window");
            assert_eq!(panic_text(Box::new(42_i32)), "unknown error");
        });
        // Korean keeps the previous text
        i18n::with_lang(Lang::Ko, || {
            assert_eq!(drop_hint(None), ko_text!("Drop here to open the file"));
            assert_eq!(
                llm_status(&Err("x".into())),
                ko_text!("LLM run failed (details in VIII on the right)")
            );
            assert_eq!(Pending::Close.label(), ko_text!("close the window"));
        });
    }

    #[test]
    fn startup_role_and_goals_use_the_startup_language() {
        let defaults = |lang| {
            i18n::with_lang(lang, || {
                (DEFAULT_ROLE.get(), DEFAULT_GOALS.map(|(t, on)| (t.get(), on)))
            })
        };
        let (role, goals) = defaults(Lang::Ko);
        assert_eq!(role, ko_text!("Data analyst who finds root causes in sales data"));
        assert_eq!(
            goals,
            [
                (ko_text!("Exclude missing values (0.2%)"), true),
                (ko_text!("Detect outliers first"), true),
                (ko_text!("Classify by the 4M threshold"), false)
            ]
        );
        let (role, goals) = defaults(Lang::En);
        assert_eq!(role, "Data analyst who finds root causes in sales data");
        assert_eq!(goals.map(|(_, on)| on), [true, true, false]);
        assert!(goals.iter().all(|(t, _)| t.is_ascii() && !t.is_empty()));
    }

    #[test]
    fn shared_phrases_translate_the_same_everywhere() {
        // When the built-in blueprints, the constraint templates and the startup screen share a Korean sentence, the
        // English sentence must be the same as well
        let mut seen = std::collections::HashMap::new();
        let all = PRESETS
            .iter()
            .flat_map(|p| std::iter::once(p.role).chain(p.goals.iter().copied()))
            .chain(
                studio_canvas::TEMPLATES
                    .iter()
                    .flat_map(|(_, items)| items.iter().copied()),
            )
            .chain(std::iter::once(DEFAULT_ROLE))
            .chain(DEFAULT_GOALS.iter().map(|(t, _)| *t));
        let mut n = 0;
        for t in all {
            let en = *seen.entry(t.ko()).or_insert(t.en());
            assert_eq!(en, t.en(), "{}", t.ko());
            n += 1;
        }
        assert!(
            n > seen.len(),
            "the check is only meaningful if some sentences are shared"
        );
    }

    #[test]
    fn target_comparison_fits_the_narrowest_center_pane() {
        // At the minimum window width the center panel is CENTER_MIN_W wide, and the content width inside the card is
        // that minus the outer margin (8) and the card margin (14). The table does not wrap, so it is drawn with the
        // target that gets the longest mark ("current · cheapest").
        let inner = CENTER_MIN_W - 2.0 * 8.0 - 2.0 * 14.0;
        let data =
            pipeline::load_data(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/sales.csv")), 8).unwrap();
        let rows = pipeline::compare_targets(&PRESETS[0].to_blueprint().to_request(vec![], Some(data)));
        for lang in Lang::ALL {
            let ctx = egui::Context::default();
            install_korean_font(&ctx);
            theme::apply(&ctx, &THEMES[3]);
            let mut used = 0.0;
            for _ in 0..3 {
                let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(inner, 800.0));
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area));
                    i18n::with_lang(lang, || {
                        compare_grid(&mut child, &THEMES[3], &rows, TargetModel::Gemini)
                    });
                    used = child.min_rect().width();
                });
                out.drop_without_applying_deltas();
            }
            assert!(used > inner * 0.5 && used <= inner + 0.5, "{lang:?}: {used} > {inner}");
        }
    }
    #[test]
    fn top_bar_reflows_at_min_width_in_both_languages() {
        // At the minimum window width (1000 px) the target, theme and language pickers move to a second row; at the
        // default window width (1400 px) they stay on one row. Either way the top bar content stays within the panel
        // width (with the longest theme name).
        for lang in Lang::ALL {
            for (window_w, want_compact) in [(1000.0, true), (1400.0, false)] {
                let ctx = egui::Context::default();
                install_korean_font(&ctx);
                let p = THEMES[6];
                theme::apply(&ctx, &p);
                let mut project = "sales_analysis.kiro".to_string();
                let (mut target, mut theme_idx) = (TargetModel::Gemini, 6);
                let mut need = TOPBAR_NEED_GUESS;
                let (mut available, mut used) = (0.0, 0.0);
                for _ in 0..4 {
                    let input = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(window_w, 700.0))),
                        ..Default::default()
                    };
                    let out = ctx.run_ui(input, |ui| {
                        egui::Panel::top("top")
                            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 8)))
                            .show(ui, |ui| {
                                available = ui.available_width();
                                i18n::with_lang(lang, || {
                                    top_bar_ui(
                                        ui,
                                        &p,
                                        TopBar {
                                            logo: None,
                                            project: &mut project,
                                            dirty: true,
                                            path: None,
                                            target: &mut target,
                                            theme_idx: &mut theme_idx,
                                            need: &mut need,
                                            compiling: false,
                                            can_undo: true,
                                            can_redo: true,
                                        },
                                    )
                                });
                                used = ui.min_rect().width();
                            });
                    });
                    out.drop_without_applying_deltas();
                }
                assert_eq!(
                    available < need,
                    want_compact,
                    "{lang:?} {window_w}: need {need}, available {available}"
                );
                assert!(used <= available + 0.5, "{lang:?} {window_w}: {used} > {available}");
            }
        }
    }
}
