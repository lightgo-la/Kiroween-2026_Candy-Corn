//! GUI / CLI / MCP가 공유하는 컴파일 파이프라인: AST 구성 → 린트 → 직렬화 → 토큰 계산.

use crate::ast::{
    ConstraintNode, DataSubAst, OutputFormat, OutputSchemaAst, RoleNode, RootPromptAst, TargetModel, codegen, linter,
};
use crate::{profiler::csv_profiler, tokenizer};
use std::path::Path;

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
    let raw_tokens = tokenizer::estimate_file_tokens(path)?;
    Ok(LoadedData {
        path: path.to_path_buf(),
        data,
        raw_tokens,
    })
}

pub fn parse_fields(s: &str) -> Vec<String> {
    s.split(',')
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// steering 마크다운의 `- ` 불릿 줄을 제약 문장으로 읽는다.
pub fn parse_steering_rules(md: &str) -> Vec<String> {
    md.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            l.strip_prefix("- ")
                .or_else(|| l.strip_prefix("* "))
                .map(|r| r.trim().to_string())
        })
        .filter(|r| !r.is_empty())
        .collect()
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
fn mock_preview(ast: &RootPromptAst) -> String {
    let fields = &ast.output_schema.required_fields;
    match ast.output_schema.format {
        OutputFormat::StrictJson => {
            let map: serde_json::Map<String, serde_json::Value> =
                fields.iter().map(|f| (f.clone(), serde_json::Value::Null)).collect();
            serde_json::to_string_pretty(&map).unwrap_or_default()
        }
        OutputFormat::MarkdownTable => {
            format!("| {} |\n|{}|", fields.join(" | "), vec!["---"; fields.len()].join("|"))
        }
    }
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
#[derive(Debug, Clone, PartialEq)]
pub struct OutputCheck {
    /// 화면 표시용 본문 (JSON이면 보기 좋게 정렬)
    pub pretty: String,
    pub ok: bool,
    pub missing: Vec<String>,
    /// 키는 있지만 값이 null이거나 빈 문자열인 필수 필드 (경고용, ok에는 영향 없음)
    pub nulls: Vec<String>,
    pub error: Option<String>,
}

/// LLM 응답이 강제한 출력 포맷과 필수 필드를 지켰는지 검사한다.
/// JSON은 ```json 코드 펜스를 벗겨 낸 뒤 파싱하고, 표는 첫 헤더 행의 열 이름을 본다.
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
                    error: None,
                }
            }
            Ok(_) => OutputCheck {
                pretty: body.to_string(),
                ok: false,
                missing: fields.to_vec(),
                nulls: vec![],
                error: Some("JSON 객체가 아닙니다".into()),
            },
            Err(e) => OutputCheck {
                pretty: text.to_string(),
                ok: false,
                missing: fields.to_vec(),
                nulls: vec![],
                error: Some(format!("JSON 파싱 실패: {e}")),
            },
        },
        OutputFormat::MarkdownTable => {
            let header = body.lines().map(str::trim).find(|l| l.starts_with('|'));
            let cols: Vec<String> = header
                .map(|h| h.trim_matches('|').split('|').map(|c| c.trim().to_string()).collect())
                .unwrap_or_default();
            let missing: Vec<String> = fields.iter().filter(|f| !cols.contains(f)).cloned().collect();
            OutputCheck {
                pretty: body.to_string(),
                ok: header.is_some() && missing.is_empty(),
                nulls: vec![],
                error: header.is_none().then(|| "Markdown 표를 찾지 못했습니다".to_string()),
                missing,
            }
        }
    }
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
}
