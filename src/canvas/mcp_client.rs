//! stdio MCP 클라이언트. 서버 프로세스를 띄워 initialize → tools/call 순서로 호출한다.
//! 줄 단위(newline-delimited) JSON-RPC 2.0만 지원한다.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: u64,
    timeout: Duration,
}

/// 공백 구분 인자 문자열을 나눈다. 큰따옴표로 감싼 구간은 하나의 인자로 취급한다.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

impl McpClient {
    pub fn spawn(command: &str, args: &str, timeout: Duration) -> Result<Self, String> {
        if command.trim().is_empty() {
            return Err("MCP command가 비어 있습니다".into());
        }
        let mut child = Command::new(command.trim())
            .args(split_args(args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("MCP 서버 실행 실패 ({command}): {e}"))?;
        let stdin = child.stdin.take().ok_or("stdin 확보 실패")?;
        let stdout = child.stdout.take().ok_or("stdout 확보 실패")?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut c = McpClient {
            child,
            stdin,
            rx,
            next_id: 1,
            timeout,
        };
        c.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "candy-corn-canvas", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        c.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(c)
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{v}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("MCP 쓰기 실패: {e}"))
    }

    /// 요청을 보내고 같은 id의 응답을 기다린다. 알림/로그 등 다른 메시지는 무시한다.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .rx
                .recv_timeout(left)
                .map_err(|_| format!("MCP 응답 시간 초과 또는 서버 종료 ({method})"))?;
            let Ok(msg) = serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}')) else {
                continue;
            };
            if msg.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(format!("MCP 오류: {}", err["message"].as_str().unwrap_or("unknown")));
            }
            return Ok(msg["result"].clone());
        }
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, String> {
        let r = self.request("tools/call", json!({ "name": name, "arguments": arguments }))?;
        let text = r["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if r["isError"].as_bool().unwrap_or(false) {
            Err(format!("도구 오류: {text}"))
        } else {
            Ok(text)
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_args() {
        assert_eq!(
            split_args(r#"/c npx -y "my pkg" ""#),
            vec!["/c", "npx", "-y", "my pkg", ""]
        );
        assert!(split_args("   ").is_empty());
    }
}
