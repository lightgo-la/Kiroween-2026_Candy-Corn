//! 블루프린트: 스튜디오 전체 설정을 담는 파일(.kiro, JSON).
//! 데이터 경로·역할·제약·출력 포맷·필드·타겟·샘플 행 수·테마·LLM 모델·LLM 실행 옵션·노드 배치를 한 번에 저장/적용한다.
//! GUI(캔버스 메뉴, 열기/저장)와 CLI(`--cli --blueprint 파일`)가 같은 형식을 쓴다.
//!
//! 버전: v1(프로젝트 파일) → v2(llm_models) → v3(llm_options). 빠진 항목은 기본값으로 읽으므로 예전 파일도 열린다.

use crate::ast::{OutputFormat, TargetModel};
use crate::canvas::graph::Provider;
use crate::i18n::Tr;
use crate::pipeline::{self, CompileRequest, LoadedData};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 3;

/// 노드 좌표의 허용 범위(±). 이보다 먼 좌표는 f32 정밀도가 떨어져 캔버스 계산(격자, 화면 맞춤)이 망가진다.
pub const POS_LIMIT: f32 = 100_000.0;

/// 저장된 노드 좌표를 검사한다. NaN·무한대가 있으면 버리고(None), 너무 먼 값은 ±POS_LIMIT로 자른다.
pub fn sanitize_pos(p: [f32; 2]) -> Option<[f32; 2]> {
    p.iter()
        .all(|v| v.is_finite())
        .then(|| p.map(|v| v.clamp(-POS_LIMIT, POS_LIMIT)))
}

pub const MIN_LLM_MAX_TOKENS: u32 = 256;
pub const MAX_LLM_MAX_TOKENS: u32 = 32_000;
pub const DEFAULT_LLM_MAX_TOKENS: u32 = 2048;

/// LLM 실행 옵션 (v3부터 저장). v1/v2 파일에는 없으므로 기본값으로 읽는다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmOptions {
    /// 출력 토큰 한도 (MIN_LLM_MAX_TOKENS..=MAX_LLM_MAX_TOKENS)
    pub max_tokens: u32,
    /// 온도. None이면 요청에 넣지 않아 제공자 기본값을 쓴다
    pub temperature: Option<f32>,
    /// 시스템 프롬프트. 비어 있으면 보내지 않는다
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

/// 제공자별 온도 상한 (Anthropic 0~1, OpenAI·Gemini 0~2).
pub fn temperature_max(p: Provider) -> f32 {
    match p {
        Provider::Anthropic => 1.0,
        Provider::OpenAi | Provider::Gemini => 2.0,
    }
}

impl LlmOptions {
    /// 범위를 벗어난 값을 바로잡는다. 온도는 제공자와 상관없는 최대 범위(0~2)로만 자르고,
    /// 제공자별 범위는 보낼 때(effective_temperature) 맞춘다.
    pub fn normalized(mut self) -> LlmOptions {
        self.max_tokens = self.max_tokens.clamp(MIN_LLM_MAX_TOKENS, MAX_LLM_MAX_TOKENS);
        self.temperature = self.temperature.filter(|t| t.is_finite()).map(|t| t.clamp(0.0, 2.0));
        self
    }

    /// 제공자에게 실제로 보낼 온도. 저장된 값은 그대로 두고 제공자 범위로 자른 값을 쓴다
    /// (GPT에서 1.5로 둔 값을 Claude로 바꿔 1.0으로 보내도, 다시 GPT로 돌아오면 1.5가 남아 있다).
    pub fn effective_temperature(&self, p: Provider) -> Option<f32> {
        self.temperature.map(|t| t.clamp(0.0, temperature_max(p)))
    }
}

/// 필드가 빠진 예전 파일(v1 프로젝트 파일 포함)도 읽을 수 있게 모든 필드에 기본값을 둔다.
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
    /// 테마 번호 (0부터, 디자인샘플 01 = 0)
    pub theme: usize,
    /// 데이터 파일 경로. 상대 경로는 블루프린트 파일이 있는 폴더 기준이다.
    pub data_path: Option<String>,
    /// 고정 노드 위치 (data, role, output, root, result, llm, settings)
    pub positions: Vec<(String, [f32; 2])>,
    /// 제공자별 LLM 모델 이름
    pub llm_models: Vec<(Provider, String)>,
    /// LLM 실행 옵션: 출력 토큰 한도, 온도, 시스템 프롬프트 (v3)
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
        let text =
            std::fs::read_to_string(path).map_err(|e| trf!("파일을 읽을 수 없음: {e}", "cannot read file: {e}"))?;
        Blueprint::parse(&text)
    }

    /// 블루프린트 JSON을 읽는다. 모든 블루프린트(v1 프로젝트 파일 포함)에는 숫자 version 키가 있으므로,
    /// 그것이 없는 JSON은 다른 종류의 파일로 보고 거부한다.
    pub fn parse(text: &str) -> Result<Blueprint, String> {
        let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|e| trf!("형식 오류: {e}", "invalid format: {e}"))?;
        if !value.get("version").is_some_and(serde_json::Value::is_u64) {
            return Err(tr!(
                "블루프린트 파일이 아닙니다 (version 항목 없음)",
                "not a blueprint file (no version field)"
            )
            .into());
        }
        let bp: Blueprint = serde_json::from_value(value).map_err(|e| trf!("형식 오류: {e}", "invalid format: {e}"))?;
        if bp.version > VERSION {
            return Err(trf!(
                "더 새로운 형식(v{})의 블루프린트입니다",
                "the blueprint uses a newer format (v{})",
                bp.version
            ));
        }
        Ok(bp.normalized())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut bp = self.clone();
        bp.version = VERSION;
        let text = serde_json::to_string_pretty(&bp).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| trf!("저장 실패: {e}", "Save failed: {e}"))
    }

    /// 범위를 벗어난 값을 바로잡는다. 노드 좌표는 NaN·무한대를 버리고 ±POS_LIMIT로 자른다
    /// (아주 먼 좌표는 캔버스의 화면 맞춤·격자 계산을 망가뜨린다).
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

    /// 저장하지 않은 변경을 찾기 위한 비교용 직렬화 (전체 내용).
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// 데이터 경로를 블루프린트 파일 위치 기준으로 해석한다.
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

    /// 데이터 파일 경로를 블루프린트 파일 위치 기준으로 저장할 문자열로 바꾼다.
    /// 둘 다 절대 경로로 만든 뒤, 데이터가 블루프린트 폴더 아래에 있으면 `/` 구분 상대 경로를,
    /// 아니면 절대 경로를 쓴다(작업 폴더 기준 상대 경로를 그대로 저장하면 다른 곳에서 열 때 깨진다).
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

    /// 사용 중이고 비어 있지 않은 제약 문장.
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

/// 내장 블루프린트: 이름, 설명, 설정. 문구(이름·설명·역할·제약)는 두 언어로 적고 적용할 때 현재 언어를 쓴다.
/// 필드 이름은 출력 스키마의 식별자라 언어와 관계없이 같다.
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
        name: Tr("매출 이상치 분석", "Sales Outlier Analysis"),
        summary: Tr(
            "Claude · JSON · 샘플 8행 · 느와르",
            "Claude · JSON · 8 sample rows · Noir",
        ),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: Tr(
            "매출 데이터를 분석해 근본 원인을 찾는 데이터 분석가",
            "Data analyst who finds root causes in sales data",
        ),
        goals: &[
            Tr("결측치 0.2% 제외", "Exclude missing values (0.2%)"),
            Tr("이상치 우선 탐지", "Detect outliers first"),
            Tr(
                "데이터에 없는 내용은 추론하지 마",
                "Do not infer anything not in the data",
            ),
        ],
        fields: "root_causes,outliers,recommendations",
        sample_rows: 8,
        theme: 3,
    },
    Preset {
        name: Tr("데이터 품질 점검", "Data Quality Check"),
        summary: Tr(
            "GPT · 표 · 샘플 12행 · 안개 묘지",
            "GPT · Table · 12 sample rows · Misty Graveyard",
        ),
        target: TargetModel::Gpt,
        format: OutputFormat::MarkdownTable,
        role: Tr("데이터 품질 감사관", "Data quality auditor"),
        goals: &[
            Tr(
                "결측치 비율을 컬럼별로 보고",
                "Report the missing-value ratio for each column",
            ),
            Tr("중복 행 여부를 확인", "Check for duplicate rows"),
            Tr(
                "데이터에 없는 내용은 추론하지 마",
                "Do not infer anything not in the data",
            ),
        ],
        fields: "column,issue,severity,fix",
        sample_rows: 12,
        theme: 5,
    },
    Preset {
        name: Tr("경영진 요약 리포트", "Executive Summary Report"),
        summary: Tr(
            "Claude · JSON · 샘플 5행 · 벨벳",
            "Claude · JSON · 5 sample rows · Velvet",
        ),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: Tr(
            "경영진에게 보고하는 비즈니스 분석가",
            "Business analyst reporting to executives",
        ),
        goals: &[
            Tr("요약은 3문장 이내", "Keep the summary to 3 sentences or fewer"),
            Tr("숫자는 소수점 둘째 자리까지", "Round numbers to 2 decimal places"),
            Tr("핵심 지표를 먼저 제시", "Present the key metrics first"),
        ],
        fields: "headline,key_metrics,next_steps",
        sample_rows: 5,
        theme: 4,
    },
    Preset {
        name: Tr("고객 세그먼트 분류", "Customer Segmentation"),
        summary: Tr(
            "Gemini · 표 · 샘플 20행 · 클래식 나이트",
            "Gemini · Table · 20 sample rows · Classic Night",
        ),
        target: TargetModel::Gemini,
        format: OutputFormat::MarkdownTable,
        role: Tr(
            "고객 데이터를 세그먼트로 분류하는 분석가",
            "Analyst who sorts customer data into segments",
        ),
        goals: &[
            Tr("금액 4M 기준으로 분류", "Classify by the 4M amount threshold"),
            Tr(
                "분류 근거를 한 줄로 설명",
                "Explain the basis for each segment in one line",
            ),
            Tr(
                "분류할 수 없으면 unknown으로 표기",
                "Mark anything that cannot be classified as unknown",
            ),
        ],
        fields: "segment,reason,confidence",
        sample_rows: 20,
        theme: 0,
    },
    Preset {
        name: Tr("빈 블루프린트", "Blank Blueprint"),
        summary: Tr("기본값에서 직접 구성", "Build it yourself from the defaults"),
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: Tr("데이터 분석가", "Data analyst"),
        goals: &[],
        fields: pipeline::DEFAULT_FIELDS,
        sample_rows: 8,
        theme: 3,
    },
];

impl Preset {
    /// 내장 블루프린트를 설정으로 만든다. 데이터와 노드 배치는 비워 둔다(현재 데이터를 유지).
    /// 이름·역할·제약은 지금 언어의 문구로 채운다(적용한 뒤에는 사용자 내용이라 언어를 바꿔도 그대로 둔다).
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
            text: "꺼진 제약".into(),
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
        // v1 형식: llm_models 없음, 범위 밖 값, BOM
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
        assert!(
            Blueprint::parse("{}")
                .unwrap_err()
                .contains("블루프린트 파일이 아닙니다")
        );
        let graph = r#"{"nodes":[{"id":1,"kind":"text"}],"edges":[]}"#;
        assert!(Blueprint::parse(graph).is_err());
        assert!(Blueprint::parse(r#"{"version":"2"}"#).is_err());
        assert!(Blueprint::parse(r#"{"version":1,"role":"r"}"#).is_ok());
        // 저장소의 캔버스 그래프 샘플도 블루프린트로 열리면 안 된다
        let sample = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/samples/flow.canvas.json")).unwrap();
        assert!(Blueprint::parse(&sample).is_err());
    }

    #[test]
    fn portable_data_path_cases() {
        let root = std::env::temp_dir().join("sc_bp_paths");
        let bp = root.join("proj").join("plan.kiro");
        // 블루프린트 폴더 아래: / 구분 상대 경로
        assert_eq!(
            Blueprint::portable_data_path(&root.join("proj").join("d.csv"), &bp),
            "d.csv"
        );
        assert_eq!(
            Blueprint::portable_data_path(&root.join("proj").join("sub").join("s.csv"), &bp),
            "sub/s.csv"
        );
        // 형제 폴더: 절대 경로
        let sibling = root.join("other").join("x.csv");
        assert_eq!(
            Blueprint::portable_data_path(&sibling, &bp),
            sibling.display().to_string()
        );
        // 작업 폴더 기준 상대 경로 데이터는 절대 경로로 바뀌어 다른 폴더에서도 열린다
        let rel = Path::new("samples").join("sales.csv");
        let saved = Blueprint::portable_data_path(&rel, &bp);
        assert!(Path::new(&saved).is_absolute(), "{saved}");
        let back = Blueprint {
            data_path: Some(saved),
            ..Blueprint::default()
        };
        assert_eq!(back.data_path_from(Some(&bp)), Some(std::path::absolute(&rel).unwrap()));
        // 저장한 상대 경로는 블루프린트 기준으로 다시 해석된다
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
        let hangul = |s: &str| s.chars().any(|c| ('가'..='힣').contains(&c));
        for p in &PRESETS {
            for t in [p.name, p.summary, p.role].iter().chain(p.goals) {
                assert!(!t.ko().trim().is_empty() && !t.en().trim().is_empty(), "{t:?}");
                assert!(!hangul(t.en()), "{t:?}");
            }
            let ko = with_lang(Lang::Ko, || p.to_blueprint());
            let en = with_lang(Lang::En, || p.to_blueprint());
            // 언어에 따라 바뀌는 것은 문구뿐이다: 필드(출력 스키마 식별자)·타겟·형식·샘플 행 수·테마·제약 수는 같다
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
        // 한국어는 예전 문구 그대로
        let ko = with_lang(Lang::Ko, || PRESETS[0].to_blueprint());
        assert_eq!(ko.role, "매출 데이터를 분석해 근본 원인을 찾는 데이터 분석가");
        assert_eq!(ko.goals[1].text, "이상치 우선 탐지");
        assert_eq!(ko.project, "매출_이상치_분석.kiro");
    }

    #[test]
    fn load_errors_follow_the_language() {
        // 부모가 파일인 경로: 읽기와 쓰기가 모두 실패한다
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
            assert!(Blueprint::parse("{").unwrap_err().starts_with("형식 오류: "));
            assert!(
                Blueprint::load(missing)
                    .unwrap_err()
                    .starts_with("파일을 읽을 수 없음: ")
            );
            assert!(
                Blueprint::default()
                    .save(missing)
                    .unwrap_err()
                    .starts_with("저장 실패: ")
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
            system: "너는 신중한 분석가다.\n숫자는 근거와 함께.".into(),
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
        // 일부만 있는 llm_options도 나머지는 기본값
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
        // 제공자 범위로 자른 값을 보내되, 저장된 값은 바꾸지 않는다
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
        // 3e9 같은 먼 좌표는 캔버스 격자 루프를 멈추게 했다: 허용 범위로 자른다. 1e39(f32 무한대)는 버린다.
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
                role: "다른 역할".into(),
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
