//! stdio JSON-RPC 2.0 MCP server. Handles newline-delimited JSON messages.
//! Tool provided: `compress_prompt`
//!
//! A bad line (not UTF-8, not JSON, not a request) gets an error reply, and handling continues with the next line.
//! Notifications (no id) and response objects sent by the client (result/error) get no reply.
//! Error messages and the lint lines of results follow the process language (`--mcp --lang en`); the tool
//! description (tools/list) is always English.

use crate::ast::{OutputFormat, TargetModel};
use crate::pipeline::{self, CompileRequest};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// Supported MCP protocol versions (newest first). initialize answers with the requested version when it is listed
/// here, otherwise with the first one. Since 2025-11-25, argument validation errors are reported as isError results;
/// this server reports argument errors as -32602, so it declares support only up to 2025-06-18, which follows that
/// rule.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// compress_prompt argument names. They must match the inputSchema properties of tool_def.
const ARG_NAMES: [&str; 7] = [
    "role",
    "constraints",
    "target",
    "format",
    "fields",
    "data_path",
    "sample_rows",
];
const DEFAULT_SAMPLE_ROWS: usize = 8;
const MAX_SAMPLE_ROWS: i64 = 50;

pub fn run() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(stdin.lock(), &mut stdout.lock())
}

/// Reads and answers line by line until the input ends. Lines are read as bytes, so a line that is not UTF-8 does not
/// stop the server.
fn serve(mut input: impl BufRead, out: &mut impl Write) -> std::io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if let Some(reply) = handle_line(&line) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
}

/// Handles one line (a single request or a batch). None when there is nothing to send back.
fn handle_line(raw: &[u8]) -> Option<Value> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return Some(error(
            Value::Null,
            PARSE_ERROR,
            tr!("JSON parse error: input is not valid UTF-8"),
        ));
    };
    // Strip the BOM some shells add and the trailing \r
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return None;
    }
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Some(error(Value::Null, PARSE_ERROR, &trf!("JSON parse error: {e}", e = e)));
        }
    };
    match msg {
        Value::Array(items) if items.is_empty() => {
            Some(error(Value::Null, INVALID_REQUEST, tr!("Invalid request: empty batch")))
        }
        // Batch: handle each item; when no item needs a reply (all notifications and the like), send nothing
        Value::Array(items) => {
            let replies: Vec<Value> = items.iter().filter_map(handle_message).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        single => handle_message(&single),
    }
}

/// Handles one message. None for notifications and for response objects sent by the client.
fn handle_message(msg: &Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            tr!("Invalid request: not a JSON object"),
        ));
    };
    let Some(method) = obj.get("method") else {
        // result or error without a method is a response sent by the client: no reply
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error(
            request_id(obj),
            INVALID_REQUEST,
            tr!("Invalid request: method is missing"),
        ));
    };
    // An MCP id is a string or an integer (not null, not a fraction)
    let id = match obj.get("id") {
        None => None,
        Some(v) if valid_id(v) => Some(v.clone()),
        Some(_) => {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                tr!("Invalid request: id must be a string or an integer"),
            ));
        }
    };
    let Some(method) = method.as_str() else {
        return Some(error(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            tr!("Invalid request: method must be a string"),
        ));
    };
    let id = id?; // Notifications (no id) get no reply
    let params = obj.get("params");
    Some(match method {
        "initialize" => ok(id, initialize_result(params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": [tool_def()] })),
        "tools/call" => call_tool(id, params),
        _ => error(
            id,
            METHOD_NOT_FOUND,
            &trf!("Method not found: {method}", method = method),
        ),
    })
}

/// MCP request id: a string or an integer
fn valid_id(v: &Value) -> bool {
    v.is_string() || v.is_i64() || v.is_u64()
}

/// The id for replying to a bad request: its value when readable, else null.
fn request_id(obj: &Map<String, Value>) -> Value {
    obj.get("id").filter(|v| valid_id(v)).cloned().unwrap_or(Value::Null)
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers with the requested version when it is supported, otherwise with the newest supported version.
fn initialize_result(params: Option<&Value>) -> Value {
    let requested = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "candy-corn", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn tool_def() -> Value {
    json!({
        "name": "compress_prompt",
        "description": "Lint and compress a prompt (role, constraints, optional CSV/TSV/JSON data profile) into a model-specific structured prompt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "role": { "type": "string", "description": "Role instruction for the model" },
                "constraints": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Constraint sentences. Items are trimmed; blank items are dropped"
                },
                "target": {
                    "type": "string",
                    "enum": ["claude", "gpt", "gemini"],
                    "default": "claude",
                    "description": "Target model (case-insensitive; aliases: anthropic, openai, google)"
                },
                "format": {
                    "type": "string",
                    "enum": ["json", "table"],
                    "default": "json",
                    "description": "Output format (case-insensitive; alias: markdown = table)"
                },
                "fields": {
                    "anyOf": [
                        { "type": "array", "items": { "type": "string" } },
                        { "type": "string" }
                    ],
                    "description": "Required output fields: array of names or a comma-separated string. Names are trimmed and blanks dropped; omitted or empty means root_causes, outliers, recommendations"
                },
                "data_path": { "type": "string", "description": "Local CSV/TSV/JSON/NDJSON file to profile" },
                "sample_rows": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_SAMPLE_ROWS,
                    "default": DEFAULT_SAMPLE_ROWS,
                    "description": "Sample rows taken from data_path (values outside 0..50 are clamped)"
                }
            },
            "required": ["role"],
            "additionalProperties": false
        }
    })
}

/// compress_prompt arguments. An invalid value is not replaced with a default; it is returned as an error that names
/// the argument.
#[derive(Debug)]
struct ToolArgs {
    role: String,
    constraints: Vec<String>,
    target: TargetModel,
    format: OutputFormat,
    fields: Vec<String>,
    data_path: Option<String>,
    sample_rows: usize,
}

impl ToolArgs {
    fn parse(args: Option<&Value>) -> Result<Self, String> {
        let empty = Map::new();
        let args = match args {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(tr!("arguments must be an object").into()),
        };
        if let Some(k) = args.keys().find(|k| !ARG_NAMES.contains(&k.as_str())) {
            return Err(trf!("Unknown argument: {k} (allowed: {})", ARG_NAMES.join(", "), k = k));
        }
        // null counts as omitted
        let get = |k: &str| args.get(k).filter(|v| !v.is_null());
        let role = match get("role") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(tr!("role must be a string").into()),
            None => return Err(tr!("role is required (string)").into()),
        };
        let constraints = match get("constraints") {
            None => vec![],
            Some(Value::Array(items)) => string_items("constraints", items)?,
            Some(_) => {
                return Err(tr!("constraints must be an array of strings").into());
            }
        };
        let target = match get("target") {
            None => TargetModel::Claude,
            Some(Value::String(s)) => parse_target(s).ok_or_else(|| {
                trf!(
                    "Invalid target: {s:?} (expected one of claude/anthropic, gpt/openai, gemini/google)",
                    s = s
                )
            })?,
            Some(_) => return Err(tr!("target must be a string").into()),
        };
        let format = match get("format") {
            None => OutputFormat::StrictJson,
            Some(Value::String(s)) => parse_format(s)
                .ok_or_else(|| trf!("Invalid format: {s:?} (expected one of json, table/markdown)", s = s))?,
            Some(_) => return Err(tr!("format must be a string").into()),
        };
        let fields = match get("fields") {
            None => vec![],
            Some(Value::String(s)) => pipeline::parse_fields(s),
            Some(Value::Array(items)) => string_items("fields", items)?,
            Some(_) => {
                return Err(tr!("fields must be an array of strings or a comma-separated string").into());
            }
        };
        let fields = if fields.is_empty() {
            pipeline::parse_fields(pipeline::DEFAULT_FIELDS)
        } else {
            fields
        };
        let data_path = match get("data_path") {
            None => None,
            Some(Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Some(_) => return Err(tr!("data_path must be a string").into()),
        };
        let sample_rows = match get("sample_rows") {
            None => DEFAULT_SAMPLE_ROWS,
            Some(v) => parse_sample_rows(v).ok_or(tr!("sample_rows must be an integer (0-50)"))?,
        };
        Ok(ToolArgs {
            role,
            constraints,
            target,
            format,
            fields,
            data_path,
            sample_rows,
        })
    }
}

/// String array argument: items are trimmed and blank items dropped. An item that is not a string is an error.
fn string_items(arg: &str, items: &[Value]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| trf!("{arg}[{i}] is not a string", arg = arg, i = i))?;
        let s = s.trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

fn parse_target(s: &str) -> Option<TargetModel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some(TargetModel::Claude),
        "gpt" | "openai" => Some(TargetModel::Gpt),
        "gemini" | "google" => Some(TargetModel::Gemini),
        _ => None,
    }
}

fn parse_format(s: &str) -> Option<OutputFormat> {
    match s.trim().to_ascii_lowercase().as_str() {
        "json" => Some(OutputFormat::StrictJson),
        "table" | "markdown" => Some(OutputFormat::MarkdownTable),
        _ => None,
    }
}

/// Accepts only integers (including integral floats such as 2.0) and clamps them to 0..=50.
fn parse_sample_rows(v: &Value) -> Option<usize> {
    let n = v
        .as_i64()
        .or_else(|| v.as_u64().map(|_| i64::MAX))
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))?;
    Some(n.clamp(0, MAX_SAMPLE_ROWS) as usize)
}

fn call_tool(id: Value, params: Option<&Value>) -> Value {
    let Some(params) = params.and_then(Value::as_object) else {
        return error(
            id,
            INVALID_PARAMS,
            tr!("params must be an object with name and arguments"),
        );
    };
    match params.get("name").and_then(Value::as_str) {
        Some("compress_prompt") => {}
        Some(other) => {
            return error(id, INVALID_PARAMS, &trf!("Unknown tool: {other}", other = other));
        }
        None => {
            return error(id, INVALID_PARAMS, tr!("params.name (tool name) is missing"));
        }
    }
    let args = match ToolArgs::parse(params.get("arguments")) {
        Ok(a) => a,
        Err(msg) => return error(id, INVALID_PARAMS, &msg),
    };
    // Even if the profiler or the compiler panics, the server keeps taking requests
    match catch_unwind(AssertUnwindSafe(|| compress(args))) {
        Ok(result) => ok(id, result),
        Err(_) => error(
            id,
            INTERNAL_ERROR,
            tr!("Internal error: panic while building the prompt"),
        ),
    }
}

/// Runs compress_prompt. A data file that cannot be read is reported as an isError result (a tool execution error).
/// The prompt part of the result text is the same in every UI language; only the lint lines follow the UI language.
fn compress(args: ToolArgs) -> Value {
    let data = match &args.data_path {
        Some(p) => match pipeline::load_data(Path::new(p), args.sample_rows) {
            Ok(d) => Some(d),
            Err(e) => return tool_text(&trf!("Failed to load data: {e}", e = e), true),
        },
        None => None,
    };
    let req = CompileRequest {
        target: args.target,
        role: args.role,
        constraints: args.constraints,
        format: args.format,
        fields: args.fields,
        data,
    };
    let r = pipeline::compile(&req);
    let lint: Vec<String> = r
        .ast
        .lint_warnings
        .iter()
        .map(|d| format!("[{}] {}", d.severity, d.message))
        .collect();
    let text = format!(
        "{}\n\n--- lint ---\n{}\n--- {} ---",
        r.output,
        lint.join("\n"),
        token_line(r.orig_tokens, r.comp_tokens, r.saving_pct, r.saved_usd)
    );
    let is_error = r.ast.lint_warnings.iter().any(|d| d.severity == "ERROR");
    tool_text(&text, is_error)
}

/// The same notation as the CLI: + when the token count grows, - when it shrinks (e.g. `tokens: 28 -> 53 (+89.3%,
/// saved $-0.0001)`).
fn token_line(orig: usize, comp: usize, saving_pct: f64, saved_usd: f64) -> String {
    // Keep 0 as is so that -0.0 is not printed as "-0.0%"
    let change = if saving_pct == 0.0 { 0.0 } else { -saving_pct };
    let saved = if saved_usd == 0.0 { 0.0 } else { saved_usd };
    format!("tokens: {orig} -> {comp} ({change:+.1}%, saved ${saved:.4})")
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str) -> Option<Value> {
        handle_line(line.as_bytes())
    }

    fn call(args: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "compress_prompt", "arguments": args }
        });
        reply(&req.to_string()).unwrap()
    }

    fn text_of(v: &Value) -> &str {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no result text: {v}"))
    }

    fn invalid_params(v: &Value, arg: &str) {
        assert_eq!(v["error"]["code"], INVALID_PARAMS, "{v}");
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains(arg), "{arg}: {msg}");
    }

    #[test]
    fn invalid_utf8_line_is_parse_error_and_serving_continues() {
        let mut input = b"\xff\xfe{\"jsonrpc\":\"2.0\",\"id\":9}\n".to_vec();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n");
        input.extend_from_slice(b"\n   \nnot json\n");
        input.extend_from_slice("\u{feff}{\"jsonrpc\":\"2.0\",\"id\":\"b\",\"method\":\"ping\"}\n".as_bytes());
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}"); // the last line has no newline
        let mut out = Vec::new();
        serve(&input[..], &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[0]["error"]["code"], PARSE_ERROR);
        assert!(lines[0]["id"].is_null());
        assert_eq!((&lines[1]["id"], &lines[1]["result"]), (&json!(1), &json!({})));
        assert_eq!(lines[2]["error"]["code"], PARSE_ERROR);
        assert_eq!(lines[3]["id"], "b");
        assert_eq!(lines[4]["id"], 2);
    }

    #[test]
    fn invalid_requests_get_32600_with_null_id() {
        for line in ["1", "\"x\"", "null", "true"] {
            let v = reply(line).unwrap();
            assert_eq!(
                (&v["error"]["code"], &v["id"]),
                (&json!(INVALID_REQUEST), &Value::Null),
                "{line}"
            );
        }
        // A batch gets a reply per item
        let v = reply("[1, 2]").unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(
            arr.iter()
                .all(|e| e["error"]["code"] == INVALID_REQUEST && e["id"].is_null())
        );
        assert_eq!(reply("[1]").unwrap()[0]["error"]["code"], INVALID_REQUEST);
        assert_eq!(reply("[]").unwrap()["error"]["code"], INVALID_REQUEST);
        // A request object without a method is -32600, not -32601 (a readable id is returned)
        let v = reply(r#"{"jsonrpc":"2.0","id":3}"#).unwrap();
        assert_eq!((&v["error"]["code"], &v["id"]), (&json!(INVALID_REQUEST), &json!(3)));
        let v = reply(r#"{"jsonrpc":"2.0","id":4,"method":7}"#).unwrap();
        assert_eq!((&v["error"]["code"], &v["id"]), (&json!(INVALID_REQUEST), &json!(4)));
        for bad_id in ["null", "true", "{}", "[1]", "1.5"] {
            let v = reply(&format!(r#"{{"jsonrpc":"2.0","id":{bad_id},"method":"ping"}}"#)).unwrap();
            assert_eq!(
                (&v["error"]["code"], &v["id"]),
                (&json!(INVALID_REQUEST), &Value::Null),
                "{bad_id}"
            );
        }
        let v = reply(r#"{"jsonrpc":"2.0","id":5,"method":"no/such"}"#).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn responses_and_notifications_get_no_reply() {
        for line in [
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":null,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"no/such/notification"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"compress_prompt"}}"#,
            r#"[{"jsonrpc":"2.0","method":"a"},{"jsonrpc":"2.0","id":3,"result":1}]"#,
            "",
            "  \r",
        ] {
            assert_eq!(reply(line), None, "{line}");
        }
        // Inside a batch, too, only the items that need a reply get one
        let v =
            reply(r#"[{"jsonrpc":"2.0","method":"a"},{"jsonrpc":"2.0","id":7,"method":"ping"},{"id":8,"result":{}}]"#)
                .unwrap();
        assert_eq!(v, json!([{ "jsonrpc": "2.0", "id": 7, "result": {} }]));
    }

    #[test]
    fn initialize_negotiates_version_and_lists_matching_schema() {
        let init = |params: &str| {
            reply(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{params}}}"#
            ))
            .unwrap()
        };
        for v in PROTOCOL_VERSIONS {
            let r = init(&format!(r#"{{"protocolVersion":"{v}","capabilities":{{}}}}"#));
            assert_eq!(r["result"]["protocolVersion"], v);
        }
        for params in [r#"{"protocolVersion":"2099-01-01"}"#, "{}", r#"{"protocolVersion":5}"#] {
            assert_eq!(
                init(params)["result"]["protocolVersion"],
                PROTOCOL_VERSIONS[0],
                "{params}"
            );
        }
        let r = init("{}")["result"].clone();
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["serverInfo"]["name"], "candy-corn");
        assert_eq!(r["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));

        let list = reply(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let schema = &list["result"]["tools"][0]["inputSchema"];
        let mut props: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        props.sort_unstable();
        let mut names = ARG_NAMES.to_vec();
        names.sort_unstable();
        assert_eq!(props, names, "inputSchema and tools/call must take the same arguments");
        assert_eq!(schema["required"], json!(["role"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["fields"]["anyOf"][1]["type"], "string");
    }

    #[test]
    fn fields_accept_string_or_array_trimmed() {
        for fields in [json!("a, b ,,c"), json!([" a ", "b", "", "c"])] {
            let v = call(json!({ "role": "r", "fields": fields }));
            let text = text_of(&v);
            assert!(text.contains(r#""required":["a","b","c"]"#), "{fields}: {text}");
            assert!(!text.contains("root_causes"));
        }
        // Empty or omitted: the default fields
        for args in [
            json!({ "role": "r" }),
            json!({ "role": "r", "fields": " , " }),
            json!({ "role": "r", "fields": [] }),
        ] {
            assert!(text_of(&call(args)).contains("root_causes"));
        }
        invalid_params(&call(json!({ "role": "r", "fields": 3 })), "fields");
        invalid_params(&call(json!({ "role": "r", "fields": ["a", 1] })), "fields[1]");
    }

    #[test]
    fn target_and_format_aliases_or_invalid_params() {
        for (t, marker) in [
            ("Claude", "<role "),
            ("anthropic", "<role "),
            ("GPT", "# Role"),
            ("openai", "# Role"),
            (" gemini ", "=== ROLE ==="),
            ("Google", "=== ROLE ==="),
        ] {
            assert!(
                text_of(&call(json!({ "role": "r", "target": t }))).starts_with(marker),
                "{t}"
            );
        }
        for f in ["table", "Markdown"] {
            let text = text_of(&call(json!({ "role": "r", "format": f, "fields": "a" }))).to_string();
            assert!(text.contains("Markdown table"), "{f}: {text}");
        }
        invalid_params(&call(json!({ "role": "r", "target": "llama" })), "target");
        invalid_params(&call(json!({ "role": "r", "target": 1 })), "target");
        invalid_params(&call(json!({ "role": "r", "format": "xml" })), "format");
    }

    #[test]
    fn other_arguments_are_validated() {
        invalid_params(
            &call(json!({ "role": "r", "constraints": ko_fixture!("core.mcp_server.one") })),
            "constraints",
        );
        invalid_params(
            &call(json!({ "role": "r", "constraints": ["a", null] })),
            "constraints[1]",
        );
        invalid_params(&call(json!({ "role": 3 })), "role");
        invalid_params(&call(json!({})), "role");
        invalid_params(&call(json!({ "role": "r", "steering": "x.md" })), "steering");
        invalid_params(&call(json!({ "role": "r", "data_path": 1 })), "data_path");
        invalid_params(&call(json!({ "role": "r", "sample_rows": "8" })), "sample_rows");
        invalid_params(&call(json!({ "role": "r", "sample_rows": 1.5 })), "sample_rows");
        invalid_params(&call(Value::Array(vec![])), "arguments");
        // Blank constraints are dropped and the rest trimmed; null is the same as omitted
        let exclude = ko_fixture!("core.exclude_missing");
        let v = call(json!({ "role": "r", "constraints": [format!("  {exclude}  "), " "], "target": null }));
        let text = text_of(&v);
        assert!(
            text.contains(&format!(">{exclude}</rule>")) && !text.contains("C2"),
            "{text}"
        );
        let bad_tool = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope"}}"#).unwrap();
        invalid_params(&bad_tool, "nope");
        let no_params = reply(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#).unwrap();
        invalid_params(&no_params, "params");
    }

    #[test]
    fn sample_rows_clamped_and_data_errors_are_tool_errors() {
        let dir = std::env::temp_dir().join(format!("cc_mcp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("d.csv");
        let rows: String = (1..=80).map(|i| format!("{i},{}\n", i % 7)).collect();
        std::fs::write(&csv, format!("id,v\n{rows}")).unwrap();
        let path = csv.display().to_string();
        let sample_lines = |n: Value| {
            let v = call(json!({ "role": "r", "target": "gpt", "data_path": path, "sample_rows": n }));
            let text = text_of(&v).to_string();
            text.lines()
                .filter(|l| l.split(',').count() == 2 && l.split(',').all(|c| c.parse::<u32>().is_ok()))
                .count()
        };
        assert_eq!(sample_lines(json!(3)), 3);
        assert_eq!(sample_lines(json!(2.0)), 2);
        assert_eq!(sample_lines(json!(999)), 50);
        assert_eq!(sample_lines(json!(-4)), 0);
        let missing = call(json!({ "role": "r", "data_path": dir.join("none.csv").display().to_string() }));
        assert_eq!(missing["result"]["isError"], true);
        assert!(text_of(&missing).starts_with("Failed to load data"));
        // A blank path means no data
        assert_eq!(
            call(json!({ "role": "r", "data_path": "  " }))["result"]["isError"],
            false
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_change_has_sign() {
        assert_eq!(
            token_line(28, 53, -89.29, -0.00008),
            "tokens: 28 -> 53 (+89.3%, saved $-0.0001)"
        );
        assert_eq!(
            token_line(2464, 282, 88.56, 0.0065),
            "tokens: 2464 -> 282 (-88.6%, saved $0.0065)"
        );
        assert_eq!(token_line(5, 5, 0.0, -0.0), "tokens: 5 -> 5 (+0.0%, saved $0.0000)");
        let text = text_of(&call(json!({ "role": "Data analyst" }))).to_string();
        let line = text.lines().last().unwrap();
        assert!(
            line.starts_with("--- tokens: ") && (line.contains("(+") || line.contains("(-")),
            "{line}"
        );
    }

    fn has_hangul(s: &str) -> bool {
        s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
    }

    /// `--mcp --lang en`: JSON-RPC error messages are English and carry the argument names and values as they are.
    #[test]
    fn english_mode_error_messages() {
        use crate::i18n::{Lang, with_lang};
        let msg = |v: &Value| v["error"]["message"].as_str().unwrap_or_default().to_string();
        with_lang(Lang::En, || {
            let lines: [(&[u8], i64, &str); 8] = [
                (b"\xff\xfe", PARSE_ERROR, "JSON parse error: input is not valid UTF-8"),
                (b"[]", INVALID_REQUEST, "Invalid request: empty batch"),
                (b"1", INVALID_REQUEST, "Invalid request: not a JSON object"),
                (
                    br#"{"jsonrpc":"2.0","id":3}"#,
                    INVALID_REQUEST,
                    "Invalid request: method is missing",
                ),
                (
                    br#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
                    INVALID_REQUEST,
                    "Invalid request: id must be a string or an integer",
                ),
                (
                    br#"{"jsonrpc":"2.0","id":4,"method":7}"#,
                    INVALID_REQUEST,
                    "Invalid request: method must be a string",
                ),
                (
                    br#"{"jsonrpc":"2.0","id":5,"method":"no/such"}"#,
                    METHOD_NOT_FOUND,
                    "Method not found: no/such",
                ),
                (
                    br#"{"jsonrpc":"2.0","id":6,"method":"tools/call"}"#,
                    INVALID_PARAMS,
                    "params must be an object with name and arguments",
                ),
            ];
            for (line, code, want) in lines {
                let v = handle_line(line).unwrap();
                assert_eq!((v["error"]["code"].as_i64(), msg(&v)), (Some(code), want.to_string()));
            }
            let v = reply("not json").unwrap();
            assert!(msg(&v).starts_with("JSON parse error: "), "{v}");
            let tool = |params: &str| {
                reply(&format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{params}}}"#
                ))
                .unwrap()
            };
            assert_eq!(msg(&tool(r#"{"name":"nope"}"#)), "Unknown tool: nope");
            assert_eq!(msg(&tool("{}")), "params.name (tool name) is missing");
            for (args, want) in [
                (
                    json!({ "role": "r", "target": "llama" }),
                    "Invalid target: \"llama\" (expected one of claude/anthropic, gpt/openai, gemini/google)",
                ),
                (
                    json!({ "role": "r", "format": "xml" }),
                    "Invalid format: \"xml\" (expected one of json, table/markdown)",
                ),
                (json!({ "role": 3 }), "role must be a string"),
                (json!({}), "role is required (string)"),
                (
                    json!({ "role": "r", "constraints": "x" }),
                    "constraints must be an array of strings",
                ),
                (
                    json!({ "role": "r", "constraints": ["a", 1] }),
                    "constraints[1] is not a string",
                ),
                (json!({ "role": "r", "target": 1 }), "target must be a string"),
                (json!({ "role": "r", "format": 1 }), "format must be a string"),
                (
                    json!({ "role": "r", "fields": 3 }),
                    "fields must be an array of strings or a comma-separated string",
                ),
                (
                    json!({ "role": "r", "fields": ["a", null] }),
                    "fields[1] is not a string",
                ),
                (json!({ "role": "r", "data_path": 1 }), "data_path must be a string"),
                (
                    json!({ "role": "r", "sample_rows": "8" }),
                    "sample_rows must be an integer (0-50)",
                ),
                (
                    json!({ "role": "r", "steering": "x.md" }),
                    "Unknown argument: steering (allowed: role, constraints, target, format, fields, data_path, sample_rows)",
                ),
                (Value::Array(vec![]), "arguments must be an object"),
            ] {
                let v = call(args.clone());
                assert_eq!(v["error"]["code"], INVALID_PARAMS, "{args}");
                assert_eq!(msg(&v), want, "{args}");
            }
        });
        // Korean mode keeps the old wording
        assert_eq!(
            with_lang(Lang::Ko, || msg(&call(json!({ "role": 3 })))),
            ko_text!("role must be a string")
        );
    }

    /// Tool results in English mode: the prompt part, tools/list and initialize are the same as in Korean mode; only
    /// the lint lines and data errors are English.
    #[test]
    fn english_mode_tool_results() {
        use crate::i18n::{Lang, with_lang};
        let args = json!({
            "role": "Analyst",
            "constraints": ["Detect outliers", "You must not detect outliers"],
            "fields": "a,a"
        });
        let (ko, en) = (
            with_lang(Lang::Ko, || call(args.clone())),
            with_lang(Lang::En, || call(args.clone())),
        );
        let split = |v: &Value| {
            let (prompt, rest) = text_of(v).split_once("\n\n--- lint ---\n").unwrap();
            (prompt.to_string(), rest.to_string())
        };
        let ((ko_prompt, ko_lint), (en_prompt, en_lint)) = (split(&ko), split(&en));
        assert_eq!(ko_prompt, en_prompt);
        assert_eq!(ko["result"]["isError"], en["result"]["isError"]);
        assert_eq!(ko_lint.lines().count(), en_lint.lines().count());
        assert_eq!(
            ko_lint.lines().last(),
            en_lint.lines().last(),
            "the token line is the same"
        );
        assert!(
            en_lint.contains(
                "[WARN] Removed contradicting instruction: \"Detect outliers\" ↔ \"You must not detect outliers\" (kept the later one)"
            ) && en_lint.contains("[WARN] Removed blank/duplicate output schema fields: duplicate \"a\"")
                && en_lint.contains("[INFO] Schema integrity check passed"),
            "{en_lint}"
        );
        let ko_clash = format!(
            ko_text!("Removed contradicting instruction: \"{}\" ↔ \"{}\" (kept the later one)"),
            "Detect outliers", "You must not detect outliers"
        );
        assert!(
            !has_hangul(&en_lint) && ko_lint.contains(&format!("[WARN] {ko_clash}")),
            "{en_lint}\n{ko_lint}"
        );
        // A data load failure is an isError result, and even the profiler's text is English
        let missing = std::env::temp_dir().join(format!("cc_mcp_en_missing_{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let v = with_lang(Lang::En, || {
            call(json!({ "role": "r", "data_path": missing.display().to_string() }))
        });
        assert_eq!(v["result"]["isError"], true);
        assert_eq!(
            text_of(&v),
            "Failed to load data: cannot open file: no such file or directory (os error 2)"
        );
        // The tool description (tools/list) and the initialize result do not depend on the language
        for line in [
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ] {
            assert_eq!(
                with_lang(Lang::Ko, || reply(line)),
                with_lang(Lang::En, || reply(line)),
                "{line}"
            );
        }
    }
}
