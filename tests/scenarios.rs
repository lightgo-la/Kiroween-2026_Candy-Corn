//! 33 scenario tests: each one prepares input files, runs the real executable and checks the output.
//! Covers the blueprint CLI, data path resolution, lint, encodings/delimiters/binary data, the MCP server paths and
//! Korean mode (`--lang ko`; English is the default). Part of `cargo test`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

include!(concat!(env!("OUT_DIR"), "/ko_fixture.rs"));

const BIN: &str = env!("CARGO_BIN_EXE_Candy-Corn");
const GRAPH_SAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json");

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// A separate work folder per scenario (tests run in parallel). Removed when the test ends.
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

/// Runs the executable and kills it if it does not finish within 30 seconds (e.g. a broken option check opens the
/// GUI). Output is read on separate threads so a full pipe cannot block the process.
fn run_cmd(cmd: Command, input: Option<&str>) -> Run {
    run_cmd_bytes(cmd, input.map(str::as_bytes))
}

/// Like `run_cmd`, but stdin is given as bytes (for testing input that is not UTF-8).
fn run_cmd_bytes(mut cmd: Command, input: Option<&[u8]>) -> Run {
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = input {
        // Close (drop) stdin after writing so the server sees the end of the input
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
            panic!("did not finish within 30 seconds: {cmd:?}");
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

/// GPT + table blueprint body
fn gpt_bp(extra: &str) -> String {
    format!(
        r#"{{"version":2,"target":"Gpt","format":"MarkdownTable","role":"{}","fields":"column,issue"{extra}}}"#,
        ko_fixture!("scenarios.role_quality_auditor")
    )
}

fn ok(r: &Run) {
    assert_eq!(r.code, 0, "exit {}\nstdout:\n{}\nstderr:\n{}", r.code, r.out, r.err);
}

#[test]
fn s01_relative_data_path_loads_next_to_blueprint() {
    let w = work("s01");
    let bp = w.join("proj/rel.kiro");
    let (check, off) = (
        ko_fixture!("scenarios.goal_check_duplicate_rows"),
        ko_fixture!("scenarios.goal_disabled"),
    );
    write(
        &bp,
        &gpt_bp(&format!(
            r#","sample_rows":2,"data_path":"d.csv","goals":[{{"text":"{check}","on":true}},{{"text":"{off}","on":false}}]"#
        )),
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains("rows: 3") && r.out.contains("| column | issue |"));
    assert!(r.out.contains(check) && !r.out.contains(off));
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
    assert!(r.err.contains("failed to load data"), "{}", r.err);
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
    let check = ko_fixture!("scenarios.goal_check_duplicate_rows");
    let extra = ko_fixture!("scenarios.extra_constraint");
    write(&bp, &gpt_bp(&format!(r#","goals":[{{"text":"{check}","on":true}}]"#)));
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp), "--constraint", extra]);
    ok(&r);
    assert!(r.out.contains(extra) && r.out.contains(check));
}

#[test]
fn s07_steering_flag_appends_rules() {
    let w = work("s07");
    let bp = w.join("proj/rel.kiro");
    write(&bp, &gpt_bp(""));
    let steering = w.join("proj/rules.md");
    let (dash, star, plain) = (
        ko_fixture!("scenarios.steering_dash_rule"),
        ko_fixture!("scenarios.steering_star_rule"),
        ko_fixture!("scenarios.steering_plain_line"),
    );
    write(
        &steering,
        &format!(
            "# {}\n- {dash}\n* {star}\n{plain}\n",
            ko_fixture!("scenarios.steering_heading")
        ),
    );
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--blueprint", s(&bp), "--steering", s(&steering)],
    );
    ok(&r);
    assert!(r.out.contains(dash) && r.out.contains(star));
    assert!(!r.out.contains(plain));
}

#[test]
fn s08_v1_project_file_loads() {
    let w = work("s08");
    let bp = w.join("proj/v1.kiro");
    let goal = ko_fixture!("scenarios.goal_outliers_first");
    write(
        &bp,
        &format!(
            r#"{{"version":1,"project":"old.kiro","target":"Claude","role":"r","goals":[{{"text":"{goal}","on":true,"pos":[1.0,2.0]}}],"format":"StrictJson","fields":"a,b","sample_rows":8,"theme":3,"data_path":"d.csv","positions":[["root",[1.0,2.0]]]}}"#
        ),
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains(r#"rows="3""#) && r.out.contains(goal));
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
    assert!(r.err.contains("invalid format"), "{}", r.err);
}

#[test]
fn s12_canvas_graph_json_is_not_a_blueprint() {
    let w = work("s12");
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", GRAPH_SAMPLE]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("not a blueprint file"), "{}", r.err);
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
    // JSON-encoded, so the newline in the goal text becomes `\n` in the file
    let goal = serde_json::Value::from(ko_fixture!("scenarios.goal_multiline"));
    write(
        &bp,
        &format!(r#"{{"version":2,"target":"Claude","role":"r","fields":"a","goals":[{{"text":{goal},"on":true}}]}}"#),
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.out.contains(ko_fixture!("scenarios.goal_multiline_collapsed")));
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
    let (exclude, include) = (
        ko_fixture!("scenarios.goal_exclude_missing"),
        ko_fixture!("scenarios.goal_include_missing"),
    );
    write(
        &bp,
        &format!(
            r#"{{"version":2,"target":"Claude","role":"r","fields":"a","goals":[{{"text":"{exclude}","on":true}},{{"text":"{include}","on":true}}]}}"#
        ),
    );
    let r = run_in(&w.join("cwd"), &["--cli", "--blueprint", s(&bp)]);
    ok(&r);
    assert!(r.err.contains("Removed contradicting instruction"), "{}", r.err);
    assert!(r.out.contains(include) && !r.out.contains(&format!(">{exclude}<")));
}

#[test]
fn s20_cp949_csv_through_blueprint() {
    let w = work("s20");
    // CP949 bytes of "id,{name}\n1,{ga}\n2,{na}\n" (the scenarios.col_name, scenarios.cell_ga and scenarios.cell_na
    // fixtures)
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
    assert!(
        r.out
            .contains(&format!("* {} [text]", ko_fixture!("scenarios.col_name"))),
        "{}",
        r.out
    );
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
    assert!(text.contains("Removed contradicting instruction"), "{text}");
    assert_eq!(v["result"]["isError"], false);
}

/// Sends input bytes to the MCP server, checks that it exits normally and parses the response lines as JSON.
fn mcp(input: &[u8]) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(BIN);
    cmd.arg("--mcp");
    let r = run_cmd_bytes(cmd, Some(input));
    ok(&r);
    r.out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l} ({e})")))
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
    // A line with CP949 bytes (one Hangul syllable): the server used to exit with code 1 and not answer the later
    // request
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
    assert!(r.err.contains("cannot profile a binary file"), "{}", r.err);
    let bp = w.join("proj/plan.kiro");
    write(&bp, &gpt_bp(""));
    let r = run_in(&w.join("cwd"), &["--cli", "--data", s(&bp)]);
    assert_eq!(r.code, 1, "{}", r.out);
    assert!(r.err.contains("a blueprint (.kiro) file is not data"), "{}", r.err);
}

#[test]
fn s28_cli_late_cp949_rows_and_utf16_csv() {
    let w = work("s28");
    // CP949 rows that first appear after more than 64 KiB of ASCII rows
    let memo = ko_fixture!("scenarios.memo_value");
    let mut csv = String::from("id,memo\n");
    for i in 1..=70 {
        csv += &format!("{i},{}\n", "a".repeat(1000));
    }
    for i in 71..=170 {
        csv += &format!("{i},{memo}\n");
    }
    std::fs::write(w.join("proj/late.csv"), encoding_rs::EUC_KR.encode(&csv).0).unwrap();
    let late = w.join("proj/late.csv");
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--sample-rows", "50", "--data", s(&late)],
    );
    ok(&r);
    assert!(r.out.contains("170 rows") && r.out.contains(memo), "{}", r.out);
    assert!(!r.out.contains('\u{FFFD}'), "{}", r.out);
    // NDJSON: used to fail with "stream did not contain valid UTF-8"
    let mut nd = String::new();
    for i in 0..1200 {
        nd += &format!("{{\"id\":{i},\"t\":\"{}\"}}\n", "b".repeat(60));
    }
    nd += &format!("{{\"id\":9999,\"t\":\"{}\"}}\n", ko_fixture!("scenarios.ndjson_value"));
    std::fs::write(w.join("proj/late.ndjson"), encoding_rs::EUC_KR.encode(&nd).0).unwrap();
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--target", "gemini", "--data", s(&w.join("proj/late.ndjson"))],
    );
    ok(&r);
    assert!(r.out.contains("1201 rows"), "{}", r.out);
    // UTF-16LE (BOM) CSV: the column names used to be garbled
    let (name, amount) = (ko_fixture!("scenarios.col_name"), ko_fixture!("scenarios.col_amount"));
    let (ga, na) = (ko_fixture!("scenarios.cell_ga"), ko_fixture!("scenarios.cell_na"));
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(
            format!("{name},{amount}\r\n{ga},1\r\n{na},2\r\n")
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
        r.out.contains(&format!("* {name} [text]")) && r.out.contains(&format!("* {amount} [numeric]")),
        "{}",
        r.out
    );
}

#[test]
fn s29_cli_semicolon_and_tab_delimited_numbers() {
    let w = work("s29");
    // Semicolon-delimited with decimal commas: "1,5" is not a thousands separator, so it is not read as a number (it
    // must not become 15)
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

fn has_hangul(s: &str) -> bool {
    s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
}

/// Whether the text has Hangul outside double-quoted user text (checks the language of diagnostics)
fn hangul_outside_quotes(s: &str) -> bool {
    s.split('"').step_by(2).any(has_hangul)
}

/// stderr lines that start with `tag`
fn tagged<'a>(err: &'a str, tag: &str) -> Vec<&'a str> {
    err.lines().filter(|l| l.starts_with(tag)).collect()
}

#[test]
fn s30_cli_lang_ko_contradiction_warning_keeps_quoted_text() {
    let w = work("s30");
    let csv = w.join("proj/d.csv");
    let (exclude, include) = (
        ko_fixture!("scenarios.goal_exclude_missing"),
        ko_fixture!("scenarios.goal_include_missing"),
    );
    let base = [
        "--cli",
        "--data",
        s(&csv),
        "--constraint",
        exclude,
        "--constraint",
        include,
        "--constraint",
        "Detect outliers",
        "--constraint",
        "You must not detect outliers",
    ];
    let pairs = [
        format!("\"{exclude}\" ↔ \"{include}\""),
        "\"Detect outliers\" ↔ \"You must not detect outliers\"".to_string(),
    ];
    // Without --lang the messages are English and keep the quoted user text as is
    let en = run_in(&w.join("cwd"), &base);
    ok(&en);
    for pair in &pairs {
        let want = format!("[WARN] Removed contradicting instruction: {pair} (kept the later one)");
        assert!(en.err.contains(&want), "{want}\n{}", en.err);
    }
    assert!(en.err.contains("[INFO] Schema integrity check passed"), "{}", en.err);
    assert!(!hangul_outside_quotes(&en.err), "{}", en.err);
    // --lang ko: the same diagnostics in Korean. The prompt (stdout) and the token line are byte-identical in both
    // languages.
    let ko = run_in(&w.join("cwd"), &[&base[..], &["--lang", "ko"]].concat());
    ok(&ko);
    let warns = tagged(&ko.err, "[WARN] ");
    assert_eq!(warns.len(), tagged(&en.err, "[WARN] ").len(), "{}", ko.err);
    for pair in &pairs {
        assert!(
            warns
                .iter()
                .any(|l| l.contains(pair.as_str()) && hangul_outside_quotes(l)),
            "{pair}\n{}",
            ko.err
        );
    }
    let info = tagged(&ko.err, "[INFO] ");
    assert!(info.len() == 1 && has_hangul(info[0]), "{}", ko.err);
    assert_eq!(ko.out, en.out);
    assert!(
        en.out.contains("rows=\"3\"")
            && en.out.contains(&format!(">{include}</rule>"))
            && !en.out.contains(&format!(">{exclude}<")),
        "{}",
        en.out
    );
    let tokens = en.err.lines().last().unwrap_or_default();
    assert!(tokens.starts_with("tokens: "), "{}", en.err);
    assert_eq!(ko.err.lines().last(), Some(tokens));
}

#[test]
fn s31_cli_lang_ko_empty_fields_error() {
    let w = work("s31");
    let en = run_in(&w.join("cwd"), &["--cli", "--fields", " , "]);
    assert_eq!(en.code, 1, "{}", en.err);
    assert!(
        en.err.contains("[ERROR] Output schema has no required fields"),
        "{}",
        en.err
    );
    assert!(!has_hangul(&en.err), "{}", en.err);
    let ko = run_in(&w.join("cwd"), &["--cli", "--lang", "ko", "--fields", " , "]);
    assert_eq!(ko.code, 1, "{}", ko.err);
    let errors = tagged(&ko.err, "[ERROR] ");
    assert!(errors.len() == 1 && has_hangul(errors[0]), "{}", ko.err);
    assert_eq!(ko.out, en.out);
}

#[test]
fn s32_cli_lang_ko_data_load_errors() {
    let w = work("s32");
    // The first part is main.rs text, the rest is profiler text. In English even the OS text of a Korean Windows is
    // English.
    let en = run_in(&w.join("cwd"), &["--cli", "--data", "missing.csv"]);
    assert_eq!(en.code, 1, "{}", en.out);
    assert_eq!(
        en.err.trim_end(),
        "failed to load data: cannot open file: no such file or directory (os error 2)"
    );
    let png = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-256.png");
    let bin = run_in(&w.join("cwd"), &["--cli", "--data", png]);
    assert_eq!(bin.code, 1, "{}", bin.out);
    assert_eq!(
        bin.err.trim_end(),
        "failed to load data: cannot profile a binary file (contains NUL bytes; only CSV/TSV/JSON/NDJSON text files are supported)"
    );
    // --lang ko: the main.rs and profiler parts are Korean (the OS text follows the Windows language)
    let ko = run_in(&w.join("cwd"), &["--cli", "--lang", "ko", "--data", "missing.csv"]);
    assert_eq!(ko.code, 1, "{}", ko.out);
    let parts: Vec<&str> = ko.err.trim_end().splitn(3, ": ").collect();
    assert!(
        parts.len() == 3 && has_hangul(parts[0]) && has_hangul(parts[1]) && parts[2].ends_with("(os error 2)"),
        "{}",
        ko.err
    );
}

/// Like `mcp`, but with explicit arguments (e.g. `--mcp --lang ko`).
fn mcp_args(args: &[&str], input: &[u8]) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(BIN);
    cmd.args(args);
    let r = run_cmd_bytes(cmd, Some(input));
    ok(&r);
    r.out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l} ({e})")))
        .collect()
}

#[test]
fn s33_mcp_lang_ko_bad_target_and_tool_text() {
    let w = work("s33");
    let csv = w.join("proj/d.csv");
    let input = format!(
        "{}\n{}\n{}\n{}\n",
        tool_call(1, serde_json::json!({ "role": "r", "target": "llama" })),
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"no/such\"}",
        tool_call(
            3,
            serde_json::json!({
                "role": "Analyst", "target": "gpt",
                "constraints": ["Detect outliers", "You must not detect outliers"],
                "data_path": csv.display().to_string()
            })
        ),
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/list\"}",
    );
    let en = mcp(input.as_bytes());
    let ko = mcp_args(&["--mcp", "--lang", "ko"], input.as_bytes());
    assert_eq!((en.len(), ko.len()), (4, 4), "{ko:?}");
    assert_eq!(en[0]["error"]["code"], -32602, "{}", en[0]);
    assert_eq!(
        en[0]["error"]["message"],
        "Invalid target: \"llama\" (expected one of claude/anthropic, gpt/openai, gemini/google)"
    );
    assert_eq!(en[1]["error"]["message"], "Method not found: no/such");
    // --lang ko: Korean messages with the same error codes; the quoted value stays as is
    assert_eq!(
        (&ko[0]["error"]["code"], &ko[1]["error"]["code"]),
        (&en[0]["error"]["code"], &en[1]["error"]["code"])
    );
    let ko_msg = ko[0]["error"]["message"].as_str().unwrap();
    assert!(
        hangul_outside_quotes(ko_msg) && ko_msg.contains("\"llama\""),
        "{ko_msg}"
    );
    let ko_missing = ko[1]["error"]["message"].as_str().unwrap();
    assert!(
        has_hangul(ko_missing) && ko_missing.ends_with("no/such"),
        "{ko_missing}"
    );
    // Tool result: the prompt is the same; only the lint lines change language
    let text = |v: &serde_json::Value| v["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let (en_text, ko_text) = (text(&en[2]), text(&ko[2]));
    let (en_prompt, en_lint) = en_text.split_once("\n\n--- lint ---\n").unwrap();
    let (ko_prompt, ko_lint) = ko_text.split_once("\n\n--- lint ---\n").unwrap();
    assert_eq!(en_prompt, ko_prompt);
    assert!(
        en_prompt.starts_with("# Role") && en_prompt.contains("rows: 3"),
        "{en_prompt}"
    );
    assert!(
        en_lint.contains(
            "[WARN] Removed contradicting instruction: \"Detect outliers\" ↔ \"You must not detect outliers\" (kept the later one)"
        ),
        "{en_lint}"
    );
    assert!(!has_hangul(en_lint), "{en_lint}");
    let ko_warn = tagged(ko_lint, "[WARN] ");
    assert!(
        ko_warn.len() == 1
            && hangul_outside_quotes(ko_warn[0])
            && ko_warn[0].contains("\"Detect outliers\" ↔ \"You must not detect outliers\""),
        "{ko_lint}"
    );
    assert_eq!(en[2]["result"]["isError"], false);
    assert_eq!(ko[2]["result"]["isError"], false);
    // tools/list (English descriptions) does not depend on the language
    assert_eq!(en[3], ko[3]);
    let desc = en[3]["result"]["tools"][0]["description"].as_str().unwrap();
    assert!(desc.starts_with("Lint and compress"), "{desc}");
}
