//! Sample data tests: the real executable reads every file in `samples/`, the planted problems show up in the data
//! profile, every sample blueprint compiles with its own data, and the MCP server refuses files that are not data.
//! The files are checked byte for byte where the encoding is the point of the sample (CP949, UTF-16, BOM).

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

include!(concat!(env!("OUT_DIR"), "/ko_fixture.rs"));

const BIN: &str = env!("CARGO_BIN_EXE_Candy-Corn");

fn samples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("samples")
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn cli(args: &[&str]) -> Out {
    let o = Command::new(BIN).arg("--cli").args(args).output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// (file, rows, text that the profile must contain)
fn data_samples() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        (
            "web_traffic.tsv",
            "240 rows",
            vec![
                "sessions: max 48210",
                "bounce_rate: max 0.97",
                "* conversions [numeric] null=1.7%",
            ],
        ),
        (
            "iot_sensors.ndjson",
            "300 rows",
            vec![
                "temperature_c: max 92.4",
                "vibration_mm_s: max 7.8",
                "battery_v: min 3.09",
            ],
        ),
        (
            "ecommerce_orders.json",
            "200 rows",
            vec![
                "quantity: max 480",
                "shipping_days: max 21",
                "* discount_pct [numeric] null=6.0%",
            ],
        ),
        (
            "hr_employees_kr.csv",
            "150 rows",
            vec![
                ko_fixture!("samples.hr.header"),
                ko_fixture!("samples.hr.salary_outlier"),
            ],
        ),
        (
            "support_tickets.csv",
            "220 rows",
            vec![
                "first_response_min: max 2880",
                "resolution_hours: max 312",
                "* csat [numeric] null=25.9%",
            ],
        ),
        ("bank_transactions.txt", "247 rows", vec!["amount: min -9850"]),
        (
            "energy_meter_utf16.txt",
            "180 rows",
            vec!["peak_kw: max 2450", "kwh: max 24900"],
        ),
        ("survey_responses.csv", "120 rows", vec!["minutes_to_complete: max 186"]),
        (
            "customers_messy.csv",
            "83 rows",
            vec![
                "age: max 212",
                "annual_spend_krw: max 45000000",
                "* email [text] null=6.0%",
            ],
        ),
    ]
}

#[test]
fn every_data_sample_is_profiled_with_its_planted_problems() {
    for (file, rows, expect) in data_samples() {
        let path = samples().join(file);
        let r = cli(&["--target", "gemini", "--data", path.to_str().unwrap()]);
        assert_eq!(r.code, 0, "{file}\n{}", r.err);
        assert!(
            r.out.contains(&format!("({file}, {rows})")),
            "{file}: {}",
            &r.out[..r.out.len().min(400)]
        );
        for text in expect {
            assert!(r.out.contains(text), "{file} should show {text:?}\n{}", r.out);
        }
        assert!(!r.err.contains("[ERROR]"), "{file}\n{}", r.err);
    }
}

#[test]
fn every_sample_blueprint_compiles_with_its_own_data() {
    let mut seen = 0;
    for entry in std::fs::read_dir(samples()).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("kiro") {
            continue;
        }
        let bp: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let data = bp["data_path"].as_str().expect("a sample blueprint names its data");
        assert!(samples().join(data).is_file(), "{path:?}: {data} is missing");
        let r = cli(&["--blueprint", path.to_str().unwrap()]);
        assert_eq!(r.code, 0, "{path:?}\n{}", r.err);
        assert!(r.out.contains(data), "{path:?}: the data file is not in the prompt");
        assert!(!r.err.contains("[ERROR]"), "{path:?}\n{}", r.err);
        seen += 1;
    }
    assert!(seen >= 10, "only {seen} sample blueprints");
}

#[test]
fn the_korean_sample_keeps_its_cp949_headers_and_salary_column() {
    let path = samples().join("hr_employees_kr.csv");
    let r = cli(&["--target", "claude", "--data", path.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.err);
    // The salary has thousands separators ("25,000") and is still read as a number
    assert!(r.out.contains(ko_fixture!("samples.hr.salary_outlier")), "{}", r.out);
    assert!(r.out.contains(ko_fixture!("samples.hr.rank_header")), "{}", r.out);
}

/// The samples exist to exercise the profiler's input paths, so their bytes must stay as generated.
#[test]
fn samples_keep_the_encodings_and_delimiters_they_are_for() {
    let read = |name: &str| std::fs::read(samples().join(name)).unwrap();
    let first_line =
        |b: &[u8]| String::from_utf8_lossy(&b[..b.iter().position(|&c| c == b'\n').unwrap_or(b.len())]).into_owned();
    // CP949: not valid UTF-8
    assert!(std::str::from_utf8(&read("hr_employees_kr.csv")).is_err());
    // UTF-16 LE with a byte order mark
    assert_eq!(&read("energy_meter_utf16.txt")[..2], [0xFF, 0xFE]);
    // UTF-8 with a byte order mark and semicolons
    let tickets = read("support_tickets.csv");
    assert_eq!(&tickets[..3], [0xEF, 0xBB, 0xBF]);
    assert!(first_line(&tickets).contains(';'));
    assert!(first_line(&read("web_traffic.tsv")).contains('\t'));
    assert!(first_line(&read("bank_transactions.txt")).contains('|'));
    // One JSON object per line, and one pretty-printed array
    let ndjson = String::from_utf8(read("iot_sensors.ndjson")).unwrap();
    assert!(ndjson.lines().all(|l| l.starts_with('{') && l.ends_with('}')));
    assert!(
        String::from_utf8(read("ecommerce_orders.json"))
            .unwrap()
            .starts_with('[')
    );
    // The messy file is messy on purpose: CRLF line ends and exact duplicate rows
    let messy = String::from_utf8(read("customers_messy.csv")).unwrap();
    assert!(messy.contains("\r\n"));
    let lines: Vec<&str> = messy.lines().collect();
    let unique: std::collections::HashSet<&&str> = lines.iter().collect();
    assert_eq!(lines.len() - unique.len(), 3, "three exact duplicate rows");
}

fn mcp(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(BIN)
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for r in requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn the_mcp_server_reads_sample_data_but_not_other_files() {
    let call = |id: u32, path: &Path| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"compress_prompt",
            "arguments":{"role":"r","target":"claude","data_path":path.to_str().unwrap()}}})
    };
    let replies = mcp(&[
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        call(2, &samples().join("web_traffic.tsv")),
        call(3, &samples().join("README.md")),
        call(4, &samples().join("customers_messy.kiro")),
    ]);
    let by_id = |id: u64| {
        replies
            .iter()
            .find(|v| v["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id}: {replies:?}"))
    };
    let text = |v: &Value| {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let good = by_id(2);
    assert_eq!(good["result"]["isError"], false, "{}", text(good));
    assert!(text(good).contains("sessions: max 48210"));
    for id in [3, 4] {
        let bad = by_id(id);
        assert_eq!(bad["result"]["isError"], true, "id {id}");
        assert!(text(bad).contains("Only data files"), "id {id}: {}", text(bad));
    }
}
