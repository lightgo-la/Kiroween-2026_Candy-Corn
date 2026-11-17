//! stdio MCP 클라이언트. 서버 프로세스를 띄워 initialize → tools/call 순서로 호출한다.
//! 줄 단위(newline-delimited) JSON-RPC 2.0만 지원한다.
//!
//! - stdin 쓰기, stdout 읽기, stderr 읽기를 각각 별도 스레드가 맡아 어느 파이프도 막히지 않는다.
//!   서버가 입력을 읽지 않거나 출력을 쏟아내도 호출한 쪽은 시간 초과로 끝난다.
//! - 서버 출력은 바이트로 읽어 UTF-8이 아닌 로그 줄(CP949 등)이 있어도 멈추지 않는다.
//! - `method`가 없는 메시지만 응답으로 본다. 서버가 보낸 ping에는 빈 결과로 답한다.
//! - Windows: `npx`처럼 확장자 없는 명령은 PATH+PATHEXT(.com/.exe/.bat/.cmd)로 찾고, 콘솔 창 없이 띄우며,
//!   시간 초과·종료 때는 stdin을 닫고 잠깐 기다린 뒤 프로세스 트리 전체를 끝낸다.

use crate::profiler::csv_profiler::looks_utf8;
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// 자식 프로세스에 콘솔 창을 만들지 않는다. GUI 빌드는 자기 콘솔을 떼어 내므로 이 플래그가 없으면
/// 콘솔 프로그램(cmd, npx, node 등)마다 새 창이 뜬다.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 오류 메시지에 붙이는 서버 stderr의 마지막 부분 크기
const STDERR_TAIL_BYTES: usize = 2048;
/// stdin을 닫은 뒤 서버가 스스로 끝나기를 기다리는 시간
const EXIT_GRACE: Duration = Duration::from_millis(500);
/// 서버가 끝난 직후 종료 코드와 stderr를 모으려고 기다리는 시간
const DIAG_WAIT: Duration = Duration::from_millis(300);
/// PATHEXT 가운데 CreateProcess로 실행할 수 있는 확장자 (.js, .vbs, .ps1 등은 바로 실행할 수 없다)
const RUNNABLE_EXTS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

pub struct McpClient {
    child: Child,
    /// stdin 쓰기 스레드로 보낼 줄. None이면 stdin을 닫았다
    writer: Option<Sender<Vec<u8>>>,
    rx: Receiver<String>,
    stderr: StderrTail,
    next_id: u64,
    timeout: Duration,
    /// 프로세스 트리를 이미 정리했다
    stopped: bool,
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
        // 따옴표로 감싼 경로("C:\Program Files\nodejs\npx.cmd")도 받는다
        let command = command.trim().trim_matches('"').trim();
        if command.is_empty() {
            return Err(tr!("MCP command가 비어 있습니다", "The MCP command is empty").into());
        }
        let program = resolve_program(command);
        let mut cmd = Command::new(&program);
        cmd.args(split_args(args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd.spawn().map_err(|e| spawn_error(command, &program, &e))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tr!(
                "MCP 서버 파이프를 만들지 못했습니다",
                "Could not create the pipes for the MCP server"
            )
            .into());
        };
        let mut c = McpClient {
            child,
            writer: Some(spawn_writer(stdin)),
            rx: spawn_line_reader(stdout),
            stderr: StderrTail::spawn(stderr),
            next_id: 1,
            timeout,
            stopped: false,
        };
        // 실패하면 c가 drop되면서 프로세스 트리를 정리한다
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

    /// 한 줄을 쓰기 스레드에 넘긴다. 쓰기 자체는 기다리지 않는다(서버가 안 읽어도 막히지 않는다).
    /// 한 줄을 쓰기 스레드에 넘긴다. 쓰기 자체는 기다리지 않는다(서버가 안 읽어도 막히지 않는다).
    /// stdin이 이미 닫혔으면 false.
    fn push_line(&self, v: &Value) -> bool {
        let mut line = v.to_string().into_bytes();
        line.push(b'\n');
        self.writer.as_ref().is_some_and(|w| w.send(line).is_ok())
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        if self.push_line(v) {
            Ok(())
        } else {
            Err(self.with_stderr(
                tr!(
                    "MCP 쓰기 실패: 서버 입력이 닫혔습니다",
                    "MCP write failed: the server's input is closed"
                )
                .into(),
            ))
        }
    }

    /// 요청을 보내고 같은 id의 응답을 기다린다. `method`가 있는 메시지는 서버가 보낸 요청·알림이라 응답으로 보지 않는다.
    /// JSON이 아닌 줄(로그)과 다른 id의 응답은 건너뛴다. 요청은 한 번에 하나뿐이므로 id가 null인 오류 응답
    /// (서버가 요청을 읽지 못함)은 이 요청의 실패다.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            // 메시지가 쉬지 않고 와도(로그 홍수) 마감 시간은 지킨다
            let now = Instant::now();
            let received = if now >= deadline {
                Err(RecvTimeoutError::Timeout)
            } else {
                self.rx.recv_timeout(deadline - now)
            };
            let line = match received {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    self.stop();
                    let secs = self.timeout.as_secs_f64();
                    return Err(self.with_stderr(trf!(
                        "MCP 응답 시간 초과 ({method}, {secs}초)",
                        "MCP response timed out ({method}, {secs}s)"
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let msg = match wait_exit(&mut self.child, DIAG_WAIT).and_then(|s| s.code()) {
                        Some(code) => trf!(
                            "MCP 서버가 응답 전에 종료됨 ({method}, 종료 코드 {code})",
                            "The MCP server exited before responding ({method}, exit code {code})"
                        ),
                        None => trf!(
                            "MCP 서버가 응답 전에 종료됨 ({method})",
                            "The MCP server exited before responding ({method})"
                        ),
                    };
                    return Err(self.with_stderr(msg));
                }
            };
            let Ok(msg) = serde_json::from_str::<Value>(line.trim().trim_start_matches('\u{feff}')) else {
                continue;
            };
            if let Some(m) = msg.get("method") {
                self.answer_server_request(m, msg.get("id"));
                continue;
            }
            let ours = match msg.get("id") {
                Some(Value::Null) => msg.get("error").is_some(),
                other => other.and_then(Value::as_u64) == Some(id),
            };
            if !ours {
                continue;
            }
            if let Some(err) = msg.get("error") {
                let text = err.get("message").and_then(Value::as_str).unwrap_or("unknown");
                return Err(trf!("MCP 오류: {text}", "MCP error: {text}"));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// 서버가 보낸 요청에 답한다: ping은 빈 결과, 그 밖의 요청(roots/list, sampling 등)은 -32601.
    /// JSON-RPC는 요청마다 응답을 요구하므로, 답하지 않으면 그 응답을 기다리는 서버가 멈출 수 있다.
    /// id가 없는 알림(로그, 진행률)은 무시한다. 답을 못 보내도(서버 입력이 닫힘) 기다리지 않고 넘어간다.
    fn answer_server_request(&self, method: &Value, id: Option<&Value>) {
        let Some(id) = id.filter(|v| v.is_string() || v.is_number()).cloned() else {
            return;
        };
        let reply = if method == "ping" {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            let name = method.as_str().unwrap_or("?");
            let message = trf!("지원하지 않는 요청: {name}", "Unsupported request: {name}");
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": message } })
        };
        let _ = self.push_line(&reply);
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
            Err(trf!("도구 오류: {text}", "Tool error: {text}"))
        } else {
            Ok(text)
        }
    }

    /// 오류 메시지에 서버 stderr의 마지막 부분을 붙인다(서버가 왜 실패했는지 알 수 있게).
    fn with_stderr(&self, msg: String) -> String {
        let tail = self.stderr.text(DIAG_WAIT);
        if tail.is_empty() {
            msg
        } else {
            trf!("{msg}\n[서버 stderr]\n{tail}", "{msg}\n[server stderr]\n{tail}")
        }
    }

    /// stdin을 닫고 잠깐 기다린 뒤에도 살아 있으면 프로세스 트리 전체를 끝낸다.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // 쓰기 스레드가 남은 줄을 쓰고 stdin을 닫는다 → 서버가 입력 끝(EOF)을 보고 스스로 끝날 기회
        self.writer = None;
        if wait_exit(&mut self.child, EXIT_GRACE).is_none() {
            kill_tree(&mut self.child);
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// stdin 쓰기 스레드. 채널이 닫히면(클라이언트 종료) stdin을 닫는다.
fn spawn_writer(mut stdin: ChildStdin) -> Sender<Vec<u8>> {
    let (tx, rx) = channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for line in rx {
            if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                break;
            }
        }
    });
    tx
}

/// stdout 줄 리더. 바이트 단위로 읽고 UTF-8이 아닌 부분은 대체 문자로 바꿔, 로그 줄 하나 때문에 멈추지 않는다.
fn spawn_line_reader(stdout: ChildStdout) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// 서버 stderr의 마지막 부분. 별도 스레드가 계속 읽어 stderr 파이프가 가득 차 서버가 멈추는 일이 없다.
struct StderrTail {
    shared: Arc<Mutex<TailBuf>>,
    reader: JoinHandle<()>,
}

#[derive(Default)]
struct TailBuf {
    bytes: Vec<u8>,
    /// 앞부분을 잘라 냈다
    cut: bool,
}

impl StderrTail {
    fn spawn(mut stderr: ChildStderr) -> Self {
        let shared = Arc::new(Mutex::new(TailBuf::default()));
        let sink = Arc::clone(&shared);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                let mut t = sink.lock().unwrap_or_else(PoisonError::into_inner);
                t.bytes.extend_from_slice(&chunk[..n]);
                if t.bytes.len() > STDERR_TAIL_BYTES {
                    let extra = t.bytes.len() - STDERR_TAIL_BYTES;
                    t.bytes.drain(..extra);
                    t.cut = true;
                }
            }
        });
        StderrTail { shared, reader }
    }

    /// stderr가 닫힐 때까지 최대 `wait` 기다린 뒤 마지막 부분을 글자로 돌려준다.
    fn text(&self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        while !self.reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        decode_tail(&t.bytes, t.cut)
    }
}

/// stderr 바이트를 글자로. UTF-8이 아니면 CP949(한국어 Windows 콘솔의 기본 코드 페이지)로 읽는다.
fn decode_tail(bytes: &[u8], cut: bool) -> String {
    // 앞을 잘라 냈으면 UTF-8 글자 중간에서 시작할 수 있으므로 이어지는 바이트(10xxxxxx)를 버린다
    let skip = if cut {
        bytes.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count()
    } else {
        0
    };
    let bytes = &bytes[skip..];
    let text = if looks_utf8(bytes) {
        String::from_utf8_lossy(bytes)
    } else {
        encoding_rs::EUC_KR.decode_without_bom_handling(bytes).0
    };
    text.trim().to_string()
}

/// 끝날 때까지 최대 `wait` 기다린다. 끝났으면 종료 상태.
fn wait_exit(child: &mut Child, wait: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// 서버와 그 자식들(cmd → npx → node 등)을 모두 끝낸다. taskkill이 실패하면 직접 띄운 프로세스만이라도 끝낸다.
/// `child` 핸들을 쥐고 있는 동안에는 Windows가 PID를 다른 프로세스에 다시 주지 않으므로 엉뚱한 프로세스를 끝낼 일은 없다.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let taskkill = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("System32").join("taskkill.exe"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("taskkill.exe"));
        let killed = Command::new(taskkill)
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .is_ok_and(|s| s.success());
        if killed && wait_exit(child, Duration::from_secs(2)).is_some() {
            return;
        }
    }
    let _ = child.kill();
    let _ = wait_exit(child, Duration::from_secs(2));
}

/// 명령 이름을 실행할 경로로 바꾼다. 표준 라이브러리는 확장자 없는 이름에 .exe만 붙여 찾으므로
/// `npx`(실제 파일은 npx.cmd) 같은 명령을 못 찾는다. 직접 찾지 못하면 원래 이름을 그대로 쓴다.
fn resolve_program(command: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let path = std::env::var_os("PATH");
        let pathext = std::env::var_os("PATHEXT");
        if let Some(p) = find_program(command, path.as_deref(), pathext.as_deref()) {
            return p;
        }
    }
    PathBuf::from(command)
}

/// PATH의 각 폴더(경로가 들어 있으면 그 위치만)에서 실행 파일을 찾는다. 확장자가 없으면 PATHEXT 순서대로
/// .com/.exe/.bat/.cmd를 붙여 본다(PATHEXT가 없으면 이 순서). 빈 PATH 항목(현재 폴더)은 보지 않는다.
#[cfg_attr(not(windows), allow(dead_code))]
fn find_program(command: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let mut exts: Vec<String> = pathext
        .map(|p| {
            p.to_string_lossy()
                .split(';')
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| RUNNABLE_EXTS.contains(&e.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if exts.is_empty() {
        exts = RUNNABLE_EXTS.iter().map(|e| e.to_string()).collect();
    }
    let given = Path::new(command);
    let has_ext = given.extension().is_some_and(|e| {
        let e = format!(".{}", e.to_string_lossy().to_ascii_lowercase());
        RUNNABLE_EXTS.contains(&e.as_str())
    });
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        if has_ext {
            return vec![base];
        }
        exts.iter()
            .map(|e| {
                let mut s = base.clone().into_os_string();
                s.push(e);
                PathBuf::from(s)
            })
            .collect()
    };
    if given.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        return candidates(given.to_path_buf()).into_iter().find(|p| p.is_file());
    }
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates(dir.join(command)))
        .find(|p| p.is_file())
}

fn spawn_error(command: &str, program: &Path, e: &std::io::Error) -> String {
    let mut msg = trf!(
        "MCP 서버 실행 실패 ({command}): {e}",
        "Failed to start the MCP server ({command}): {e}"
    );
    if e.kind() == std::io::ErrorKind::NotFound && program == Path::new(command) {
        msg += tr!(
            "\nPATH에서 실행 파일(.exe/.cmd/.bat/.com)을 찾지 못했습니다. 전체 경로를 쓰거나 PATH를 확인하세요",
            "\nNo executable (.exe/.cmd/.bat/.com) was found on PATH. Use the full path or check PATH"
        );
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    /// 이 테스트 실행 파일을 가짜 MCP 서버로 다시 띄울 때 쓰는 테스트 이름
    const FAKE_TEST: &str = "canvas::mcp_client::tests::fake_server";

    #[test]
    fn splits_quoted_args() {
        assert_eq!(
            split_args(r#"/c npx -y "my pkg" ""#),
            vec!["/c", "npx", "-y", "my pkg", ""]
        );
        assert!(split_args("   ").is_empty());
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cc_mcpc_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 가짜 MCP 서버. 이 테스트 실행 파일을 `cc-fake-mode=<모드>` 인자와 함께 다시 띄우면 서버로 동작한다.
    /// 보통의 `cargo test`에서는 그 인자가 없으므로 아무것도 하지 않는다.
    #[test]
    fn fake_server() {
        let arg = |key: &str| std::env::args().find_map(|a| a.strip_prefix(key).map(String::from));
        let Some(mode) = arg("cc-fake-mode=") else {
            return;
        };
        let file = arg("cc-fake-file=").map(PathBuf::from);
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let mut out = std::io::stdout().lock();
        let mut recv = || -> Option<Value> {
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(serde_json::from_str(&line).unwrap_or(Value::Null)),
            }
        };
        let mut emit = |bytes: &[u8]| {
            out.write_all(bytes).unwrap();
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
        };
        // 테스트 하네스가 같은 줄에 찍어 둔 "test ... " 뒤에 붙지 않게 줄을 먼저 끝낸다
        emit(b"");
        let result = |id: &Value, result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
        let init_result = json!({
            "protocolVersion": "2024-11-05", "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake", "version": "0" }
        });
        match mode.as_str() {
            "ping_trap" => {
                let init = recv().unwrap();
                // CP949 로그 줄(UTF-8 아님), 클라이언트 요청과 같은 id의 ping, 다른 요청, 알림
                emit(&encoding_rs::EUC_KR.encode("로그: 서버 시작").0);
                emit(
                    json!({ "jsonrpc": "2.0", "id": init["id"], "method": "ping" })
                        .to_string()
                        .as_bytes(),
                );
                emit(br#"{"jsonrpc":"2.0","id":"r1","method":"roots/list"}"#);
                emit(br#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"x"}}"#);
                let (pong, roots) = (recv().unwrap(), recv().unwrap());
                let ping_ok = pong["id"] == init["id"] && pong["result"] == json!({});
                let roots_ok = roots["id"] == "r1" && roots["error"]["code"] == -32601;
                emit(result(&init["id"], init_result).as_bytes());
                let note = recv().unwrap();
                let call = recv().unwrap();
                let text = format!(
                    "ping_ok={ping_ok} roots_ok={roots_ok} notified={} args={}",
                    note["method"] == "notifications/initialized",
                    call["params"]["arguments"]
                );
                emit(result(&call["id"], json!({ "content": [{ "type": "text", "text": text }] })).as_bytes());
                while recv().is_some() {}
            }
            "crash" => {
                let mut err = std::io::stderr();
                err.write_all(&encoding_rs::EUC_KR.encode("치명적 오류: 설정 파일 없음\r\n").0)
                    .unwrap();
                err.flush().unwrap();
                std::process::exit(3);
            }
            "hang" => {
                // 손자 프로세스(60초 잠)를 하나 띄우고 응답하지 않는다
                let me = std::env::current_exe().unwrap();
                let mut grandchild = Command::new(me)
                    .args(["--exact", FAKE_TEST, "--nocapture", "cc-fake-mode=sleep"])
                    .spawn()
                    .unwrap();
                std::fs::write(file.unwrap(), grandchild.id().to_string()).unwrap();
                let _ = grandchild.wait();
            }
            "sleep" => std::thread::sleep(Duration::from_secs(60)),
            "null_error" => {
                // 요청을 읽지 못한 서버처럼 id가 null인 오류로 답한다
                let _ = recv();
                emit(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#);
                while recv().is_some() {}
            }
            "basic" => {
                while let Some(msg) = recv() {
                    match msg["method"].as_str() {
                        Some("initialize") => emit(result(&msg["id"], init_result.clone()).as_bytes()),
                        // 도구 이름이 "fail"이면 도구 오류(isError)로 답한다
                        Some("tools/call") if msg["params"]["name"] == "fail" => {
                            let body = json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] });
                            emit(result(&msg["id"], body).as_bytes());
                        }
                        Some("tools/call") => {
                            let text = format!("basic:{}", msg["params"]["arguments"]);
                            emit(
                                result(&msg["id"], json!({ "content": [{ "type": "text", "text": text }] })).as_bytes(),
                            );
                        }
                        _ => {}
                    }
                }
                // 입력 끝(EOF)을 보고 스스로 끝났다는 표시
                if let Some(f) = file {
                    std::fs::write(f, "eof").unwrap();
                }
            }
            other => panic!("알 수 없는 가짜 서버 모드: {other}"),
        }
    }

    fn fake(mode: &str, file: Option<&Path>, timeout: Duration) -> Result<McpClient, String> {
        let exe = std::env::current_exe().unwrap();
        let mut args = format!("--exact {FAKE_TEST} --nocapture --test-threads=1 -q cc-fake-mode={mode}");
        if let Some(f) = file {
            args += &format!(" \"cc-fake-file={}\"", f.display());
        }
        McpClient::spawn(&exe.display().to_string(), &args, timeout)
    }

    #[cfg(windows)]
    fn alive(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }

    #[test]
    fn answers_server_ping_and_survives_cp949_log_line() {
        let mut c = fake("ping_trap", None, Duration::from_secs(20)).unwrap();
        let text = c.call_tool("echo", json!({ "q": "한글" })).unwrap();
        assert!(text.contains("ping_ok=true"), "{text}");
        assert!(
            text.contains("roots_ok=true") && text.contains("notified=true"),
            "{text}"
        );
        assert!(text.contains(r#"args={"q":"한글"}"#), "{text}");
    }

    #[test]
    fn crash_reports_exit_code_and_stderr_tail() {
        let err = fake("crash", None, Duration::from_secs(20)).err().unwrap();
        assert!(err.contains("응답 전에 종료됨 (initialize, 종료 코드 3)"), "{err}");
        assert!(err.contains("[서버 stderr]\n치명적 오류: 설정 파일 없음"), "{err}");
    }

    #[test]
    fn null_id_error_fails_the_pending_request_at_once() {
        let started = Instant::now();
        let err = fake("null_error", None, Duration::from_secs(20)).err().unwrap();
        assert_eq!(err, "MCP 오류: parse error");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "시간 초과까지 기다리면 안 된다"
        );
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_whole_process_tree() {
        let dir = tmp_dir("tree");
        let pid_file = dir.join("pid.txt");
        let started = Instant::now();
        let err = fake("hang", Some(&pid_file), Duration::from_secs(4)).err().unwrap();
        assert!(err.contains("MCP 응답 시간 초과 (initialize, 4초)"), "{err}");
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive(pid), "손자 프로세스 {pid}가 남아 있음");
        assert!(started.elapsed() < Duration::from_secs(20));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn cmd_wrapper_found_without_extension_and_closed_by_eof() {
        let dir = tmp_dir("cmd");
        let marker = dir.join("eof.txt");
        let exe = std::env::current_exe().unwrap();
        std::fs::write(
            dir.join("fake-mcp.cmd"),
            format!(
                "@\"{}\" --exact {FAKE_TEST} --nocapture -q cc-fake-mode=basic \"cc-fake-file={}\"\r\n",
                exe.display(),
                marker.display()
            ),
        )
        .unwrap();
        // 확장자 없이 넘겨도 .cmd를 찾아 cmd.exe로 실행한다 (npx → npx.cmd와 같은 경우)
        let quoted = format!("\"{}\"", dir.join("fake-mcp").display());
        let mut c = McpClient::spawn(&quoted, "", Duration::from_secs(20)).unwrap();
        assert_eq!(c.call_tool("echo", json!({ "n": 1 })).unwrap(), r#"basic:{"n":1}"#);
        let t = Instant::now();
        drop(c);
        assert!(marker.is_file(), "stdin을 닫으면 서버가 EOF를 보고 스스로 끝나야 한다");
        assert!(t.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_command_explains_path_lookup() {
        let err = McpClient::spawn("cc-no-such-mcp-server", "", Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(err.contains("MCP 서버 실행 실패 (cc-no-such-mcp-server)"), "{err}");
        assert!(err.contains("PATH"), "{err}");
        assert!(McpClient::spawn("  ", "", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn finds_cmd_and_exe_via_pathext() {
        let d = tmp_dir("pathext");
        for f in ["tool.cmd", "both.cmd", "both.exe", "plain", "script.js"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        // 빈 항목(현재 폴더)은 건너뛴다
        let path = std::env::join_paths([PathBuf::new(), d.clone()]).unwrap();
        let ext = OsStr::new(".COM;.EXE;.BAT;.CMD;.VBS;.JS");
        let find = |c: &str| find_program(c, Some(&path), Some(ext));
        assert_eq!(find("tool"), Some(d.join("tool.cmd")));
        assert_eq!(find("tool.cmd"), Some(d.join("tool.cmd")));
        assert_eq!(find("both"), Some(d.join("both.exe")), "PATHEXT 순서");
        assert_eq!(
            find_program("both", Some(&path), Some(OsStr::new(".CMD;.EXE"))),
            Some(d.join("both.cmd"))
        );
        assert_eq!(find_program("tool", Some(&path), None), Some(d.join("tool.cmd")));
        assert_eq!(find("plain"), None, "확장자 없는 파일(sh 스크립트 등)은 실행할 수 없다");
        assert_eq!(find("script"), None, ".js는 CreateProcess로 실행할 수 없다");
        assert_eq!(find("missing"), None);
        // 경로가 들어 있으면 PATH 대신 그 위치에서 찾는다
        let full = d.join("tool").display().to_string();
        assert_eq!(find_program(&full, None, Some(ext)), Some(d.join("tool.cmd")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stderr_tail_decodes_cp949_and_cut_utf8() {
        let (cp, _, _) = encoding_rs::EUC_KR.encode("오류: 설정 파일 없음\r\n");
        assert_eq!(decode_tail(&cp, false), "오류: 설정 파일 없음");
        let utf8 = "가나다 error".as_bytes();
        assert_eq!(decode_tail(&utf8[1..], true), "나다 error", "잘린 첫 글자는 버린다");
        assert_eq!(decode_tail(b"  plain\n", false), "plain");
    }

    #[test]
    fn tool_errors_are_reported_with_their_text() {
        let mut c = fake("basic", None, Duration::from_secs(20)).unwrap();
        assert_eq!(c.call_tool("fail", json!({})).unwrap_err(), "도구 오류: boom");
        let en = with_lang(Lang::En, || c.call_tool("fail", json!({})));
        assert_eq!(en.unwrap_err(), "Tool error: boom");
        assert_eq!(c.call_tool("echo", json!({ "n": 1 })).unwrap(), r#"basic:{"n":1}"#);
    }

    #[test]
    fn client_errors_in_english() {
        with_lang(Lang::En, || {
            let err = McpClient::spawn("  ", "", Duration::from_secs(1)).err().unwrap();
            assert_eq!(err, "The MCP command is empty");
            let err = McpClient::spawn("cc-no-such-mcp-server", "", Duration::from_secs(1))
                .err()
                .unwrap();
            assert!(
                err.starts_with("Failed to start the MCP server (cc-no-such-mcp-server): "),
                "{err}"
            );
            assert!(
                err.ends_with(
                    "\nNo executable (.exe/.cmd/.bat/.com) was found on PATH. Use the full path or check PATH"
                ),
                "{err}"
            );
            let err = fake("crash", None, Duration::from_secs(20)).err().unwrap();
            assert!(
                err.contains("The MCP server exited before responding (initialize, exit code 3)"),
                "{err}"
            );
            assert!(
                err.contains("[server stderr]\n치명적 오류: 설정 파일 없음"),
                "서버가 쓴 글자는 그대로: {err}"
            );
            let err = fake("null_error", None, Duration::from_secs(20)).err().unwrap();
            assert_eq!(err, "MCP error: parse error");
        });
    }
}
