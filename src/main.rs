//! Candy-Corn: the GUI without arguments, `--cli` is the terminal linter, `--mcp` is the stdio MCP server.

// Declared first: the tr!/trf! macros must be defined before the other modules.
#[macro_use]
mod i18n;
mod app;
mod ast;
mod blueprint;
mod canvas;
mod history;
mod lexicon;
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
    about = "Prompt compression/lint studio (opens the GUI without arguments)"
)]
struct Cli {
    /// CSV/JSON file to load when the GUI starts (a .kiro file opens as a blueprint).
    /// With --canvas it is a canvas graph JSON file
    #[arg(conflicts_with_all = ["cli", "mcp", "run_graph"])]
    file: Option<PathBuf>,
    /// Blueprint (.kiro) that applies every setting at once. Without --cli it opens the GUI with those settings;
    /// with --cli it compiles with them (--data replaces the blueprint's data path, --constraint/--steering add
    /// constraints)
    #[arg(long, value_name = "FILE", conflicts_with_all = ["mcp", "canvas", "run_graph", "file"])]
    blueprint: Option<PathBuf>,
    /// GUI theme number 1-7 (design samples 01-07)
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=7))]
    theme: Option<u8>,
    /// Terminal linter mode
    #[arg(long, conflicts_with = "mcp")]
    cli: bool,
    /// stdio JSON-RPC MCP server mode
    #[arg(long)]
    mcp: bool,
    /// Node canvas GUI (connect and test prompt/LLM/MCP nodes). Open a graph file with the positional argument or
    /// --graph
    #[arg(long, conflicts_with_all = ["cli", "mcp", "run_graph"])]
    canvas: bool,
    /// --canvas: canvas graph JSON file to open (the positional argument also works)
    #[arg(
        long,
        requires = "canvas",
        conflicts_with_all = ["file", "cli", "mcp", "run_graph", "blueprint"]
    )]
    graph: Option<PathBuf>,
    /// Run a graph headless and write the final output to stdout
    #[arg(long, value_name = "FILE", conflicts_with_all = ["cli", "mcp"])]
    run_graph: Option<PathBuf>,
    /// --run-graph: test input that replaces the value of the test input node (the graph's saved test_node, else
    /// the first Text node; may start with '-')
    #[arg(long, requires = "run_graph", allow_hyphen_values = true)]
    input: Option<String>,
    /// --run-graph: allow MCP nodes to start local processes
    #[arg(long, requires = "run_graph")]
    allow_mcp: bool,
    /// --run-graph: allow custom LLM Base URLs (the API key is sent there; https or localhost only)
    #[arg(long, requires = "run_graph")]
    allow_custom_endpoint: bool,
    /// CSV/TSV/JSON/NDJSON data file
    #[arg(long, requires = "cli")]
    data: Option<PathBuf>,
    /// Role instruction
    #[arg(long, default_value = "Data analyst", requires = "cli", conflicts_with = "blueprint")]
    role: String,
    /// Constraint (repeatable)
    #[arg(long = "constraint", requires = "cli")]
    constraints: Vec<String>,
    /// Steering markdown (`- ` bullets are read as constraints)
    #[arg(long, requires = "cli")]
    steering: Option<PathBuf>,
    /// Target model
    #[arg(
        long,
        value_enum,
        default_value = "claude",
        requires = "cli",
        conflicts_with = "blueprint"
    )]
    target: TargetArg,
    /// Output format
    #[arg(
        long,
        value_enum,
        default_value = "json",
        requires = "cli",
        conflicts_with = "blueprint"
    )]
    format: FormatArg,
    /// Required output fields (comma separated)
    #[arg(long, default_value = pipeline::DEFAULT_FIELDS, requires = "cli", conflicts_with = "blueprint")]
    fields: String,
    /// Sample rows (max 50; larger values are capped with a warning)
    #[arg(long, default_value_t = 8, requires = "cli", conflicts_with = "blueprint")]
    sample_rows: usize,
    /// UI and message language: en (English) or ko (Korean). The GUI defaults to the saved setting (else en);
    /// --cli/--mcp/--run-graph default to en so script output does not depend on settings
    #[arg(long, value_name = "en|ko", value_parser = parse_lang)]
    lang: Option<i18n::Lang>,
}

fn parse_lang(s: &str) -> Result<i18n::Lang, String> {
    let codes = i18n::Lang::ALL
        .map(|l| format!("{} ({})", l.code(), l.native_name()))
        .join(", ");
    i18n::Lang::parse(s).ok_or_else(|| trf!("unsupported language: {s} (available: {codes})", s = s, codes = codes))
}

/// Korean help: (argument id, text). The id is the Cli field name. English is the derive doc comments above.
const KO_HELP: &[(&str, &str)] = &[
    (
        "file",
        ko_text!(
            "CSV/JSON file to load when the GUI starts (a .kiro file opens as a blueprint). With --canvas it is a canvas graph JSON file"
        ),
    ),
    (
        "blueprint",
        ko_text!(
            "Blueprint (.kiro) that applies every setting at once. Without --cli it opens the GUI with those settings; with --cli it compiles with them (--data replaces the blueprint's data path, --constraint/--steering add constraints)"
        ),
    ),
    ("theme", ko_text!("GUI theme number 1-7 (design samples 01-07)")),
    ("cli", ko_text!("Terminal linter mode")),
    ("mcp", ko_text!("stdio JSON-RPC MCP server mode")),
    (
        "canvas",
        ko_text!(
            "Node canvas GUI (connect and test prompt/LLM/MCP nodes). Open a graph file with the positional argument or --graph"
        ),
    ),
    (
        "graph",
        ko_text!("--canvas: canvas graph JSON file to open (the positional argument also works)"),
    ),
    (
        "run_graph",
        ko_text!("Run a graph headless and write the final output to stdout"),
    ),
    (
        "input",
        ko_text!(
            "--run-graph: test input that replaces the value of the test input node (the graph's saved test_node, else the first Text node; may start with '-')"
        ),
    ),
    (
        "allow_mcp",
        ko_text!("--run-graph: allow MCP nodes to start local processes"),
    ),
    (
        "allow_custom_endpoint",
        ko_text!("--run-graph: allow custom LLM Base URLs (the API key is sent there; https or localhost only)"),
    ),
    ("data", ko_text!("CSV/TSV/JSON/NDJSON data file")),
    ("role", ko_text!("Role instruction")),
    ("constraints", ko_text!("Constraint (repeatable)")),
    (
        "steering",
        ko_text!("Steering markdown (`- ` bullets are read as constraints)"),
    ),
    ("target", ko_text!("Target model")),
    ("format", ko_text!("Output format")),
    ("fields", ko_text!("Required output fields (comma separated)")),
    (
        "sample_rows",
        ko_text!("Sample rows (max 50; larger values are capped with a warning)"),
    ),
    (
        "lang",
        ko_text!(
            "UI and message language: en (English) or ko (Korean). The GUI defaults to the saved setting (else en); --cli/--mcp/--run-graph default to en so script output does not depend on settings"
        ),
    ),
];

/// Command-line definition for a language. English is the derive doc comments; Korean swaps in `KO_HELP`.
fn cli_command(lang: i18n::Lang) -> clap::Command {
    use clap::CommandFactory;
    let cmd = Cli::command();
    if lang == i18n::Lang::En {
        return cmd;
    }
    KO_HELP.iter().fold(
        cmd.about(tr!("Prompt compression/lint studio (opens the GUI without arguments)")),
        |c, (id, help)| c.mut_arg(*id, |a| a.help(*help)),
    )
}
/// Reads the command line. Looks at `--lang` first so the help and value errors are in that language (clap's
/// own text stays English).
fn parse_cli() -> Cli {
    use clap::FromArgMatches;
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let lang = i18n::lang_arg(&args).unwrap_or_default();
    i18n::set(lang);
    let matches = cli_command(lang).get_matches_from(args);
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// Language for the run mode. The GUI follows the user's saved choice; the terminal modes use English unless
/// `--lang` is given.
fn mode_lang(cli: &Cli) -> i18n::Lang {
    let terminal = cli.cli || cli.mcp || cli.run_graph.is_some();
    if terminal {
        cli.lang.unwrap_or_default()
    } else {
        i18n::startup_lang(cli.lang)
    }
}

/// In release builds, closes the console window that opens with the GUI (debug builds keep it for logs).
#[cfg(all(windows, not(debug_assertions)))]
fn detach_console() {
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    // SAFETY: a Win32 call without arguments; a failure is harmless.
    unsafe {
        FreeConsole();
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn detach_console() {}

/// Window icon (title bar, taskbar, Alt+Tab). build.rs embeds the exe file icon from assets/icon.ico.
fn window_icon() -> eframe::egui::ViewportBuilder {
    let vb = eframe::egui::ViewportBuilder::default();
    match eframe::icon_data::from_png_bytes(theme::ICON_PNG) {
        Ok(icon) => vb.with_icon(std::sync::Arc::new(icon)),
        Err(e) => {
            eprintln!("{}", trf!("failed to load window icon: {e}", e = e));
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
                eprintln!("{}", trf!("mcp error: {e}", e = e));
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
                eprintln!("{}", trf!("failed to start GUI: {e}", e = e));
                ExitCode::FAILURE
            }
        };
    }
    // A .kiro file passed as the positional argument (e.g. "Open with" in Explorer) opens as a blueprint.
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
            eprintln!("{}", trf!("failed to start GUI: {e}", e = e));
            ExitCode::FAILURE
        }
    }
}

/// Graph file to open with --canvas: the positional argument or --graph (clap rejects both together).
fn canvas_file(cli: &Cli) -> Option<PathBuf> {
    cli.file.clone().or_else(|| cli.graph.clone())
}

/// Upper bound of `--cli --sample-rows`: the same `MAX_SAMPLE_ROWS` as blueprints.
fn clamp_sample_rows(n: usize) -> usize {
    n.min(blueprint::MAX_SAMPLE_ROWS)
}

fn run_cli(cli: Cli) -> ExitCode {
    let bp = match &cli.blueprint {
        Some(p) => match blueprint::Blueprint::load(p) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("{}", trf!("failed to load blueprint: {e}", e = e));
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let mut constraints = cli.constraints;
    if let Some(p) = &cli.steering {
        // Detects UTF-8 (with or without BOM), UTF-16 BOM and CP949 like the GUI.
        match pipeline::read_text_auto(p) {
            Ok(md) => constraints.extend(pipeline::parse_steering_rules(&md)),
            Err(e) => {
                eprintln!("{}", trf!("failed to read steering file: {e}", e = e));
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
                        "warning: sample rows are capped at {n} ({} → {n})",
                        cli.sample_rows,
                        n = n
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
                eprintln!("{}", trf!("failed to load data: {e}", e = e));
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
            eprintln!("{}", trf!("failed to load graph: {e}", e = e));
            return ExitCode::FAILURE;
        }
    };
    let input_override = input.and_then(|t| {
        // Same node as the GUI test runner: the graph's saved test input node (test_node) if it is a Text node,
        // else the first Text node
        let is_text = |n: &&graph::Node| matches!(n.kind, NodeKind::Text { .. });
        let target = g
            .test_node
            .and_then(|id| g.node(id))
            .filter(is_text)
            .or_else(|| g.nodes.iter().find(is_text));
        if target.is_none() {
            eprintln!("{}", tr!("warning: no Text node to receive --input; ignored"));
        }
        target.map(|n| (n.id, t))
    });
    let opts = exec::RunOptions {
        allow_mcp,
        allow_custom_endpoint,
        input_override,
        // A Prompt node's relative data_path is relative to the folder of the graph file.
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
            // A failed run leaves no empty line on stdout; a successful empty result prints an empty line.
            if r.ok() || !r.final_text.is_empty() {
                println!("{}", r.final_text);
            }
            if r.ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("{}", trf!("run failed: {e}", e = e));
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
            "without --canvas the studio must not open"
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
            assert!(parse(&args).is_err(), "{other:?} --graph must not be silently ignored");
        }
    }

    #[test]
    fn graph_files_are_not_studio_files() {
        // With --run-graph a positional argument would be silently ignored, so it is rejected.
        assert!(parse(&["--run-graph", "a.json", "b.json"]).is_err());
        assert!(parse(&["--canvas", "--run-graph", "a.json"]).is_err());
    }

    #[test]
    fn input_may_start_with_a_hyphen() {
        let input = ko_fixture!("cli.hyphen_input");
        let cli = parse(&["--run-graph", "g.json", "--input", input]).unwrap();
        assert_eq!(cli.input.as_deref(), Some(input));
        assert!(parse(&["--input", "x"]).is_err(), "needs --run-graph");
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
    fn lang_flag_parses_and_terminal_modes_default_to_english() {
        use i18n::Lang;
        assert_eq!(parse(&["--cli", "--lang", "en"]).unwrap().lang, Some(Lang::En));
        assert_eq!(parse(&["--mcp", "--lang=ko"]).unwrap().lang, Some(Lang::Ko));
        assert!(parse(&["--cli", "--lang", "fr"]).is_err());
        // Terminal modes ignore the saved GUI setting: without --lang they are English
        for args in [["--cli"].as_slice(), &["--mcp"], &["--run-graph", "g.json"]] {
            assert_eq!(mode_lang(&parse(args).unwrap()), Lang::En, "{args:?}");
        }
        assert_eq!(mode_lang(&parse(&["--cli", "--lang", "ko"]).unwrap()), Lang::Ko);
        // In GUI mode an explicit --lang wins over the saved setting
        assert_eq!(mode_lang(&parse(&["--lang", "ko"]).unwrap()), Lang::Ko);
    }

    #[test]
    fn korean_help_covers_every_argument() {
        use clap::CommandFactory;
        let ids: Vec<String> = Cli::command()
            .get_arguments()
            .map(|a| a.get_id().to_string())
            .filter(|id| id != "help" && id != "version")
            .collect();
        for id in &ids {
            assert!(KO_HELP.iter().any(|(k, _)| k == id), "missing Korean help: {id}");
        }
        for (k, _) in KO_HELP {
            assert!(ids.iter().any(|id| id == k), "Korean help for an unknown argument: {k}");
        }
        // The Korean command can render help (a typo in mut_arg panics here); the English one has no Hangul
        let ko = cli_command(i18n::Lang::Ko).render_long_help().to_string();
        assert!(ko.contains(ko_text!("Terminal linter mode")));
        let en = cli_command(i18n::Lang::En).render_long_help().to_string();
        assert!(en.contains("Terminal linter mode") && !en.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)));
    }
}
