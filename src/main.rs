//! Candy-Corn: 인자 없으면 GUI, `--cli` 는 터미널 린터, `--mcp` 는 stdio MCP 서버.

// tr!/trf! 매크로를 다른 모듈보다 먼저 정의해야 하므로 맨 앞에 둔다.
#[macro_use]
mod i18n;
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
    /// GUI 시작 시 바로 불러올 CSV/JSON 파일 (.kiro 파일이면 블루프린트로 연다).
    /// --canvas와 함께 쓰면 캔버스 그래프 JSON 파일이다.
    #[arg(conflicts_with_all = ["cli", "mcp", "run_graph"])]
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
    /// 노드 캔버스 GUI (프롬프트/LLM/MCP 연결·테스트). 그래프 파일은 위치 인자나 --graph 로 함께 열 수 있다
    #[arg(long, conflicts_with_all = ["cli", "mcp", "run_graph"])]
    canvas: bool,
    /// --canvas: 열 캔버스 그래프 JSON 파일 (위치 인자로 줘도 된다)
    #[arg(
        long,
        requires = "canvas",
        conflicts_with_all = ["file", "cli", "mcp", "run_graph", "blueprint"]
    )]
    graph: Option<PathBuf>,
    /// 그래프를 헤드리스로 실행하고 최종 출력을 stdout에 쓴다
    #[arg(long, value_name = "FILE", conflicts_with_all = ["cli", "mcp"])]
    run_graph: Option<PathBuf>,
    /// --run-graph: 테스트 입력 노드(그래프에 저장된 test_node, 없으면 첫 Text 노드)의 값을 대체할 테스트 입력
    /// (`-`로 시작해도 된다)
    #[arg(long, requires = "run_graph", allow_hyphen_values = true)]
    input: Option<String>,
    /// --run-graph: MCP 노드의 로컬 프로세스 실행 허용
    #[arg(long, requires = "run_graph")]
    allow_mcp: bool,
    /// --run-graph: LLM 노드의 사용자 지정 Base URL 허용 (API 키가 그 주소로 전송된다. https 또는 localhost만)
    #[arg(long, requires = "run_graph")]
    allow_custom_endpoint: bool,
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
    /// 타겟 모델
    #[arg(
        long,
        value_enum,
        default_value = "claude",
        requires = "cli",
        conflicts_with = "blueprint"
    )]
    target: TargetArg,
    /// 출력 포맷
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
    /// 샘플 행 수 (최대 50, 넘으면 50으로 줄이고 경고)
    #[arg(long, default_value_t = 8, requires = "cli", conflicts_with = "blueprint")]
    sample_rows: usize,
    /// 화면·메시지 언어: ko(한국어) 또는 en(English). GUI는 생략하면 저장된 설정(없으면 ko),
    /// --cli/--mcp/--run-graph는 생략하면 ko(스크립트 출력이 설정에 따라 바뀌지 않게).
    #[arg(long, value_name = "ko|en", value_parser = parse_lang)]
    lang: Option<i18n::Lang>,
}

fn parse_lang(s: &str) -> Result<i18n::Lang, String> {
    let codes = i18n::Lang::ALL
        .map(|l| format!("{} ({})", l.code(), l.native_name()))
        .join(", ");
    i18n::Lang::parse(s).ok_or_else(|| {
        trf!(
            "지원하지 않는 언어: {s} (가능: {codes})",
            "unsupported language: {s} (available: {codes})"
        )
    })
}

/// 영어 도움말: (인자 id, 설명). id는 Cli 필드 이름이다.
const EN_HELP: &[(&str, &str)] = &[
    (
        "file",
        "CSV/JSON file to load when the GUI starts (a .kiro file opens as a blueprint). With --canvas it is a canvas graph JSON file",
    ),
    (
        "blueprint",
        "Blueprint (.kiro) that applies every setting at once. Without --cli it opens the GUI with those settings; with --cli it compiles with them (--data replaces the blueprint's data path, --constraint/--steering add constraints)",
    ),
    ("theme", "GUI theme number 1-7 (design samples 01-07)"),
    ("cli", "Terminal linter mode"),
    ("mcp", "stdio JSON-RPC MCP server mode"),
    (
        "canvas",
        "Node canvas GUI (connect and test prompt/LLM/MCP nodes). Open a graph file with the positional argument or --graph",
    ),
    (
        "graph",
        "--canvas: canvas graph JSON file to open (the positional argument also works)",
    ),
    ("run_graph", "Run a graph headless and write the final output to stdout"),
    (
        "input",
        "--run-graph: test input that replaces the value of the test input node (the graph's saved test_node, else the first Text node; may start with '-')",
    ),
    ("allow_mcp", "--run-graph: allow MCP nodes to start local processes"),
    (
        "allow_custom_endpoint",
        "--run-graph: allow custom LLM Base URLs (the API key is sent there; https or localhost only)",
    ),
    ("data", "CSV/TSV/JSON/NDJSON data file"),
    ("role", "Role instruction"),
    ("constraints", "Constraint (repeatable)"),
    ("steering", "Steering markdown (`- ` bullets are read as constraints)"),
    ("target", "Target model"),
    ("format", "Output format"),
    ("fields", "Required output fields (comma separated)"),
    (
        "sample_rows",
        "Sample rows (max 50; larger values are capped with a warning)",
    ),
    (
        "lang",
        "UI and message language: ko (Korean) or en (English). The GUI defaults to the saved setting (else ko); --cli/--mcp/--run-graph default to ko so script output does not depend on settings",
    ),
];

/// 언어에 맞는 명령줄 정의. 한국어는 doc 주석 그대로, 영어는 설명을 바꿔 끼운다.
fn cli_command(lang: i18n::Lang) -> clap::Command {
    use clap::CommandFactory;
    let cmd = Cli::command();
    if lang == i18n::Lang::Ko {
        return cmd;
    }
    EN_HELP.iter().fold(
        cmd.about("Prompt compression/lint studio (opens the GUI without arguments)"),
        |c, (id, help)| c.mut_arg(*id, |a| a.help(*help)),
    )
}

/// 명령줄을 읽는다. `--lang`을 먼저 보고 도움말·값 오류 문구를 그 언어로 만든다(clap 자체 문구는 영어).
fn parse_cli() -> Cli {
    use clap::FromArgMatches;
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let lang = i18n::lang_arg(&args).unwrap_or_default();
    i18n::set(lang);
    let matches = cli_command(lang).get_matches_from(args);
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// 실행 모드에 맞는 언어. GUI는 사용자가 고른 설정을 따르고, 터미널 모드는 `--lang`이 없으면 한국어로 고정한다.
fn mode_lang(cli: &Cli) -> i18n::Lang {
    let terminal = cli.cli || cli.mcp || cli.run_graph.is_some();
    if terminal {
        cli.lang.unwrap_or_default()
    } else {
        i18n::startup_lang(cli.lang)
    }
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
            eprintln!(
                "{}",
                trf!("창 아이콘 로드 실패: {e}", "failed to load window icon: {e}")
            );
            vb
        }
    }
}

fn main() -> ExitCode {
    let cli = parse_cli();
    i18n::set(mode_lang(&cli));
    if cli.mcp {
        return match mcp_server::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}", trf!("MCP 서버 오류: {e}", "mcp error: {e}"));
                ExitCode::FAILURE
            }
        };
    }
    if let Some(p) = &cli.run_graph {
        return run_graph_cli(p, cli.input.clone(), cli.allow_mcp, cli.allow_custom_endpoint);
    }
    if cli.cli {
        return run_cli(cli);
    }
    if cli.canvas {
        detach_console();
        let file = canvas_file(&cli);
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
                eprintln!("{}", trf!("GUI 실행 실패: {e}", "failed to start GUI: {e}"));
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
            eprintln!("{}", trf!("GUI 실행 실패: {e}", "failed to start GUI: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// --canvas로 열 그래프 파일: 위치 인자 또는 --graph (둘을 함께 주는 것은 clap이 막는다).
fn canvas_file(cli: &Cli) -> Option<PathBuf> {
    cli.file.clone().or_else(|| cli.graph.clone())
}

/// `--cli --sample-rows`의 상한. 블루프린트와 같은 `MAX_SAMPLE_ROWS`로 맞춘다.
fn clamp_sample_rows(n: usize) -> usize {
    n.min(blueprint::MAX_SAMPLE_ROWS)
}

fn run_cli(cli: Cli) -> ExitCode {
    let bp = match &cli.blueprint {
        Some(p) => match blueprint::Blueprint::load(p) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("{}", trf!("블루프린트 로드 실패: {e}", "failed to load blueprint: {e}"));
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let mut constraints = cli.constraints;
    if let Some(p) = &cli.steering {
        // GUI와 같이 UTF-8(BOM 포함)·UTF-16 BOM·CP949를 자동 판별한다.
        match pipeline::read_text_auto(p) {
            Ok(md) => constraints.extend(pipeline::parse_steering_rules(&md)),
            Err(e) => {
                eprintln!(
                    "{}",
                    trf!("steering 읽기 실패: {e}", "failed to read steering file: {e}")
                );
                return ExitCode::FAILURE;
            }
        }
    }
    let sample_rows = match &bp {
        Some(b) => b.sample_rows,
        None => {
            let n = clamp_sample_rows(cli.sample_rows);
            if n != cli.sample_rows {
                eprintln!(
                    "{}",
                    trf!(
                        "경고: 샘플 행 수는 최대 {n}행입니다 ({} → {n})",
                        "warning: sample rows are capped at {n} ({} → {n})",
                        cli.sample_rows
                    )
                );
            }
            n
        }
    };
    let data_path = cli
        .data
        .clone()
        .or_else(|| bp.as_ref().and_then(|b| b.data_path_from(cli.blueprint.as_deref())));
    let data = match &data_path {
        Some(p) => match pipeline::load_data(p, sample_rows) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("{}", trf!("데이터 로드 실패: {e}", "failed to load data: {e}"));
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

fn run_graph_cli(
    path: &std::path::Path,
    input: Option<String>,
    allow_mcp: bool,
    allow_custom_endpoint: bool,
) -> ExitCode {
    use canvas::{
        exec,
        graph::{self, Graph, NodeKind},
    };
    let g = match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| Graph::from_json(&s))
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{}", trf!("그래프 로드 실패: {e}", "failed to load graph: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let input_override = input.and_then(|t| {
        // GUI 테스트 실행기와 같은 노드: 그래프에 저장된 테스트 입력 노드(test_node)가 Text 노드면 그것, 아니면 첫 Text 노드
        let is_text = |n: &&graph::Node| matches!(n.kind, NodeKind::Text { .. });
        let target = g
            .test_node
            .and_then(|id| g.node(id))
            .filter(is_text)
            .or_else(|| g.nodes.iter().find(is_text));
        if target.is_none() {
            eprintln!(
                "{}",
                tr!(
                    "경고: --input을 받을 Text 노드가 없어 무시합니다",
                    "warning: no Text node to receive --input; ignored"
                )
            );
        }
        target.map(|n| (n.id, t))
    });
    let opts = exec::RunOptions {
        allow_mcp,
        allow_custom_endpoint,
        input_override,
        // Prompt 노드의 상대 data_path는 그래프 파일이 있는 폴더 기준이다.
        base_dir: exec::base_dir_of(path),
        cancel: None,
    };
    match exec::run_graph(&g, &opts, |e| match e {
        exec::Event::Done { id, ms, note, .. } => match note {
            Some(n) => eprintln!("[ok] node {id} ({ms} ms) · {n}"),
            None => eprintln!("[ok] node {id} ({ms} ms)"),
        },
        exec::Event::Failed { id, error } => eprintln!("[fail] node {id}: {error}"),
        exec::Event::Skipped(id) => eprintln!("[skip] node {id}"),
        exec::Event::Started(_) => {}
    }) {
        Ok(r) => {
            // 실패한 실행이 빈 줄을 stdout에 남기지 않게 한다. 성공한 빈 결과는 빈 줄로 낸다.
            if r.ok() || !r.final_text.is_empty() {
                println!("{}", r.final_text);
            }
            if r.ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("{}", trf!("실행 실패: {e}", "run failed: {e}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("Candy-Corn").chain(args.iter().copied()))
    }

    #[test]
    fn canvas_accepts_a_positional_graph_file() {
        let cli = parse(&["--canvas", "flow.canvas.json"]).unwrap();
        assert_eq!(canvas_file(&cli), Some(PathBuf::from("flow.canvas.json")));
        let cli = parse(&["--canvas", "--graph", "g.json"]).unwrap();
        assert_eq!(canvas_file(&cli), Some(PathBuf::from("g.json")));
        assert_eq!(canvas_file(&parse(&["--canvas"]).unwrap()), None);
    }

    #[test]
    fn graph_flag_needs_canvas_and_excludes_the_positional_file() {
        assert!(
            parse(&["--graph", "g.json"]).is_err(),
            "--canvas 없이는 스튜디오가 열리면 안 된다"
        );
        assert!(parse(&["--canvas", "--graph", "g.json", "other.json"]).is_err());
        for other in [
            ["--cli"].as_slice(),
            &["--mcp"],
            &["--run-graph", "a.json"],
            &["--blueprint", "b.kiro"],
        ] {
            let mut args = other.to_vec();
            args.extend(["--graph", "g.json"]);
            assert!(parse(&args).is_err(), "{other:?} --graph 는 조용히 무시되면 안 된다");
        }
    }

    #[test]
    fn graph_files_are_not_studio_files() {
        // --run-graph와 위치 인자를 함께 주면 위치 인자가 조용히 무시되므로 막는다.
        assert!(parse(&["--run-graph", "a.json", "b.json"]).is_err());
        assert!(parse(&["--canvas", "--run-graph", "a.json"]).is_err());
    }

    #[test]
    fn input_may_start_with_a_hyphen() {
        let cli = parse(&["--run-graph", "g.json", "--input", "-5% 증가 --x"]).unwrap();
        assert_eq!(cli.input.as_deref(), Some("-5% 증가 --x"));
        assert!(parse(&["--input", "x"]).is_err(), "--run-graph가 필요하다");
    }

    #[test]
    fn custom_endpoint_flag_belongs_to_run_graph() {
        let cli = parse(&["--run-graph", "g.json", "--allow-custom-endpoint"]).unwrap();
        assert!(cli.allow_custom_endpoint && !cli.allow_mcp);
        assert!(parse(&["--allow-custom-endpoint"]).is_err());
        assert!(!parse(&["--run-graph", "g.json"]).unwrap().allow_custom_endpoint);
    }

    #[test]
    fn sample_rows_are_capped_like_blueprints() {
        assert_eq!(clamp_sample_rows(0), 0);
        assert_eq!(clamp_sample_rows(8), 8);
        assert_eq!(
            clamp_sample_rows(blueprint::MAX_SAMPLE_ROWS),
            blueprint::MAX_SAMPLE_ROWS
        );
        assert_eq!(clamp_sample_rows(2000), blueprint::MAX_SAMPLE_ROWS);
        assert_eq!(clamp_sample_rows(usize::MAX), blueprint::MAX_SAMPLE_ROWS);
    }

    #[test]
    fn lang_flag_parses_and_terminal_modes_default_to_korean() {
        use i18n::Lang;
        assert_eq!(parse(&["--cli", "--lang", "en"]).unwrap().lang, Some(Lang::En));
        assert_eq!(parse(&["--mcp", "--lang=ko"]).unwrap().lang, Some(Lang::Ko));
        assert!(parse(&["--cli", "--lang", "fr"]).is_err());
        // 터미널 모드는 저장된 GUI 설정과 관계없이 --lang이 없으면 한국어다
        for args in [["--cli"].as_slice(), &["--mcp"], &["--run-graph", "g.json"]] {
            assert_eq!(mode_lang(&parse(args).unwrap()), Lang::Ko, "{args:?}");
        }
        assert_eq!(mode_lang(&parse(&["--cli", "--lang", "en"]).unwrap()), Lang::En);
        // GUI 모드에서 --lang을 주면 그 언어가 저장된 설정보다 우선한다
        assert_eq!(mode_lang(&parse(&["--lang", "en"]).unwrap()), Lang::En);
    }

    #[test]
    fn english_help_covers_every_argument() {
        use clap::CommandFactory;
        let ids: Vec<String> = Cli::command()
            .get_arguments()
            .map(|a| a.get_id().to_string())
            .filter(|id| id != "help" && id != "version")
            .collect();
        for id in &ids {
            assert!(EN_HELP.iter().any(|(k, _)| k == id), "영어 도움말 없음: {id}");
        }
        for (k, _) in EN_HELP {
            assert!(ids.iter().any(|id| id == k), "없는 인자의 도움말: {k}");
        }
        // 영어 명령 정의는 도움말을 만들 수 있고(mut_arg 오타면 여기서 패닉) 한국어 문구가 없다
        let help = cli_command(i18n::Lang::En).render_long_help().to_string();
        assert!(help.contains("Terminal linter mode") && !help.chars().any(|c| ('가'..='힣').contains(&c)));
        let ko = cli_command(i18n::Lang::Ko).render_long_help().to_string();
        assert!(ko.contains("터미널 린터 모드"));
    }
}
