//! stdio JSON-RPC 2.0 MCP 서버. 줄 단위(newline-delimited) JSON 메시지를 처리한다.
//! 제공 도구: `compress_prompt`
//!
//! 잘못된 줄(UTF-8 아님, JSON 아님, 요청 형식 아님)에는 오류로 답하고 다음 줄을 계속 처리한다.
//! notification(id 없음)과 클라이언트가 보낸 응답 객체(result/error)에는 답하지 않는다.
//! 오류 메시지와 결과의 lint 줄은 프로세스 언어(`--mcp --lang en`)를 따르고, 도구 설명(tools/list)은 언제나 영어다.

use crate::ast::{OutputFormat, TargetModel};
use crate::pipeline::{self, CompileRequest};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// 지원하는 MCP 프로토콜 버전(최신순). initialize에서 요청한 버전이 여기 있으면 그대로, 없으면 맨 앞을 돌려준다.
/// 2025-11-25부터는 인자 검증 오류를 isError 결과로 알리게 바뀌었는데, 이 서버는 인자 오류를 -32602로 알리므로
/// 그 규칙을 따르는 2025-06-18까지만 지원한다고 밝힌다.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// compress_prompt 인자 이름. tool_def의 inputSchema properties와 같아야 한다.
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

/// 입력이 끝날 때까지 한 줄씩 읽어 답한다. 줄은 바이트로 읽으므로 UTF-8이 아닌 줄이 와도 멈추지 않는다.
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

/// 한 줄(요청 하나 또는 배치)을 처리한다. 보낼 응답이 없으면 None.
fn handle_line(raw: &[u8]) -> Option<Value> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return Some(error(
            Value::Null,
            PARSE_ERROR,
            tr!(
                "JSON 파싱 오류: UTF-8이 아닌 입력입니다",
                "JSON parse error: input is not valid UTF-8"
            ),
        ));
    };
    // 일부 셸이 붙이는 BOM과 줄 끝 \r 제거
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return None;
    }
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return Some(error(
                Value::Null,
                PARSE_ERROR,
                &trf!("JSON 파싱 오류: {e}", "JSON parse error: {e}"),
            ));
        }
    };
    match msg {
        Value::Array(items) if items.is_empty() => Some(error(
            Value::Null,
            INVALID_REQUEST,
            tr!("잘못된 요청: 빈 배치", "Invalid request: empty batch"),
        )),
        // 배치: 항목마다 처리하고, 답할 항목이 하나도 없으면(모두 notification 등) 아무것도 보내지 않는다
        Value::Array(items) => {
            let replies: Vec<Value> = items.iter().filter_map(handle_message).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        single => handle_message(&single),
    }
}

/// 메시지 하나를 처리한다. notification과 클라이언트가 보낸 응답 객체에는 None.
fn handle_message(msg: &Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            tr!(
                "잘못된 요청: JSON 객체가 아닙니다",
                "Invalid request: not a JSON object"
            ),
        ));
    };
    let Some(method) = obj.get("method") else {
        // method 없이 result나 error가 있으면 클라이언트가 보낸 응답이다: 답하지 않는다
        if obj.contains_key("result") || obj.contains_key("error") {
            return None;
        }
        return Some(error(
            request_id(obj),
            INVALID_REQUEST,
            tr!("잘못된 요청: method가 없습니다", "Invalid request: method is missing"),
        ));
    };
    // MCP의 id는 문자열 또는 정수다(null·소수 불가)
    let id = match obj.get("id") {
        None => None,
        Some(v) if valid_id(v) => Some(v.clone()),
        Some(_) => {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                tr!(
                    "잘못된 요청: id는 문자열 또는 정수여야 합니다",
                    "Invalid request: id must be a string or an integer"
                ),
            ));
        }
    };
    let Some(method) = method.as_str() else {
        return Some(error(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            tr!(
                "잘못된 요청: method는 문자열이어야 합니다",
                "Invalid request: method must be a string"
            ),
        ));
    };
    let id = id?; // notification(id 없음)에는 답하지 않는다
    let params = obj.get("params");
    Some(match method {
        "initialize" => ok(id, initialize_result(params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": [tool_def()] })),
        "tools/call" => call_tool(id, params),
        _ => error(
            id,
            METHOD_NOT_FOUND,
            &trf!("메서드를 찾을 수 없습니다: {method}", "Method not found: {method}"),
        ),
    })
}

/// MCP 요청 id: 문자열 또는 정수
fn valid_id(v: &Value) -> bool {
    v.is_string() || v.is_i64() || v.is_u64()
}

/// 잘못된 요청에 답할 때 쓸 id: 읽을 수 있으면 그 값, 아니면 null.
fn request_id(obj: &Map<String, Value>) -> Value {
    obj.get("id").filter(|v| valid_id(v)).cloned().unwrap_or(Value::Null)
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 요청한 버전을 지원하면 그대로, 아니면 지원하는 최신 버전으로 답한다.
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

/// compress_prompt 인자. 잘못된 값은 기본값으로 바꾸지 않고 인자 이름을 밝힌 오류로 돌려준다.
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
            Some(_) => return Err(tr!("arguments 값은 객체여야 합니다", "arguments must be an object").into()),
        };
        if let Some(k) = args.keys().find(|k| !ARG_NAMES.contains(&k.as_str())) {
            return Err(trf!(
                "알 수 없는 인자: {k} (사용할 수 있는 인자: {})",
                "Unknown argument: {k} (allowed: {})",
                ARG_NAMES.join(", ")
            ));
        }
        // null은 생략한 것으로 본다
        let get = |k: &str| args.get(k).filter(|v| !v.is_null());
        let role = match get("role") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(tr!("role 인자는 문자열이어야 합니다", "role must be a string").into()),
            None => return Err(tr!("role 인자가 필요합니다 (문자열)", "role is required (string)").into()),
        };
        let constraints = match get("constraints") {
            None => vec![],
            Some(Value::Array(items)) => string_items("constraints", items)?,
            Some(_) => {
                return Err(tr!(
                    "constraints 인자는 문자열 배열이어야 합니다",
                    "constraints must be an array of strings"
                )
                .into());
            }
        };
        let target = match get("target") {
            None => TargetModel::Claude,
            Some(Value::String(s)) => parse_target(s).ok_or_else(|| {
                trf!(
                    "target 인자 값이 올바르지 않습니다: {s:?} (claude/anthropic, gpt/openai, gemini/google 중 하나)",
                    "Invalid target: {s:?} (expected one of claude/anthropic, gpt/openai, gemini/google)"
                )
            })?,
            Some(_) => return Err(tr!("target 인자는 문자열이어야 합니다", "target must be a string").into()),
        };
        let format = match get("format") {
            None => OutputFormat::StrictJson,
            Some(Value::String(s)) => parse_format(s).ok_or_else(|| {
                trf!(
                    "format 인자 값이 올바르지 않습니다: {s:?} (json, table/markdown 중 하나)",
                    "Invalid format: {s:?} (expected one of json, table/markdown)"
                )
            })?,
            Some(_) => return Err(tr!("format 인자는 문자열이어야 합니다", "format must be a string").into()),
        };
        let fields = match get("fields") {
            None => vec![],
            Some(Value::String(s)) => pipeline::parse_fields(s),
            Some(Value::Array(items)) => string_items("fields", items)?,
            Some(_) => {
                return Err(tr!(
                    "fields 인자는 문자열 배열이나 쉼표로 구분한 문자열이어야 합니다",
                    "fields must be an array of strings or a comma-separated string"
                )
                .into());
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
            Some(_) => return Err(tr!("data_path 인자는 문자열이어야 합니다", "data_path must be a string").into()),
        };
        let sample_rows = match get("sample_rows") {
            None => DEFAULT_SAMPLE_ROWS,
            Some(v) => parse_sample_rows(v).ok_or(tr!(
                "sample_rows 인자는 정수여야 합니다 (0~50)",
                "sample_rows must be an integer (0-50)"
            ))?,
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

/// 문자열 배열 인자: 항목의 앞뒤 공백을 떼고 빈 항목은 버린다. 문자열이 아닌 항목은 오류.
fn string_items(arg: &str, items: &[Value]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(items.len());
    for (i, v) in items.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| trf!("{arg}[{i}] 값이 문자열이 아닙니다", "{arg}[{i}] is not a string"))?;
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

/// 정수(2.0 같은 정수 값의 실수 포함)만 받고 0..=50으로 자른다.
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
            tr!(
                "params는 name과 arguments를 담은 객체여야 합니다",
                "params must be an object with name and arguments"
            ),
        );
    };
    match params.get("name").and_then(Value::as_str) {
        Some("compress_prompt") => {}
        Some(other) => {
            return error(
                id,
                INVALID_PARAMS,
                &trf!("알 수 없는 도구: {other}", "Unknown tool: {other}"),
            );
        }
        None => {
            return error(
                id,
                INVALID_PARAMS,
                tr!(
                    "params.name(도구 이름)이 없습니다",
                    "params.name (tool name) is missing"
                ),
            );
        }
    }
    let args = match ToolArgs::parse(params.get("arguments")) {
        Ok(a) => a,
        Err(msg) => return error(id, INVALID_PARAMS, &msg),
    };
    // 프로파일러나 컴파일러에서 패닉이 나도 서버는 다음 요청을 계속 받는다
    match catch_unwind(AssertUnwindSafe(|| compress(args))) {
        Ok(result) => ok(id, result),
        Err(_) => error(
            id,
            INTERNAL_ERROR,
            tr!(
                "내부 오류: 프롬프트를 만드는 중 패닉이 발생했습니다",
                "Internal error: panic while building the prompt"
            ),
        ),
    }
}

/// compress_prompt 실행. 데이터 파일을 읽지 못하면 isError 결과(도구 실행 오류)로 알린다.
/// 결과 텍스트의 프롬프트 부분은 UI 언어와 관계없이 같고, lint 줄만 UI 언어를 따른다.
fn compress(args: ToolArgs) -> Value {
    let data = match &args.data_path {
        Some(p) => match pipeline::load_data(Path::new(p), args.sample_rows) {
            Ok(d) => Some(d),
            Err(e) => return tool_text(&trf!("데이터 로드 실패: {e}", "Failed to load data: {e}"), true),
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

/// CLI와 같은 표기: 토큰이 늘면 +, 줄면 - (예: `tokens: 28 -> 53 (+89.3%, saved $-0.0001)`).
fn token_line(orig: usize, comp: usize, saving_pct: f64, saved_usd: f64) -> String {
    // -0.0이 "-0.0%"로 찍히지 않게 0은 그대로 둔다
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
            .unwrap_or_else(|| panic!("결과 텍스트 없음: {v}"))
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
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}"); // 마지막 줄은 개행 없이
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
        // 배치는 항목마다 답한다
        let v = reply("[1, 2]").unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(
            arr.iter()
                .all(|e| e["error"]["code"] == INVALID_REQUEST && e["id"].is_null())
        );
        assert_eq!(reply("[1]").unwrap()[0]["error"]["code"], INVALID_REQUEST);
        assert_eq!(reply("[]").unwrap()["error"]["code"], INVALID_REQUEST);
        // method가 없는 요청 객체는 -32601이 아니라 -32600 (읽을 수 있는 id는 돌려준다)
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
        // 배치 안에서도 답할 항목만 답한다
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
        assert_eq!(props, names, "inputSchema와 tools/call이 받는 인자가 같아야 한다");
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
        // 비었거나 생략하면 기본 필드
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
        invalid_params(&call(json!({ "role": "r", "constraints": "하나" })), "constraints");
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
        // 빈 제약은 버리고 앞뒤 공백을 뗀다, null은 생략과 같다
        let v = call(json!({ "role": "r", "constraints": ["  결측치 제외  ", " "], "target": null }));
        let text = text_of(&v);
        assert!(text.contains(">결측치 제외</rule>") && !text.contains("C2"), "{text}");
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
        assert!(text_of(&missing).starts_with("데이터 로드 실패"));
        // 빈 경로는 데이터 없음
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
        s.chars().any(|c| ('가'..='힣').contains(&c))
    }

    /// `--mcp --lang en`: JSON-RPC 오류 메시지는 영어이고 인자 이름·값을 그대로 담는다.
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
        // 기본 언어(한국어)는 예전 문구 그대로
        assert_eq!(msg(&call(json!({ "role": 3 }))), "role 인자는 문자열이어야 합니다");
    }

    /// 영어 모드의 도구 결과: 프롬프트 부분과 tools/list·initialize는 한국어 모드와 같고, lint 줄과 데이터 오류만 영어다.
    #[test]
    fn english_mode_tool_results() {
        use crate::i18n::{Lang, with_lang};
        let args = json!({
            "role": "Analyst",
            "constraints": ["Detect outliers", "You must not detect outliers"],
            "fields": "a,a"
        });
        let (ko, en) = (call(args.clone()), with_lang(Lang::En, || call(args.clone())));
        let split = |v: &Value| {
            let (prompt, rest) = text_of(v).split_once("\n\n--- lint ---\n").unwrap();
            (prompt.to_string(), rest.to_string())
        };
        let ((ko_prompt, ko_lint), (en_prompt, en_lint)) = (split(&ko), split(&en));
        assert_eq!(ko_prompt, en_prompt);
        assert_eq!(ko["result"]["isError"], en["result"]["isError"]);
        assert_eq!(ko_lint.lines().count(), en_lint.lines().count());
        assert_eq!(ko_lint.lines().last(), en_lint.lines().last(), "토큰 줄은 같다");
        assert!(
            en_lint.contains(
                "[WARN] Removed contradicting instruction: \"Detect outliers\" ↔ \"You must not detect outliers\" (kept the later one)"
            ) && en_lint.contains("[WARN] Removed blank/duplicate output schema fields: duplicate \"a\"")
                && en_lint.contains("[INFO] Schema integrity check passed"),
            "{en_lint}"
        );
        assert!(
            !has_hangul(&en_lint) && ko_lint.contains("[WARN] 모순 지시 자동제거"),
            "{en_lint}\n{ko_lint}"
        );
        // 데이터 로드 실패는 isError 결과로, 프로파일러 문구까지 영어다
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
        // 도구 설명(tools/list)과 initialize 결과는 언어와 관계없다
        for line in [
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ] {
            assert_eq!(reply(line), with_lang(Lang::En, || reply(line)), "{line}");
        }
    }
}
