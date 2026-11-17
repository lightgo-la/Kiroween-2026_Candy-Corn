//! 시나리오 테스트 29개: 입력 파일 준비 → 실제 실행 파일 실행 → 출력 검증까지 한 번에 확인한다.
//! 블루프린트 CLI, 데이터 경로 해석, 린트, 인코딩·구분자·바이너리 데이터, MCP 서버 경로를 다룬다. `cargo test`에 포함된다.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_Candy-Corn");
const GRAPH_SAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json");

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// 시나리오마다 독립된 작업 폴더 (병렬 실행 대비). 테스트가 끝나면 지운다.
struct Work(PathBuf);

impl std::ops::Deref for Work {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn work(name: &str) -> Work {
    let dir = std::env::temp_dir().join(format!("cc_scn_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj").join("sub")).unwrap();
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    write(&dir.join("proj/d.csv"), "id,amount\n1,10\n2,20\n3,30\n");
    write(&dir.join("proj/sub/s.csv"), "id,amount\n1,10\n2,20\n3,30\n4,40\n");
    write(&dir.join("cwd/c.csv"), "id,amount\n1,1\n2,2\n3,3\n4,4\n5,5\n");
    Work(dir)
}

fn write(p: &Path, text: &str) {
    std::fs::write(p, text).unwrap();
}

/// 실행 파일을 돌리고 30초 안에 끝나지 않으면 종료시킨다(예: 옵션 검사가 깨져 GUI가 뜨는 경우).
/// 출력은 별도 스레드에서 읽어 파이프가 가득 차 멈추지 않게 한다.
fn run_cmd(cmd: Command, input: Option<&str>) -> Run {
    run_cmd_bytes(cmd, input.map(str::as_bytes))
}

/// `run_cmd`와 같지만 표준 입력을 바이트로 준다(UTF-8이 아닌 입력 시험용).
fn run_cmd_bytes(mut cmd: Command, input: Option<&[u8]>) -> Run {
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = input {
        // 쓰고 나서 닫아야(drop) 서버가 입력 끝을 본다
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(Box::new(child.stdout.take().unwrap()));
    let err = drain(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("30초 안에 끝나지 않음: {cmd:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        code: status.code().unwrap_or(-1),
        out: out.join().unwrap(),
        err: err.join().unwrap(),
    }
}

fn run_in(cwd: &Path, args: &[&str]) -> Run {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(cwd).args(args);
    run_cmd(cmd, None)
}
fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// GPT + 표 형식 블루프린트 본문
fn gpt_bp(extra: &str) -> String {
    format!(
        r#"{{"version":2,"target":"Gpt","format":"MarkdownTable","role":"품질 감사관","fields":"column,issue"{extra}}}"#
    )
}

fn ok(r: &Run) {
    assert_eq!(r.code, 0, "exit {}\nstdout:\n{}\nstderr:\n{}", r.code, r.out, r.err);
}

#[test]
fn s01_relative_data_path_loads_next_to_blueprint() {
    let w = work("s01");
    let bp = w.join("proj/rel.kiro");
    write(
        &bp,
        &gpt_bp(
            r#","sample_rows":2,"data_path":"d.csv","goals":[{"text":"중복 행 여부를 확인","on":true},{"text":"꺼진 제약","on":false}]"#,
        ),
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains("rows: 3") && r.out.contains("| column | issue |"));
    assert!(r.out.contains("중복 행 여부를 확인") && !r.out.contains("꺼진 제약"));
}

#[test]
fn s02_subfolder_data_path() {
    let w = work("s02");
    let bp = w.join("proj/sub.kiro");
    write(&bp, &gpt_bp(r#","data_path":"sub/s.csv""#));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains("rows: 4"));
}

#[test]
fn s03_absolute_data_path_from_other_folder() {
    let w = work("s03");
    let bp = w.join("other/abs.kiro");
    let csv = w.join("proj/d.csv").display().to_string().replace('\\', "\\\\");
    write(&bp, &gpt_bp(&format!(r#","data_path":"{csv}""#)));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains("rows: 3"));
}

#[test]
fn s04_missing_data_file_fails() {
    let w = work("s04");
    let bp = w.join("other/missing.kiro");
    write(&bp, &gpt_bp(r#","data_path":"d.csv""#));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("데이터 로드 실패"));
}

#[test]
fn s05_data_flag_overrides_blueprint_data() {
    let w = work("s05");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(r#","data_path":"d.csv""#));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp), "--data", "c.csv"]);
    ok(&r);
    assert!(r.out.contains("rows: 5"));
}

#[test]
fn s06_constraint_flag_appends() {
    let w = work("s06");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(r#","goals":[{"text":"중복 행 여부를 확인","on":true}]"#));
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--blueprint", s(&bp), "--constraint", "추가 제약"],
    );
    ok(&r);
    assert!(r.out.contains("추가 제약") && r.out.contains("중복 행 여부를 확인"));
}

#[test]
fn s07_steering_flag_appends_rules() {
    let w = work("s07");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(""));
    let steering = w.join("proj/rules.md");
    write(
        &steering,
        "# 규칙\n- 숫자는 소수점 둘째 자리까지\n* 근거 컬럼을 함께 표기\n설명 줄은 무시\n",
    );
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--blueprint", s(&bp), "--steering", s(&steering)],
    );
    ok(&r);
    assert!(r.out.contains("숫자는 소수점 둘째 자리까지") && r.out.contains("근거 컬럼을 함께 표기"));
    assert!(!r.out.contains("설명 줄은 무시"));
}

#[test]
fn s08_v1_project_file_loads() {
    let w = work("s08");
    let bp = w.join("proj/v1.kiro");
    write(
        &bp,
        r#"{"version":1,"project":"old.kiro","target":"Claude","role":"r","goals":[{"text":"이상치 우선 탐지","on":true,"pos":[1.0,2.0]}],"format":"StrictJson","fields":"a,b","sample_rows":8,"theme":3,"data_path":"d.csv","positions":[["root",[1.0,2.0]]]}"#,
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains(r#"rows="3""#) && r.out.contains("이상치 우선 탐지"));
}

#[test]
fn s09_utf8_bom_blueprint() {
    let w = work("s09");
    let bp = w.join("proj/bom.kiro");
    write(&bp, &format!("\u{feff}{}", gpt_bp("")));
    ok(&run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]));
}

#[test]
fn s10_newer_version_rejected() {
    let w = work("s10");
    let bp = w.join("proj/v99.kiro");
    write(&bp, r#"{"version":99}"#);
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("v99"));
}

#[test]
fn s11_malformed_json_rejected() {
    let w = work("s11");
    let bp = w.join("proj/bad.kiro");
    write(&bp, r#"{"version":2,"#);
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("형식 오류"));
}

#[test]
fn s12_canvas_graph_json_is_not_a_blueprint() {
    let w = work("s12");
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", GRAPH_SAMPLE]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("블루프린트 파일이 아닙니다"), "{}", r.err);
}

#[test]
fn s13_sample_rows_clamped_to_50() {
    let w = work("s13");
    let rows: String = (1..=80).map(|i| format!("{i},{}\n", i * 3)).collect();
    write(&w.join("proj/big.csv"), &format!("id,amount\n{rows}"));
    let bp = w.join("proj/clamp.kiro");
    write(&bp, &gpt_bp(r#","sample_rows":999,"theme":42,"data_path":"big.csv""#));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    let sample_lines = r
        .out
        .lines()
        .filter(|l| l.split(',').count() == 2 && l.split(',').all(|c| c.parse::<u32>().is_ok()))
        .count();
    assert!(r.out.contains("rows: 80"));
    assert_eq!(sample_lines, 50);
}

#[test]
fn s14_cli_value_conflicts_with_blueprint() {
    let w = work("s14");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(""));
    for extra in [["--role", "x"], ["--sample-rows", "8"], ["--target", "claude"]] {
        let mut args = vec!["--cli", "--blueprint", s(&bp)];
        args.extend(extra);
        assert_eq!(run_in(&w.join("cwd"), &args).code, 2, "{extra:?}");
    }
}

#[test]
fn s15_gui_blueprint_with_data_requires_cli() {
    let w = work("s15");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(""));
    let r = run_in(&w.join("cwd"), &["--blueprint", s(&bp), "--data", "c.csv"]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("--cli"));
}

#[test]
fn s16_empty_fields_is_lint_error() {
    let w = work("s16");
    let bp = w.join("proj/nofields.kiro");
    write(&bp, r#"{"version":2,"target":"Claude","role":"r","fields":" , "}"#);
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("[ERROR]"));
}

#[test]
fn s17_multiline_goal_whitespace_collapsed() {
    let w = work("s17");
    let bp = w.join("proj/multi.kiro");
    write(
        &bp,
        r#"{"version":2,"target":"Claude","role":"r","fields":"a","goals":[{"text":"이상치는   z-score 3 기준으로\n    판단","on":true}]}"#,
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains("z-score 3 기준으로 판단"));
}

#[test]
fn s18_relative_blueprint_path_from_other_cwd() {
    let w = work("s18");
    write(&w.join("proj/rel.kiro"), &gpt_bp(r#","data_path":"d.csv""#));
    let r = run_in(&w, &["--cli", "--blueprint", "proj/rel.kiro"]);
    ok(&r);
    assert!(r.out.contains("rows: 3"));
}

#[test]
fn s19_contradiction_removed_end_to_end() {
    let w = work("s19");
    let bp = w.join("proj/conflict.kiro");
    write(
        &bp,
        r#"{"version":2,"target":"Claude","role":"r","fields":"a","goals":[{"text":"결측치 제외","on":true},{"text":"결측치 포함","on":true}]}"#,
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.err.contains("모순 지시 자동제거"));
    assert!(r.out.contains("결측치 포함") && !r.out.contains(">결측치 제외<"));
}

#[test]
fn s20_cp949_csv_through_blueprint() {
    let w = work("s20");
    // "id,이름\n1,가\n2,나\n" 의 CP949 바이트
    let bytes: &[u8] = b"id,\xc0\xcc\xb8\xa7\n1,\xb0\xa1\n2,\xb3\xaa\n";
    std::fs::write(w.join("proj/k.csv"), bytes).unwrap();
    let bp = w.join("proj/k.kiro");
    write(
        &bp,
        r#"{"version":2,"target":"Gemini","role":"r","fields":"a","data_path":"k.csv"}"#,
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.starts_with("=== ROLE ==="));
    assert!(r.out.contains("* 이름 [text]"), "{}", r.out);
}

#[test]
fn s21_mcp_compress_prompt_with_data() {
    let w = work("s21");
    let csv = w.join("proj/d.csv");
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "compress_prompt", "arguments": {
            "role": "Analyst", "target": "gpt", "format": "json",
            "constraints": ["Detect outliers", "You must not detect outliers"],
            "data_path": csv.display().to_string()
        }}
    });
    let mut cmd = Command::new(BIN);
    cmd.arg("--mcp");
    let r = run_cmd(cmd, Some(&format!("{call}\n")));
    ok(&r);
    let v: serde_json::Value = serde_json::from_str(r.out.trim()).unwrap();
    assert_eq!(v["id"], 7);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("# Role") && text.contains("rows: 3"));
    assert!(text.contains("모순 지시 자동제거"));
    assert_eq!(v["result"]["isError"], false);
}

/// MCP 서버에 입력 바이트를 보내고, 정상 종료를 확인한 뒤 응답 줄들을 JSON으로 읽는다.
fn mcp(input: &[u8]) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(BIN);
    cmd.arg("--mcp");
    let r = run_cmd_bytes(cmd, Some(input));
    ok(&r);
    r.out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("JSON 아님: {l} ({e})")))
        .collect()
}

fn tool_call(id: u32, args: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "compress_prompt", "arguments": args }
    })
    .to_string()
}

#[test]
fn s22_mcp_invalid_utf8_line_then_request_answered() {
    // CP949 바이트("한")가 섞인 줄: 예전에는 서버가 종료 코드 1로 끝나 뒤의 요청에 답하지 못했다
    let mut input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"x\":\"\xc7\xd1\"}\n".to_vec();
    input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
    let lines = mcp(&input);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["error"]["code"], -32700);
    assert!(lines[0]["id"].is_null());
    assert_eq!(lines[1]["id"], 2);
    assert_eq!(lines[1]["result"], serde_json::json!({}));
}

#[test]
fn s23_mcp_bad_target_is_invalid_params_and_aliases_work() {
    let input = format!(
        "{}\n{}\n{}\n",
        tool_call(1, serde_json::json!({ "role": "r", "target": "llama" })),
        tool_call(2, serde_json::json!({ "role": "r", "format": "yaml" })),
        tool_call(
            3,
            serde_json::json!({ "role": "Analyst", "target": "OpenAI", "format": "Markdown", "fields": " cause , fix " })
        ),
    );
    let lines = mcp(input.as_bytes());
    assert_eq!(lines.len(), 3, "{lines:?}");
    for (line, arg) in [(&lines[0], "target"), (&lines[1], "format")] {
        assert_eq!(line["error"]["code"], -32602, "{line}");
        assert!(line["error"]["message"].as_str().unwrap().contains(arg), "{line}");
    }
    let text = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("# Role") && text.contains("| cause | fix |"), "{text}");
    let tokens = text.lines().last().unwrap();
    assert!(
        tokens.starts_with("--- tokens: ") && (tokens.contains("(+") || tokens.contains("(-")),
        "{tokens}"
    );
}

#[test]
fn s24_mcp_ignores_responses_and_notifications() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":6,\"error\":{\"code\":-1,\"message\":\"x\"}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"no/such/notification\"}\n",
        "[{\"jsonrpc\":\"2.0\",\"method\":\"a\"},{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":1}]\n",
        "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n",
    );
    assert_eq!(
        mcp(input.as_bytes()),
        [serde_json::json!({ "jsonrpc": "2.0", "id": 9, "result": {} })]
    );
}

#[test]
fn s25_mcp_invalid_requests_get_32600() {
    let input = "1\n[1,2]\n{\"jsonrpc\":\"2.0\",\"id\":3}\n[]\n{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"nope\"}\n";
    let lines = mcp(input.as_bytes());
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0]["error"]["code"], -32600);
    assert!(lines[0]["id"].is_null());
    let batch = lines[1].as_array().unwrap();
    assert!(batch.len() == 2 && batch.iter().all(|e| e["error"]["code"] == -32600 && e["id"].is_null()));
    assert_eq!(
        (&lines[2]["error"]["code"], &lines[2]["id"]),
        (&serde_json::json!(-32600), &serde_json::json!(3))
    );
    assert_eq!(lines[3]["error"]["code"], -32600);
    assert_eq!(lines[4]["error"]["code"], -32601);
}

#[test]
fn s26_mcp_initialize_negotiates_version_and_lists_schema() {
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",",
        "\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2099-01-01\"}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\"}\n",
    );
    let lines = mcp(input.as_bytes());
    assert_eq!(lines.len(), 3, "{lines:?}");
    let init = &lines[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-03-26");
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(init["serverInfo"]["name"], "candy-corn");
    assert_eq!(lines[1]["result"]["protocolVersion"], "2025-06-18");
    let schema = &lines[2]["result"]["tools"][0]["inputSchema"];
    let props = schema["properties"].as_object().unwrap();
    for k in [
        "role",
        "constraints",
        "target",
        "format",
        "fields",
        "data_path",
        "sample_rows",
    ] {
        assert!(props.contains_key(k), "{k}");
    }
    assert_eq!(props.len(), 7);
    assert_eq!(schema["additionalProperties"], false);
}

#[test]
fn s27_cli_rejects_binary_and_blueprint_data() {
    let w = work("s27");
    let png = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-256.png");
    let r = run_in(&w.join("cwd"), &["--cli", "--data", png]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.err.contains("바이너리 파일은 프로파일링할 수 없습니다"), "{}", r.err);
    let bp = w.join("proj/plan.kiro");
    write(&bp, &gpt_bp(""));
    let r = run_in(&w.join("cwd"), &["--cli", "--data", s(&bp)]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(
        r.err.contains("블루프린트(.kiro) 파일은 데이터가 아닙니다"),
        "{}",
        r.err
    );
}

#[test]
fn s28_cli_late_cp949_rows_and_utf16_csv() {
    let w = work("s28");
    // 64KiB가 넘는 ASCII 행 뒤에 처음 나오는 CP949 행
    let mut csv = String::from("id,memo\n");
    for i in 1..=70 {
        csv += &format!("{i},{}\n", "a".repeat(1000));
    }
    for i in 71..=170 {
        csv += &format!("{i},홍길동\n");
    }
    std::fs::write(w.join("proj/late.csv"), encoding_rs::EUC_KR.encode(&csv).0).unwrap();
    let late = w.join("proj/late.csv");
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--sample-rows", "50", "--data", s(&late)],
    );
    ok(&r);
    assert!(r.out.contains("170 rows") && r.out.contains("홍길동"), "{}", r.out);
    assert!(!r.out.contains('\u{FFFD}'), "{}", r.out);
    // NDJSON: 예전에는 "stream did not contain valid UTF-8"로 실패했다
    let mut nd = String::new();
    for i in 0..1200 {
        nd += &format!("{{\"id\":{i},\"t\":\"{}\"}}\n", "b".repeat(60));
    }
    nd += "{\"id\":9999,\"t\":\"김철수\"}\n";
    std::fs::write(w.join("proj/late.ndjson"), encoding_rs::EUC_KR.encode(&nd).0).unwrap();
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--data", s(&w.join("proj/late.ndjson"))],
    );
    ok(&r);
    assert!(r.out.contains("1201 rows"), "{}", r.out);
    // UTF-16LE(BOM) CSV: 예전에는 컬럼 이름이 깨졌다
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(
            "이름,금액\r\n가,1\r\n나,2\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        )
        .collect();
    std::fs::write(w.join("proj/u16.csv"), utf16).unwrap();
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--data", s(&w.join("proj/u16.csv"))],
    );
    ok(&r);
    assert!(
        r.out.contains("* 이름 [text]") && r.out.contains("* 금액 [numeric]"),
        "{}",
        r.out
    );
}

#[test]
fn s29_cli_semicolon_and_tab_delimited_numbers() {
    let w = work("s29");
    // 세미콜론 구분 + 소수점 쉼표: "1,5"는 천 단위 구분이 아니므로 숫자로 읽지 않는다(15가 되면 안 된다)
    write(
        &w.join("proj/eu.csv"),
        "id;amount;rate;city\n1;10;1,5;Seoul\n2;20;2,25;Busan\n3;30;0,75;Daegu\n",
    );
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--data", s(&w.join("proj/eu.csv"))],
    );
    ok(&r);
    assert!(
        r.out
            .contains("* amount [numeric] null=0.0% min:10, max:30, mean:20, std:8.165"),
        "{}",
        r.out
    );
    assert!(
        r.out.contains("* rate [text]") && r.out.contains("* city [text]"),
        "{}",
        r.out
    );
    write(&w.join("proj/t.tsv"), "id\tamount\n1\t1,000\n2\t2,500\n");
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--data", s(&w.join("proj/t.tsv"))],
    );
    ok(&r);
    assert!(
        r.out
            .contains("* amount [numeric] null=0.0% min:1000, max:2500, mean:1750, std:750"),
        "{}",
        r.out
    );
}
