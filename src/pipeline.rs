//! The compile pipeline shared by the GUI, CLI and MCP: build the AST → lint → serialize → count tokens.
//! Lint diagnostics and check messages follow the UI language; the compiled prompt and the token counts are the same
//! in every UI language.

use crate::ast::{
    ConstraintNode, DataSubAst, OutputFormat, OutputSchemaAst, RoleNode, RootPromptAst, TargetModel, codegen, linter,
};
use crate::canvas::llm::{Finish, LlmReply, Usage};
use crate::{profiler::csv_profiler, tokenizer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

pub const DEFAULT_FIELDS: &str = "root_causes,outliers,recommendations";

#[derive(Clone, Debug)]
pub struct LoadedData {
    pub path: std::path::PathBuf,
    pub data: DataSubAst,
    pub raw_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct CompileRequest {
    pub target: TargetModel,
    pub role: String,
    pub constraints: Vec<String>,
    pub format: OutputFormat,
    pub fields: Vec<String>,
    pub data: Option<LoadedData>,
}

#[derive(Clone, Debug)]
pub struct CompileResult {
    pub ast: RootPromptAst,
    pub output: String,
    pub orig_tokens: usize,
    pub comp_tokens: usize,
    pub saving_pct: f64,
    pub saved_usd: f64,
    pub preview: String,
}

pub fn load_data(path: &Path, sample_rows: usize) -> Result<LoadedData, String> {
    let data = csv_profiler::profile_file(path, sample_rows)?;
    let (raw_tokens, _) = raw_file_tokens(path)?;
    Ok(LoadedData {
        path: path.to_path_buf(),
        data,
        raw_tokens,
    })
}

/// Key of the raw file token estimate cache: (canonical path, file length, modification time).
type RawTokenKey = (PathBuf, u64, SystemTime);
static RAW_TOKENS: LazyLock<Mutex<HashMap<RawTokenKey, usize>>> = LazyLock::new(Default::default);
/// Maximum number of cache entries. Past it, the cache is cleared and filled again.
const RAW_TOKENS_MAX: usize = 64;

/// Token estimate of the raw file. The same file (same path, length and modification time) is not tokenized again
/// (when it is profiled again with only a different sample row count). The second value tells whether the cache was
/// hit.
fn raw_file_tokens(path: &Path) -> Result<(usize, bool), String> {
    let key = std::fs::metadata(path).ok().and_then(|m| {
        let modified = m.modified().ok()?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Some((canonical, m.len(), modified))
    });
    let Some(key) = key else {
        // Without a modification time a changed file cannot be told apart, so nothing is cached.
        return tokenizer::estimate_file_tokens(path).map(|n| (n, false));
    };
    let cache = || RAW_TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&n) = cache().get(&key) {
        return Ok((n, true));
    }
    // The lock is not held while tokenizing (so other loads are not blocked).
    let n = tokenizer::estimate_file_tokens(path)?;
    let mut c = cache();
    if c.len() >= RAW_TOKENS_MAX {
        c.clear();
    }
    c.insert(key, n);
    Ok((n, false))
}

/// Reads a text file and detects its encoding (`csv_profiler::detect_text_encoding`, the same as the profiler).
/// A BOM (UTF-8/UTF-16) is followed; otherwise the file is UTF-8 if it decodes as UTF-8 (UTF-8 with a few broken
/// bytes mixed in still counts as UTF-8), else it is read as CP949 (EUC-KR). This keeps Korean documents saved as
/// ANSI (CP949) in Notepad, such as steering files, from being garbled.
pub fn read_text_auto(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| trf!("cannot read file: {}", csv_profiler::io_error_text(&e)))?;
    Ok(decode_text_auto(&bytes))
}

fn decode_text_auto(bytes: &[u8]) -> String {
    let enc = csv_profiler::detect_text_encoding(bytes, true);
    enc.decode_with_bom_removal(bytes).0.into_owned()
}

pub fn parse_fields(s: &str) -> Vec<String> {
    s.split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Reads the `- ` and `* ` bullet lines of a steering markdown file as constraint sentences.
/// Lines inside code fences (``` or ~~~) are example code and are skipped (so `- rm -rf build` in a yaml block does
/// not become a constraint).
pub fn parse_steering_rules(md: &str) -> Vec<String> {
    let mut fence: Option<(char, usize)> = None;
    let mut rules = Vec::new();
    for line in md.trim_start_matches('\u{feff}').lines() {
        let l = line.trim_start();
        if let Some((ch, len)) = fence {
            if closes_fence(l, ch, len) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opens_fence(l) {
            fence = Some(open);
            continue;
        }
        if let Some(r) = l
            .strip_prefix("- ")
            .or_else(|| l.strip_prefix("* "))
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            rules.push(r.to_string());
        }
    }
    rules
}

/// (fence character, length) when the line opens a code fence. The info string of a backtick fence cannot contain
/// backticks, so inline code such as ```` ```one-line code``` ```` is not a fence.
fn opens_fence(l: &str) -> Option<(char, usize)> {
    let ch = l.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = l.chars().take_while(|c| *c == ch).count();
    (len >= 3 && !(ch == '`' && l[len..].contains('`'))).then_some((ch, len))
}

/// A closing line uses the same character as the opening fence, is at least as long, and has only whitespace after
/// it.
fn closes_fence(l: &str, ch: char, len: usize) -> bool {
    let run = l.chars().take_while(|c| *c == ch).count();
    run >= len && l[run..].trim().is_empty()
}

pub fn compile(req: &CompileRequest) -> CompileResult {
    let mut ast = RootPromptAst {
        target_model: req.target,
        role: RoleNode {
            name: "Analyst".into(),
            instruction: req.role.clone(),
        },
        data_sub_ast: req.data.as_ref().map(|d| d.data.clone()),
        constraints: req
            .constraints
            .iter()
            .enumerate()
            .map(|(i, c)| ConstraintNode::classify(format!("C{}", i + 1), c))
            .collect(),
        output_schema: OutputSchemaAst::new(req.format, req.fields.clone()),
        lint_warnings: vec![],
    };

    // Baseline before compression: the original instructions + the whole raw data + the schema description
    let baseline_text = format!(
        "{}\n{}\n{}",
        req.role,
        req.constraints.join("\n"),
        OutputSchemaAst::render_schema(req.format, &req.fields)
    );
    let orig_tokens = tokenizer::count(&baseline_text) + req.data.as_ref().map_or(0, |d| d.raw_tokens);

    linter::lint(&mut ast);
    ast.constraints.sort_by_key(|c| c.priority); // Stable sort: equal priorities keep the input order
    for (i, c) in ast.constraints.iter_mut().enumerate() {
        c.id = format!("C{}", i + 1); // Renumber the IDs consecutively after removal and sorting
    }
    let output = codegen::generate(&ast);
    let comp_tokens = tokenizer::count(&output);

    let saving_pct = if orig_tokens == 0 {
        0.0
    } else {
        (1.0 - comp_tokens as f64 / orig_tokens as f64) * 100.0
    };
    let price = req.target.usd_per_million_input();
    let saved_usd = tokenizer::cost_usd(orig_tokens, price) - tokenizer::cost_usd(comp_tokens, price);
    let preview = mock_preview(&ast);
    CompileResult {
        ast,
        output,
        orig_tokens,
        comp_tokens,
        saving_pct,
        saved_usd,
        preview,
    }
}

/// Local mock reply that calls no LLM: shows only the output schema skeleton.
/// The table header is escaped by the same rule as the prompt (codegen::md_cell), so it passes its own check
/// (check_output).
fn mock_preview(ast: &RootPromptAst) -> String {
    let fields = &ast.output_schema.required_fields;
    match ast.output_schema.format {
        OutputFormat::StrictJson => {
            let map: serde_json::Map<String, serde_json::Value> =
                fields.iter().map(|f| (f.clone(), serde_json::Value::Null)).collect();
            serde_json::to_string_pretty(&map).unwrap_or_default()
        }
        OutputFormat::MarkdownTable => {
            let header: Vec<String> = fields.iter().map(|f| codegen::md_cell(f)).collect();
            format!("| {} |\n|{}|", header.join(" | "), vec!["---"; fields.len()].join("|"))
        }
    }
}

/// Token counts and estimated cost of compiling the same input for one target.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetCost {
    pub target: TargetModel,
    pub orig_tokens: usize,
    pub comp_tokens: usize,
    pub saving_pct: f64,
    /// Estimated input cost of sending the compressed prompt once (USD)
    pub cost_usd: f64,
    pub saved_usd: f64,
}

/// Compiles the same request for Claude, GPT and Gemini to compare tokens and cost (called on a worker thread).
pub fn compare_targets(req: &CompileRequest) -> Vec<TargetCost> {
    TargetModel::ALL
        .iter()
        .map(|&target| {
            let r = compile(&CompileRequest { target, ..req.clone() });
            TargetCost {
                target,
                orig_tokens: r.orig_tokens,
                comp_tokens: r.comp_tokens,
                saving_pct: r.saving_pct,
                cost_usd: tokenizer::cost_usd(r.comp_tokens, target.usd_per_million_input()),
                saved_usd: r.saved_usd,
            }
        })
        .collect()
}

/// Looks for the steering file, walking up from the working folder and from the executable's folder.
pub fn find_steering() -> Option<std::path::PathBuf> {
    const REL: &str = ".kiro/steering/compiler-rules.md";
    let starts = [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf())),
    ];
    starts
        .into_iter()
        .flatten()
        .find_map(|s| s.ancestors().map(|a| a.join(REL)).find(|p| p.is_file()))
}

/// Result of checking a real LLM reply against the output schema.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutputCheck {
    /// Body for display (pretty-printed when it is JSON)
    pub pretty: String,
    pub ok: bool,
    pub missing: Vec<String>,
    /// Required fields whose key is present but whose value is null or an empty string (for warnings; does not affect
    /// ok)
    pub nulls: Vec<String>,
    pub error: Option<String>,
    /// The stop reason exactly as the provider reported it (stop_reason / finish_reason / finishReason)
    pub stop_reason: Option<String>,
    /// Why the reply was not generated to the end (token limit, refusal or filter). When set, the reply does not pass
    /// even if its format is right
    pub cut_off: Option<String>,
    /// Token usage reported by the provider (only check_reply fills it)
    pub usage: Option<Usage>,
    /// For the table format, the number of body rows of the table found (header and separator rows excluded). 0 is
    /// for a warning and does not affect ok
    pub table_rows: Option<usize>,
}

impl OutputCheck {
    /// Whether the reply was cut off by the output token limit (a case that a rerun with a higher limit may fix).
    pub fn hit_max_tokens(&self) -> bool {
        let reply = LlmReply {
            stop_reason: self.stop_reason.clone(),
            ..LlmReply::default()
        };
        self.cut_off.is_some() && reply.finish() == Finish::MaxTokens
    }
}

/// Checks an LLM reply. A reply cut off by the output token limit or similar is reported as cut off, not as a format
/// violation, and does not pass.
pub fn check_reply(format: OutputFormat, fields: &[String], reply: &LlmReply, max_tokens: u32) -> OutputCheck {
    let mut c = check_output(format, fields, &reply.text);
    let reason = reply.stop_reason.as_deref().unwrap_or_default();
    c.cut_off = match reply.finish() {
        Finish::MaxTokens => Some(trf!(
            "Reached the output token limit ({max_tokens}); the response was cut off ({reason})",
            max_tokens = max_tokens,
            reason = reason
        )),
        Finish::ContextWindow => Some(trf!(
            "The model's context window is full; the response was cut off ({reason})",
            reason = reason
        )),
        Finish::Stopped => Some(trf!(
            "The response did not finish (stop reason: {reason})",
            reason = reason
        )),
        Finish::Complete | Finish::Unknown => None,
    };
    c.ok &= c.cut_off.is_none();
    c.stop_reason = reply.stop_reason.clone();
    c.usage = reply.usage;
    c
}

/// Checks whether an LLM reply kept the enforced output format and the required fields.
/// JSON is parsed after stripping a ```json code fence; for tables, the column names of the header row are checked.
pub fn check_output(format: OutputFormat, fields: &[String], text: &str) -> OutputCheck {
    let body = strip_fence(text.trim());
    match format {
        OutputFormat::StrictJson => match serde_json::from_str::<serde_json::Value>(body) {
            Ok(serde_json::Value::Object(map)) => {
                let missing: Vec<String> = fields.iter().filter(|f| !map.contains_key(*f)).cloned().collect();
                let nulls: Vec<String> = fields
                    .iter()
                    .filter(|f| {
                        map.get(*f)
                            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
                    })
                    .cloned()
                    .collect();
                OutputCheck {
                    pretty: serde_json::to_string_pretty(&map).unwrap_or_else(|_| body.to_string()),
                    ok: missing.is_empty(),
                    missing,
                    nulls,
                    ..OutputCheck::default()
                }
            }
            Ok(_) => OutputCheck {
                pretty: body.to_string(),
                ok: false,
                missing: fields.to_vec(),
                error: Some(tr!("Not a JSON object").into()),
                ..OutputCheck::default()
            },
            Err(e) => OutputCheck {
                pretty: text.to_string(),
                ok: false,
                missing: fields.to_vec(),
                error: Some(trf!("JSON parse error: {e}", e = e)),
                ..OutputCheck::default()
            },
        },
        OutputFormat::MarkdownTable => {
            let table = find_table(body);
            let header = table.as_ref().map(|t| t.header.as_slice()).unwrap_or_default();
            let missing: Vec<String> = fields.iter().filter(|f| !header_has(header, f)).cloned().collect();
            OutputCheck {
                pretty: body.to_string(),
                ok: table.is_some() && missing.is_empty(),
                error: table.is_none().then(|| tr!("No Markdown table found").to_string()),
                missing,
                table_rows: table.map(|t| t.rows),
                ..OutputCheck::default()
            }
        }
    }
}

/// Whether a header cell equals the field name. Case-sensitive; emphasis and code marks around the cell are also
/// stripped for the comparison. Line breaks in the field name count as spaces, as in the prompt's table header
/// (codegen::md_cell).
fn header_has(header: &[String], field: &str) -> bool {
    let want = field.replace(['\n', '\r'], " ");
    let want = want.trim();
    header
        .iter()
        .any(|c| c == field || c == want || strip_emphasis(c) == want)
}

/// Strips Markdown emphasis and code marks around the whole cell (`**x**`, `__x__`, `*x*`, `_x_`, `` `x` ``).
/// Nested marks such as `***x***` or `` **`x`** `` are stripped too; underscores inside (root_causes) stay.
fn strip_emphasis(cell: &str) -> &str {
    let mut s = cell.trim();
    'outer: loop {
        for m in ["**", "__", "`", "*", "_"] {
            if s.len() > 2 * m.len() && s.starts_with(m) && s.ends_with(m) {
                s = s[m.len()..s.len() - m.len()].trim();
                continue 'outer;
            }
        }
        return s;
    }
}

/// A Markdown table found in a reply.
struct Table {
    header: Vec<String>,
    /// Number of body rows (header and separator rows excluded)
    rows: usize,
}

/// The header cells and the number of body rows of a Markdown table. Like GFM it looks for the separator row
/// (`---|:--:`) right under the header row, so tables without leading and trailing pipes are recognized too. Without a
/// separator row, the first line starting with `|` is the header, as before.
/// The body runs below the header until a blank line or a line without `|`.
fn find_table(body: &str) -> Option<Table> {
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let gfm = (0..lines.len().saturating_sub(1)).find_map(|i| {
        let width = delimiter_width(lines[i + 1])?;
        let cells = table_cells(lines[i]);
        (!lines[i].is_empty() && cells.len() == width).then(|| Table {
            header: cells,
            rows: lines[i + 2..]
                .iter()
                .take_while(|l| !l.is_empty() && l.contains('|'))
                .count(),
        })
    });
    gfm.or_else(|| {
        let i = lines.iter().position(|l| l.starts_with('|'))?;
        Some(Table {
            header: table_cells(lines[i]),
            rows: lines[i + 1..]
                .iter()
                .take_while(|l| l.starts_with('|'))
                .filter(|l| delimiter_width(l).is_none())
                .count(),
        })
    })
}

/// The column count when the line is a separator row: every cell has at least one `-`, and `:` is allowed only at the
/// ends. `---` without a pipe is a horizontal rule (or a heading underline), not a table.
fn delimiter_width(line: &str) -> Option<usize> {
    if !line.contains('|') {
        return None;
    }
    let cells = table_cells(line);
    cells
        .iter()
        .all(|c| {
            let dashes = c.strip_prefix(':').unwrap_or(c);
            let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
            !dashes.is_empty() && dashes.chars().all(|ch| ch == '-')
        })
        .then_some(cells.len())
}

/// Splits a table line into cells. Leading and trailing pipes are optional, and `\|` is a `|` character inside a cell.
fn table_cells(line: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut chars = line.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("there is always a current cell").push('|');
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().expect("there is always a current cell").push(ch),
        }
    }
    // Drop the empty cells made by the opening and closing pipes. Spaces inside cells are kept until this point, which
    // tells those apart from empty columns such as `| |`.
    if cells.len() > 1 && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

/// Strips a ```json ... ``` code fence. Handles both multi-line and one-line fences (```{"a":1}```,
/// ```json {"a":1}```).
fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let rest = rest.trim_end();
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    let starts_body = |t: &str| t.trim_start().starts_with(['{', '[', '|']);
    let body = match rest.split_once('\n') {
        // Drop the first line when it is a language tag (json, markdown etc.)
        Some((first, after)) if !starts_body(first) => after,
        _ if starts_body(rest) => rest,
        // One line: strip only the leading language tag word
        _ => rest
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_alphanumeric()),
    };
    body.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn json_with_fence_and_missing_field() {
        let c = check_output(OutputFormat::StrictJson, &f(&["a", "b"]), "```json\n{\"a\": 1}\n```");
        assert!(!c.ok);
        assert_eq!(c.missing, f(&["b"]));
        assert!(c.error.is_none());
        assert!(c.pretty.contains("\"a\": 1"));
    }

    #[test]
    fn json_ok_and_invalid() {
        assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), r#"{"a":null,"x":2}"#).ok);
        let bad = check_output(
            OutputFormat::StrictJson,
            &f(&["a"]),
            ko_fixture!("core.pipeline.prose_reply"),
        );
        assert!(!bad.ok && bad.error.is_some());
        let arr = check_output(OutputFormat::StrictJson, &f(&["a"]), "[1,2]");
        assert!(!arr.ok && arr.error.is_some());
    }

    #[test]
    fn single_line_fences_and_nulls() {
        for t in [
            "```{\"a\":1}```",
            "```json {\"a\":1}```",
            "```json\n{\"a\":1}\n```",
            "```\n{\"a\":1}```",
        ] {
            assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), t).ok, "{t}");
        }
        let multi = "```\n{\n\"a\": 1\n}\n```";
        assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), multi).ok);
        let c = check_output(OutputFormat::StrictJson, &f(&["a", "b"]), r#"{"a":null,"b":" "}"#);
        assert!(c.ok);
        assert_eq!(c.nulls, f(&["a", "b"]));
    }

    #[test]
    fn markdown_table_header() {
        let t = &format!(
            "{}\n| a | b |\n|---|---|\n| 1 | 2 |",
            ko_fixture!("core.pipeline.explanation")
        );
        assert!(check_output(OutputFormat::MarkdownTable, &f(&["a", "b"]), t).ok);
        let c = check_output(OutputFormat::MarkdownTable, &f(&["a", "c"]), t);
        assert_eq!(c.missing, f(&["c"]));
        assert!(
            !check_output(
                OutputFormat::MarkdownTable,
                &f(&["a"]),
                ko_fixture!("core.pipeline.no_table")
            )
            .ok
        );
    }

    #[test]
    fn markdown_table_without_outer_pipes() {
        let fields = f(&["root_causes", "outliers"]);
        for t in ko_fixture!("core.pipeline.tables_without_outer_pipes") {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(c.ok, "{t:?} -> {c:?}");
        }
        // A `\|` inside a header cell is not a column separator
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["a|b", "c"]),
            "a\\|b | c\n--- | ---\n1 | 2",
        );
        assert!(c.ok, "{c:?}");
    }

    #[test]
    fn markdown_table_rejects_non_tables() {
        let fields = f(&["a", "b"]);
        let setext = format!("a b\n---\n{}", ko_fixture!("core.pipeline.body"));
        for t in [
            setext.as_str(),               // setext heading underline
            "a | b",                       // neither a separator row nor a leading pipe
            "a | b | c\n--- | ---\n1 | 2", // the header and the separator row have different column counts
            "a | b\n--- | x\n1 | 2",       // a character other than `-` in the separator row
            "a | b\n--- |   \n1 | 2",      // an empty separator cell
        ] {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(!c.ok, "{t:?}");
            assert_eq!(c.error.as_deref(), Some("No Markdown table found"), "{t:?}");
        }
        // A `|` row without a separator row is the header, as before
        assert!(check_output(OutputFormat::MarkdownTable, &fields, "| a | b |\n| 1 | 2 |").ok);
        // A table found by its separator row wins over an earlier `|` line
        let t = &format!("| {} |\n\na | b\n--- | ---\n1 | 2", ko_fixture!("core.col.memo"));
        assert!(check_output(OutputFormat::MarkdownTable, &fields, t).ok);
    }

    #[test]
    fn table_cells_split() {
        assert_eq!(table_cells("| a | b |"), f(&["a", "b"]));
        assert_eq!(table_cells("a | b"), f(&["a", "b"]));
        assert_eq!(table_cells("a | |"), f(&["a", ""]));
        assert_eq!(table_cells(r"a \| b | c\|"), f(&["a | b", "c|"]));
        assert_eq!(table_cells(r"a\b"), f(&[r"a\b"]));
        assert_eq!(delimiter_width("|:--|--:|:-:|"), Some(3));
        assert_eq!(delimiter_width("---"), None);
        assert_eq!(delimiter_width("|::|"), None);
    }

    fn reply(text: &str, stop: Option<&str>) -> LlmReply {
        LlmReply {
            text: text.into(),
            stop_reason: stop.map(String::from),
            usage: None,
        }
    }

    #[test]
    fn truncated_reply_is_reported_as_cut_off_not_format_error() {
        let fields = f(&["a", "b"]);
        let c = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(
                &format!("{{\"a\": \"{}", ko_fixture!("core.pipeline.long_explanation")),
                Some("max_tokens"),
            ),
            2048,
        );
        assert!(!c.ok);
        assert_eq!(
            c.cut_off.as_deref(),
            Some("Reached the output token limit (2048); the response was cut off (max_tokens)")
        );
        assert_eq!(c.stop_reason.as_deref(), Some("max_tokens"));
        assert!(c.error.is_some_and(|e| e.starts_with("JSON parse error")));

        // A table passes the format check when the header is right, but a cut-off reply does not pass
        let table = &format!(
            "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | {}",
            ko_fixture!("core.pipeline.unfinished")
        );
        assert!(check_output(OutputFormat::MarkdownTable, &fields, table).ok);
        let c = check_reply(OutputFormat::MarkdownTable, &fields, &reply(table, Some("length")), 512);
        assert!(!c.ok && c.missing.is_empty() && c.error.is_none());
        assert!(c.cut_off.is_some_and(|m| m.contains("(512)") && m.contains("length")));

        // A Gemini reply whose thinking tokens used up the limit, leaving the body empty
        let c = check_reply(OutputFormat::StrictJson, &fields, &reply("", Some("MAX_TOKENS")), 2048);
        assert!(!c.ok && c.cut_off.is_some());
    }

    #[test]
    fn stop_reason_is_kept_for_complete_and_other_replies() {
        let fields = f(&["a"]);
        let ok = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("end_turn")),
            2048,
        );
        assert!(ok.ok && ok.cut_off.is_none());
        assert_eq!(ok.stop_reason.as_deref(), Some("end_turn"));
        let unknown = check_reply(OutputFormat::StrictJson, &fields, &reply(r#"{"a":1}"#, None), 2048);
        assert_eq!(unknown, check_output(OutputFormat::StrictJson, &fields, r#"{"a":1}"#));

        let ctx = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("model_context_window_exceeded")),
            2048,
        );
        assert!(!ctx.ok && ctx.cut_off.is_some_and(|m| m.contains("context window")));
        let filtered = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply("", Some("content_filter")),
            2048,
        );
        assert!(!filtered.ok);
        assert_eq!(
            filtered.cut_off.as_deref(),
            Some("The response did not finish (stop reason: content_filter)")
        );
    }

    #[test]
    fn usage_and_limit_hit_are_carried() {
        let fields = f(&["a"]);
        let mut r = reply(
            &format!("{{\"a\": \"{}", ko_fixture!("core.pipeline.cut_syllable")),
            Some("max_tokens"),
        );
        r.usage = Some(Usage {
            input: 812,
            output: 2048,
        });
        let c = check_reply(OutputFormat::StrictJson, &fields, &r, 2048);
        assert_eq!(
            c.usage,
            Some(Usage {
                input: 812,
                output: 2048
            })
        );
        assert!(c.hit_max_tokens());
        // For a reply stopped by a filter or a normal reply, raising the limit does not help
        let filtered = check_reply(OutputFormat::StrictJson, &fields, &reply("", Some("SAFETY")), 2048);
        assert!(filtered.cut_off.is_some() && !filtered.hit_max_tokens());
        let ok = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("stop")),
            2048,
        );
        assert!(!ok.hit_max_tokens());
    }

    #[test]
    fn table_header_cells_ignore_emphasis_and_code_marks() {
        let fields = f(&["root_causes", "outliers", "next_steps"]);
        for t in [
            "| **root_causes** | `outliers` | __next_steps__ |\n|---|---|---|\n| a | b | c |",
            "| *root_causes* | _outliers_ | ***next_steps*** |\n|---|---|---|\n| a | b | c |",
            "| ** root_causes ** | **`outliers`** | ``next_steps`` |\n|---|---|---|\n| a | b | c |",
        ] {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(c.ok, "{t:?} -> {c:?}");
            assert_eq!(c.table_rows, Some(1));
        }
        // Case-sensitive, and underscores inside are not stripped
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["root_causes"]),
            "| **Root_Causes** |\n|---|\n| a |",
        );
        assert_eq!(c.missing, f(&["root_causes"]));
        // When the field name itself is wrapped in underscores, it also matches as is
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["__init__"]),
            "| __init__ |\n|---|\n| 1 |",
        );
        assert!(c.ok, "{c:?}");
        assert_eq!(strip_emphasis("**a_b**"), "a_b");
        assert_eq!(strip_emphasis("*"), "*");
        assert_eq!(strip_emphasis("**"), "**");
        assert_eq!(strip_emphasis("_a"), "_a");
    }

    #[test]
    fn table_body_rows_are_counted() {
        let fields = f(&["a", "b"]);
        let rows = |t: &str| check_output(OutputFormat::MarkdownTable, &fields, t).table_rows;
        assert_eq!(rows("| a | b |\n|---|---|"), Some(0));
        assert_eq!(
            rows(&format!(
                "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n{}",
                ko_fixture!("core.pipeline.paragraph_row")
            )),
            Some(2)
        );
        assert_eq!(
            rows(&format!("a | b\n--- | ---\n1 | 2\n{}", ko_fixture!("core.col.memo"))),
            Some(1)
        );
        assert_eq!(rows("| a | b |\n| 1 | 2 |\n| 3 | 4 |"), Some(2)); // the old format without a separator row
        assert_eq!(rows(ko_fixture!("core.pipeline.no_table")), None);
        // A table with only a header still passes the format check (the UI shows the warning)
        let c = check_output(OutputFormat::MarkdownTable, &fields, "| a | b |\n|---|---|");
        assert!(c.ok && c.table_rows == Some(0));
        assert_eq!(
            check_output(OutputFormat::StrictJson, &fields, r#"{"a":1,"b":2}"#).table_rows,
            None
        );
    }

    #[test]
    fn mock_preview_passes_its_own_check() {
        let fields = f(ko_fixture!("core.pipeline.odd_fields"));
        for format in [OutputFormat::StrictJson, OutputFormat::MarkdownTable] {
            let r = compile(&CompileRequest {
                target: TargetModel::Gpt,
                role: "r".into(),
                constraints: vec![],
                format,
                fields: fields.clone(),
                data: None,
            });
            let want = &r.ast.output_schema.required_fields;
            assert!(!want.is_empty());
            let c = check_output(format, want, &r.preview);
            assert!(c.ok, "{format:?}\n{}\n{c:?}", r.preview);
            assert!(c.missing.is_empty(), "{format:?}: {:?}", c.missing);
        }
    }

    #[test]
    fn steering_rules_skip_fenced_code() {
        let md = ko_fixture!("core.pipeline.steering_md");
        assert_eq!(parse_steering_rules(md), f(ko_fixture!("core.pipeline.steering_rules")));
        // An unclosed fence is code up to the end of the document
        assert_eq!(parse_steering_rules("- a\n```\n- b\n- c"), f(&["a"]));
        // BOM and empty bullets
        assert_eq!(parse_steering_rules("\u{feff}- a\n- \n-  b "), f(&["a", "b"]));
    }

    #[test]
    fn read_text_auto_detects_encoding() {
        let dir = std::env::temp_dir().join(format!("cc_text_auto_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (rule, exclude) = (
            ko_fixture!("core.no_inference_outside_data"),
            ko_fixture!("core.exclude_missing"),
        );
        let text: &str = &format!("- {rule}\n- {exclude}");
        let (cp949, _, _) = encoding_rs::EUC_KR.encode(text);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let cases: [(&str, Vec<u8>); 4] = [
            ("utf8.md", text.as_bytes().to_vec()),
            ("bom.md", [b"\xEF\xBB\xBF".as_slice(), text.as_bytes()].concat()),
            ("cp949.md", cp949.into_owned()),
            ("utf16.md", utf16),
        ];
        for (name, bytes) in cases {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_text_auto(&p).unwrap(), text, "{name}");
            assert_eq!(parse_steering_rules(&read_text_auto(&p).unwrap()).len(), 2, "{name}");
        }
        assert!(
            read_text_auto(&dir.join(ko_fixture!("core.pipeline.missing_file")))
                .unwrap_err()
                .contains("cannot read file")
        );
        // Review repro: one broken byte in a UTF-8 document used to make the whole file read as CP949, garbling all
        // the Korean. With the same decision as the profiler it is read as UTF-8 and only the broken byte becomes
        // U+FFFD.
        let damaged = dir.join("damaged.md");
        std::fs::write(&damaged, [text.as_bytes(), b"\n- \xff"].concat()).unwrap();
        let read = read_text_auto(&damaged).unwrap();
        assert_eq!(read, format!("{text}\n- \u{FFFD}"));
        assert_eq!(parse_steering_rules(&read)[..2], f(&[rule, exclude]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_token_estimate_is_cached_until_file_changes() {
        let dir = std::env::temp_dir().join(format!("cc_tok_cache_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.csv");
        std::fs::write(&p, "id,amount\n1,10\n2,20\n3,30\n").unwrap();
        let (n, hit) = raw_file_tokens(&p).unwrap();
        assert!(!hit && n > 0);
        assert_eq!(raw_file_tokens(&p).unwrap(), (n, true));
        // Profiling again with only a different sample row count uses the same cached estimate
        let a = load_data(&p, 1).unwrap();
        let b = load_data(&p, 3).unwrap();
        assert_eq!((a.raw_tokens, b.raw_tokens), (n, n));
        assert!(raw_file_tokens(&p).unwrap().1);
        // When the file changes (length, modification time) it is computed again
        std::fs::write(&p, ko_fixture!("core.pipeline.changed_csv")).unwrap();
        let (m, hit) = raw_file_tokens(&p).unwrap();
        assert!(!hit && m != n, "{n} -> {m}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compare_targets_matches_single_compiles() {
        let req = CompileRequest {
            target: TargetModel::Claude,
            role: ko_fixture!("core.pipeline.sales_analyst").into(),
            constraints: vec![
                ko_fixture!("core.detect_outliers_first").into(),
                ko_fixture!("core.no_inference_outside_data").into(),
            ],
            format: OutputFormat::StrictJson,
            fields: f(&["root_causes", "outliers"]),
            data: None,
        };
        let cmp = compare_targets(&req);
        assert_eq!(
            cmp.iter().map(|c| c.target).collect::<Vec<_>>(),
            TargetModel::ALL.to_vec()
        );
        for c in &cmp {
            let single = compile(&CompileRequest {
                target: c.target,
                ..req.clone()
            });
            assert_eq!((c.orig_tokens, c.comp_tokens), (single.orig_tokens, single.comp_tokens));
            let want = tokenizer::cost_usd(single.comp_tokens, c.target.usd_per_million_input());
            assert!((c.cost_usd - want).abs() < 1e-12);
        }
    }

    /// Whether there is Hangul outside the user sentences quoted in double quotes
    fn hangul_outside_quotes(s: &str) -> bool {
        s.split('"')
            .step_by(2)
            .any(|part| part.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)))
    }

    /// The UI language only changes the text shown to the user. Compiling the same request (with data) in Korean and
    /// in English gives the same prompt (model-facing text, including profile statistics, outliers, samples and the
    /// schema), token counts, preview, and diagnostic count, severities and positions.
    #[test]
    fn compile_is_identical_in_both_languages_except_messages() {
        use crate::i18n::{Lang, with_lang};
        let dir = std::env::temp_dir().join(format!("cc_lang_compile_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(ko_fixture!("core.pipeline.sales_file"));
        let (region, memo) = (ko_fixture!("core.col.region"), ko_fixture!("core.col.memo"));
        let mut csv = format!("order_id,{region},amount,{memo}\n");
        for i in 0..60 {
            csv += &format!("{i},R{},{},\"{memo}, {i}\"\n", i % 4, 1000 + (i * 37) % 900);
        }
        csv += &format!("60,R1,9000000,{}\n", ko_fixture!("core.pipeline.unusual_trade"));
        std::fs::write(&p, csv).unwrap();
        let rules: Vec<String> = [
            ko_fixture!("core.pipeline.important_detect_first"),
            ko_fixture!("core.dont_detect_outliers"),
            "Detect outliers first",
            "detect outliers first.",
            ko_fixture!("core.exclude_missing"),
            ko_fixture!("core.include_missing"),
            ko_fixture!("core.compute_value"),
            ko_fixture!("core.dont_compute_value"),
            ko_fixture!("core.no_inference_outside_data"),
        ]
        .map(String::from)
        .to_vec();
        let mut seen = std::collections::BTreeSet::new();
        for target in TargetModel::ALL {
            for format in [OutputFormat::StrictJson, OutputFormat::MarkdownTable] {
                for fields in [f(&["root_causes", "outliers", "outliers", " "]), vec![]] {
                    let run = |lang| {
                        with_lang(lang, || {
                            let data = load_data(&p, 5).unwrap();
                            compile(&CompileRequest {
                                target,
                                role: "Please analyze very carefully".into(),
                                constraints: rules.clone(),
                                format,
                                fields: fields.clone(),
                                data: Some(data),
                            })
                        })
                    };
                    let (ko, en) = (run(Lang::Ko), run(Lang::En));
                    let case = format!("{target:?} {format:?} {fields:?}");
                    assert_eq!(ko.output, en.output, "{case}");
                    assert_eq!(
                        (ko.orig_tokens, ko.comp_tokens),
                        (en.orig_tokens, en.comp_tokens),
                        "{case}"
                    );
                    assert_eq!(ko.preview, en.preview, "{case}");
                    assert_eq!(
                        serde_json::to_string(&ko.ast.data_sub_ast).unwrap(),
                        serde_json::to_string(&en.ast.data_sub_ast).unwrap(),
                        "{case}"
                    );
                    let shape = |r: &CompileResult| {
                        r.ast
                            .lint_warnings
                            .iter()
                            .map(|d| (d.severity.clone(), d.indices.clone()))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(shape(&ko), shape(&en), "{case}");
                    for (k, e) in ko.ast.lint_warnings.iter().zip(&en.ast.lint_warnings) {
                        assert_ne!(k.message, e.message, "{case}: not translated");
                        assert!(!hangul_outside_quotes(&e.message), "{case}: {}", e.message);
                        seen.insert(e.severity.clone());
                    }
                    // The prompt really contains the profile (in the model-facing English wording)
                    assert!(
                        en.output.contains("amount") && en.output.contains("9000000 (z="),
                        "{case}"
                    );
                }
            }
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), ["ERROR", "INFO", "WARN"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_messages_follow_the_language() {
        use crate::i18n::{Lang, with_lang};
        let fields = f(&["a", "b"]);
        let replies = [
            reply(
                &format!("{{\"a\": \"{}", ko_fixture!("core.pipeline.long_explanation")),
                Some("max_tokens"),
            ),
            reply(
                &format!(
                    "| a | b |\n|---|---|\n| 1 | {}",
                    ko_fixture!("core.pipeline.unfinished")
                ),
                Some("length"),
            ),
            reply(r#"{"a":1}"#, Some("model_context_window_exceeded")),
            reply("", Some("content_filter")),
            reply("[1,2]", Some("stop")),
            reply(ko_fixture!("core.pipeline.no_table"), Some("stop")),
            reply(r#"{"a":1,"b":null}"#, Some("end_turn")),
        ];
        let formats = [
            OutputFormat::StrictJson,
            OutputFormat::MarkdownTable,
            OutputFormat::StrictJson,
            OutputFormat::StrictJson,
            OutputFormat::StrictJson,
            OutputFormat::MarkdownTable,
            OutputFormat::StrictJson,
        ];
        let checks = |lang| {
            with_lang(lang, || {
                replies
                    .iter()
                    .zip(formats)
                    .map(|(r, fmt)| {
                        check_reply(
                            fmt,
                            &fields,
                            r,
                            if fmt == OutputFormat::MarkdownTable { 512 } else { 2048 },
                        )
                    })
                    .collect::<Vec<_>>()
            })
        };
        let (ko, en) = (checks(Lang::Ko), checks(Lang::En));
        assert_eq!(
            en[0].cut_off.as_deref(),
            Some("Reached the output token limit (2048); the response was cut off (max_tokens)")
        );
        assert!(
            en[0]
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("JSON parse error: ")),
            "{:?}",
            en[0]
        );
        assert_eq!(
            en[1].cut_off.as_deref(),
            Some("Reached the output token limit (512); the response was cut off (length)")
        );
        assert_eq!(
            en[2].cut_off.as_deref(),
            Some("The model's context window is full; the response was cut off (model_context_window_exceeded)")
        );
        assert_eq!(
            en[3].cut_off.as_deref(),
            Some("The response did not finish (stop reason: content_filter)")
        );
        assert_eq!(en[4].error.as_deref(), Some("Not a JSON object"));
        assert_eq!(en[5].error.as_deref(), Some("No Markdown table found"));
        assert!(en[6].ok && en[6].error.is_none() && en[6].cut_off.is_none());
        // The Korean wording stays as it was, and the verdicts (pass, missing, blank values, stop reason, table rows,
        // limit reached) do not depend on the language
        assert_eq!(ko[4].error.as_deref(), Some(ko_text!("Not a JSON object")));
        for (k, e) in ko.iter().zip(&en) {
            assert_eq!(
                (&k.pretty, k.ok, &k.missing, &k.nulls, &k.stop_reason, k.table_rows),
                (&e.pretty, e.ok, &e.missing, &e.nulls, &e.stop_reason, e.table_rows)
            );
            assert_eq!(k.hit_max_tokens(), e.hit_max_tokens());
            assert_eq!(k.cut_off.is_some(), e.cut_off.is_some());
            assert_eq!(k.error.is_some(), e.error.is_some());
            for m in e.cut_off.iter().chain(&e.error) {
                assert!(!m.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)), "{m}");
            }
        }
    }

    #[test]
    fn read_errors_follow_the_language() {
        use crate::i18n::{Lang, with_lang};
        let p = std::env::temp_dir().join(format!("cc_missing_steering_{}.md", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert_eq!(
            with_lang(Lang::En, || read_text_auto(&p)),
            Err("cannot read file: no such file or directory (os error 2)".to_string())
        );
        let os = std::fs::read(&p).unwrap_err();
        assert_eq!(
            with_lang(Lang::Ko, || read_text_auto(&p)),
            Err(format!(ko_text!("cannot read file: {}"), os))
        );
        // Data load errors pass the profiler's message up unchanged
        let e = with_lang(Lang::En, || load_data(&p.with_extension("csv"), 2).unwrap_err());
        assert_eq!(e, "cannot open file: no such file or directory (os error 2)");
    }
}
