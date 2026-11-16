//! Multi-AST 정의. RootPromptAst 아래에 Data / Constraint / OutputSchema 서브 AST가 붙는다.

pub mod codegen;
pub mod linter;

use serde::{Deserialize, Serialize};

/// 최상위 통합 프롬프트 AST
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootPromptAst {
    pub target_model: TargetModel,
    pub role: RoleNode,
    pub data_sub_ast: Option<DataSubAst>,
    pub constraints: Vec<ConstraintNode>,
    pub output_schema: OutputSchemaAst,
    pub lint_warnings: Vec<LintDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetModel {
    Claude, // XML 태그 기반 직렬화에 최적화
    Gpt,    // Markdown + JSON Schema에 최적화
    Gemini, // 구조화된 섹션 + 명확한 구분자에 최적화
}

impl TargetModel {
    pub const ALL: [TargetModel; 3] = [TargetModel::Claude, TargetModel::Gpt, TargetModel::Gemini];

    pub fn label(self) -> &'static str {
        match self {
            TargetModel::Claude => "Claude",
            TargetModel::Gpt => "GPT",
            TargetModel::Gemini => "Gemini",
        }
    }

    /// 입력 토큰 100만 개당 USD 단가(추정치, 대시보드 표시용).
    pub fn usd_per_million_input(self) -> f64 {
        match self {
            TargetModel::Claude => 3.0,
            TargetModel::Gpt => 2.5,
            TargetModel::Gemini => 1.25,
        }
    }
}

/// 역할 노드
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleNode {
    pub name: String,
    pub instruction: String,
}

/// 1. 데이터 프로파일링 서브 AST (CSV/JSON 압축 결과)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSubAst {
    pub source_name: String,
    pub total_rows: usize,
    pub columns: Vec<ColumnProfile>,
    pub anomalies: Vec<String>,   // 통계적 이상치 요약
    pub sample_rows: Vec<String>, // 토큰 예산 내 샘플
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub name: String,
    pub dtype: String,
    pub null_ratio: f64,
    pub summary_stats: String, // 예: "min:120, max:9800, mean:4520"
}

/// 2. 제약 조건 서브 AST (Steering Docs 및 UI 체크박스에서 주입)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintNode {
    pub id: String,
    pub rule_type: ConstraintType,
    pub expression: String, // 압축된 핵심 명령문
    pub priority: u8,       // 낮을수록 프롬프트 앞쪽(어텐션 우선)에 배치
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintType {
    NegativeRule,   // Do not ...
    BoundaryRule,   // 범위 제한
    FactualityRule, // 데이터 외 추론 금지
}

impl ConstraintType {
    pub fn tag(&self) -> &'static str {
        match self {
            ConstraintType::NegativeRule => "negative",
            ConstraintType::BoundaryRule => "boundary",
            ConstraintType::FactualityRule => "factuality",
        }
    }
}

impl ConstraintNode {
    /// 문장에서 규칙 유형과 우선순위를 추정해 노드를 만든다.
    pub fn classify(id: String, text: &str) -> Self {
        let lower = text.to_lowercase();
        let (rule_type, priority) = if [
            "데이터 외",
            "추론",
            "추측",
            "only from the data",
            "hallucinat",
            "do not infer",
        ]
        .iter()
        .any(|k| lower.contains(k))
        {
            (ConstraintType::FactualityRule, 0)
        } else if linter::is_negative(text) {
            (ConstraintType::NegativeRule, 1)
        } else {
            (ConstraintType::BoundaryRule, 2)
        };
        ConstraintNode {
            id,
            rule_type,
            expression: text.trim().to_string(),
            priority,
        }
    }
}

/// 3. 출력 스키마 서브 AST
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSchemaAst {
    pub format: OutputFormat,
    pub raw_schema: String, // 압축된 JSON Schema 문자열(표 형식이면 헤더 행)
    pub required_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    StrictJson,
    MarkdownTable,
}

impl OutputSchemaAst {
    pub fn new(format: OutputFormat, required_fields: Vec<String>) -> Self {
        let raw_schema = Self::render_schema(format, &required_fields);
        OutputSchemaAst {
            format,
            raw_schema,
            required_fields,
        }
    }

    pub fn render_schema(format: OutputFormat, fields: &[String]) -> String {
        match format {
            OutputFormat::StrictJson => {
                let props: serde_json::Map<String, serde_json::Value> = fields
                    .iter()
                    .map(|f| (f.clone(), serde_json::json!({ "type": "string" })))
                    .collect();
                serde_json::json!({ "type": "object", "properties": props, "required": fields }).to_string()
            }
            OutputFormat::MarkdownTable => {
                format!(
                    "| {} |",
                    fields
                        .iter()
                        .map(|f| codegen::md_cell(f))
                        .collect::<Vec<_>>()
                        .join(" | ")
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintDiagnostic {
    pub severity: String, // "INFO", "WARN", "ERROR"
    pub message: String,
}

impl LintDiagnostic {
    pub fn new(severity: &str, message: impl Into<String>) -> Self {
        LintDiagnostic {
            severity: severity.to_string(),
            message: message.into(),
        }
    }
}
