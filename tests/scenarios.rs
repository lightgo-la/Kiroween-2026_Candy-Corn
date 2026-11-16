//! 시나리오 테스트 21개: 입력 파일 준비 → 실제 실행 파일 실행 → 출력 검증까지 한 번에 확인한다.
//! 블루프린트 CLI, 데이터 경로 해석, 린트, 인코딩, MCP 서버 경로를 다룬다. `cargo test`에 포함된다.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_Candy-Corn");
const STEERING: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/.kiro/steering/compiler-rules.md");
const GRAPH_SAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json");

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// 시나리오마다 독립된 작업 폴더 (병렬 실행 대비)
fn work(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cc_scn_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj").join("sub")).unwrap();
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    write(&dir.join("proj/d.csv"), "id,amount\n1,10\n2,20\n3,30\n");
    write(&dir.join("proj/sub/s.csv"), "id,amount\n1,10\n2,20\n3,30\n4,40\n");
    write(&dir.join("cwd/c.csv"), "id,amount\n1,1\n2,2\n3,3\n4,4\n5,5\n");
    dir
}

fn write(p: &Path, text: &str) {
    std::fs::write(p, text).unwrap();
}

fn run_in(cwd: &Path, args: &[&str]) -> Run {
    let o = Command::new(BIN).current_dir(cwd).args(args).output().unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
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
    let r = run_in(
        &w.join("cwd"),
        &["--cli", "--blueprint", s(&bp), "--steering", STEERING],
    );
    ok(&r);
    assert!(r.out.contains("데이터에 없는 내용은 추론하지 마"));
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
    let mut child = Command::new(BIN)
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "{call}").unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["id"], 7);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("# Role") && text.contains("rows: 3"));
    assert!(text.contains("모순 지시 자동제거"));
    assert_eq!(v["result"]["isError"], false);
}
