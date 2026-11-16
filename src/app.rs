//! egui 3-Pane UI (좌: 입력 폼 / 중: 캔버스·AST X-Ray / 우: 출력 & 벤치마크).
//! 무거운 작업(프로파일링, 컴파일)은 워커 스레드에서 실행하고 mpsc 채널로 결과만 받는다.

use crate::ast::{ConstraintNode, OutputFormat, TargetModel};
use crate::blueprint::{self, Blueprint, GoalSpec, PRESETS};
use crate::canvas::{graph::Provider, llm};
use crate::history::{self, History};
use crate::pipeline::{self, CompileRequest, CompileResult, LoadedData};
use crate::studio_canvas::{Action, Canvas, Goal, LlmView, Model, NodeKey};
use crate::theme::{self, Palette, THEMES};
use eframe::egui::{self, Pos2, RichText};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

enum Msg {
    Loaded(u64, PathBuf, Result<LoadedData, String>),
}

enum CompileMsg {
    Done(u64, Box<CompileResult>),
    Failed(u64, String),
}

/// 실제 LLM 실행 결과. 첫 값은 요청 세대(오래된 응답은 버린다).
enum LlmMsg {
    Done(u64, Result<pipeline::OutputCheck, String>),
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

/// 파일 대화상자는 UI 패스 안에서 열면 프레임이 멈추고 입력이 꼬이므로 프레임 끝에서 실행한다.
#[derive(Clone, Copy)]
enum Dialog {
    PickData,
    SaveProject,
    LoadProject,
    SaveOutput,
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
    loading: bool,
    compiling: bool,
    status: String,
    status_err: bool,
    result: Option<CompileResult>,
    result_key: String,
    dialog: Option<Dialog>,
    load_gen: u64,
    hovering_file: bool,
    steering_rules: Vec<String>,
    /// 마지막으로 적용한 테마(캔버스·헤더·블루프린트 어디서 바꿔도 프레임 시작에 한 번 적용)
    applied_theme: usize,
    /// 마지막으로 열거나 저장한 블루프린트 파일
    blueprint_path: Option<PathBuf>,
    /// 데이터 파일 참조 (화면의 데이터와 진행 중·누락된 요청). 블루프린트 저장과 다시 프로파일링에 쓴다.
    data_ref: DataRef,
    compile_gen: u64,
    /// 컴파일 결과가 현재 입력과 다른지 (프레임마다 한 번 계산)
    stale: bool,
    history: History<Snapshot>,
    logo: Option<egui::TextureHandle>,
    llm_models: [String; 3],
    llm_running: bool,
    llm_gen: u64,
    llm_out: Option<Result<pipeline::OutputCheck, String>>,
    llm_tx: Sender<LlmMsg>,
    llm_rx: Receiver<LlmMsg>,

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
            loading: false,
            compiling: false,
            status: "데이터를 불러오고 Compile & Run을 누르세요 (Ctrl+Enter)".into(),
            status_err: false,
            result: None,
            result_key: String::new(),
            dialog: None,
            load_gen: 0,
            hovering_file: false,
            steering_rules: vec![],
            applied_theme: theme_idx,
            blueprint_path: None,
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
            llm_running: false,
            llm_gen: 0,
            llm_out: None,
            llm_tx,
            llm_rx,
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
        // 작업 폴더의 steering 규칙을 자동 주입
        if let Some(md) = pipeline::find_steering().and_then(|p| std::fs::read_to_string(p).ok()) {
            app.steering_rules = pipeline::parse_steering_rules(&md);
            for r in app.steering_rules.clone() {
                app.add_goal(r, true);
            }
        }
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
            app.spawn_load(p, &cc.egui_ctx);
        }
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
        self.llm_gen += 1;
        self.llm_running = true;
        self.llm_out = None;
        self.status = format!("{} · {model} 실행 중...", provider.label());
        self.status_err = false;
        let (tx, ctx, generation) = (self.llm_tx.clone(), ctx.clone(), self.llm_gen);
        std::thread::spawn(move || {
            let req = llm::LlmRequest {
                provider,
                model: &model,
                system: "",
                prompt: &prompt,
                api_key: &key,
                base_url: "",
                max_tokens: 2048,
                temperature: None,
            };
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                llm::call(&req).map(|text| pipeline::check_output(format, &fields, &text))
            }))
            .unwrap_or_else(|e| Err(format!("LLM 호출 내부 오류: {}", panic_text(e))));
            let _ = tx.send(LlmMsg::Done(generation, out));
            ctx.request_repaint();
        });
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
        THEMES[self.theme_idx]
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        // 프레임 끝(run_dialog)에서만 호출된다.
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV / JSON", &["csv", "json", "jsonl", "ndjson"])
            .pick_file()
        else {
            return;
        };
        self.spawn_load(path, ctx);
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

    /// 컴파일 결과가 현재 입력과 같은지 비교하기 위한 키.
    fn input_key(&self) -> String {
        format!(
            "{:?}|{}|{:?}|{:?}|{}|{:?}",
            self.target,
            self.role,
            self.goals.iter().filter(|g| g.on).map(|g| &g.text).collect::<Vec<_>>(),
            self.format,
            self.fields_text,
            self.loaded
                .as_ref()
                .map(|d| (&d.path, d.data.total_rows, d.data.sample_rows.len()))
        )
    }

    fn start_compile(&mut self, ctx: &egui::Context) {
        let req = CompileRequest {
            target: self.target,
            role: self.role.clone(),
            constraints: self
                .goals
                .iter()
                .filter(|g| g.on && !g.text.trim().is_empty())
                .map(|g| g.text.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect(),
            format: self.format,
            fields: pipeline::parse_fields(&self.fields_text),
            data: self.loaded.clone(),
        };
        self.compiling = true;
        self.status = "컴파일 중...".into();
        self.status_err = false;
        self.result_key = self.input_key();
        self.compile_gen += 1;
        let (tx, ctx, generation) = (self.ctx_tx.clone(), ctx.clone(), self.compile_gen);
        std::thread::spawn(move || {
            let msg = match std::panic::catch_unwind(|| pipeline::compile(&req)) {
                Ok(r) => CompileMsg::Done(generation, Box::new(r)),
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
                    self.loaded = Some(d);
                    self.data_ref.succeeded();
                }
                Err(e) => {
                    self.status = format!("로드 실패: {e}");
                    self.status_err = true;
                    // 화면에 이전 데이터가 남아 있으면 저장·다시 프로파일링도 그 데이터를 가리킨다.
                    self.data_ref.failed();
                }
            }
        }
        while let Ok(msg) = self.ctx_rx.try_recv() {
            let generation = match &msg {
                CompileMsg::Done(g, _) | CompileMsg::Failed(g, _) => *g,
            };
            if generation != self.compile_gen {
                continue;
            }
            self.compiling = false;
            match msg {
                CompileMsg::Done(_, r) => {
                    self.status = "컴파일 완료".into();
                    self.status_err = false;
                    self.result = Some(*r);
                    // 이전 프롬프트로 받은 LLM 응답은 더 이상 맞지 않는다.
                    self.llm_out = None;
                    self.llm_running = false;
                    self.llm_gen += 1;
                }
                CompileMsg::Failed(_, e) => {
                    self.status = format!("컴파일 실패: {e}");
                    self.status_err = true;
                    self.result_key.clear(); // 남아 있는 이전 결과를 "입력이 바뀜"으로 표시
                }
            }
        }
        while let Ok(LlmMsg::Done(generation, out)) = self.llm_rx.try_recv() {
            if generation != self.llm_gen {
                continue;
            }
            self.llm_running = false;
            match &out {
                Ok(c) if c.ok => self.status = "LLM 응답 · 출력 형식·필수 필드 검증 통과".into(),
                Ok(_) => self.status = "LLM 응답 · 출력 형식 위반 (오른쪽 VIII 참고)".into(),
                Err(_) => self.status = "LLM 실행 실패 (오른쪽 VIII에 자세한 내용)".into(),
            }
            self.status_err = !matches!(&out, Ok(c) if c.ok);
            self.llm_out = Some(out);
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
        }
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
        self.canvas.reset_layout();
        self.project = bp.project.clone();
        self.target = bp.target;
        self.role = bp.role.clone();
        self.format = bp.format;
        self.fields_text = bp.fields.clone();
        self.sample_rows = bp.sample_rows;
        self.theme_idx = bp.theme;
        self.goals.clear();
        for g in &bp.goals {
            self.add_goal(g.text.clone(), g.on);
            if let (Some(p), Some(last)) = (g.pos, self.goals.last()) {
                self.canvas.set_pos(NodeKey::Goal(last.id), Pos2::new(p[0], p[1]));
            }
        }
        for (name, p) in &bp.positions {
            if let Some(k) = NodeKey::FIXED
                .into_iter()
                .find(|k| k.fixed_name() == Some(name.as_str()))
            {
                self.canvas.set_pos(k, Pos2::new(p[0], p[1]));
            }
        }
        for (i, p) in Provider::ALL.iter().enumerate() {
            if let Some(m) = bp.llm_model(*p) {
                self.llm_models[i] = m.to_string();
            }
        }
        self.canvas.request_fit();
        self.result = None;
        self.result_key.clear();
        self.llm_out = None;
        self.llm_gen += 1;
        self.llm_running = false;
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
        self.history.reset(self.snapshot()); // 다른 블루프린트로 되돌아가지 않게 기록을 비운다
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

    fn save_project(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Candy-Corn 블루프린트", &["kiro"])
            .set_file_name(self.project.clone());
        if let Some(dir) = self.blueprint_path.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else {
            return;
        };
        match self.to_blueprint(Some(&path)).save(&path) {
            Ok(()) => {
                self.status = format!("블루프린트 저장: {}", path.display());
                self.status_err = false;
                self.blueprint_path = Some(path);
            }
            Err(e) => {
                self.status = e;
                self.status_err = true;
            }
        }
    }

    fn load_project(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Candy-Corn 블루프린트", &["kiro"])
            .pick_file()
        else {
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

    /// 내장 블루프린트 적용. 현재 데이터는 유지하고, 편집 내용은 실행 취소할 수 있다.
    fn apply_preset(&mut self, i: usize, ctx: &egui::Context) {
        let Some(preset) = PRESETS.get(i) else { return };
        let mut bp = self.with_steering(preset.to_blueprint());
        bp.llm_models = Provider::ALL
            .iter()
            .copied()
            .zip(self.llm_models.iter().cloned())
            .collect();
        self.apply_blueprint(bp, None, true, ctx);
        self.status = format!("블루프린트 적용: {} ({})", preset.name, preset.summary);
        self.status_err = false;
    }

    fn remove_goal(&mut self, id: u64) {
        self.goals.retain(|g| g.id != id);
        self.canvas.forget(NodeKey::Goal(id));
    }

    fn run_dialog(&mut self, d: Dialog, ctx: &egui::Context) {
        match d {
            Dialog::PickData => self.pick_file(ctx),
            Dialog::SaveProject => self.save_project(),
            Dialog::LoadProject => self.load_project(ctx),
            Dialog::SaveOutput => self.save_output(),
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
                Action::Preset(i) => self.apply_preset(i, ctx),
                Action::RunLlm => {
                    if !self.llm_running {
                        self.start_llm(ctx)
                    }
                }
                Action::ReloadSample => self.reprofile_if_needed(ctx),
                Action::SaveProject => self.dialog = Some(Dialog::SaveProject),
                Action::LoadProject => self.dialog = Some(Dialog::LoadProject),
                Action::SaveOutput => self.dialog = Some(Dialog::SaveOutput),
            }
        }
    }

    // ---------- 패널 ----------

    fn left_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        theme::section_title(ui, p, "I", "데이터 소스");
        let mut reload = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.loading, egui::Button::new("📂 CSV/JSON 로드"))
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
                let provider = provider_for(self.result.as_ref().map_or(self.target, |r| r.ast.target_model));
                let verdict = self.llm_out.as_ref().map(|o| match o {
                    Ok(c) if c.ok => (true, "✔ 출력 형식·필수 필드 통과".to_string()),
                    Ok(c) => (
                        false,
                        c.error
                            .clone()
                            .unwrap_or_else(|| format!("누락 필드: {}", c.missing.join(", "))),
                    ),
                    Err(e) => (false, short(e, 80)),
                });
                let has_key = api_key(provider).is_some();
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
                        theme_idx: &mut self.theme_idx,
                        sample_rows: &mut self.sample_rows,
                        llm: LlmView {
                            provider: provider.label(),
                            key_env: provider.default_key_env(),
                            model: &mut self.llm_models[provider_index(provider)],
                            model_hint: provider.default_model(),
                            has_key,
                            running: self.llm_running,
                            verdict,
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
        egui::ScrollArea::vertical()
            .id_salt("ast")
            .max_height(ui.available_height() * 0.65)
            .show(ui, |ui| {
                egui::CollapsingHeader::new("▼ RootPromptAST")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!("target: {}", ast.target_model.label()));
                        egui::CollapsingHeader::new(format!("RoleNode ({})", ast.role.name)).show(ui, |ui| {
                            ui.label(&ast.role.instruction);
                        });
                        match &ast.data_sub_ast {
                            Some(d) => {
                                egui::CollapsingHeader::new(format!("DataSubAST (Compressed, {} rows)", d.total_rows))
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
                        .show(ui, |ui| {
                            ui.monospace(&ast.output_schema.raw_schema);
                        });
                    });
            });
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
        let h = ui.available_height() * 0.62;
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
        let Some((target, preview, has_data)) = self
            .result
            .as_ref()
            .map(|r| (r.ast.target_model, r.preview.clone(), r.ast.data_sub_ast.is_some()))
        else {
            return;
        };
        theme::section_title(ui, p, "VIII", "LLM 실행 결과");
        let provider = provider_for(target);
        let has_key = api_key(provider).is_some();
        let stale = self.stale;
        let mut run = false;
        ui.horizontal(|ui| {
            ui.label(provider.label());
            ui.add(
                egui::TextEdit::singleline(&mut self.llm_models[provider_index(provider)])
                    .desired_width(150.0)
                    .hint_text(provider.default_model()),
            );
            let label = if self.llm_running {
                "실행 중"
            } else {
                "▶ 실제 실행"
            };
            let btn = theme::primary_button(ui, p, label, has_key && !stale && !self.compiling && !self.llm_running);
            // 키가 없을 때만 안내한다(실행 중 비활성 상태에는 붙이지 않는다).
            let btn = if has_key {
                btn
            } else {
                btn.on_disabled_hover_text(format!(
                    "환경 변수 {}에 API 키를 설정한 뒤 앱을 다시 시작하세요",
                    provider.default_key_env()
                ))
            };
            run = btn.clicked();
            if self.llm_running {
                ui.spinner();
            }
        });
        let note = match (has_key && !stale, has_data) {
            _ if has_key && stale => "입력이 바뀌었습니다. 다시 컴파일한 뒤 실행하세요".to_string(),
            (true, true) => format!(
                "실행하면 최종 출력 전체(파일 이름, 컬럼 통계, 이상치, 샘플 행 포함)가 {}로 전송됩니다",
                provider.label()
            ),
            (true, false) => format!("실행하면 최종 출력 프롬프트가 {}로 전송됩니다", provider.label()),
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
                Some(Err(e)) => {
                    ui.colored_label(theme::ERR, e);
                }
                Some(Ok(c)) => {
                    if c.ok {
                        ui.colored_label(theme::OK, "✔ 출력 형식·필수 필드 검증 통과");
                        if !c.nulls.is_empty() {
                            ui.colored_label(p.warn(), format!("⚠ 값이 비어 있는 필드: {}", c.nulls.join(", ")));
                        }
                    } else {
                        if let Some(e) = &c.error {
                            ui.colored_label(theme::ERR, format!("✖ {e}"));
                        }
                        if !c.missing.is_empty() {
                            ui.colored_label(p.warn(), format!("⚠ 누락 필드: {}", c.missing.join(", ")));
                        }
                    }
                    if ui.small_button("응답 복사").clicked() {
                        ui.ctx().copy_text(c.pretty.clone());
                    }
                    ui.add(egui::Label::new(RichText::new(&c.pretty).monospace().color(p.sub)).selectable(true));
                }
            });
        if run {
            let ctx = ui.ctx().clone();
            self.start_llm(&ctx);
        }
    }
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
        let p = self.palette();
        theme::paint_backdrop(ui.painter(), ctx.content_rect(), &p);
        theme::paint_fog(&ctx, &p);

        // 파일 드래그 앤 드롭
        // 로드 중에 놓아도 새 로드가 이긴다(load_gen).
        if let Some(path) = ctx.input(|i| {
            i.raw.dropped_files.iter().find_map(|f| {
                Some(f.path())
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.to_path_buf())
            })
        }) {
            self.spawn_load(path, &ctx);
        }
        self.hovering_file = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !ctx.text_edit_focused() {
            let cmd = egui::Modifiers::COMMAND;
            // Ctrl+Shift+Z를 먼저 검사한다(Ctrl+Z 패턴은 Shift가 눌려도 일치한다).
            if ctx.input_mut(|i| {
                i.consume_key(cmd | egui::Modifiers::SHIFT, egui::Key::Z) || i.consume_key(cmd, egui::Key::Y)
            }) {
                self.redo(&ctx);
            } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Z)) {
                self.undo(&ctx);
            }
        }
        // Ctrl+Enter로 컴파일
        if !self.compiling && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
            self.start_compile(&ctx);
        }

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 8))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    match &self.logo {
                        Some(tex) => {
                            ui.add(
                                egui::Image::new(egui::load::SizedTexture::new(tex.id(), [38.0, 38.0]))
                                    .corner_radius(9),
                            );
                        }
                        None => {
                            let (logo, _) = ui.allocate_exact_size(egui::vec2(38.0, 38.0), egui::Sense::hover());
                            theme::pumpkin_logo(ui.painter(), logo.center(), 38.0, &p);
                        }
                    }
                    ui.vertical(|ui| {
                        ui.label(RichText::new("CANDY-CORN").size(20.0).strong().color(p.hi));
                        ui.label(RichText::new(p.tag).size(10.0).color(p.accent));
                    });
                    ui.add_space(16.0);
                    ui.label("프로젝트");
                    ui.add(egui::TextEdit::singleline(&mut self.project).desired_width(150.0));
                    ui.label("타겟");
                    egui::ComboBox::from_id_salt("target")
                        .selected_text(self.target.label())
                        .show_ui(ui, |ui| {
                            for t in TargetModel::ALL {
                                ui.selectable_value(&mut self.target, t, t.label());
                            }
                        });
                    ui.label("테마");
                    egui::ComboBox::from_id_salt("theme")
                        .width(190.0)
                        .selected_text(THEMES[self.theme_idx].name)
                        .show_ui(ui, |ui| {
                            for (i, t) in THEMES.iter().enumerate() {
                                ui.selectable_value(&mut self.theme_idx, i, t.name);
                            }
                        });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::primary_button(ui, &p, "⚡ Compile & Run", !self.compiling).clicked() {
                            self.start_compile(&ctx);
                        }
                        if self.compiling {
                            ui.spinner();
                        }
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(self.history.can_redo(), egui::Button::new("다시 실행"))
                            .on_hover_text("Ctrl+Shift+Z / Ctrl+Y")
                            .clicked()
                        {
                            self.redo(&ctx);
                        }
                        if ui
                            .add_enabled(self.history.can_undo(), egui::Button::new("실행 취소"))
                            .on_hover_text(
                                "Ctrl+Z · 대상: 역할, 제약, 출력 포맷·필드, 타겟\n(테마·데이터·샘플 행 수·프로젝트 이름·노드 배치는 대상이 아닙니다. 글자 입력 중에는 입력칸 안에서 되돌립니다)",
                            )
                            .clicked()
                        {
                            self.undo(&ctx);
                        }
                    });
                });
            });

        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(p.panel2.gamma_multiply(0.85))
                    .inner_margin(egui::Margin::symmetric(16, 5))
                    .stroke(egui::Stroke::new(1.0, p.line)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.hovering_file {
                        ui.label(RichText::new("여기에 놓으면 데이터를 로드합니다").small().color(p.hi));
                    } else {
                        let color = if self.status_err { theme::ERR } else { p.sub };
                        let r = ui.label(RichText::new(short(&self.status, 110)).small().color(color));
                        if self.status.chars().count() > 110 {
                            r.on_hover_text(&self.status);
                        }
                    }
                    if self.stale && !self.compiling {
                        ui.label(
                            RichText::new("● 입력이 바뀜: 다시 컴파일하세요")
                                .small()
                                .color(p.warn()),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("Ctrl+Enter 컴파일 · 파일 끌어다 놓기로 로드")
                                .small()
                                .color(p.sub),
                        );
                    });
                });
            });

        egui::Panel::left("left")
            .resizable(true)
            .default_size(340.0)
            .size_range(280.0..=520.0)
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
            .size_range(320.0..=700.0)
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

    /// 실패: 화면에 데이터가 있으면 그쪽으로 되돌리고, 없으면 요청한 파일을 참조로 남긴다.
    fn failed(&mut self) {
        if self.shown.is_some() {
            self.pending = None;
        }
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
        d.failed(); // B가 깨진 파일
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
        d.failed();
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
}
