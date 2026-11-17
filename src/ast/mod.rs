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

/// 데이터 밖의 내용을 막는 표현. 있으면 부정 표지가 없어도 사실성 규칙이다.
const FACTUALITY_MARKERS: [&str; 9] = [
    "데이터 외",
    "데이터외",
    "데이터에 없는",
    "데이터에없는",
    "only from the data",
    "not in the data",
    "outside the data",
    "beyond the data",
    "hallucinat",
];

/// 추론·추측을 뜻하는 말. 금지 문맥("추론하지 마", "do not infer", "추측 배제")일 때만 사실성 규칙이다.
/// "추론 과정을 단계별로 설명해"는 금지가 아니므로 일반 규칙이다.
const INFERENCE_WORDS: [&str; 5] = ["추론", "추측", "짐작", "지어내", "꾸며내"];
/// 영어는 단어 앞부분으로 맞춘다(infer, inferred, guessing, speculate, fabricated).
const INFERENCE_PREFIXES: [&str; 4] = ["infer", "guess", "speculat", "fabricat"];
/// 부정 표지가 아니어도 추론을 막는 말("추론 없이", "임의 추측 배제", "answer without guessing", "no guessing").
const INFERENCE_BANS: [&str; 4] = ["없이", "배제", "지양", "자제"];
const INFERENCE_BAN_WORDS: [&str; 2] = ["without", "no"];

fn is_factuality(text: &str) -> bool {
    let lower = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    if FACTUALITY_MARKERS.iter().any(|k| lower.contains(k)) {
        return true;
    }
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).collect();
    let infers = INFERENCE_WORDS.iter().any(|k| lower.contains(k))
        || words
            .iter()
            .any(|w| INFERENCE_PREFIXES.iter().any(|p| w.starts_with(p)))
        || lower.contains("make up")
        || lower.contains("made up");
    let banned = INFERENCE_BANS.iter().any(|k| lower.contains(k))
        || words.iter().any(|w| INFERENCE_BAN_WORDS.contains(w))
        || linter::is_negative(text);
    infers && banned
}

impl ConstraintNode {
    /// 문장에서 규칙 유형과 우선순위를 추정해 노드를 만든다.
    pub fn classify(id: String, text: &str) -> Self {
        let (rule_type, priority) = if is_factuality(text) {
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

    /// 린트 전 ID(`C1`..`Cn`, 입력 순서)에서 0부터 센 입력 위치를 읽는다. 형식이 다르면 None.
    pub fn input_index(&self) -> Option<usize> {
        self.id.strip_prefix('C')?.parse::<usize>().ok()?.checked_sub(1)
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
    /// 진단에 걸린 제약들의 원래 입력 위치(0부터). 린트 전 ID `C1..Cn`이 입력 순서이므로 C3 → 2.
    /// 제거 경고는 [지운 제약, 남긴 제약], 모순 의심은 [앞 제약, 뒤 제약] 순서다. 제약에 대한 진단이 아니면 비어 있다.
    #[serde(default)]
    pub indices: Vec<usize>,
}

impl LintDiagnostic {
    pub fn new(severity: &str, message: impl Into<String>) -> Self {
        LintDiagnostic {
            severity: severity.to_string(),
            message: message.into(),
            indices: Vec::new(),
        }
    }

    /// 제약들에 대한 진단. `indices`는 걸린 제약들의 원래 입력 위치(0부터)다.
    pub fn for_constraints(severity: &str, message: impl Into<String>, indices: Vec<usize>) -> Self {
        LintDiagnostic {
            indices,
            ..Self::new(severity, message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(text: &str) -> (ConstraintType, u8) {
        let c = ConstraintNode::classify("C1".into(), text);
        (c.rule_type, c.priority)
    }

    #[test]
    fn factuality_needs_a_prohibitive_context() {
        for t in [
            "데이터에 없는 내용은 추론하지 마",
            "데이터 외 추론 금지",
            "추측하지 말 것",
            "원인을 추론하지 않는다",
            "Do not infer causes",
            "Never guess missing values",
            "Answer only from the data",
            "No hallucinations",
            "Don't make up numbers",
            "추론 없이 데이터만 사용해",
            "임의 추측 배제",
            "Answer without guessing",
            "추측은 지양",
        ] {
            assert_eq!(kind(t), (ConstraintType::FactualityRule, 0), "{t}");
        }
        for (t, want) in [
            ("추론 과정을 단계별로 설명해", (ConstraintType::BoundaryRule, 2)),
            ("Explain your inference steps", (ConstraintType::BoundaryRule, 2)),
            ("추측치와 실측치를 비교해", (ConstraintType::BoundaryRule, 2)),
            ("Do not use the inventory column", (ConstraintType::NegativeRule, 1)),
            ("You can treat empty cells as blanks", (ConstraintType::BoundaryRule, 2)),
        ] {
            assert_eq!(kind(t), want, "{t}");
        }
    }
}
