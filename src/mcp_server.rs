//! (보너스) stdio JSON-RPC 2.0 MCP 서버. 줄 단위(newline-delimited) JSON 메시지를 처리한다.
//! 제공 도구: `compress_prompt`

use crate::ast::{OutputFormat, TargetModel};
use crate::pipeline::{self, CompileRequest};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

pub fn run() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim_start_matches('\u{feff}').to_string(); // 일부 셸이 붙이는 BOM 제거
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_json(&mut out, &error(Value::Null, -32700, &format!("parse error: {e}")))?;
                continue;
            }
        };
        // JSON-RPC 배치(배열)도 지원한다. 응답이 없는 notification은 건너뛴다.
        let reply = match msg {
            Value::Array(items) if items.is_empty() => Some(error(Value::Null, -32600, "empty batch")),
            Value::Array(items) => {
                let replies: Vec<Value> = items.iter().filter_map(handle).collect();
                if replies.is_empty() {
                    None
                } else {
                    Some(Value::Array(replies))
                }
            }
            single => handle(&single),
        };
        if let Some(reply) = reply {
            write_json(&mut out, &reply)?;
        }
    }
    Ok(())
}

/// 요청 하나를 처리한다. id가 없는 notification이면 None.
fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    Some(match method {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "sugar-coat-studio", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": [tool_def()] })),
        "tools/call" => call_tool(id, msg.get("params").unwrap_or(&Value::Null)),
        _ => error(id, -32601, "method not found"),
    })
}
fn write_json(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    writeln!(out, "{v}")?;
    out.flush()
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_def() -> Value {
    json!({
        "name": "compress_prompt",
        "description": "Lint and compress a prompt (role, constraints, optional CSV/JSON data profile) into a model-specific structured prompt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "role": { "type": "string" },
                "constraints": { "type": "array", "items": { "type": "string" } },
                "target": { "type": "string", "enum": ["claude", "gpt", "gemini"] },
                "format": { "type": "string", "enum": ["json", "table"] },
                "fields": { "type": "array", "items": { "type": "string" } },
                "data_path": { "type": "string", "description": "Local CSV/JSON/NDJSON file" }
            },
            "required": ["role"]
        }
    })
}

fn call_tool(id: Value, params: &Value) -> Value {
    if params.get("name").and_then(Value::as_str) != Some("compress_prompt") {
        return error(id, -32602, "unknown tool");
    }
    let args = params.get("arguments").unwrap_or(&Value::Null);
    let strings = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let data = match args.get("data_path").and_then(Value::as_str) {
        Some(p) => match pipeline::load_data(Path::new(p), 8) {
            Ok(d) => Some(d),
            Err(e) => return ok(id, tool_text(&format!("data load failed: {e}"), true)),
        },
        None => None,
    };
    let mut fields = strings("fields");
    if fields.is_empty() {
        fields = pipeline::parse_fields(pipeline::DEFAULT_FIELDS);
    }
    let req = CompileRequest {
        target: match args.get("target").and_then(Value::as_str) {
            Some("gpt") => TargetModel::Gpt,
            Some("gemini") => TargetModel::Gemini,
            _ => TargetModel::Claude,
        },
        role: args.get("role").and_then(Value::as_str).unwrap_or("").to_string(),
        constraints: strings("constraints"),
        format: match args.get("format").and_then(Value::as_str) {
            Some("table") => OutputFormat::MarkdownTable,
            _ => OutputFormat::StrictJson,
        },
        fields,
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
        "{}\n\n--- lint ---\n{}\n--- tokens: {} -> {} ({:.1}%) ---",
        r.output,
        lint.join("\n"),
        r.orig_tokens,
        r.comp_tokens,
        -r.saving_pct
    );
    let is_error = r.ast.lint_warnings.iter().any(|d| d.severity == "ERROR");
    ok(id, tool_text(&text, is_error))
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}
