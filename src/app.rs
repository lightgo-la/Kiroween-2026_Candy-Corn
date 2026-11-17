//! egui 3-Pane UI (좌: 입력 폼 / 중: 캔버스·AST X-Ray / 우: 출력 & 벤치마크).
//! 무거운 작업(프로파일링, 컴파일, LLM 호출)은 워커 스레드에서 실행하고 mpsc 채널로 결과만 받는다.

use crate::ast::{ConstraintNode, OutputFormat, TargetModel};
use crate::blueprint::{self, Blueprint, GoalSpec, LlmOptions, PRESETS};
use crate::canvas::{graph::Provider, llm};
use crate::history::{self, History};
use crate::pipeline::{self, CompileRequest, CompileResult, LoadedData, TargetCost};
use crate::studio_canvas::{self, Action, Canvas, Goal, LlmView, Model, NodeKey};
use crate::theme::{self, Palette, THEMES};
use crate::tokenizer;
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

enum Msg {
    Loaded(u64, PathBuf, Result<LoadedData, String>),
}

enum CompileMsg {
    /// 컴파일 결과와, 같은 입력을 타겟별로 컴파일한 비교
    Done(u64, Box<CompileResult>, Vec<TargetCost>),
    Failed(u64, String),
}

/// 실제 LLM 실행 한 번의 결과.
struct LlmRun {
    out: Result<pipeline::OutputCheck, String>,
    /// 요청부터 응답까지 걸린 시간 (일시 오류 재시도 포함, ms)
    ms: u64,
    /// 이 실행에 쓴 출력 토큰 한도
    max_tokens: u32,
}

/// 실제 LLM 실행 결과. 첫 값은 요청 세대(오래된 응답은 버린다).
enum LlmMsg {
    Done(u64, LlmRun),
}

/// LLM 실행 세대 관리. 새 실행·취소·재컴파일·블루프린트 적용은 세대를 올려 진행 중인 응답을 버린다.
/// (워커의 HTTP 호출은 멈출 수 없으므로 취소는 "응답을 기다리지 않고 버리기"다.)
#[derive(Default, Debug)]
struct LlmJobs {
    generation: u64,
    running: bool,
}

impl LlmJobs {
    fn start(&mut self) -> u64 {
        self.generation += 1;
        self.running = true;
        self.generation
    }

    /// 진행 중인 실행을 버린다. 실행 중이었으면 true.
    fn cancel(&mut self) -> bool {
        self.generation += 1;
        std::mem::take(&mut self.running)
    }

    /// 응답이 지금 실행의 것이면 받아들이고 실행 상태를 끝낸다.
    fn accept(&mut self, generation: u64) -> bool {
        let current = self.running && generation == self.generation;
        if current {
            self.running = false;
        }
        current
    }
}

/// 실행 취소 대상이 되는 편집 상태.
#[derive(Clone, PartialEq)]
struct Snapshot {
    target: TargetModel,
    role: String,
    goals: Vec<Goal>,
    format: OutputFormat,
    fields_text: String,
}

/// 컴파일 타겟에 맞는 LLM 제공자.
fn provider_for(t: TargetModel) -> Provider {
    match t {
        TargetModel::Claude => Provider::Anthropic,
        TargetModel::Gpt => Provider::OpenAi,
        TargetModel::Gemini => Provider::Gemini,
    }
}

fn provider_index(p: Provider) -> usize {
    Provider::ALL.iter().position(|x| *x == p).unwrap_or(0)
}

/// 환경 변수에서 API 키를 읽는다. 키는 화면·파일·로그에 남기지 않는다.
/// 실행 중인 프로세스는 나중에 설정한 환경 변수를 보지 못하므로 설정 후 앱을 다시 시작해야 한다.
fn api_key(p: Provider) -> Option<String> {
    std::env::var(p.default_key_env())
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// 입력칸마다 남아 있는 egui 자체 실행 취소 기록을 지운다.
/// 앱이 글자를 바꾼 뒤(실행 취소, 프로젝트 불러오기, 블루프린트 적용) 옛 글자가 되살아나는 것을 막는다.
fn clear_text_undo(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_by_type::<egui::text_edit::TextEditState>());
}

/// 상태 줄에 넣을 짧은 문구. 긴 오류 본문은 마우스를 올렸을 때만 보여 준다.
fn short(s: &str, n: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}

/// LLM 실행 결과의 상태 줄 문구. 잘리거나 중단된 응답은 형식 위반과 구분해 알린다.
fn llm_status(out: &Result<pipeline::OutputCheck, String>) -> String {
    match out {
        Ok(c) if c.ok => "LLM 응답 · 출력 형식·필수 필드 검증 통과".into(),
        Ok(c) => match &c.cut_off {
            Some(cut) => format!("LLM 응답 · {cut} (오른쪽 VIII 참고)"),
            None => "LLM 응답 · 출력 형식 위반 (오른쪽 VIII 참고)".into(),
        },
        Err(_) => "LLM 실행 실패 (오른쪽 VIII에 자세한 내용)".into(),
    }
}

/// 캔버스 LLM 노드에 보여 줄 (통과 여부, 한 줄 요약).
fn llm_verdict(out: &Result<pipeline::OutputCheck, String>) -> (bool, String) {
    match out {
        Ok(c) if c.ok && c.table_rows == Some(0) => (true, "✔ 형식 통과 · ⚠ 표에 데이터 행 없음".to_string()),
        Ok(c) if c.ok => (true, "✔ 출력 형식·필수 필드 통과".to_string()),
        Ok(c) => (
            false,
            c.cut_off
                .clone()
                .or_else(|| c.error.clone())
                .unwrap_or_else(|| format!("누락 필드: {}", c.missing.join(", "))),
        ),
        Err(e) => (false, short(e, 80)),
    }
}

/// VIII 검사 결과 줄의 색 구분.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Ok,
    Err,
    Warn,
    Muted,
}

fn tone_color(t: Tone, p: &Palette) -> egui::Color32 {
    match t {
        Tone::Ok => theme::OK,
        Tone::Err => theme::ERR,
        Tone::Warn => p.warn(),
        Tone::Muted => p.sub,
    }
}

/// VIII에 보여 줄 검사 결과 줄. 잘린 응답이면 잘림을 먼저 알리고 형식 검사 결과는 흐린 한 줄로만 덧붙인다
/// (잘린 JSON의 파싱 실패·누락 필드가 형식 위반처럼 보이지 않게). 파싱이나 표 찾기에 실패하면
/// 누락 필드는 언제나 전체 목록이므로 따로 나열하지 않는다.
fn check_lines(c: &pipeline::OutputCheck) -> Vec<(Tone, String)> {
    if let Some(cut) = &c.cut_off {
        return vec![
            (Tone::Err, format!("✖ {cut}")),
            (Tone::Muted, format!("형식 검사(잘린 응답 기준): {}", format_summary(c))),
        ];
    }
    let mut lines = Vec::new();
    if c.ok {
        lines.push((Tone::Ok, "✔ 출력 형식·필수 필드 검증 통과".to_string()));
        if !c.nulls.is_empty() {
            lines.push((Tone::Warn, format!("⚠ 값이 비어 있는 필드: {}", c.nulls.join(", "))));
        }
    } else if let Some(e) = &c.error {
        lines.push((Tone::Err, format!("✖ {e}")));
    } else if !c.missing.is_empty() {
        lines.push((Tone::Warn, format!("⚠ 누락 필드: {}", c.missing.join(", "))));
    }
    if c.table_rows == Some(0) {
        lines.push((Tone::Warn, "⚠ 표에 데이터 행이 없습니다 (헤더만 있음)".to_string()));
    }
    lines
}

/// 형식 검사 결과 한 줄 요약 (잘린 응답에 덧붙이는 흐린 줄).
fn format_summary(c: &pipeline::OutputCheck) -> String {
    if c.pretty.trim().is_empty() {
        return "본문 없음".into();
    }
    match &c.error {
        Some(e) => e.clone(),
        None if !c.missing.is_empty() => format!("누락 필드: {}", c.missing.join(", ")),
        None => "출력 형식·필수 필드 일치".into(),
    }
}

/// 응답 시간과 제공자가 보고한 토큰 사용량, 컴파일된 프롬프트의 추정 토큰 수.
fn run_details(ms: u64, usage: Option<llm::Usage>, prompt_tokens: usize) -> String {
    match usage {
        Some(u) => format!(
            "응답 {ms} ms · 입력 {} · 출력 {} 토큰 (압축 프롬프트 추정 {prompt_tokens})",
            u.input, u.output
        ),
        None => format!("응답 {ms} ms · 사용량 정보 없음 (압축 프롬프트 추정 {prompt_tokens} 토큰)"),
    }
}

fn run_line(run: &LlmRun, prompt_tokens: usize) -> String {
    match &run.out {
        Ok(c) => run_details(run.ms, c.usage, prompt_tokens),
        Err(_) => format!("응답 {} ms 후 실패", run.ms),
    }
}

/// 출력 한도를 두 배로 (상한 MAX_LLM_MAX_TOKENS). 이미 상한이면 None.
fn doubled_limit(n: u32) -> Option<u32> {
    (n < blueprint::MAX_LLM_MAX_TOKENS).then(|| n.saturating_mul(2).min(blueprint::MAX_LLM_MAX_TOKENS))
}

/// 출력 토큰 한도에서 잘린 응답이면 다시 실행할 때 쓸 늘린 한도.
fn retry_limit(run: &LlmRun) -> Option<u32> {
    match &run.out {
        Ok(c) if c.hit_max_tokens() => doubled_limit(run.max_tokens),
        _ => None,
    }
}

/// 데이터로 불러오는 파일 확장자 (파일 대화상자와 끌어다 놓기가 같은 목록을 쓴다).
const DATA_EXTS: [&str; 6] = ["csv", "tsv", "txt", "json", "jsonl", "ndjson"];

/// 끌어다 놓은(또는 시작 인자로 받은) 파일의 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropKind {
    Blueprint,
    Data,
    Unsupported,
}

/// 확장자로 파일 종류를 정한다 (대소문자 무시). .kiro는 블루프린트, 데이터 확장자는 프로파일러로, 나머지는 거부.
fn drop_kind(path: &Path) -> DropKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "kiro" {
        DropKind::Blueprint
    } else if DATA_EXTS.contains(&ext.as_str()) {
        DropKind::Data
    } else {
        DropKind::Unsupported
    }
}

/// 여러 파일을 한꺼번에 놓으면 처리할 수 있는 첫 파일을 고른다(없으면 첫 파일로 오류를 알린다).
fn pick_drop(paths: &[PathBuf]) -> Option<&PathBuf> {
    paths
        .iter()
        .find(|p| drop_kind(p) != DropKind::Unsupported)
        .or(paths.first())
}

/// 파일을 끌고 있을 때 상태 줄 안내.
fn drop_hint(kind: Option<DropKind>) -> &'static str {
    match kind {
        Some(DropKind::Blueprint) => "여기에 놓으면 블루프린트를 엽니다",
        Some(DropKind::Data) => "여기에 놓으면 데이터를 로드합니다",
        Some(DropKind::Unsupported) => {
            "지원하지 않는 파일 형식입니다 (CSV·TSV·TXT·JSON·JSONL·NDJSON 데이터 또는 .kiro 블루프린트)"
        }
        None => "여기에 놓으면 파일을 엽니다",
    }
}

/// 사용 중이고 비어 있지 않은 제약을 공백을 정리해 돌려준다 (컴파일 요청과 stale 키가 같이 쓴다).
fn normalized_constraints(goals: &[Goal]) -> Vec<String> {
    goals
        .iter()
        .filter(|g| g.on && !g.text.trim().is_empty())
        .map(|g| g.text.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// 컴파일 요청의 편집 입력 부분(데이터 제외). start_compile과 stale 키가 같은 정규화를 쓴다.
fn compile_inputs(
    target: TargetModel,
    role: &str,
    goals: &[Goal],
    format: OutputFormat,
    fields_text: &str,
) -> CompileRequest {
    CompileRequest {
        target,
        role: role.to_string(),
        constraints: normalized_constraints(goals),
        format,
        fields: pipeline::parse_fields(fields_text),
        data: None,
    }
}

/// 컴파일 결과가 현재 입력과 같은지 비교하는 키. 실제로 보내는 정규화된 요청과 데이터 지문으로 만들므로
/// 빈 제약을 추가하거나 공백·쉼표 간격만 바꿔도 "입력이 바뀜"이 되지 않는다.
fn compile_key(req: &CompileRequest, data: Option<u64>) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        req.target, req.role, req.constraints, req.format, req.fields, data
    )
}

/// 열이 하나로 읽혔는데 그 이름에 탭·세미콜론·쉼표·파이프가 남아 있으면 경고 문구.
/// 프로파일러가 헤더 줄로 구분자(쉼표·세미콜론·탭·파이프)를 고르지만, 판별이 빗나가면 한 열짜리
/// 잘못된 프로파일이 성공처럼 보여 프롬프트에 들어가므로 알린다.
fn delimiter_warning(d: &LoadedData) -> Option<&'static str> {
    let cols = &d.data.columns;
    (cols.len() == 1 && cols[0].name.contains(['\t', ';', ',', '|']))
        .then_some("⚠ 열이 하나로 읽혔습니다: 헤더에 탭·세미콜론·쉼표·파이프가 남아 있으니 구분자를 확인하세요")
}

/// 프롬프트에 들어가는 데이터 내용의 지문 (stale 키용). 같은 파일을 같은 설정으로 다시 읽으면 같은 값이다.
fn data_fingerprint(d: &LoadedData) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    let s = &d.data;
    (
        &d.path,
        d.raw_tokens,
        &s.source_name,
        s.total_rows,
        &s.anomalies,
        &s.sample_rows,
    )
        .hash(&mut h);
    for c in &s.columns {
        (&c.name, &c.dtype, c.null_ratio.to_bits(), &c.summary_stats).hash(&mut h);
    }
    h.finish()
}

/// 마지막으로 열거나 저장한 블루프린트. 현재 설정을 직렬화해 비교하면 저장하지 않은 변경을 알 수 있다.
#[derive(Default, Debug)]
struct SavedState {
    fingerprint: String,
}

impl SavedState {
    fn mark(&mut self, bp: &Blueprint) {
        self.fingerprint = bp.fingerprint();
    }

    fn is_dirty(&self, bp: &Blueprint) -> bool {
        bp.fingerprint() != self.fingerprint
    }
}

/// 창 제목: 저장하지 않은 변경이 있으면 앞에 ●, 블루프린트 파일 이름이 프로젝트 이름과 다르면 괄호로 덧붙인다.
fn window_title(project: &str, dirty: bool, path: Option<&Path>) -> String {
    let mark = if dirty { "● " } else { "" };
    let name = match project.trim() {
        "" => "이름 없음",
        n => n,
    };
    let file = path
        .and_then(Path::file_name)
        .map(|f| f.to_string_lossy())
        .filter(|f| f.as_ref() != name)
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();
    format!("{mark}{name}{file} — Candy-Corn")
}

/// 저장하지 않은 변경이 있을 때 먼저 물어보는 작업.
#[derive(Clone, Debug, PartialEq)]
enum Pending {
    /// 블루프린트 열기 대화상자
    OpenDialog,
    /// 이 블루프린트 파일 열기 (끌어다 놓기)
    OpenPath(PathBuf),
    /// 내장 블루프린트 적용 (PRESETS 번호)
    Preset(usize),
    /// 창 닫기
    Close,
}

impl Pending {
    fn label(&self) -> &'static str {
        match self {
            Pending::OpenDialog | Pending::OpenPath(_) => "블루프린트 열기",
            Pending::Preset(_) => "내장 블루프린트 적용",
            Pending::Close => "창 닫기",
        }
    }
}

/// 블루프린트 저장 결과. 저장 여부 확인 창의 "저장"은 Saved일 때만 이어지는 작업을 한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    /// 저장 대화상자를 닫음
    Cancelled,
    /// 쓰기 실패 (상태 줄에 오류)
    Failed,
}

/// 파일 대화상자는 UI 패스 안에서 열면 프레임이 멈추고 입력이 꼬이므로 프레임 끝에서 실행한다.
#[derive(Clone, Debug)]
enum Dialog {
    PickData,
    /// 다른 이름으로 저장 (항상 위치를 묻는다)
    SaveAs,
    /// Ctrl+S: 지금 블루프린트 파일에 저장 (파일이 없으면 위치를 묻는다)
    QuickSave,
    LoadProject,
    SaveOutput,
    /// 저장에 성공하면 이어서 작업을 한다 (저장 여부 확인 창의 "저장")
    SaveThen(Pending),
}

/// 상단 바 버튼
enum TopAction {
    Compile,
    Undo,
    Redo,
}

const UNDO_HINT: &str = "Ctrl+Z · 대상: 역할, 제약, 출력 포맷·필드, 타겟\n(테마·데이터·샘플 행 수·프로젝트 이름·노드 배치·LLM 실행 옵션은 대상이 아닙니다. 글자 입력 중에는 입력칸 안에서 되돌립니다)";

/// 상단 바를 한 줄에 그리는 데 필요한 너비의 첫 프레임 추정치 (이후에는 지난 프레임에 잰 값을 쓴다).
const TOPBAR_NEED_GUESS: f32 = 1240.0;

/// 가운데 캔버스에 늘 남겨 둘 최소 너비.
const CENTER_MIN_W: f32 = 360.0;

/// 창 너비에 따른 (왼쪽, 오른쪽) 패널 최대 너비. 좁은 창(최소 1000px)에서도 가운데 캔버스가 CENTER_MIN_W 이상
/// 남도록 남는 너비를 45:55로 나누되, 각 패널의 최소 너비(280·320)와 원래 최대 너비(520·700)는 지킨다.
fn side_panel_max(window_w: f32) -> (f32, f32) {
    let spare = (window_w - CENTER_MIN_W).max(0.0);
    ((spare * 0.45).clamp(280.0, 520.0), (spare * 0.55).clamp(320.0, 700.0))
}

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "알 수 없는 오류".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Canvas,
    XRay,
}

pub struct StudioApp {
    project: String,
    target: TargetModel,
    role: String,
    goals: Vec<Goal>,
    next_id: u64,
    new_goal: String,
    format: OutputFormat,
    fields_text: String,
    sample_rows: usize,
    theme_idx: usize,
    tab: Tab,
    canvas: Canvas,

    loaded: Option<LoadedData>,
    /// 화면 데이터의 내용 지문 (stale 키용, loaded가 있을 때만 의미가 있다)
    loaded_fp: u64,
    loading: bool,
    compiling: bool,
    status: String,
    status_err: bool,
    result: Option<CompileResult>,
    /// 결과와 같은 입력을 타겟별로 컴파일한 토큰·비용 비교 (X-Ray 탭)
    compare: Vec<TargetCost>,
    result_key: String,
    dialog: Option<Dialog>,
    load_gen: u64,
    /// 창 위로 끌고 있는 파일의 종류 (Some(None)이면 경로를 알 수 없는 파일)
    hover_drop: Option<Option<DropKind>>,
    steering_rules: Vec<String>,
    /// 마지막으로 적용한 테마(캔버스·헤더·블루프린트 어디서 바꿔도 프레임 시작에 한 번 적용)
    applied_theme: usize,
    /// 마지막으로 열거나 저장한 블루프린트 파일
    blueprint_path: Option<PathBuf>,
    /// 블루프린트 대화상자를 열 폴더 (마지막으로 열거나 저장한 파일의 폴더)
    last_dir: Option<PathBuf>,
    /// 저장 여부를 확인했으니 창을 닫아도 된다
    allow_close: bool,
    /// 데이터 파일 참조 (화면의 데이터와 진행 중·누락된 요청). 블루프린트 저장과 다시 프로파일링에 쓴다.
    data_ref: DataRef,
    compile_gen: u64,
    /// 컴파일 결과가 현재 입력과 다른지 (프레임마다 한 번 계산)
    stale: bool,
    history: History<Snapshot>,
    logo: Option<egui::TextureHandle>,
    llm_models: [String; 3],
    /// 출력 한도·온도·시스템 프롬프트 (캔버스 LLM 노드와 VIII이 같은 값을 편집, 블루프린트 v3에 저장)
    llm_opts: LlmOptions,
    llm: LlmJobs,
    llm_out: Option<LlmRun>,
    llm_tx: Sender<LlmMsg>,
    llm_rx: Receiver<LlmMsg>,
    /// 안개 테마의 움직이는 안개 (앱 설정이라 블루프린트에는 저장하지 않는다)
    fog_on: bool,
    /// 마지막으로 열거나 저장한 블루프린트
    saved: SavedState,
    /// 저장하지 않은 변경이 있는지 (프레임마다 계산)
    dirty: bool,
    /// 저장 여부를 묻고 있는 작업
    confirm: Option<Pending>,
    /// 마지막으로 보낸 창 제목
    title: String,
    /// 상단 바를 한 줄에 그리는 데 필요한 너비 (지난 프레임에 잰 값)
    topbar_need: f32,

    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx_tx: Sender<CompileMsg>,
    ctx_rx: Receiver<CompileMsg>,
}

/// Windows 한글 폰트를 egui 폰트 목록의 뒤쪽 fallback으로 등록한다.
pub fn install_korean_font(ctx: &egui::Context) -> bool {
    const CANDIDATES: [&str; 3] = [
        "C:\\Windows\\Fonts\\malgun.ttf",
        "C:\\Windows\\Fonts\\NanumGothic.ttf",
        "C:\\Windows\\Fonts\\gulim.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "korean".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("korean".to_owned());
    }
    ctx.set_fonts(fonts);
    true
}

impl StudioApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        startup_file: Option<PathBuf>,
        startup_blueprint: Option<PathBuf>,
        theme_arg: Option<usize>,
    ) -> Self {
        // 첫 컴파일이 토크나이저 사전 로드(~85ms)를 기다리지 않게 미리 불러 둔다.
        std::thread::spawn(tokenizer::warm_up);
        let font_ok = install_korean_font(&cc.egui_ctx);
        let theme_idx = theme_arg.unwrap_or(3).min(THEMES.len() - 1); // 기본: 04 프리미엄 느와르 오렌지
        theme::apply(&cc.egui_ctx, &THEMES[theme_idx]);
        let (tx, rx) = channel();
        let (ctx_tx, ctx_rx) = channel();
        let (llm_tx, llm_rx) = channel();
        let logo = eframe::icon_data::from_png_bytes(theme::ICON_PNG).ok().map(|icon| {
            let img = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            cc.egui_ctx.load_texture("app-logo", img, egui::TextureOptions::LINEAR)
        });

        let mut app = StudioApp {
            project: "sales_analysis.kiro".into(),
            target: TargetModel::Claude,
            role: "매출 데이터를 분석해 근본 원인을 찾는 데이터 분석가".into(),
            goals: vec![],
            next_id: 1,
            new_goal: String::new(),
            format: OutputFormat::StrictJson,
            fields_text: pipeline::DEFAULT_FIELDS.into(),
            sample_rows: 8,
            theme_idx,
            tab: Tab::Canvas,
            canvas: Canvas::default(),
            loaded: None,
            loaded_fp: 0,
            loading: false,
            compiling: false,
            status: "데이터를 불러오고 Compile & Run을 누르세요 (Ctrl+Enter)".into(),
            status_err: false,
            result: None,
            compare: vec![],
            result_key: String::new(),
            dialog: None,
            load_gen: 0,
            hover_drop: None,
            steering_rules: vec![],
            applied_theme: theme_idx,
            blueprint_path: None,
            last_dir: None,
            allow_close: false,
            data_ref: DataRef::default(),
            compile_gen: 0,
            stale: false,
            history: History::new(Snapshot {
                target: TargetModel::Claude,
                role: String::new(),
                goals: vec![],
                format: OutputFormat::StrictJson,
                fields_text: String::new(),
            }),
            logo,
            llm_models: Provider::ALL.map(|p| p.default_model().to_string()),
            llm_opts: LlmOptions::default(),
            llm: LlmJobs::default(),
            llm_out: None,
            llm_tx,
            llm_rx,
            fog_on: true,
            saved: SavedState::default(),
            dirty: false,
            confirm: None,
            title: String::new(),
            topbar_need: TOPBAR_NEED_GUESS,
            tx,
            rx,
            ctx_tx,
            ctx_rx,
        };
        for (text, on) in [
            ("결측치 0.2% 제외", true),
            ("이상치 우선 탐지", true),
            ("4M 기준 분류", false),
        ] {
            app.add_goal(text.to_string(), on);
        }
        // 작업 폴더의 steering 규칙을 자동 주입 (UTF-8·CP949 자동 판별)
        if let Some(md) = pipeline::find_steering().and_then(|p| pipeline::read_text_auto(&p).ok()) {
            app.steering_rules = pipeline::parse_steering_rules(&md);
            for r in app.steering_rules.clone() {
                app.add_goal(r, true);
            }
        }
        // 기본 배치를 바로 계산해 두어 첫 프레임 전에도 블루프린트에 노드 위치가 들어간다.
        app.canvas.reset_layout(&app.goals);
        app.history.reset(app.snapshot()); // 시작 상태는 되돌릴 대상이 아니다
        if !font_ok {
            app.status = "한글 폰트(malgun.ttf 등)를 찾지 못해 한글이 깨질 수 있습니다".into();
            app.status_err = true;
        }
        if let Some(bp) = startup_blueprint {
            app.open_blueprint(&bp, &cc.egui_ctx);
            if let Some(t) = theme_arg {
                app.theme_idx = t.min(THEMES.len() - 1); // --theme가 블루프린트보다 우선
            }
        }
        if let Some(p) = startup_file {
            app.open_path(p, &cc.egui_ctx);
        }
        // 시작 상태(방금 연 블루프린트, --theme, 시작 인자로 받은 데이터 포함)를 저장된 상태로 본다.
        app.mark_saved();
        app
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            target: self.target,
            role: self.role.clone(),
            goals: self.goals.clone(),
            format: self.format,
            fields_text: self.fields_text.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot, what: &str) {
        self.target = s.target;
        self.role = s.role;
        self.format = s.format;
        self.fields_text = s.fields_text;
        self.goals = s.goals;
        // 되살린 제약과 새 제약의 id가 겹치지 않게 한다.
        let max_id = self.goals.iter().map(|g| g.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id + 1);
        self.status = what.to_string();
        self.status_err = false;
    }

    fn undo(&mut self, ctx: &egui::Context) {
        match self.history.undo() {
            Some(s) => {
                self.restore(s, "실행 취소");
                clear_text_undo(ctx);
            }
            None => self.status = "되돌릴 작업이 없습니다".into(),
        }
    }

    fn redo(&mut self, ctx: &egui::Context) {
        match self.history.redo() {
            Some(s) => {
                self.restore(s, "다시 실행");
                clear_text_undo(ctx);
            }
            None => self.status = "다시 실행할 작업이 없습니다".into(),
        }
    }

    /// LLM 노드·VIII에 보여 줄 제공자 (컴파일 결과가 있으면 그 타겟, 없으면 현재 타겟).
    fn llm_provider(&self) -> Provider {
        provider_for(self.result.as_ref().map_or(self.target, |r| r.ast.target_model))
    }

    /// 지금 LLM을 실행할 수 있는지 (API 키는 start_llm이 따로 확인한다).
    fn llm_ready(&self) -> bool {
        self.result.is_some() && !self.stale && !self.compiling && !self.llm.running
    }

    /// 컴파일된 프롬프트를 실제 LLM에 보내고 응답을 출력 스키마로 검사한다(워커 스레드).
    fn start_llm(&mut self, ctx: &egui::Context) {
        let Some(r) = &self.result else { return };
        let provider = provider_for(r.ast.target_model);
        let Some(key) = api_key(provider) else {
            self.status = format!("환경 변수 {}가 없습니다", provider.default_key_env());
            self.status_err = true;
            return;
        };
        let model = {
            let m = self.llm_models[provider_index(provider)].trim();
            if m.is_empty() {
                provider.default_model().to_string()
            } else {
                m.to_string()
            }
        };
        let prompt = r.output.clone();
        let format = r.ast.output_schema.format;
        let fields = r.ast.output_schema.required_fields.clone();
        let opts = self.llm_opts.clone().normalized();
        // 제공자 범위로 자른 온도를 보낸다 (저장된 값은 그대로 둔다).
        let (max_tokens, temperature) = (opts.max_tokens, opts.effective_temperature(provider));
        let system = opts.system.trim().to_string();
        let generation = self.llm.start();
        self.llm_out = None;
        self.status = format!("{} · {model} 실행 중... (출력 한도 {max_tokens})", provider.label());
        self.status_err = false;
        let (tx, ctx) = (self.llm_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let req = llm::LlmRequest {
                provider,
                model: &model,
                system: &system,
                prompt: &prompt,
                api_key: &key,
                base_url: "",
                max_tokens,
                temperature,
            };
            let started = Instant::now();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                llm::call(&req).map(|reply| pipeline::check_reply(format, &fields, &reply, max_tokens))
            }))
            .unwrap_or_else(|e| Err(format!("LLM 호출 내부 오류: {}", panic_text(e))));
            let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let _ = tx.send(LlmMsg::Done(generation, LlmRun { out, ms, max_tokens }));
            ctx.request_repaint();
        });
    }

    /// 진행 중인 LLM 실행을 버린다 (응답이 와도 무시한다).
    fn cancel_llm(&mut self) {
        if self.llm.cancel() {
            self.status = "LLM 실행 취소됨".into();
            self.status_err = false;
        }
    }

    /// 출력 한도를 늘려 같은 프롬프트를 다시 보낸다 (잘린 응답의 "한도 2배로 다시 실행").
    fn retry_longer(&mut self, limit: u32, ctx: &egui::Context) {
        self.llm_opts.max_tokens = limit.clamp(blueprint::MIN_LLM_MAX_TOKENS, blueprint::MAX_LLM_MAX_TOKENS);
        if self.llm_ready() {
            self.start_llm(ctx);
        }
    }

    fn add_goal(&mut self, text: String, on: bool) {
        self.goals.push(Goal {
            id: self.next_id,
            text,
            on,
        });
        self.next_id += 1;
    }

    fn palette(&self) -> Palette {
        THEMES[self.theme_idx.min(THEMES.len() - 1)]
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        // 프레임 끝(run_dialog)에서만 호출된다.
        let Some(path) = rfd::FileDialog::new()
            .add_filter("데이터 (CSV·TSV·TXT·JSON·JSONL·NDJSON)", &DATA_EXTS)
            .pick_file()
        else {
            return;
        };
        self.spawn_load(path, ctx);
    }

    /// 끌어다 놓은(또는 시작 인자로 받은) 파일을 확장자에 따라 연다:
    /// .kiro는 블루프린트로, 데이터 파일은 프로파일러로, 그 밖의 파일은 오류로 알린다.
    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) {
        match drop_kind(&path) {
            DropKind::Blueprint => self.request(Pending::OpenPath(path), ctx),
            DropKind::Data => self.spawn_load(path, ctx),
            DropKind::Unsupported => {
                self.status = format!(
                    "지원하지 않는 파일 형식: {} (CSV·TSV·TXT·JSON·JSONL·NDJSON 데이터 또는 .kiro 블루프린트만 열 수 있습니다)",
                    path.display()
                );
                self.status_err = true;
            }
        }
    }

    fn spawn_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.data_ref.requested(path.clone(), self.sample_rows);
        self.loading = true;
        self.status = format!("프로파일링 중: {}", path.display());
        self.status_err = false;
        self.load_gen += 1;
        let (tx, ctx, k, generation) = (self.tx.clone(), ctx.clone(), self.sample_rows, self.load_gen);
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| pipeline::load_data(&path, k))
                .unwrap_or_else(|e| Err(format!("프로파일러 내부 오류: {}", panic_text(e))));
            let _ = tx.send(Msg::Loaded(generation, path, r));
            ctx.request_repaint();
        });
    }

    /// 샘플 행 수가 마지막 프로파일링 요청과 다르면 같은 파일을 다시 프로파일링한다.
    /// 로드가 진행 중이면 그 파일을 대상으로 한다(이전 파일로 되돌아가지 않는다).
    fn reprofile_if_needed(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.data_ref.reprofile_target(self.sample_rows) {
            self.spawn_load(path, ctx);
        }
    }

    /// 컴파일 요청의 편집 입력 부분 (데이터 제외).
    fn request_inputs(&self) -> CompileRequest {
        compile_inputs(self.target, &self.role, &self.goals, self.format, &self.fields_text)
    }

    fn data_fp(&self) -> Option<u64> {
        self.loaded.as_ref().map(|_| self.loaded_fp)
    }

    /// 컴파일 결과가 현재 입력과 같은지 비교하기 위한 키 (start_compile이 보내는 요청과 같은 정규화).
    fn input_key(&self) -> String {
        compile_key(&self.request_inputs(), self.data_fp())
    }

    fn start_compile(&mut self, ctx: &egui::Context) {
        let inputs = self.request_inputs();
        self.result_key = compile_key(&inputs, self.data_fp());
        let req = CompileRequest {
            data: self.loaded.clone(),
            ..inputs
        };
        self.compiling = true;
        self.status = "컴파일 중...".into();
        self.status_err = false;
        self.compile_gen += 1;
        let (tx, ctx, generation) = (self.ctx_tx.clone(), ctx.clone(), self.compile_gen);
        std::thread::spawn(move || {
            let msg = match std::panic::catch_unwind(|| (pipeline::compile(&req), pipeline::compare_targets(&req))) {
                Ok((r, cmp)) => CompileMsg::Done(generation, Box::new(r), cmp),
                Err(e) => CompileMsg::Failed(generation, panic_text(e)),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(Msg::Loaded(generation, path, r)) = self.rx.try_recv() {
            if generation != self.load_gen {
                continue; // 더 최근에 시작한 로드가 있으면 오래된 결과는 버린다
            }
            self.loading = false;
            match r {
                Ok(d) => {
                    self.status = format!("로드 완료 · {} ({} rows)", path.display(), d.data.total_rows);
                    if let Some(w) = delimiter_warning(&d) {
                        self.status = format!("{} · {w}", self.status);
                        self.status_err = true;
                    }
                    self.loaded_fp = data_fingerprint(&d);
                    self.loaded = Some(d);
                    self.data_ref.succeeded();
                }
                Err(e) => {
                    self.status = format!("로드 실패: {e}");
                    self.status_err = true;
                    // 화면에 이전 데이터가 남아 있으면 저장·다시 프로파일링도 그 데이터를 가리키고,
                    // 샘플 행 수도 그 데이터에 맞춘다(폼과 실제 데이터가 어긋나지 않게).
                    if let Some(rows) = self.data_ref.failed()
                        && rows != self.sample_rows
                    {
                        self.sample_rows = rows;
                        self.status.push_str(&format!(" · 샘플 행 수를 {rows}로 되돌림"));
                    }
                }
            }
        }
        while let Ok(msg) = self.ctx_rx.try_recv() {
            let generation = match &msg {
                CompileMsg::Done(g, ..) | CompileMsg::Failed(g, _) => *g,
            };
            if generation != self.compile_gen {
                continue;
            }
            self.compiling = false;
            match msg {
                CompileMsg::Done(_, r, cmp) => {
                    self.status = "컴파일 완료".into();
                    self.status_err = false;
                    self.result = Some(*r);
                    self.compare = cmp;
                    // 이전 프롬프트로 받은(또는 받는 중인) LLM 응답은 더 이상 맞지 않는다.
                    self.llm_out = None;
                    self.llm.cancel();
                }
                CompileMsg::Failed(_, e) => {
                    self.status = format!("컴파일 실패: {e}");
                    self.status_err = true;
                    self.result_key.clear(); // 남아 있는 이전 결과를 "입력이 바뀜"으로 표시
                }
            }
        }
        while let Ok(LlmMsg::Done(generation, run)) = self.llm_rx.try_recv() {
            if !self.llm.accept(generation) {
                continue; // 취소했거나 새 실행·재컴파일로 버린 응답
            }
            self.status = llm_status(&run.out);
            self.status_err = !matches!(&run.out, Ok(c) if c.ok);
            self.llm_out = Some(run);
        }
    }

    // ---------- 프로젝트 / 결과 파일 ----------

    /// 현재 화면의 전체 설정을 블루프린트로 만든다.
    fn to_blueprint(&self, save_to: Option<&Path>) -> Blueprint {
        let positions = NodeKey::FIXED
            .iter()
            .filter_map(|k| Some((k.fixed_name()?.to_string(), self.canvas.pos_of(*k).map(|p| [p.x, p.y])?)))
            .collect();
        // 데이터가 블루프린트와 같은 폴더(또는 하위)에 있으면 상대 경로로 저장해 폴더째 옮겨도 동작하게 한다.
        let data_path = self.data_ref.source().map(|d| match save_to {
            Some(bp_file) => Blueprint::portable_data_path(d, bp_file),
            None => d.display().to_string(),
        });
        Blueprint {
            version: blueprint::VERSION,
            project: self.project.clone(),
            target: self.target,
            role: self.role.clone(),
            goals: self
                .goals
                .iter()
                .map(|g| GoalSpec {
                    text: g.text.clone(),
                    on: g.on,
                    pos: self.canvas.pos_of(NodeKey::Goal(g.id)).map(|p| [p.x, p.y]),
                })
                .collect(),
            format: self.format,
            fields: self.fields_text.clone(),
            sample_rows: self.sample_rows,
            theme: self.theme_idx,
            data_path,
            positions,
            llm_models: Provider::ALL
                .iter()
                .copied()
                .zip(self.llm_models.iter().cloned())
                .collect(),
            llm_options: self.llm_opts.clone(),
        }
    }

    /// 지금 설정을 "저장된 상태"로 기억한다 (열기·저장 직후). 이후 달라지면 ● 표시가 붙는다.
    fn mark_saved(&mut self) {
        self.saved.mark(&self.to_blueprint(None));
        self.dirty = false;
    }

    /// 블루프린트의 설정을 모두 적용한다. keep_data면 현재 데이터를 유지하고(샘플 행 수가 바뀌면 다시 프로파일링),
    /// 아니면 블루프린트의 데이터 경로를 불러온다. 경고 문구가 있으면 돌려준다.
    fn apply_blueprint(
        &mut self,
        bp: Blueprint,
        source: Option<&Path>,
        keep_data: bool,
        ctx: &egui::Context,
    ) -> Option<String> {
        let bp = bp.normalized();
        self.project = bp.project.clone();
        self.target = bp.target;
        self.role = bp.role.clone();
        self.format = bp.format;
        self.fields_text = bp.fields.clone();
        self.sample_rows = bp.sample_rows;
        self.theme_idx = bp.theme;
        self.goals.clear();
        let mut saved_pos = Vec::new();
        for g in &bp.goals {
            self.add_goal(g.text.clone(), g.on);
            if let (Some(p), Some(last)) = (g.pos, self.goals.last()) {
                saved_pos.push((NodeKey::Goal(last.id), p));
            }
        }
        saved_pos.extend(
            bp.positions
                .iter()
                .filter_map(|(name, p)| Some((NodeKey::from_fixed_name(name)?, *p))),
        );
        // 기본 배치를 먼저 계산하고 저장된 위치를 덮는다 (위치가 없는 노드는 원점이 아니라 기본 자리에 남는다).
        self.canvas.load_layout(&self.goals, saved_pos);
        for (i, p) in Provider::ALL.iter().enumerate() {
            if let Some(m) = bp.llm_model(*p) {
                self.llm_models[i] = m.to_string();
            }
        }
        self.llm_opts = bp.llm_options.clone();
        self.canvas.request_fit();
        self.result = None;
        self.compare.clear();
        self.result_key.clear();
        self.llm_out = None;
        self.llm.cancel();
        clear_text_undo(ctx);
        // 진행 중인 컴파일 결과는 이 블루프린트와 맞지 않으므로 버린다.
        self.compile_gen += 1;
        self.compiling = false;
        if keep_data {
            self.reprofile_if_needed(ctx);
            return None;
        }
        // 이전 데이터와 진행 중인 로드를 버린다.
        self.loaded = None;
        self.loading = false;
        self.load_gen += 1;
        self.data_ref = DataRef::default();
        match bp.data_path_from(source) {
            Some(dp) if dp.is_file() => {
                self.spawn_load(dp, ctx);
                None
            }
            Some(dp) => {
                let warning = format!("데이터 파일을 찾을 수 없음: {}", dp.display());
                self.data_ref.missing(dp, self.sample_rows); // 다시 저장해도 참조가 사라지지 않게
                Some(warning)
            }
            None => None,
        }
    }

    fn with_steering(&self, bp: Blueprint) -> Blueprint {
        merge_steering(bp, &self.steering_rules)
    }

    fn open_blueprint(&mut self, path: &Path, ctx: &egui::Context) {
        let bp = match Blueprint::load(path) {
            Ok(b) => self.with_steering(b),
            Err(e) => {
                self.status = format!("블루프린트 열기 실패: {e}");
                self.status_err = true;
                return;
            }
        };
        let warning = self.apply_blueprint(bp, Some(path), false, ctx);
        self.blueprint_path = Some(path.to_path_buf());
        self.last_dir = path.parent().map(Path::to_path_buf);
        self.history.reset(self.snapshot()); // 다른 블루프린트로 되돌아가지 않게 기록을 비운다
        self.mark_saved();
        match warning {
            Some(w) => {
                self.status = format!("블루프린트 열림 · {w}");
                self.status_err = true;
            }
            None => {
                self.status = format!("블루프린트 열림: {}", path.display());
                self.status_err = false;
            }
        }
    }

    /// 블루프린트를 path에 저장한다. 성공하면 그 파일을 지금 블루프린트로 삼고 저장된 상태로 표시한다.
    fn save_to(&mut self, path: &Path) -> SaveOutcome {
        match self.to_blueprint(Some(path)).save(path) {
            Ok(()) => {
                self.status = format!("블루프린트 저장: {}", path.display());
                self.status_err = false;
                self.last_dir = path.parent().map(Path::to_path_buf);
                self.blueprint_path = Some(path.to_path_buf());
                self.mark_saved();
                SaveOutcome::Saved
            }
            Err(e) => {
                self.status = e;
                self.status_err = true;
                SaveOutcome::Failed
            }
        }
    }

    /// 다른 이름으로 저장 (프레임 끝에서만 호출). 대화상자는 마지막으로 열거나 저장한 폴더에서 연다.
    fn save_as(&mut self) -> SaveOutcome {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Candy-Corn 블루프린트", &["kiro"])
            .set_file_name(self.project.clone());
        if let Some(dir) = &self.last_dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else {
            return SaveOutcome::Cancelled;
        };
        self.save_to(&path)
    }

    /// Ctrl+S: 지금 블루프린트 파일에 저장하고, 아직 파일이 없으면 위치를 묻는다 (프레임 끝에서만 호출).
    fn quick_save(&mut self) -> SaveOutcome {
        match self.blueprint_path.clone() {
            Some(path) => self.save_to(&path),
            None => self.save_as(),
        }
    }

    fn load_project(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new().add_filter("Candy-Corn 블루프린트", &["kiro"]);
        if let Some(dir) = &self.last_dir {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        self.open_blueprint(&path, ctx);
    }

    fn save_output(&mut self) {
        let Some(r) = &self.result else {
            self.status = "저장할 결과가 없습니다. 먼저 컴파일하세요".into();
            self.status_err = true;
            return;
        };
        let ext = match r.ast.target_model {
            TargetModel::Claude => "xml",
            TargetModel::Gpt => "md",
            TargetModel::Gemini => "txt",
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("프롬프트", &[ext, "txt"])
            .set_file_name(format!("prompt.{ext}"))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, &r.output) {
            Ok(()) => {
                self.status = format!("결과 저장: {}", path.display());
                self.status_err = false;
            }
            Err(e) => {
                self.status = format!("저장 실패: {e}");
                self.status_err = true;
            }
        }
    }

    /// 내장 블루프린트 적용. 현재 데이터·LLM 모델·실행 옵션은 유지하고, 편집 내용은 실행 취소할 수 있다.
    /// 템플릿에서 새로 시작하는 것이므로 지금 파일과의 연결은 끊는다 (Ctrl+S가 이전 .kiro를 덮어쓰지 않고 위치를 묻는다).
    fn apply_preset(&mut self, i: usize, ctx: &egui::Context) {
        let Some(preset) = PRESETS.get(i) else { return };
        let mut bp = self.with_steering(preset.to_blueprint());
        bp.llm_models = Provider::ALL
            .iter()
            .copied()
            .zip(self.llm_models.iter().cloned())
            .collect();
        bp.llm_options = self.llm_opts.clone();
        self.apply_blueprint(bp, None, true, ctx);
        self.blueprint_path = None;
        self.status = format!("블루프린트 적용: {} ({})", preset.name, preset.summary);
        self.status_err = false;
    }

    /// 블루프린트 열기·내장 블루프린트 적용·창 닫기. 저장하지 않은 변경이 있으면 먼저 묻는다.
    fn request(&mut self, p: Pending, ctx: &egui::Context) {
        if self.saved.is_dirty(&self.to_blueprint(None)) {
            self.confirm = Some(p);
        } else {
            self.perform(p, ctx);
        }
    }

    fn perform(&mut self, p: Pending, ctx: &egui::Context) {
        match p {
            Pending::OpenDialog => {
                // 대화상자는 프레임 끝에서 연다 (이미 프레임 끝이면 다음 프레임).
                self.dialog = Some(Dialog::LoadProject);
                ctx.request_repaint();
            }
            Pending::OpenPath(path) => self.open_blueprint(&path, ctx),
            Pending::Preset(i) => self.apply_preset(i, ctx),
            Pending::Close => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn remove_goal(&mut self, id: u64) {
        self.goals.retain(|g| g.id != id);
        self.canvas.forget(NodeKey::Goal(id));
    }

    fn run_dialog(&mut self, d: Dialog, ctx: &egui::Context) {
        match d {
            Dialog::PickData => self.pick_file(ctx),
            Dialog::SaveAs => {
                self.save_as();
            }
            Dialog::QuickSave => {
                self.quick_save();
            }
            Dialog::LoadProject => self.load_project(ctx),
            Dialog::SaveOutput => self.save_output(),
            Dialog::SaveThen(p) => match self.quick_save() {
                SaveOutcome::Saved => self.perform(p, ctx),
                // 저장하지 않았으면 이어지는 작업도 하지 않는다(변경을 잃지 않게).
                SaveOutcome::Cancelled => {
                    self.status = format!("저장을 취소해서 작업({})을 하지 않았습니다", p.label());
                    self.status_err = false;
                }
                SaveOutcome::Failed => self
                    .status
                    .push_str(&format!(" · 작업({})을 하지 않았습니다", p.label())),
            },
        }
    }

    fn handle(&mut self, actions: Vec<Action>, ctx: &egui::Context) {
        for a in actions {
            match a {
                Action::PickFile => self.dialog = Some(Dialog::PickData),
                Action::Compile => {
                    if !self.compiling {
                        self.start_compile(ctx)
                    }
                }
                Action::RemoveGoal(id) => self.remove_goal(id),
                Action::AddGoal(t) => self.add_goal(t, true),
                Action::Preset(i) => self.request(Pending::Preset(i), ctx),
                Action::RunLlm => {
                    if self.llm_ready() {
                        self.start_llm(ctx)
                    }
                }
                Action::CancelLlm => self.cancel_llm(),
                Action::RetryLonger(limit) => self.retry_longer(limit, ctx),
                Action::ReloadSample => self.reprofile_if_needed(ctx),
                Action::SaveProject => self.dialog = Some(Dialog::SaveAs),
                Action::QuickSave => self.dialog = Some(Dialog::QuickSave),
                Action::LoadProject => self.request(Pending::OpenDialog, ctx),
                Action::SaveOutput => self.dialog = Some(Dialog::SaveOutput),
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = window_title(&self.project, self.dirty, self.blueprint_path.as_deref());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    /// 끌어다 놓은 파일 처리와 끌고 있는 파일 안내.
    fn handle_drops(&mut self, ctx: &egui::Context, modal_open: bool) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        // 로드 중에 놓아도 새 로드가 이긴다(load_gen).
        if let Some(path) = pick_drop(&dropped).cloned() {
            if modal_open {
                self.status = "먼저 저장 여부를 선택하세요".into();
                self.status_err = true;
            } else {
                self.open_path(path, ctx);
            }
        }
        self.hover_drop = ctx.input(|i| i.raw.hovered_files.first().map(|f| f.path.as_deref().map(drop_kind)));
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = egui::Modifiers::COMMAND;
        let shift = egui::Modifiers::SHIFT;
        if !ctx.text_edit_focused() {
            // Ctrl+Shift+Z를 먼저 검사한다(Ctrl+Z 패턴은 Shift가 눌려도 일치한다).
            if ctx.input_mut(|i| i.consume_key(cmd | shift, egui::Key::Z) || i.consume_key(cmd, egui::Key::Y)) {
                self.redo(ctx);
            } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Z)) {
                self.undo(ctx);
            }
        }
        // Ctrl+Enter로 컴파일
        if !self.compiling && ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Enter)) {
            self.start_compile(ctx);
        }
        // Ctrl+Shift+S: 다른 이름으로 저장, Ctrl+S: 지금 파일에 저장 (입력칸에 있어도 동작한다)
        if ctx.input_mut(|i| i.consume_key(cmd | shift, egui::Key::S)) {
            self.dialog = Some(Dialog::SaveAs);
        } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::S)) {
            self.dialog = Some(Dialog::QuickSave);
        }
        // Ctrl+O: 블루프린트 열기 (저장하지 않은 변경이 있으면 먼저 묻는다)
        if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::O)) {
            self.request(Pending::OpenDialog, ctx);
        }
    }

    /// 저장하지 않은 변경이 있을 때 다른 블루프린트를 열거나 내장 블루프린트를 적용하기 전에 묻는다.
    fn confirm_modal(&mut self, ctx: &egui::Context, p: &Palette) {
        enum Choice {
            Save,
            Discard,
            Cancel,
        }
        let Some(pending) = self.confirm.clone() else { return };
        let question = match &pending {
            Pending::Preset(i) => format!(
                "내장 블루프린트 「{}」 적용 전에 현재 변경 사항을 저장할까요?",
                PRESETS.get(*i).map_or("", |x| x.name)
            ),
            Pending::OpenDialog | Pending::OpenPath(_) => {
                "다른 블루프린트를 열기 전에 현재 변경 사항을 저장할까요?".to_string()
            }
            Pending::Close => "창을 닫기 전에 현재 변경 사항을 저장할까요?".to_string(),
        };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::unique("unsaved-changes")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.label(RichText::new("저장하지 않은 변경 사항").strong().size(16.0).color(p.hi));
            ui.add_space(4.0);
            ui.label(format!("「{}」에 저장하지 않은 변경 사항이 있습니다.", self.project));
            ui.label(question);
            if let Pending::OpenPath(path) = &pending {
                ui.label(RichText::new(path.display().to_string()).small().color(p.sub));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, p, "저장", true).clicked() {
                    choice = Some(Choice::Save);
                }
                if ui.button("저장 안 함").clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.button("취소").clicked() {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(Choice::Cancel); // Esc·바깥 클릭
        }
        match choice {
            Some(Choice::Save) => {
                self.confirm = None;
                self.dialog = Some(Dialog::SaveThen(pending));
            }
            Some(Choice::Discard) => {
                self.confirm = None;
                self.perform(pending, ctx);
            }
            Some(Choice::Cancel) => {
                self.confirm = None;
                self.status = "취소했습니다".into();
                self.status_err = false;
            }
            None => {}
        }
    }

    // ---------- 패널 ----------

    /// 상단 바. 한 줄에 다 들어가지 않으면(최소 창 너비 1000px 등) 타겟·테마를 둘째 줄로 내린다.
    /// 한 줄에 필요한 너비는 지난 프레임에 잰 각 묶음의 너비로 정하므로 글꼴·테마 이름이 달라도 맞는다.
    fn top_bar(&mut self, ui: &mut egui::Ui, p: &Palette, ctx: &egui::Context) {
        let compact = ui.available_width() < self.topbar_need;
        let gap = ui.spacing().item_spacing.x;
        let mut brand_w = 0.0;
        let mut sel_w = 0.0;
        let can_undo = self.history.can_undo();
        let can_redo = self.history.can_redo();
        let compiling = self.compiling;
        let (left_w, (right_w, act)) = egui::Sides::new().height(46.0).show(
            ui,
            |ui| {
                let x0 = ui.cursor().left();
                brand(ui, self.logo.as_ref(), p);
                brand_w = ui.cursor().left() - x0;
                ui.add_space(16.0);
                project_field(ui, &mut self.project, self.dirty, self.blueprint_path.as_deref(), p);
                if !compact {
                    sel_w = selectors(ui, &mut self.target, &mut self.theme_idx);
                }
                ui.min_rect().width()
            },
            |ui| top_actions(ui, p, compiling, can_undo, can_redo),
        );
        if compact {
            ui.horizontal(|ui| {
                ui.add_space(brand_w + 16.0);
                sel_w = selectors(ui, &mut self.target, &mut self.theme_idx);
            });
        }
        let one_row = if compact { left_w + gap + sel_w } else { left_w };
        self.topbar_need = one_row + gap + right_w + 24.0;
        match act {
            Some(TopAction::Compile) => {
                if !self.compiling {
                    self.start_compile(ctx);
                }
            }
            Some(TopAction::Undo) => self.undo(ctx),
            Some(TopAction::Redo) => self.redo(ctx),
            None => {}
        }
    }

    fn footer(&self, ui: &mut egui::Ui, p: &Palette) {
        egui::Sides::new().shrink_left().truncate().show(
            ui,
            |ui| match self.hover_drop {
                Some(kind) => {
                    let color = if kind == Some(DropKind::Unsupported) {
                        theme::ERR
                    } else {
                        p.hi
                    };
                    ui.label(RichText::new(drop_hint(kind)).small().color(color));
                }
                None => {
                    let color = if self.status_err { theme::ERR } else { p.sub };
                    let r = ui.label(RichText::new(short(&self.status, 200)).small().color(color));
                    if self.status.chars().count() > 40 {
                        r.on_hover_text(&self.status);
                    }
                }
            },
            // 오른쪽을 먼저 그리므로(긴 상태 문구는 왼쪽에서 잘린다) "입력이 바뀜" 경고는 늘 보인다.
            |ui| {
                ui.label(
                    RichText::new("Ctrl+Enter 컴파일 · Ctrl+S 저장 · Ctrl+O 열기 · 파일 끌어다 놓기")
                        .small()
                        .color(p.sub),
                );
                if self.stale && !self.compiling {
                    ui.label(
                        RichText::new("● 입력이 바뀜: 다시 컴파일하세요")
                            .small()
                            .color(p.warn()),
                    );
                }
            },
        );
    }

    fn left_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        theme::section_title(ui, p, "I", "데이터 소스");
        let mut reload = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.loading, egui::Button::new("📂 CSV/JSON 로드"))
                .on_hover_text("CSV·TSV·TXT·JSON·JSONL·NDJSON")
                .clicked()
            {
                self.dialog = Some(Dialog::PickData);
            }
            if self.loading {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label("샘플 행 수");
            let r = ui.add(egui::DragValue::new(&mut self.sample_rows).range(0..=blueprint::MAX_SAMPLE_ROWS));
            reload = r.drag_stopped() || r.lost_focus();
        });
        if reload {
            self.reprofile_if_needed(ui.ctx());
        }
        match &self.loaded {
            Some(d) => {
                ui.label(format!(
                    "{}  ·  {} rows  ·  {} columns",
                    d.data.source_name,
                    d.data.total_rows,
                    d.data.columns.len()
                ))
                .on_hover_text(d.path.display().to_string());
            }
            None => {
                ui.label(RichText::new("로드된 데이터 없음 (파일을 창에 끌어다 놓아도 됩니다)").color(p.sub));
            }
        }

        ui.add_space(10.0);
        theme::section_title(ui, p, "II", "분석 목표 & 제약");
        ui.label(RichText::new("역할").color(p.sub));
        ui.add(
            egui::TextEdit::multiline(&mut self.role)
                .desired_rows(3)
                .desired_width(ui.available_width()),
        );
        let row_w = (ui.available_width() - 70.0).max(80.0);
        let mut remove = None;
        for g in self.goals.iter_mut() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut g.on, "");
                ui.add(
                    egui::TextEdit::multiline(&mut g.text)
                        .id_salt(("goal-form", g.id))
                        .desired_rows(1)
                        .desired_width(row_w),
                );
                if ui.small_button("×").on_hover_text("이 제약 삭제").clicked() {
                    remove = Some(g.id);
                }
            });
        }
        if let Some(id) = remove {
            self.remove_goal(id);
        }
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.new_goal)
                    .hint_text("새 제약 추가 (Enter)")
                    .desired_width(row_w),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("＋").clicked() || enter) && !self.new_goal.trim().is_empty() {
                let t = self.new_goal.trim().to_string();
                self.add_goal(t, true);
                self.new_goal.clear();
            }
        });

        ui.add_space(10.0);
        theme::section_title(ui, p, "III", "출력 포맷 강제");
        ui.radio_value(&mut self.format, OutputFormat::StrictJson, "Strict JSON");
        ui.radio_value(&mut self.format, OutputFormat::MarkdownTable, "Markdown Table");
        ui.label(RichText::new("필수 필드 (쉼표 구분)").color(p.sub));
        ui.add(egui::TextEdit::singleline(&mut self.fields_text).desired_width(ui.available_width()));
        if pipeline::parse_fields(&self.fields_text).is_empty() {
            ui.colored_label(theme::ERR, "필수 필드가 비어 있습니다");
        }
    }

    fn center_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Canvas, RichText::new("IV · 캔버스").size(15.0));
            ui.selectable_value(&mut self.tab, Tab::XRay, RichText::new("Multi-AST X-Ray").size(15.0));
        });
        ui.add_space(2.0);
        match self.tab {
            Tab::Canvas => {
                let stale = self.stale;
                let provider = self.llm_provider();
                let verdict = self.llm_out.as_ref().map(|run| llm_verdict(&run.out));
                let prompt_tokens = self.result.as_ref().map_or(0, |r| r.comp_tokens);
                let details = self.llm_out.as_ref().map(|run| run_line(run, prompt_tokens));
                let retry = self.llm_out.as_ref().and_then(retry_limit);
                let has_key = api_key(provider).is_some();
                let fog_theme = self.palette().fog;
                let dirty = self.dirty;
                let actions = {
                    let mut model = Model {
                        role: &mut self.role,
                        goals: &mut self.goals,
                        format: &mut self.format,
                        fields_text: &mut self.fields_text,
                        target: &mut self.target,
                        loaded: self.loaded.as_ref(),
                        result: self.result.as_ref(),
                        loading: self.loading,
                        compiling: self.compiling,
                        stale,
                        project: &mut self.project,
                        dirty,
                        theme_idx: &mut self.theme_idx,
                        fog: &mut self.fog_on,
                        fog_theme,
                        sample_rows: &mut self.sample_rows,
                        llm: LlmView {
                            provider: provider.label(),
                            key_env: provider.default_key_env(),
                            model: &mut self.llm_models[provider_index(provider)],
                            model_hint: provider.default_model(),
                            has_key,
                            running: self.llm.running,
                            verdict,
                            opts: &mut self.llm_opts,
                            temp_max: blueprint::temperature_max(provider),
                            details,
                            retry_limit: retry,
                        },
                    };
                    self.canvas.show(ui, &mut model, p)
                };
                let ctx = ui.ctx().clone();
                self.handle(actions, &ctx);
            }
            Tab::XRay => self.xray(ui, p),
        }
    }

    fn xray(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(r) = &self.result else {
            ui.label(RichText::new("1) 데이터(CSV/JSON)를 로드하거나 파일을 창에 끌어다 놓기").color(p.sub));
            ui.label(RichText::new("2) 제약을 체크하고 Compile & Run (Ctrl+Enter)").color(p.sub));
            return;
        };
        let ast = &r.ast;
        // 제목에 바뀌는 값(행 수, Lint 상태, 형식)이 들어가므로 열림 상태는 고정 id_salt로 기억한다.
        egui::ScrollArea::vertical()
            .id_salt("ast")
            .max_height(ui.available_height() * 0.6)
            .show(ui, |ui| {
                egui::CollapsingHeader::new("▼ RootPromptAST")
                    .id_salt("xray-root")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!("target: {}", ast.target_model.label()));
                        egui::CollapsingHeader::new(format!("RoleNode ({})", ast.role.name))
                            .id_salt("xray-role")
                            .show(ui, |ui| {
                                ui.label(&ast.role.instruction);
                            });
                        match &ast.data_sub_ast {
                            Some(d) => {
                                egui::CollapsingHeader::new(format!("DataSubAST (Compressed, {} rows)", d.total_rows))
                                    .id_salt("xray-data")
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for c in &d.columns {
                                            ui.label(format!(
                                                "{} : {} | null {:.1}% | {}",
                                                c.name,
                                                c.dtype,
                                                c.null_ratio * 100.0,
                                                c.summary_stats
                                            ));
                                        }
                                        if !d.anomalies.is_empty() {
                                            ui.colored_label(p.warn(), "Outliers");
                                            for a in &d.anomalies {
                                                ui.label(format!("  • {a}"));
                                            }
                                        }
                                    });
                            }
                            None => {
                                ui.label(RichText::new("DataSubAST: (데이터 없음)").color(p.sub));
                            }
                        }
                        let lint_ok = !ast.lint_warnings.iter().any(|d| d.severity != "INFO");
                        egui::CollapsingHeader::new(format!(
                            "ConstraintSubAST ({})",
                            if lint_ok { "Lint OK" } else { "Lint 경고" }
                        ))
                        .id_salt("xray-constraints")
                        .default_open(true)
                        .show(ui, |ui| {
                            for c in &ast.constraints {
                                constraint_row(ui, c);
                            }
                        });
                        egui::CollapsingHeader::new(format!(
                            "OutputSchemaAST ({})",
                            match ast.output_schema.format {
                                OutputFormat::StrictJson => "JSON",
                                OutputFormat::MarkdownTable => "Table",
                            }
                        ))
                        .id_salt("xray-schema")
                        .show(ui, |ui| {
                            ui.monospace(&ast.output_schema.raw_schema);
                        });
                    });
            });
        if !self.compare.is_empty() {
            ui.separator();
            compare_grid(ui, p, &self.compare, ast.target_model);
        }
        ui.separator();
        theme::section_title(ui, p, "V", "실시간 린트 로그");
        egui::ScrollArea::vertical().id_salt("lint").show(ui, |ui| {
            for d in &ast.lint_warnings {
                let (icon, color) = match d.severity.as_str() {
                    "INFO" => ("✔", theme::OK),
                    "WARN" => ("⚠", p.warn()),
                    _ => ("✖", theme::ERR),
                };
                ui.colored_label(color, format!("{icon} {}", d.message));
            }
        });
    }

    fn right_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        theme::section_title(ui, p, "VI", "토큰 절감률");
        let Some(r) = &self.result else {
            ui.label(RichText::new("컴파일하면 토큰 절감률과 최종 프롬프트가 여기에 표시됩니다.").color(p.sub));
            return;
        };
        let good = r.saving_pct >= 0.0;
        ui.label(
            RichText::new(format!(
                "{:+.1}%",
                if r.saving_pct == 0.0 { 0.0 } else { -r.saving_pct }
            ))
            .size(44.0)
            .color(if good { p.hi } else { theme::ERR }),
        );
        let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
        ui.painter().rect_filled(bar, 3.0, p.line.gamma_multiply(0.5));
        let fill = egui::Rect::from_min_size(
            bar.min,
            egui::vec2(
                bar.width() * (r.saving_pct / 100.0).clamp(0.0, 1.0) as f32,
                bar.height(),
            ),
        );
        theme::paint_h_gradient(ui.painter(), fill, p.hi, p.accent);
        ui.label(format!("원본 {} → 압축 {} tokens", r.orig_tokens, r.comp_tokens));
        ui.label(
            RichText::new(format!("절감 ${:.4}  ·  o200k_base 근사, 단가는 추정치", r.saved_usd))
                .small()
                .color(p.sub),
        );

        ui.add_space(8.0);
        theme::section_title(ui, p, "VII", "최종 프롬프트 출력");
        ui.horizontal(|ui| {
            if ui.button("복사").clicked() {
                ui.ctx().copy_text(r.output.clone());
            }
            if ui.button("파일로 저장").clicked() {
                self.dialog = Some(Dialog::SaveOutput);
            }
        });
        let h = ui.available_height() * 0.55;
        egui::ScrollArea::both().id_salt("out").max_height(h).show(ui, |ui| {
            egui::Frame::new()
                .fill(p.panel2)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(&r.output).monospace().color(p.sub)).selectable(true));
                });
        });
        ui.add_space(6.0);
        self.llm_section(ui, p);
    }

    fn llm_section(&mut self, ui: &mut egui::Ui, p: &Palette) {
        enum Act {
            Run,
            Cancel,
            Retry(u32),
        }
        let Some((target, preview, has_data, prompt_tokens)) = self.result.as_ref().map(|r| {
            (
                r.ast.target_model,
                r.preview.clone(),
                r.ast.data_sub_ast.is_some(),
                r.comp_tokens,
            )
        }) else {
            return;
        };
        theme::section_title(ui, p, "VIII", "LLM 실행 결과");
        let provider = provider_for(target);
        let has_key = api_key(provider).is_some();
        let stale = self.stale;
        let running = self.llm.running;
        let ready = has_key && self.llm_ready();
        let mut act = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(provider.label());
            ui.add(
                egui::TextEdit::singleline(&mut self.llm_models[provider_index(provider)])
                    .desired_width(150.0)
                    .hint_text(provider.default_model()),
            );
            let label = if running { "실행 중" } else { "▶ 실제 실행" };
            let btn = theme::primary_button(ui, p, label, ready);
            // 키가 없을 때만 안내한다(실행 중 비활성 상태에는 붙이지 않는다).
            let btn = if has_key {
                btn
            } else {
                btn.on_disabled_hover_text(format!(
                    "환경 변수 {}에 API 키를 설정한 뒤 앱을 다시 시작하세요",
                    provider.default_key_env()
                ))
            };
            if btn.clicked() {
                act = Some(Act::Run);
            }
            if running {
                ui.spinner();
                if ui
                    .button("■ 취소")
                    .on_hover_text("응답을 기다리지 않고 버립니다")
                    .clicked()
                {
                    act = Some(Act::Cancel);
                }
            }
        });
        // 제목에 현재 값 요약이 붙어 바뀌므로 열림 상태는 고정 id로 기억한다.
        let temp_max = blueprint::temperature_max(provider);
        egui::CollapsingHeader::new(format!(
            "실행 옵션 · {}",
            studio_canvas::options_summary(&self.llm_opts, temp_max)
        ))
        .id_salt("viii-llm-opts")
        .show(ui, |ui| {
            studio_canvas::llm_options_ui(ui, &mut self.llm_opts, temp_max, "viii", p)
        });
        let with_system = if self.llm_opts.system.trim().is_empty() {
            ""
        } else {
            " (시스템 프롬프트 포함)"
        };
        let note = match (has_key && !stale, has_data) {
            _ if has_key && stale => "입력이 바뀌었습니다. 다시 컴파일한 뒤 실행하세요".to_string(),
            (true, true) => format!(
                "실행하면 최종 출력 전체(파일 이름, 컬럼 통계, 이상치, 샘플 행 포함){with_system}가 {}로 전송됩니다",
                provider.label()
            ),
            (true, false) => format!(
                "실행하면 최종 출력 프롬프트{with_system}가 {}로 전송됩니다",
                provider.label()
            ),
            (false, _) => format!(
                "{} 환경 변수가 없어 스키마 골격(모의 응답)만 보여 줍니다. 설정 후 앱을 다시 시작하세요",
                provider.default_key_env()
            ),
        };
        ui.label(RichText::new(note).small().color(p.sub));
        egui::ScrollArea::vertical()
            .id_salt("preview")
            .show(ui, |ui| match &self.llm_out {
                None => {
                    ui.add(egui::Label::new(RichText::new(&preview).monospace().color(p.sub)).selectable(true));
                }
                Some(run) => {
                    let c = match &run.out {
                        Ok(c) => c,
                        Err(e) => {
                            ui.colored_label(theme::ERR, e);
                            ui.label(RichText::new(run_line(run, prompt_tokens)).small().color(p.sub));
                            return;
                        }
                    };
                    for (tone, line) in check_lines(c) {
                        ui.colored_label(tone_color(tone, p), line);
                    }
                    ui.label(
                        RichText::new(format!(
                            "종료 이유: {}",
                            c.stop_reason.as_deref().unwrap_or("(응답에 없음)")
                        ))
                        .small()
                        .color(p.sub),
                    );
                    ui.label(RichText::new(run_line(run, prompt_tokens)).small().color(p.sub));
                    let empty = c.pretty.trim().is_empty();
                    ui.horizontal_wrapped(|ui| {
                        if let Some(limit) = retry_limit(run)
                            && ui
                                .add_enabled(ready, egui::Button::new(format!("한도 2배({limit})로 다시 실행")))
                                .on_hover_text("출력 토큰 한도를 두 배로 늘려 같은 프롬프트를 다시 보냅니다")
                                .clicked()
                        {
                            act = Some(Act::Retry(limit));
                        }
                        if ui.add_enabled(!empty, egui::Button::new("응답 복사").small()).clicked() {
                            ui.ctx().copy_text(c.pretty.clone());
                        }
                    });
                    if empty {
                        ui.label(RichText::new("(본문 없음)").monospace().color(p.sub));
                    } else {
                        ui.add(egui::Label::new(RichText::new(&c.pretty).monospace().color(p.sub)).selectable(true));
                    }
                }
            });
        let ctx = ui.ctx().clone();
        match act {
            Some(Act::Run) => self.start_llm(&ctx),
            Some(Act::Cancel) => self.cancel_llm(),
            Some(Act::Retry(limit)) => self.retry_longer(limit, &ctx),
            None => {}
        }
    }
}

/// 상단 바 왼쪽 로고와 이름.
fn brand(ui: &mut egui::Ui, logo: Option<&egui::TextureHandle>, p: &Palette) {
    match logo {
        Some(tex) => {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), [38.0, 38.0])).corner_radius(9));
        }
        None => {
            let (logo, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
            theme::pumpkin_logo(ui.painter(), logo.center(), 38.0, p);
        }
    }
    ui.vertical(|ui| {
        ui.label(RichText::new("CANDY-CORN").size(20.0).strong().color(p.hi));
        ui.label(RichText::new(p.tag).size(10.0).color(p.accent));
    });
}

/// 프로젝트 이름과 저장하지 않은 변경 표시(●). 표시 자리는 늘 잡아 두어 상단 바 너비가 바뀌지 않게 한다.
fn project_field(ui: &mut egui::Ui, project: &mut String, dirty: bool, path: Option<&Path>, p: &Palette) {
    ui.label("프로젝트");
    ui.add(
        egui::TextEdit::singleline(project)
            .id_salt("top-project")
            .desired_width(150.0),
    )
    .on_hover_text(path.map_or_else(
        || "아직 파일로 저장하지 않은 블루프린트 (Ctrl+S로 저장)".to_string(),
        |p| p.display().to_string(),
    ));
    let (slot, resp) = ui.allocate_exact_size(egui::vec2(12.0, 18.0), egui::Sense::hover());
    if dirty {
        ui.painter().text(
            slot.center(),
            egui::Align2::CENTER_CENTER,
            "●",
            egui::FontId::proportional(13.0),
            p.warn(),
        );
        resp.on_hover_text("저장하지 않은 변경 사항이 있습니다 (Ctrl+S로 저장)");
    }
}

/// 타겟·테마 선택. 그린 너비를 돌려준다.
fn selectors(ui: &mut egui::Ui, target: &mut TargetModel, theme_idx: &mut usize) -> f32 {
    let x0 = ui.cursor().left();
    ui.label("타겟");
    egui::ComboBox::from_id_salt("target")
        .selected_text(target.label())
        .show_ui(ui, |ui| {
            for t in TargetModel::ALL {
                ui.selectable_value(target, t, t.label());
            }
        });
    ui.label("테마");
    egui::ComboBox::from_id_salt("theme")
        .width(190.0)
        .selected_text(THEMES[(*theme_idx).min(THEMES.len() - 1)].name)
        .show_ui(ui, |ui| {
            for (i, t) in THEMES.iter().enumerate() {
                ui.selectable_value(theme_idx, i, t.name);
            }
        });
    ui.cursor().left() - x0
}

/// 상단 바 오른쪽 버튼 (오른쪽부터 왼쪽으로 쌓인다). (그린 너비, 누른 버튼)을 돌려준다.
fn top_actions(
    ui: &mut egui::Ui,
    p: &Palette,
    compiling: bool,
    can_undo: bool,
    can_redo: bool,
) -> (f32, Option<TopAction>) {
    let mut act = None;
    if theme::primary_button(ui, p, "⚡ Compile & Run", !compiling)
        .on_hover_text("Ctrl+Enter")
        .clicked()
    {
        act = Some(TopAction::Compile);
    }
    // 스피너 자리는 항상 잡아 두어 컴파일 중에도 너비가 바뀌지 않게 한다.
    let spin = egui::vec2(18.0, 18.0);
    if compiling {
        ui.add_sized(spin, egui::Spinner::new());
    } else {
        ui.allocate_exact_size(spin, egui::Sense::hover());
    }
    ui.add_space(8.0);
    if ui
        .add_enabled(can_redo, egui::Button::new("다시 실행"))
        .on_hover_text("Ctrl+Shift+Z / Ctrl+Y")
        .clicked()
    {
        act = Some(TopAction::Redo);
    }
    if ui
        .add_enabled(can_undo, egui::Button::new("실행 취소"))
        .on_hover_text(UNDO_HINT)
        .clicked()
    {
        act = Some(TopAction::Undo);
    }
    (ui.min_rect().width(), act)
}

/// X-Ray 탭의 타겟 비교 표: 같은 입력을 세 타겟으로 컴파일한 토큰 수와 입력 비용 추정치.
fn compare_grid(ui: &mut egui::Ui, p: &Palette, rows: &[TargetCost], current: TargetModel) {
    ui.label(
        RichText::new("타겟 비교 · 같은 입력을 세 타겟으로 컴파일")
            .color(p.accent)
            .strong(),
    );
    let cheapest = rows.iter().map(|c| c.cost_usd).fold(f64::INFINITY, f64::min);
    egui::Grid::new("xray-compare")
        .striped(true)
        .num_columns(5)
        .show(ui, |ui| {
            for h in ["타겟", "압축 토큰", "원본 대비", "입력 비용(추정)", ""] {
                ui.label(RichText::new(h).small().color(p.sub));
            }
            ui.end_row();
            for c in rows {
                let name = RichText::new(c.target.label());
                ui.label(if c.target == current {
                    name.strong().color(p.hi)
                } else {
                    name
                });
                ui.label(c.comp_tokens.to_string());
                ui.label(format!(
                    "{:+.1}%",
                    if c.saving_pct == 0.0 { 0.0 } else { -c.saving_pct }
                ));
                ui.label(format!("${:.5}", c.cost_usd));
                let mut marks = Vec::new();
                if c.target == current {
                    marks.push("현재");
                }
                if c.cost_usd <= cheapest && rows.len() > 1 {
                    marks.push("최저가");
                }
                ui.label(RichText::new(marks.join(" · ")).small().color(p.sub));
                ui.end_row();
            }
        });
    ui.label(
        RichText::new("o200k_base 근사 · 단가는 입력 100만 토큰당 추정치")
            .small()
            .color(p.sub),
    );
}

fn constraint_row(ui: &mut egui::Ui, c: &ConstraintNode) {
    ui.label(format!(
        "[{}] {} (p{}): {}",
        c.id,
        c.rule_type.tag(),
        c.priority,
        c.expression
    ));
}

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        if self.theme_idx != self.applied_theme {
            self.theme_idx = self.theme_idx.min(THEMES.len() - 1);
            theme::apply(&ctx, &THEMES[self.theme_idx]);
            self.applied_theme = self.theme_idx;
        }
        self.stale = self.result.is_some() && self.result_key != self.input_key();
        // 캔버스를 보지 않을 때(X-Ray 탭) 추가한 제약에도 자리를 주어, 저장 후 캔버스로 돌아왔을 때
        // 위치가 새로 생겨 "저장하지 않은 변경"으로 바뀌지 않게 한다.
        self.canvas.ensure_positions(&self.goals);
        self.dirty = self.saved.is_dirty(&self.to_blueprint(None));
        // 창 닫기: 저장하지 않은 변경이 있으면 닫기를 미루고 묻는다.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.dirty {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.confirm.is_none() {
                self.confirm = Some(Pending::Close);
            }
        }
        self.update_title(&ctx);
        let p = self.palette();
        theme::paint_backdrop(ui.painter(), ctx.content_rect(), &p);
        theme::paint_fog(&ctx, &p, self.fog_on);

        // 저장 여부를 묻는 동안에는 단축키와 파일 놓기를 받지 않는다.
        let modal_open = self.confirm.is_some();
        self.handle_drops(&ctx, modal_open);
        if !modal_open {
            self.shortcuts(&ctx);
        }

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 8))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| self.top_bar(ui, &p, &ctx));

        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 5))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| self.footer(ui, &p));

        // 좁은 창에서도 가운데 캔버스가 남도록 양쪽 패널의 최대 너비를 창 너비에 맞춘다.
        let (left_max, right_max) = side_panel_max(ctx.content_rect().width());

        egui::Panel::left("left")
            .resizable(true)
            .default_size(340.0)
            .size_range(280.0..=left_max)
            .frame(theme::backdrop(&p))
            .show(ui, |ui| {
                theme::card(&p).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    egui::ScrollArea::vertical()
                        .id_salt("left_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.left_pane(ui, &p));
                });
            });
        egui::Panel::right("right")
            .resizable(true)
            .default_size(440.0)
            .size_range(320.0..=right_max)
            .frame(theme::backdrop(&p))
            .show(ui, |ui| {
                theme::card(&p).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    self.right_pane(ui, &p);
                });
            });
        egui::CentralPanel::default().frame(theme::backdrop(&p)).show(ui, |ui| {
            theme::card(&p).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.center_pane(ui, &p);
            });
        });
        // 편집이 잠시 멈추면 한 단계로 기록한다.
        let now = ctx.input(|i| i.time);
        if self.history.track(&self.snapshot(), now) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(history::SETTLE_SECS));
        }
        self.confirm_modal(&ctx, &p);
        // 파일 대화상자는 모든 위젯 처리가 끝난 뒤 실행한다.
        if let Some(d) = self.dialog.take() {
            self.run_dialog(d, &ctx);
        }
    }
}

/// 작업 폴더의 steering 규칙 중 블루프린트에 없는 것을 사용 상태로 덧붙인다.
/// 블루프린트에 이미 있는 규칙(꺼 둔 것 포함)은 그대로 둔다.
fn merge_steering(mut bp: Blueprint, rules: &[String]) -> Blueprint {
    for rule in rules {
        if !bp.goals.iter().any(|g| g.text.trim() == rule.trim()) {
            bp.goals.push(GoalSpec {
                text: rule.clone(),
                on: true,
                pos: None,
            });
        }
    }
    bp
}

/// 데이터 파일 참조. 화면·컴파일에 쓰는 데이터(shown)와 진행 중이거나 실패·누락된 요청(pending)을 나눠,
/// 로드가 실패해도 이전 데이터를 가리키고 파일이 없어도 블루프린트의 참조를 잃지 않게 한다.
/// 각 항목은 (경로, 그 경로를 프로파일링할 때의 샘플 행 수).
#[derive(Default, Debug, Clone, PartialEq)]
struct DataRef {
    shown: Option<(PathBuf, usize)>,
    pending: Option<(PathBuf, usize)>,
}

impl DataRef {
    fn requested(&mut self, path: PathBuf, rows: usize) {
        self.pending = Some((path, rows));
    }

    /// 블루프린트가 가리키는 파일이 없을 때: 로드는 하지 않고 참조만 남긴다.
    fn missing(&mut self, path: PathBuf, rows: usize) {
        self.pending = Some((path, rows));
    }

    fn succeeded(&mut self) {
        if let Some(p) = self.pending.take() {
            self.shown = Some(p);
        }
    }

    /// 실패: 화면에 데이터가 있으면 그쪽으로 되돌리고(그 데이터의 샘플 행 수를 돌려준다),
    /// 없으면 요청한 파일을 참조로 남긴다.
    fn failed(&mut self) -> Option<usize> {
        let (_, rows) = self.shown.as_ref()?;
        self.pending = None;
        Some(*rows)
    }

    fn current(&self) -> Option<&(PathBuf, usize)> {
        self.pending.as_ref().or(self.shown.as_ref())
    }

    /// 블루프린트에 저장할 데이터 파일
    fn source(&self) -> Option<&Path> {
        self.current().map(|(p, _)| p.as_path())
    }

    /// 샘플 행 수가 바뀌었을 때 다시 프로파일링할 파일 (진행 중인 요청이 있으면 그 파일)
    fn reprofile_target(&self, want_rows: usize) -> Option<PathBuf> {
        let (path, rows) = self.current()?;
        (want_rows != *rows && path.is_file()).then(|| path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_csv(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("cc_dataref_{}_{name}.csv", std::process::id()));
        std::fs::write(&p, "a\n1\n").unwrap();
        p
    }

    #[test]
    fn data_ref_missing_blueprint_file_keeps_reference() {
        let missing = PathBuf::from("Z:/no/such/data.csv");
        let mut d = DataRef::default();
        d.missing(missing.clone(), 8);
        assert_eq!(d.source(), Some(missing.as_path())); // 다시 저장해도 data_path가 남는다
        assert_eq!(d.reprofile_target(12), None); // 없는 파일은 다시 프로파일링하지 않는다
    }

    #[test]
    fn data_ref_request_b_while_a_shown_targets_b() {
        let (a, b) = (temp_csv("a1"), temp_csv("b1"));
        let mut d = DataRef::default();
        d.requested(a.clone(), 8);
        d.succeeded();
        d.requested(b.clone(), 8); // B 로드 중
        assert_eq!(d.source(), Some(b.as_path()));
        assert_eq!(d.reprofile_target(12), Some(b.clone()));
        assert_eq!(d.reprofile_target(8), None);
        d.succeeded();
        assert_eq!(d.shown, Some((b.clone(), 8)));
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }

    #[test]
    fn data_ref_failed_load_falls_back_to_shown_data() {
        let (a, b) = (temp_csv("a2"), temp_csv("b2"));
        let mut d = DataRef::default();
        d.requested(a.clone(), 8);
        d.succeeded();
        d.requested(b.clone(), 12);
        assert_eq!(d.failed(), Some(8)); // B가 깨진 파일: 화면의 A(8행)로 되돌린다
        assert_eq!(d.source(), Some(a.as_path()));
        assert_eq!(d.reprofile_target(12), Some(a.clone())); // A를 12행으로 다시
        assert_eq!(d.reprofile_target(8), None); // A는 이미 8행
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }

    #[test]
    fn data_ref_first_load_failure_keeps_reference() {
        let b = temp_csv("b3");
        let mut d = DataRef::default();
        d.requested(b.clone(), 8);
        assert_eq!(d.failed(), None); // 되돌릴 데이터가 없다
        assert_eq!(d.source(), Some(b.as_path()));
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn steering_rules_are_added_once_and_disabled_ones_stay_off() {
        let bp = Blueprint {
            goals: vec![
                GoalSpec {
                    text: "규칙 A".into(),
                    on: false,
                    pos: None,
                },
                GoalSpec {
                    text: "다른 제약".into(),
                    on: true,
                    pos: None,
                },
            ],
            ..Blueprint::default()
        };
        let merged = merge_steering(bp, &["규칙 A ".to_string(), "규칙 B".to_string()]);
        let texts: Vec<(&str, bool)> = merged.goals.iter().map(|g| (g.text.as_str(), g.on)).collect();
        assert_eq!(texts, vec![("규칙 A", false), ("다른 제약", true), ("규칙 B", true)]);
    }

    #[test]
    fn provider_follows_target() {
        assert_eq!(provider_for(TargetModel::Claude), Provider::Anthropic);
        assert_eq!(provider_for(TargetModel::Gpt), Provider::OpenAi);
        assert_eq!(provider_for(TargetModel::Gemini), Provider::Gemini);
        for p in Provider::ALL {
            assert_eq!(Provider::ALL[provider_index(p)], p);
        }
    }

    #[test]
    fn short_status_collapses_whitespace_and_truncates() {
        assert_eq!(short("a\n  b", 10), "a b");
        assert_eq!(short("가나다라마", 3), "가나다…");
    }

    #[test]
    fn dropped_files_are_routed_by_extension() {
        for (name, kind) in [
            ("plan.kiro", DropKind::Blueprint),
            ("C:/a b/PLAN.KIRO", DropKind::Blueprint),
            ("sales.csv", DropKind::Data),
            ("sales.TSV", DropKind::Data),
            ("notes.txt", DropKind::Data),
            ("d.json", DropKind::Data),
            ("d.JSONL", DropKind::Data),
            ("d.ndjson", DropKind::Data),
            ("photo.png", DropKind::Unsupported),
            ("plan.kiro.bak", DropKind::Unsupported),
            ("README", DropKind::Unsupported),
            ("archive.csv.zip", DropKind::Unsupported),
        ] {
            assert_eq!(drop_kind(Path::new(name)), kind, "{name}");
        }
        let png = PathBuf::from("a.png");
        let csv = PathBuf::from("b.csv");
        assert_eq!(pick_drop(&[png.clone(), csv.clone()]), Some(&csv)); // 열 수 있는 파일을 고른다
        assert_eq!(pick_drop(std::slice::from_ref(&png)), Some(&png)); // 없으면 오류를 알릴 첫 파일
        assert_eq!(pick_drop(&[]), None);
        assert!(drop_hint(Some(DropKind::Unsupported)).contains("지원하지 않는"));
        assert!(drop_hint(Some(DropKind::Blueprint)).contains("블루프린트"));
    }

    fn goal(id: u64, text: &str, on: bool) -> Goal {
        Goal {
            id,
            text: text.into(),
            on,
        }
    }

    fn key_of(goals: &[Goal], fields: &str, data: Option<u64>) -> String {
        let req = compile_inputs(TargetModel::Claude, "역할", goals, OutputFormat::StrictJson, fields);
        compile_key(&req, data)
    }

    #[test]
    fn stale_key_uses_the_normalized_request() {
        let base = key_of(&[goal(1, "이상치 우선 탐지", true)], "a,b", Some(1));
        // 빈 제약 추가, 공백·줄바꿈, 꺼 둔 빈 제약, 필드 쉼표 간격은 보내는 요청을 바꾸지 않는다
        let same = [
            key_of(&[goal(1, "이상치 우선 탐지", true), goal(2, "", true)], "a,b", Some(1)),
            key_of(&[goal(1, "  이상치   우선\n탐지 ", true)], "a,b", Some(1)),
            key_of(
                &[goal(1, "이상치 우선 탐지", true), goal(2, "꺼진 제약", false)],
                "a,b",
                Some(1),
            ),
            key_of(
                &[goal(1, "이상치 우선 탐지", true), goal(2, "   ", true)],
                " a , b ,",
                Some(1),
            ),
        ];
        for k in same {
            assert_eq!(k, base);
        }
        // 실제로 바뀌는 입력은 다른 키
        let different = [
            key_of(&[goal(1, "이상치 우선 탐지", false)], "a,b", Some(1)),
            key_of(&[goal(1, "이상치 탐지", true)], "a,b", Some(1)),
            key_of(&[goal(1, "이상치 우선 탐지", true)], "a,c", Some(1)),
            key_of(&[goal(1, "이상치 우선 탐지", true)], "a,b", Some(2)),
            key_of(&[goal(1, "이상치 우선 탐지", true)], "a,b", None),
        ];
        for k in different {
            assert_ne!(k, base);
        }
        let req = compile_inputs(
            TargetModel::Gpt,
            "r",
            &[goal(1, " a  b ", true), goal(2, "", true)],
            OutputFormat::MarkdownTable,
            "x, y",
        );
        assert_eq!(req.constraints, vec!["a b".to_string()]);
        assert_eq!(req.fields, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn data_fingerprint_changes_with_profile_content() {
        let dir = std::env::temp_dir().join(format!("cc_datafp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.csv");
        std::fs::write(&p, "id,amount\n1,10\n2,20\n3,30\n4,40\n").unwrap();
        let a = pipeline::load_data(&p, 2).unwrap();
        let again = pipeline::load_data(&p, 2).unwrap();
        let more = pipeline::load_data(&p, 3).unwrap();
        assert_eq!(data_fingerprint(&a), data_fingerprint(&again));
        assert_ne!(data_fingerprint(&a), data_fingerprint(&more));
        assert_eq!(delimiter_warning(&a), None);
        // 탭 구분 파일이 한 열로 읽히면 경고한다 (프로파일러가 구분자를 판별하게 되면 경고가 사라진다)
        let tsv = dir.join("t.tsv");
        std::fs::write(&tsv, "id\tamount\tregion\n1\t10\tA\n2\t20\tB\n").unwrap();
        let t = pipeline::load_data(&tsv, 2).unwrap();
        if t.data.columns.len() == 1 {
            assert!(delimiter_warning(&t).is_some());
        } else {
            assert_eq!(delimiter_warning(&t), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 리뷰 재현: 쉼표로 구분한 파일 이름이 x.tsv면 "id,region,amount" 한 열로 읽혔는데 경고가 없었다.
    /// 프로파일러가 이제 쉼표를 고르고, 그래도 한 열에 쉼표·파이프가 남으면 경고한다.
    #[test]
    fn delimiter_warning_flags_commas_and_pipes_in_a_single_column() {
        let dir = std::env::temp_dir().join(format!("cc_delim_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [
            ("x.tsv", "id,region,amount\n1,A,10\n2,B,20\n"),
            ("p.tsv", "id|region|amount\n1|A|10\n2|B|20\n"),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            let d = pipeline::load_data(&p, 2).unwrap();
            let names: Vec<&str> = d.data.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, ["id", "region", "amount"], "{name}");
            assert_eq!(delimiter_warning(&d), None, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let one = |name: &str| LoadedData {
            path: PathBuf::from("d.tsv"),
            data: crate::ast::DataSubAst {
                source_name: "d.tsv".into(),
                total_rows: 1,
                columns: vec![crate::ast::ColumnProfile {
                    name: name.into(),
                    dtype: "text".into(),
                    null_ratio: 0.0,
                    summary_stats: "distinct:1".into(),
                }],
                anomalies: vec![],
                sample_rows: vec![],
            },
            raw_tokens: 1,
        };
        for name in ["id,region,amount", "id|region", "a\tb", "a;b"] {
            let w = delimiter_warning(&one(name));
            assert!(w.is_some_and(|w| w.contains("쉼표·파이프")), "{name:?}");
        }
        assert_eq!(delimiter_warning(&one("amount")), None);
    }

    fn reply(text: &str, stop: Option<&str>) -> llm::LlmReply {
        llm::LlmReply {
            text: text.into(),
            stop_reason: stop.map(String::from),
            usage: None,
        }
    }

    fn fields(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cut_off_reply_shows_one_muted_format_line() {
        let f = fields(&["root_causes", "outliers"]);
        let c = pipeline::check_reply(
            OutputFormat::StrictJson,
            &f,
            &reply("{\"root_causes\": \"긴 설", Some("max_tokens")),
            2048,
        );
        let lines = check_lines(&c);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].0, Tone::Err);
        assert!(lines[0].1.starts_with("✖ 출력 토큰 한도(2048)"), "{}", lines[0].1);
        assert_eq!(lines[1].0, Tone::Muted);
        assert!(
            lines[1].1.starts_with("형식 검사(잘린 응답 기준): JSON 파싱 실패"),
            "{}",
            lines[1].1
        );
        assert!(!lines.iter().any(|(_, l)| l.contains("누락 필드")));
        // 형식은 맞지만 잘린 표
        let t = "| root_causes | outliers |\n|---|---|\n| a | b |\n| c | 미";
        let c = pipeline::check_reply(OutputFormat::MarkdownTable, &f, &reply(t, Some("length")), 512);
        assert_eq!(
            check_lines(&c)[1],
            (
                Tone::Muted,
                "형식 검사(잘린 응답 기준): 출력 형식·필수 필드 일치".to_string()
            )
        );
        // 본문 없이 끝난 응답 (사고 토큰으로 한도를 다 쓴 Gemini)
        let c = pipeline::check_reply(OutputFormat::StrictJson, &f, &reply("", Some("MAX_TOKENS")), 2048);
        assert_eq!(
            check_lines(&c)[1],
            (Tone::Muted, "형식 검사(잘린 응답 기준): 본문 없음".to_string())
        );
        assert_eq!(format_summary(&c), "본문 없음");
    }

    #[test]
    fn complete_reply_lines() {
        let f = fields(&["a", "b"]);
        let ok = pipeline::check_reply(
            OutputFormat::StrictJson,
            &f,
            &reply(r#"{"a":1,"b":null}"#, Some("stop")),
            2048,
        );
        assert_eq!(
            check_lines(&ok),
            vec![
                (Tone::Ok, "✔ 출력 형식·필수 필드 검증 통과".to_string()),
                (Tone::Warn, "⚠ 값이 비어 있는 필드: b".to_string())
            ]
        );
        // 파싱 실패에는 "누락 필드: 전체"를 덧붙이지 않는다
        let bad = pipeline::check_reply(OutputFormat::StrictJson, &f, &reply("분석 결과", Some("stop")), 2048);
        let lines = check_lines(&bad);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].0 == Tone::Err && lines[0].1.starts_with("✖ JSON 파싱 실패"));
        let missing = pipeline::check_reply(OutputFormat::StrictJson, &f, &reply(r#"{"a":1}"#, None), 2048);
        assert_eq!(check_lines(&missing), vec![(Tone::Warn, "⚠ 누락 필드: b".to_string())]);
        // 헤더만 있는 표는 통과하되 경고한다
        let empty_table = pipeline::check_output(OutputFormat::MarkdownTable, &f, "| a | b |\n|---|---|");
        let lines = check_lines(&empty_table);
        assert_eq!(lines.last().unwrap().0, Tone::Warn);
        assert!(lines.last().unwrap().1.contains("데이터 행이 없습니다"));
        let (ok, text) = llm_verdict(&Ok(empty_table));
        assert!(ok && text.contains("데이터 행 없음"), "{text}");
    }

    #[test]
    fn retry_with_doubled_limit_only_for_max_tokens() {
        assert_eq!(doubled_limit(2048), Some(4096));
        assert_eq!(doubled_limit(256), Some(512));
        assert_eq!(doubled_limit(20_000), Some(blueprint::MAX_LLM_MAX_TOKENS));
        assert_eq!(doubled_limit(blueprint::MAX_LLM_MAX_TOKENS), None);
        let f = fields(&["a"]);
        let run = |stop: &str, max_tokens: u32| LlmRun {
            out: Ok(pipeline::check_reply(
                OutputFormat::StrictJson,
                &f,
                &reply("{\"a\": \"잘", Some(stop)),
                max_tokens,
            )),
            ms: 10,
            max_tokens,
        };
        assert_eq!(retry_limit(&run("max_tokens", 2048)), Some(4096));
        assert_eq!(retry_limit(&run("MAX_TOKENS", 32_000)), None); // 이미 상한
        assert_eq!(retry_limit(&run("content_filter", 2048)), None); // 한도와 무관한 중단
        let failed = LlmRun {
            out: Err("HTTP 500".into()),
            ms: 5,
            max_tokens: 2048,
        };
        assert_eq!(retry_limit(&failed), None);
        assert_eq!(run_line(&failed, 0), "응답 5 ms 후 실패");
    }

    #[test]
    fn cancelled_llm_reply_is_ignored() {
        let mut jobs = LlmJobs::default();
        let first = jobs.start();
        assert!(jobs.cancel()); // ■ 취소
        assert!(!jobs.running);
        assert!(!jobs.accept(first)); // 늦게 온 응답은 버린다
        assert!(!jobs.cancel()); // 실행 중이 아니면 상태 문구를 바꾸지 않는다
        let second = jobs.start();
        let third = jobs.start(); // 다시 실행: 앞 요청은 오래된 것
        assert!(!jobs.accept(second));
        assert!(jobs.running);
        assert!(jobs.accept(third));
        assert!(!jobs.running);
        assert!(!jobs.accept(third)); // 같은 응답을 두 번 받지 않는다
    }

    #[test]
    fn run_details_show_latency_and_usage() {
        assert_eq!(
            run_details(
                1234,
                Some(llm::Usage {
                    input: 812,
                    output: 431
                }),
                790
            ),
            "응답 1234 ms · 입력 812 · 출력 431 토큰 (압축 프롬프트 추정 790)"
        );
        assert_eq!(
            run_details(50, None, 10),
            "응답 50 ms · 사용량 정보 없음 (압축 프롬프트 추정 10 토큰)"
        );
    }

    #[test]
    fn dirty_detection_compares_with_last_saved_blueprint() {
        let mut saved = SavedState::default();
        let bp = PRESETS[0].to_blueprint();
        assert!(saved.is_dirty(&bp)); // 아직 아무것도 저장하지 않음
        saved.mark(&bp);
        assert!(!saved.is_dirty(&bp.clone()));
        let mut edited = bp.clone();
        edited.role.push_str(" (수정)");
        assert!(saved.is_dirty(&edited));
        edited.role = bp.role.clone();
        assert!(!saved.is_dirty(&edited)); // 되돌리면 깨끗한 상태
        edited.positions = vec![("root".into(), [10.0, 20.0])]; // 노드 이동도 저장 대상
        assert!(saved.is_dirty(&edited));
        let mut opts = bp.clone();
        opts.llm_options.max_tokens = 4096;
        assert!(saved.is_dirty(&opts));
        // 저장한 파일을 다시 열면 같은 지문이다
        let dir = std::env::temp_dir().join(format!("cc_dirty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.kiro");
        bp.save(&path).unwrap();
        assert!(!saved.is_dirty(&Blueprint::load(&path).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_title_marks_unsaved_changes() {
        assert_eq!(window_title("plan.kiro", false, None), "plan.kiro — Candy-Corn");
        assert_eq!(window_title("plan.kiro", true, None), "● plan.kiro — Candy-Corn");
        assert_eq!(
            window_title("plan.kiro", true, Some(Path::new("C:/x/plan.kiro"))),
            "● plan.kiro — Candy-Corn"
        );
        assert_eq!(
            window_title("매출", false, Some(Path::new("C:/x/a.kiro"))),
            "매출 (a.kiro) — Candy-Corn"
        );
        assert_eq!(window_title("  ", false, None), "이름 없음 — Candy-Corn");
    }

    #[test]
    fn side_panels_leave_room_for_the_canvas() {
        // 최소 창 너비 1000px: 기본 너비(340·440)보다 줄여 가운데에 360px을 남긴다
        let (l, r) = side_panel_max(1000.0);
        assert!(1000.0 - l - r >= CENTER_MIN_W - 0.5, "{l} {r}");
        assert!(l >= 280.0 && r >= 320.0);
        // 기본 창 너비(1400px)에서는 기본 너비를 줄이지 않는다
        let (l, r) = side_panel_max(1400.0);
        assert!(l >= 340.0 && r >= 440.0, "{l} {r}");
        // 넓은 창에서는 원래 최대 너비
        assert_eq!(side_panel_max(2560.0), (520.0, 700.0));
        // 아주 좁아도 최소 너비 아래로 내려가지 않는다
        assert_eq!(side_panel_max(500.0), (280.0, 320.0));
    }
}
