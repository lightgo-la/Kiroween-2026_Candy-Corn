//! Multi-AST definitions. The Data / Constraint / OutputSchema sub-ASTs hang off RootPromptAst.

pub mod codegen;
pub mod linter;

use crate::lexicon;
use serde::{Deserialize, Serialize};

/// Top-level combined prompt AST
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
    Claude, // Optimized for XML-tag serialization
    Gpt,    // Optimized for Markdown + JSON Schema
    Gemini, // Optimized for structured sections + clear delimiters
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

    /// USD price per million input tokens (estimate, for the dashboard).
    pub fn usd_per_million_input(self) -> f64 {
        match self {
            TargetModel::Claude => 3.0,
            TargetModel::Gpt => 2.5,
            TargetModel::Gemini => 1.25,
        }
    }
}

/// Role node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleNode {
    pub name: String,
    pub instruction: String,
}

/// 1. Data profiling sub-AST (compressed CSV/JSON result)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSubAst {
    pub source_name: String,
    pub total_rows: usize,
    pub columns: Vec<ColumnProfile>,
    pub anomalies: Vec<String>,   // Summary of statistical outliers
    pub sample_rows: Vec<String>, // Sample rows within the token budget
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub name: String,
    pub dtype: String,
    pub null_ratio: f64,
    pub summary_stats: String, // e.g. "min:120, max:9800, mean:4520"
}

/// 2. Constraint sub-AST (injected from steering docs and UI checkboxes)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintNode {
    pub id: String,
    pub rule_type: ConstraintType,
    pub expression: String, // Compressed core instruction
    pub priority: u8,       // Lower values go earlier in the prompt (attention priority)
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintType {
    NegativeRule,   // Do not ...
    BoundaryRule,   // Scope limit
    FactualityRule, // No inference beyond the data
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

/// Phrases that block content from outside the data. With one, the sentence is a factuality rule even without a
/// negation marker. These are the English phrases; the Korean ones are `lexicon::AST_KO_FACTUALITY_MARKERS`.
/// (The Korean word lists are data for judging input sentences, so they are used as is regardless of the UI language.)
const FACTUALITY_MARKERS: [&str; 5] = [
    "only from the data",
    "not in the data",
    "outside the data",
    "beyond the data",
    "hallucinat",
];

/// Korean words meaning inference or guessing. A factuality rule only in a prohibitive context ("churonhaji ma", do
/// not infer; "do not infer"; "chucheuk baeje", no guessing). "Explain the inference step by step" is not a
/// prohibition, so it is an ordinary rule.
const INFERENCE_WORDS: &[&str] = lexicon::AST_KO_INFERENCE_WORDS;
/// English words are matched by their beginning (infer, inferred, guessing, speculate, fabricated).
const INFERENCE_PREFIXES: [&str; 4] = ["infer", "guess", "speculat", "fabricat"];
/// Words that block inference even without a negation marker ("churon eopsi", without inference;
/// "imui chucheuk baeje", no arbitrary guessing; "answer without guessing", "no guessing"): the Korean ones here, the
/// English ones in `INFERENCE_BAN_WORDS`.
const INFERENCE_BANS: &[&str] = lexicon::AST_KO_INFERENCE_BANS;
const INFERENCE_BAN_WORDS: [&str; 2] = ["without", "no"];

/// Whether `lower` has `marker` starting at a word boundary and, unless the marker is a word stem, ending at one:
/// "not in the data" must not match "not in the database".
fn has_marker(lower: &str, marker: &str, stem: bool) -> bool {
    lower.match_indices(marker).any(|(i, m)| {
        let before = lower[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = lower[i + m.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before && (stem || after)
    })
}

fn is_factuality(text: &str) -> bool {
    let lower = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    if FACTUALITY_MARKERS
        .iter()
        .any(|k| has_marker(&lower, k, *k == "hallucinat"))
        || lexicon::AST_KO_FACTUALITY_MARKERS.iter().any(|k| lower.contains(k))
    {
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
    /// Builds a node, inferring the rule type and priority from the sentence.
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

    /// Reads the zero-based input position from the pre-lint ID (`C1`..`Cn`, in input order). None for any other
    /// format.
    pub fn input_index(&self) -> Option<usize> {
        self.id.strip_prefix('C')?.parse::<usize>().ok()?.checked_sub(1)
    }
}

/// 3. Output schema sub-AST
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSchemaAst {
    pub format: OutputFormat,
    pub raw_schema: String, // Compressed JSON Schema string (the header row for the table format)
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
    /// Original input positions (zero-based) of the constraints involved. The pre-lint IDs `C1..Cn` follow the input
    /// order, so C3 → 2. Removal warnings are ordered [removed, kept], possible contradictions [earlier, later].
    /// Empty for diagnostics that are not about constraints.
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

    /// Diagnostic about constraints. `indices` are the original input positions (zero-based) of the constraints
    /// involved.
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
        let english = [
            "Do not infer causes",
            "Never guess missing values",
            "Answer only from the data",
            "No hallucinations",
            "Don't make up numbers",
            "Answer without guessing",
        ];
        for t in english
            .into_iter()
            .chain(ko_fixture!("core.ast.factuality").iter().copied())
        {
            assert_eq!(kind(t), (ConstraintType::FactualityRule, 0), "{t}");
        }
        let english = [
            ("Explain your inference steps", (ConstraintType::BoundaryRule, 2)),
            ("Do not use the inventory column", (ConstraintType::NegativeRule, 1)),
            ("You can treat empty cells as blanks", (ConstraintType::BoundaryRule, 2)),
        ];
        let ordinary = ko_fixture!("core.ast.ordinary_inference")
            .iter()
            .map(|t| (*t, (ConstraintType::BoundaryRule, 2)));
        for (t, want) in english.into_iter().chain(ordinary) {
            assert_eq!(kind(t), want, "{t}");
        }
    }

    #[test]
    fn factuality_markers_match_whole_words() {
        assert!(is_factuality("Use only from the data"));
        assert!(is_factuality("Report only what is not in the data"));
        assert!(is_factuality("Do not hallucinate values"));
        // "not in the data" is inside "not in the database", which is an ordinary rule
        assert!(!is_factuality("Flag customers who are not in the database"));
        assert!(!is_factuality("Round amounts to 2 decimals"));
    }
}
