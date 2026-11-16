//! QC: 무작위 입력 10,000건(컴파일 6,000 + 프로파일러 4,000)으로 불변식을 검사한다.
//! 시드가 고정되어 있어 실패는 재현 가능하다. 실패는 모아서 한 번에 보고한다.

use crate::ast::{ColumnProfile, DataSubAst, OutputFormat, TargetModel, linter};
use crate::pipeline::{self, CompileRequest, LoadedData};
use crate::profiler::csv_profiler::profile_file;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

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
    fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

const PIECES: &[&str] = &[
    "detect",
    "outliers",
    "이상치",
    "탐지",
    "하지 마",
    "do not",
    "never",
    "very",
    "매우",
    "please",
    "exclude",
    "include",
    "결측치",
    "0.2%",
    "<b>",
    "&",
    "\"",
    "|",
    "💥",
    "제외",
    "포함",
    "must not",
    "말 것",
    "</rule>",
    "</role>",
    "`",
    "",
    "amount",
    "금액",
    "우선",
    "allow",
    "forbid",
    "증가",
    "감소",
    "\\",
    "a|b",
    "'",
];

fn text(r: &mut Rng) -> String {
    let n = r.below(9);
    (0..n)
        .map(|_| PIECES[r.below(PIECES.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

fn one_line(s: String) -> String {
    s.replace(['\n', '\r'], " ")
}

/// 실패를 분류별로 모은다.
#[derive(Default)]
struct Failures(BTreeMap<String, Vec<String>>);

impl Failures {
    fn add(&mut self, kind: &str, detail: String) {
        self.0.entry(kind.to_string()).or_default().push(detail);
    }
    fn report(&self, total: usize) -> String {
        let mut s = format!("{} 건 중 실패 분류 {}종\n", total, self.0.len());
        for (k, v) in &self.0 {
            s += &format!("  [{k}] {}건, 예: {}\n", v.len(), v[0]);
        }
        s
    }
}

fn synth_data(r: &mut Rng) -> LoadedData {
    let ncols = 1 + r.below(4);
    let columns = (0..ncols)
        .map(|_| ColumnProfile {
            name: one_line(text(r)),
            dtype: if r.chance(50) { "numeric".into() } else { "text".into() },
            null_ratio: r.below(101) as f64 / 100.0,
            summary_stats: one_line(text(r)),
        })
        .collect();
    let sample_rows = (0..r.below(4)).map(|_| one_line(text(r))).collect();
    let anomalies = (0..r.below(3)).map(|_| one_line(text(r))).collect();
    LoadedData {
        path: PathBuf::new(),
        raw_tokens: r.below(1_000_000),
        data: DataSubAst {
            source_name: one_line(text(r)),
            total_rows: r.below(1_000_000),
            columns,
            anomalies,
            sample_rows,
        },
    }
}

/// XML 출력에서 우리가 내보낸 태그만 존재하고 속성 따옴표가 맞는지 검사한다.
fn check_xml(out: &str) -> Result<(), String> {
    const OK: [(&str, usize); 10] = [
        ("<role ", 4),
        ("</role>", 0),
        ("<data_profile ", 6),
        ("</data_profile>", 0),
        ("<schema>", 0),
        ("</schema>", 0),
        ("<anomalies>", 0),
        ("</anomalies>", 0),
        ("<sample>", 0),
        ("</sample>", 0),
    ];
    const OK2: [(&str, usize); 6] = [
        ("<constraints>", 0),
        ("</constraints>", 0),
        ("<rule ", 4),
        ("</rule>", 0),
        ("<output_format>", 0),
        ("</output_format>", 0),
    ];
    let mut rest = out;
    while let Some(p) = rest.find('<') {
        rest = &rest[p..];
        let end = rest.find('>').ok_or("태그가 닫히지 않음")?;
        let tag = &rest[..=end];
        let expected = OK.iter().chain(OK2.iter()).find(|(pre, _)| tag.starts_with(pre));
        match expected {
            None => return Err(format!("알 수 없는 태그: {tag}")),
            Some((_, quotes)) => {
                let q = tag.matches('"').count();
                let want = match *quotes {
                    4 if tag.starts_with("<role ") => 2,
                    6 => 4,
                    n => n,
                };
                if q != want {
                    return Err(format!("속성 따옴표 {q}개(기대 {want}): {tag}"));
                }
            }
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

fn unescaped_pipes(line: &str) -> usize {
    line.matches('|').count() - line.matches("\\|").count()
}

fn check_markdown(out: &str, ncols_data: Option<usize>) -> Result<(), String> {
    if let Some(_n) = ncols_data {
        let mut lines = out.lines().skip_while(|l| !l.starts_with("|---|---|---|---|"));
        lines.next();
        for l in lines.take_while(|l| l.starts_with('|')) {
            if unescaped_pipes(l) != 5 {
                return Err(format!("데이터 표 행의 열 수가 어긋남: {l}"));
            }
        }
    }
    Ok(())
}

#[test]
fn qc_compile_6000() {
    let mut r = Rng(0x00C0_FFEE_1234_5678);
    let mut f = Failures::default();
    let total = 6000;
    for i in 0..total {
        let target = TargetModel::ALL[r.below(3)];
        let fmt = if r.chance(50) {
            OutputFormat::StrictJson
        } else {
            OutputFormat::MarkdownTable
        };
        let req = CompileRequest {
            target,
            role: text(&mut r),
            constraints: (0..r.below(9)).map(|_| text(&mut r)).collect(),
            format: fmt,
            fields: (0..r.below(5))
                .map(|_| {
                    if r.chance(70) {
                        format!("f{}", r.below(4))
                    } else {
                        one_line(text(&mut r))
                    }
                })
                .collect(),
            data: if r.chance(60) { Some(synth_data(&mut r)) } else { None },
        };
        let res = match catch_unwind(AssertUnwindSafe(|| pipeline::compile(&req))) {
            Ok(x) => x,
            Err(_) => {
                f.add("compile 패닉", format!("#{i} {req:?}"));
                continue;
            }
        };
        let ctx = |s: &str| format!("#{i} {s}");

        // ID 연속성, 개수
        for (k, c) in res.ast.constraints.iter().enumerate() {
            if c.id != format!("C{}", k + 1) {
                f.add("ID 불연속", ctx(&c.id));
                break;
            }
        }
        if res.ast.constraints.len() > req.constraints.len() {
            f.add("제약 개수 증가", ctx(""));
        }
        // 재린트 멱등성: 더 이상 바뀌거나 경고가 나오면 안 된다
        let mut again = res.ast.clone();
        again.lint_warnings.clear();
        linter::lint(&mut again);
        let changed = again.constraints.len() != res.ast.constraints.len()
            || again.role.instruction != res.ast.role.instruction
            || again
                .constraints
                .iter()
                .zip(&res.ast.constraints)
                .any(|(a, b)| a.expression != b.expression);
        if changed {
            f.add(
                "린트 비멱등(재실행 시 변경)",
                ctx(&format!(
                    "{:?} -> {:?}",
                    res.ast.constraints.iter().map(|c| &c.expression).collect::<Vec<_>>(),
                    again.constraints.iter().map(|c| &c.expression).collect::<Vec<_>>()
                )),
            );
        }
        // 토큰/퍼센트
        if !res.saving_pct.is_finite() || !res.saved_usd.is_finite() || res.comp_tokens == 0 {
            f.add(
                "토큰/절감률 비정상",
                ctx(&format!("{} {} {}", res.saving_pct, res.saved_usd, res.comp_tokens)),
            );
        }
        // 스키마
        let o = &res.ast.output_schema;
        match o.format {
            OutputFormat::StrictJson => match serde_json::from_str::<serde_json::Value>(&o.raw_schema) {
                Ok(v) => {
                    let req_f: Vec<String> = v["required"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    if req_f != o.required_fields {
                        f.add("JSON 스키마 required 불일치", ctx(&o.raw_schema));
                    }
                }
                Err(e) => f.add("JSON 스키마 파싱 실패", ctx(&format!("{e}"))),
            },
            OutputFormat::MarkdownTable => {
                if !o.required_fields.is_empty() && unescaped_pipes(&o.raw_schema) != o.required_fields.len() + 1 {
                    f.add("표 헤더 열 수 불일치", ctx(&o.raw_schema));
                }
            }
        }
        // 직렬화 구조
        match target {
            TargetModel::Claude => {
                if let Err(e) = check_xml(&res.output) {
                    f.add("XML 구조 오류", ctx(&e));
                }
            }
            TargetModel::Gpt => {
                if let Err(e) = check_markdown(&res.output, req.data.as_ref().map(|d| d.data.columns.len())) {
                    f.add("Markdown 표 깨짐", ctx(&e));
                }
            }
            TargetModel::Gemini => {
                if !res.output.starts_with("=== ROLE ===") || !res.output.contains("=== OUTPUT ===") {
                    f.add("Gemini 섹션 누락", ctx(""));
                }
            }
        }
        // 미리보기
        if o.format == OutputFormat::StrictJson && serde_json::from_str::<serde_json::Value>(&res.preview).is_err() {
            f.add("미리보기 JSON 오류", ctx(&res.preview));
        }
    }
    assert!(f.0.is_empty(), "\n{}", f.report(total));
}

// ---------------- 프로파일러 ----------------

fn gen_cell(r: &mut Rng, numeric: bool) -> String {
    if r.chance(10) {
        return ["", "null", "NaN", "n/a"][r.below(4)].to_string();
    }
    if numeric {
        match r.below(8) {
            0 => format!("{}", r.below(1000)),
            1 => format!("-{}.{}", r.below(100), r.below(100)),
            2 => format!("{},{:03}", r.below(100), r.below(1000)),
            3 => "1e308".into(),
            4 => "-1e308".into(),
            5 => "1e-300".into(),
            6 => "  7 ".into(),
            _ => "inf".into(),
        }
    } else {
        let mut s = text(r);
        if r.chance(15) {
            s += "\n줄바꿈";
        }
        if r.chance(15) {
            s += ", 쉼표 \"따옴표\"";
        }
        if r.chance(5) {
            s = "가".repeat(60);
        }
        s
    }
}

fn csv_quote(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn json_val(r: &mut Rng, numeric: bool) -> serde_json::Value {
    use serde_json::{Value, json};
    if r.chance(10) {
        return Value::Null;
    }
    if numeric {
        match r.below(4) {
            0 => json!(r.below(1000)),
            1 => json!(r.below(1000) as f64 / 7.0),
            2 => json!(1e308),
            _ => json!(-5),
        }
    } else {
        match r.below(5) {
            0 => json!(true),
            1 => json!({"n": r.below(3)}),
            2 => json!([1, 2]),
            _ => Value::String(gen_cell(r, false)),
        }
    }
}

const HEADERS: &[&str] = &[
    "id", "order_id", "amount", "이름", "a", "a", "", "region", "금액", "x,y", "q\"t", "note",
];

#[test]
fn qc_profiler_4000() {
    let dir = std::env::temp_dir().join(format!("sc_qc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut r = Rng(0x0BAD_C0DE_9876_5432);
    let mut f = Failures::default();
    let total = 4000;
    for i in 0..total {
        let ncols = 2 + r.below(5);
        let nrows = 1 + r.below(60);
        let headers: Vec<&str> = (0..ncols).map(|_| HEADERS[r.below(HEADERS.len())]).collect();
        let numeric_col: Vec<bool> = (0..ncols).map(|_| r.chance(50)).collect();
        let k = r.below(6);
        let mode = r.below(5); // 0 csv, 1 csv+bom, 2 csv cp949, 3 json array, 4 ndjson
        let (path, expected_rows) = match mode {
            0..=2 => {
                let mut s = headers.iter().map(|h| csv_quote(h)).collect::<Vec<_>>().join(",") + "\n";
                for _ in 0..nrows {
                    let row: Vec<String> = (0..ncols)
                        .map(|c| csv_quote(&gen_cell(&mut r, numeric_col[c])))
                        .collect();
                    s += &(row.join(",") + "\n");
                }
                let mut bytes = s.clone().into_bytes();
                if mode == 1 {
                    bytes = [&[0xEF, 0xBB, 0xBF][..], &bytes].concat();
                } else if mode == 2 {
                    let (enc, _, errs) = encoding_rs::EUC_KR.encode(&s);
                    if !errs {
                        bytes = enc.into_owned();
                    }
                }
                let p = dir.join(format!("{i}.csv"));
                std::fs::write(&p, bytes).unwrap();
                (p, nrows)
            }
            _ => {
                let mut objs = vec![];
                let mut n_obj = 0;
                for _ in 0..nrows {
                    if r.chance(10) {
                        objs.push(serde_json::json!(r.below(5))); // 객체가 아닌 요소는 건너뛴다
                    } else {
                        let mut m = serde_json::Map::new();
                        for c in 0..ncols {
                            if r.chance(85) {
                                m.insert(headers[c].to_string(), json_val(&mut r, numeric_col[c]));
                            }
                        }
                        objs.push(serde_json::Value::Object(m));
                        n_obj += 1;
                    }
                }
                let p = dir.join(format!("{i}.{}", if mode == 3 { "json" } else { "ndjson" }));
                if mode == 3 {
                    std::fs::write(&p, serde_json::to_string(&objs).unwrap()).unwrap();
                } else {
                    // NDJSON은 객체 줄만 쓴다(원시값 줄은 형식이 아님)
                    let lines: Vec<String> = objs.iter().filter(|o| o.is_object()).map(|o| o.to_string()).collect();
                    std::fs::write(&p, lines.join("\n\n")).unwrap();
                }
                (p, n_obj)
            }
        };
        let ctx = |s: &str| format!("#{i} mode={mode} {s}");
        let res = match catch_unwind(AssertUnwindSafe(|| profile_file(&path, k))) {
            Ok(x) => x,
            Err(_) => {
                f.add("profile 패닉", ctx(""));
                continue;
            }
        };
        let _ = std::fs::remove_file(&path);
        let d = match res {
            Ok(d) => d,
            Err(e) => {
                if expected_rows == 0 && e.contains("데이터 행이 없음") {
                    continue;
                }
                f.add("profile 오류 반환", ctx(&e));
                continue;
            }
        };
        if expected_rows == 0 {
            f.add("행 0건인데 성공", ctx(""));
            continue;
        }
        if d.total_rows != expected_rows {
            f.add("행 수 불일치", ctx(&format!("{} != {}", d.total_rows, expected_rows)));
        }
        let mut names: Vec<&String> = d.columns.iter().map(|c| &c.name).collect();
        names.sort();
        names.dedup();
        if names.len() != d.columns.len() {
            f.add(
                "컬럼 이름 중복",
                ctx(&format!("{:?}", d.columns.iter().map(|c| &c.name).collect::<Vec<_>>())),
            );
        }
        if d.columns.iter().any(|c| c.name.trim().is_empty()) {
            f.add("빈 컬럼 이름", ctx(""));
        }
        if d.sample_rows.len() != k.min(d.total_rows) {
            f.add(
                "샘플 행 수 불일치",
                ctx(&format!("{} != {}", d.sample_rows.len(), k.min(d.total_rows))),
            );
        }
        for c in &d.columns {
            if !(0.0..=1.0).contains(&c.null_ratio) || c.null_ratio.is_nan() {
                f.add("null_ratio 범위 오류", ctx(&format!("{} {}", c.name, c.null_ratio)));
            }
            let s = c.summary_stats.to_lowercase();
            if s.contains("nan") || s.contains("inf") {
                f.add("통계에 NaN/inf", ctx(&format!("{}: {}", c.name, c.summary_stats)));
            }
        }
        for a in &d.anomalies {
            let a = a.to_lowercase();
            if a.contains("nan") || a.contains("inf") {
                f.add("이상치에 NaN/inf", ctx(&a));
            }
        }
        for row in &d.sample_rows {
            if row.contains(['\n', '\r']) {
                f.add("샘플 행에 개행", ctx(row));
            }
            if row.is_empty() && d.columns.len() == 1 {
                continue; // 단일 컬럼의 빈 값은 빈 줄로 표현된다(정상)
            }
            let mut rdr = csv::ReaderBuilder::new().has_headers(false).from_reader(row.as_bytes());
            match rdr.records().next() {
                Some(Ok(rec)) if rec.len() == d.columns.len() => {}
                other => f.add("샘플 행 CSV 열 수 불일치", ctx(&format!("{row:?} {other:?}"))),
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(f.0.is_empty(), "\n{}", f.report(total));
}
