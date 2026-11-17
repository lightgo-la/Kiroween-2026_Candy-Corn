//! QA: end-to-end scenario test that runs the real binary 1,000 times.
//! It takes a while, so the default `cargo test` skips it: `cargo test --test qa -- --ignored`

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

include!(concat!(env!("OUT_DIR"), "/ko_fixture.rs"));

const BIN: &str = env!("CARGO_BIN_EXE_Candy-Corn");
const TOTAL: usize = 1000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const RULES: &[&str] = ko_fixture!("qa.rules");

fn make_data(r: &mut Rng, dir: &Path, i: usize) -> PathBuf {
    let rows = 1 + r.below(200);
    match r.below(5) {
        0..=2 => {
            let memo = ko_fixture!("qa.memo");
            let mut s = format!("order_id,region,amount,{memo}\n");
            for n in 0..rows {
                let amount = if n == rows / 2 && rows > 20 {
                    9_000_000
                } else {
                    1000 + (n * 37) % 900
                };
                s += &format!("{n},R{},{amount},\"{memo}, {n}\"\n", n % 4);
            }
            let kind = r.below(3);
            let bytes = match kind {
                1 => [&[0xEF, 0xBB, 0xBF][..], s.as_bytes()].concat(),
                2 => {
                    let (b, _, errs) = encoding_rs::EUC_KR.encode(&s);
                    if errs { s.into_bytes() } else { b.into_owned() }
                }
                _ => s.into_bytes(),
            };
            let p = dir.join(format!("d{i}.csv"));
            std::fs::write(&p, bytes).unwrap();
            p
        }
        3 => {
            let items: Vec<String> = (0..rows)
                .map(|n| format!("{{\"id\":{n},\"v\":{},\"t\":\"x{n}\"}}", n * 3 % 17))
                .collect();
            let p = dir.join(format!("d{i}.json"));
            std::fs::write(&p, format!("[{}]", items.join(","))).unwrap();
            p
        }
        _ => {
            let items: Vec<String> = (0..rows).map(|n| format!("{{\"id\":{n},\"v\":{}}}", n % 9)).collect();
            let p = dir.join(format!("d{i}.ndjson"));
            std::fs::write(&p, items.join("\n")).unwrap();
            p
        }
    }
}

fn marker(target: &str) -> &'static str {
    match target {
        "claude" => "<role ",
        "gpt" => "# Role",
        _ => "=== ROLE ===",
    }
}

fn run_cli_ok(r: &mut Rng, dir: &Path, i: usize) -> Result<(), String> {
    let target = ["claude", "gpt", "gemini"][r.below(3)];
    let format = ["json", "table"][r.below(2)];
    let mut cmd = Command::new(BIN);
    cmd.args(["--cli", "--target", target, "--format", format]);
    if r.below(4) != 0 {
        cmd.arg("--data").arg(make_data(r, dir, i));
    }
    cmd.args(["--role", ko_fixture!("qa.roles")[r.below(3)]]);
    for _ in 0..r.below(6) {
        let rule = RULES[r.below(RULES.len())];
        if !rule.is_empty() {
            cmd.args(["--constraint", rule]);
        }
    }
    cmd.args(["--sample-rows", &r.below(10).to_string()]);
    let out = cmd.output().map_err(|e| e.to_string())?;
    let (so, se) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    if !out.status.success() {
        return Err(format!("exit {:?}: {se}", out.status.code()));
    }
    if !so.contains(marker(target)) {
        return Err(format!("{target} marker missing: {}", &so[..so.len().min(80)]));
    }
    if !se.contains("tokens:") || se.contains("panicked") {
        return Err(format!("unexpected stderr: {se}"));
    }
    if !so.contains("Respond with ONLY") {
        return Err("output instruction missing".into());
    }
    Ok(())
}

fn run_cli_err(r: &mut Rng, dir: &Path, i: usize) -> Result<(), String> {
    let mut cmd = Command::new(BIN);
    cmd.arg("--cli");
    match r.below(6) {
        0 => {
            cmd.args(["--data", "Z:\\no\\such\\file.csv"]);
        }
        1 => {
            let p = dir.join(format!("bad{i}.json"));
            std::fs::write(&p, "{bad json").unwrap();
            cmd.arg("--data").arg(p);
        }
        2 => {
            let p = dir.join(format!("empty{i}.csv"));
            std::fs::write(&p, "").unwrap();
            cmd.arg("--data").arg(p);
        }
        3 => {
            cmd.args(["--fields", ","]);
        }
        4 => {
            cmd.args(["--target", "bogus"]);
        }
        _ => {
            cmd.args(["--steering", "Z:\\no\\steering.md"]);
        }
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    let se = String::from_utf8_lossy(&out.stderr);
    match out.status.code() {
        Some(1) | Some(2) if !se.trim().is_empty() && !se.contains("panicked") => Ok(()),
        c => Err(format!("not the expected error exit: exit={c:?} stderr={se}")),
    }
}

fn run_mcp(r: &mut Rng, dir: &Path, i: usize) -> Result<(), String> {
    let data = if r.below(2) == 0 {
        Some(make_data(r, dir, i))
    } else {
        None
    };
    let target = ["claude", "gpt", "gemini"][r.below(3)];
    let rules = vec![RULES[r.below(RULES.len())], RULES[r.below(RULES.len())]];
    let fmt = ["json", "table"][r.below(2)];
    let mut args = serde_json::json!({
        "role": "Analyst",
        "constraints": rules,
        "target": target,
        "format": fmt,
    });
    if let Some(d) = &data {
        args["data_path"] = serde_json::json!(d.to_string_lossy());
    }
    let reqs = vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"compress_prompt","arguments":args}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"unknown/method"}),
    ];
    let mut child = Command::new(BIN)
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    {
        let stdin = child.stdin.as_mut().unwrap();
        for q in &reqs {
            writeln!(stdin, "{q}").map_err(|e| e.to_string())?;
        }
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).map_err(|e| format!("not JSON: {l} ({e})")))
        .collect::<Result<_, _>>()?;
    if lines.len() != 4 {
        return Err(format!("{} responses (expected 4)", lines.len()));
    }
    let ids: Vec<i64> = lines.iter().map(|l| l["id"].as_i64().unwrap_or(-1)).collect();
    if ids != [1, 2, 3, 4] {
        return Err(format!("id order {ids:?}"));
    }
    let text = lines[2]["result"]["content"][0]["text"]
        .as_str()
        .ok_or("no tools/call text")?;
    if !text.contains(marker(target)) {
        return Err(format!("{target} marker missing"));
    }
    if lines[3]["error"]["code"] != -32601 || lines[1]["result"]["tools"][0]["name"] != "compress_prompt" {
        return Err("unexpected tools/list or error response".into());
    }
    Ok(())
}

#[test]
#[ignore]
fn qa_1000_scenarios() {
    let dir = std::env::temp_dir().join(format!("sc_qa_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let next = AtomicUsize::new(0);
    let fails: Mutex<Vec<String>> = Mutex::new(vec![]);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= TOTAL {
                        break;
                    }
                    let mut r = Rng(0x9E3779B97F4A7C15 ^ (i as u64 + 1).wrapping_mul(0xD1B54A32D192ED03));
                    let (kind, res) = match i % 20 {
                        0..=13 => ("cli-ok", run_cli_ok(&mut r, &dir, i)),
                        14..=16 => ("cli-err", run_cli_err(&mut r, &dir, i)),
                        _ => ("mcp", run_mcp(&mut r, &dir, i)),
                    };
                    if let Err(e) = res {
                        fails.lock().unwrap().push(format!("#{i} [{kind}] {e}"));
                    }
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
    let fails = fails.into_inner().unwrap();
    assert!(
        fails.is_empty(),
        "\nQA: {} of {} runs failed\n{}",
        fails.len(),
        TOTAL,
        fails.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// The CLI path that applies every setting from a blueprint (part of the default cargo test).
#[test]
fn blueprint_cli() {
    let dir = std::env::temp_dir().join(format!("sc_qa_bp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("d.csv"), "id,amount\n1,10\n2,20\n3,30\n").unwrap();
    let bp = dir.join("plan.kiro");
    let role = ko_fixture!("scenarios.role_quality_auditor");
    let (check, off) = (
        ko_fixture!("scenarios.goal_check_duplicate_rows"),
        ko_fixture!("scenarios.goal_disabled"),
    );
    let extra = ko_fixture!("scenarios.extra_constraint");
    std::fs::write(
        &bp,
        format!(
            r#"{{"version":2,"target":"Gpt","format":"MarkdownTable","role":"{role}","fields":"column,issue",
            "sample_rows":2,"data_path":"d.csv",
            "goals":[{{"text":"{check}","on":true}},{{"text":"{off}","on":false}}]}}"#
        ),
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["--cli", "--blueprint"])
        .arg(&bp)
        .args(["--constraint", extra])
        .output()
        .unwrap();
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(so.contains("# Role"), "must be GPT format: {so}");
    assert!(so.contains("rows: 3"), "the relative data path must load");
    assert!(so.contains(check) && so.contains(extra));
    assert!(!so.contains(off));
    assert!(so.contains("| column | issue |"));

    // Values that the blueprint sets are rejected when they are also given on the command line.
    let clash = Command::new(BIN)
        .args(["--cli", "--blueprint"])
        .arg(&bp)
        .args(["--role", "x"])
        .output()
        .unwrap();
    assert_eq!(clash.status.code(), Some(2));
    // A missing file is an error exit
    let missing = Command::new(BIN)
        .args(["--cli", "--blueprint", "Z:\\no\\such.kiro"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}
