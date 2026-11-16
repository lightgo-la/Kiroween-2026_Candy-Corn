//! egui 3-Pane UI (좌: 입력 폼 / 중: 캔버스·AST X-Ray / 우: 출력 & 벤치마크).
//! 무거운 작업(프로파일링, 컴파일)은 워커 스레드에서 실행하고 mpsc 채널로 결과만 받는다.

use crate::ast::{ConstraintNode, OutputFormat, TargetModel};
use crate::canvas::{graph::Provider, llm};
use crate::history::{self, History};
use crate::pipeline::{self, CompileRequest, CompileResult, LoadedData};
use crate::studio_canvas::{self as canvas, Action, Canvas, Goal, Model, NodeKey};
use crate::theme::{self, Palette, THEMES};
use eframe::egui::{self, Pos2, RichText};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

enum Msg {
    Loaded(u64, PathBuf, Result<LoadedData, String>),
}

enum CompileMsg {
    Done(Box<CompileResult>),
    Failed(String),
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
fn api_key(p: Provider) -> Option<String> {
    std::env::var(p.default_key_env()).ok().filter(|k| !k.trim().is_empty())
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

/// `.kiro` 프로젝트 파일 형식. 필드가 빠진 예전 파일도 읽을 수 있게 기본값을 둔다.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct ProjectFile {
    version: u32,
    project: String,
    target: TargetModel,
    role: String,
    goals: Vec<GoalSave>,
    format: OutputFormat,
    fields: String,
    sample_rows: usize,
    theme: usize,
    data_path: Option<String>,
    positions: Vec<(String, [f32; 2])>,
}

const PROJECT_VERSION: u32 = 1;

impl Default for ProjectFile {
    fn default() -> Self {
        ProjectFile {
            version: PROJECT_VERSION,
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
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct GoalSave {
    text: String,
    on: bool,
    pos: Option<[f32; 2]>,
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
    loaded_k: usize,
    hovering_file: bool,
    steering_rules: Vec<String>,
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
    pub fn new(cc: &eframe::CreationContext<'_>, startup_file: Option<PathBuf>, theme_arg: Option<usize>) -> Self {
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
            loaded_k: 8,
            hovering_file: false,
            steering_rules: vec![],
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

    fn undo(&mut self) {
        match self.history.undo() {
            Some(s) => self.restore(s, "실행 취소"),
            None => self.status = "되돌릴 작업이 없습니다".into(),
        }
    }

    fn redo(&mut self) {
        match self.history.redo() {
            Some(s) => self.restore(s, "다시 실행"),
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
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| llm::call(&req)))
                .unwrap_or_else(|e| Err(format!("LLM 호출 내부 오류: {}", panic_text(e))))
                .map(|text| pipeline::check_output(format, &fields, &text));
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
        self.loading = true;
        self.status = format!("프로파일링 중: {}", path.display());
        self.status_err = false;
        self.load_gen += 1;
        self.loaded_k = self.sample_rows;
        let (tx, ctx, k, generation) = (self.tx.clone(), ctx.clone(), self.sample_rows, self.load_gen);
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| pipeline::load_data(&path, k))
                .unwrap_or_else(|e| Err(format!("프로파일러 내부 오류: {}", panic_text(e))));
            let _ = tx.send(Msg::Loaded(generation, path, r));
            ctx.request_repaint();
        });
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
        let (tx, ctx) = (self.ctx_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let msg = match std::panic::catch_unwind(|| pipeline::compile(&req)) {
                Ok(r) => CompileMsg::Done(Box::new(r)),
                Err(e) => CompileMsg::Failed(panic_text(e)),
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
                }
                Err(e) => {
                    self.status = format!("로드 실패: {e}");
                    self.status_err = true;
                }
            }
        }
        while let Ok(msg) = self.ctx_rx.try_recv() {
            self.compiling = false;
            match msg {
                CompileMsg::Done(r) => {
                    self.status = "컴파일 완료".into();
                    self.status_err = false;
                    self.result = Some(*r);
                    // 이전 프롬프트로 받은 LLM 응답은 더 이상 맞지 않는다.
                    self.llm_out = None;
                    self.llm_running = false;
                    self.llm_gen += 1;
                }
                CompileMsg::Failed(e) => {
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
                Ok(c) if c.ok => self.status = "LLM 응답 · 출력 스키마 검증 통과".into(),
                Ok(_) => self.status = "LLM 응답 · 출력 스키마 위반".into(),
                Err(e) => self.status = format!("LLM 실행 실패: {e}"),
            }
            self.status_err = !matches!(&out, Ok(c) if c.ok);
            self.llm_out = Some(out);
        }
    }

    // ---------- 프로젝트 / 결과 파일 ----------

    fn save_project(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Sugar-Coat 프로젝트", &["kiro"])
            .set_file_name(self.project.clone())
            .save_file()
        else {
            return;
        };
        let mut positions = vec![];
        for k in NodeKey::FIXED {
            if let (Some(name), Some(p)) = (k.fixed_name(), self.canvas.pos_of(k)) {
                positions.push((name.to_string(), [p.x, p.y]));
            }
        }
        let file = ProjectFile {
            version: PROJECT_VERSION,
            project: self.project.clone(),
            target: self.target,
            role: self.role.clone(),
            goals: self
                .goals
                .iter()
                .map(|g| GoalSave {
                    text: g.text.clone(),
                    on: g.on,
                    pos: self.canvas.pos_of(NodeKey::Goal(g.id)).map(|p| [p.x, p.y]),
                })
                .collect(),
            format: self.format,
            fields: self.fields_text.clone(),
            sample_rows: self.sample_rows,
            theme: self.theme_idx,
            data_path: self.loaded.as_ref().map(|d| d.path.display().to_string()),
            positions,
        };
        match serde_json::to_string_pretty(&file)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&path, s).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                self.status = format!("프로젝트 저장: {}", path.display());
                self.status_err = false;
            }
            Err(e) => {
                self.status = format!("저장 실패: {e}");
                self.status_err = true;
            }
        }
    }

    fn load_project(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Sugar-Coat 프로젝트", &["kiro"])
            .pick_file()
        else {
            return;
        };
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<ProjectFile>(&s).map_err(|e| format!("형식 오류: {e}")));
        let f = match parsed {
            Ok(f) if f.version > PROJECT_VERSION => {
                self.status = format!("불러오기 실패: 더 새로운 형식(v{})의 프로젝트입니다", f.version);
                self.status_err = true;
                return;
            }
            Ok(f) => f,
            Err(e) => {
                self.status = format!("불러오기 실패: {e}");
                self.status_err = true;
                return;
            }
        };
        // 이전 프로젝트의 데이터와 진행 중인 로드를 버린다.
        self.loaded = None;
        self.loading = false;
        self.load_gen += 1;
        self.canvas.reset_layout();
        self.project = f.project;
        self.target = f.target;
        self.role = f.role;
        self.format = f.format;
        self.fields_text = f.fields;
        self.sample_rows = f.sample_rows.min(50);
        self.theme_idx = f.theme.min(THEMES.len() - 1);
        theme::apply(ctx, &self.palette());
        self.goals.clear();
        for g in f.goals {
            self.add_goal(g.text, g.on);
            if let (Some(p), Some(last)) = (g.pos, self.goals.last()) {
                self.canvas.set_pos(NodeKey::Goal(last.id), Pos2::new(p[0], p[1]));
            }
        }
        for (name, p) in f.positions {
            if let Some(k) = NodeKey::FIXED
                .into_iter()
                .find(|k| k.fixed_name() == Some(name.as_str()))
            {
                self.canvas.set_pos(k, Pos2::new(p[0], p[1]));
            }
        }
        self.canvas.request_fit();
        self.result = None;
        self.status = format!("프로젝트 불러옴: {}", path.display());
        self.status_err = false;
        match f.data_path.map(PathBuf::from) {
            Some(dp) if dp.is_file() => self.spawn_load(dp, ctx),
            Some(dp) => {
                self.status = format!("프로젝트 불러옴 · 데이터 파일을 찾을 수 없음: {}", dp.display());
                self.status_err = true;
            }
            None => {}
        }
        self.llm_out = None;
        self.llm_gen += 1;
        self.llm_running = false;
        self.history.reset(self.snapshot()); // 다른 프로젝트로 되돌아가지 않게 기록을 비운다
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

    fn apply_recipe(&mut self, i: usize) {
        let r = &canvas::RECIPES[i];
        self.role = r.role.to_string();
        self.fields_text = r.fields.to_string();
        self.goals.clear();
        for g in r.goals {
            self.add_goal(g.to_string(), true);
        }
        // steering 규칙은 레시피와 별개로 유지한다.
        for rule in self.steering_rules.clone() {
            if !r.goals.contains(&rule.as_str()) {
                self.add_goal(rule, true);
            }
        }
        self.canvas.reset_layout();
        self.status = format!("레시피 적용: {}", r.name);
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
                Action::Recipe(i) => self.apply_recipe(i),
                Action::SaveProject => self.dialog = Some(Dialog::SaveProject),
                Action::LoadProject => self.dialog = Some(Dialog::LoadProject),
                Action::SaveOutput => self.dialog = Some(Dialog::SaveOutput),
            }
        }
    }

    // ---------- 패널 ----------

    fn left_pane(&mut self, ui: &mut egui::Ui, p: &Palette) {
        theme::section_title(ui, p, "I", "데이터 소스");
        let mut reload = None;
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
            let r = ui.add(egui::DragValue::new(&mut self.sample_rows).range(0..=50));
            if (r.drag_stopped() || r.lost_focus()) && self.sample_rows != self.loaded_k {
                reload = self.loaded.as_ref().map(|d| d.path.clone());
            }
        });
        if let Some(path) = reload {
            self.spawn_load(path, ui.ctx());
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
                if enter {
                    resp.request_focus(); // 연속 입력
                }
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
                let stale = self.result.is_some() && self.result_key != self.input_key();
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
        theme::section_title(ui, p, "VII", "최종 Sugar-Coated 출력");
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
        let Some((target, preview)) = self.result.as_ref().map(|r| (r.ast.target_model, r.preview.clone())) else {
            return;
        };
        theme::section_title(ui, p, "VIII", "LLM 실행 결과");
        let provider = provider_for(target);
        let has_key = api_key(provider).is_some();
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
            run = theme::primary_button(ui, p, label, has_key && !self.llm_running)
                .on_disabled_hover_text(format!(
                    "환경 변수 {}에 API 키를 설정하세요",
                    provider.default_key_env()
                ))
                .clicked();
            if self.llm_running {
                ui.spinner();
            }
        });
        let note = if has_key {
            format!(
                "실행하면 데이터 샘플이 포함된 프롬프트가 {}로 전송됩니다",
                provider.label()
            )
        } else {
            format!(
                "{} 환경 변수가 없어 스키마 골격(모의 응답)만 보여 줍니다",
                provider.default_key_env()
            )
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
                        ui.colored_label(theme::OK, "✔ 출력 스키마 검증 통과");
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
        if !ctx.egui_wants_keyboard_input() {
            let cmd = egui::Modifiers::COMMAND;
            // Ctrl+Shift+Z를 먼저 검사한다(Ctrl+Z 패턴은 Shift가 눌려도 일치한다).
            if ctx.input_mut(|i| {
                i.consume_key(cmd | egui::Modifiers::SHIFT, egui::Key::Z) || i.consume_key(cmd, egui::Key::Y)
            }) {
                self.redo();
            } else if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Z)) {
                self.undo();
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
                        ui.label(RichText::new("SUGAR-COAT STUDIO").size(20.0).strong().color(p.hi));
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
                    let before = self.theme_idx;
                    egui::ComboBox::from_id_salt("theme")
                        .width(190.0)
                        .selected_text(THEMES[self.theme_idx].name)
                        .show_ui(ui, |ui| {
                            for (i, t) in THEMES.iter().enumerate() {
                                ui.selectable_value(&mut self.theme_idx, i, t.name);
                            }
                        });
                    if before != self.theme_idx {
                        theme::apply(&ctx, &THEMES[self.theme_idx]);
                    }
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
                            self.redo();
                        }
                        if ui
                            .add_enabled(self.history.can_undo(), egui::Button::new("실행 취소"))
                            .on_hover_text("Ctrl+Z (글자 입력 중에는 입력칸 안에서 되돌립니다)")
                            .clicked()
                        {
                            self.undo();
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
                        ui.label(RichText::new(&self.status).small().color(color));
                    }
                    if self.result.is_some() && !self.compiling && self.result_key != self.input_key() {
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
