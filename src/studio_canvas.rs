//! Blueprint canvas: connects the settings, data, role, constraint and output blocks to the compiler node and lays out
//! everything up to the result and the LLM run on one screen.
//! The values of all canvas nodes are exactly the content of the blueprint (.kiro) file.
//! Panning (drag) and zooming (Ctrl+wheel, pinch) are handled by egui::Scene.

use crate::ast::{ConstraintNode, ConstraintType, LintDiagnostic, OutputFormat, OutputSchemaAst, TargetModel};
use crate::blueprint::{self, LlmOptions, MAX_SAMPLE_ROWS, PRESETS};
use crate::i18n::{self, Lang, Tr};
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

    /// Finds a fixed node by its name in the blueprint's `positions`.
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

/// One constraint line. The canvas nodes and the form on the left edit the same list.
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
    /// Whether it differs from the last opened or saved blueprint (● next to the name)
    pub dirty: bool,
    pub theme_idx: &'a mut usize,
    /// Whether the moving fog of the fog themes is on
    pub fog: &'a mut bool,
    /// Whether the current theme is a fog theme (if not, the fog effect setting is shown disabled)
    pub fog_theme: bool,
    pub sample_rows: &'a mut usize,
    pub llm: LlmView<'a>,
}

/// State shown in the LLM run node.
pub struct LlmView<'a> {
    pub provider: &'static str,
    pub key_env: &'static str,
    pub model: &'a mut String,
    pub model_hint: &'static str,
    /// Current models of the provider for the model picker (model ID, display name)
    pub models: &'static [(&'static str, &'static str)],
    pub has_key: bool,
    pub running: bool,
    /// (passed, one-line summary)
    pub verdict: Option<(bool, String)>,
    /// Output limit, temperature, system prompt (the same values as VIII on the right)
    pub opts: &'a mut LlmOptions,
    /// Upper temperature limit of the current provider
    pub temp_max: f32,
    /// One line with the reply time and token usage of the last run
    pub details: Option<String>,
    /// The doubled limit if the last reply was cut off at the output limit (retry button)
    pub retry_limit: Option<u32>,
}

/// Actions the canvas asks the app to perform (passed back as return values to avoid ownership conflicts).
pub enum Action {
    PickFile,
    Compile,
    RemoveGoal(u64),
    AddGoal(String),
    /// Apply a built-in blueprint (index into blueprint::PRESETS)
    Preset(usize),
    /// Save as (always asks for a location)
    SaveProject,
    /// Save to the current blueprint file (Ctrl+S; asks for a location if there is no file yet)
    QuickSave,
    LoadProject,
    SaveOutput,
    RunLlm,
    /// Discard the LLM run in progress
    CancelLlm,
    /// Raise the output limit to this value and run again
    RetryLonger(u32),
    /// The sample row count changed, so profile the data again
    ReloadSample,
    /// Change the UI language (language picker in the settings node)
    SetLang(Lang),
    /// Open the 🔑 API keys window
    OpenKeys,
}

/// Constraint node templates: (category, sentences). Picking one adds a constraint with the sentence in the current
/// language.
pub const TEMPLATES: [(Tr, &[Tr]); 5] = [
    (
        trp!("Missing values"),
        &[
            trp!("Exclude missing values (0.2%)"),
            trp!("Report the missing-value ratio for each column"),
        ],
    ),
    (
        trp!("Outliers"),
        &[trp!("Detect outliers first"), trp!("Judge outliers by a z-score of 3")],
    ),
    (
        trp!("Factuality"),
        &[
            trp!("Do not infer anything not in the data"),
            trp!("Name the columns that support each claim"),
        ],
    ),
    (
        trp!("Format"),
        &[
            trp!("Keep the summary to 3 sentences or fewer"),
            trp!("Round numbers to 2 decimal places"),
        ],
    ),
    (
        trp!("Classification"),
        &[
            trp!("Classify by the 4M amount threshold"),
            trp!("Mark anything that cannot be classified as unknown"),
        ],
    ),
];

/// Vertical gap between nodes
const GAP: f32 = 20.0;
const LEFT_X: f32 = 0.0;
const GOAL_X: f32 = 290.0;
const ROOT_X: f32 = 590.0;
const LLM_X: f32 = 900.0;
/// Grid dot spacing (doubled step by step when zoomed far out) and the maximum number of dots on one screen
const GRID_STEP: f32 = 40.0;
const MAX_GRID_DOTS: i64 = 6000;
/// Flow animation frame interval (at most 20 fps)
pub const FLOW_FRAME: Duration = Duration::from_millis(50);

/// Estimated node height, used when the height measured in the last frame is unknown (before the first draw).
fn default_height(key: NodeKey) -> f32 {
    match key {
        NodeKey::Settings => 224.0,
        NodeKey::Data => 210.0,
        NodeKey::Role => 160.0,
        NodeKey::Output => 150.0,
        NodeKey::Goal(_) => 92.0,
        NodeKey::Root => 170.0,
        NodeKey::Result => 300.0,
        NodeKey::Llm => 260.0,
    }
}

/// Estimated constraint node height. Long sentences wrap inside the text box, which makes the node taller
/// (about 17 full-width characters per line; ASCII counts as half width).
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

/// Default layout. The left column (settings, data, role, output) and the constraint column are stacked by node height,
/// and the compiler, result and LLM nodes are placed at the middle height of the constraint column.
/// height returns a node's height (measured in the last frame, or an estimate); None means the default estimate.
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

/// Place for a new constraint node: right below the lowest constraint node (same x). Without constraints, the top of
/// the constraint column.
/// nodes are the existing constraint nodes as (position, height).
pub fn place_below(nodes: impl IntoIterator<Item = (Pos2, f32)>) -> Pos2 {
    nodes
        .into_iter()
        .max_by(|a, b| (a.0.y + a.1).total_cmp(&(b.0.y + b.1)))
        .map_or(Pos2::new(GOAL_X, 0.0), |(p, h)| Pos2::new(p.x, p.y + h + GAP))
}

/// Clamps a position to the allowed range (blueprint::POS_LIMIT). None for NaN or infinity.
fn clamp_pos(p: Pos2) -> Option<Pos2> {
    blueprint::sanitize_pos([p.x, p.y]).map(|[x, y]| Pos2::new(x, y))
}

/// Grid dots as (spacing, x cell index range, y cell index range). The spacing is doubled until there are at most
/// max_dots dots.
/// Cells are counted with integer indices so this always ends, however large the coordinates (repeated f32 addition
/// stops advancing at large coordinates).
/// None if the visible area is invalid (NaN, infinity, empty).
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

/// Repaint interval for the flow animation. It moves at up to 20 fps only while compiling, loading or running the LLM,
/// or while the pointer is over the canvas; otherwise the still frame is left as is, so no idle CPU is used.
pub fn flow_repaint(animate: bool, busy: bool, pointer_over: bool, focused: bool) -> Option<Duration> {
    (animate && focused && (busy || pointer_over)).then_some(FLOW_FRAME)
}

/// Summary appended to the collapsible run options title: "max 2048 · default temp · system". The temperature is the
/// value actually sent, clamped to the provider's range.
pub fn options_summary(o: &LlmOptions, temp_max: f32) -> String {
    let temp = o.temperature.map_or_else(
        || tr!("default temp").to_string(),
        |t| trf!("temp {:.2}", t.clamp(0.0, temp_max)),
    );
    let system = if o.system.trim().is_empty() {
        ""
    } else {
        tr!(" · system")
    };
    trf!("max {} · {temp}{system}", o.max_tokens, temp = temp, system = system)
}

/// Output limits above this get a notice: replies grow longer and may hit the LLM call timeout.
const LONG_REPLY_TOKENS: u32 = 8192;

/// LLM run option inputs. The canvas LLM node and VIII on the right edit the same values (salt keeps the input ids
/// apart).
pub fn llm_options_ui(ui: &mut Ui, o: &mut LlmOptions, temp_max: f32, salt: &str, p: &Palette) {
    ui.horizontal(|ui| {
        ui.label(tr!("Max output"));
        ui.add(
            egui::DragValue::new(&mut o.max_tokens)
                .range(blueprint::MIN_LLM_MAX_TOKENS..=blueprint::MAX_LLM_MAX_TOKENS)
                .speed(16.0)
                .suffix(tr!(" tokens")),
        )
        .on_hover_text(trf!(
            "The reply is cut off at this many tokens ({}-{})",
            blueprint::MIN_LLM_MAX_TOKENS,
            blueprint::MAX_LLM_MAX_TOKENS
        ));
    });
    if o.max_tokens > LONG_REPLY_TOKENS {
        ui.label(
            RichText::new(tr!("⚠ A large limit makes replies longer and may exceed the call timeout; each model allows a different maximum"))
            .small()
            .color(p.warn()),
        );
    }
    ui.horizontal(|ui| {
        ui.label(tr!("Temperature"));
        let mut default = o.temperature.is_none();
        if ui
            .checkbox(&mut default, tr!("Default"))
            .on_hover_text(tr!("Use the provider's default temperature (not sent in the request)"))
            .changed()
        {
            o.temperature = (!default).then_some(1.0_f32.min(temp_max));
        }
    });
    if let Some(t) = &mut o.temperature {
        // Show the value clamped to the provider's range and change the saved value only when the user moves the slider
        // (so 1.5 set for GPT is still there after switching to Claude and back).
        let mut shown = t.clamp(0.0, temp_max);
        if ui
            .add(
                egui::Slider::new(&mut shown, 0.0..=temp_max)
                    .step_by(0.05)
                    .fixed_decimals(2),
            )
            .on_hover_text(trf!(
                "0-{temp_max:.0} (the range differs by provider)",
                temp_max = temp_max
            ))
            .changed()
        {
            *t = shown;
        }
    }
    ui.label(RichText::new(tr!("System prompt (optional)")).small().color(p.sub));
    ui.add(
        egui::TextEdit::multiline(&mut o.system)
            .id_salt((salt, "llm-system"))
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .hint_text(tr!("Not sent when empty")),
    );
}

/// Language picker (top bar and settings node). The list shows each language by its native name (Korean in Hangul,
/// English), and returns the language when another one is picked. The app does the switching (saving, recompiling).
pub fn lang_picker(ui: &mut Ui, id_salt: &str) -> Option<Lang> {
    let current = i18n::current();
    let mut chosen = current;
    ui.label(tr!("Language"));
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current.native_name())
        .show_ui(ui, |ui| {
            for l in Lang::ALL {
                ui.selectable_value(&mut chosen, l, l.native_name());
            }
        })
        .response
        .on_hover_text(tr!("UI language · your role and constraint text stays as typed"));
    (chosen != current).then_some(chosen)
}

pub struct Canvas {
    scene_rect: Rect,
    pos: HashMap<NodeKey, Pos2>,
    /// Node areas drawn in the last frame (the heights are used for the layout and for placing new constraints)
    rects: HashMap<NodeKey, Rect>,
    pub snap: bool,
    pub animate: bool,
    selected: Option<NodeKey>,
    want_layout: bool,
    want_fit: bool,
    /// The canvas's screen area in the last frame (for deciding background clicks and Delete).
    area: Rect,
    /// Whether the user panned or zoomed the canvas. If not, the view is fitted again when the window size changes.
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

/// Whether a lint diagnostic points at the constraint at position `i` of the compile request. Only the input positions
/// attached to the diagnostic are used (for a removal, the removed and the kept one; for a possible contradiction, both
/// constraints). The warning text quotes the body with the fillers stripped, so comparing original texts misses it.
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

    /// Sets a node position. Invalid coordinates (NaN, infinity) are ignored and far ones are clamped to the allowed
    /// range. The auto-layout flag is unchanged, so blueprint positions go on top after reset_layout (or load_layout).
    pub fn set_pos(&mut self, key: NodeKey, p: Pos2) {
        if let Some(p) = clamp_pos(p) {
            self.pos.insert(key, p);
        }
    }

    pub fn request_fit(&mut self) {
        self.want_fit = true;
        self.user_view = false;
    }

    /// Resets the node layout to the default (keeping the snap and animation settings). The default layout is computed
    /// right away, so positions set later with set_pos stay and nodes without a position do not pile up at the origin.
    pub fn reset_layout(&mut self, goals: &[Goal]) {
        self.pos.clear();
        self.rects.clear();
        self.selected = None;
        self.layout(goals);
        self.want_layout = false;
        self.want_fit = true;
        self.user_view = false;
    }

    /// For applying a blueprint: computes the default layout first and puts the saved positions over it. Nodes without
    /// a saved position (such as the settings, data and LLM nodes of v1 files) stay in their default place.
    pub fn load_layout(&mut self, goals: &[Goal], saved: impl IntoIterator<Item = (NodeKey, [f32; 2])>) {
        self.reset_layout(goals);
        for (k, [x, y]) in saved {
            self.set_pos(k, Pos2::new(x, y));
        }
    }

    /// Cleans up the layout and selection state of a deleted node.
    /// Its position (pos) is kept, so it reappears in the same place when undo brings it back.
    pub fn forget(&mut self, key: NodeKey) {
        self.rects.remove(&key);
        if self.selected == Some(key) {
            self.selected = None;
        }
    }

    /// Node height measured in the last frame
    fn measured_height(&self, key: NodeKey) -> Option<f32> {
        self.rects
            .get(&key)
            .map(Rect::height)
            .filter(|h| h.is_finite() && *h > 0.0)
    }

    /// Constraint node height: the value measured in the last frame, or else an estimate from the sentence length.
    fn goal_height(&self, g: &Goal) -> f32 {
        self.measured_height(NodeKey::Goal(g.id))
            .unwrap_or_else(|| estimate_goal_height(&g.text))
    }

    /// Default layout. It uses the node heights measured in the last frame, so long wrapped constraints do not overlap.
    fn layout(&mut self, goals: &[Goal]) {
        let ids: Vec<u64> = goals.iter().map(|g| g.id).collect();
        let placed = layout_positions(&ids, |k| match k {
            NodeKey::Goal(id) => goals.iter().find(|g| g.id == id).map(|g| self.goal_height(g)),
            _ => self.measured_height(k),
        });
        self.pos.extend(placed);
    }

    /// Gives nodes without a position a place: fixed nodes get the default layout, new constraints go below the lowest
    /// constraint node.
    /// The app calls this every frame (even when the canvas is not shown), so a saved blueprint always has positions.
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

    /// Place for a node without a position (normally unused, since show() fills them in beforehand). Keeps nodes from
    /// piling up at the origin.
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
        // Clicking empty space clears the selection (a click on a node's title bar selects it again in node() below)
        if pointer_in && ctx.input(|i| i.pointer.primary_clicked()) {
            self.selected = None;
        }
        // Delete works only while the pointer is over the canvas and no text is being typed.
        if let Some(NodeKey::Goal(id)) = self.selected
            && pointer_in
            && !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Delete))
        {
            actions.push(Action::RemoveGoal(id));
            self.selected = None;
        }
        // Flow animation: repaint only while working or while the pointer is over the canvas.
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
            ui.menu_button(tr!("＋ Constraint"), |ui| {
                for (cat, items) in TEMPLATES {
                    ui.label(RichText::new(cat.get()).color(p.accent).strong());
                    for t in items {
                        if ui.button(t.get()).clicked() {
                            actions.push(Action::AddGoal(t.get().to_string()));
                            ui.close();
                        }
                    }
                    ui.separator();
                }
                if ui.button(tr!("Add empty constraint")).clicked() {
                    actions.push(Action::AddGoal(String::new()));
                    ui.close();
                }
            });
            ui.menu_button(tr!("🧩 Blueprints"), |ui| {
                ui.label(
                    RichText::new(tr!("Sets target, role, constraints, output, sample rows and theme at once (keeps the current data)"))
                    .small()
                    .color(p.sub),
                );
                for (i, preset) in PRESETS.iter().enumerate() {
                    if ui
                        .button(preset.name.get())
                        .on_hover_text(preset.summary.get())
                        .clicked()
                    {
                        actions.push(Action::Preset(i));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(tr!("Open blueprint…")).clicked() {
                    actions.push(Action::LoadProject);
                    ui.close();
                }
                if ui
                    .button(tr!("Save current settings as blueprint…"))
                    .clicked()
                {
                    actions.push(Action::SaveProject);
                    ui.close();
                }
            });
            if ui
                .button(tr!("Auto layout"))
                .on_hover_text(tr!("Move nodes back to the default layout"))
                .clicked()
            {
                self.want_layout = true;
            }
            if ui.button(tr!("Fit view")).clicked() {
                self.request_fit();
            }
            ui.checkbox(&mut self.snap, tr!("Snap to grid"));
            ui.checkbox(&mut self.animate, tr!("Flow animation"))
                .on_hover_text(tr!("Moves only while working or while the pointer is over the canvas"));
            ui.separator();
            if ui
                .button(tr!("Save blueprint"))
                .on_hover_text(tr!("Ctrl+S · saves to the current blueprint file (asks for a location the first time)\nData path, role, constraints, output, target, sample rows, theme, LLM models, run options, node layout"))
                .clicked()
            {
                actions.push(Action::QuickSave);
            }
            if ui
                .button(tr!("Open blueprint"))
                .on_hover_text("Ctrl+O")
                .clicked()
            {
                actions.push(Action::LoadProject);
            }
            if ui.button(tr!("Save result")).clicked() {
                actions.push(Action::SaveOutput);
            }
        });
        ui.label(
            RichText::new(tr!("Drag a node title to move it · drag empty space to pan · Ctrl+wheel to zoom · Delete removes the selected constraint · the whole canvas is saved as the blueprint"))
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
        // Grid dots (counted with integer cell indices, so the loop ends at any coordinates)
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

        // Blueprint settings
        self.node(
            ui,
            NodeKey::Settings,
            tr!("⚙ Blueprint settings"),
            SETTINGS_COLOR,
            250.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr!("Name"));
                    // Unsaved-changes mark (●). Its slot is always reserved and the text box has a fixed id,
                    // so the text box keeps its focus when the mark appears as the first character is typed.
                    let (slot, resp) = ui.allocate_exact_size(egui::vec2(10.0, 18.0), Sense::hover());
                    if m.dirty {
                        ui.painter().text(
                            slot.center(),
                            egui::Align2::CENTER_CENTER,
                            "●",
                            egui::FontId::proportional(12.0),
                            p.warn(),
                        );
                        resp.on_hover_text(tr!("Unsaved changes (Ctrl+S to save)"));
                    }
                    ui.add(
                        egui::TextEdit::singleline(m.project)
                            .id_salt("canvas-project")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(tr!("Theme"));
                    // Long English names grow only up to the remaining width and are truncated (so they stay inside the
                    // node; the list shows the full name)
                    egui::ComboBox::from_id_salt("canvas-theme")
                        .width(170.0)
                        .truncate()
                        .selected_text(THEMES[(*m.theme_idx).min(THEMES.len() - 1)].name.get())
                        .show_ui(ui, |ui| {
                            for (i, t) in THEMES.iter().enumerate() {
                                ui.selectable_value(m.theme_idx, i, t.name.get());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    if let Some(lang) = lang_picker(ui, "canvas-lang") {
                        actions.push(Action::SetLang(lang));
                    }
                });
                ui.add_enabled(m.fog_theme, egui::Checkbox::new(m.fog, tr!("Fog effect")))
                    .on_hover_text(tr!("Draws drifting fog. Turning it off reduces repaints"))
                    .on_disabled_hover_text(tr!("Only used by the fog themes (06, 07)"));
                ui.horizontal(|ui| {
                    if ui
                        .small_button(tr!("Save"))
                        .on_hover_text(tr!("Ctrl+S · save to the current blueprint file"))
                        .clicked()
                    {
                        actions.push(Action::QuickSave);
                    }
                    if ui.small_button(tr!("Save as…")).clicked() {
                        actions.push(Action::SaveProject);
                    }
                    if ui.small_button(tr!("Open")).clicked() {
                        actions.push(Action::LoadProject);
                    }
                });
                // API keys are an app setting for this Windows user; they are never saved in the blueprint.
                if ui
                    .small_button(tr!("🔑 API keys"))
                    .on_hover_text(tr!(
                        "Keys for the LLM run, saved for this Windows user (not in the blueprint)"
                    ))
                    .clicked()
                {
                    actions.push(Action::OpenKeys);
                }
            },
        );

        // Data
        self.node(
            ui,
            NodeKey::Data,
            tr!("◆ Data source"),
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
                            ui.label(RichText::new(trf!("Outliers found: {}", d.data.anomalies.len())).color(p.warn()));
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
                        ui.label(RichText::new(tr!("No data")).color(p.sub));
                        ui.label(
                            RichText::new(tr!("You can also drop a file on the window"))
                                .small()
                                .color(p.sub),
                        );
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(tr!("Sample rows"));
                    let r = ui.add(egui::DragValue::new(m.sample_rows).range(0..=MAX_SAMPLE_ROWS));
                    if r.drag_stopped() || r.lost_focus() {
                        actions.push(Action::ReloadSample);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!m.loading, egui::Button::new(tr!("📂 Load"))).clicked() {
                        actions.push(Action::PickFile);
                    }
                    if m.loading {
                        ui.spinner();
                    }
                });
            },
        );

        // Role
        self.node(ui, NodeKey::Role, tr!("◆ Role"), ROLE_COLOR, 250.0, p, false, |ui| {
            ui.add(
                egui::TextEdit::multiline(m.role)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text(tr!("Describe the role")),
            );
            ui.label(
                RichText::new(tr!("Lint trims fillers (very, please…)"))
                    .small()
                    .color(p.sub),
            );
        });

        // Output
        self.node(
            ui,
            NodeKey::Output,
            tr!("◆ Output format"),
            OUT_COLOR,
            250.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(m.format, OutputFormat::StrictJson, "JSON");
                    ui.radio_value(m.format, OutputFormat::MarkdownTable, tr!("Table"));
                });
                ui.add(
                    egui::TextEdit::singleline(m.fields_text)
                        .desired_width(f32::INFINITY)
                        .hint_text(tr!("field1,field2")),
                );
                let fields = pipeline::parse_fields(m.fields_text);
                if fields.is_empty() {
                    ui.label(RichText::new(tr!("No required fields")).small().color(theme::ERR));
                } else {
                    let schema = OutputSchemaAst::render_schema(*m.format, &fields);
                    let short: String = schema.chars().take(70).collect();
                    ui.label(RichText::new(short).monospace().small().color(p.sub));
                }
            },
        );

        // Constraints
        // The lint warning badge is found by the position the diagnostic points at (the order among the constraints
        // that are on and not empty, as sent in the compile request). It is not shown for a stale result, whose
        // positions may no longer match.
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
            let title = trf!("● Constraint · {}", kind.tag());
            let id = g.id;
            self.node(ui, NodeKey::Goal(id), &title, color, 260.0, p, !g.on, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut g.text)
                        .id_salt(("canvas-goal", id))
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .hint_text(tr!("Constraint text")),
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut g.on, tr!("On"));
                    if ui.small_button(tr!("Delete")).clicked() {
                        actions.push(Action::RemoveGoal(id));
                    }
                    if related {
                        ui.label(RichText::new(tr!("⚠ Lint warning")).small().color(p.warn()));
                    }
                });
            });
        }

        // Compiler (root)
        let active = m.goals.iter().filter(|g| g.on).count();
        self.node(
            ui,
            NodeKey::Root,
            tr!("▶ RootPromptAST compiler"),
            p.accent,
            230.0,
            p,
            false,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(tr!("Target"));
                    egui::ComboBox::from_id_salt("canvas-target")
                        .selected_text(m.target.label())
                        .show_ui(ui, |ui| {
                            for t in TargetModel::ALL {
                                ui.selectable_value(m.target, t, t.label());
                            }
                        });
                });
                ui.label(trf!("Active constraints: {active}", active = active));
                if theme::primary_button(ui, p, "⚡ Compile & Run", !m.compiling).clicked() {
                    actions.push(Action::Compile);
                }
                if m.compiling {
                    ui.spinner();
                } else if m.stale && m.result.is_some() {
                    ui.label(
                        RichText::new(tr!("● Inputs changed · recompile"))
                            .small()
                            .color(p.warn()),
                    );
                }
            },
        );

        // Result
        self.node(
            ui,
            NodeKey::Result,
            tr!("★ Result · token savings"),
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
                    ui.label(trf!("Original {} → compressed {} tokens", r.orig_tokens, r.comp_tokens));
                    ui.label(RichText::new(trf!("Saved ${:.4}", r.saved_usd)).color(p.sub));
                    let warns = r.ast.lint_warnings.iter().filter(|d| d.severity == "WARN").count();
                    let errs = r.ast.lint_warnings.iter().filter(|d| d.severity == "ERROR").count();
                    ui.label(
                        RichText::new(trf!(
                            "Lint warnings {warns} · errors {errs}",
                            warns = warns,
                            errs = errs
                        ))
                        .color(if errs > 0 { theme::ERR } else { theme::OK }),
                    );
                    for line in r.output.lines().take(5) {
                        ui.add(egui::Label::new(RichText::new(line).monospace().small().color(p.sub)).truncate());
                    }
                    ui.horizontal(|ui| {
                        if ui.small_button(tr!("Copy")).clicked() {
                            ui.ctx().copy_text(r.output.clone());
                        }
                        if ui.small_button(tr!("Save")).clicked() {
                            actions.push(Action::SaveOutput);
                        }
                    });
                }
                None => {
                    ui.label(RichText::new(tr!("Not compiled yet")).color(p.sub));
                }
            },
        );

        // LLM run
        let has_result = m.result.is_some();
        let stale = m.stale;
        let compiling = m.compiling;
        let llm = &mut m.llm;
        self.node(
            ui,
            NodeKey::Llm,
            tr!("◆ LLM run"),
            LLM_COLOR,
            250.0,
            p,
            !llm.has_key,
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(llm.provider);
                    crate::keys::model_menu(ui, llm.models, llm.model);
                    ui.add(
                        egui::TextEdit::singleline(llm.model)
                            .id_salt("canvas-llm-model")
                            .desired_width(f32::INFINITY)
                            .hint_text(llm.model_hint),
                    );
                });
                // The title includes a summary of the current values, so it changes; a fixed id keeps the open state.
                egui::CollapsingHeader::new(trf!("Options · {}", options_summary(llm.opts, llm.temp_max)))
                    .id_salt("canvas-llm-opts")
                    .show(ui, |ui| llm_options_ui(ui, llm.opts, llm.temp_max, "canvas", p));
                // Do not run with a stale result after the inputs changed.
                let enabled = llm.has_key && has_result && !stale && !compiling && !llm.running;
                ui.horizontal(|ui| {
                    let label = if llm.running {
                        tr!("Running")
                    } else {
                        tr!("▶ Run live")
                    };
                    if theme::primary_button(ui, p, label, enabled).clicked() {
                        actions.push(Action::RunLlm);
                    }
                    if llm.running {
                        ui.spinner();
                        if ui
                            .small_button(tr!("■ Cancel"))
                            .on_hover_text(tr!("Discard the reply without waiting"))
                            .clicked()
                        {
                            actions.push(Action::CancelLlm);
                        }
                    }
                });
                match (&llm.verdict, llm.has_key, has_result) {
                    _ if has_result && stale && llm.has_key && !llm.running => {
                        ui.label(
                            RichText::new(tr!("Inputs changed · recompile, then run"))
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
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(tr!("No API key yet")).small().color(p.sub));
                            if ui
                                .small_button(tr!("🔑 API keys"))
                                .on_hover_text(trf!(
                                    "A saved key is used first, then the {} environment variable",
                                    llm.key_env
                                ))
                                .clicked()
                            {
                                actions.push(Action::OpenKeys);
                            }
                        });
                    }
                    (None, true, false) => {
                        ui.label(RichText::new(tr!("Compile first")).small().color(p.sub));
                    }
                    (None, true, true) => {
                        ui.label(
                            RichText::new(tr!("Replies are checked for format and required fields"))
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
                            egui::Button::new(trf!("Retry with 2× limit ({limit})", limit = limit)).small(),
                        )
                        .clicked()
                {
                    actions.push(Action::RetryLonger(limit));
                }
            },
        );

        // Wires
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
        // The first constraint is removed as a contradiction: the badge goes on the removed constraint (position 0) and
        // on the kept one (position 1). It does not go on another constraint whose text merely appears in the warning
        // (position 2, a word of the first constraint): that was a false positive of the old substring comparison.
        let r = pipeline::compile(&req(ko_fixture!("studio.lint_badge.clash")));
        let d = &r.ast.lint_warnings;
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        assert!(!lint_mentions(d, 2), "{d:?}");
        // Duplicate sentence: both the removed one (position 0) and the kept one (position 1, the later one) are found
        // by position.
        let r = pipeline::compile(&req(ko_fixture!("studio.lint_badge.duplicate")));
        let d = &r.ast.lint_warnings;
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        // Review repro: the possible-contradiction warning quotes the body with the filler stripped (`suspect_quote`),
        // so the old lookup by the original text did not put a badge on the first constraint. Both constraints get it.
        let suspect = ko_fixture!("studio.lint_badge.suspect");
        let r = i18n::with_lang(Lang::Ko, || pipeline::compile(&req(suspect)));
        let d = &r.ast.lint_warnings;
        let warning = format!(
            ko_text!(
                "Possible contradiction (kept both): \"{}\" ↔ \"{}\". Check whether they give opposite instructions for the same thing"
            ),
            ko_fixture!("studio.lint_badge.suspect_quote"),
            suspect[1]
        );
        assert!(d.iter().any(|w| w.message == warning), "{d:?}");
        assert!(lint_mentions(d, 0) && lint_mentions(d, 1), "{d:?}");
        // No badge on constraints without warnings
        let r = pipeline::compile(&req(ko_fixture!("studio.lint_badge.clean")));
        assert!(!lint_mentions(&r.ast.lint_warnings, 0) && !lint_mentions(&r.ast.lint_warnings, 1));
    }

    fn goals(ids: &[u64]) -> Vec<Goal> {
        ids.iter()
            .map(|&id| Goal {
                id,
                text: format!("{} {id}", ko_fixture!("studio.canvas.goal_label")),
                on: true,
            })
            .collect()
    }

    fn rect_of(c: &Canvas, key: NodeKey, h: f32) -> Rect {
        Rect::from_min_size(c.pos_of(key).unwrap(), egui::vec2(260.0, h))
    }

    #[test]
    fn grid_loop_terminates_at_huge_and_bad_coordinates() {
        // At y=1.5e9, f32 y += 40 no longer advances, so the old loop never ended.
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
                // At 1e30 the height of 700 is lost in f32 precision and the area becomes empty: no dots are drawn
                None => assert!(top.abs() > 1.0e20, "top={top}"),
            }
        }
        // A very wide area gets a larger spacing that limits the number of dots
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
        assert_eq!(c.pos_of(NodeKey::Data), before); // NaN is ignored
    }

    #[test]
    fn default_layout_matches_previous_fixed_spacing() {
        let placed: HashMap<NodeKey, Pos2> = layout_positions(&[1, 2, 3], |_| None).into_iter().collect();
        assert_eq!(placed[&NodeKey::Settings], Pos2::new(0.0, 0.0));
        // The settings node got a language picker row (estimated height 150 -> 200) and then an API keys button row
        // (200 -> 224), so the left column moves down by 74 in total
        assert_eq!(placed[&NodeKey::Data], Pos2::new(0.0, 244.0));
        assert_eq!(placed[&NodeKey::Role], Pos2::new(0.0, 474.0));
        assert_eq!(placed[&NodeKey::Output], Pos2::new(0.0, 654.0));
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
        // Last frame: constraint 1 wrapped and is 300 px tall
        c.rects.insert(NodeKey::Goal(1), rect_of(&c, NodeKey::Goal(1), 300.0));
        c.rects.insert(NodeKey::Goal(2), rect_of(&c, NodeKey::Goal(2), 92.0));
        c.layout(&gs);
        assert_eq!(c.pos_of(NodeKey::Goal(2)), Some(Pos2::new(290.0, 320.0)));
        // Without a measured height, it is estimated from the sentence length
        assert!(
            estimate_goal_height(&ko_fixture!("studio.canvas.wide_char").repeat(60))
                > estimate_goal_height(ko_fixture!("studio.canvas.short_goal"))
        );
        assert_eq!(
            estimate_goal_height(ko_fixture!("studio.canvas.short_goal")),
            default_height(NodeKey::Goal(0))
        );
        assert!(
            estimate_goal_height(ko_fixture!("studio.canvas.three_lines"))
                > estimate_goal_height(ko_fixture!("studio.canvas.one_line"))
        );
    }

    #[test]
    fn new_goal_goes_below_the_lowest_goal() {
        // Remove the first of three and add one: it used to land at index*112 = 224, on top of the last constraint.
        let mut c = Canvas::default();
        c.reset_layout(&goals(&[1, 2, 3]));
        let mut live = goals(&[2, 3]);
        c.forget(NodeKey::Goal(1));
        live.push(Goal {
            id: 4,
            text: ko_fixture!("studio.canvas.new_goal").into(),
            on: true,
        });
        c.ensure_positions(&live);
        let new = c.pos_of(NodeKey::Goal(4)).unwrap();
        assert_eq!(new, Pos2::new(290.0, 224.0 + 92.0 + GAP));
        let new_rect = Rect::from_min_size(new, egui::vec2(260.0, 92.0));
        for id in [2, 3] {
            assert!(!new_rect.intersects(rect_of(&c, NodeKey::Goal(id), 92.0)), "{id}");
        }
        // Uses the height measured in the last frame (a node made taller by wrapping)
        c.rects.insert(NodeKey::Goal(3), rect_of(&c, NodeKey::Goal(3), 200.0));
        live.push(Goal {
            id: 5,
            text: String::new(),
            on: true,
        });
        c.ensure_positions(&live);
        // The lowest node is number 4 (336-428): right below it
        assert_eq!(c.pos_of(NodeKey::Goal(5)), Some(Pos2::new(290.0, 336.0 + 92.0 + GAP)));
        assert_eq!(place_below(std::iter::empty()), Pos2::new(GOAL_X, 0.0));
        // It goes below a column the user moved
        assert_eq!(
            place_below([(Pos2::new(700.0, 40.0), 100.0), (Pos2::new(290.0, 0.0), 92.0)]),
            Pos2::new(700.0, 160.0)
        );
    }

    #[test]
    fn v1_layout_without_fixed_positions_does_not_pile_at_origin() {
        // v1 project file: only one constraint and root have positions. The rest must follow the default layout.
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
        // Every node has a position before anything is drawn, so saving keeps the same layout
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
        assert_eq!(flow_repaint(true, false, false, true), None); // idle: no repaint
        assert_eq!(flow_repaint(true, true, false, true), Some(FLOW_FRAME)); // working
        assert_eq!(flow_repaint(true, false, true, true), Some(FLOW_FRAME)); // pointer over the canvas
        assert_eq!(flow_repaint(false, true, true, true), None); // animation off
        assert_eq!(flow_repaint(true, true, true, false), None); // not focused
        assert!(FLOW_FRAME >= Duration::from_millis(50)); // at most 20 fps
    }

    #[test]
    fn llm_options_summary() {
        let summary = |max: u32, temperature: &str, system: &str| {
            format!(
                ko_text!("max {} · {temp}{system}"),
                max,
                temp = temperature,
                system = system
            )
        };
        let temp = |t: f32| format!(ko_text!("temp {:.2}"), t);
        let mut o = LlmOptions::default();
        i18n::with_lang(Lang::Ko, || {
            assert_eq!(options_summary(&o, 1.0), summary(2048, ko_text!("default temp"), ""));
            o.temperature = Some(1.5);
            o.system = ko_fixture!("studio.canvas.system_prompt").into();
            o.max_tokens = 4096;
            assert_eq!(
                options_summary(&o, 2.0),
                summary(4096, &temp(1.5), ko_text!(" · system"))
            );
            // For Anthropic (max 1.0) the value actually sent is shown
            assert_eq!(
                options_summary(&o, 1.0),
                summary(4096, &temp(1.0), ko_text!(" · system"))
            );
        });
    }

    /// Screen state used for drawing. With full, data, a compile result, an LLM verdict and long texts are all shown.
    struct Scenario {
        role: String,
        goals: Vec<Goal>,
        format: OutputFormat,
        fields: String,
        target: TargetModel,
        project: String,
        theme_idx: usize,
        fog: bool,
        sample_rows: usize,
        llm_model: String,
        opts: LlmOptions,
        loaded: Option<LoadedData>,
        result: Option<CompileResult>,
        full: bool,
    }

    impl Scenario {
        /// Built-in blueprint 0 applied in the current thread's language
        fn new(full: bool) -> Scenario {
            let bp = PRESETS[0].to_blueprint();
            let loaded = full.then(|| {
                pipeline::load_data(
                    std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/sales.csv")),
                    8,
                )
                .unwrap()
            });
            let result = full.then(|| pipeline::compile(&bp.to_request(vec![], loaded.clone())));
            let mut goals: Vec<Goal> = bp
                .goals
                .iter()
                .zip(1..)
                .map(|(g, id)| Goal {
                    id,
                    text: g.text.clone(),
                    on: g.on,
                })
                .collect();
            goals.push(Goal {
                id: 9,
                text: String::new(),
                on: false,
            });
            Scenario {
                role: bp.role.clone(),
                goals,
                format: bp.format,
                fields: bp.fields.clone(),
                target: bp.target,
                project: bp.project.clone(),
                theme_idx: 6, // longest theme name
                fog: true,
                sample_rows: 8,
                llm_model: "claude-sonnet-5-5".into(),
                opts: LlmOptions::default(),
                loaded,
                result,
                full,
            }
        }

        /// Draws the canvas for a few frames without a window and returns the node areas (adds the Korean font, like
        /// the real app).
        fn render(&mut self) -> HashMap<NodeKey, Rect> {
            let ctx = egui::Context::default();
            crate::app::install_korean_font(&ctx);
            theme::apply(&ctx, &THEMES[self.theme_idx]); // the real app's spacing and button padding
            let mut canvas = Canvas::default();
            canvas.reset_layout(&self.goals);
            for frame in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                    time: Some(frame as f64 * 0.1),
                    ..Default::default()
                };
                let full = self.full;
                let palette = THEMES[self.theme_idx];
                let out = ctx.run_ui(input, |ui| {
                    let mut m = Model {
                        role: &mut self.role,
                        goals: &mut self.goals,
                        format: &mut self.format,
                        fields_text: &mut self.fields,
                        target: &mut self.target,
                        loaded: self.loaded.as_ref(),
                        result: self.result.as_ref(),
                        loading: full,
                        compiling: full,
                        stale: false,
                        project: &mut self.project,
                        dirty: full,
                        theme_idx: &mut self.theme_idx,
                        fog: &mut self.fog,
                        fog_theme: true,
                        sample_rows: &mut self.sample_rows,
                        llm: LlmView {
                            provider: "Anthropic",
                            key_env: "ANTHROPIC_API_KEY",
                            model: &mut self.llm_model,
                            model_hint: "claude-sonnet-5-5",
                            models: crate::canvas::graph::Provider::Anthropic.models(),
                            has_key: full,
                            running: full,
                            verdict: full.then(|| (false, "x".repeat(60))),
                            opts: &mut self.opts,
                            temp_max: 1.0,
                            details: full.then(|| "y".repeat(60)),
                            retry_limit: full.then_some(32_000),
                        },
                    };
                    let _ = canvas.show(ui, &mut m, &palette);
                });
                out.drop_without_applying_deltas();
            }
            canvas.rects.clone()
        }
    }

    fn node_width(k: NodeKey) -> f32 {
        match k {
            NodeKey::Goal(_) => 260.0,
            NodeKey::Root => 230.0,
            NodeKey::Result => 270.0,
            _ => 250.0,
        }
    }

    /// The node width including its border (1.5 px) does not exceed the set width (no text pushed the node wider).
    fn assert_within_width(rects: &HashMap<NodeKey, Rect>, what: &str) {
        assert!(!rects.is_empty(), "{what}");
        for (k, r) in rects {
            assert!(
                r.width() <= node_width(*k) + 3.5,
                "{what} {k:?}: {:.1} > {}",
                r.width(),
                node_width(*k)
            );
        }
    }

    #[test]
    fn nodes_keep_their_width_in_both_languages() {
        for lang in Lang::ALL {
            for full in [false, true] {
                // Default run options, and a set temperature (the summary is appended to the run options title)
                for temperature in [None, Some(0.7)] {
                    let rects = i18n::with_lang(lang, || {
                        let mut s = Scenario::new(full);
                        s.opts = LlmOptions {
                            temperature,
                            ..LlmOptions::default()
                        };
                        s.render()
                    });
                    assert_within_width(&rects, &format!("{lang:?} full={full} temp={temperature:?}"));
                }
            }
        }
    }

    #[test]
    fn startup_layout_estimates_cover_measured_heights() {
        // The startup layout is computed from the estimated heights before drawing, so nodes overlap if a real height
        // exceeds the estimate plus the gap.
        for lang in Lang::ALL {
            let mut s = i18n::with_lang(lang, || Scenario::new(false));
            let rects = i18n::with_lang(lang, || s.render());
            for (k, r) in &rects {
                let estimate = match k {
                    NodeKey::Goal(id) => {
                        let g = s.goals.iter().find(|g| g.id == *id).unwrap();
                        estimate_goal_height(&g.text)
                    }
                    _ => default_height(*k),
                };
                assert!(
                    r.height() <= estimate + GAP,
                    "{lang:?} {k:?}: {:.1} > {estimate} + {GAP}",
                    r.height()
                );
            }
        }
    }

    #[test]
    fn templates_exist_in_both_languages() {
        let hangul = |s: &str| s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
        for (cat, items) in TEMPLATES {
            assert!(!items.is_empty(), "{cat:?}");
            for t in std::iter::once(&cat).chain(items) {
                assert!(!t.ko().trim().is_empty() && !t.en().trim().is_empty(), "{t:?}");
                assert!(hangul(t.ko()) && !hangul(t.en()), "{t:?}");
            }
        }
        // Picking one makes the sentence in the current language the constraint (the same value as the menu)
        let (_, items) = TEMPLATES[1];
        assert_eq!(i18n::with_lang(Lang::En, || items[0].get()), "Detect outliers first");
        assert_eq!(
            i18n::with_lang(Lang::Ko, || items[0].get()),
            ko_text!("Detect outliers first")
        );
    }

    #[test]
    fn llm_options_summary_in_english() {
        let mut o = LlmOptions::default();
        i18n::with_lang(Lang::En, || {
            assert_eq!(options_summary(&o, 1.0), "max 2048 · default temp");
            o.temperature = Some(1.5);
            o.system = "s".into();
            assert_eq!(options_summary(&o, 1.0), "max 2048 · temp 1.00 · system");
        });
    }
}
