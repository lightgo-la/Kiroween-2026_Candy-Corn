//! Blueprint: the file (.kiro, JSON) that holds the whole studio setup.
//! Saves/applies the data path, role, constraints, output format, fields, target, sample row count, theme, LLM models,
//! LLM run options and node layout in one go.
//! The GUI (canvas menu, open/save) and the CLI (`--cli --blueprint FILE`) use the same format.
//!
//! Versions: v1 (project file) → v2 (llm_models) → v3 (llm_options). Missing entries are read as defaults, so older
//! files still open.

use crate::ast::{OutputFormat, TargetModel};
use crate::canvas::graph::Provider;
use crate::i18n::Tr;
use crate::pipeline::{self, CompileRequest, LoadedData};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 3;

/// Allowed range (±) of node coordinates. Farther coordinates lose f32 precision and break the canvas math (grid, fit
/// to view).
pub const POS_LIMIT: f32 = 100_000.0;

/// Checks a saved node position: drops it (None) if it has NaN or infinity, and clamps values that are too far to
/// ±POS_LIMIT.
pub fn sanitize_pos(p: [f32; 2]) -> Option<[f32; 2]> {
    p.iter()
        .all(|v| v.is_finite())
        .then(|| p.map(|v| v.clamp(-POS_LIMIT, POS_LIMIT)))
}

pub const MIN_LLM_MAX_TOKENS: u32 = 256;
pub const MAX_LLM_MAX_TOKENS: u32 = 32_000;
pub const DEFAULT_LLM_MAX_TOKENS: u32 = 2048;

/// LLM run options (saved since v3). v1/v2 files do not have them, so they are read as defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmOptions {
    /// Output token limit (MIN_LLM_MAX_TOKENS..=MAX_LLM_MAX_TOKENS)
    pub max_tokens: u32,
    /// Temperature. None leaves it out of the request, so the provider default is used
    pub temperature: Option<f32>,
    /// System prompt. Not sent when empty
    pub system: String,
}

impl Default for LlmOptions {
    fn default() -> Self {
        LlmOptions {
            max_tokens: DEFAULT_LLM_MAX_TOKENS,
            temperature: None,
            system: String::new(),
        }
    }
}

/// Upper temperature limit per provider (Anthropic 0-1, OpenAI and Gemini 0-2).
pub fn temperature_max(p: Provider) -> f32 {
    match p {
        Provider::Anthropic => 1.0,
        Provider::OpenAi | Provider::Gemini => 2.0,
    }
}

impl LlmOptions {
    /// Fixes out-of-range values. The temperature is only clamped to the provider-independent maximum range (0-2);
    /// the per-provider range is applied when sending (effective_temperature).
    pub fn normalized(mut self) -> LlmOptions {
        self.max_tokens = self.max_tokens.clamp(MIN_LLM_MAX_TOKENS, MAX_LLM_MAX_TOKENS);
        self.temperature = self.temperature.filter(|t| t.is_finite()).map(|t| t.clamp(0.0, 2.0));
        self
    }

    /// The temperature actually sent to the provider: the saved value is left alone and a copy clamped to the
    /// provider's range is used (1.5 set for GPT is sent as 1.0 after switching to Claude, and is still 1.5 after
    /// switching back to GPT).
    pub fn effective_temperature(&self, p: Provider) -> Option<f32> {
        self.temperature.map(|t| t.clamp(0.0, temperature_max(p)))
    }
}

/// Every field has a default so that older files with missing fields (including v1 project files) can be read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Blueprint {
    pub version: u32,
    pub project: String,
    pub target: TargetModel,
    pub role: String,
    pub goals: Vec<GoalSpec>,
    pub format: OutputFormat,
    pub fields: String,
    pub sample_rows: usize,
    /// Theme number (from 0; design sample 01 = 0)
    pub theme: usize,
    /// Data file path. A relative path is relative to the folder of the blueprint file.
    pub data_path: Option<String>,
    /// Fixed node positions (data, role, output, root, result, llm, settings)
    pub positions: Vec<(String, [f32; 2])>,
    /// LLM model name per provider
    pub llm_models: Vec<(Provider, String)>,
    /// LLM run options: output token limit, temperature, system prompt (v3)
    pub llm_options: LlmOptions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GoalSpec {
    pub text: String,
    pub on: bool,
    pub pos: Option<[f32; 2]>,
}

impl Default for Blueprint {
    fn default() -> Self {
        Blueprint {
            version: VERSION,
            project: "untitled.kiro".into(),
            target: TargetModel::Claude,
            role: String::new(),
            goals: vec![],
            format: OutputFormat::StrictJson,
            fields: pipeline::DEFAULT_FIELDS.into(),
            sample_rows: 8,
            theme: 3,
            data_path: None,
            positions: vec![],
            llm_models: vec![],
            llm_options: LlmOptions::default(),
        }
    }
}

pub const MAX_SAMPLE_ROWS: usize = 50;

impl Blueprint {
    pub fn load(path: &Path) -> Result<Blueprint, String> {
        let text = std::fs::read_to_string(path).map_err(|e| trf!("cannot read file: {e}", e = e))?;
        Blueprint::parse(&text)
    }

    /// Parses blueprint JSON. Every blueprint (including v1 project files) has a numeric version key,
    /// so JSON without one is taken as a different kind of file and rejected.
    pub fn parse(text: &str) -> Result<Blueprint, String> {
        let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| trf!("invalid format: {e}", e = e))?;
        if !value.get("version").is_some_and(serde_json::Value::is_u64) {
            return Err(tr!("not a blueprint file (no version field)").into());
        }
        let bp: Blueprint = serde_json::from_value(value).map_err(|e| trf!("invalid format: {e}", e = e))?;
        if bp.version > VERSION {
            return Err(trf!("the blueprint uses a newer format (v{})", bp.version));
        }
        Ok(bp.normalized())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut bp = self.clone();
        bp.version = VERSION;
        let text = serde_json::to_string_pretty(&bp).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| trf!("Save failed: {e}", e = e))
    }

    /// Fixes out-of-range values. Node positions with NaN or infinity are dropped, the rest are clamped to ±POS_LIMIT
    /// (very distant coordinates break the canvas fit-to-view and grid math).
    pub fn normalized(mut self) -> Blueprint {
        self.sample_rows = self.sample_rows.min(MAX_SAMPLE_ROWS);
        self.theme = self.theme.min(crate::theme::THEMES.len() - 1);
        for g in &mut self.goals {
            g.pos = g.pos.and_then(sanitize_pos);
        }
        self.positions = std::mem::take(&mut self.positions)
            .into_iter()
            .filter_map(|(name, p)| Some((name, sanitize_pos(p)?)))
            .collect();
        self.llm_options = std::mem::take(&mut self.llm_options).normalized();
        self
    }

    /// Serialization of the whole content, compared to detect unsaved changes.
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Resolves the data path relative to the blueprint file's location.
    pub fn data_path_from(&self, blueprint_file: Option<&Path>) -> Option<PathBuf> {
        let p = PathBuf::from(self.data_path.as_ref()?.trim());
        if p.as_os_str().is_empty() {
            return None;
        }
        Some(match blueprint_file.and_then(Path::parent) {
            Some(dir) if p.is_relative() => dir.join(p),
            _ => p,
        })
    }

    /// Turns a data file path into the string to save, relative to the blueprint file's location.
    /// Both are made absolute first; if the data is under the blueprint's folder a `/`-separated relative path is used,
    /// otherwise the absolute path (saving a path relative to the working folder as is breaks when opened elsewhere).
    pub fn portable_data_path(data: &Path, blueprint_file: &Path) -> String {
        let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let data_abs = abs(data);
        let bp_abs = abs(blueprint_file);
        match bp_abs.parent().and_then(|dir| data_abs.strip_prefix(dir).ok()) {
            Some(rel) if !rel.as_os_str().is_empty() => rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            _ => data_abs.display().to_string(),
        }
    }

    pub fn llm_model(&self, p: Provider) -> Option<&str> {
        self.llm_models.iter().find(|(k, _)| *k == p).map(|(_, m)| m.as_str())
    }

    /// Constraint sentences that are on and not empty.
    pub fn active_constraints(&self) -> Vec<String> {
        self.goals
            .iter()
            .filter(|g| g.on && !g.text.trim().is_empty())
            .map(|g| g.text.trim().to_string())
            .collect()
    }

    pub fn to_request(&self, extra_constraints: Vec<String>, data: Option<LoadedData>) -> CompileRequest {
        let mut constraints = self.active_constraints();
        constraints.extend(extra_constraints);
        CompileRequest {
            target: self.target,
            role: self.role.clone(),
            constraints,
            format: self.format,
            fields: pipeline::parse_fields(&self.fields),
            data,
        }
    }
}

/// Built-in blueprint: name, summary, settings. The text (name, summary, role, constraints) is written in both
/// languages, and the current language is used when it is applied.
/// Field names are output schema identifiers, so they are the same in every language.
pub struct Preset {
    pub name: Tr,
    pub summary: Tr,
    pub target: TargetModel,
    pub format: OutputFormat,
    pub role: Tr,
    pub goals: &'static [Tr],
    pub fields: &'static str,
    pub sample_rows: usize,
    pub theme: usize,
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: trp!("Sales Outlier Analysis"),
        summary: trp!("Claude · JSON · 8 sample rows · Noir"),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: trp!("Data analyst who finds root causes in sales data"),
        goals: &[
            trp!("Exclude missing values (0.2%)"),
            trp!("Detect outliers first"),
            trp!("Do not infer anything not in the data"),
        ],
        fields: "root_causes,outliers,recommendations",
        sample_rows: 8,
        theme: 3,
    },
    Preset {
        name: trp!("Data Quality Check"),
        summary: trp!("GPT · Table · 12 sample rows · Misty Graveyard"),
        target: TargetModel::Gpt,
        format: OutputFormat::MarkdownTable,
        role: trp!("Data quality auditor"),
        goals: &[
            trp!("Report the missing-value ratio for each column"),
            trp!("Check for duplicate rows"),
            trp!("Do not infer anything not in the data"),
        ],
        fields: "column,issue,severity,fix",
        sample_rows: 12,
        theme: 5,
    },
    Preset {
        name: trp!("Executive Summary Report"),
        summary: trp!("Claude · JSON · 5 sample rows · Velvet"),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: trp!("Business analyst reporting to executives"),
        goals: &[
            trp!("Keep the summary to 3 sentences or fewer"),
            trp!("Round numbers to 2 decimal places"),
            trp!("Present the key metrics first"),
        ],
        fields: "headline,key_metrics,next_steps",
        sample_rows: 5,
        theme: 4,
    },
    Preset {
        name: trp!("Customer Segmentation"),
        summary: trp!("Gemini · Table · 20 sample rows · Classic Night"),
        target: TargetModel::Gemini,
        format: OutputFormat::MarkdownTable,
        role: trp!("Analyst who sorts customer data into segments"),
        goals: &[
            trp!("Classify by the 4M amount threshold"),
            trp!("Explain the basis for each segment in one line"),
            trp!("Mark anything that cannot be classified as unknown"),
        ],
        fields: "segment,reason,confidence",
        sample_rows: 20,
        theme: 0,
    },
    Preset {
        name: trp!("Blank Blueprint"),
        summary: trp!("Build it yourself from the defaults"),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: trp!("Data analyst"),
        goals: &[],
        fields: pipeline::DEFAULT_FIELDS,
        sample_rows: 8,
        theme: 3,
    },
];

impl Preset {
    /// Turns the built-in blueprint into settings. Data and node layout are left empty (the current data is kept).
    /// The name, role and constraints are filled in the current language (once applied they are user content and stay
    /// as they are when the language changes).
    pub fn to_blueprint(&self) -> Blueprint {
        Blueprint {
            project: format!("{}.kiro", self.name.get().replace(' ', "_")),
            target: self.target,
            role: self.role.get().into(),
            goals: self
                .goals
                .iter()
                .map(|g| GoalSpec {
                    text: g.get().to_string(),
                    on: true,
                    pos: None,
                })
                .collect(),
            format: self.format,
            fields: self.fields.into(),
            sample_rows: self.sample_rows,
            theme: self.theme,
            ..Blueprint::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    #[test]
    fn save_load_roundtrip_and_relative_data_path() {
        let dir = std::env::temp_dir().join(format!("sc_bp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.kiro");
        let mut bp = PRESETS[1].to_blueprint();
        bp.data_path = Some("sales.csv".into());
        bp.positions = vec![("root".into(), [1.0, 2.0])];
        bp.llm_models = vec![(Provider::OpenAi, "gpt-x".into())];
        bp.goals.push(GoalSpec {
            text: ko_fixture!("studio.goal_off").into(),
            on: false,
            pos: Some([3.0, 4.0]),
        });
        bp.save(&path).unwrap();
        let back = Blueprint::load(&path).unwrap();
        assert_eq!(back, bp);
        assert_eq!(back.data_path_from(Some(&path)), Some(dir.join("sales.csv")));
        assert_eq!(back.llm_model(Provider::OpenAi), Some("gpt-x"));
        assert_eq!(back.active_constraints().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_v1_project_file_and_bad_values_load() {
        let dir = std::env::temp_dir().join(format!("sc_bp1_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.kiro");
        // v1 format: no llm_models, out-of-range values, BOM
        std::fs::write(
            &path,
            "\u{feff}{\"version\":1,\"role\":\"r\",\"sample_rows\":999,\"theme\":42,\"target\":\"Gpt\"}",
        )
        .unwrap();
        let bp = Blueprint::load(&path).unwrap();
        assert_eq!(
            (bp.sample_rows, bp.theme, bp.target),
            (MAX_SAMPLE_ROWS, 6, TargetModel::Gpt)
        );
        std::fs::write(&path, "{\"version\":99}").unwrap();
        assert!(Blueprint::load(&path).unwrap_err().contains("v99"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_json_that_is_not_a_blueprint() {
        assert_eq!(
            with_lang(Lang::Ko, || Blueprint::parse("{}").unwrap_err()),
            ko_text!("not a blueprint file (no version field)")
        );
        let graph = r#"{"nodes":[{"id":1,"kind":"text"}],"edges":[]}"#;
        assert!(Blueprint::parse(graph).is_err());
        assert!(Blueprint::parse(r#"{"version":"2"}"#).is_err());
        assert!(Blueprint::parse(r#"{"version":1,"role":"r"}"#).is_ok());
        // The repository's canvas graph sample must not open as a blueprint either
        let sample = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json")).unwrap();
        assert!(Blueprint::parse(&sample).is_err());
    }

    #[test]
    fn portable_data_path_cases() {
        let root = std::env::temp_dir().join("sc_bp_paths");
        let bp = root.join("proj").join("plan.kiro");
        // Under the blueprint folder: a /-separated relative path
        assert_eq!(
            Blueprint::portable_data_path(&root.join("proj").join("d.csv"), &bp),
            "d.csv"
        );
        assert_eq!(
            Blueprint::portable_data_path(&root.join("proj").join("sub").join("s.csv"), &bp),
            "sub/s.csv"
        );
        // Sibling folder: an absolute path
        let sibling = root.join("other").join("x.csv");
        assert_eq!(
            Blueprint::portable_data_path(&sibling, &bp),
            sibling.display().to_string()
        );
        // Data relative to the working folder becomes an absolute path, so it also opens from other folders
        let rel = Path::new("samples").join("sales.csv");
        let saved = Blueprint::portable_data_path(&rel, &bp);
        assert!(Path::new(&saved).is_absolute(), "{saved}");
        let back = Blueprint {
            data_path: Some(saved),
            ..Blueprint::default()
        };
        assert_eq!(back.data_path_from(Some(&bp)), Some(std::path::absolute(&rel).unwrap()));
        // A saved relative path is resolved again relative to the blueprint
        let back = Blueprint {
            data_path: Some("sub/s.csv".into()),
            ..Blueprint::default()
        };
        assert_eq!(
            back.data_path_from(Some(&bp)),
            Some(root.join("proj").join("sub/s.csv"))
        );
    }

    #[test]
    fn presets_are_valid() {
        for lang in Lang::ALL {
            for p in &PRESETS {
                let bp = with_lang(lang, || p.to_blueprint());
                let name = p.name.ko();
                assert!(!pipeline::parse_fields(&bp.fields).is_empty(), "{name}");
                assert!(bp.theme < crate::theme::THEMES.len());
                let r = with_lang(lang, || pipeline::compile(&bp.to_request(vec![], None)));
                assert!(
                    !r.ast.lint_warnings.iter().any(|d| d.severity == "ERROR"),
                    "{name} {lang:?}"
                );
            }
        }
    }

    #[test]
    fn presets_exist_in_both_languages_with_identical_fields() {
        let hangul = |s: &str| s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c));
        for p in &PRESETS {
            for t in [p.name, p.summary, p.role].iter().chain(p.goals) {
                assert!(!t.ko().trim().is_empty() && !t.en().trim().is_empty(), "{t:?}");
                assert!(!hangul(t.en()), "{t:?}");
            }
            let ko = with_lang(Lang::Ko, || p.to_blueprint());
            let en = with_lang(Lang::En, || p.to_blueprint());
            // Only the text depends on the language: fields (output schema identifiers), target, format, sample row
            // count, theme and number of constraints are the same
            assert_eq!(ko.fields, en.fields, "{}", p.name.ko());
            assert!(!hangul(&en.fields));
            assert_eq!(
                (ko.target, ko.format, ko.sample_rows, ko.theme),
                (en.target, en.format, en.sample_rows, en.theme)
            );
            let on = |b: &Blueprint| b.goals.iter().map(|g| g.on).collect::<Vec<_>>();
            assert_eq!(on(&ko), on(&en));
            assert_eq!(ko.goals.len(), p.goals.len());
        }
    }

    #[test]
    fn applying_a_preset_uses_the_current_language() {
        let en = with_lang(Lang::En, || PRESETS[0].to_blueprint());
        assert_eq!(en.role, "Data analyst who finds root causes in sales data");
        let goals: Vec<&str> = en.goals.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(
            goals,
            [
                "Exclude missing values (0.2%)",
                "Detect outliers first",
                "Do not infer anything not in the data"
            ]
        );
        assert_eq!(en.project, "Sales_Outlier_Analysis.kiro");
        // Korean keeps the previous text
        let ko = with_lang(Lang::Ko, || PRESETS[0].to_blueprint());
        assert_eq!(ko.role, ko_text!("Data analyst who finds root causes in sales data"));
        assert_eq!(ko.goals[1].text, ko_text!("Detect outliers first"));
        assert_eq!(
            ko.project,
            format!("{}.kiro", ko_text!("Sales Outlier Analysis").replace(' ', "_"))
        );
    }

    #[test]
    fn load_errors_follow_the_language() {
        // A path whose parent is a file: both reading and writing fail
        let missing = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Cargo.toml")
            .join("plan.kiro");
        let missing = missing.as_path();
        with_lang(Lang::En, || {
            assert_eq!(
                Blueprint::parse("{}").unwrap_err(),
                "not a blueprint file (no version field)"
            );
            assert!(Blueprint::parse("{").unwrap_err().starts_with("invalid format: "));
            assert!(
                Blueprint::parse(r#"{"version":99}"#)
                    .unwrap_err()
                    .contains("newer format (v99)")
            );
            assert!(Blueprint::load(missing).unwrap_err().starts_with("cannot read file: "));
            let e = Blueprint::default().save(missing).unwrap_err();
            assert!(e.starts_with("Save failed: "), "{e}");
        });
        with_lang(Lang::Ko, || {
            // The text of a `{e}` template in front of the error detail
            let lead = |template: &'static str| template.strip_suffix("{e}").unwrap();
            assert!(
                Blueprint::parse("{")
                    .unwrap_err()
                    .starts_with(lead(ko_text!("invalid format: {e}")))
            );
            assert!(
                Blueprint::load(missing)
                    .unwrap_err()
                    .starts_with(lead(ko_text!("cannot read file: {e}")))
            );
            assert!(
                Blueprint::default()
                    .save(missing)
                    .unwrap_err()
                    .starts_with(lead(ko_text!("Save failed: {e}")))
            );
        });
    }

    #[test]
    fn v3_llm_options_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sc_bp3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v3.kiro");
        let mut bp = PRESETS[0].to_blueprint();
        bp.llm_options = LlmOptions {
            max_tokens: 8192,
            temperature: Some(0.7),
            system: ko_fixture!("studio.blueprint.system_prompt").into(),
        };
        bp.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"version\": 3") && text.contains("\"llm_options\""),
            "{text}"
        );
        let back = Blueprint::load(&path).unwrap();
        assert_eq!(back, bp);
        assert_eq!(back.llm_options.temperature, Some(0.7));
        assert_eq!(back.fingerprint(), bp.fingerprint());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_and_v2_files_get_default_llm_options() {
        let v2 = r#"{"version":2,"target":"Gpt","role":"r","llm_models":[["openai","gpt-x"]]}"#;
        let bp = Blueprint::parse(v2).unwrap();
        assert_eq!(bp.llm_options, LlmOptions::default());
        assert_eq!(bp.llm_options.max_tokens, DEFAULT_LLM_MAX_TOKENS);
        assert_eq!(bp.llm_model(Provider::OpenAi), Some("gpt-x"));
        let v1 = r#"{"version":1,"role":"r","goals":[{"text":"g","on":true,"pos":[1.0,2.0]}],"positions":[["root",[3.0,4.0]]]}"#;
        let bp = Blueprint::parse(v1).unwrap();
        assert_eq!(bp.llm_options, LlmOptions::default());
        assert_eq!(bp.goals[0].pos, Some([1.0, 2.0]));
        assert_eq!(bp.positions, vec![("root".to_string(), [3.0, 4.0])]);
        // A partial llm_options gets defaults for the rest
        let partial = r#"{"version":3,"llm_options":{"temperature":0.3}}"#;
        let o = Blueprint::parse(partial).unwrap().llm_options;
        assert_eq!((o.max_tokens, o.temperature, o.system.as_str()), (2048, Some(0.3), ""));
    }

    #[test]
    fn llm_options_are_clamped() {
        let bad = r#"{"version":3,"llm_options":{"max_tokens":10,"temperature":5.0}}"#;
        let o = Blueprint::parse(bad).unwrap().llm_options;
        assert_eq!((o.max_tokens, o.temperature), (MIN_LLM_MAX_TOKENS, Some(2.0)));
        let big = r#"{"version":3,"llm_options":{"max_tokens":999999,"temperature":-1.0}}"#;
        let o = Blueprint::parse(big).unwrap().llm_options;
        assert_eq!((o.max_tokens, o.temperature), (MAX_LLM_MAX_TOKENS, Some(0.0)));
        let nan = LlmOptions {
            temperature: Some(f32::NAN),
            ..LlmOptions::default()
        };
        assert_eq!(nan.normalized().temperature, None);
        // The value clamped to the provider's range is sent, but the saved value does not change
        let o = LlmOptions {
            temperature: Some(1.6),
            ..LlmOptions::default()
        };
        assert_eq!(o.effective_temperature(Provider::OpenAi), Some(1.6));
        assert_eq!(o.effective_temperature(Provider::Anthropic), Some(1.0));
        assert_eq!(o.temperature, Some(1.6));
        assert_eq!(LlmOptions::default().effective_temperature(Provider::Gemini), None);
        assert_eq!(temperature_max(Provider::Gemini), 2.0);
    }

    #[test]
    fn node_positions_are_sanitized() {
        // Far coordinates such as 3e9 used to hang the canvas grid loop: they are clamped to the allowed range. 1e39
        // (f32 infinity) is dropped.
        let text = r#"{"version":2,"goals":[{"text":"a","on":true,"pos":[0.0,3e9]},{"text":"b","on":true,"pos":[1e39,0.0]}],
            "positions":[["root",[-5e9,10.0]],["data",[1e39,1.0]],["llm",[5.0,6.0]]]}"#;
        let bp = Blueprint::parse(text).unwrap();
        assert_eq!(bp.goals[0].pos, Some([0.0, POS_LIMIT]));
        assert_eq!(bp.goals[1].pos, None);
        assert_eq!(
            bp.positions,
            vec![
                ("root".to_string(), [-POS_LIMIT, 10.0]),
                ("llm".to_string(), [5.0, 6.0])
            ]
        );
        let direct = Blueprint {
            goals: vec![GoalSpec {
                text: "x".into(),
                on: true,
                pos: Some([f32::NAN, 1.0]),
            }],
            positions: vec![("role".into(), [f32::INFINITY, 0.0]), ("root".into(), [1.0, 2.0])],
            ..Blueprint::default()
        }
        .normalized();
        assert_eq!(direct.goals[0].pos, None);
        assert_eq!(direct.positions, vec![("root".to_string(), [1.0, 2.0])]);
        assert_eq!(sanitize_pos([12.5, -3.0]), Some([12.5, -3.0]));
    }

    #[test]
    fn fingerprint_tracks_every_saved_setting() {
        let base = PRESETS[0].to_blueprint();
        let fp = base.fingerprint();
        assert_eq!(base.clone().fingerprint(), fp);
        let changed = [
            Blueprint {
                role: ko_fixture!("studio.blueprint.other_role").into(),
                ..base.clone()
            },
            Blueprint {
                positions: vec![("root".into(), [1.0, 2.0])],
                ..base.clone()
            },
            Blueprint {
                llm_options: LlmOptions {
                    max_tokens: 4096,
                    ..LlmOptions::default()
                },
                ..base.clone()
            },
            Blueprint {
                theme: 0,
                ..base.clone()
            },
        ];
        for c in changed {
            assert_ne!(c.fingerprint(), fp);
        }
    }
}
