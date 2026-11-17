//! CSV / TSV / JSON(배열, NDJSON) 스트리밍 프로파일러.
//! 행 단위로 통계(Welford)를 누적하고, 리저버 샘플링으로 토큰 예산 내 샘플 행을 뽑는다.
//!
//! 인코딩: BOM(UTF-8, UTF-16LE, UTF-16BE)이 있으면 그대로 따른다. 없으면 앞부분 64KiB로 바이너리 여부를 보고,
//! 처음 나오는 비ASCII 바이트부터 64KiB로 UTF-8/CP949를 판별한다(`detect_text_encoding`과 같은 판정이라
//! 토큰 추정·steering 읽기도 같은 결과를 낸다). 파일 전체를 메모리에 올리지 않는다.
//!
//! 프로파일 결과(컬럼 이름·타입, `summary_stats`, `anomalies`, 샘플 행)는 프롬프트에 들어가는 모델용 글이라
//! UI 언어와 관계없이 같다. 번역하는 것은 사용자에게 보이는 오류 문구뿐이다.

use crate::ast::{ColumnProfile, DataSubAst};
use encoding_rs::{EUC_KR, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Chain, Cursor, Read};
use std::path::Path;

const DISTINCT_CAP: usize = 1000;
const CELL_MAX_CHARS: usize = 40;
/// 이 크기를 넘는 값은 분산 계산이 overflow하므로 숫자 통계에서 제외한다.
const MAX_ABS: f64 = 1e15;
/// 바이너리·인코딩·구분자 판별에 쓰는 앞부분 크기.
/// 앞부분이 ASCII뿐이면 처음 나오는 비ASCII 바이트부터 이만큼을 다시 보고 인코딩을 정한다.
const SNIFF_BYTES: usize = 64 * 1024;
/// 변환 리더가 원본에서 한 번에 읽는 크기
const CHUNK_BYTES: usize = 32 * 1024;
/// 구분자 판별에 쓰는 레코드 수 (헤더 포함)
const SNIFF_RECORDS: usize = 21;

#[derive(Default)]
struct ColAcc {
    /// 실제 이름. `synthetic`이면 비어 있고 finish에서 `col_N`을 붙인다.
    name: String,
    /// 이름이 비어 있던 CSV 헤더나 JSON 키
    synthetic: bool,
    present: usize,
    numeric: usize,
    min: f64,
    max: f64,
    mean: f64,
    m2: f64,
    distinct: HashSet<String>,
}

impl ColAcc {
    fn push(&mut self, v: &str) {
        let v = v.trim();
        if v.is_empty()
            || v.eq_ignore_ascii_case("null")
            || v.eq_ignore_ascii_case("nan")
            || v.eq_ignore_ascii_case("n/a")
        {
            return;
        }
        self.present += 1;
        if let Some(x) = parse_number(v) {
            if self.numeric == 0 {
                self.min = x;
                self.max = x;
            } else {
                self.min = self.min.min(x);
                self.max = self.max.max(x);
            }
            self.numeric += 1;
            let d = x - self.mean;
            self.mean += d / self.numeric as f64;
            self.m2 += d * (x - self.mean);
        }
        if self.distinct.len() < DISTINCT_CAP {
            self.distinct.insert(v.to_string());
        }
    }

    fn is_numeric(&self) -> bool {
        self.numeric > 0 && self.numeric * 10 >= self.present * 9
    }

    fn std(&self) -> f64 {
        if self.numeric > 1 {
            // 반올림 오차로 m2가 아주 작은 음수가 되면 NaN이 나오므로 0으로 막는다
            (self.m2 / self.numeric as f64).max(0.0).sqrt()
        } else {
            0.0
        }
    }
}

/// 숫자로 읽는다. 쉼표는 천 단위 구분(첫 묶음 1~3자리, 나머지 정확히 3자리)일 때만 허용한다:
/// "1,234.5"는 1234.5지만 "1,5"·"1,2,3"·"1234,567"은 숫자가 아니다.
fn parse_number(v: &str) -> Option<f64> {
    let x = if v.contains(',') {
        let (int_part, frac) = match v.split_once('.') {
            Some((i, f)) => (i, Some(f)),
            None => (v, None),
        };
        let digits = int_part.strip_prefix(['-', '+']).unwrap_or(int_part);
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let mut groups = digits.split(',');
        let first_ok = groups.next().is_some_and(|g| g.len() <= 3 && all_digits(g));
        if !first_ok || !groups.all(|g| g.len() == 3 && all_digits(g)) || !frac.is_none_or(all_digits) {
            return None;
        }
        v.replace(',', "").parse::<f64>().ok()?
    } else {
        v.parse::<f64>().ok()?
    };
    Some(x).filter(|x| x.is_finite() && x.abs() <= MAX_ABS)
}

struct Accumulator {
    cols: Vec<ColAcc>,
    /// 실제 이름 → 컬럼
    index: HashMap<String, usize>,
    /// 빈(공백뿐인) JSON 키 → 컬럼. 같은 키는 몇 번째 행에 나오든 같은 컬럼이다.
    blank: HashMap<String, usize>,
    rows: usize,
    sample_k: usize,
    sample: Vec<Vec<(usize, String)>>,
    rng: u64,
}

impl Accumulator {
    fn new(sample_k: usize) -> Self {
        Accumulator {
            cols: vec![],
            index: HashMap::new(),
            blank: HashMap::new(),
            rows: 0,
            sample_k,
            sample: vec![],
            rng: 0x9E3779B97F4A7C15,
        }
    }

    fn push_col(&mut self, name: String, synthetic: bool) -> usize {
        self.cols.push(ColAcc {
            name,
            synthetic,
            ..Default::default()
        });
        self.cols.len() - 1
    }

    /// JSON 키로 컬럼을 찾고 없으면 만든다. 빈 키도 키마다 컬럼 하나에 고정한다(행마다 새 컬럼을 만들지 않는다).
    fn col(&mut self, key: &str) -> usize {
        if key.trim().is_empty() {
            if let Some(&i) = self.blank.get(key) {
                return i;
            }
            let i = self.push_col(String::new(), true);
            self.blank.insert(key.to_string(), i);
            return i;
        }
        if let Some(&i) = self.index.get(key) {
            return i;
        }
        let i = self.push_col(key.to_string(), false);
        self.index.insert(key.to_string(), i);
        i
    }

    /// CSV 헤더용: 같은 이름이 또 나오면 `name_2`처럼 구분해 별도 컬럼으로 만든다. 빈 헤더는 자리마다 별도 컬럼이다.
    fn col_unique(&mut self, name: &str) -> usize {
        if name.trim().is_empty() {
            return self.push_col(String::new(), true);
        }
        let mut candidate = name.to_string();
        let mut n = 2;
        while self.index.contains_key(&candidate) {
            candidate = format!("{name}_{n}");
            n += 1;
        }
        let i = self.push_col(candidate.clone(), false);
        self.index.insert(candidate, i);
        i
    }

    /// 표시 이름. 빈 헤더·키는 자리 번호로 `col_N`을 붙이고, 실제 이름과 겹치면 `col_N_2`처럼 비켜 간다.
    fn column_names(&self) -> Vec<String> {
        let mut taken: HashSet<String> = self
            .cols
            .iter()
            .filter(|c| !c.synthetic)
            .map(|c| c.name.clone())
            .collect();
        self.cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if !c.synthetic {
                    return c.name.clone();
                }
                let base = format!("col_{}", i + 1);
                let mut name = base.clone();
                let mut n = 2;
                while !taken.insert(name.clone()) {
                    name = format!("{base}_{n}");
                    n += 1;
                }
                name
            })
            .collect()
    }

    fn next_rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn add_row(&mut self, row: Vec<(usize, String)>) {
        for (i, v) in &row {
            self.cols[*i].push(v);
        }
        self.rows += 1;
        if self.sample.len() < self.sample_k {
            self.sample.push(row);
        } else if self.sample_k > 0 {
            let j = (self.next_rand() % self.rows as u64) as usize;
            if j < self.sample_k {
                self.sample[j] = row;
            }
        }
    }

    fn finish(self, source: &str) -> DataSubAst {
        let rows = self.rows;
        let names = self.column_names();
        let mut anomalies = vec![];
        let columns: Vec<ColumnProfile> = self
            .cols
            .iter()
            .zip(&names)
            .map(|(c, name)| {
                let null_ratio = if rows == 0 {
                    0.0
                } else {
                    (rows - c.present.min(rows)) as f64 / rows as f64
                };
                if c.is_numeric() {
                    let sd = c.std();
                    if c.numeric >= 8 && sd > 0.0 && !looks_like_id(name) {
                        let zmax = (c.max - c.mean) / sd;
                        let zmin = (c.mean - c.min) / sd;
                        if zmax > 3.0 {
                            anomalies.push(format!("{name}: max {} (z={zmax:.1})", fmt(c.max)));
                        }
                        if zmin > 3.0 {
                            anomalies.push(format!("{name}: min {} (z={zmin:.1})", fmt(c.min)));
                        }
                    }
                    ColumnProfile {
                        name: name.clone(),
                        dtype: "numeric".into(),
                        null_ratio,
                        summary_stats: format!(
                            "min:{}, max:{}, mean:{}, std:{}",
                            fmt(c.min),
                            fmt(c.max),
                            fmt(c.mean),
                            fmt(sd)
                        ),
                    }
                } else {
                    let d = if c.distinct.len() >= DISTINCT_CAP {
                        format!("distinct:>={}", DISTINCT_CAP)
                    } else {
                        format!("distinct:{}", c.distinct.len())
                    };
                    ColumnProfile {
                        name: name.clone(),
                        dtype: "text".into(),
                        null_ratio,
                        summary_stats: d,
                    }
                }
            })
            .collect();
        anomalies.truncate(8);
        let ncols = self.cols.len();
        let sample_rows = self
            .sample
            .iter()
            .map(|row| {
                let mut cells = vec![String::new(); ncols];
                for (i, v) in row {
                    cells[*i] = csv_cell(v);
                }
                cells.join(",")
            })
            .collect();
        DataSubAst {
            source_name: source.to_string(),
            total_rows: rows,
            columns,
            anomalies,
            sample_rows,
        }
    }
}

/// 통계 값 표시. 정수는 그대로 쓴다. 절댓값이 1 이상이면 소수 셋째 자리까지, 1 미만이면 유효 숫자 4자리까지 보여
/// 0.0001 같은 작은 값이 0으로 뭉개지지 않게 한다. 아주 작거나 큰 값은 지수로 쓴다.
fn fmt(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let a = x.abs();
    if !(1e-4..1e15).contains(&a) {
        let s = format!("{x:.3e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        return format!("{}e{exp}", trim_zeros(mantissa));
    }
    let decimals = if a >= 1.0 {
        3
    } else {
        (3 - a.log10().floor() as i32).max(3) as usize
    };
    trim_zeros(&format!("{x:.decimals$}")).to_string()
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

fn csv_cell(v: &str) -> String {
    let v: String = v.chars().take(CELL_MAX_CHARS).collect();
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\"").replace(['\n', '\r'], " "))
    } else {
        v
    }
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn add_json_object(acc: &mut Accumulator, v: &serde_json::Value) {
    if let serde_json::Value::Object(map) = v {
        let row = map.iter().map(|(k, val)| (acc.col(k), json_scalar(val))).collect();
        acc.add_row(row);
    }
}

/// `order_id`, `CustomerID`, `id_order`, `orderId`, `customerid`, `No.`, `고객번호` 같은 식별자 컬럼은 이상치 판정에서 제외한다.
/// 이름을 단어로 나눠 `id`가 어디든 있거나, 붙여 쓴 `<개체>id`이거나, 마지막 단어가 번호·코드·키를 뜻하면 식별자로 본다.
fn looks_like_id(name: &str) -> bool {
    let n = name.trim();
    // i18n-data
    if n == "#" || ["번호", "코드", "아이디", "순번"].iter().any(|k| n.contains(k)) {
        return true;
    }
    let words = name_words(n);
    words
        .iter()
        .any(|w| matches!(w.as_str(), "id" | "ids" | "uuid" | "guid") || joined_id(w))
        || words.last().is_some_and(|w| {
            matches!(
                w.as_str(),
                "no" | "num" | "nbr" | "number" | "code" | "key" | "seq" | "idx"
            )
        })
}

/// 한 단어로 붙여 쓴 식별자(`customerid`, `USERID`, `uid`). `paid`, `valid`, `android`, `bid` 같은 일반 단어와
/// 구분하려고 `id` 앞이 흔한 개체 이름이거나, 일반 단어가 되지 않는 한 글자일 때만 본다.
fn joined_id(word: &str) -> bool {
    const ENTITIES: [&str; 32] = [
        "user",
        "customer",
        "client",
        "order",
        "product",
        "item",
        "account",
        "member",
        "employee",
        "session",
        "transaction",
        "txn",
        "device",
        "store",
        "shop",
        "seller",
        "vendor",
        "invoice",
        "payment",
        "ticket",
        "event",
        "record",
        "row",
        "patient",
        "student",
        "post",
        "message",
        "request",
        "trace",
        "group",
        "project",
        "sku",
    ];
    word.strip_suffix("id").is_some_and(|stem| {
        ENTITIES.contains(&stem)
            || (stem.len() == 1 && stem.bytes().all(|b| b.is_ascii_lowercase()) && !"abdhklmr".contains(stem))
    })
}

/// 컬럼 이름을 소문자 단어로 나눈다. 구분 문자(공백, `_`, `-`, `.` 등), camelCase(`orderId`),
/// 약어 경계(`CustomerID`, `IDNumber`), 한글·영문 경계(`고객ID`), 영문·숫자 경계(`id2`)에서 끊는다.
fn name_words(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev = None;
            continue;
        }
        if let Some(p) = prev {
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let split = (p.is_lowercase() && c.is_uppercase())
                || (p.is_uppercase() && c.is_uppercase() && next_lower)
                || p.is_ascii() != c.is_ascii()
                || (p.is_ascii() && c.is_ascii() && p.is_ascii_digit() != c.is_ascii_digit());
            if split {
                words.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words.into_iter().map(|w| w.to_lowercase()).collect()
}

/// 앞부분이 UTF-8로 해석되면 true (버퍼 끝에서 잘린 멀티바이트는 허용). 파일 인코딩은 `detect_text_encoding`으로 정한다.
pub fn looks_utf8(buf: &[u8]) -> bool {
    match std::str::from_utf8(buf) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// BOM이 있으면 (인코딩, BOM 길이).
fn bom_encoding(bytes: &[u8]) -> Option<(&'static Encoding, usize)> {
    match bytes {
        [0xEF, 0xBB, 0xBF, ..] => Some((UTF_8, 3)),
        [0xFF, 0xFE, ..] => Some((UTF_16LE, 2)),
        [0xFE, 0xFF, ..] => Some((UTF_16BE, 2)),
        _ => None,
    }
}

/// BOM이 없는 텍스트의 인코딩(UTF-8 또는 CP949). 처음 나오는 비ASCII 바이트부터 최대 64KiB(판정 창)를
/// `prefers_utf8`로 본다. ASCII뿐이면 UTF-8(두 인코딩에서 같다).
/// `complete`: `bytes`가 텍스트 끝까지인가. 아니면 창이 `bytes` 끝에 닿아도 끝에서 잘린 글자를 오류로 세지 않는다.
fn sniff_encoding(bytes: &[u8], complete: bool) -> &'static Encoding {
    let Some(start) = bytes.iter().position(|b| !b.is_ascii()) else {
        return UTF_8;
    };
    let end = bytes.len().min(start + SNIFF_BYTES);
    if prefers_utf8(&bytes[start..end], complete && end == bytes.len()) {
        UTF_8
    } else {
        EUC_KR
    }
}

/// 텍스트 바이트의 인코딩을 정한다. 프로파일러, 원본 토큰 추정(tokenizer), steering 문서 읽기(pipeline)가 모두
/// 이 판정을 써서 같은 파일을 같은 글자로 읽는다(깨진 바이트 하나 때문에 한쪽만 CP949로 읽지 않는다).
/// BOM(UTF-8, UTF-16LE, UTF-16BE)이 있으면 그것을 따르고, 없으면 처음 나오는 비ASCII 바이트부터 64KiB로
/// UTF-8과 CP949 중 고른다(잘못된 바이트가 비ASCII 바이트의 5% 이하면 군데군데 손상된 UTF-8로 본다).
/// `complete`가 false면(`bytes`가 파일 앞부분) 끝에서 잘린 글자는 오류로 세지 않는다.
/// 돌려준 인코딩의 `decode_with_bom_removal`로 읽으면 BOM도 떼어진다.
pub fn detect_text_encoding(bytes: &[u8], complete: bool) -> &'static Encoding {
    match bom_encoding(bytes) {
        Some((enc, _)) => enc,
        None => sniff_encoding(bytes, complete),
    }
}

/// 판정 창(첫 비ASCII 바이트부터 최대 64KiB)을 UTF-8로 읽을지 정한다.
/// - UTF-8로 잘못된 바이트가 없으면 UTF-8.
/// - 있어도 비ASCII 바이트의 5% 이하이면 군데군데 손상된 UTF-8로 본다. 깨진 바이트 몇 개 때문에 창 전체를
///   CP949로 읽으면 "café"가 "caf챕"이 되듯 멀쩡한 글자까지 망가진다.
/// - 그 밖에는 두 인코딩 중 오류(대체 문자)가 적은 쪽. CP949 한글을 UTF-8로 읽으면 거의 글자마다 오류가 나고
///   ("홍길동"은 "ȫ�浿"처럼 일부만 우연히 맞는다) UTF-8 한글을 CP949로 읽어도 오류가 잦다.
///
/// `complete`가 아니면(창이 파일 끝에 닿지 않음) 창 끝에서 잘린 글자는 오류로 세지 않는다.
fn prefers_utf8(window: &[u8], complete: bool) -> bool {
    let utf8_errors = utf8_error_count(window, complete);
    if utf8_errors == 0 {
        return true;
    }
    let non_ascii = window.iter().filter(|b| !b.is_ascii()).count();
    utf8_errors * 20 <= non_ascii || cp949_error_count(window, complete) >= utf8_errors
}

/// UTF-8로 읽을 때 대체 문자(U+FFFD)가 되는 구간 수
fn utf8_error_count(buf: &[u8], complete: bool) -> usize {
    let mut chunks = buf.utf8_chunks().peekable();
    let mut errors = 0;
    while let Some(chunk) = chunks.next() {
        let bad = chunk.invalid();
        let cut_at_end =
            !complete && chunks.peek().is_none() && std::str::from_utf8(bad).is_err_and(|e| e.error_len().is_none());
        if !bad.is_empty() && !cut_at_end {
            errors += 1;
        }
    }
    errors
}

/// CP949로 읽을 때 대체 문자가 되는 수 (U+FFFD는 CP949에 없으므로 출력에 나온 U+FFFD는 모두 오류다)
fn cp949_error_count(buf: &[u8], complete: bool) -> usize {
    let mut decoder = EUC_KR.new_decoder_without_bom_handling();
    let cap = decoder
        .max_utf8_buffer_length(buf.len())
        .unwrap_or(buf.len().saturating_mul(3).saturating_add(16));
    let mut text = String::with_capacity(cap);
    let _ = decoder.decode_to_string(buf, &mut text, complete);
    text.chars().filter(|&c| c == '\u{FFFD}').count()
}

fn binary_error(ext: &str) -> String {
    let hint = match ext {
        "xlsx" | "xlsm" | "xls" => tr!("엑셀 파일은 CSV로 저장한 뒤 여세요", "save Excel files as CSV first"),
        _ => tr!(
            "CSV/TSV/JSON/NDJSON 텍스트 파일만 지원합니다",
            "only CSV/TSV/JSON/NDJSON text files are supported"
        ),
    };
    trf!(
        "바이너리 파일은 프로파일링할 수 없습니다 (NUL 바이트 포함, {hint})",
        "cannot profile a binary file (contains NUL bytes; {hint})"
    )
}

/// 입출력 오류 문구. 한국어는 운영체제 문구 그대로 쓰고(예전과 같다), 영어는 운영체제 언어와 관계없이 영어로 쓴다.
/// 한국어 Windows의 운영체제 문구("지정된 파일을 찾을 수 없습니다")가 `--lang en` 메시지에 섞이지 않게 한다.
/// 운영체제 오류가 아니면(라이브러리가 만든 오류) 원래 문구 그대로다.
pub fn io_error_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) if crate::i18n::is_en() => {
            let what = if cfg!(windows) && code == 32 {
                // ERROR_SHARING_VIOLATION: 엑셀 등 다른 프로그램이 파일을 잠그고 있다
                "the file is being used by another process".to_string()
            } else if e.kind() == std::io::ErrorKind::NotFound {
                "no such file or directory".to_string()
            } else {
                e.kind().to_string()
            };
            format!("{what} (os error {code})")
        }
        _ => e.to_string(),
    }
}

/// CSV 리더 오류. 바이트 레코드를 유연하게 읽으므로 실제로 나는 오류는 거의 입출력 오류다.
fn csv_error_text(e: &csv::Error) -> String {
    match e.kind() {
        csv::ErrorKind::Io(io) => io_error_text(io),
        _ => e.to_string(),
    }
}

fn read_err(e: std::io::Error) -> String {
    trf!("파일을 읽을 수 없음: {}", "cannot read file: {}", io_error_text(&e))
}

/// 원본 바이트를 UTF-8로 바꿔 내주는 스트리밍 리더.
/// `decoder`가 없으면 지금까지 ASCII만 나온 상태다. 처음 비ASCII 바이트가 나오면 그 뒤 64KiB를 보고 UTF-8인지
/// CP949인지 정한다. 앞부분은 ASCII라 두 인코딩에서 같으므로 처음부터 다시 읽을 필요가 없다.
/// 판정이 UTF-8이면 뒤에 나오는 잘못된 바이트는 U+FFFD로 바꾼다(이미 UTF-8 글자를 본 파일이므로 손상으로 본다).
struct Transcode<R> {
    inner: R,
    decoder: Option<encoding_rs::Decoder>,
    /// 아직 변환하지 않은 원본 바이트는 `raw[pos..]`
    raw: Vec<u8>,
    pos: usize,
    eof: bool,
    /// 변환을 마친 UTF-8 바이트는 `out[out_pos..]`
    out: Vec<u8>,
    out_pos: usize,
    finished: bool,
}

impl<R: Read> Transcode<R> {
    fn new(inner: R, encoding: Option<&'static Encoding>) -> Self {
        Transcode {
            inner,
            decoder: encoding.map(Encoding::new_decoder_without_bom_handling),
            raw: Vec::new(),
            pos: 0,
            eof: false,
            out: Vec::new(),
            out_pos: 0,
            finished: false,
        }
    }

    /// 남은 원본이 `want` 바이트 이상이 되거나 입력이 끝날 때까지 읽는다.
    fn fill(&mut self, want: usize) -> std::io::Result<()> {
        self.raw.drain(..self.pos);
        self.pos = 0;
        while !self.eof && self.raw.len() < want {
            let start = self.raw.len();
            self.raw.resize(start + CHUNK_BYTES, 0);
            let r = self.inner.read(&mut self.raw[start..]);
            self.raw.truncate(start + r.as_ref().map_or(0, |&n| n));
            match r {
                Ok(0) => self.eof = true,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// 변환 결과를 `out`에 새로 채운다. 입력이 모두 끝났으면 false.
    fn refill(&mut self) -> std::io::Result<bool> {
        self.out.clear();
        self.out_pos = 0;
        if self.pos == self.raw.len() {
            self.fill(1)?;
        }
        if self.decoder.is_none() {
            let avail = &self.raw[self.pos..];
            if avail.is_empty() {
                return Ok(false);
            }
            let ascii = avail.iter().position(|b| !b.is_ascii()).unwrap_or(avail.len());
            if ascii > 0 {
                self.out.extend_from_slice(&avail[..ascii]);
                self.pos += ascii;
                return Ok(true);
            }
            // 처음 나온 비ASCII 바이트: 그 뒤 64KiB(또는 파일 끝까지)로 인코딩을 정한다(detect_text_encoding과 같은 판정)
            self.fill(SNIFF_BYTES)?;
            let enc = sniff_encoding(&self.raw[self.pos..], self.eof);
            self.decoder = Some(enc.new_decoder_without_bom_handling());
        }
        if self.finished {
            return Ok(false);
        }
        let Some(dec) = self.decoder.as_mut() else {
            return Ok(false); // 위에서 정했으므로 오지 않는다
        };
        // 입력이 끝났으면 남은 원본이 모두 raw에 있으므로 마지막 호출로 디코더를 비운다
        let last = self.eof;
        let src = &self.raw[self.pos..];
        let cap = dec
            .max_utf8_buffer_length(src.len())
            .unwrap_or(src.len().saturating_mul(3).saturating_add(16));
        self.out.resize(cap, 0);
        let (_, read, written, _) = dec.decode_to_utf8(src, &mut self.out, last);
        self.out.truncate(written);
        self.pos += read;
        if last && self.pos == self.raw.len() {
            self.finished = true;
        }
        Ok(true)
    }
}

impl<R: Read> Read for Transcode<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.out_pos == self.out.len() {
            if !self.refill()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.out.len() - self.out_pos);
        buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        Ok(n)
    }
}

type Decoded<R> = Transcode<Chain<Cursor<Vec<u8>>, R>>;

/// 원본의 앞부분(`head`, 최대 64KiB)과 나머지(`rest`)로 UTF-8 리더를 만든다.
/// BOM을 먼저 따르고, BOM이 없는데 NUL 바이트가 있으면 바이너리로 보고 거부한다.
fn decoded_reader<R: Read>(head: Vec<u8>, rest: R, ext: &str) -> Result<Decoded<R>, String> {
    let (bom, skip) = match bom_encoding(&head) {
        Some((enc, len)) => (Some(enc), len),
        None => (None, 0),
    };
    let body = &head[skip..];
    // 텍스트에는 NUL이 나오지 않는다. UTF-16은 디코딩한 글자로 본다.
    let binary = match bom {
        Some(enc) if enc != UTF_8 => enc.decode_without_bom_handling(body).0.contains('\0'),
        _ => body.contains(&0),
    };
    if binary {
        return Err(binary_error(ext));
    }
    // BOM이 없으면 앞부분으로 미리 정하지 않는다. 앞부분의 비ASCII가 64KiB 경계에서 잘린 한두 바이트뿐이면
    // 판단 근거가 없기 때문이다. 변환 리더가 첫 비ASCII 바이트부터 64KiB를 보고 정한다.
    let mut head = Cursor::new(head);
    head.set_position(skip as u64);
    Ok(Transcode::new(head.chain(rest), bom))
}

/// 파일을 열어 UTF-8 바이트를 내주는 리더를 만든다.
fn open_decoded(path: &Path, ext: &str) -> Result<Decoded<File>, String> {
    let mut f =
        File::open(path).map_err(|e| trf!("파일을 열 수 없음: {}", "cannot open file: {}", io_error_text(&e)))?;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    f.by_ref()
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(read_err)?;
    decoded_reader(head, f, ext)
}

/// 구분자 `d`로 나눈 결과: 레코드마다의 필드 수(최대 SNIFF_RECORDS개, 빈 줄 제외)와 따옴표가 어긋난 횟수.
/// 따옴표로 시작한 필드 안의 구분자와 줄바꿈은 세지 않는다. 닫는 따옴표 뒤에 구분자·줄바꿈이 아닌 글자가 오거나
/// 따옴표가 끝까지 닫히지 않으면 어긋난 것으로 센다(그 구분자로는 맞지 않는 파일이라는 신호).
/// `complete`가 아니면(앞부분만 읽음) 끝에서 잘린 레코드는 버린다.
fn split_stats(text: &[u8], d: u8, complete: bool) -> (Vec<usize>, usize) {
    let mut counts = Vec::new();
    let mut bad = 0;
    let (mut fields, mut content) = (1, false);
    let (mut at_start, mut quoted, mut after_quote) = (true, false, false);
    for &b in text {
        if quoted {
            if b == b'"' {
                quoted = false;
                after_quote = true;
            }
            continue;
        }
        if after_quote {
            after_quote = false;
            if b == b'"' {
                quoted = true; // 따옴표 이스케이프("")
                continue;
            }
            if b != d && b != b'\n' && b != b'\r' {
                bad += 1;
            }
        } else if b == b'"' && at_start {
            quoted = true;
            at_start = false;
            content = true;
            continue;
        }
        if b == b'\n' || b == b'\r' {
            if content {
                counts.push(fields);
                if counts.len() == SNIFF_RECORDS {
                    return (counts, bad);
                }
            }
            fields = 1;
            content = false;
            at_start = true;
        } else if b == d {
            fields += 1;
            content = true;
            at_start = true;
        } else {
            content = true;
            at_start = false;
        }
    }
    if complete {
        bad += usize::from(quoted);
        if content {
            counts.push(fields);
        }
    }
    (counts, bad)
}

/// 헤더와 이어지는 행들로 구분자(탭, 세미콜론, 파이프, 쉼표)를 고른다.
/// 헤더를 2개 이상의 필드로 나누는 후보 중 따옴표가 덜 어긋나고, 뒤 행들의 필드 수가 헤더와 같은 경우가
/// 많은 것을 고른다. 그래도 같으면 탭 > 세미콜론 > 파이프 > 쉼표 순이다(쉼표는 소수점·텍스트 안에도 흔하다).
/// 어느 후보도 헤더를 나누지 못하면(컬럼 하나) None.
fn find_delimiter(text: &[u8], complete: bool) -> Option<u8> {
    let mut best: Option<((std::cmp::Reverse<usize>, usize), u8)> = None;
    for d in *b"\t;|," {
        let (counts, bad) = split_stats(text, d, complete);
        let Some((&header, rows)) = counts.split_first() else {
            continue;
        };
        if header < 2 {
            continue;
        }
        let consistent = rows.iter().filter(|&&n| n == header).count();
        let score = (std::cmp::Reverse(bad), consistent);
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, d));
        }
    }
    best.map(|(_, d)| d)
}

/// `find_delimiter`와 같고, 헤더를 나누는 후보가 없으면(컬럼 하나) 쉼표.
fn sniff_delimiter(text: &[u8], complete: bool) -> u8 {
    find_delimiter(text, complete).unwrap_or(b',')
}

/// JSON 배열을 요소 단위로 읽어 메모리에 전체를 올리지 않는다.
struct ArrayVisitor<'a>(&'a mut Accumulator);

impl<'de> serde::de::Visitor<'de> for ArrayVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON array of objects")
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(v) = seq.next_element::<serde_json::Value>()? {
            add_json_object(self.0, &v);
        }
        Ok(())
    }
}

/// JSON 배열 또는 NDJSON(한 줄에 객체 하나). 첫 글자가 `[`이면 배열이다.
fn profile_json(src: impl Read, acc: &mut Accumulator) -> Result<(), String> {
    use serde::Deserializer as _;
    let mut reader = BufReader::new(src);
    // 앞쪽 공백을 건너뛰며 첫 글자를 찾는다 (줄 번호를 맞추려고 건너뛴 줄 수를 센다)
    let mut skipped_lines = 0;
    let first = loop {
        let buf = reader.fill_buf().map_err(read_err)?;
        if buf.is_empty() {
            break None;
        }
        let ws = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
        skipped_lines += buf[..ws].iter().filter(|&&b| b == b'\n').count();
        let next = buf.get(ws).copied();
        reader.consume(ws);
        if next.is_some() {
            break next;
        }
    };
    if first == Some(b'[') {
        let mut de = serde_json::Deserializer::from_reader(reader);
        let json_err = |e: serde_json::Error| trf!("JSON 파싱 실패: {e}", "JSON parse error: {e}");
        de.deserialize_seq(ArrayVisitor(acc)).map_err(json_err)?;
        de.end().map_err(json_err)?;
    } else {
        for (n, line) in reader.lines().enumerate() {
            let line = line.map_err(read_err)?;
            if line.trim().is_empty() {
                continue;
            }
            let at = n + 1 + skipped_lines;
            let v: serde_json::Value = serde_json::from_str(&line)
                .map_err(|e| trf!("{at}번째 줄 JSON 오류: {e}", "JSON error on line {at}: {e}"))?;
            add_json_object(acc, &v);
        }
    }
    Ok(())
}

/// 구분자 텍스트(CSV/TSV). `preferred`(.tsv의 탭)가 헤더를 나누면 그것을 쓰고, 나누지 못하면 앞부분으로 판별한다
/// (이름만 .tsv인 쉼표·파이프 파일). 판별할 구분자가 없으면(컬럼 하나) `preferred`를 쓴다.
/// `preferred`가 없으면 판별하고, 그래도 없으면 쉼표.
fn profile_csv(mut src: impl Read, acc: &mut Accumulator, preferred: Option<u8>) -> Result<(), String> {
    // 구분자를 정하려고 앞부분을 먼저 읽고, 그 뒤에 나머지를 이어 붙여 CSV 파서에 넘긴다
    let mut prefix = Vec::with_capacity(SNIFF_BYTES);
    src.by_ref()
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(read_err)?;
    let complete = prefix.len() < SNIFF_BYTES;
    let splits_header = |d: u8| split_stats(&prefix, d, complete).0.first().is_some_and(|&n| n >= 2);
    let delimiter = match preferred {
        Some(d) if splits_header(d) => d,
        Some(d) => find_delimiter(&prefix, complete).unwrap_or(d),
        None => sniff_delimiter(&prefix, complete),
    };
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(Cursor::new(prefix).chain(src));
    let headers = rdr
        .byte_headers()
        .map_err(|e| trf!("CSV 헤더 오류: {}", "CSV header error: {}", csv_error_text(&e)))?
        .clone();
    let idx: Vec<usize> = headers
        .iter()
        .map(|h| acc.col_unique(&String::from_utf8_lossy(h)))
        .collect();
    for rec in rdr.byte_records() {
        let rec = rec.map_err(|e| trf!("CSV 파싱 오류: {}", "CSV parse error: {}", csv_error_text(&e)))?;
        let row = rec
            .iter()
            .enumerate()
            .filter_map(|(i, v)| idx.get(i).map(|&c| (c, String::from_utf8_lossy(v).into_owned())))
            .collect();
        acc.add_row(row);
    }
    Ok(())
}

/// 파일 확장자에 따라 JSON(.json/.jsonl/.ndjson) 또는 구분자 텍스트로 프로파일링한다. 구분자는 .tsv면 탭
/// (헤더에 탭이 없고 다른 구분자가 헤더를 나누면 그것), 그 밖에는 판별한다.
pub fn profile_file(path: &Path, sample_rows: usize) -> Result<DataSubAst, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "kiro" {
        return Err(tr!(
            "블루프린트(.kiro) 파일은 데이터가 아닙니다. 블루프린트 열기로 여세요",
            "a blueprint (.kiro) file is not data. Open it as a blueprint instead"
        )
        .into());
    }
    let src = open_decoded(path, &ext)?;
    let mut acc = Accumulator::new(sample_rows);
    match ext.as_str() {
        "json" | "jsonl" | "ndjson" => profile_json(src, &mut acc)?,
        "tsv" => profile_csv(src, &mut acc, Some(b'\t'))?,
        _ => profile_csv(src, &mut acc, None)?,
    }
    if acc.rows == 0 {
        return Err(tr!("데이터 행이 없음", "no data rows").into());
    }
    Ok(acc.finish(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// 프로세스마다 다른 임시 파일 (다른 작업 폴더의 테스트가 동시에 돌아도 겹치지 않게)
    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cc_prof_{}_{name}", std::process::id()))
    }

    fn profile_bytes(name: &str, bytes: &[u8], k: usize) -> Result<DataSubAst, String> {
        let p = tmp(name);
        std::fs::write(&p, bytes).unwrap();
        let r = profile_file(&p, k);
        let _ = std::fs::remove_file(&p);
        r
    }

    /// 파일과 같은 방식(앞 64KiB + 나머지)으로 디코딩한 전체 텍스트
    fn decode_all(bytes: &[u8]) -> Result<String, String> {
        let split = bytes.len().min(SNIFF_BYTES);
        let mut r = decoded_reader(bytes[..split].to_vec(), &bytes[split..], "csv")?;
        let mut s = String::new();
        r.read_to_string(&mut s).map_err(|e| e.to_string())?;
        Ok(s)
    }

    fn names(d: &DataSubAst) -> Vec<&str> {
        d.columns.iter().map(|c| c.name.as_str()).collect()
    }

    fn utf16(s: &str, le: bool) -> Vec<u8> {
        let bom: [u8; 2] = if le { [0xFF, 0xFE] } else { [0xFE, 0xFF] };
        let units = s
            .encode_utf16()
            .flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() });
        bom.into_iter().chain(units).collect()
    }

    #[test]
    fn profiles_csv_with_outlier_and_nulls() {
        let path = tmp("outlier.csv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "id,amount,city").unwrap();
        for i in 0..50 {
            writeln!(f, "{i},{},{}", 100 + i % 3, if i % 10 == 0 { "" } else { "Seoul" }).unwrap();
        }
        writeln!(f, "99,100000,Busan").unwrap();
        drop(f);
        let d = profile_file(&path, 3).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.total_rows, 51);
        assert_eq!(d.sample_rows.len(), 3);
        assert!(d.anomalies.iter().any(|a| a.starts_with("amount: max")));
        assert!(d.columns[2].null_ratio > 0.0);
    }

    #[test]
    fn reads_cp949_and_streams_json_array() {
        let (bytes, _, _) = EUC_KR.encode("id,이름\n1,홍길동\n2,김철수\n");
        let d = profile_bytes("cp949.csv", &bytes, 2).unwrap();
        assert_eq!(d.columns[1].name, "이름");
        assert!(
            d.sample_rows
                .iter()
                .any(|r| r.contains("홍길동") || r.contains("김철수"))
        );

        let d = profile_bytes("arr.json", br#"[{"a":1},{"a":2,"b":"x"}]"#, 2).unwrap();
        assert_eq!((d.total_rows, d.columns.len()), (2, 2));
        // 여러 줄 배열과 앞쪽 빈 줄
        let d = profile_bytes("arr_pretty.json", b"\n\n  [\n {\"a\": 1},\n {\"a\": 2}\n]\n", 0).unwrap();
        assert_eq!(d.total_rows, 2);
    }

    #[test]
    fn id_columns_are_not_flagged() {
        let mut s = String::from("order_id,v\n");
        for i in 0..30 {
            s += &format!("{},{}\n", if i == 29 { 9_000_000 } else { i }, i % 3);
        }
        assert!(profile_bytes("id.csv", s.as_bytes(), 1).unwrap().anomalies.is_empty());
    }

    #[test]
    fn utf16_with_bom_decodes_names_and_values() {
        for le in [true, false] {
            let bytes = utf16("이름,금액\r\n가,1\r\n나,2\r\n", le);
            let d = profile_bytes(&format!("u16_{le}.csv"), &bytes, 2).unwrap();
            assert_eq!(names(&d), ["이름", "금액"], "le={le}");
            assert_eq!(d.columns[1].dtype, "numeric");
            assert!(
                d.sample_rows.iter().any(|r| r == "가,1" || r == "나,2"),
                "{:?}",
                d.sample_rows
            );
        }
        // UTF-16 TSV (엑셀 "유니코드 텍스트")
        let d = profile_bytes("u16.txt", &utf16("a\tb\n1\t2\n", true), 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
    }

    #[test]
    fn cp949_rows_after_64k_ascii_prefix() {
        let mut text = String::from("id,memo\n");
        let mut n = 0;
        while text.len() < 86 * 1024 {
            n += 1;
            text += &format!("{n},{}\n", "a".repeat(80));
        }
        for i in 0..100 {
            text += &format!("{},홍길동{i}\n", n + 1 + i);
        }
        let (cp, _, unmappable) = EUC_KR.encode(&text);
        assert!(!unmappable);
        assert!(cp[..SNIFF_BYTES].is_ascii(), "앞 64KiB는 ASCII여야 시험이 된다");
        assert_eq!(decode_all(&cp).unwrap(), text);
        let d = profile_bytes("late.csv", &cp, 50).unwrap();
        assert_eq!(d.total_rows, n + 100);
        let all = d.sample_rows.join("\n");
        assert!(all.contains("홍길동") && !all.contains('\u{FFFD}'), "{all}");

        // NDJSON은 예전에 "stream did not contain valid UTF-8"로 실패했다
        let mut nd = String::new();
        for i in 0..1200 {
            nd += &format!("{{\"id\":{i},\"t\":\"{}\"}}\n", "b".repeat(60));
        }
        nd += "{\"id\":9999,\"t\":\"김철수\"}\n";
        let (cp, _, _) = EUC_KR.encode(&nd);
        let d = profile_bytes("late.ndjson", &cp, 0).unwrap();
        assert_eq!(d.total_rows, 1201);
    }

    #[test]
    fn multibyte_text_split_across_chunks() {
        // ASCII 뒤에 오는 한글이 변환 버퍼 경계(32KiB, 64KiB)를 여러 번 걸치게 한다
        let mut text = "x".repeat(SNIFF_BYTES - 1);
        for i in 0..20_000 {
            text += if i % 7 == 0 { "\n" } else { "가나다라" };
        }
        let (cp, _, _) = EUC_KR.encode(&text);
        assert_eq!(decode_all(&cp).unwrap(), text);
        assert_eq!(decode_all(text.as_bytes()).unwrap(), text);
        // 앞부분부터 UTF-8 한글이면 처음부터 UTF-8로 읽는다
        let early = format!("가{}", &text[SNIFF_BYTES - 1..]);
        assert_eq!(decode_all(early.as_bytes()).unwrap(), early);
    }

    #[test]
    fn utf8_file_keeps_utf8_and_replaces_bad_bytes() {
        // 올바른 UTF-8 한글이 훨씬 많고 잘못된 바이트가 조금 있으면 손상된 UTF-8로 본다
        let mut bytes = "이름,메모\n가,데이터 품질 점검\n".as_bytes().to_vec();
        bytes.extend_from_slice(b"\xff\xfe,2\n");
        let s = decode_all(&bytes).unwrap();
        assert!(
            s.starts_with("이름,메모\n가,데이터 품질 점검\n") && s.contains('\u{FFFD}'),
            "{s}"
        );
        // 64KiB 뒤에 나오는 잘못된 바이트도 마찬가지
        let mut late = "이름\n".repeat(SNIFF_BYTES / 7 + 10).into_bytes();
        late.extend_from_slice(b"\xc0\n");
        assert!(decode_all(&late).unwrap().ends_with("이름\n\u{FFFD}\n"));
        // 한글이 아닌 UTF-8(°, —, é)에 깨진 바이트 하나가 섞여도 CP949로 바꿔 읽지 않는다
        let mut latin = "x".repeat(70_000);
        for _ in 0..200 {
            latin += "25°C — café\n";
        }
        let mut latin = latin.into_bytes();
        latin.extend_from_slice(b"\xa0\n");
        let s = decode_all(&latin).unwrap();
        assert!(
            s.contains("25°C — café\n") && s.ends_with("\u{FFFD}\n"),
            "{}",
            &s[s.len() - 40..]
        );
        // UTF-8 BOM은 떼고 읽는다
        let d = profile_bytes("bom.csv", "\u{feff}이름,값\n가,1\n".as_bytes(), 1).unwrap();
        assert_eq!(names(&d), ["이름", "값"]);
        assert!(prefers_utf8("가나".as_bytes(), true));
        assert!(!prefers_utf8(&EUC_KR.encode("가나다 이상치 탐지").0, true));
        // "홍길동"의 CP949 바이트는 UTF-8로 읽으면 일부 글자가 우연히 맞지만(ȫ, 浿) 오류가 훨씬 많다
        assert!(!prefers_utf8(&EUC_KR.encode("홍길동\n홍길동\n").0, true));
        // 창 끝에서 잘린 글자는 파일 끝이 아니면 오류로 세지 않는다
        assert!(prefers_utf8(&"가나".as_bytes()[..4], false));
    }

    #[test]
    fn non_ascii_cut_at_the_64k_boundary_is_decided_later() {
        // 첫 비ASCII 바이트가 앞 64KiB 끝에 걸치는 경우(CP949 한 글자의 첫 바이트만, 또는 한 글자만 들어감):
        // 앞부분만 보고 UTF-8로 정하면 뒤의 CP949가 모두 깨진다
        for start in SNIFF_BYTES - 4..=SNIFF_BYTES + 1 {
            let mut text = "a".repeat(start) + "홍길동 황";
            for i in 0..300 {
                text += &format!("\n{i},홍길동 황");
            }
            let (cp, _, _) = EUC_KR.encode(&text);
            assert_eq!(decode_all(&cp).unwrap(), text, "cp949 start={start}");
            assert_eq!(decode_all(text.as_bytes()).unwrap(), text, "utf8 start={start}");
        }
    }

    #[test]
    fn binary_and_blueprint_files_are_rejected() {
        let png = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-256.png")).unwrap();
        for name in ["icon.png", "icon.csv", "icon", "icon.json"] {
            let e = profile_bytes(name, &png, 3).unwrap_err();
            assert!(e.contains("바이너리 파일은 프로파일링할 수 없습니다"), "{name}: {e}");
        }
        let e = profile_bytes("book.xlsx", b"PK\x03\x04\x14\x00\x00\x00", 0).unwrap_err();
        assert!(e.contains("엑셀"), "{e}");
        let e = profile_bytes("text_nul.csv", b"a,b\n1,2\n\x003,4\n", 0).unwrap_err();
        assert!(e.contains("바이너리"), "{e}");
        let e = profile_bytes("plan.kiro", br#"{"version":2,"role":"r"}"#, 0).unwrap_err();
        assert!(e.contains("블루프린트"), "{e}");
        // .txt, 확장자 없는 CSV는 그대로 된다
        for name in ["d.txt", "d"] {
            let d = profile_bytes(name, b"a,b\n1,2\n", 0).unwrap();
            assert_eq!((d.total_rows, d.columns.len()), (1, 2), "{name}");
        }
    }

    #[test]
    fn empty_json_key_is_one_stable_column() {
        let mut nd = String::new();
        for i in 0..50 {
            nd += &format!("{{\"a\":{i},\"\":{i},\"col_2\":{i}}}\n");
        }
        nd += "{\" \":1}\n";
        let d = profile_bytes("empty_key.ndjson", nd.as_bytes(), 0).unwrap();
        // 빈 키는 2번째 자리라 col_2지만 실제 키 "col_2"와 겹치므로 col_2_2가 된다
        assert_eq!(names(&d), ["a", "col_2_2", "col_2", "col_4"]);
        assert_eq!(d.columns[1].null_ratio, d.columns[2].null_ratio);

        // CSV: 빈 헤더가 뒤에 오는 실제 헤더 "col_1"과 겹쳐도 실제 이름이 이긴다
        let d = profile_bytes("empty_head.csv", b",col_1,\n1,2,3\n", 0).unwrap();
        assert_eq!(names(&d), ["col_1_2", "col_1", "col_3"]);
    }

    #[test]
    fn commas_only_as_thousands_separators() {
        for (s, want) in [
            ("1,234", Some(1234.0)),
            ("1,234.5", Some(1234.5)),
            ("-12,345,678", Some(-12_345_678.0)),
            ("+999", Some(999.0)),
            ("1e5", Some(1e5)),
            ("0.25", Some(0.25)),
            ("1,5", None),
            ("1,2,3", None),
            ("1234,567", None),
            (",123", None),
            ("1,23", None),
            ("1,234.", None),
            ("1,234.5e3", None),
            ("12,34a", None),
            ("inf", None),
            ("1e300", None),
        ] {
            assert_eq!(parse_number(s), want, "{s}");
        }
        let d = profile_bytes("commas.csv", b"v,w\n\"1,5\",\"1,000\"\n\"1,2,3\",\"2,500.5\"\n", 0).unwrap();
        assert_eq!(d.columns[0].dtype, "text");
        assert_eq!(
            d.columns[1].summary_stats,
            "min:1000, max:2500.5, mean:1750.25, std:750.25"
        );
    }

    #[test]
    fn small_values_keep_significant_digits() {
        for (x, want) in [
            (0.0001, "0.0001"),
            (0.00025, "0.00025"),
            (0.000111803, "0.0001118"),
            (0.5, "0.5"),
            (0.123456, "0.1235"),
            (-0.0042, "-0.0042"),
            (4423.7536, "4423.754"),
            (2.5, "2.5"),
            (0.00001234, "1.234e-5"),
            (3.0, "3"),
            (0.0, "0"),
        ] {
            assert_eq!(fmt(x), want, "{x}");
        }
        let d = profile_bytes("tiny.csv", b"v\n0.0001\n0.0002\n0.0003\n0.0004\n", 0).unwrap();
        assert_eq!(
            d.columns[0].summary_stats,
            "min:0.0001, max:0.0004, mean:0.00025, std:0.0001118"
        );
    }

    #[test]
    fn id_naming_patterns() {
        for n in [
            "id",
            "ID",
            "order_id",
            "CustomerID",
            "customer_id",
            "id_order",
            "orderId",
            "Order ID",
            "No.",
            "no",
            "고객번호",
            "고객ID",
            "상품코드",
            "user-uuid",
            "zip_code",
            "row_idx",
            "id_2",
            "#",
            "순번",
            "customerid",
            "USERID",
            "uid",
            "orderid_2",
        ] {
            assert!(looks_like_id(n), "{n}");
        }
        for n in [
            "amount",
            "paid",
            "valid",
            "idle_time",
            "video_count",
            "no_show",
            "금액",
            "android",
            "key_metric",
            "col_3",
            "bid",
            "lid",
            "mid",
            "rapid",
            "squid",
        ] {
            assert!(!looks_like_id(n), "{n}");
        }
        // 식별자 이름마다 극단값을 넣어도 이상치로 잡지 않는다. 숫자 컬럼 v의 극단값은 잡는다.
        let ids = ["CustomerID", "customer_id", "id_order", "orderId", "No.", "고객번호"];
        let mut s = format!("{},v\n", ids.join(","));
        for i in 1..=40 {
            s += &format!("{},{}\n", vec![i.to_string(); ids.len()].join(","), 100 + i % 3);
        }
        s += &format!("{},100000\n", vec!["9000000"; ids.len()].join(","));
        let d = profile_bytes("ids.csv", s.as_bytes(), 0).unwrap();
        assert_eq!(d.anomalies.len(), 1, "{:?}", d.anomalies);
        assert!(d.anomalies[0].starts_with("v: max 100000"));
    }

    #[test]
    fn sales_sample_reports_exactly_one_amount_outlier() {
        let d = profile_file(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/sales.csv")), 8).unwrap();
        assert_eq!(d.total_rows, 301);
        assert_eq!(d.anomalies, ["amount: max 900000 (z=17.3)"]);
    }

    #[test]
    fn sniffs_semicolon_tab_pipe_and_crlf() {
        let d = profile_bytes(
            "semi.csv",
            b"id;amount;note\r\n1;10;a,b\r\n2;20;c\r\n3;30;\"d;e\"\r\n",
            3,
        )
        .unwrap();
        assert_eq!(names(&d), ["id", "amount", "note"]);
        assert_eq!(d.columns[1].summary_stats, "min:10, max:30, mean:20, std:8.165");
        // 헤더 안의 쉼표보다 행마다 맞는 세미콜론을 고른다
        let d = profile_bytes("semi2.csv", b"Preis, netto;Menge\n1,5;3\n2,25;4\n", 0).unwrap();
        assert_eq!(names(&d), ["Preis, netto", "Menge"]);
        // .tsv는 항상 탭, .txt는 판별
        let d = profile_bytes("t.tsv", b"a,b\tc\n1,2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a,b", "c"]);
        let d = profile_bytes("t.txt", b"a\tb\tc\n1\t2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b", "c"]);
        let d = profile_bytes("p.csv", b"a|b\n1|2\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
        // 세미콜론이 텍스트 안에만 있는 쉼표 파일, 컬럼 하나짜리 파일
        let d = profile_bytes("c.csv", b"a,b\n\"x;y\",1\nz;w,2\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
        let d = profile_bytes("one.csv", b"amount\r\n1\r\n2\r\n", 0).unwrap();
        assert_eq!(names(&d), ["amount"]);
        assert_eq!(d.total_rows, 2);
        assert_eq!(sniff_delimiter(b"\"x,y\";b\n1;2\n", true), b';');
        assert_eq!(sniff_delimiter(b"a,\"q\"\"t;\",c\n", true), b',');
    }

    /// 리뷰 재현: 쉼표·파이프로 구분한 파일 이름이 x.tsv면 "id,region,amount" 한 컬럼으로 읽혔다(경고도 없었다).
    #[test]
    fn tsv_without_tabs_in_the_header_uses_the_sniffed_delimiter() {
        for (name, bytes) in [
            ("x.tsv", b"id,region,amount\n1,A,10\n2,B,20\n3,C,30\n".as_slice()),
            ("p.tsv", b"id|region|amount\n1|A|10\n2|B|20\n".as_slice()),
            ("s.tsv", b"id;region;amount\r\n1;A;10\r\n".as_slice()),
        ] {
            let d = profile_bytes(name, bytes, 0).unwrap();
            assert_eq!(names(&d), ["id", "region", "amount"], "{name}");
            assert_eq!(d.columns[2].dtype, "numeric", "{name}");
        }
        // 헤더에 탭이 있으면 쉼표가 있어도 탭이다
        let d = profile_bytes("t2.tsv", b"a,b\tc\n1,2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a,b", "c"]);
        // 어느 구분자도 헤더를 나누지 못하는 한 컬럼 TSV는 값 속 쉼표에서 나누지 않는다
        let d = profile_bytes("one.tsv", b"memo\nhello, world\nbye, now\n", 2).unwrap();
        assert_eq!(names(&d), ["memo"]);
        assert!(
            d.sample_rows.iter().any(|r| r.contains("hello, world")),
            "{:?}",
            d.sample_rows
        );
    }

    /// 리뷰 재현: 400행 UTF-8 한글 CSV에 0xFF 한 바이트가 섞이면 프로파일러는 UTF-8로 읽는데 토큰 추정·steering
    /// 읽기의 엄격한 판정(looks_utf8)은 CP949로 읽었다. 모두 `detect_text_encoding` 하나를 쓴다.
    #[test]
    fn shared_encoding_detector_matches_the_profiler() {
        let mut text = String::from("id,지역,금액,메모\n");
        for i in 1..=400 {
            text += &format!("{i},서울특별시 강남구,{},정상 거래 확인 완료 메모 {i}\n", 1000 + i * 7);
        }
        let mut damaged = text.clone().into_bytes();
        let at = text.find("\n1,").unwrap() + 1;
        damaged.insert(at, 0xFF);
        assert_eq!(detect_text_encoding(text.as_bytes(), true), UTF_8);
        assert_eq!(detect_text_encoding(&damaged, true), UTF_8);
        assert!(!looks_utf8(&damaged), "예전 판정은 이 파일을 UTF-8로 보지 않았다");
        let d = profile_bytes("damaged.csv", &damaged, 2).unwrap();
        assert_eq!(names(&d), ["id", "지역", "금액", "메모"]);
        assert_eq!(decode_all(&damaged).unwrap(), UTF_8.decode_with_bom_removal(&damaged).0);
        // CP949, BOM, ASCII
        let (cp, _, _) = EUC_KR.encode(&text);
        assert_eq!(detect_text_encoding(&cp, true), EUC_KR);
        assert_eq!(decode_all(&cp).unwrap(), text);
        assert_eq!(detect_text_encoding(&utf16("가,나", true), true), UTF_16LE);
        assert_eq!(detect_text_encoding(&utf16("가,나", false), true), UTF_16BE);
        assert_eq!(detect_text_encoding("\u{feff}가".as_bytes(), true), UTF_8);
        assert_eq!(detect_text_encoding(b"plain ascii", true), UTF_8);
        assert_eq!(detect_text_encoding(b"", true), UTF_8);
        // 64KiB가 넘는 ASCII 앞부분 뒤의 CP949도 프로파일러처럼 찾는다
        let late = format!("{}\n홍길동,김철수\n", "a".repeat(SNIFF_BYTES + 10));
        assert_eq!(detect_text_encoding(&EUC_KR.encode(&late).0, true), EUC_KR);
        // 앞부분만 읽은 버퍼 끝에서 잘린 글자는 오류로 세지 않는다
        assert_eq!(detect_text_encoding(&"가나".as_bytes()[..4], false), UTF_8);
    }

    fn has_hangul(s: &str) -> bool {
        s.chars().any(|c| ('가'..='힣').contains(&c))
    }

    /// 영어 모드의 오류 문구는 영어다. 한국어 Windows가 돌려주는 운영체제 문구("지정된 파일을 찾을 수 없습니다")도
    /// 영어로 바꾸고, 오류 번호는 그대로 둔다. 한국어 모드는 예전 문구 그대로다.
    #[test]
    fn english_mode_errors_are_english() {
        use crate::i18n::{Lang, with_lang};
        let png = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon-256.png")).unwrap();
        let missing = tmp("en_missing.csv");
        let _ = std::fs::remove_file(&missing);
        let errors = with_lang(Lang::En, || {
            vec![
                profile_bytes("en_icon.png", &png, 3).unwrap_err(),
                profile_bytes("en_book.xlsx", b"PK\x03\x04\x14\x00\x00\x00", 0).unwrap_err(),
                profile_bytes("en_plan.kiro", br#"{"version":2,"role":"r"}"#, 0).unwrap_err(),
                profile_bytes("en_header_only.csv", b"a,b\n", 0).unwrap_err(),
                profile_bytes("en_bad.ndjson", b"{\"a\":1}\n\n{oops}\n", 0).unwrap_err(),
                profile_bytes("en_bad.json", b"[{\"a\":1},", 0).unwrap_err(),
                profile_file(&missing, 0).unwrap_err(),
            ]
        });
        assert_eq!(
            errors[0],
            "cannot profile a binary file (contains NUL bytes; only CSV/TSV/JSON/NDJSON text files are supported)"
        );
        assert_eq!(
            errors[1],
            "cannot profile a binary file (contains NUL bytes; save Excel files as CSV first)"
        );
        assert!(
            errors[2].starts_with("a blueprint (.kiro) file is not data."),
            "{}",
            errors[2]
        );
        assert_eq!(errors[3], "no data rows");
        assert!(errors[4].starts_with("JSON error on line 3: "), "{}", errors[4]);
        assert!(errors[5].starts_with("JSON parse error: "), "{}", errors[5]);
        assert_eq!(errors[6], "cannot open file: no such file or directory (os error 2)");
        for e in &errors {
            assert!(!has_hangul(e), "{e}");
        }
        // 한국어 모드: 운영체제 문구를 그대로 붙인다
        let os = File::open(&missing).unwrap_err();
        let ko = with_lang(Lang::Ko, || profile_file(&missing, 0).unwrap_err());
        assert_eq!(ko, format!("파일을 열 수 없음: {os}"));
        assert_eq!(
            with_lang(Lang::Ko, || profile_bytes("ko_header_only.csv", b"a,b\n", 0)
                .unwrap_err()),
            "데이터 행이 없음"
        );
    }

    #[test]
    fn io_error_text_follows_the_language() {
        use crate::i18n::{Lang, with_lang};
        let not_found = std::io::Error::from_raw_os_error(2);
        let custom = std::io::Error::other("stream broke");
        with_lang(Lang::En, || {
            assert_eq!(io_error_text(&not_found), "no such file or directory (os error 2)");
            assert_eq!(io_error_text(&custom), "stream broke");
            if cfg!(windows) {
                let locked = std::io::Error::from_raw_os_error(32);
                assert_eq!(
                    io_error_text(&locked),
                    "the file is being used by another process (os error 32)"
                );
            }
            // 그 밖의 운영체제 오류는 오류 종류의 영어 이름과 번호
            let denied = std::io::Error::from_raw_os_error(if cfg!(windows) { 5 } else { 13 });
            assert!(io_error_text(&denied).starts_with("permission denied (os error "));
        });
        with_lang(Lang::Ko, || {
            assert_eq!(io_error_text(&not_found), not_found.to_string());
            assert_eq!(io_error_text(&custom), "stream broke");
        });
    }

    /// 프로파일 결과(컬럼 이름·타입, 통계, 이상치, 샘플 행)는 프롬프트에 들어가므로 UI 언어와 관계없이 같다.
    #[test]
    fn profile_is_the_same_in_every_language() {
        use crate::i18n::{Lang, with_lang};
        let mut csv = String::from("고객번호,금액,지역,메모\n");
        for i in 0..40 {
            csv += &format!("{i},{},서울,\"정상, {i}\"\n", 100 + i % 5);
        }
        csv += "99,90000,부산,\n";
        let profile = |lang| {
            with_lang(lang, || {
                let d = profile_bytes("lang.csv", csv.as_bytes(), 5).unwrap();
                serde_json::to_string(&d).unwrap()
            })
        };
        let (ko, en) = (profile(Lang::Ko), profile(Lang::En));
        assert_eq!(ko, en);
        // 이상치 문구가 실제로 들어 있고(식별자 컬럼 "고객번호"는 빠진다), 모델용 문구는 영어 표기다
        assert!(
            en.contains("금액: max 90000 (z=") && !en.contains("고객번호: max"),
            "{en}"
        );
        assert!(en.contains("\"dtype\":\"numeric\"") && en.contains("distinct:"), "{en}");
    }
}
