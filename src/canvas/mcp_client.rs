//! stdio MCP client. Starts the server process and calls initialize → tools/call in that order.
//! Only newline-delimited JSON-RPC 2.0 is supported.
//!
//! - Writing stdin, reading stdout and reading stderr each have their own thread, so no pipe blocks.
//!   Even if the server does not read its input or floods its output, the caller ends with a timeout.
//! - Server output is read as bytes, so log lines that are not UTF-8 (CP949 etc.) do not stall it.
//! - Only messages without `method` count as responses. Pings sent by the server get an empty result.
//! - Windows: commands without an extension such as `npx` are found via PATH+PATHEXT (.com/.exe/.bat/.cmd) and
//!   started without a console window; on timeout or shutdown, stdin is closed and, after a short wait, the whole
//!   process tree is ended.

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

/// Do not create a console window for child processes. The GUI build detaches its own console, so without this
/// flag every console program (cmd, npx, node, ...) would open a new window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Size of the end of the server's stderr that is appended to error messages
const STDERR_TAIL_BYTES: usize = 2048;
/// How long to wait for the server to exit by itself after stdin is closed
const EXIT_GRACE: Duration = Duration::from_millis(500);
/// How long to wait right after the server ends to collect its exit code and stderr
const DIAG_WAIT: Duration = Duration::from_millis(300);
/// PATHEXT extensions that CreateProcess can run (.js, .vbs, .ps1 etc. cannot be run directly)
const RUNNABLE_EXTS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

pub struct McpClient {
    child: Child,
    /// Lines to send to the stdin writer thread. None once stdin is closed
    writer: Option<Sender<Vec<u8>>>,
    rx: Receiver<String>,
    stderr: StderrTail,
    next_id: u64,
    timeout: Duration,
    /// The process tree has already been cleaned up
    stopped: bool,
}

/// Splits a whitespace-separated argument string. A span in double quotes counts as one argument.
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
        // Quoted paths ("C:\Program Files\nodejs\npx.cmd") are accepted too
        let command = command.trim().trim_matches('"').trim();
        if command.is_empty() {
            return Err(tr!("The MCP command is empty").into());
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
            return Err(tr!("Could not create the pipes for the MCP server").into());
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
        // On failure, dropping c cleans up the process tree
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

    /// Hands one line to the writer thread. It does not wait for the write itself (so it does not block even if the
    /// server does not read).
    /// false if stdin is already closed.
    fn push_line(&self, v: &Value) -> bool {
        let mut line = v.to_string().into_bytes();
        line.push(b'\n');
        self.writer.as_ref().is_some_and(|w| w.send(line).is_ok())
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        if self.push_line(v) {
            Ok(())
        } else {
            Err(self.with_stderr(tr!("MCP write failed: the server's input is closed").into()))
        }
    }

    /// Sends a request and waits for the response with the same id. Messages with `method` are requests or
    /// notifications sent by the server, so they are not taken as responses.
    /// Lines that are not JSON (logs) and responses with another id are skipped. Only one request is pending at a
    /// time, so an error response with a null id (the server could not read the request) fails this request.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            // Keep the deadline even if messages keep coming (a log flood)
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
                        "MCP response timed out ({method}, {secs}s)",
                        method = method,
                        secs = secs
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let msg = match wait_exit(&mut self.child, DIAG_WAIT).and_then(|s| s.code()) {
                        Some(code) => trf!(
                            "The MCP server exited before responding ({method}, exit code {code})",
                            method = method,
                            code = code
                        ),
                        None => trf!("The MCP server exited before responding ({method})", method = method),
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
                return Err(trf!("MCP error: {text}", text = text));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Answers a request sent by the server: ping gets an empty result, any other request (roots/list, sampling,
    /// ...) gets -32601.
    /// JSON-RPC requires a response to every request, so without an answer a server waiting for it could hang.
    /// Notifications without an id (logs, progress) are ignored. If the answer cannot be sent (the server's input is
    /// closed), it moves on without waiting.
    fn answer_server_request(&self, method: &Value, id: Option<&Value>) {
        let Some(id) = id.filter(|v| v.is_string() || v.is_number()).cloned() else {
            return;
        };
        let reply = if method == "ping" {
            json!({ "jsonrpc": "2.0", "id": id, "result": {} })
        } else {
            let name = method.as_str().unwrap_or("?");
            let message = trf!("Unsupported request: {name}", name = name);
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
            Err(trf!("Tool error: {text}", text = text))
        } else {
            Ok(text)
        }
    }

    /// Appends the end of the server's stderr to an error message (so it shows why the server failed).
    fn with_stderr(&self, msg: String) -> String {
        let tail = self.stderr.text(DIAG_WAIT);
        if tail.is_empty() {
            msg
        } else {
            trf!("{msg}\n[server stderr]\n{tail}", msg = msg, tail = tail)
        }
    }

    /// Closes stdin and, if the server is still alive after a short wait, ends the whole process tree.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // The writer thread writes the remaining lines and closes stdin → a chance for the server to see the end of
        // its input (EOF) and exit by itself
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

/// Stdin writer thread. Closes stdin when the channel closes (the client is done).
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

/// Stdout line reader. Reads bytes and replaces parts that are not UTF-8 with the replacement character, so a single
/// log line cannot stall it.
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

/// The end of the server's stderr. A separate thread keeps reading, so the server never stalls on a full stderr
/// pipe.
struct StderrTail {
    shared: Arc<Mutex<TailBuf>>,
    reader: JoinHandle<()>,
}

#[derive(Default)]
struct TailBuf {
    bytes: Vec<u8>,
    /// The beginning was cut off
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

    /// Waits up to `wait` for stderr to close, then returns its end as text.
    fn text(&self, wait: Duration) -> String {
        let deadline = Instant::now() + wait;
        while !self.reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let t = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        decode_tail(&t.bytes, t.cut)
    }
}

/// Stderr bytes as text. Bytes that are not UTF-8 are read as CP949 (the default code page of Korean Windows
/// consoles).
fn decode_tail(bytes: &[u8], cut: bool) -> String {
    // If the beginning was cut off, it may start in the middle of a UTF-8 character, so drop continuation bytes
    // (10xxxxxx)
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

/// Waits up to `wait` for the process to end. The exit status if it ended.
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

/// Ends the server and all its children (cmd → npx → node, ...). If taskkill fails, at least the process started
/// directly is ended.
/// While the `child` handle is held, Windows does not give the PID to another process, so the wrong process is never
/// ended.
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

/// Turns a command name into the path to run. The standard library only tries .exe for a name without an
/// extension, so it cannot find commands such as `npx` (the actual file is npx.cmd). If our own lookup finds
/// nothing, the original name is used as is.
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

/// Looks for the executable in each PATH folder (only in the given location if the command contains a path).
/// Without an extension, .com/.exe/.bat/.cmd are tried in PATHEXT order (in this order if PATHEXT is not set).
/// Empty PATH entries (the current folder) are not searched.
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
        "Failed to start the MCP server ({command}): {e}",
        command = command,
        e = e
    );
    if e.kind() == std::io::ErrorKind::NotFound && program == Path::new(command) {
        msg += tr!("\nNo executable (.exe/.cmd/.bat/.com) was found on PATH. Use the full path or check PATH");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    /// Name of the test used to start this test executable again as a fake MCP server
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

    /// Fake MCP server. Started again with a `cc-fake-mode=<mode>` argument, this test executable acts as a server.
    /// A normal `cargo test` run has no such argument, so it does nothing.
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
        // End the line first so the output does not stick to the "test ... " the test harness printed on that line
        emit(b"");
        let result = |id: &Value, result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
        let init_result = json!({
            "protocolVersion": "2024-11-05", "capabilities": { "tools": {} },
            "serverInfo": { "name": "fake", "version": "0" }
        });
        match mode.as_str() {
            "ping_trap" => {
                let init = recv().unwrap();
                // A CP949 log line (not UTF-8), a ping with the same id as the client's request, another request,
                // a notification
                emit(&encoding_rs::EUC_KR.encode(ko_fixture!("graph.mcp.cp949_log_line")).0);
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
                let line = format!("{}\r\n", ko_fixture!("graph.mcp.crash_stderr"));
                err.write_all(&encoding_rs::EUC_KR.encode(&line).0).unwrap();
                err.flush().unwrap();
                std::process::exit(3);
            }
            "hang" => {
                // Start one grandchild process (it sleeps for 60 s) and never respond
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
                // Answer with a null-id error, like a server that could not read the request
                let _ = recv();
                emit(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#);
                while recv().is_some() {}
            }
            "basic" => {
                while let Some(msg) = recv() {
                    match msg["method"].as_str() {
                        Some("initialize") => emit(result(&msg["id"], init_result.clone()).as_bytes()),
                        // If the tool name is "fail", answer with a tool error (isError)
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
                // Marker showing that the server saw the end of its input (EOF) and exited by itself
                if let Some(f) = file {
                    std::fs::write(f, "eof").unwrap();
                }
            }
            other => panic!("unknown fake server mode: {other}"),
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
        let arg = ko_fixture!("graph.mcp.non_ascii_arg");
        let text = c.call_tool("echo", json!({ "q": arg })).unwrap();
        assert!(text.contains("ping_ok=true"), "{text}");
        assert!(
            text.contains("roots_ok=true") && text.contains("notified=true"),
            "{text}"
        );
        assert!(text.contains(&format!(r#"args={{"q":"{arg}"}}"#)), "{text}");
    }

    #[test]
    fn crash_reports_exit_code_and_stderr_tail() {
        with_lang(Lang::Ko, || {
            let err = fake("crash", None, Duration::from_secs(20)).err().unwrap();
            let exited = format!(
                ko_text!("The MCP server exited before responding ({method}, exit code {code})"),
                method = "initialize",
                code = 3
            );
            assert!(err.contains(&exited), "{err}");
            let tail = format!(
                ko_text!("{msg}\n[server stderr]\n{tail}"),
                msg = "",
                tail = ko_fixture!("graph.mcp.crash_stderr")
            );
            assert!(err.contains(&tail), "{err}");
        });
    }

    #[test]
    fn null_id_error_fails_the_pending_request_at_once() {
        let started = Instant::now();
        let err = with_lang(Lang::Ko, || {
            fake("null_error", None, Duration::from_secs(20)).err().unwrap()
        });
        assert_eq!(err, format!(ko_text!("MCP error: {text}"), text = "parse error"));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it must not wait for the timeout"
        );
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_whole_process_tree() {
        let dir = tmp_dir("tree");
        let pid_file = dir.join("pid.txt");
        let started = Instant::now();
        let err = fake("hang", Some(&pid_file), Duration::from_secs(4)).err().unwrap();
        assert!(err.contains("MCP response timed out (initialize, 4s)"), "{err}");
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive(pid), "grandchild process {pid} is still alive");
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
        // Even without an extension the .cmd is found and run through cmd.exe (the same case as npx → npx.cmd)
        let quoted = format!("\"{}\"", dir.join("fake-mcp").display());
        let mut c = McpClient::spawn(&quoted, "", Duration::from_secs(20)).unwrap();
        assert_eq!(c.call_tool("echo", json!({ "n": 1 })).unwrap(), r#"basic:{"n":1}"#);
        let t = Instant::now();
        drop(c);
        assert!(
            marker.is_file(),
            "after stdin is closed the server must see EOF and exit by itself"
        );
        assert!(t.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_command_explains_path_lookup() {
        let err = with_lang(Lang::Ko, || {
            McpClient::spawn("cc-no-such-mcp-server", "", Duration::from_secs(1))
                .err()
                .unwrap()
        });
        let failed = format!(
            ko_text!("Failed to start the MCP server ({command}): {e}"),
            command = "cc-no-such-mcp-server",
            e = ""
        );
        assert!(err.starts_with(&failed), "{err}");
        assert!(err.contains("PATH"), "{err}");
        assert!(McpClient::spawn("  ", "", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn finds_cmd_and_exe_via_pathext() {
        let d = tmp_dir("pathext");
        for f in ["tool.cmd", "both.cmd", "both.exe", "plain", "script.js"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        // Empty entries (the current folder) are skipped
        let path = std::env::join_paths([PathBuf::new(), d.clone()]).unwrap();
        let ext = OsStr::new(".COM;.EXE;.BAT;.CMD;.VBS;.JS");
        let find = |c: &str| find_program(c, Some(&path), Some(ext));
        assert_eq!(find("tool"), Some(d.join("tool.cmd")));
        assert_eq!(find("tool.cmd"), Some(d.join("tool.cmd")));
        assert_eq!(find("both"), Some(d.join("both.exe")), "PATHEXT order");
        assert_eq!(
            find_program("both", Some(&path), Some(OsStr::new(".CMD;.EXE"))),
            Some(d.join("both.cmd"))
        );
        assert_eq!(find_program("tool", Some(&path), None), Some(d.join("tool.cmd")));
        assert_eq!(
            find("plain"),
            None,
            "files without an extension (sh scripts etc.) cannot be run"
        );
        assert_eq!(find("script"), None, ".js cannot be run with CreateProcess");
        assert_eq!(find("missing"), None);
        // A command that contains a path is looked up there instead of on PATH
        let full = d.join("tool").display().to_string();
        assert_eq!(find_program(&full, None, Some(ext)), Some(d.join("tool.cmd")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stderr_tail_decodes_cp949_and_cut_utf8() {
        let error = ko_fixture!("graph.mcp.cp949_error");
        let line = format!("{error}\r\n");
        let (cp, _, _) = encoding_rs::EUC_KR.encode(&line);
        assert_eq!(decode_tail(&cp, false), error);
        let utf8 = ko_fixture!("graph.mcp.cut_utf8").as_bytes();
        assert_eq!(
            decode_tail(&utf8[1..], true),
            ko_fixture!("graph.mcp.cut_utf8_tail"),
            "the cut first character is dropped"
        );
        assert_eq!(decode_tail(b"  plain\n", false), "plain");
    }

    #[test]
    fn tool_errors_are_reported_with_their_text() {
        let mut c = fake("basic", None, Duration::from_secs(20)).unwrap();
        let ko = with_lang(Lang::Ko, || c.call_tool("fail", json!({})));
        assert_eq!(ko.unwrap_err(), format!(ko_text!("Tool error: {text}"), text = "boom"));
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
            let tail = format!("[server stderr]\n{}", ko_fixture!("graph.mcp.crash_stderr"));
            assert!(err.contains(&tail), "the server's own text stays as is: {err}");
            let err = fake("null_error", None, Duration::from_secs(20)).err().unwrap();
            assert_eq!(err, "MCP error: parse error");
        });
    }
}
