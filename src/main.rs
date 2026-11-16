//! Candy-Corn: 인자 없으면 GUI, `--cli` 는 터미널 린터, `--mcp` 는 stdio MCP 서버.

mod app;
mod ast;
mod blueprint;
mod canvas;
mod history;
mod mcp_server;
mod pipeline;
mod profiler;
#[cfg(test)]
mod qc;
mod studio_canvas;
mod theme;
mod tokenizer;

use ast::{OutputFormat, TargetModel};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
enum TargetArg {
    Claude,
    Gpt,
    Gemini,
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Json,
    Table,
}

#[derive(Parser)]
#[command(
    name = "Candy-Corn",
    version,
    about = "프롬프트 압축/린트 스튜디오 (인자 없으면 GUI 실행)"
)]
struct Cli {
    /// GUI 시작 시 바로 불러올 CSV/JSON 파일 (.kiro 파일이면 블루프린트로 연다)
    #[arg(conflicts_with_all = ["cli", "mcp"])]
    file: Option<PathBuf>,
    /// 블루프린트(.kiro): 전체 설정을 한 번에 적용한다. --cli 없이 쓰면 GUI를 그 설정으로 연다.
    /// --cli와 함께 쓰면 그 설정으로 컴파일한다. 이때 --data는 블루프린트의 데이터 경로를 대신하고,
    /// --constraint/--steering은 제약을 추가한다.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["mcp", "canvas", "run_graph", "file"])]
    blueprint: Option<PathBuf>,
    /// GUI 테마 번호 1~7 (디자인샘플 01~07)
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=7))]
    theme: Option<u8>,
    /// 터미널 린터 모드
    #[arg(long, conflicts_with = "mcp")]
    cli: bool,
    /// stdio JSON-RPC MCP 서버 모드
    #[arg(long)]
    mcp: bool,
    /// 노드 캔버스 GUI (프롬프트/LLM/MCP 연결·테스트). --graph 로 파일을 함께 열 수 있다
    #[arg(long, conflicts_with_all = ["cli", "mcp", "run_graph"])]
    canvas: bool,
    /// 캔버스 그래프 JSON 파일
    #[arg(long)]
    graph: Option<PathBuf>,
    /// 그래프를 헤드리스로 실행하고 최종 출력을 stdout에 쓴다
    #[arg(long, value_name = "FILE", conflicts_with_all = ["cli", "mcp"])]
    run_graph: Option<PathBuf>,
    /// --run-graph: 첫 Text 노드의 값을 대체할 테스트 입력
    #[arg(long, requires = "run_graph")]
    input: Option<String>,
    /// --run-graph: MCP 노드의 로컬 프로세스 실행 허용
    #[arg(long, requires = "run_graph")]
    allow_mcp: bool,
    /// CSV/JSON/NDJSON 데이터 파일
    #[arg(long, requires = "cli")]
    data: Option<PathBuf>,
    /// 역할 지시문
    #[arg(long, default_value = "Data analyst", requires = "cli", conflicts_with = "blueprint")]
    role: String,
    /// 제약 조건 (여러 번 지정 가능)
    #[arg(long = "constraint", requires = "cli")]
    constraints: Vec<String>,
    /// steering 마크다운 (`- ` 불릿을 제약으로 읽음)
    #[arg(long, requires = "cli")]
    steering: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        default_value = "claude",
        requires = "cli",
        conflicts_with = "blueprint"
    )]
    target: TargetArg,
    #[arg(
        long,
        value_enum,
        default_value = "json",
        requires = "cli",
        conflicts_with = "blueprint"
    )]
    format: FormatArg,
    /// 필수 출력 필드 (쉼표 구분)
    #[arg(long, default_value = pipeline::DEFAULT_FIELDS, requires = "cli", conflicts_with = "blueprint")]
    fields: String,
    /// 샘플 행 수
    #[arg(long, default_value_t = 8, requires = "cli", conflicts_with = "blueprint")]
    sample_rows: usize,
}

/// 릴리스 빌드에서 GUI 실행 시 같이 뜨는 콘솔 창을 닫는다(디버그 빌드는 로그 확인용으로 유지).
#[cfg(all(windows, not(debug_assertions)))]
fn detach_console() {
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    // SAFETY: 인자 없는 Win32 호출이며 실패해도 무해하다.
    unsafe {
        FreeConsole();
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn detach_console() {}

/// 창 아이콘(제목 표시줄·작업 표시줄·Alt+Tab). exe 파일 아이콘은 build.rs가 assets/icon.ico로 넣는다.
fn window_icon() -> eframe::egui::ViewportBuilder {
    let vb = eframe::egui::ViewportBuilder::default();
    match eframe::icon_data::from_png_bytes(theme::ICON_PNG) {
        Ok(icon) => vb.with_icon(std::sync::Arc::new(icon)),
        Err(e) => {
            eprintln!("창 아이콘 로드 실패: {e}");
            vb
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.mcp {
        return match mcp_server::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("mcp error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(p) = &cli.run_graph {
        return run_graph_cli(p, cli.input.clone(), cli.allow_mcp);
    }
    if cli.cli {
        return run_cli(cli);
    }
    if cli.canvas {
        detach_console();
        let file = cli.graph.clone();
        let options = eframe::NativeOptions {
            viewport: window_icon()
                .with_inner_size([1500.0, 900.0])
                .with_title("Candy-Corn Canvas"),
            ..Default::default()
        };
        return match eframe::run_native(
            "Candy-Corn Canvas",
            options,
            Box::new(move |cc| Ok(Box::new(canvas::ui::CanvasApp::new(cc, file)))),
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("GUI 실행 실패: {e}");
                ExitCode::FAILURE
            }
        };
    }
    // .kiro 파일을 위치 인자로 넘기면(탐색기에서 연결 프로그램으로 열기 등) 블루프린트로 연다.
    let (startup_file, startup_blueprint) = match (cli.file.clone(), cli.blueprint.clone()) {
        (Some(f), None) if f.extension().is_some_and(|e| e.eq_ignore_ascii_case("kiro")) => (None, Some(f)),
        (f, b) => (f, b),
    };
    let theme_arg = cli.theme.map(|t| t as usize - 1);
    detach_console();
    let options = eframe::NativeOptions {
        viewport: window_icon()
            .with_inner_size([1400.0, 820.0])
            .with_min_inner_size([1000.0, 600.0])
            .with_title("Candy-Corn"),
        ..Default::default()
    };
    match eframe::run_native(
        "Candy-Corn",
        options,
        Box::new(|cc| {
            Ok(Box::new(app::StudioApp::new(
                cc,
                startup_file.clone(),
                startup_blueprint.clone(),
                theme_arg,
            )))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("GUI 실행 실패: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(cli: Cli) -> ExitCode {
    let bp = match &cli.blueprint {
        Some(p) => match blueprint::Blueprint::load(p) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("블루프린트 로드 실패: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let mut constraints = cli.constraints;
    if let Some(p) = &cli.steering {
        match std::fs::read_to_string(p) {
            Ok(md) => constraints.extend(pipeline::parse_steering_rules(&md)),
            Err(e) => {
                eprintln!("steering 읽기 실패: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let sample_rows = bp.as_ref().map_or(cli.sample_rows, |b| b.sample_rows);
    let data_path = cli
        .data
        .clone()
        .or_else(|| bp.as_ref().and_then(|b| b.data_path_from(cli.blueprint.as_deref())));
    let data = match &data_path {
        Some(p) => match pipeline::load_data(p, sample_rows) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("데이터 로드 실패: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let req = match &bp {
        Some(b) => b.to_request(constraints, data),
        None => pipeline::CompileRequest {
            target: match cli.target {
                TargetArg::Claude => TargetModel::Claude,
                TargetArg::Gpt => TargetModel::Gpt,
                TargetArg::Gemini => TargetModel::Gemini,
            },
            role: cli.role,
            constraints,
            format: match cli.format {
                FormatArg::Json => OutputFormat::StrictJson,
                FormatArg::Table => OutputFormat::MarkdownTable,
            },
            fields: pipeline::parse_fields(&cli.fields),
            data,
        },
    };
    let r = pipeline::compile(&req);
    println!("{}\n", r.output);
    for d in &r.ast.lint_warnings {
        eprintln!("[{}] {}", d.severity, d.message);
    }
    eprintln!(
        "tokens: {} -> {} ({:+.1}%, saved ${:.4})",
        r.orig_tokens, r.comp_tokens, -r.saving_pct, r.saved_usd
    );
    if r.ast.lint_warnings.iter().any(|d| d.severity == "ERROR") {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_graph_cli(path: &std::path::Path, input: Option<String>, allow_mcp: bool) -> ExitCode {
    use canvas::{
        exec,
        graph::{Graph, NodeKind},
    };
    let g = match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| Graph::from_json(&s))
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("그래프 로드 실패: {e}");
            return ExitCode::FAILURE;
        }
    };
    let input_override = input.and_then(|t| {
        g.nodes
            .iter()
            .find(|n| matches!(n.kind, NodeKind::Text { .. }))
            .map(|n| (n.id, t))
    });
    let opts = exec::RunOptions {
        allow_mcp,
        input_override,
    };
    match exec::run_graph(&g, &opts, |e| match e {
        exec::Event::Done { id, ms, .. } => eprintln!("[ok] node {id} ({ms} ms)"),
        exec::Event::Failed { id, error } => eprintln!("[fail] node {id}: {error}"),
        exec::Event::Skipped(id) => eprintln!("[skip] node {id}"),
        exec::Event::Started(_) => {}
    }) {
        Ok(r) => {
            println!("{}", r.final_text);
            if r.ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("실행 실패: {e}");
            ExitCode::FAILURE
        }
    }
}
