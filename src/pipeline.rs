//! GUI / CLI / MCP가 공유하는 컴파일 파이프라인: AST 구성 → 린트 → 직렬화 → 토큰 계산.

use crate::ast::{
    ConstraintNode, DataSubAst, OutputFormat, OutputSchemaAst, RoleNode, RootPromptAst, TargetModel, codegen, linter,
};
use crate::canvas::llm::{Finish, LlmReply, Usage};
use crate::{profiler::csv_profiler, tokenizer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

pub const DEFAULT_FIELDS: &str = "root_causes,outliers,recommendations";

#[derive(Clone, Debug)]
pub struct LoadedData {
    pub path: std::path::PathBuf,
    pub data: DataSubAst,
    pub raw_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct CompileRequest {
    pub target: TargetModel,
    pub role: String,
    pub constraints: Vec<String>,
    pub format: OutputFormat,
    pub fields: Vec<String>,
    pub data: Option<LoadedData>,
}

#[derive(Clone, Debug)]
pub struct CompileResult {
    pub ast: RootPromptAst,
    pub output: String,
    pub orig_tokens: usize,
    pub comp_tokens: usize,
    pub saving_pct: f64,
    pub saved_usd: f64,
    pub preview: String,
}

pub fn load_data(path: &Path, sample_rows: usize) -> Result<LoadedData, String> {
    let data = csv_profiler::profile_file(path, sample_rows)?;
    let (raw_tokens, _) = raw_file_tokens(path)?;
    Ok(LoadedData {
        path: path.to_path_buf(),
        data,
        raw_tokens,
    })
}

/// 원본 파일 토큰 추정치 캐시의 키: (정규화한 경로, 파일 길이, 수정 시각).
type RawTokenKey = (PathBuf, u64, SystemTime);
static RAW_TOKENS: LazyLock<Mutex<HashMap<RawTokenKey, usize>>> = LazyLock::new(Default::default);
/// 캐시 항목 수 상한. 넘으면 비우고 다시 쌓는다.
const RAW_TOKENS_MAX: usize = 64;

/// 원본 파일의 토큰 수 추정치. 같은 파일(경로·길이·수정 시각이 같음)이면 다시 토크나이즈하지 않는다
/// (샘플 행 수만 바꿔 다시 프로파일링할 때). 두 번째 값은 캐시 적중 여부.
fn raw_file_tokens(path: &Path) -> Result<(usize, bool), String> {
    let key = std::fs::metadata(path).ok().and_then(|m| {
        let modified = m.modified().ok()?;
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Some((canonical, m.len(), modified))
    });
    let Some(key) = key else {
        // 수정 시각을 모르면 바뀐 파일을 구분할 수 없으므로 캐시하지 않는다.
        return tokenizer::estimate_file_tokens(path).map(|n| (n, false));
    };
    let cache = || RAW_TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&n) = cache().get(&key) {
        return Ok((n, true));
    }
    // 토크나이즈하는 동안에는 잠금을 잡지 않는다(다른 로드를 막지 않게).
    let n = tokenizer::estimate_file_tokens(path)?;
    let mut c = cache();
    if c.len() >= RAW_TOKENS_MAX {
        c.clear();
    }
    c.insert(key, n);
    Ok((n, false))
}

/// 텍스트 파일을 읽고 인코딩을 자동으로 판별한다(프로파일러와 같은 `csv_profiler::detect_text_encoding`).
/// BOM(UTF-8/UTF-16)이 있으면 그것을 따르고, 없으면 UTF-8로 해석되는지 보고(깨진 바이트가 조금 섞인 UTF-8은
/// UTF-8로 본다) 아니면 CP949(EUC-KR)로 읽는다. steering 파일처럼 메모장에서 ANSI(CP949)로 저장한 한글 문서도
/// 깨지지 않게 한다.
pub fn read_text_auto(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("파일을 읽을 수 없음: {e}"))?;
    Ok(decode_text_auto(&bytes))
}

fn decode_text_auto(bytes: &[u8]) -> String {
    let enc = csv_profiler::detect_text_encoding(bytes, true);
    enc.decode_with_bom_removal(bytes).0.into_owned()
}

pub fn parse_fields(s: &str) -> Vec<String> {
    s.split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// steering 마크다운의 `- `·`* ` 불릿 줄을 제약 문장으로 읽는다.
/// 코드 펜스(``` 또는 ~~~) 안의 줄은 예시 코드이므로 건너뛴다 (yaml 블록의 `- rm -rf build`가 제약이 되지 않게).
pub fn parse_steering_rules(md: &str) -> Vec<String> {
    let mut fence: Option<(char, usize)> = None;
    let mut rules = Vec::new();
    for line in md.trim_start_matches('\u{feff}').lines() {
        let l = line.trim_start();
        if let Some((ch, len)) = fence {
            if closes_fence(l, ch, len) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = opens_fence(l) {
            fence = Some(open);
            continue;
        }
        if let Some(r) = l
            .strip_prefix("- ")
            .or_else(|| l.strip_prefix("* "))
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            rules.push(r.to_string());
        }
    }
    rules
}

/// 코드 펜스를 여는 줄이면 (펜스 글자, 길이). 백틱 펜스의 정보 문자열에는 백틱이 올 수 없으므로
/// ```` ```한 줄 코드``` ```` 같은 인라인 코드는 펜스가 아니다.
fn opens_fence(l: &str) -> Option<(char, usize)> {
    let ch = l.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = l.chars().take_while(|c| *c == ch).count();
    (len >= 3 && !(ch == '`' && l[len..].contains('`'))).then_some((ch, len))
}

/// 여는 펜스와 같은 글자로 같은 길이 이상이고 뒤에 공백만 있으면 닫는 줄이다.
fn closes_fence(l: &str, ch: char, len: usize) -> bool {
    let run = l.chars().take_while(|c| *c == ch).count();
    run >= len && l[run..].trim().is_empty()
}

pub fn compile(req: &CompileRequest) -> CompileResult {
    let mut ast = RootPromptAst {
        target_model: req.target,
        role: RoleNode {
            name: "Analyst".into(),
            instruction: req.role.clone(),
        },
        data_sub_ast: req.data.as_ref().map(|d| d.data.clone()),
        constraints: req
            .constraints
            .iter()
            .enumerate()
            .map(|(i, c)| ConstraintNode::classify(format!("C{}", i + 1), c))
            .collect(),
        output_schema: OutputSchemaAst::new(req.format, req.fields.clone()),
        lint_warnings: vec![],
    };

    // 압축 전 기준선: 원문 지시문 + 원본 데이터 전체 + 스키마 설명
    let baseline_text = format!(
        "{}\n{}\n{}",
        req.role,
        req.constraints.join("\n"),
        OutputSchemaAst::render_schema(req.format, &req.fields)
    );
    let orig_tokens = tokenizer::count(&baseline_text) + req.data.as_ref().map_or(0, |d| d.raw_tokens);

    linter::lint(&mut ast);
    ast.constraints.sort_by_key(|c| c.priority); // 안정 정렬: 같은 우선순위는 입력 순서 유지
    for (i, c) in ast.constraints.iter_mut().enumerate() {
        c.id = format!("C{}", i + 1); // 제거/정렬 뒤 ID를 연속 번호로 재부여
    }
    let output = codegen::generate(&ast);
    let comp_tokens = tokenizer::count(&output);

    let saving_pct = if orig_tokens == 0 {
        0.0
    } else {
        (1.0 - comp_tokens as f64 / orig_tokens as f64) * 100.0
    };
    let price = req.target.usd_per_million_input();
    let saved_usd = tokenizer::cost_usd(orig_tokens, price) - tokenizer::cost_usd(comp_tokens, price);
    let preview = mock_preview(&ast);
    CompileResult {
        ast,
        output,
        orig_tokens,
        comp_tokens,
        saving_pct,
        saved_usd,
        preview,
    }
}

/// LLM을 호출하지 않는 로컬 모의 응답: 출력 스키마 골격만 보여 준다.
/// 표 헤더는 프롬프트와 같은 규칙(codegen::md_cell)으로 이스케이프해 자기 검사(check_output)를 통과한다.
fn mock_preview(ast: &RootPromptAst) -> String {
    let fields = &ast.output_schema.required_fields;
    match ast.output_schema.format {
        OutputFormat::StrictJson => {
            let map: serde_json::Map<String, serde_json::Value> =
                fields.iter().map(|f| (f.clone(), serde_json::Value::Null)).collect();
            serde_json::to_string_pretty(&map).unwrap_or_default()
        }
        OutputFormat::MarkdownTable => {
            let header: Vec<String> = fields.iter().map(|f| codegen::md_cell(f)).collect();
            format!("| {} |\n|{}|", header.join(" | "), vec!["---"; fields.len()].join("|"))
        }
    }
}

/// 같은 입력을 한 타겟으로 컴파일했을 때의 토큰 수와 추정 비용.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetCost {
    pub target: TargetModel,
    pub orig_tokens: usize,
    pub comp_tokens: usize,
    pub saving_pct: f64,
    /// 압축 프롬프트를 한 번 보낼 때의 입력 비용 추정치 (USD)
    pub cost_usd: f64,
    pub saved_usd: f64,
}

/// 같은 요청을 Claude·GPT·Gemini로 각각 컴파일해 토큰과 비용을 비교한다 (워커 스레드에서 호출).
pub fn compare_targets(req: &CompileRequest) -> Vec<TargetCost> {
    TargetModel::ALL
        .iter()
        .map(|&target| {
            let r = compile(&CompileRequest { target, ..req.clone() });
            TargetCost {
                target,
                orig_tokens: r.orig_tokens,
                comp_tokens: r.comp_tokens,
                saving_pct: r.saving_pct,
                cost_usd: tokenizer::cost_usd(r.comp_tokens, target.usd_per_million_input()),
                saved_usd: r.saved_usd,
            }
        })
        .collect()
}

/// 작업 폴더와 실행 파일 위치에서 위로 올라가며 steering 파일을 찾는다.
pub fn find_steering() -> Option<std::path::PathBuf> {
    const REL: &str = ".kiro/steering/compiler-rules.md";
    let starts = [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf())),
    ];
    starts
        .into_iter()
        .flatten()
        .find_map(|s| s.ancestors().map(|a| a.join(REL)).find(|p| p.is_file()))
}

/// 실제 LLM 응답을 출력 스키마로 검사한 결과.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutputCheck {
    /// 화면 표시용 본문 (JSON이면 보기 좋게 정렬)
    pub pretty: String,
    pub ok: bool,
    pub missing: Vec<String>,
    /// 키는 있지만 값이 null이거나 빈 문자열인 필수 필드 (경고용, ok에는 영향 없음)
    pub nulls: Vec<String>,
    pub error: Option<String>,
    /// 제공자가 알려 준 종료 이유 원문 (stop_reason / finish_reason / finishReason)
    pub stop_reason: Option<String>,
    /// 응답이 끝까지 생성되지 않은 이유 (토큰 한도, 거부·필터). 있으면 형식이 맞아도 통과가 아니다
    pub cut_off: Option<String>,
    /// 제공자가 보고한 토큰 사용량 (check_reply만 채운다)
    pub usage: Option<Usage>,
    /// 표 형식에서 찾은 표의 본문 행 수 (헤더·구분 행 제외). 0이면 경고용, ok에는 영향 없음
    pub table_rows: Option<usize>,
}

impl OutputCheck {
    /// 출력 토큰 한도에 걸려 잘린 응답인지 (한도를 늘려 다시 실행하면 나아질 수 있는 경우).
    pub fn hit_max_tokens(&self) -> bool {
        let reply = LlmReply {
            stop_reason: self.stop_reason.clone(),
            ..LlmReply::default()
        };
        self.cut_off.is_some() && reply.finish() == Finish::MaxTokens
    }
}

/// LLM 응답을 검사한다. 출력 토큰 한도 등으로 잘린 응답은 형식 위반이 아니라 잘림으로 알리고 통과로 보지 않는다.
pub fn check_reply(format: OutputFormat, fields: &[String], reply: &LlmReply, max_tokens: u32) -> OutputCheck {
    let mut c = check_output(format, fields, &reply.text);
    let reason = reply.stop_reason.as_deref().unwrap_or_default();
    c.cut_off = match reply.finish() {
        Finish::MaxTokens => Some(format!(
            "출력 토큰 한도({max_tokens})에 도달해 응답이 잘렸습니다 ({reason})"
        )),
        Finish::ContextWindow => Some(format!("모델 컨텍스트 창이 가득 차 응답이 잘렸습니다 ({reason})")),
        Finish::Stopped => Some(format!("응답이 끝까지 생성되지 않았습니다 (종료 이유: {reason})")),
        Finish::Complete | Finish::Unknown => None,
    };
    c.ok &= c.cut_off.is_none();
    c.stop_reason = reply.stop_reason.clone();
    c.usage = reply.usage;
    c
}

/// LLM 응답이 강제한 출력 포맷과 필수 필드를 지켰는지 검사한다.
/// JSON은 ```json 코드 펜스를 벗겨 낸 뒤 파싱하고, 표는 헤더 행의 열 이름을 본다.
pub fn check_output(format: OutputFormat, fields: &[String], text: &str) -> OutputCheck {
    let body = strip_fence(text.trim());
    match format {
        OutputFormat::StrictJson => match serde_json::from_str::<serde_json::Value>(body) {
            Ok(serde_json::Value::Object(map)) => {
                let missing: Vec<String> = fields.iter().filter(|f| !map.contains_key(*f)).cloned().collect();
                let nulls: Vec<String> = fields
                    .iter()
                    .filter(|f| {
                        map.get(*f)
                            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
                    })
                    .cloned()
                    .collect();
                OutputCheck {
                    pretty: serde_json::to_string_pretty(&map).unwrap_or_else(|_| body.to_string()),
                    ok: missing.is_empty(),
                    missing,
                    nulls,
                    ..OutputCheck::default()
                }
            }
            Ok(_) => OutputCheck {
                pretty: body.to_string(),
                ok: false,
                missing: fields.to_vec(),
                error: Some("JSON 객체가 아닙니다".into()),
                ..OutputCheck::default()
            },
            Err(e) => OutputCheck {
                pretty: text.to_string(),
                ok: false,
                missing: fields.to_vec(),
                error: Some(format!("JSON 파싱 실패: {e}")),
                ..OutputCheck::default()
            },
        },
        OutputFormat::MarkdownTable => {
            let table = find_table(body);
            let header = table.as_ref().map(|t| t.header.as_slice()).unwrap_or_default();
            let missing: Vec<String> = fields.iter().filter(|f| !header_has(header, f)).cloned().collect();
            OutputCheck {
                pretty: body.to_string(),
                ok: table.is_some() && missing.is_empty(),
                error: table.is_none().then(|| "Markdown 표를 찾지 못했습니다".to_string()),
                missing,
                table_rows: table.map(|t| t.rows),
                ..OutputCheck::default()
            }
        }
    }
}

/// 헤더 셀 중에 필드 이름과 같은 것이 있는지. 대소문자는 구분하고, 셀을 감싼 강조·코드 표시는 벗겨서도 비교한다.
/// 필드 이름의 줄바꿈은 프롬프트의 표 헤더(codegen::md_cell)처럼 공백으로 본다.
fn header_has(header: &[String], field: &str) -> bool {
    let want = field.replace(['\n', '\r'], " ");
    let want = want.trim();
    header
        .iter()
        .any(|c| c == field || c == want || strip_emphasis(c) == want)
}

/// 셀 전체를 감싼 Markdown 강조·코드 표시(`**x**`, `__x__`, `*x*`, `_x_`, `` `x` ``)를 벗긴다.
/// `***x***`나 `` **`x`** ``처럼 겹친 표시도 벗기고, 안쪽의 밑줄(root_causes)은 그대로 둔다.
fn strip_emphasis(cell: &str) -> &str {
    let mut s = cell.trim();
    'outer: loop {
        for m in ["**", "__", "`", "*", "_"] {
            if s.len() > 2 * m.len() && s.starts_with(m) && s.ends_with(m) {
                s = s[m.len()..s.len() - m.len()].trim();
                continue 'outer;
            }
        }
        return s;
    }
}

/// 응답에서 찾은 Markdown 표.
struct Table {
    header: Vec<String>,
    /// 본문 행 수 (헤더·구분 행 제외)
    rows: usize,
}

/// Markdown 표의 헤더 셀과 본문 행 수. GFM처럼 헤더 행 바로 아래의 구분 행(`---|:--:`)을 기준으로 찾으므로
/// 앞뒤 파이프가 없는 표도 인식한다. 구분 행이 없으면 예전처럼 `|`로 시작하는 첫 줄을 헤더로 본다.
/// 본문은 헤더 아래로 빈 줄이나 `|`가 없는 줄이 나오기 전까지다.
fn find_table(body: &str) -> Option<Table> {
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let gfm = (0..lines.len().saturating_sub(1)).find_map(|i| {
        let width = delimiter_width(lines[i + 1])?;
        let cells = table_cells(lines[i]);
        (!lines[i].is_empty() && cells.len() == width).then(|| Table {
            header: cells,
            rows: lines[i + 2..]
                .iter()
                .take_while(|l| !l.is_empty() && l.contains('|'))
                .count(),
        })
    });
    gfm.or_else(|| {
        let i = lines.iter().position(|l| l.starts_with('|'))?;
        Some(Table {
            header: table_cells(lines[i]),
            rows: lines[i + 1..]
                .iter()
                .take_while(|l| l.starts_with('|'))
                .filter(|l| delimiter_width(l).is_none())
                .count(),
        })
    })
}

/// 구분 행이면 열 수를 돌려준다. 셀마다 `-`가 하나 이상이고 양끝에만 `:`를 허용한다.
/// 파이프가 없는 `---`는 구분선(또는 제목 밑줄)이므로 표로 보지 않는다.
fn delimiter_width(line: &str) -> Option<usize> {
    if !line.contains('|') {
        return None;
    }
    let cells = table_cells(line);
    cells
        .iter()
        .all(|c| {
            let dashes = c.strip_prefix(':').unwrap_or(c);
            let dashes = dashes.strip_suffix(':').unwrap_or(dashes);
            !dashes.is_empty() && dashes.chars().all(|ch| ch == '-')
        })
        .then_some(cells.len())
}

/// 표 한 줄을 셀로 나눈다. 앞뒤 파이프는 있어도 없어도 되고, `\|`는 셀 안의 `|` 글자로 본다.
fn table_cells(line: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let mut chars = line.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("시작 셀이 있다").push('|');
            }
            '|' => cells.push(String::new()),
            _ => cells.last_mut().expect("시작 셀이 있다").push(ch),
        }
    }
    // 여는·닫는 파이프가 만든 빈 셀을 뗀다. 셀 안의 공백은 떼기 전에 남겨 두어 `| |` 같은 빈 열과 구분한다.
    if cells.len() > 1 && cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

/// ```json ... ``` 코드 펜스를 벗긴다. 여러 줄과 한 줄(```{"a":1}```, ```json {"a":1}```) 모두 처리한다.
fn strip_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    let rest = rest.trim_end();
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    let starts_body = |t: &str| t.trim_start().starts_with(['{', '[', '|']);
    let body = match rest.split_once('\n') {
        // 첫 줄이 언어 표시(json, markdown 등)면 그 줄을 버린다
        Some((first, after)) if !starts_body(first) => after,
        _ if starts_body(rest) => rest,
        // 한 줄: 앞의 언어 표시 단어만 떼어 낸다
        _ => rest
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_alphanumeric()),
    };
    body.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn json_with_fence_and_missing_field() {
        let c = check_output(OutputFormat::StrictJson, &f(&["a", "b"]), "```json\n{\"a\": 1}\n```");
        assert!(!c.ok);
        assert_eq!(c.missing, f(&["b"]));
        assert!(c.error.is_none());
        assert!(c.pretty.contains("\"a\": 1"));
    }

    #[test]
    fn json_ok_and_invalid() {
        assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), r#"{"a":null,"x":2}"#).ok);
        let bad = check_output(OutputFormat::StrictJson, &f(&["a"]), "분석 결과: a는 1입니다");
        assert!(!bad.ok && bad.error.is_some());
        let arr = check_output(OutputFormat::StrictJson, &f(&["a"]), "[1,2]");
        assert!(!arr.ok && arr.error.is_some());
    }

    #[test]
    fn single_line_fences_and_nulls() {
        for t in [
            "```{\"a\":1}```",
            "```json {\"a\":1}```",
            "```json\n{\"a\":1}\n```",
            "```\n{\"a\":1}```",
        ] {
            assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), t).ok, "{t}");
        }
        let multi = "```\n{\n\"a\": 1\n}\n```";
        assert!(check_output(OutputFormat::StrictJson, &f(&["a"]), multi).ok);
        let c = check_output(OutputFormat::StrictJson, &f(&["a", "b"]), r#"{"a":null,"b":" "}"#);
        assert!(c.ok);
        assert_eq!(c.nulls, f(&["a", "b"]));
    }

    #[test]
    fn markdown_table_header() {
        let t = "설명\n| a | b |\n|---|---|\n| 1 | 2 |";
        assert!(check_output(OutputFormat::MarkdownTable, &f(&["a", "b"]), t).ok);
        let c = check_output(OutputFormat::MarkdownTable, &f(&["a", "c"]), t);
        assert_eq!(c.missing, f(&["c"]));
        assert!(!check_output(OutputFormat::MarkdownTable, &f(&["a"]), "표 없음").ok);
    }

    #[test]
    fn markdown_table_without_outer_pipes() {
        let fields = f(&["root_causes", "outliers"]);
        for t in [
            "root_causes | outliers\n--- | ---\n가격 인상 | 1건",
            "root_causes | outliers |\n---|---|\n가격 인상 | 1건 |",
            "| root_causes | outliers\n|:---|---:\n| 가격 인상 | 1건",
            "분석 결과입니다.\r\n\r\nroot_causes | outliers\r\n:-: | -\r\n가격 인상 | 1건\r\n",
            "```markdown\nroot_causes | outliers\n--- | ---\n가격 인상 | 1건\n```",
        ] {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(c.ok, "{t:?} -> {c:?}");
        }
        // 헤더 셀 안의 `\|`는 열 구분이 아니다
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["a|b", "c"]),
            "a\\|b | c\n--- | ---\n1 | 2",
        );
        assert!(c.ok, "{c:?}");
    }

    #[test]
    fn markdown_table_rejects_non_tables() {
        let fields = f(&["a", "b"]);
        for t in [
            "a b\n---\n본문",              // 제목 밑줄(setext)
            "a | b",                       // 구분 행도 앞 파이프도 없음
            "a | b | c\n--- | ---\n1 | 2", // 헤더와 구분 행의 열 수가 다름
            "a | b\n--- | x\n1 | 2",       // 구분 행에 `-` 아닌 글자
            "a | b\n--- |   \n1 | 2",      // 빈 구분 셀
        ] {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(!c.ok, "{t:?}");
            assert_eq!(c.error.as_deref(), Some("Markdown 표를 찾지 못했습니다"), "{t:?}");
        }
        // 구분 행이 없는 `|` 행은 예전처럼 헤더로 본다
        assert!(check_output(OutputFormat::MarkdownTable, &fields, "| a | b |\n| 1 | 2 |").ok);
        // 구분 행으로 찾은 표가 앞쪽의 `|` 줄보다 우선한다
        let t = "| 메모 |\n\na | b\n--- | ---\n1 | 2";
        assert!(check_output(OutputFormat::MarkdownTable, &fields, t).ok);
    }

    #[test]
    fn table_cells_split() {
        assert_eq!(table_cells("| a | b |"), f(&["a", "b"]));
        assert_eq!(table_cells("a | b"), f(&["a", "b"]));
        assert_eq!(table_cells("a | |"), f(&["a", ""]));
        assert_eq!(table_cells(r"a \| b | c\|"), f(&["a | b", "c|"]));
        assert_eq!(table_cells(r"a\b"), f(&[r"a\b"]));
        assert_eq!(delimiter_width("|:--|--:|:-:|"), Some(3));
        assert_eq!(delimiter_width("---"), None);
        assert_eq!(delimiter_width("|::|"), None);
    }

    fn reply(text: &str, stop: Option<&str>) -> LlmReply {
        LlmReply {
            text: text.into(),
            stop_reason: stop.map(String::from),
            usage: None,
        }
    }

    #[test]
    fn truncated_reply_is_reported_as_cut_off_not_format_error() {
        let fields = f(&["a", "b"]);
        let c = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply("{\"a\": \"긴 설명", Some("max_tokens")),
            2048,
        );
        assert!(!c.ok);
        assert_eq!(
            c.cut_off.as_deref(),
            Some("출력 토큰 한도(2048)에 도달해 응답이 잘렸습니다 (max_tokens)")
        );
        assert_eq!(c.stop_reason.as_deref(), Some("max_tokens"));
        assert!(c.error.is_some_and(|e| e.starts_with("JSON 파싱 실패")));

        // 표는 헤더만 맞아도 형식 검사를 통과하지만 잘린 응답이면 통과가 아니다
        let table = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 미완";
        assert!(check_output(OutputFormat::MarkdownTable, &fields, table).ok);
        let c = check_reply(OutputFormat::MarkdownTable, &fields, &reply(table, Some("length")), 512);
        assert!(!c.ok && c.missing.is_empty() && c.error.is_none());
        assert!(c.cut_off.is_some_and(|m| m.contains("(512)") && m.contains("length")));

        // 사고 토큰으로 한도를 다 써서 본문이 빈 Gemini 응답
        let c = check_reply(OutputFormat::StrictJson, &fields, &reply("", Some("MAX_TOKENS")), 2048);
        assert!(!c.ok && c.cut_off.is_some());
    }

    #[test]
    fn stop_reason_is_kept_for_complete_and_other_replies() {
        let fields = f(&["a"]);
        let ok = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("end_turn")),
            2048,
        );
        assert!(ok.ok && ok.cut_off.is_none());
        assert_eq!(ok.stop_reason.as_deref(), Some("end_turn"));
        let unknown = check_reply(OutputFormat::StrictJson, &fields, &reply(r#"{"a":1}"#, None), 2048);
        assert_eq!(unknown, check_output(OutputFormat::StrictJson, &fields, r#"{"a":1}"#));

        let ctx = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("model_context_window_exceeded")),
            2048,
        );
        assert!(!ctx.ok && ctx.cut_off.is_some_and(|m| m.contains("컨텍스트 창")));
        let filtered = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply("", Some("content_filter")),
            2048,
        );
        assert!(!filtered.ok);
        assert_eq!(
            filtered.cut_off.as_deref(),
            Some("응답이 끝까지 생성되지 않았습니다 (종료 이유: content_filter)")
        );
    }

    #[test]
    fn usage_and_limit_hit_are_carried() {
        let fields = f(&["a"]);
        let mut r = reply("{\"a\": \"잘", Some("max_tokens"));
        r.usage = Some(Usage {
            input: 812,
            output: 2048,
        });
        let c = check_reply(OutputFormat::StrictJson, &fields, &r, 2048);
        assert_eq!(
            c.usage,
            Some(Usage {
                input: 812,
                output: 2048
            })
        );
        assert!(c.hit_max_tokens());
        // 필터로 중단된 응답이나 정상 응답은 한도를 늘려도 소용없다
        let filtered = check_reply(OutputFormat::StrictJson, &fields, &reply("", Some("SAFETY")), 2048);
        assert!(filtered.cut_off.is_some() && !filtered.hit_max_tokens());
        let ok = check_reply(
            OutputFormat::StrictJson,
            &fields,
            &reply(r#"{"a":1}"#, Some("stop")),
            2048,
        );
        assert!(!ok.hit_max_tokens());
    }

    #[test]
    fn table_header_cells_ignore_emphasis_and_code_marks() {
        let fields = f(&["root_causes", "outliers", "next_steps"]);
        for t in [
            "| **root_causes** | `outliers` | __next_steps__ |\n|---|---|---|\n| a | b | c |",
            "| *root_causes* | _outliers_ | ***next_steps*** |\n|---|---|---|\n| a | b | c |",
            "| ** root_causes ** | **`outliers`** | ``next_steps`` |\n|---|---|---|\n| a | b | c |",
        ] {
            let c = check_output(OutputFormat::MarkdownTable, &fields, t);
            assert!(c.ok, "{t:?} -> {c:?}");
            assert_eq!(c.table_rows, Some(1));
        }
        // 대소문자는 구분하고, 안쪽 밑줄은 벗기지 않는다
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["root_causes"]),
            "| **Root_Causes** |\n|---|\n| a |",
        );
        assert_eq!(c.missing, f(&["root_causes"]));
        // 밑줄로 감싼 이름 자체가 필드면 그대로도 맞는다
        let c = check_output(
            OutputFormat::MarkdownTable,
            &f(&["__init__"]),
            "| __init__ |\n|---|\n| 1 |",
        );
        assert!(c.ok, "{c:?}");
        assert_eq!(strip_emphasis("**a_b**"), "a_b");
        assert_eq!(strip_emphasis("*"), "*");
        assert_eq!(strip_emphasis("**"), "**");
        assert_eq!(strip_emphasis("_a"), "_a");
    }

    #[test]
    fn table_body_rows_are_counted() {
        let fields = f(&["a", "b"]);
        let rows = |t: &str| check_output(OutputFormat::MarkdownTable, &fields, t).table_rows;
        assert_eq!(rows("| a | b |\n|---|---|"), Some(0));
        assert_eq!(
            rows("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n설명 문단 | 끝"),
            Some(2)
        );
        assert_eq!(rows("a | b\n--- | ---\n1 | 2\n메모"), Some(1));
        assert_eq!(rows("| a | b |\n| 1 | 2 |\n| 3 | 4 |"), Some(2)); // 구분 행 없는 예전 형식
        assert_eq!(rows("표 없음"), None);
        // 헤더만 있는 표도 형식 검사는 통과한다 (경고는 UI가 띄운다)
        let c = check_output(OutputFormat::MarkdownTable, &fields, "| a | b |\n|---|---|");
        assert!(c.ok && c.table_rows == Some(0));
        assert_eq!(
            check_output(OutputFormat::StrictJson, &fields, r#"{"a":1,"b":2}"#).table_rows,
            None
        );
    }

    #[test]
    fn mock_preview_passes_its_own_check() {
        let fields = f(&["a|b", "root_causes", "x y", "따옴표\"키", "**굵게**", "줄\n바꿈"]);
        for format in [OutputFormat::StrictJson, OutputFormat::MarkdownTable] {
            let r = compile(&CompileRequest {
                target: TargetModel::Gpt,
                role: "r".into(),
                constraints: vec![],
                format,
                fields: fields.clone(),
                data: None,
            });
            let want = &r.ast.output_schema.required_fields;
            assert!(!want.is_empty());
            let c = check_output(format, want, &r.preview);
            assert!(c.ok, "{format:?}\n{}\n{c:?}", r.preview);
            assert!(c.missing.is_empty(), "{format:?}: {:?}", c.missing);
        }
    }

    #[test]
    fn steering_rules_skip_fenced_code() {
        let md = "# 규칙\n- 첫 규칙\n```yaml\nsteps:\n  - rm -rf build\n```\n* 둘째 규칙\n~~~\n- 물결 펜스 안\n~~~\n\
                  ````md\n```\n- 긴 펜스 안\n````\n  - 들여 쓴 규칙\n```한 줄 코드```\n- 마지막 규칙\n";
        assert_eq!(
            parse_steering_rules(md),
            f(&["첫 규칙", "둘째 규칙", "들여 쓴 규칙", "마지막 규칙"])
        );
        // 닫히지 않은 펜스는 문서 끝까지 코드다
        assert_eq!(parse_steering_rules("- a\n```\n- b\n- c"), f(&["a"]));
        // BOM과 빈 불릿
        assert_eq!(parse_steering_rules("\u{feff}- a\n- \n-  b "), f(&["a", "b"]));
    }

    #[test]
    fn read_text_auto_detects_encoding() {
        let dir = std::env::temp_dir().join(format!("cc_text_auto_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = "- 데이터에 없는 내용은 추론하지 마\n- 결측치 제외";
        let (cp949, _, _) = encoding_rs::EUC_KR.encode(text);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let cases: [(&str, Vec<u8>); 4] = [
            ("utf8.md", text.as_bytes().to_vec()),
            ("bom.md", [b"\xEF\xBB\xBF".as_slice(), text.as_bytes()].concat()),
            ("cp949.md", cp949.into_owned()),
            ("utf16.md", utf16),
        ];
        for (name, bytes) in cases {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_text_auto(&p).unwrap(), text, "{name}");
            assert_eq!(parse_steering_rules(&read_text_auto(&p).unwrap()).len(), 2, "{name}");
        }
        assert!(
            read_text_auto(&dir.join("없는 파일.md"))
                .unwrap_err()
                .contains("읽을 수 없음")
        );
        // 리뷰 재현: UTF-8 문서에 깨진 바이트가 하나 섞이면 예전에는 전체를 CP949로 읽어 한글이 모두 깨졌다.
        // 프로파일러와 같은 판정으로 UTF-8로 읽고 깨진 바이트만 U+FFFD가 된다.
        let damaged = dir.join("damaged.md");
        std::fs::write(&damaged, [text.as_bytes(), b"\n- \xff"].concat()).unwrap();
        let read = read_text_auto(&damaged).unwrap();
        assert_eq!(read, format!("{text}\n- \u{FFFD}"));
        assert_eq!(
            parse_steering_rules(&read)[..2],
            f(&["데이터에 없는 내용은 추론하지 마", "결측치 제외"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_token_estimate_is_cached_until_file_changes() {
        let dir = std::env::temp_dir().join(format!("cc_tok_cache_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.csv");
        std::fs::write(&p, "id,amount\n1,10\n2,20\n3,30\n").unwrap();
        let (n, hit) = raw_file_tokens(&p).unwrap();
        assert!(!hit && n > 0);
        assert_eq!(raw_file_tokens(&p).unwrap(), (n, true));
        // 샘플 행 수만 바꿔 다시 프로파일링해도 같은 추정치를 캐시에서 쓴다
        let a = load_data(&p, 1).unwrap();
        let b = load_data(&p, 3).unwrap();
        assert_eq!((a.raw_tokens, b.raw_tokens), (n, n));
        assert!(raw_file_tokens(&p).unwrap().1);
        // 파일이 바뀌면(길이·수정 시각) 다시 계산한다
        std::fs::write(&p, "id,amount,memo\n1,10,가나다라마바사\n2,20,아자차카타파하\n").unwrap();
        let (m, hit) = raw_file_tokens(&p).unwrap();
        assert!(!hit && m != n, "{n} -> {m}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compare_targets_matches_single_compiles() {
        let req = CompileRequest {
            target: TargetModel::Claude,
            role: "매출 분석가".into(),
            constraints: vec!["이상치 우선 탐지".into(), "데이터에 없는 내용은 추론하지 마".into()],
            format: OutputFormat::StrictJson,
            fields: f(&["root_causes", "outliers"]),
            data: None,
        };
        let cmp = compare_targets(&req);
        assert_eq!(
            cmp.iter().map(|c| c.target).collect::<Vec<_>>(),
            TargetModel::ALL.to_vec()
        );
        for c in &cmp {
            let single = compile(&CompileRequest {
                target: c.target,
                ..req.clone()
            });
            assert_eq!((c.orig_tokens, c.comp_tokens), (single.orig_tokens, single.comp_tokens));
            let want = tokenizer::cost_usd(single.comp_tokens, c.target.usd_per_million_input());
            assert!((c.cost_usd - want).abs() < 1e-12);
        }
    }
}
