# 🌽 Candy-Corn

> A prompt compiler for data analysis, written in Rust. Kiroween 2026 submission.

<img width="2527" height="1311" alt="Candy-Corn" src="https://github.com/user-attachments/assets/4171ba49-4529-422a-853a-668cc9aa94dd" />

| | |
|---|---|
| **Hackathon** | Kiroween 2026 |
| **Category** | TODO (e.g. Costume Contest / Frankenstein / Resurrection / Skeleton Crew) |
| **Team** | lightgo-la |
| **Repository** | https://github.com/lightgo-la/Kiroween-2026_Candy-Corn |
| **Demo video** | TODO |
| **License** | MIT. See [LICENSE](LICENSE) |

## Inspiration

TODO: Why a prompt compiler for data analysis? Describe the problem you wanted to solve.

## What it does

Candy-Corn takes a role, a list of constraints and a profile of a CSV/JSON data file, lints them, and writes a short structured prompt for the target model: XML for Claude, Markdown for GPT, delimited sections for Gemini.

One executable, `Candy-Corn.exe`, contains a desktop GUI (egui), a terminal linter, a stdio MCP server and a separate graph tool.

### Features

- **Data profiling**: CSV, TSV, JSON arrays and NDJSON are read in one streaming pass: row count, column types, null ratios, min/max/mean/std, sampled rows and z-score (> 3) outliers. Encodings are detected (UTF-8, UTF-8/UTF-16 BOM, CP949), the delimiter is sniffed (`,` `;` tab `|`), binary files are rejected, and ID-like columns are left out of outlier detection.
- **Lint passes**: filler words are compressed; duplicate constraints are removed and the later copy is kept; contradicting constraints are removed and the later instruction wins (uncertain pairs are kept with a warning); the output schema is checked (blank and duplicate fields are removed, no required fields is an ERROR). Every removal warning names the removed text.
- **Code generation**: Claude XML, GPT Markdown, Gemini sections. Text that comes from the data is sanitized so it cannot open a new prompt section.
- **Token report**: original vs. compressed token counts (tiktoken `o200k_base`, the same approximation for every vendor) and the estimated input-cost saving.
- **LLM run** (GUI): sends the compiled prompt to Anthropic, OpenAI or Gemini and checks that the reply follows the output format and contains the required fields. API keys are read only from `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `GEMINI_API_KEY`. The stop reason, token usage and latency are shown, and a reply cut off at the token limit is reported as cut off. Without a key the result is a mock reply built from the schema.
- **Blueprints**: all settings of the studio canvas (data path, sample rows, role, constraints, output format and fields, target, theme, LLM model and options, node layout) are saved as a `.kiro` JSON file. Five built-in blueprints are included, and the CLI reads the same files.
- **Steering rules**: the `- ` and `* ` bullets of `.kiro/steering/compiler-rules.md` are added as constraints when the GUI starts, or with `--steering` on the CLI.
- **Graph tool**: a node canvas that connects Text, Prompt, LLM, MCP and Output nodes, with saved test cases and a headless runner. MCP nodes and custom LLM endpoints run only when they are explicitly allowed.
- **Languages**: English (default) and Korean for the UI and all messages. The compiled prompt is the same in both languages.

## How we built it

- **Language and UI**: Rust (2024 edition), egui for the desktop GUI, clap for the command line.
- **Shared pipeline**: the GUI, the CLI and the MCP server all use one compile pipeline (`src/pipeline.rs`): profiler → AST → lint passes → code generation → token report.
- **Testing**: unit tests, 13,000 seeded random QC cases, source checks, 33 scenarios, 16 graph end-to-end tests, and a QA suite that runs the real binary 1,000 times.
- **Localization**: Rust sources are English. Korean text lives only in `locales/` and `tests/fixtures/`, and `build.rs` turns it into macros and constants, so a missing translation is a compile error.

### Development timeline

| Day | Date | Focus |
|---|---|---|
| 1 | Nov 16 | Architecture & Core Engine |
| 2 | Nov 17 | Async Worker & State Persistence |
| 3 | Nov 18 | GUI Layout |
| 4 | Nov 19 | Release |

## How we used Kiro

The `.kiro/` directory holds the steering docs, specs, agent and hooks used in this project.

- **Steering**: `.kiro/steering/compiler-rules.md` is also read by the product itself and its bullets become constraints.
- **Specs**: TODO (which features were built spec-first)
- **Agent hooks**: TODO
- **MCP**: Candy-Corn exposes a `compress_prompt` tool through `--mcp`. TODO: how it was used with Kiro
- **Vibe coding**: TODO

## Challenges we ran into

TODO

## Accomplishments we're proud of

- One executable covers GUI, CLI, MCP server and graph tool.
- Contradicting and duplicate constraints are detected and every removal names the removed text.
- Bilingual (English/Korean) UI and messages, enforced by the build and by tests.

## What we learned

TODO

## What's next

TODO

---

## Getting started

### Requirements

- Rust 2024 edition. Developed with rustc 1.98 on Windows (`x86_64-pc-windows-msvc`).
- `rc.exe` from the Windows SDK to embed the exe icon (without it the build only warns).
- `C:\Windows\Fonts\malgun.ttf` is used as the Korean font in the GUI.
- `.cargo/config.toml` puts the build output in `C:/cargo-target/candy-corn`.

### Build

```
cargo build --release
```

### Quick start

```
Candy-Corn.exe --cli --data samples/sales.csv --constraint "Detect outliers first" --constraint "Do not detect outliers"
Candy-Corn.exe --cli --lang ko --data samples/sales.csv --target gpt --format table
Candy-Corn.exe --cli --blueprint plan.kiro
```

The first command prints a Claude XML prompt and reports this on stderr:

```
[WARN] Removed contradicting instruction: "Detect outliers first" ↔ "Do not detect outliers" (kept the later one)
```

## Usage

### Modes

| Command | What it does |
|---|---|
| `Candy-Corn.exe [FILE]` | GUI. `FILE` is a CSV/JSON file to load; a `.kiro` file opens as a blueprint |
| `--blueprint FILE.kiro` | GUI with the blueprint applied; with `--cli`, compiles with it |
| `--cli` | Terminal linter: the prompt goes to stdout, lint diagnostics and token counts to stderr; exit code 1 on an ERROR diagnostic or a load error |
| `--mcp` | stdio JSON-RPC MCP server with the `compress_prompt` tool |
| `--canvas [GRAPH.json]` | Graph tool GUI (`--graph GRAPH.json` also opens a file) |
| `--run-graph GRAPH.json` | Runs a graph headless and prints the final output |

### Options

`--cli`:

| Option | Description |
|---|---|
| `--data FILE` | Data file to profile |
| `--role TEXT` | Role (default `Data analyst`) |
| `--constraint TEXT` | Constraint (repeatable) |
| `--steering FILE.md` | Steering file whose bullets become constraints |
| `--target claude\|gpt\|gemini` | Target model |
| `--format json\|table` | Output format |
| `--fields a,b,c` | Output fields (default `root_causes,outliers,recommendations`) |
| `--sample-rows N` | Sample rows (default 8, at most 50) |

With `--blueprint`, the role, target, format, fields and sample rows come from the blueprint. `--data` replaces its data path, and `--constraint`/`--steering` add constraints.

`--run-graph`:

| Option | Description |
|---|---|
| `--input TEXT` | Replaces the text of the test input node |
| `--allow-mcp` | Lets MCP nodes start local processes |
| `--allow-custom-endpoint` | Allows an LLM Base URL other than the provider default (https or localhost only; the API key is sent there) |

Other:

- `--theme 1-7` selects the GUI theme.
- `--lang en|ko` sets the UI and message language. Without it, the terminal modes (`--cli`, `--mcp`, `--run-graph`) use English, and the GUI uses the language saved in `%APPDATA%\Candy-Corn\settings.json` (English if there is none).
- `--help --lang ko` prints the Korean help.

## Testing

```
cargo test                                  # unit tests, QC (13,000 seeded random cases), source checks, 33 scenarios, 16 graph e2e tests
cargo test --test qa -- --ignored           # QA: 1,000 runs of the real binary
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

`cargo test` also fails if a Rust file contains Hangul (`no_hangul_in_rust_sources`) or a translation is no longer used (`every_translation_is_used`).

## Project layout

```
src/main.rs           command line (clap): GUI, --blueprint, --cli, --mcp, --canvas, --run-graph, --lang
src/i18n.rs           UI language: tr!/trf!/trp! macros, per-thread override for tests, settings file
src/lexicon.rs        Korean word lists for the linter and the profiler (generated from locales/)
src/app.rs            GUI studio: 3-pane layout, workers, LLM run, undo
src/studio_canvas.rs  blueprint canvas
src/blueprint.rs      .kiro format, built-in blueprints
src/pipeline.rs       compile pipeline shared by GUI, CLI and MCP; steering parser; LLM reply check
src/ast/              AST, linter passes, code generation
src/profiler/         CSV/TSV/JSON/NDJSON profiler
src/tokenizer.rs      token counts and cost
src/mcp_server.rs     stdio MCP server
src/canvas/           graph tool: execution, graph format, LLM client, MCP client, UI
src/qc.rs             randomized QC tests (test builds only)
build.rs              exe icon; generates ko_text!, the lexicon constants and ko_fixture!
locales/              Korean UI text (ko*.json) and Korean linguistic data (lexicon.ko*.json)
tests/                scenario, graph and QA tests; tests/fixtures/ holds the Korean test inputs
samples/              sales.csv, flow.canvas.json
.kiro/                steering docs, specs, agent and hooks
```

## License

MIT. See [LICENSE](LICENSE).
