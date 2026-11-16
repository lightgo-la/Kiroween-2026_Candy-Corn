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
