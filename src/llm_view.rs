//! Visual parts of the VIII LLM run result: required-field chips, token usage bars, a chart and table of recent runs,
//! and the analysis tools for the reply itself (overview tiles, sortable and filterable tables with data bars, bar and
//! donut charts, column statistics, CSV export, and a check of which data columns and outliers the reply mentions).
//!
//! The reply is parsed once into a `ReplyModel` (on the worker thread that received it), so drawing a frame never
//! parses JSON or lays out thousands of rows.

use crate::canvas::graph::Provider;
use crate::canvas::llm::Usage;
use crate::pipeline;
use crate::theme::{self, Palette};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke};
use serde_json::Value;
use std::collections::HashMap;

/// How the reply body is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplyTab {
    /// Overview tiles, tables with sorting and filtering, charts, and the coverage of the data
    #[default]
    Analysis,
    /// JSON as a tree with tables, a Markdown table as a grid
    Structured,
    /// The reply text as it came back (pretty-printed when it is JSON)
    Text,
}

/// One finished run, for the history chart and table.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRecord {
    pub provider: Provider,
    pub model: String,
    pub ms: u64,
    pub usage: Option<Usage>,
    /// The reply passed the format and required-field check
    pub passed: bool,
    /// The call itself failed (HTTP or network error)
    pub failed: bool,
}

/// Number of runs kept in the history.
pub const HISTORY_LEN: usize = 12;

/// Adds a run to the history and drops the oldest one beyond HISTORY_LEN.
pub fn push_record(history: &mut Vec<RunRecord>, r: RunRecord) {
    history.push(r);
    if history.len() > HISTORY_LEN {
        let extra = history.len() - HISTORY_LEN;
        history.drain(..extra);
    }
}

/// State of one required field in the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldState {
    Present,
    /// The key is there but the value is null or an empty string
    Empty,
    Missing,
}

/// Each required field with its state, in the schema's order.
pub fn field_states(required: &[String], missing: &[String], nulls: &[String]) -> Vec<(String, FieldState)> {
    required
        .iter()
        .map(|f| {
            let state = if missing.contains(f) {
                FieldState::Missing
            } else if nulls.contains(f) {
                FieldState::Empty
            } else {
                FieldState::Present
            };
            (f.clone(), state)
        })
        .collect()
}

/// Colored chips for the required fields: ✔ present, ⚠ empty, ✖ missing.
pub fn field_chips(ui: &mut egui::Ui, p: &Palette, states: &[(String, FieldState)]) {
    if states.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr!("Fields")).small().color(p.sub));
        for (name, state) in states {
            let (mark, color, tip) = match state {
                FieldState::Present => ("✔", theme::OK, tr!("Present")),
                FieldState::Empty => ("⚠", p.warn(), tr!("Present but empty (null or \"\")")),
                FieldState::Missing => ("✖", theme::ERR, tr!("Missing from the reply")),
            };
            chip(ui, color, &format!("{mark} {name}"), tip);
        }
    });
}

/// A small rounded label in `color` (used for fields, columns and outliers).
fn chip(ui: &mut egui::Ui, color: Color32, text: &str, tip: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.7)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        })
        .response
        .on_hover_text(tip);
}

/// Share of the output limit used, 0..=1 (0 when there is no limit).
pub fn output_fraction(output: u64, max_tokens: u32) -> f32 {
    if max_tokens == 0 {
        return 0.0;
    }
    (output as f32 / max_tokens as f32).clamp(0.0, 1.0)
}

/// One labelled horizontal bar: `frac` of the width is filled, `text` is written at its right end.
fn bar(ui: &mut egui::Ui, p: &Palette, label: &str, label_w: f32, frac: f32, fill: Color32, text: String) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [label_w, 16.0],
            egui::Label::new(RichText::new(label).small().color(p.sub)).truncate(),
        );
        let w = (ui.available_width() - 4.0).max(60.0);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 16.0), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(4), p.line.gamma_multiply(0.6));
        let mut filled = rect;
        filled.set_width(rect.width() * frac.clamp(0.0, 1.0));
        if filled.width() > 0.5 {
            painter.rect_filled(filled, CornerRadius::same(4), fill);
        }
        painter.text(
            rect.right_center() - egui::vec2(6.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            &text,
            egui::FontId::proportional(11.0),
            p.hi,
        );
        resp.on_hover_text(text);
    });
}

/// Token usage as bars: input and output on one scale, and the share of the output limit used.
pub fn token_bars(ui: &mut egui::Ui, p: &Palette, usage: Usage, prompt_estimate: usize, max_tokens: u32) {
    let scale = usage.input.max(usage.output).max(1) as f32;
    bar(
        ui,
        p,
        tr!("Input"),
        64.0,
        usage.input as f32 / scale,
        p.accent.gamma_multiply(0.8),
        trf!("{n} tokens (estimate {est})", n = usage.input, est = prompt_estimate),
    );
    bar(
        ui,
        p,
        tr!("Output"),
        64.0,
        usage.output as f32 / scale,
        theme::OK.gamma_multiply(0.8),
        trf!("{n} tokens", n = usage.output),
    );
    let frac = output_fraction(usage.output, max_tokens);
    let fill = if frac >= 0.95 {
        theme::ERR
    } else if frac >= 0.8 {
        p.warn()
    } else {
        theme::OK
    }
    .gamma_multiply(0.8);
    bar(
        ui,
        p,
        tr!("Limit"),
        64.0,
        frac,
        fill,
        trf!(
            "{pct}% of {max}",
            pct = format!("{:.0}", frac * 100.0),
            max = max_tokens
        ),
    );
}

/// Color of a run in the history.
fn record_color(r: &RunRecord, p: &Palette) -> Color32 {
    if r.failed {
        theme::ERR
    } else if r.passed {
        theme::OK
    } else {
        p.warn()
    }
}

/// Bar chart of the response times of recent runs (oldest left). Hovering a bar shows its details.
pub fn latency_chart(ui: &mut egui::Ui, p: &Palette, history: &[RunRecord]) {
    if history.is_empty() {
        return;
    }
    let max_ms = history.iter().map(|r| r.ms).max().unwrap_or(1).max(1) as f32;
    let w = ui.available_width().max(120.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 64.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0, p.line),
        egui::StrokeKind::Inside,
    );
    let slot = rect.width() / HISTORY_LEN as f32;
    for (i, r) in history.iter().enumerate() {
        let h = ((r.ms as f32 / max_ms) * (rect.height() - 14.0)).max(2.0);
        let x = rect.left() + slot * i as f32;
        let bar = egui::Rect::from_min_max(
            egui::pos2(x + slot * 0.2, rect.bottom() - 4.0 - h),
            egui::pos2(x + slot * 0.8, rect.bottom() - 4.0),
        );
        painter.rect_filled(bar, CornerRadius::same(3), record_color(r, p).gamma_multiply(0.85));
        let resp = ui.interact(bar, ui.scope_id().with(("latency-bar", i)), Sense::hover());
        resp.on_hover_text(record_line(r));
    }
    painter.text(
        rect.left_top() + egui::vec2(6.0, 3.0),
        egui::Align2::LEFT_TOP,
        trf!("max {ms} ms", ms = max_ms as u64),
        egui::FontId::proportional(10.0),
        p.sub,
    );
}

/// "Anthropic · model · 1234 ms · in 10 / out 20 · passed"
fn record_line(r: &RunRecord) -> String {
    let tokens = match r.usage {
        Some(u) => trf!("in {i} / out {o}", i = u.input, o = u.output),
        None => "-".into(),
    };
    format!(
        "{} · {} · {} ms · {} · {}",
        r.provider.label(),
        r.model,
        r.ms,
        tokens,
        result_text(r)
    )
}

fn result_text(r: &RunRecord) -> &'static str {
    if r.failed {
        tr!("call failed")
    } else if r.passed {
        tr!("passed")
    } else {
        tr!("check failed")
    }
}

/// Table of recent runs, newest first.
pub fn history_table(ui: &mut egui::Ui, p: &Palette, history: &[RunRecord]) {
    egui::Grid::new("llm-history")
        .striped(true)
        .num_columns(4)
        .show(ui, |ui| {
            for h in [tr!("Model"), tr!("Time"), tr!("Tokens"), tr!("Result")] {
                ui.label(RichText::new(h).small().color(p.sub));
            }
            ui.end_row();
            for r in history.iter().rev() {
                ui.label(RichText::new(&r.model).small())
                    .on_hover_text(r.provider.label());
                ui.label(RichText::new(format!("{} ms", r.ms)).small());
                ui.label(
                    RichText::new(match r.usage {
                        Some(u) => format!("{} / {}", u.input, u.output),
                        None => "-".into(),
                    })
                    .small(),
                );
                ui.label(RichText::new(result_text(r)).small().color(record_color(r, p)));
                ui.end_row();
            }
        });
}

// ---------- the reply as data ----------

/// Most tables taken from one reply.
pub const MAX_TABLES: usize = 12;
/// Most rows kept per table.
pub const MAX_TABLE_ROWS: usize = 2000;
/// Most columns kept per table.
pub const MAX_COLS: usize = 12;
/// Rows drawn at once (the filter narrows the rest).
const SHOWN_ROWS: usize = 100;
/// Most bars (or slices) drawn in a chart.
const MAX_BARS: usize = 15;
const MAX_SLICES: usize = 7;

/// Reads a number the way a reader would: thousands commas, a trailing `%` and a leading `$` are allowed.
pub fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim().trim_end_matches('%').trim();
    let t = t.strip_prefix('$').unwrap_or(t);
    if t.is_empty() || t.len() > 32 {
        return None;
    }
    let cleaned: String = t.chars().filter(|c| *c != ',').collect();
    if !cleaned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
    {
        return None;
    }
    cleaned.parse::<f64>().ok().filter(|x| x.is_finite())
}

/// A number for display: whole numbers without decimals, others with at most 2.
pub fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A table found in a reply: an array of objects, an object of numbers (key and value columns) or a Markdown table.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// The JSON path it came from (`outliers`, `a.b[0]`); empty for the whole reply or a Markdown table
    pub title: String,
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// The number in each cell (None for text and empty cells), same shape as `rows`
    pub nums: Vec<Vec<Option<f64>>>,
    /// A column is numeric when at least 90% of its filled cells are numbers
    pub numeric: Vec<bool>,
    /// Lowercase text of each row, for the filter
    search: Vec<String>,
}

impl Table {
    pub fn new(title: String, mut header: Vec<String>, rows: Vec<Vec<String>>) -> Table {
        header.truncate(MAX_COLS);
        let width = header.len();
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .take(MAX_TABLE_ROWS)
            .map(|mut r| {
                r.resize(width, String::new());
                r
            })
            .collect();
        let nums: Vec<Vec<Option<f64>>> = rows.iter().map(|r| r.iter().map(|c| parse_num(c)).collect()).collect();
        let numeric = (0..width)
            .map(|c| {
                let filled = rows.iter().filter(|r| !r[c].trim().is_empty()).count();
                let numbers = nums.iter().filter(|r| r[c].is_some()).count();
                filled > 0 && numbers * 10 >= filled * 9
            })
            .collect();
        let search = rows.iter().map(|r| r.join("\u{1}").to_lowercase()).collect();
        Table {
            title,
            header,
            rows,
            nums,
            numeric,
            search,
        }
    }
}

/// What kind of value an overview tile summarizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileKind {
    Text,
    Number,
    List,
    Other,
}

/// One value of the reply's top level, as a tile.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub name: String,
    pub kind: TileKind,
    pub detail: String,
}

/// Which data columns and outliers of the compiled prompt the reply mentions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// (column name, mentioned in the reply)
    pub columns: Vec<(String, bool)>,
    /// (outlier line of the data profile, how the reply mentions it)
    pub outliers: Vec<(String, Mention)>,
}

/// How a reply refers to an outlier of the data profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mention {
    /// The column and the value are both in the reply
    Value,
    /// Only the column name is
    Column,
    No,
}

/// The reply parsed once: the JSON value, the tables in it, overview tiles and the data coverage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplyModel {
    /// The reply as JSON (None for a Markdown table or plain text)
    pub value: Option<Value>,
    pub tables: Vec<Table>,
    pub tiles: Vec<Tile>,
    /// None when the prompt had no data
    pub coverage: Option<Coverage>,
}

impl ReplyModel {
    /// Parses a reply. `columns` and `anomalies` come from the data profile of the compiled prompt (empty without
    /// data).
    pub fn build(text: &str, columns: &[String], anomalies: &[String]) -> ReplyModel {
        let body = text.trim();
        let mut model = ReplyModel::default();
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            model.tiles = tiles(&v);
            collect_tables(&v, "", 0, &mut model.tables);
            model.value = Some(v);
        } else if let Some((header, rows)) = pipeline::parse_reply_table(body) {
            model.tables.push(Table::new(String::new(), header, rows));
        }
        if !columns.is_empty() {
            model.coverage = Some(coverage(body, columns, anomalies));
        }
        model
    }

    /// The tab that shows this reply best: the analysis tools when there is a table, else the structure view.
    pub fn default_tab(&self) -> ReplyTab {
        if self.tables.is_empty() {
            ReplyTab::Structured
        } else {
            ReplyTab::Analysis
        }
    }
}

/// Text of one JSON value in a table cell. Short lists of plain values are joined; anything nested is summarized.
fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => {
            a.iter().map(cell_text).collect::<Vec<_>>().join("; ")
        }
        other => value_summary(other, 300),
    }
}

fn is_number_like(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::String(s) => parse_num(s).is_some(),
        _ => false,
    }
}

fn join_path(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Finds the tables in a JSON reply: arrays of objects, and objects of mostly numbers (as key and value rows).
fn collect_tables(v: &Value, path: &str, depth: usize, out: &mut Vec<Table>) {
    if out.len() >= MAX_TABLES || depth > 3 {
        return;
    }
    match v {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let cols = object_columns(items, MAX_COLS);
            let rows = items
                .iter()
                .take(MAX_TABLE_ROWS)
                .map(|it| {
                    cols.iter()
                        .map(|c| it.get(c).map(cell_text).unwrap_or_default())
                        .collect()
                })
                .collect();
            out.push(Table::new(path.to_string(), cols, rows));
        }
        Value::Array(items) => {
            for (i, x) in items.iter().enumerate().take(20) {
                if x.is_object() || x.is_array() {
                    collect_tables(x, &format!("{path}[{i}]"), depth + 1, out);
                }
            }
        }
        Value::Object(map) => {
            let scalar = |x: &Value| !x.is_object() && !x.is_array();
            let numbers = map.values().filter(|x| is_number_like(x)).count();
            if map.len() >= 2 && map.values().all(scalar) && numbers * 10 >= map.len() * 8 {
                let rows = map.iter().map(|(k, x)| vec![k.clone(), cell_text(x)]).collect();
                out.push(Table::new(path.to_string(), vec!["key".into(), "value".into()], rows));
            }
            for (k, x) in map {
                collect_tables(x, &join_path(path, k), depth + 1, out);
            }
        }
        _ => {}
    }
}

/// Overview tiles for the top level of a JSON reply (at most 12).
fn tiles(v: &Value) -> Vec<Tile> {
    let one = |name: &str, x: &Value| {
        let (kind, detail) = match x {
            Value::String(s) => (TileKind::Text, truncate(s, 36)),
            Value::Number(n) => (TileKind::Number, n.to_string()),
            Value::Bool(b) => (TileKind::Other, b.to_string()),
            Value::Null => (TileKind::Other, "null".to_string()),
            Value::Array(a) => (TileKind::List, trf!("{n} items", n = a.len())),
            Value::Object(o) => (TileKind::List, trf!("{n} keys", n = o.len())),
        };
        Tile {
            name: name.to_string(),
            kind,
            detail,
        }
    };
    match v {
        Value::Object(map) => map.iter().take(12).map(|(k, x)| one(k, x)).collect(),
        Value::Array(a) => vec![Tile {
            name: "[ ]".into(),
            kind: TileKind::List,
            detail: trf!("{n} items", n = a.len()),
        }],
        _ => vec![],
    }
}

/// Whether `hay` has the number `needle` as a whole number (not inside a longer number or a word). Thousands commas in
/// `hay` are ignored by the caller.
fn has_number(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, m)| {
        let before = hay[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = hay[i + m.len()..].chars().next().is_none_or(|c| !c.is_ascii_digit());
        before && after
    })
}

/// Which columns and outliers of the data profile the reply mentions. An outlier line looks like
/// `amount: max 90000 (z=5.7)`.
pub fn coverage(reply: &str, columns: &[String], anomalies: &[String]) -> Coverage {
    let lower = reply.to_lowercase();
    let plain = lower.replace(',', "");
    // Short ASCII names ("id", "n") are in every text; two Hangul or kanji characters are already a word
    let named = |c: &str| {
        let c = c.trim();
        c.chars().count() >= if c.is_ascii() { 3 } else { 2 } && lower.contains(&c.to_lowercase())
    };
    Coverage {
        columns: columns.iter().map(|c| (c.clone(), named(c))).collect(),
        outliers: anomalies
            .iter()
            .map(|line| {
                let (col, rest) = line.split_once(':').unwrap_or((line.as_str(), ""));
                let value = rest
                    .split_whitespace()
                    .skip_while(|w| !matches!(*w, "max" | "min"))
                    .nth(1)
                    .unwrap_or("");
                let col_in = named(col);
                let value_in = !value.is_empty() && has_number(&plain, &value.replace(',', "").to_lowercase());
                let mention = match (col_in, value_in) {
                    (true, true) => Mention::Value,
                    (true, false) => Mention::Column,
                    // The value alone (a reply that says "48210 sessions" without the column name) still counts
                    (false, true) => Mention::Value,
                    (false, false) => Mention::No,
                };
                (line.clone(), mention)
            })
            .collect(),
    }
}

// ---------- analysis state and pure helpers ----------

/// How the chart draws its groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChartKind {
    #[default]
    Bars,
    Donut,
}

/// What the user chose in the Analysis tab: the table, sort, filter and chart settings.
#[derive(Clone, Debug, Default)]
pub struct AnalysisState {
    pub table: usize,
    /// (column, descending)
    pub sort: Option<(usize, bool)>,
    pub filter: String,
    pub chart: ChartKind,
    /// Column that names the groups of the chart
    pub x: usize,
    /// Column to add up per group; None counts the rows
    pub y: Option<usize>,
    /// Row indices after the filter and the sort
    visible: Vec<usize>,
    /// (table, sort, filter) that `visible` was built for
    key: Option<ViewKey>,
}

/// The table, sort and filter that a list of visible rows was built for.
type ViewKey = (usize, Option<(usize, bool)>, String);

impl AnalysisState {
    /// Defaults for a table: groups by its first text column and adds up its first number column.
    pub fn for_table(index: usize, t: &Table) -> AnalysisState {
        let x = t.numeric.iter().position(|n| !n).unwrap_or(0);
        let y = t.numeric.iter().enumerate().position(|(i, n)| *n && i != x);
        AnalysisState {
            table: index,
            x,
            y,
            ..AnalysisState::default()
        }
    }

    pub fn for_model(m: Option<&ReplyModel>) -> AnalysisState {
        m.and_then(|m| m.tables.first())
            .map_or_else(AnalysisState::default, |t| AnalysisState::for_table(0, t))
    }

    /// Row indices of the table after the filter and the sort (as of the last draw).
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// Keeps the columns in range and rebuilds the visible rows when the table, sort or filter changed.
    fn sync(&mut self, t: &Table) {
        let n = t.header.len();
        self.x = self.x.min(n.saturating_sub(1));
        self.y = self.y.filter(|y| *y < n);
        self.sort = self.sort.filter(|(c, _)| *c < n);
        let key = (self.table, self.sort, self.filter.clone());
        if self.key.as_ref() != Some(&key) {
            self.visible = visible_rows(t, self.sort, &self.filter);
            self.key = Some(key);
        }
    }
}

/// Compares two optional numbers; empty cells go last in either direction.
fn cmp_num(a: Option<f64>, b: Option<f64>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => {
            let o = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            if desc { o.reverse() } else { o }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Row indices of a table after the filter (any cell contains the text, ignoring case) and the sort.
pub fn visible_rows(t: &Table, sort: Option<(usize, bool)>, filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    let mut idx: Vec<usize> = (0..t.rows.len())
        .filter(|&i| needle.is_empty() || t.search[i].contains(&needle))
        .collect();
    if let Some((col, desc)) = sort.filter(|(c, _)| *c < t.header.len()) {
        if t.numeric[col] {
            idx.sort_by(|&a, &b| cmp_num(t.nums[a][col], t.nums[b][col], desc));
        } else {
            idx.sort_by(|&a, &b| {
                let (x, y) = (
                    t.rows[a][col].trim().to_lowercase(),
                    t.rows[b][col].trim().to_lowercase(),
                );
                match (x.is_empty(), y.is_empty()) {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    (false, false) if desc => y.cmp(&x),
                    (false, false) => x.cmp(&y),
                }
            });
        }
    }
    idx
}

/// The next sort when a header is clicked: ascending, then descending, then none.
pub fn next_sort(current: Option<(usize, bool)>, col: usize) -> Option<(usize, bool)> {
    match current {
        Some((c, false)) if c == col => Some((col, true)),
        Some((c, true)) if c == col => None,
        _ => Some((col, false)),
    }
}

/// Statistics of a numeric column over some rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NumStats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
}

impl NumStats {
    pub fn mean(&self) -> f64 {
        self.sum / self.count as f64
    }
}

pub fn column_stats(t: &Table, col: usize, rows: &[usize]) -> Option<NumStats> {
    let mut it = rows.iter().filter_map(|&i| t.nums.get(i)?.get(col).copied().flatten());
    let first = it.next()?;
    let mut s = NumStats {
        count: 1,
        min: first,
        max: first,
        sum: first,
    };
    for x in it {
        s.count += 1;
        s.min = s.min.min(x);
        s.max = s.max.max(x);
        s.sum += x;
    }
    Some(s)
}

/// Groups for a chart: the rows are grouped by column `x` and either counted (`y` None) or their `y` numbers added
/// up. Largest first. Rows without a `y` number are skipped.
pub fn chart_points(t: &Table, rows: &[usize], x: usize, y: Option<usize>) -> Vec<(String, f64)> {
    let mut order: Vec<String> = Vec::new();
    let mut sums: HashMap<String, f64> = HashMap::new();
    for &i in rows {
        let value = match y {
            None => Some(1.0),
            Some(c) => t.nums.get(i).and_then(|r| r.get(c)).copied().flatten(),
        };
        let Some(value) = value else { continue };
        let label = t.rows.get(i).and_then(|r| r.get(x)).map_or("", |s| s.trim());
        let label = if label.is_empty() { "—" } else { label };
        let slot = sums.entry(label.to_string()).or_insert_with(|| {
            order.push(label.to_string());
            0.0
        });
        *slot += value;
    }
    let mut points: Vec<(String, f64)> = order
        .into_iter()
        .map(|l| {
            let v = sums[&l];
            (l, v)
        })
        .collect();
    points.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    points
}

/// CSV text of some rows of a table (quoted where needed). `bom` adds the byte order mark Excel needs for UTF-8.
pub fn to_csv(t: &Table, rows: &[usize], bom: bool) -> String {
    let quote = |s: &str| {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let line = |cells: &[String]| cells.iter().map(|c| quote(c)).collect::<Vec<_>>().join(",");
    let mut out = String::new();
    if bom {
        out.push('\u{feff}');
    }
    out.push_str(&line(&t.header));
    out.push('\n');
    for &i in rows {
        if let Some(r) = t.rows.get(i) {
            out.push_str(&line(r));
            out.push('\n');
        }
    }
    out
}

// ---------- drawing the analysis ----------

/// Something the Analysis tab asks the app to do (a file dialog must open at the end of the frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisAct {
    SaveCsv,
}

const SLICE_COLORS: [Color32; 8] = [
    Color32::from_rgb(0xe8, 0x8a, 0x2d),
    Color32::from_rgb(0x6e, 0xa8, 0xfe),
    Color32::from_rgb(0x8f, 0xd1, 0x9e),
    Color32::from_rgb(0xc7, 0x92, 0xea),
    Color32::from_rgb(0xef, 0x7b, 0x72),
    Color32::from_rgb(0xf2, 0xcc, 0x60),
    Color32::from_rgb(0x89, 0xdd, 0xff),
    Color32::from_rgb(0x9a, 0x9a, 0x9a),
];

/// The Analysis tab: overview tiles, the coverage of the data, one table with sort, filter and CSV export, column
/// statistics and a chart.
pub fn analysis_ui(ui: &mut egui::Ui, p: &Palette, model: &ReplyModel, st: &mut AnalysisState) -> Option<AnalysisAct> {
    tiles_ui(ui, p, &model.tiles);
    if let Some(cov) = &model.coverage {
        coverage_ui(ui, p, cov);
    }
    if model.tables.is_empty() {
        ui.label(
            RichText::new(tr!(
                "No table in this reply (an array of objects, an object of numbers or a Markdown table is needed)"
            ))
            .small()
            .color(p.sub),
        );
        return None;
    }
    let mut act = None;
    st.table = st.table.min(model.tables.len() - 1);
    if model.tables.len() > 1 {
        let mut pick = st.table;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("Table")).small().color(p.sub));
            egui::ComboBox::from_id_salt("llm-table-pick")
                .selected_text(table_name(&model.tables[st.table], st.table))
                .show_ui(ui, |ui| {
                    for (i, t) in model.tables.iter().enumerate() {
                        ui.selectable_value(&mut pick, i, table_name(t, i));
                    }
                });
        });
        if pick != st.table {
            *st = AnalysisState::for_table(pick, &model.tables[pick]);
        }
    }
    let t = &model.tables[st.table];
    st.sync(t);
    let total = t.rows.len();

    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut st.filter)
                .hint_text(tr!("Filter rows…"))
                .desired_width(150.0),
        );
        ui.label(
            RichText::new(trf!("{shown} of {total} rows", shown = st.visible.len(), total = total))
                .small()
                .color(p.sub),
        );
        if ui
            .small_button(tr!("Copy CSV"))
            .on_hover_text(tr!("Copies the rows shown (with the filter and sort) as CSV"))
            .clicked()
        {
            ui.ctx().copy_text(to_csv(t, &st.visible, false));
        }
        if ui.small_button(tr!("Save CSV…")).clicked() {
            act = Some(AnalysisAct::SaveCsv);
        }
    });

    table_ui(ui, p, t, st);
    stats_ui(ui, p, t, st);
    chart_ui(ui, p, t, st);
    act
}

/// Name of a table in the picker.
fn table_name(t: &Table, i: usize) -> String {
    let name = if t.title.is_empty() {
        tr!("Reply").to_string()
    } else {
        t.title.clone()
    };
    format!("{}. {name} ({}×{})", i + 1, t.rows.len(), t.header.len())
}

fn tiles_ui(ui: &mut egui::Ui, p: &Palette, tiles: &[Tile]) {
    if tiles.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for t in tiles {
            let color = match t.kind {
                TileKind::Number => p.accent,
                TileKind::List => theme::OK,
                TileKind::Text | TileKind::Other => p.hi,
            };
            egui::Frame::new()
                .fill(p.panel2.gamma_multiply(0.6))
                .stroke(Stroke::new(1.0, p.line))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&t.name).small().color(p.sub));
                        ui.label(RichText::new(&t.detail).strong().color(color));
                    });
                });
        }
    });
}

fn coverage_ui(ui: &mut egui::Ui, p: &Palette, cov: &Coverage) {
    let mentioned = cov.columns.iter().filter(|(_, m)| *m).count();
    egui::CollapsingHeader::new(trf!(
        "Data coverage · {a} of {b} columns mentioned",
        a = mentioned,
        b = cov.columns.len()
    ))
    .id_salt("llm-coverage")
    .default_open(true)
    .show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            for (name, yes) in &cov.columns {
                let (mark, color, tip) = if *yes {
                    ("✔", theme::OK, tr!("The reply names this column"))
                } else {
                    ("–", p.sub, tr!("The reply does not name this column"))
                };
                chip(ui, color, &format!("{mark} {name}"), tip);
            }
        });
        if !cov.outliers.is_empty() {
            ui.label(RichText::new(tr!("Outliers found in the data")).small().color(p.sub));
            for (line, m) in &cov.outliers {
                let (mark, color, tip) = match m {
                    Mention::Value => ("✔", theme::OK, tr!("The reply mentions this outlier")),
                    Mention::Column => ("◐", p.warn(), tr!("The reply names the column but not this value")),
                    Mention::No => ("✖", theme::ERR, tr!("The reply does not mention this outlier")),
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(mark).color(color)).on_hover_text(tip);
                    ui.label(RichText::new(line).small());
                });
            }
        }
    });
}

fn table_ui(ui: &mut egui::Ui, p: &Palette, t: &Table, st: &mut AnalysisState) {
    let shown: Vec<usize> = st.visible.iter().copied().take(SHOWN_ROWS).collect();
    // The largest absolute value of each numeric column among the visible rows scales its data bars
    let col_max: Vec<f64> = (0..t.header.len())
        .map(|c| {
            st.visible
                .iter()
                .filter_map(|&i| t.nums[i][c])
                .fold(0.0_f64, |m, x| m.max(x.abs()))
        })
        .collect();
    let (table_idx, sort) = (st.table, st.sort);
    let mut clicked = None;
    egui::ScrollArea::horizontal()
        .id_salt(("llm-table-scroll", table_idx))
        .show(ui, |ui| {
            egui::Grid::new(("llm-table-grid", table_idx))
                .striped(true)
                .spacing([10.0, 3.0])
                .show(ui, |ui| {
                    for (c, h) in t.header.iter().enumerate() {
                        let arrow = match sort {
                            Some((sc, desc)) if sc == c => {
                                if desc {
                                    " ▼"
                                } else {
                                    " ▲"
                                }
                            }
                            _ => "",
                        };
                        let r = ui.add(
                            egui::Label::new(RichText::new(format!("{h}{arrow}")).size(12.0).strong().color(p.accent))
                                .sense(Sense::click()),
                        );
                        if r.on_hover_text(tr!("Click to sort")).clicked() {
                            clicked = Some(c);
                        }
                    }
                    ui.end_row();
                    for &i in &shown {
                        for (c, max) in col_max.iter().enumerate() {
                            cell_ui(ui, p, t, i, c, *max);
                        }
                        ui.end_row();
                    }
                });
        });
    if let Some(c) = clicked {
        st.sort = next_sort(st.sort, c);
    }
    if st.visible.len() > shown.len() {
        ui.label(
            RichText::new(trf!(
                "… {n} more rows (narrow them with the filter)",
                n = st.visible.len() - shown.len()
            ))
            .small()
            .color(p.sub),
        );
    }
}

/// One cell: numbers get a data bar scaled to the column, text is cut to fit (the full text is in the tooltip).
fn cell_ui(ui: &mut egui::Ui, p: &Palette, t: &Table, row: usize, col: usize, col_max: f64) {
    let text = &t.rows[row][col];
    match t.nums[row][col] {
        Some(x) if t.numeric[col] => {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(96.0, 16.0), Sense::hover());
            let painter = ui.painter_at(rect);
            if col_max > 0.0 {
                let mut bar = rect;
                bar.set_width(rect.width() * (x.abs() / col_max).clamp(0.0, 1.0) as f32);
                let color = if x < 0.0 { theme::ERR } else { p.accent };
                painter.rect_filled(bar, CornerRadius::same(3), color.gamma_multiply(0.35));
            }
            painter.text(
                rect.right_center() - egui::vec2(4.0, 0.0),
                egui::Align2::RIGHT_CENTER,
                truncate(text.trim(), 14),
                egui::FontId::proportional(12.0),
                p.hi,
            );
            resp.on_hover_text(text);
        }
        _ => {
            let shown = truncate(text, 48);
            let full = shown != *text;
            let r = ui.add(egui::Label::new(RichText::new(shown).size(12.0)).selectable(true));
            if full {
                r.on_hover_text(text);
            }
        }
    }
}

fn stats_ui(ui: &mut egui::Ui, p: &Palette, t: &Table, st: &AnalysisState) {
    let cols: Vec<(usize, NumStats)> = (0..t.header.len())
        .filter(|&c| t.numeric[c])
        .filter_map(|c| column_stats(t, c, &st.visible).map(|s| (c, s)))
        .collect();
    if cols.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(tr!("Σ Column statistics (rows shown)"))
        .id_salt(("llm-stats", st.table))
        .show(ui, |ui| {
            egui::Grid::new(("llm-stats-grid", st.table))
                .striped(true)
                .spacing([10.0, 3.0])
                .show(ui, |ui| {
                    for h in [
                        tr!("Column"),
                        tr!("Count"),
                        tr!("Min"),
                        tr!("Mean"),
                        tr!("Max"),
                        tr!("Sum"),
                    ] {
                        ui.label(RichText::new(h).small().color(p.sub));
                    }
                    ui.end_row();
                    for (c, s) in cols {
                        ui.label(RichText::new(&t.header[c]).small().strong());
                        ui.label(RichText::new(s.count.to_string()).small());
                        ui.label(RichText::new(fmt_num(s.min)).small());
                        ui.label(RichText::new(fmt_num(s.mean())).small());
                        ui.label(RichText::new(fmt_num(s.max)).small());
                        ui.label(RichText::new(fmt_num(s.sum)).small());
                        ui.end_row();
                    }
                });
        });
}

fn chart_ui(ui: &mut egui::Ui, p: &Palette, t: &Table, st: &mut AnalysisState) {
    egui::CollapsingHeader::new(tr!("📊 Chart"))
        .id_salt(("llm-chart", st.table))
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr!("Group by")).small().color(p.sub));
                egui::ComboBox::from_id_salt(("llm-chart-x", st.table))
                    .selected_text(t.header[st.x].clone())
                    .show_ui(ui, |ui| {
                        for (i, h) in t.header.iter().enumerate() {
                            ui.selectable_value(&mut st.x, i, h);
                        }
                    });
                ui.label(RichText::new(tr!("Value")).small().color(p.sub));
                let y_text =
                    st.y.map_or_else(|| tr!("Row count").to_string(), |c| t.header[c].clone());
                egui::ComboBox::from_id_salt(("llm-chart-y", st.table))
                    .selected_text(y_text)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut st.y, None, tr!("Row count"));
                        for (i, h) in t.header.iter().enumerate().filter(|(i, _)| t.numeric[*i]) {
                            ui.selectable_value(&mut st.y, Some(i), format!("Σ {h}"));
                        }
                    });
                ui.selectable_value(&mut st.chart, ChartKind::Bars, tr!("▤ Bars"));
                ui.selectable_value(&mut st.chart, ChartKind::Donut, tr!("◔ Donut"));
            });
            let points = chart_points(t, &st.visible, st.x, st.y);
            if points.is_empty() {
                ui.label(
                    RichText::new(tr!("Nothing to chart with these settings"))
                        .small()
                        .color(p.sub),
                );
                return;
            }
            match st.chart {
                ChartKind::Bars => bars_chart(ui, p, &points),
                ChartKind::Donut => donut_chart(ui, p, &points),
            }
        });
}

fn bars_chart(ui: &mut egui::Ui, p: &Palette, points: &[(String, f64)]) {
    let max = points.iter().map(|(_, v)| v.abs()).fold(0.0_f64, f64::max);
    for (i, (label, v)) in points.iter().take(MAX_BARS).enumerate() {
        let frac = if max > 0.0 { (v.abs() / max) as f32 } else { 0.0 };
        let color = SLICE_COLORS[i % SLICE_COLORS.len()].gamma_multiply(0.85);
        bar(ui, p, &truncate(label, 18), 120.0, frac, color, fmt_num(*v));
    }
    if points.len() > MAX_BARS {
        ui.label(
            RichText::new(trf!("… {n} more groups", n = points.len() - MAX_BARS))
                .small()
                .color(p.sub),
        );
    }
}

/// Slices for the donut: the largest groups and one "others" slice. Only positive values count.
pub fn donut_slices(points: &[(String, f64)]) -> Vec<(String, f64)> {
    let positive: Vec<&(String, f64)> = points.iter().filter(|(_, v)| *v > 0.0).collect();
    let mut slices: Vec<(String, f64)> = positive.iter().take(MAX_SLICES).map(|(l, v)| (l.clone(), *v)).collect();
    let rest: f64 = positive.iter().skip(MAX_SLICES).map(|(_, v)| *v).sum();
    if rest > 0.0 {
        slices.push((tr!("Others").to_string(), rest));
    }
    slices
}

fn donut_chart(ui: &mut egui::Ui, p: &Palette, points: &[(String, f64)]) {
    let slices = donut_slices(points);
    let total: f64 = slices.iter().map(|(_, v)| *v).sum();
    if total <= 0.0 {
        ui.label(RichText::new(tr!("No positive values to show")).small().color(p.sub));
        return;
    }
    ui.horizontal(|ui| {
        let size = 140.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
        let painter = ui.painter_at(rect);
        let center = rect.center();
        let thickness = 26.0;
        let radius = size / 2.0 - thickness / 2.0 - 2.0;
        let mut start = -std::f32::consts::FRAC_PI_2;
        for (i, (_, v)) in slices.iter().enumerate() {
            let sweep = (*v / total) as f32 * std::f32::consts::TAU;
            let steps = ((sweep / 0.06).ceil() as usize).max(2);
            let arc: Vec<egui::Pos2> = (0..=steps)
                .map(|s| {
                    let a = start + sweep * s as f32 / steps as f32;
                    center + egui::vec2(a.cos(), a.sin()) * radius
                })
                .collect();
            painter.add(egui::Shape::line(
                arc,
                Stroke::new(thickness, SLICE_COLORS[i % SLICE_COLORS.len()]),
            ));
            start += sweep;
        }
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            fmt_num(total),
            egui::FontId::proportional(13.0),
            p.hi,
        );
        ui.vertical(|ui| {
            for (i, (label, v)) in slices.iter().enumerate() {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                    ui.painter()
                        .rect_filled(dot, CornerRadius::same(2), SLICE_COLORS[i % SLICE_COLORS.len()]);
                    ui.label(
                        RichText::new(format!(
                            "{}  {}  ({:.0}%)",
                            truncate(label, 22),
                            fmt_num(*v),
                            v / total * 100.0
                        ))
                        .small(),
                    );
                });
            }
        });
    });
}

// ---------- structure view ----------

/// Short one-line text for a JSON value.
pub fn value_summary(v: &Value, max_chars: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => trf!("{n} keys", n = o.len()),
        Value::Array(a) => trf!("{n} items", n = a.len()),
        other => other.to_string(),
    };
    truncate(&s, max_chars)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let cut: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Column names for a table of JSON objects: the keys in order of first appearance, at most `max`.
pub fn object_columns(items: &[Value], max: usize) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    'items: for it in items {
        if let Value::Object(o) = it {
            for k in o.keys() {
                if !cols.contains(k) {
                    cols.push(k.clone());
                    if cols.len() >= max {
                        break 'items;
                    }
                }
            }
        }
    }
    cols
}

/// Most keys or items one level of the tree draws.
const TREE_LIMIT: usize = 100;

fn grid(ui: &mut egui::Ui, p: &Palette, id: egui::Id, header: &[String], rows: &[Vec<String>]) {
    egui::ScrollArea::horizontal().id_salt(id).show(ui, |ui| {
        egui::Grid::new(id.with("grid"))
            .striped(true)
            .num_columns(header.len())
            .show(ui, |ui| {
                for h in header {
                    ui.label(RichText::new(h).small().strong().color(p.accent));
                }
                ui.end_row();
                for row in rows.iter().take(SHOWN_ROWS) {
                    for i in 0..header.len() {
                        let cell = row.get(i).map_or("", String::as_str);
                        ui.add(egui::Label::new(RichText::new(truncate(cell, 60)).small()).selectable(true))
                            .on_hover_text(cell);
                    }
                    ui.end_row();
                }
            });
    });
    if rows.len() > SHOWN_ROWS {
        ui.label(
            RichText::new(trf!("… {n} more rows", n = rows.len() - SHOWN_ROWS))
                .small()
                .color(p.sub),
        );
    }
}

fn more_label(ui: &mut egui::Ui, p: &Palette, total: usize) {
    if total > TREE_LIMIT {
        ui.label(
            RichText::new(trf!("… {n} more", n = total - TREE_LIMIT))
                .small()
                .color(p.sub),
        );
    }
}

fn value_ui(ui: &mut egui::Ui, p: &Palette, id: egui::Id, v: &Value, depth: usize) {
    match v {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let cols = object_columns(items, MAX_COLS.min(8));
            let rows: Vec<Vec<String>> = items
                .iter()
                .take(SHOWN_ROWS)
                .map(|it| {
                    cols.iter()
                        .map(|c| it.get(c).map_or(String::new(), cell_text))
                        .collect()
                })
                .collect();
            grid(ui, p, id, &cols, &rows);
            if items.len() > SHOWN_ROWS {
                ui.label(
                    RichText::new(trf!("… {n} more rows", n = items.len() - SHOWN_ROWS))
                        .small()
                        .color(p.sub),
                );
            }
        }
        Value::Array(items) => {
            for (i, it) in items.iter().enumerate().take(TREE_LIMIT) {
                if depth < 3 && (it.is_object() || it.is_array()) {
                    egui::CollapsingHeader::new(format!("[{i}] {}", value_summary(it, 40)))
                        .id_salt(id.with(i))
                        .show(ui, |ui| value_ui(ui, p, id.with(i), it, depth + 1));
                } else {
                    ui.add(egui::Label::new(RichText::new(format!("• {}", value_summary(it, 300)))).selectable(true));
                }
            }
            more_label(ui, p, items.len());
        }
        Value::Object(o) => {
            for (k, val) in o.iter().take(TREE_LIMIT) {
                if depth < 3 && (val.is_object() || val.is_array()) {
                    egui::CollapsingHeader::new(RichText::new(format!("{k}  ·  {}", value_summary(val, 30))).strong())
                        .id_salt(id.with(k))
                        .default_open(depth == 0)
                        .show(ui, |ui| value_ui(ui, p, id.with(k), val, depth + 1));
                } else {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(k).strong().color(p.accent));
                        ui.add(egui::Label::new(value_summary(val, 400)).selectable(true));
                    });
                }
            }
            more_label(ui, p, o.len());
        }
        other => {
            ui.add(egui::Label::new(value_summary(other, 2000)).selectable(true));
        }
    }
}

/// Draws the reply as a JSON tree or a table. Returns false when the reply is neither JSON nor a Markdown table.
pub fn structured_view(ui: &mut egui::Ui, p: &Palette, model: &ReplyModel) -> bool {
    let id = egui::Id::unique("llm-structured");
    if let Some(v) = &model.value {
        value_ui(ui, p, id, v, 0);
        return true;
    }
    if let Some(t) = model.tables.first() {
        ui.label(RichText::new(trf!("{n} rows", n = t.rows.len())).small().color(p.sub));
        grid(ui, p, id.with("md"), &t.header, &t.rows);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::OutputFormat;
    use crate::blueprint::Blueprint;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn table(header: &[&str], rows: &[&[&str]]) -> Table {
        Table::new("t".into(), s(header), rows.iter().map(|r| s(r)).collect())
    }

    #[test]
    fn field_states_follow_the_schema_order() {
        let st = field_states(&s(&["a", "b", "c"]), &s(&["c"]), &s(&["b"]));
        assert_eq!(
            st,
            vec![
                ("a".into(), FieldState::Present),
                ("b".into(), FieldState::Empty),
                ("c".into(), FieldState::Missing)
            ]
        );
    }

    #[test]
    fn output_fraction_is_clamped() {
        assert_eq!(output_fraction(50, 100), 0.5);
        assert_eq!(output_fraction(500, 100), 1.0);
        assert_eq!(output_fraction(5, 0), 0.0);
    }

    #[test]
    fn history_keeps_the_latest_runs() {
        let mut h = Vec::new();
        for ms in 0..20 {
            push_record(
                &mut h,
                RunRecord {
                    provider: Provider::OpenAi,
                    model: "m".into(),
                    ms,
                    usage: None,
                    passed: true,
                    failed: false,
                },
            );
        }
        assert_eq!(h.len(), HISTORY_LEN);
        assert_eq!(h.first().unwrap().ms, 20 - HISTORY_LEN as u64);
        assert_eq!(h.last().unwrap().ms, 19);
    }

    #[test]
    fn numbers_are_read_like_a_reader_would() {
        for (text, want) in [
            ("42", Some(42.0)),
            (" 1,234.5 ", Some(1234.5)),
            ("12%", Some(12.0)),
            ("$1,200", Some(1200.0)),
            ("-3.5", Some(-3.5)),
            ("1e3", Some(1000.0)),
            ("2026-09-05", None),
            ("SO-104001", None),
            ("", None),
            ("abc", None),
            ("-", None),
        ] {
            assert_eq!(parse_num(text), want, "{text:?}");
        }
        assert_eq!(fmt_num(48210.0), "48210");
        assert_eq!(fmt_num(0.4), "0.4");
        assert_eq!(fmt_num(2.456), "2.46");
    }

    #[test]
    fn a_column_is_numeric_when_nine_in_ten_cells_are_numbers() {
        let rows: Vec<Vec<String>> = (0..10)
            .map(|i| {
                vec![
                    format!("r{i}"),
                    if i == 0 { "n/a".into() } else { i.to_string() },
                    i.to_string(),
                ]
            })
            .collect();
        let t = Table::new("t".into(), s(&["name", "mostly", "all"]), rows);
        assert_eq!(t.numeric, vec![false, true, true]);
        // Rows shorter than the header are padded
        let ragged = Table::new("t".into(), s(&["a", "b"]), vec![s(&["1"])]);
        assert_eq!(ragged.rows[0], s(&["1", ""]));
    }

    #[test]
    fn json_arrays_of_objects_and_number_maps_become_tables() {
        let reply = r#"{
            "headline": "Revenue up",
            "key_metrics": {"revenue": 1200, "orders": "45", "growth": "12%"},
            "outliers": [{"row": 7, "value": 90000, "note": "spike"}, {"row": 9, "value": 12}],
            "next_steps": ["a", "b"]
        }"#;
        let m = ReplyModel::build(reply, &[], &[]);
        let names: Vec<&str> = m.tables.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(names, vec!["key_metrics", "outliers"]);
        assert_eq!(m.tables[0].header, s(&["key", "value"]));
        assert_eq!(m.tables[1].header, s(&["row", "value", "note"]));
        assert_eq!(m.tables[1].numeric, vec![true, true, false]);
        assert_eq!(m.tiles.len(), 4);
        assert_eq!(m.tiles[0].detail, "Revenue up");
        assert_eq!(m.default_tab(), ReplyTab::Analysis);
        assert!(m.coverage.is_none());
    }

    #[test]
    fn markdown_and_plain_replies_are_handled() {
        let md = ReplyModel::build("```markdown\n| a | b |\n|---|---|\n| 1 | x |\n| 2 | y |\n```", &[], &[]);
        assert_eq!(md.tables.len(), 1);
        assert_eq!(md.tables[0].rows.len(), 2);
        assert!(md.value.is_none());
        let plain = ReplyModel::build("just words", &[], &[]);
        assert!(plain.tables.is_empty() && plain.value.is_none());
        assert_eq!(plain.default_tab(), ReplyTab::Structured);
    }

    #[test]
    fn filter_and_sort_pick_the_right_rows() {
        let t = table(
            &["name", "n"],
            &[&["Bravo", "10"], &["alpha", "2"], &["Charlie", ""], &["delta", "30"]],
        );
        assert_eq!(visible_rows(&t, None, ""), vec![0, 1, 2, 3]);
        assert_eq!(visible_rows(&t, None, " AL "), vec![1]);
        assert_eq!(visible_rows(&t, Some((1, false)), ""), vec![1, 0, 3, 2]);
        // Empty cells stay last when descending too
        assert_eq!(visible_rows(&t, Some((1, true)), ""), vec![3, 0, 1, 2]);
        assert_eq!(visible_rows(&t, Some((0, false)), ""), vec![1, 0, 2, 3]);
        assert_eq!(visible_rows(&t, Some((0, true)), "a"), vec![3, 2, 0, 1]);
        assert_eq!(visible_rows(&t, Some((9, true)), ""), vec![0, 1, 2, 3]);
    }

    #[test]
    fn clicking_a_header_cycles_ascending_descending_none() {
        let a = next_sort(None, 2);
        assert_eq!(a, Some((2, false)));
        let b = next_sort(a, 2);
        assert_eq!(b, Some((2, true)));
        assert_eq!(next_sort(b, 2), None);
        assert_eq!(next_sort(b, 1), Some((1, false)));
    }

    #[test]
    fn column_statistics_and_chart_groups() {
        let t = table(
            &["kind", "n"],
            &[&["a", "10"], &["b", "5"], &["a", "20"], &["", "1"], &["c", "x"]],
        );
        let all = visible_rows(&t, None, "");
        let st = column_stats(&t, 1, &all).unwrap();
        assert_eq!((st.count, st.min, st.max, st.sum), (4, 1.0, 20.0, 36.0));
        assert_eq!(st.mean(), 9.0);
        assert!(column_stats(&t, 0, &all).is_none());
        // Counted per group, largest first, blank names grouped under a dash
        assert_eq!(
            chart_points(&t, &all, 0, None),
            vec![
                ("a".into(), 2.0),
                ("b".into(), 1.0),
                ("—".into(), 1.0),
                ("c".into(), 1.0)
            ]
        );
        // Added up per group; the row whose number is text is skipped
        assert_eq!(
            chart_points(&t, &all, 0, Some(1)),
            vec![("a".into(), 30.0), ("b".into(), 5.0), ("—".into(), 1.0)]
        );
    }

    #[test]
    fn donut_groups_the_tail_into_others() {
        let points: Vec<(String, f64)> = (0..10).map(|i| (format!("g{i}"), (10 - i) as f64)).collect();
        let slices = donut_slices(&points);
        assert_eq!(slices.len(), MAX_SLICES + 1);
        assert_eq!(slices[MAX_SLICES].1, 3.0 + 2.0 + 1.0);
        assert!(donut_slices(&[("x".into(), -1.0), ("y".into(), 0.0)]).is_empty());
    }

    #[test]
    fn csv_export_quotes_and_follows_the_view() {
        let t = table(&["a", "b"], &[&["x, y", "1"], &["say \"hi\"", "2"], &["z", "3"]]);
        assert_eq!(to_csv(&t, &[1, 0], false), "a,b\n\"say \"\"hi\"\"\",2\n\"x, y\",1\n");
        assert!(to_csv(&t, &[], true).starts_with('\u{feff}'));
    }

    #[test]
    fn coverage_tells_columns_values_and_misses_apart() {
        let cols = s(&["sessions", "bounce_rate", "id"]);
        let outliers = s(&[
            "sessions: max 48210 (z=14.2)",
            "bounce_rate: max 0.97 (z=4.1)",
            "users: max 47350 (z=14.5)",
        ]);
        let reply = "On Sept 5 the pricing page had 48,210 sessions, and the bounce_rate was extreme.";
        let c = coverage(reply, &cols, &outliers);
        assert_eq!(
            c.columns,
            vec![
                ("sessions".into(), true),
                ("bounce_rate".into(), true),
                ("id".into(), false)
            ]
        );
        let m: Vec<Mention> = c.outliers.iter().map(|(_, m)| *m).collect();
        // sessions: name and value; bounce_rate: name only; users: neither
        assert_eq!(m, vec![Mention::Value, Mention::Column, Mention::No]);
        // A number inside a longer number does not count
        assert!(!has_number("the value 190000", "90000"));
        assert!(has_number("the value 90000.5", "90000"));
    }

    #[test]
    fn building_a_model_with_data_adds_coverage() {
        let m = ReplyModel::build(
            r#"{"a": "amount spiked"}"#,
            &s(&["amount", "region"]),
            &s(&["amount: max 90000 (z=5.7)"]),
        );
        let cov = m.coverage.unwrap();
        assert_eq!(cov.columns[0], ("amount".into(), true));
        assert_eq!(cov.outliers[0].1, Mention::Column);
    }

    #[test]
    fn analysis_state_picks_sensible_defaults() {
        let t = table(&["n", "name", "m"], &[&["1", "a", "2"], &["3", "b", "4"]]);
        let st = AnalysisState::for_table(0, &t);
        assert_eq!((st.x, st.y), (1, Some(0)));
        let only_text = table(&["a", "b"], &[&["x", "y"]]);
        assert_eq!(AnalysisState::for_table(0, &only_text).y, None);
    }

    #[test]
    fn huge_replies_stay_bounded() {
        let items: Vec<Value> = (0..5000)
            .map(|i| serde_json::json!({"id": i, "v": i * 2, "a":1,"b":2,"c":3,"d":4,"e":5,"f":6,"g":7,"h":8,"i":9,"j":10,"k":11,"l":12,"m":13}))
            .collect();
        let m = ReplyModel::build(&Value::Array(items).to_string(), &[], &[]);
        assert_eq!(m.tables.len(), 1);
        assert_eq!(m.tables[0].rows.len(), MAX_TABLE_ROWS);
        assert_eq!(m.tables[0].header.len(), MAX_COLS);
    }

    #[test]
    fn the_analysis_and_structure_views_draw_without_panicking() {
        let ctx = egui::Context::default();
        let p = crate::theme::THEMES[3];
        let replies = [
            r#"{"headline":"h","outliers":[{"row":1,"col":"x","z":4.2},{"row":2,"col":"y","z":3.1}],"k":{"a":1,"b":2}}"#,
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |",
            "plain text",
        ];
        for reply in replies {
            let model = ReplyModel::build(reply, &s(&["row", "col"]), &s(&["row: max 9 (z=4.0)"]));
            let mut st = AnalysisState::for_model(Some(&model));
            for kind in [ChartKind::Bars, ChartKind::Donut] {
                st.chart = kind;
                st.sort = Some((0, true));
                st.filter = "1".into();
                for _ in 0..2 {
                    let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                        analysis_ui(ui, &p, &model, &mut st);
                        structured_view(ui, &p, &model);
                    });
                    out.drop_without_applying_deltas();
                }
            }
        }
    }

    #[test]
    fn structured_view_reports_what_it_cannot_draw() {
        let ctx = egui::Context::default();
        let p = crate::theme::THEMES[3];
        for (reply, drawn) in [
            (r#"{"a":[1,2]}"#, true),
            ("| a |\n|---|\n| 1 |", true),
            ("words", false),
        ] {
            let model = ReplyModel::build(reply, &[], &[]);
            let mut got = None;
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                got = Some(structured_view(ui, &p, &model))
            });
            out.drop_without_applying_deltas();
            assert_eq!(got, Some(drawn), "{reply}");
        }
    }

    /// Every sample reply passes the format check of the sample blueprint it belongs to and gives the analysis
    /// tools a table to work with.
    #[test]
    fn sample_replies_match_their_blueprints() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("samples");
        let replies = std::fs::read_dir(dir.join("replies")).map(|d| d.count()).unwrap_or(0);
        assert!(replies >= 5, "samples/replies should hold the sample replies");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir.join("replies")).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some((stem, _)) = name.split_once(".reply.") else {
                continue;
            };
            let bp = Blueprint::load(&dir.join(format!("{stem}.kiro"))).unwrap_or_else(|e| panic!("{stem}.kiro: {e}"));
            let text = pipeline::read_text_auto(&path).unwrap();
            let fields = pipeline::parse_fields(&bp.fields);
            let check = pipeline::check_output(bp.format, &fields, &text);
            assert!(check.ok, "{name}: {check:?}");
            assert_eq!(
                bp.format == OutputFormat::MarkdownTable,
                name.ends_with(".md"),
                "{name}: the extension follows the output format"
            );
            let model = ReplyModel::build(&check.pretty, &[], &[]);
            assert!(!model.tables.is_empty(), "{name} has no table for the analysis tools");
            assert!(
                model.tables.iter().any(|t| t.numeric.contains(&true)),
                "{name} has no number column"
            );
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} sample replies were checked");
    }
}
