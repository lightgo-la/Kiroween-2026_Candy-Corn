//! CSV / JSON(배열, NDJSON) 스트리밍 프로파일러.
//! 행 단위로 통계(Welford)를 누적하고, 리저버 샘플링으로 토큰 예산 내 샘플 행을 뽑는다.

use crate::ast::{ColumnProfile, DataSubAst};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const DISTINCT_CAP: usize = 1000;
const CELL_MAX_CHARS: usize = 40;
/// 이 크기를 넘는 값은 분산 계산이 overflow하므로 숫자 통계에서 제외한다.
const MAX_ABS: f64 = 1e15;

#[derive(Default)]
struct ColAcc {
    name: String,
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
        if let Some(x) = v
            .replace(',', "")
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite() && x.abs() <= MAX_ABS)
        {
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
            (self.m2 / self.numeric as f64).sqrt()
        } else {
            0.0
        }
    }
}

struct Accumulator {
    cols: Vec<ColAcc>,
    index: HashMap<String, usize>,
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
            rows: 0,
            sample_k,
            sample: vec![],
            rng: 0x9E3779B97F4A7C15,
        }
    }

    /// 이름으로 컬럼을 찾고 없으면 만든다. 빈 이름은 `col_N`으로 대체한다.
    fn col(&mut self, name: &str) -> usize {
        let name = if name.trim().is_empty() {
            format!("col_{}", self.cols.len() + 1)
        } else {
            name.to_string()
        };
        if let Some(&i) = self.index.get(&name) {
            return i;
        }
        self.cols.push(ColAcc {
            name: name.clone(),
            ..Default::default()
        });
        self.index.insert(name, self.cols.len() - 1);
        self.cols.len() - 1
    }

    /// CSV 헤더용: 같은 이름이 또 나오면 `name_2`처럼 구분해 별도 컬럼으로 만든다.
    fn col_unique(&mut self, name: &str) -> usize {
        let base = if name.trim().is_empty() {
            format!("col_{}", self.cols.len() + 1)
        } else {
            name.to_string()
        };
        let mut candidate = base.clone();
        let mut n = 2;
        while self.index.contains_key(&candidate) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        self.col(&candidate)
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
        let mut anomalies = vec![];
        let columns: Vec<ColumnProfile> = self
            .cols
            .iter()
            .map(|c| {
                let null_ratio = if rows == 0 {
                    0.0
                } else {
                    (rows - c.present.min(rows)) as f64 / rows as f64
                };
                if c.is_numeric() {
                    let sd = c.std();
                    if c.numeric >= 8 && sd > 0.0 && !looks_like_id(&c.name) {
                        let zmax = (c.max - c.mean) / sd;
                        let zmin = (c.mean - c.min) / sd;
                        if zmax > 3.0 {
                            anomalies.push(format!("{}: max {} (z={:.1})", c.name, fmt(c.max), zmax));
                        }
                        if zmin > 3.0 {
                            anomalies.push(format!("{}: min {} (z={:.1})", c.name, fmt(c.min), zmin));
                        }
                    }
                    ColumnProfile {
                        name: c.name.clone(),
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
                        name: c.name.clone(),
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

fn fmt(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{:.3}", x);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
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

/// `order_id`, `ID`, `고객번호` 같은 식별자 컬럼은 이상치 판정에서 제외한다.
fn looks_like_id(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == "id"
        || n == "no"
        || n.ends_with("_id")
        || n.ends_with(" id")
        || name.trim().ends_with("Id")
        || n.contains("번호")
        || n.contains("코드")
}

/// 앞부분이 UTF-8로 해석되면 true (버퍼 끝에서 잘린 멀티바이트는 허용).
pub fn looks_utf8(buf: &[u8]) -> bool {
    match std::str::from_utf8(buf) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// 파일을 열어 UTF-8로 디코딩하는 리더를 만든다. UTF-8이 아니면 CP949(EUC-KR)로 본다.
/// BOM(UTF-8/UTF-16)은 자동으로 처리된다.
fn open_decoded(path: &Path) -> Result<Box<dyn Read>, String> {
    let mut f = File::open(path).map_err(|e| format!("파일을 열 수 없음: {e}"))?;
    let mut head = vec![0u8; 64 * 1024];
    let n = f.read(&mut head).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let enc = if looks_utf8(&head[..n]) {
        None
    } else {
        Some(encoding_rs::EUC_KR)
    };
    Ok(Box::new(
        encoding_rs_io::DecodeReaderBytesBuilder::new().encoding(enc).build(f),
    ))
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

/// 파일 확장자에 따라 CSV 또는 JSON으로 프로파일링한다.
pub fn profile_file(path: &Path, sample_rows: usize) -> Result<DataSubAst, String> {
    use serde::Deserializer as _;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let reader = BufReader::new(open_decoded(path)?);
    let mut acc = Accumulator::new(sample_rows);
    match ext.as_str() {
        "json" | "jsonl" | "ndjson" => {
            let mut reader = reader;
            let buf = reader.fill_buf().map_err(|e| e.to_string())?;
            let first = buf.iter().find(|b| !b.is_ascii_whitespace()).copied();
            if first == Some(b'[') {
                let mut de = serde_json::Deserializer::from_reader(reader);
                de.deserialize_seq(ArrayVisitor(&mut acc))
                    .map_err(|e| format!("JSON 파싱 실패: {e}"))?;
                de.end().map_err(|e| format!("JSON 파싱 실패: {e}"))?;
            } else {
                for (n, line) in reader.lines().enumerate() {
                    let line = line.map_err(|e| e.to_string())?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: serde_json::Value =
                        serde_json::from_str(&line).map_err(|e| format!("{}번째 줄 JSON 오류: {e}", n + 1))?;
                    add_json_object(&mut acc, &v);
                }
            }
        }
        _ => {
            let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);
            let headers = rdr.byte_headers().map_err(|e| format!("CSV 헤더 오류: {e}"))?.clone();
            let idx: Vec<usize> = headers
                .iter()
                .map(|h| acc.col_unique(&String::from_utf8_lossy(h)))
                .collect();
            for rec in rdr.byte_records() {
                let rec = rec.map_err(|e| format!("CSV 파싱 오류: {e}"))?;
                let row = rec
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| idx.get(i).map(|&c| (c, String::from_utf8_lossy(v).into_owned())))
                    .collect();
                acc.add_row(row);
            }
        }
    }
    if acc.rows == 0 {
        return Err("데이터 행이 없음".into());
    }
    Ok(acc.finish(&name))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn profiles_csv_with_outlier_and_nulls() {
        let dir = std::env::temp_dir().join("candy_corn_test.csv");
        let mut f = File::create(&dir).unwrap();
        writeln!(f, "id,amount,city").unwrap();
        for i in 0..50 {
            writeln!(f, "{i},{},{}", 100 + i % 3, if i % 10 == 0 { "" } else { "Seoul" }).unwrap();
        }
        writeln!(f, "99,100000,Busan").unwrap();
        drop(f);
        let d = profile_file(&dir, 3).unwrap();
        assert_eq!(d.total_rows, 51);
        assert_eq!(d.sample_rows.len(), 3);
        assert!(d.anomalies.iter().any(|a| a.starts_with("amount: max")));
        assert!(d.columns[2].null_ratio > 0.0);
    }

    #[test]
    fn reads_cp949_and_streams_json_array() {
        let dir = std::env::temp_dir();
        let p = dir.join("candy_corn_cp949.csv");
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("id,이름\n1,홍길동\n2,김철수\n");
        std::fs::write(&p, &*bytes).unwrap();
        let d = profile_file(&p, 2).unwrap();
        assert_eq!(d.columns[1].name, "이름");
        assert!(
            d.sample_rows
                .iter()
                .any(|r| r.contains("홍길동") || r.contains("김철수"))
        );

        let j = dir.join("candy_corn_arr.json");
        std::fs::write(&j, r#"[{"a":1},{"a":2,"b":"x"}]"#).unwrap();
        let d = profile_file(&j, 2).unwrap();
        assert_eq!((d.total_rows, d.columns.len()), (2, 2));
    }

    #[test]
    fn id_columns_are_not_flagged() {
        let p = std::env::temp_dir().join("candy_corn_id.csv");
        let mut s = String::from("order_id,v\n");
        for i in 0..30 {
            s += &format!("{},{}\n", if i == 29 { 9_000_000 } else { i }, i % 3);
        }
        std::fs::write(&p, s).unwrap();
        assert!(profile_file(&p, 1).unwrap().anomalies.is_empty());
    }
}
