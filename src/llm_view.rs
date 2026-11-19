//! Visual parts of the VIII LLM run result: required-field chips, token usage bars, a structured view of the reply
//! (JSON tree and tables, Markdown tables) and a chart and table of recent runs.

use crate::canvas::graph::Provider;
use crate::canvas::llm::Usage;
use crate::theme::{self, Palette};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke};
use serde_json::Value;

/// How the reply body is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplyTab {
    /// JSON as a tree with tables, a Markdown table as a grid
    #[default]
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
            egui::Frame::new()
                .fill(color.gamma_multiply(0.15))
                .stroke(Stroke::new(1.0, color.gamma_multiply(0.7)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(7, 2))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("{mark} {name}")).small().color(color));
                })
                .response
                .on_hover_text(tip);
        }
    });
}

/// Share of the output limit used, 0..=1 (0 when there is no limit).
pub fn output_fraction(output: u64, max_tokens: u32) -> f32 {
    if max_tokens == 0 {
        return 0.0;
    }
    (output as f32 / max_tokens as f32).clamp(0.0, 1.0)
}

/// One labelled horizontal bar: `frac` of the width is filled.
fn bar(ui: &mut egui::Ui, p: &Palette, label: &str, frac: f32, fill: Color32, text: String) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [64.0, 16.0],
            egui::Label::new(RichText::new(label).small().color(p.sub)),
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
        usage.input as f32 / scale,
        p.accent.gamma_multiply(0.8),
        trf!("{n} tokens (estimate {est})", n = usage.input, est = prompt_estimate),
    );
    bar(
        ui,
        p,
        tr!("Output"),
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

// ---------- structured reply ----------

/// A Markdown table in `text` as (header, rows). The first `|` line is the header, separator lines are skipped.
pub fn parse_markdown_table(text: &str) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let cells = |line: &str| -> Vec<String> {
        let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
        inner.split('|').map(|c| c.trim().to_string()).collect()
    };
    let is_sep = |line: &str| line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'));
    let mut lines = text.lines().map(str::trim).filter(|l| l.starts_with('|'));
    let header = cells(lines.next()?);
    let rows: Vec<Vec<String>> = lines.filter(|l| !is_sep(l)).map(cells).collect();
    (header.len() > 1 || !rows.is_empty()).then_some((header, rows))
}

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
    for it in items {
        if let Value::Object(o) = it {
            for k in o.keys() {
                if !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    cols.truncate(max);
    cols
}

const MAX_COLS: usize = 8;
const MAX_ROWS: usize = 100;

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
                for row in rows.iter().take(MAX_ROWS) {
                    for i in 0..header.len() {
                        let cell = row.get(i).map(String::as_str).unwrap_or("");
                        ui.add(egui::Label::new(RichText::new(truncate(cell, 60)).small()).selectable(true))
                            .on_hover_text(cell);
                    }
                    ui.end_row();
                }
            });
    });
    if rows.len() > MAX_ROWS {
        ui.label(
            RichText::new(trf!("… {n} more rows", n = rows.len() - MAX_ROWS))
                .small()
                .color(p.sub),
        );
    }
}

fn value_ui(ui: &mut egui::Ui, p: &Palette, id: egui::Id, v: &Value, depth: usize) {
    match v {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let cols = object_columns(items, MAX_COLS);
            let rows: Vec<Vec<String>> = items
                .iter()
                .map(|it| {
                    cols.iter()
                        .map(|c| it.get(c).map_or(String::new(), |x| value_summary(x, 200)))
                        .collect()
                })
                .collect();
            grid(ui, p, id, &cols, &rows);
        }
        Value::Array(items) => {
            for (i, it) in items.iter().enumerate().take(MAX_ROWS) {
                if depth < 3 && (it.is_object() || it.is_array()) {
                    egui::CollapsingHeader::new(format!("[{i}] {}", value_summary(it, 40)))
                        .id_salt(id.with(i))
                        .show(ui, |ui| value_ui(ui, p, id.with(i), it, depth + 1));
                } else {
                    ui.add(egui::Label::new(RichText::new(format!("• {}", value_summary(it, 300)))).selectable(true));
                }
            }
        }
        Value::Object(o) => {
            for (k, val) in o {
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
        }
        other => {
            ui.add(egui::Label::new(value_summary(other, 2000)).selectable(true));
        }
    }
}

/// Draws the reply as a JSON tree or a table. Returns false when the text is neither JSON nor a Markdown table.
pub fn structured_view(ui: &mut egui::Ui, p: &Palette, text: &str) -> bool {
    let id = egui::Id::unique("llm-structured");
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        value_ui(ui, p, id, &v, 0);
        return true;
    }
    if let Some((header, rows)) = parse_markdown_table(text) {
        ui.label(RichText::new(trf!("{n} rows", n = rows.len())).small().color(p.sub));
        grid(ui, p, id.with("md"), &header, &rows);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
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
    fn markdown_table_is_parsed() {
        let t = "Intro\n| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\nend";
        let (h, rows) = parse_markdown_table(t).unwrap();
        assert_eq!(h, s(&["a", "b"]));
        assert_eq!(rows, vec![s(&["1", "2"]), s(&["3", "4"])]);
        assert!(parse_markdown_table("no table here").is_none());
    }

    #[test]
    fn object_columns_keep_first_seen_order() {
        let items: Vec<Value> = serde_json::from_str(r#"[{"b":1,"a":2},{"c":3,"a":4}]"#).unwrap();
        assert_eq!(object_columns(&items, 8), s(&["b", "a", "c"]));
        assert_eq!(object_columns(&items, 2), s(&["b", "a"]));
    }

    #[test]
    fn summaries_are_short_and_single_line() {
        assert_eq!(value_summary(&Value::String("x\ny".into()), 10), "x y");
        assert_eq!(value_summary(&Value::String("abcdef".into()), 4), "abc…");
        assert_eq!(value_summary(&serde_json::json!([1, 2]), 10), "2 items");
    }

    #[test]
    fn structured_view_draws_json_and_tables() {
        let ctx = egui::Context::default();
        let p = crate::theme::THEMES[3];
        for (text, structured) in [
            (
                r#"{"root_causes":["a","b"],"outliers":[{"row":1,"col":"x"}],"recommendations":"r"}"#,
                true,
            ),
            ("| a | b |\n|---|---|\n| 1 | 2 |", true),
            ("plain text", false),
        ] {
            let mut got = None;
            let out = ctx.run_ui(egui::RawInput::default(), |ui| {
                got = Some(structured_view(ui, &p, text));
            });
            out.drop_without_applying_deltas();
            assert_eq!(got, Some(structured), "{text}");
        }
    }
}
