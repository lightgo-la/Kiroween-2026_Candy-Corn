//! Streaming profiler for CSV / TSV / JSON (arrays, NDJSON).
//! Statistics (Welford) are accumulated row by row, and reservoir sampling picks sample rows within the token budget.
//!
//! Encoding: a BOM (UTF-8, UTF-16LE, UTF-16BE) is followed as is. Without one, the first 64 KiB decide whether the
//! file is binary, and 64 KiB from the first non-ASCII byte decide between UTF-8 and CP949 (the same decision as
//! `detect_text_encoding`, so the token estimate and steering reads get the same result). The whole file is never
//! loaded into memory.
//!
//! The profile (column names and types, `summary_stats`, `anomalies`, sample rows) is model-facing text that goes into
//! the prompt, so it is the same in every UI language. Only the error messages shown to the user are translated.

use crate::ast::{ColumnProfile, DataSubAst};
use crate::lexicon;
use encoding_rs::{EUC_KR, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Chain, Cursor, Read};
use std::path::Path;

const DISTINCT_CAP: usize = 1000;
const CELL_MAX_CHARS: usize = 40;
/// Values larger than this overflow the variance computation, so they are left out of the numeric statistics.
const MAX_ABS: f64 = 1e15;
/// Size of the head used to detect binary files, the encoding and the delimiter.
/// When the head is all ASCII, this many bytes from the first non-ASCII byte are looked at to decide the encoding.
const SNIFF_BYTES: usize = 64 * 1024;
/// How much the transcoding reader reads from the source at a time
const CHUNK_BYTES: usize = 32 * 1024;
/// Number of records used to detect the delimiter (including the header)
const SNIFF_RECORDS: usize = 21;

#[derive(Default)]
struct ColAcc {
    /// The real name. Empty when `synthetic`; `finish` names it `col_N`.
    name: String,
    /// A CSV header or JSON key that was blank
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
            // Rounding errors can make m2 a tiny negative number, which would give NaN, so clamp it at 0
            (self.m2 / self.numeric as f64).max(0.0).sqrt()
        } else {
            0.0
        }
    }
}

/// Reads a number. Commas are allowed only as thousands separators (first group 1-3 digits, the rest exactly 3):
/// "1,234.5" is 1234.5, but "1,5", "1,2,3" and "1234,567" are not numbers.
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
    /// Real name → column
    index: HashMap<String, usize>,
    /// Blank (whitespace-only) JSON key → column. The same key is the same column in whatever row it appears.
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

    /// Finds the column of a JSON key, creating it if needed. Each blank key also stays tied to one column (no new
    /// column per row).
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

    /// For CSV headers: a repeated name becomes a separate column told apart like `name_2`. Each blank header is a
    /// separate column.
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

    /// Display names. Blank headers and keys get `col_N` from their position, moving aside to `col_N_2` and so on
    /// when that clashes with a real name.
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

/// Formats a statistic. Integers are written as is. Absolute values of 1 or more get up to 3 decimals and smaller
/// values up to 4 significant digits, so small values such as 0.0001 do not collapse to 0. Very small or large values
/// use exponent notation.
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

/// Identifier columns such as `order_id`, `CustomerID`, `id_order`, `orderId`, `customerid`, `No.` or the Korean
/// `gogaek-beonho` (customer number) are left out of outlier detection. The name is split into words; it is an
/// identifier when `id` appears anywhere, when it is a glued `<entity>id`, or when the last word means number, code or
/// key.
fn looks_like_id(name: &str) -> bool {
    let n = name.trim();
    if n == "#" || lexicon::PROFILER_KO_ID_WORDS.iter().any(|k| n.contains(k)) {
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

/// An identifier written as one word (`customerid`, `USERID`, `uid`). To tell it apart from ordinary words such as
/// `paid`, `valid`, `android` and `bid`, it counts only when the part before `id` is a common entity name or a single
/// letter that does not make an ordinary word.
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

/// Splits a column name into lowercase words. It splits at separators (space, `_`, `-`, `.` etc.), camelCase
/// (`orderId`), acronym boundaries (`CustomerID`, `IDNumber`), Hangul/Latin boundaries (Korean `gogaek` + `ID`) and
/// letter/digit boundaries (`id2`).
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

/// True when the head decodes as UTF-8 (a multibyte character cut off at the end of the buffer is allowed). The file
/// encoding is decided by `detect_text_encoding`.
pub fn looks_utf8(buf: &[u8]) -> bool {
    match std::str::from_utf8(buf) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// (encoding, BOM length) when there is a BOM.
fn bom_encoding(bytes: &[u8]) -> Option<(&'static Encoding, usize)> {
    match bytes {
        [0xEF, 0xBB, 0xBF, ..] => Some((UTF_8, 3)),
        [0xFF, 0xFE, ..] => Some((UTF_16LE, 2)),
        [0xFE, 0xFF, ..] => Some((UTF_16BE, 2)),
        _ => None,
    }
}

/// Encoding of text without a BOM (UTF-8 or CP949). Up to 64 KiB from the first non-ASCII byte (the decision window)
/// go through `prefers_utf8`. All-ASCII text is UTF-8 (the same in both encodings).
/// `complete`: whether `bytes` reaches the end of the text. If not, a character cut off at the end is not counted as
/// an error even when the window reaches the end of `bytes`.
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

/// Decides the encoding of text bytes. The profiler, the raw token estimate (tokenizer) and steering document reads
/// (pipeline) all use this decision, so they read the same file as the same characters (one broken byte does not make
/// only one of them read CP949).
/// A BOM (UTF-8, UTF-16LE, UTF-16BE) is followed; without one, 64 KiB from the first non-ASCII byte choose between
/// UTF-8 and CP949 (when invalid bytes are at most 5% of the non-ASCII bytes, it is UTF-8 with scattered damage).
/// When `complete` is false (`bytes` is the head of a file), a character cut off at the end is not counted as an
/// error. Reading with `decode_with_bom_removal` of the returned encoding also strips the BOM.
pub fn detect_text_encoding(bytes: &[u8], complete: bool) -> &'static Encoding {
    match bom_encoding(bytes) {
        Some((enc, _)) => enc,
        None => sniff_encoding(bytes, complete),
    }
}

/// Decides whether to read the decision window (up to 64 KiB from the first non-ASCII byte) as UTF-8.
/// - No invalid UTF-8 bytes: UTF-8.
/// - Invalid bytes that are at most 5% of the non-ASCII bytes: UTF-8 with scattered damage. Reading the whole window
///   as CP949 because of a few broken bytes would ruin intact characters too, as "café" turns into "caf" followed by
///   a Hangul syllable.
/// - Otherwise the encoding with fewer errors (replacement characters). Korean text in CP949 read as UTF-8 errs at
///   almost every character (a name such as "Hong Gil-dong" becomes "ȫ�浿", matching only partly by chance), and
///   Korean text in UTF-8 read as CP949 errs often too.
///
/// When not `complete` (the window does not reach the end of the file), a character cut off at the end of the window
/// is not counted as an error.
fn prefers_utf8(window: &[u8], complete: bool) -> bool {
    let utf8_errors = utf8_error_count(window, complete);
    if utf8_errors == 0 {
        return true;
    }
    let non_ascii = window.iter().filter(|b| !b.is_ascii()).count();
    utf8_errors * 20 <= non_ascii || cp949_error_count(window, complete) >= utf8_errors
}

/// Number of spans that become replacement characters (U+FFFD) when read as UTF-8
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

/// Number of replacement characters when read as CP949 (CP949 has no U+FFFD, so every U+FFFD in the output is an
/// error)
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
        "xlsx" | "xlsm" | "xls" => tr!("save Excel files as CSV first"),
        _ => tr!("only CSV/TSV/JSON/NDJSON text files are supported"),
    };
    trf!("cannot profile a binary file (contains NUL bytes; {hint})", hint = hint)
}

/// Text of an I/O error. Korean uses the operating system's message as is (as before); English is English whatever
/// the OS language, so the messages of Korean Windows ("the system cannot find the file specified", in Korean) do not
/// end up in `--lang en` output. Errors that do not come from the OS (made by a library) keep their own text.
pub fn io_error_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) if crate::i18n::is_en() => {
            let what = if cfg!(windows) && code == 32 {
                // ERROR_SHARING_VIOLATION: another program, such as Excel, has the file locked
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

/// CSV reader error. Byte records are read flexibly, so the errors that actually occur are almost always I/O errors.
fn csv_error_text(e: &csv::Error) -> String {
    match e.kind() {
        csv::ErrorKind::Io(io) => io_error_text(io),
        _ => e.to_string(),
    }
}

fn read_err(e: std::io::Error) -> String {
    trf!("cannot read file: {}", io_error_text(&e))
}

/// Streaming reader that converts the source bytes to UTF-8.
/// Without a `decoder`, only ASCII has appeared so far. At the first non-ASCII byte, the next 64 KiB decide between
/// UTF-8 and CP949. Everything before is ASCII and the same in both encodings, so nothing has to be read again.
/// When the decision is UTF-8, later invalid bytes become U+FFFD (the file already had UTF-8 characters, so they are
/// damage).
struct Transcode<R> {
    inner: R,
    decoder: Option<encoding_rs::Decoder>,
    /// Source bytes not converted yet: `raw[pos..]`
    raw: Vec<u8>,
    pos: usize,
    eof: bool,
    /// Converted UTF-8 bytes: `out[out_pos..]`
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

    /// Reads until at least `want` source bytes are pending or the input ends.
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

    /// Refills `out` with converted bytes. False when all input is done.
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
            // First non-ASCII byte: the next 64 KiB (or up to the end of the file) decide the encoding (the same
            // decision as detect_text_encoding)
            self.fill(SNIFF_BYTES)?;
            let enc = sniff_encoding(&self.raw[self.pos..], self.eof);
            self.decoder = Some(enc.new_decoder_without_bom_handling());
        }
        if self.finished {
            return Ok(false);
        }
        let Some(dec) = self.decoder.as_mut() else {
            return Ok(false); // Not reached: set above
        };
        // Once the input has ended, all remaining source bytes are in raw, so this last call flushes the decoder
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

/// Builds a UTF-8 reader from the head of the source (`head`, up to 64 KiB) and the rest (`rest`).
/// A BOM is followed first; without a BOM, NUL bytes mean a binary file, which is rejected.
fn decoded_reader<R: Read>(head: Vec<u8>, rest: R, ext: &str) -> Result<Decoded<R>, String> {
    let (bom, skip) = match bom_encoding(&head) {
        Some((enc, len)) => (Some(enc), len),
        None => (None, 0),
    };
    let body = &head[skip..];
    // Text never contains NUL. For UTF-16 the decoded characters are checked.
    let binary = match bom {
        Some(enc) if enc != UTF_8 => enc.decode_without_bom_handling(body).0.contains('\0'),
        _ => body.contains(&0),
    };
    if binary {
        return Err(binary_error(ext));
    }
    // Without a BOM the head does not decide in advance: when the only non-ASCII bytes in the head are one or two cut
    // off at the 64 KiB boundary, there is nothing to go on. The transcoding reader decides from 64 KiB starting at the
    // first non-ASCII byte.
    let mut head = Cursor::new(head);
    head.set_position(skip as u64);
    Ok(Transcode::new(head.chain(rest), bom))
}

/// Opens a file as a reader of UTF-8 bytes.
fn open_decoded(path: &Path, ext: &str) -> Result<Decoded<File>, String> {
    let mut f = File::open(path).map_err(|e| trf!("cannot open file: {}", io_error_text(&e)))?;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    f.by_ref()
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(read_err)?;
    decoded_reader(head, f, ext)
}

/// The result of splitting with the delimiter `d`: the field count of each record (at most SNIFF_RECORDS, blank lines
/// skipped) and the number of quote mismatches. Delimiters and line breaks inside a field that starts with a quote are
/// not counted. A character other than a delimiter or a line break after a closing quote, or a quote that never
/// closes, counts as a mismatch (a sign that the file does not fit that delimiter).
/// When not `complete` (only the head was read), a record cut off at the end is dropped.
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
                quoted = true; // Escaped quote ("")
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

/// Chooses the delimiter (tab, semicolon, pipe, comma) from the header and the rows after it.
/// Among the candidates that split the header into 2 or more fields, it picks the one with fewer quote mismatches and
/// then more rows whose field count matches the header. Ties go tab > semicolon > pipe > comma (commas are common in
/// decimals and text). None when no candidate splits the header (one column).
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

/// Like `find_delimiter`, but a comma when no candidate splits the header (one column).
fn sniff_delimiter(text: &[u8], complete: bool) -> u8 {
    find_delimiter(text, complete).unwrap_or(b',')
}

/// Reads a JSON array element by element so the whole array is never in memory.
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

/// A JSON array or NDJSON (one object per line). It is an array when the first character is `[`.
fn profile_json(src: impl Read, acc: &mut Accumulator) -> Result<(), String> {
    use serde::Deserializer as _;
    let mut reader = BufReader::new(src);
    // Find the first character, skipping leading whitespace (and counting the skipped lines to keep line numbers
    // right)
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
        let json_err = |e: serde_json::Error| trf!("JSON parse error: {e}", e = e);
        de.deserialize_seq(ArrayVisitor(acc)).map_err(json_err)?;
        de.end().map_err(json_err)?;
    } else {
        for (n, line) in reader.lines().enumerate() {
            let line = line.map_err(read_err)?;
            if line.trim().is_empty() {
                continue;
            }
            let at = n + 1 + skipped_lines;
            let v: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| trf!("JSON error on line {at}: {e}", at = at, e = e))?;
            add_json_object(acc, &v);
        }
    }
    Ok(())
}

/// Delimited text (CSV/TSV). `preferred` (the tab of .tsv) is used when it splits the header; otherwise the head
/// decides (comma or pipe files that are only named .tsv). When no delimiter can be detected (one column),
/// `preferred` is used. Without `preferred` the delimiter is detected, falling back to a comma.
fn profile_csv(mut src: impl Read, acc: &mut Accumulator, preferred: Option<u8>) -> Result<(), String> {
    // Read the head first to decide the delimiter, then hand head + rest to the CSV parser
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
        .map_err(|e| trf!("CSV header error: {}", csv_error_text(&e)))?
        .clone();
    let idx: Vec<usize> = headers
        .iter()
        .map(|h| acc.col_unique(&String::from_utf8_lossy(h)))
        .collect();
    for rec in rdr.byte_records() {
        let rec = rec.map_err(|e| trf!("CSV parse error: {}", csv_error_text(&e)))?;
        let row = rec
            .iter()
            .enumerate()
            .filter_map(|(i, v)| idx.get(i).map(|&c| (c, String::from_utf8_lossy(v).into_owned())))
            .collect();
        acc.add_row(row);
    }
    Ok(())
}

/// Profiles the file as JSON (.json/.jsonl/.ndjson) or as delimited text depending on the extension. The delimiter is
/// a tab for .tsv (or another delimiter that splits the header when the header has no tab), and detected otherwise.
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
        return Err(tr!("a blueprint (.kiro) file is not data. Open it as a blueprint instead").into());
    }
    let src = open_decoded(path, &ext)?;
    let mut acc = Accumulator::new(sample_rows);
    match ext.as_str() {
        "json" | "jsonl" | "ndjson" => profile_json(src, &mut acc)?,
        "tsv" => profile_csv(src, &mut acc, Some(b'\t'))?,
        _ => profile_csv(src, &mut acc, None)?,
    }
    if acc.rows == 0 {
        return Err(tr!("no data rows").into());
    }
    Ok(acc.finish(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// A temp file unique to the process (so tests from other working folders running at the same time do not
    /// collide)
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

    /// The whole text decoded the same way as a file (64 KiB head + rest)
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
        let (name, hong, kim) = (
            ko_fixture!("core.col.name"),
            ko_fixture!("core.profiler.hong"),
            ko_fixture!("core.profiler.kim"),
        );
        let csv = format!("id,{name}\n1,{hong}\n2,{kim}\n");
        let (bytes, _, _) = EUC_KR.encode(&csv);
        let d = profile_bytes("cp949.csv", &bytes, 2).unwrap();
        assert_eq!(d.columns[1].name, name);
        assert!(d.sample_rows.iter().any(|r| r.contains(hong) || r.contains(kim)));

        let d = profile_bytes("arr.json", br#"[{"a":1},{"a":2,"b":"x"}]"#, 2).unwrap();
        assert_eq!((d.total_rows, d.columns.len()), (2, 2));
        // A multi-line array with leading blank lines
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
        let (name, amount) = (ko_fixture!("core.col.name"), ko_fixture!("core.col.amount"));
        let (ga, na) = (ko_fixture!("core.profiler.ga"), ko_fixture!("core.profiler.na"));
        for le in [true, false] {
            let bytes = utf16(&format!("{name},{amount}\r\n{ga},1\r\n{na},2\r\n"), le);
            let d = profile_bytes(&format!("u16_{le}.csv"), &bytes, 2).unwrap();
            assert_eq!(names(&d), [name, amount], "le={le}");
            assert_eq!(d.columns[1].dtype, "numeric");
            assert!(
                d.sample_rows
                    .iter()
                    .any(|r| *r == format!("{ga},1") || *r == format!("{na},2")),
                "{:?}",
                d.sample_rows
            );
        }
        // UTF-16 TSV (Excel "Unicode Text")
        let d = profile_bytes("u16.txt", &utf16("a\tb\n1\t2\n", true), 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
    }

    #[test]
    fn cp949_rows_after_64k_ascii_prefix() {
        let (hong, kim) = (ko_fixture!("core.profiler.hong"), ko_fixture!("core.profiler.kim"));
        let mut text = String::from("id,memo\n");
        let mut n = 0;
        while text.len() < 86 * 1024 {
            n += 1;
            text += &format!("{n},{}\n", "a".repeat(80));
        }
        for i in 0..100 {
            text += &format!("{},{hong}{i}\n", n + 1 + i);
        }
        let (cp, _, unmappable) = EUC_KR.encode(&text);
        assert!(!unmappable);
        assert!(
            cp[..SNIFF_BYTES].is_ascii(),
            "the first 64 KiB must be ASCII for this test to mean anything"
        );
        assert_eq!(decode_all(&cp).unwrap(), text);
        let d = profile_bytes("late.csv", &cp, 50).unwrap();
        assert_eq!(d.total_rows, n + 100);
        let all = d.sample_rows.join("\n");
        assert!(all.contains(hong) && !all.contains('\u{FFFD}'), "{all}");

        // NDJSON used to fail with "stream did not contain valid UTF-8"
        let mut nd = String::new();
        for i in 0..1200 {
            nd += &format!("{{\"id\":{i},\"t\":\"{}\"}}\n", "b".repeat(60));
        }
        nd += &format!("{{\"id\":9999,\"t\":\"{kim}\"}}\n");
        let (cp, _, _) = EUC_KR.encode(&nd);
        let d = profile_bytes("late.ndjson", &cp, 0).unwrap();
        assert_eq!(d.total_rows, 1201);
    }

    #[test]
    fn multibyte_text_split_across_chunks() {
        // Korean text after ASCII straddles the conversion buffer boundaries (32 KiB, 64 KiB) many times
        let syllables = ko_fixture!("core.profiler.ganadara");
        let mut text = "x".repeat(SNIFF_BYTES - 1);
        for i in 0..20_000 {
            text += if i % 7 == 0 { "\n" } else { syllables };
        }
        let (cp, _, _) = EUC_KR.encode(&text);
        assert_eq!(decode_all(&cp).unwrap(), text);
        assert_eq!(decode_all(text.as_bytes()).unwrap(), text);
        // UTF-8 Korean from the very start is read as UTF-8 from the start
        let early = format!("{}{}", ko_fixture!("core.profiler.ga"), &text[SNIFF_BYTES - 1..]);
        assert_eq!(decode_all(early.as_bytes()).unwrap(), early);
    }

    #[test]
    fn utf8_file_keeps_utf8_and_replaces_bad_bytes() {
        let (name, memo, value) = (
            ko_fixture!("core.col.name"),
            ko_fixture!("core.col.memo"),
            ko_fixture!("core.col.value"),
        );
        let (ga, gana, hong) = (
            ko_fixture!("core.profiler.ga"),
            ko_fixture!("core.profiler.gana"),
            ko_fixture!("core.profiler.hong"),
        );
        // Mostly valid UTF-8 Korean with a few invalid bytes is damaged UTF-8
        let good = format!("{name},{memo}\n{ga},{}\n", ko_fixture!("core.profiler.quality_check"));
        let mut bytes = good.as_bytes().to_vec();
        bytes.extend_from_slice(b"\xff\xfe,2\n");
        let s = decode_all(&bytes).unwrap();
        assert!(s.starts_with(&good) && s.contains('\u{FFFD}'), "{s}");
        // The same for an invalid byte after 64 KiB
        let mut late = format!("{name}\n").repeat(SNIFF_BYTES / 7 + 10).into_bytes();
        late.extend_from_slice(b"\xc0\n");
        assert!(decode_all(&late).unwrap().ends_with(&format!("{name}\n\u{FFFD}\n")));
        // Non-Korean UTF-8 (°, —, é) with one broken byte mixed in is not switched to CP949
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
        // A UTF-8 BOM is stripped
        let d = profile_bytes("bom.csv", format!("\u{feff}{name},{value}\n{ga},1\n").as_bytes(), 1).unwrap();
        assert_eq!(names(&d), [name, value]);
        assert!(prefers_utf8(gana.as_bytes(), true));
        assert!(!prefers_utf8(
            &EUC_KR.encode(ko_fixture!("core.profiler.cp949_sentence")).0,
            true
        ));
        // The CP949 bytes of a Korean name partly decode as UTF-8 by chance (ȫ, 浿), but with far more errors
        assert!(!prefers_utf8(&EUC_KR.encode(&format!("{hong}\n{hong}\n")).0, true));
        // A character cut off at the end of the window is not an error unless the file ends there
        assert!(prefers_utf8(&gana.as_bytes()[..4], false));
    }

    #[test]
    fn non_ascii_cut_at_the_64k_boundary_is_decided_later() {
        // The first non-ASCII byte falls at the end of the first 64 KiB (only the first byte of one CP949 character, or
        // a single character, fits): deciding UTF-8 from the head alone would break all the CP949 after it
        let marker = ko_fixture!("core.profiler.name_and_letter");
        for start in SNIFF_BYTES - 4..=SNIFF_BYTES + 1 {
            let mut text = "a".repeat(start) + marker;
            for i in 0..300 {
                text += &format!("\n{i},{marker}");
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
            assert!(e.contains("cannot profile a binary file"), "{name}: {e}");
        }
        let e = profile_bytes("book.xlsx", b"PK\x03\x04\x14\x00\x00\x00", 0).unwrap_err();
        assert!(e.contains("Excel"), "{e}");
        let e = profile_bytes("text_nul.csv", b"a,b\n1,2\n\x003,4\n", 0).unwrap_err();
        assert!(e.contains("binary"), "{e}");
        let e = profile_bytes("plan.kiro", br#"{"version":2,"role":"r"}"#, 0).unwrap_err();
        assert!(e.contains("blueprint"), "{e}");
        // .txt and CSV without an extension still work
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
        // The blank key is in position 2, so col_2, but that clashes with the real key "col_2", so it becomes col_2_2
        assert_eq!(names(&d), ["a", "col_2_2", "col_2", "col_4"]);
        assert_eq!(d.columns[1].null_ratio, d.columns[2].null_ratio);

        // CSV: when a blank header clashes with a later real header "col_1", the real name wins
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
        let english = [
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
            "user-uuid",
            "zip_code",
            "row_idx",
            "id_2",
            "#",
            "customerid",
            "USERID",
            "uid",
            "orderid_2",
        ];
        for n in english
            .into_iter()
            .chain(ko_fixture!("core.profiler.id_names").iter().copied())
        {
            assert!(looks_like_id(n), "{n}");
        }
        let english = [
            "amount",
            "paid",
            "valid",
            "idle_time",
            "video_count",
            "no_show",
            "android",
            "key_metric",
            "col_3",
            "bid",
            "lid",
            "mid",
            "rapid",
            "squid",
        ];
        for n in english
            .into_iter()
            .chain(ko_fixture!("core.profiler.not_id_names").iter().copied())
        {
            assert!(!looks_like_id(n), "{n}");
        }
        // Extreme values in every identifier column are not flagged as outliers; the extreme value of the numeric
        // column v is.
        let ids = [
            "CustomerID",
            "customer_id",
            "id_order",
            "orderId",
            "No.",
            ko_fixture!("core.col.customer_no"),
        ];
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
        // A semicolon that fits every row wins over the comma inside the header
        let d = profile_bytes("semi2.csv", b"Preis, netto;Menge\n1,5;3\n2,25;4\n", 0).unwrap();
        assert_eq!(names(&d), ["Preis, netto", "Menge"]);
        // .tsv is always a tab, .txt is detected
        let d = profile_bytes("t.tsv", b"a,b\tc\n1,2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a,b", "c"]);
        let d = profile_bytes("t.txt", b"a\tb\tc\n1\t2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b", "c"]);
        let d = profile_bytes("p.csv", b"a|b\n1|2\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
        // A comma file with semicolons only inside text, a one-column file
        let d = profile_bytes("c.csv", b"a,b\n\"x;y\",1\nz;w,2\n", 0).unwrap();
        assert_eq!(names(&d), ["a", "b"]);
        let d = profile_bytes("one.csv", b"amount\r\n1\r\n2\r\n", 0).unwrap();
        assert_eq!(names(&d), ["amount"]);
        assert_eq!(d.total_rows, 2);
        assert_eq!(sniff_delimiter(b"\"x,y\";b\n1;2\n", true), b';');
        assert_eq!(sniff_delimiter(b"a,\"q\"\"t;\",c\n", true), b',');
    }

    /// Review repro: a comma- or pipe-delimited file named x.tsv was read as a single "id,region,amount" column
    /// (without any warning).
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
        // With a tab in the header it is a tab even when there are commas
        let d = profile_bytes("t2.tsv", b"a,b\tc\n1,2\t3\n", 0).unwrap();
        assert_eq!(names(&d), ["a,b", "c"]);
        // A one-column TSV whose header no delimiter splits is not split at commas inside values
        let d = profile_bytes("one.tsv", b"memo\nhello, world\nbye, now\n", 2).unwrap();
        assert_eq!(names(&d), ["memo"]);
        assert!(
            d.sample_rows.iter().any(|r| r.contains("hello, world")),
            "{:?}",
            d.sample_rows
        );
    }

    /// Review repro: with one 0xFF byte in a 400-row UTF-8 Korean CSV, the profiler read it as UTF-8 but the strict
    /// check of the token estimate and steering reads (looks_utf8) read it as CP949. They all use the one
    /// `detect_text_encoding` now.
    #[test]
    fn shared_encoding_detector_matches_the_profiler() {
        let (region, memo) = (
            ko_fixture!("core.korean_csv.region"),
            ko_fixture!("core.korean_csv.memo"),
        );
        let mut text = format!("{}\n", ko_fixture!("core.korean_csv.header"));
        for i in 1..=400 {
            text += &format!("{i},{region},{},{memo} {i}\n", 1000 + i * 7);
        }
        let mut damaged = text.clone().into_bytes();
        let at = text.find("\n1,").unwrap() + 1;
        damaged.insert(at, 0xFF);
        assert_eq!(detect_text_encoding(text.as_bytes(), true), UTF_8);
        assert_eq!(detect_text_encoding(&damaged, true), UTF_8);
        assert!(!looks_utf8(&damaged), "the old check did not see this file as UTF-8");
        let d = profile_bytes("damaged.csv", &damaged, 2).unwrap();
        assert_eq!(names(&d), ko_fixture!("core.korean_csv.columns"));
        assert_eq!(decode_all(&damaged).unwrap(), UTF_8.decode_with_bom_removal(&damaged).0);
        // CP949, BOM, ASCII
        let (cp, _, _) = EUC_KR.encode(&text);
        assert_eq!(detect_text_encoding(&cp, true), EUC_KR);
        assert_eq!(decode_all(&cp).unwrap(), text);
        let (ga, na) = (ko_fixture!("core.profiler.ga"), ko_fixture!("core.profiler.na"));
        assert_eq!(
            detect_text_encoding(&utf16(&format!("{ga},{na}"), true), true),
            UTF_16LE
        );
        assert_eq!(
            detect_text_encoding(&utf16(&format!("{ga},{na}"), false), true),
            UTF_16BE
        );
        assert_eq!(detect_text_encoding(format!("\u{feff}{ga}").as_bytes(), true), UTF_8);
        assert_eq!(detect_text_encoding(b"plain ascii", true), UTF_8);
        assert_eq!(detect_text_encoding(b"", true), UTF_8);
        // CP949 after an ASCII head longer than 64 KiB is found the same way the profiler finds it
        let late = format!(
            "{}\n{},{}\n",
            "a".repeat(SNIFF_BYTES + 10),
            ko_fixture!("core.profiler.hong"),
            ko_fixture!("core.profiler.kim")
        );
        assert_eq!(detect_text_encoding(&EUC_KR.encode(&late).0, true), EUC_KR);
        // A character cut off at the end of a buffer that holds only the head is not an error
        assert_eq!(
            detect_text_encoding(&ko_fixture!("core.profiler.gana").as_bytes()[..4], false),
            UTF_8
        );
    }

    fn has_hangul(s: &str) -> bool {
        s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
    }

    /// Error messages in English mode are English. The OS message that Korean Windows returns ("the system cannot find
    /// the file specified", in Korean) is replaced with English too, and the error number is kept. Korean mode keeps
    /// the old wording.
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
        // Korean mode: the OS message is appended as is
        let os = File::open(&missing).unwrap_err();
        let ko = with_lang(Lang::Ko, || profile_file(&missing, 0).unwrap_err());
        assert_eq!(ko, format!(ko_text!("cannot open file: {}"), os));
        assert_eq!(
            with_lang(Lang::Ko, || profile_bytes("ko_header_only.csv", b"a,b\n", 0)
                .unwrap_err()),
            ko_text!("no data rows")
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
            // Any other OS error gives the English name of the error kind and the number
            let denied = std::io::Error::from_raw_os_error(if cfg!(windows) { 5 } else { 13 });
            assert!(io_error_text(&denied).starts_with("permission denied (os error "));
        });
        with_lang(Lang::Ko, || {
            assert_eq!(io_error_text(&not_found), not_found.to_string());
            assert_eq!(io_error_text(&custom), "stream broke");
        });
    }

    /// The profile (column names and types, statistics, outliers, sample rows) goes into the prompt, so it is the same
    /// in every UI language.
    #[test]
    fn profile_is_the_same_in_every_language() {
        use crate::i18n::{Lang, with_lang};
        let (customer_no, amount) = (ko_fixture!("core.col.customer_no"), ko_fixture!("core.col.amount"));
        let (region, memo) = (ko_fixture!("core.col.region"), ko_fixture!("core.col.memo"));
        let (seoul, busan, normal) = (
            ko_fixture!("core.profiler.seoul"),
            ko_fixture!("core.profiler.busan"),
            ko_fixture!("core.profiler.normal"),
        );
        let mut csv = format!("{customer_no},{amount},{region},{memo}\n");
        for i in 0..40 {
            csv += &format!("{i},{},{seoul},\"{normal}, {i}\"\n", 100 + i % 5);
        }
        csv += &format!("99,90000,{busan},\n");
        let profile = |lang| {
            with_lang(lang, || {
                let d = profile_bytes("lang.csv", csv.as_bytes(), 5).unwrap();
                serde_json::to_string(&d).unwrap()
            })
        };
        let (ko, en) = (profile(Lang::Ko), profile(Lang::En));
        assert_eq!(ko, en);
        // The outlier text is really there (the identifier column, customer number, is left out), and the model-facing
        // wording is English
        assert!(
            en.contains(&format!("{amount}: max 90000 (z=")) && !en.contains(&format!("{customer_no}: max")),
            "{en}"
        );
        assert!(en.contains("\"dtype\":\"numeric\"") && en.contains("distinct:"), "{en}");
    }
}
