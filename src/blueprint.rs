//! 블루프린트: 스튜디오 전체 설정을 담는 파일(.kiro, JSON).
//! 데이터 경로·역할·제약·출력 포맷·필드·타겟·샘플 행 수·테마·LLM 모델·노드 배치를 한 번에 저장/적용한다.
//! GUI(캔버스 메뉴, 열기/저장)와 CLI(`--cli --blueprint 파일`)가 같은 형식을 쓴다.

use crate::ast::{OutputFormat, TargetModel};
use crate::canvas::graph::Provider;
use crate::pipeline::{self, CompileRequest, LoadedData};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 2;

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
        }
    }
}

pub const MAX_SAMPLE_ROWS: usize = 50;

impl Blueprint {
    pub fn load(path: &Path) -> Result<Blueprint, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("파일을 읽을 수 없음: {e}"))?;
        let bp: Blueprint =
            serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("형식 오류: {e}"))?;
        if bp.version > VERSION {
            return Err(format!("더 새로운 형식(v{})의 블루프린트입니다", bp.version));
        }
        Ok(bp.normalized())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut bp = self.clone();
        bp.version = VERSION;
        let text = serde_json::to_string_pretty(&bp).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("저장 실패: {e}"))
    }

    /// 범위를 벗어난 값을 바로잡는다.
    pub fn normalized(mut self) -> Blueprint {
        self.sample_rows = self.sample_rows.min(MAX_SAMPLE_ROWS);
        self.theme = self.theme.min(crate::theme::THEMES.len() - 1);
        self
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

/// 내장 블루프린트: 이름, 설명, 설정.
pub struct Preset {
    pub name: &'static str,
    pub summary: &'static str,
    pub target: TargetModel,
    pub format: OutputFormat,
    pub role: &'static str,
    pub goals: &'static [&'static str],
    pub fields: &'static str,
    pub sample_rows: usize,
    pub theme: usize,
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "매출 이상치 분석",
        summary: "Claude · JSON · 샘플 8행 · 느와르",
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: "매출 데이터를 분석해 근본 원인을 찾는 데이터 분석가",
        goals: &[
            "결측치 0.2% 제외",
            "이상치 우선 탐지",
            "데이터에 없는 내용은 추론하지 마",
        ],
        fields: "root_causes,outliers,recommendations",
        sample_rows: 8,
        theme: 3,
    },
    Preset {
        name: "데이터 품질 점검",
        summary: "GPT · 표 · 샘플 12행 · 안개 묘지",
        target: TargetModel::Gpt,
        format: OutputFormat::MarkdownTable,
        role: "데이터 품질 감사관",
        goals: &[
            "결측치 비율을 컬럼별로 보고",
            "중복 행 여부를 확인",
            "데이터에 없는 내용은 추론하지 마",
        ],
        fields: "column,issue,severity,fix",
        sample_rows: 12,
        theme: 5,
    },
    Preset {
        name: "경영진 요약 리포트",
        summary: "Claude · JSON · 샘플 5행 · 벨벳",
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: "경영진에게 보고하는 비즈니스 분석가",
        goals: &[
            "요약은 3문장 이내",
            "숫자는 소수점 둘째 자리까지",
            "핵심 지표를 먼저 제시",
        ],
        fields: "headline,key_metrics,next_steps",
        sample_rows: 5,
        theme: 4,
    },
    Preset {
        name: "고객 세그먼트 분류",
        summary: "Gemini · 표 · 샘플 20행 · 클래식 나이트",
        target: TargetModel::Gemini,
        format: OutputFormat::MarkdownTable,
        role: "고객 데이터를 세그먼트로 분류하는 분석가",
        goals: &[
            "금액 4M 기준으로 분류",
            "분류 근거를 한 줄로 설명",
            "분류할 수 없으면 unknown으로 표기",
        ],
        fields: "segment,reason,confidence",
        sample_rows: 20,
        theme: 0,
    },
    Preset {
        name: "빈 블루프린트",
        summary: "기본값에서 직접 구성",
        target: TargetModel::Claude,
        format: OutputFormat::StrictJson,
        role: "데이터 분석가",
        goals: &[],
        fields: pipeline::DEFAULT_FIELDS,
        sample_rows: 8,
        theme: 3,
    },
];

impl Preset {
    /// 내장 블루프린트를 설정으로 만든다. 데이터와 노드 배치는 비워 둔다(현재 데이터를 유지).
    pub fn to_blueprint(&self) -> Blueprint {
        Blueprint {
            project: format!("{}.kiro", self.name.replace(' ', "_")),
            target: self.target,
            role: self.role.into(),
            goals: self
                .goals
                .iter()
                .map(|g| GoalSpec {
                    text: g.to_string(),
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
    fn presets_are_valid() {
        for p in &PRESETS {
            let bp = p.to_blueprint();
            assert!(!pipeline::parse_fields(&bp.fields).is_empty(), "{}", p.name);
            assert!(bp.theme < crate::theme::THEMES.len());
            let r = pipeline::compile(&bp.to_request(vec![], None));
            assert!(!r.ast.lint_warnings.iter().any(|d| d.severity == "ERROR"), "{}", p.name);
        }
    }
}
