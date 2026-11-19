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

A data-analysis prompt is usually put together by hand: a role, rows pasted from a CSV, a list of rules that grows with every attempt, and a request for JSON. The pasted rows cost tokens and filler words add more. Rules written at different times start to repeat or contradict each other ("Detect outliers first", later "Do not detect outliers"), and the model has to guess which one counts. Each model also needs its own layout (XML tags for Claude, Markdown for GPT, delimited sections for Gemini), and nothing checks whether the reply kept the requested format.

We wanted to treat a prompt the way a compiler treats source code: parse it into an AST, run lint passes over it, and generate output for a target. The data goes in as a profile (types, null ratios, statistics, outliers and a few sample rows) instead of raw rows, every removed rule is reported by name, and the token counts before and after show what the compression saved.

## What it does

Candy-Corn takes a role, a list of constraints and a profile of a CSV/JSON data file, lints them, and writes a short structured prompt for the target model: XML for Claude, Markdown for GPT, delimited sections for Gemini.

One executable, `Candy-Corn.exe`, contains a desktop GUI (egui), a terminal linter, a stdio MCP server and a separate graph tool.

### Features

- **Data profiling**: CSV, TSV, JSON arrays and NDJSON are read in one streaming pass: row count, column types, null ratios, min/max/mean/std, sampled rows and z-score (> 3) outliers. Encodings are detected (UTF-8, UTF-8/UTF-16 BOM, CP949), the delimiter is sniffed (`,` `;` tab `|`), binary files are rejected, and ID-like columns are left out of outlier detection.
- **Lint passes**: filler words are compressed; duplicate constraints are removed and the later copy is kept; contradicting constraints are removed and the later instruction wins (uncertain pairs are kept with a warning); the output schema is checked (blank and duplicate fields are removed, no required fields is an ERROR). Every removal warning names the removed text.
- **Code generation**: Claude XML, GPT Markdown, Gemini sections. Text that comes from the data is sanitized so it cannot open a new prompt section.
- **Token report**: original vs. compressed token counts (tiktoken `o200k_base`, the same approximation for every vendor) and the estimated input-cost saving.
- **LLM run** (GUI): sends the compiled prompt to Anthropic, OpenAI or Gemini and checks that the reply follows the output format and contains the required fields. Keys come from the 🔑 API keys window (saved in Windows Credential Manager, never in blueprints or logs) or from `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and `GEMINI_API_KEY`; a saved key wins. The stop reason, token usage and latency are shown, and a reply cut off at the token limit is reported as cut off. Without a key the result is a mock reply built from the schema. In 🔑 API keys, each provider also has an optional endpoint (a proxy or a local OpenAI-compatible server; https, or http for localhost) saved in `settings.json`, and a "Test connection" button. See [API keys and endpoints](#api-keys-and-endpoints).
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

- **Steering**: `.kiro/steering/compiler-rules.md` is also read by the product itself and its bullets become constraints. `product.md` (features and the 3-minute demo), `tech.md` (stack, threading and i18n rules, commands) and `structure.md` (module map) give agents the project context, and the worker briefs tell each agent to read them first.
- **Specs**: The first prompt was a design brief: the folder layout, a wireframe of the 3-pane studio and the AST types. Kiro wrote it down in `.kiro/specs/`: nine EARS requirements in `requirements.md` (profiling, the compile pipeline, the lint passes, per-target code generation, the token report, the CLI/MCP modes, the Korean font), the data flow and modules in `design.md`, and the task list in `tasks.md`. Built spec-first: the compile pipeline, the CLI/MCP modes and the studio layout from the brief, then the LLM run and undo/redo, which were the two open tasks of the first commit and became the next commit. Blueprints, the bilingual UI and the graph tool upgrades started as chat requests and were added to the specs afterwards (requirement 10, rounds 5–7 in `tasks.md`). The open items in `tasks.md` are the plan for the next round.
- **Agent hooks**: `rust-quality.json` runs `cargo fmt` and `cargo clippy --all-targets -- -D warnings` after every `.rs` save. At the end of each agent turn (Stop) it runs clippy and the full `cargo test`, and a failure goes back to the agent, which keeps fixing until both pass. `auto-lint.json` runs when the AST code or `compiler-rules.md` changes: `cargo test` and the product's own CLI linter with the steering rules on `samples/sales.csv`, which fails on an ERROR diagnostic. The first hook file was written in the Kiro IDE format. The `introspect` agent checked the Kiro docs, and the hooks were rewritten in the CLI's `.kiro/hooks/*.json` format, with the checks the agent must act on at Stop, because a failing PostFileSave hook in the CLI only shows a warning.
- **MCP**: `Candy-Corn.exe --mcp` is a stdio JSON-RPC MCP server with one tool, `compress_prompt` (role, constraints, target, format, fields, and an optional `data_path` and `sample_rows`). It returns the compiled prompt, the lint log and the token counts, so a Kiro agent can compile a data-analysis prompt from the chat; [MCP server in Kiro](#mcp-server-in-kiro) has the config. The server was tested by piping JSON-RPC lines into `--mcp`: 11 unit tests in `src/mcp_server.rs`, plus scenario and QA tests. The graph tool is also an MCP client: an MCP node starts a local MCP server and calls one of its tools, only with `--allow-mcp`.
- **Vibe coding**: Everything was built in Kiro CLI sessions in autopilot mode over two days.
  - Day 1: the baseline (41 files, about 12,000 lines) came from the design brief in one session. Separate sessions at the same time made seven Halloween design samples (now the themes), the pumpkin icon and the graph tool, which was then merged in. Later prompts were mostly a goal plus a check, for example to run 10,000 QC cases and 1,000 QA runs and fix whatever failed, or to keep fixing until the reviewer approved.
  - Review loop: the `semantic_reviewer` agent reviewed each git diff. Rounds 1–3 came back NEEDS_CHANGES, with findings such as two undo stacks over the same text field and a sample-row reload racing an in-flight load. Round 4 was approved.
  - Day 2, rounds 5–7: three reviewer agents (core, studio, graph canvas) ran in parallel, then four worker agents fixed and upgraded their areas, each in its own git worktree and branch with a list of files it owned. The main session merged the branches and ran an integration review and a fix pass. Round 6 (Korean/English UI, three workers) and round 7 (English source code, four workers) followed the same pattern, with one written brief of hard rules for all workers.
  - The `introspect` agent answered questions about Kiro itself, such as the hook format.

## Challenges we ran into

- **Removing the right rule.** The first version of the lexer-based contradiction rule (round 5) removed compatible pairs such as "Use the median instead of the mean" and "Do not use the mean": in a review of 60 realistic pairs, 7 lost a rule. Now a pair is removed only when the contradiction is certain. When the positive rule has an exception word (instead, except) or the two rules differ only by a descriptive antonym (increase/decrease), both stay, with a "Possible contradiction (kept both)" warning.
- **Data that reads like instructions.** A CSV header or cell can close an XML tag or start a Markdown heading or a Gemini section. A review found that a zero-width space before `=== OUTPUT ===` in a sample row got past the guard and opened a fake Gemini section ("Reply only in French"). The guards now look past invisible characters and list markers, and all data-derived text is sanitized: control characters, a CSV-quoted sample header, code fences longer than any backtick run inside, and a `\` before any line that would open a section or heading.
- **Messy data files.** CSV exports from Korean Excel are often CP949, and a large file can be plain ASCII for the first 64 KiB with Korean text only after that. Other files are UTF-16, use `;`, tab or `|`, or are binary files with a `.csv` name. The profiler follows a BOM if there is one, otherwise decides between UTF-8 and CP949 from the 64 KiB after the first non-ASCII byte, sniffs the delimiter and rejects binary input.
- **Cut-off replies.** A reply that stopped at the token limit used to be reported as a format violation. The LLM client now reads each vendor's stop reason (`max_tokens`, `length`, `MAX_TOKENS`) and reports such a reply as cut off, with a "Retry with 2× limit" button. The HTTP timeout scales with `max_tokens`, because a review found that a doubled limit could outrun a fixed timeout.
- **Stale results in an immediate-mode GUI.** egui redraws every frame, so profiling, compiling and LLM calls run on worker threads and send results back over mpsc with a generation number, and a result from an older run is dropped. The review rounds kept finding races of this kind: a sample-row reload racing an in-flight load, an LLM run started during a compile, runs that crossed a blueprint switch.
- **Parallel agents on one codebase.** Workers editing the same files would conflict, and all of them needed new Korean translations in `locales/ko.json`. Each worker got its own git worktree, branch and build directory, a list of files it owned, and its own translation fragment (`locales/ko.<area>.json`, merged by `build.rs`), which was folded into `ko.json` after the merge.
- **Hooks outside the project folder.** The long sessions were started from the home folder, so the project's hooks did not fire. The agents ran the same gate themselves (fmt, clippy with `-D warnings`, `cargo test`), and the worker briefs required it before every commit.

## Accomplishments we're proud of

- One executable covers GUI, CLI, MCP server and graph tool.
- Contradicting and duplicate constraints are detected and every removal names the removed text.
- Bilingual (English/Korean) UI and messages, enforced by the build and by tests.

## What we learned

- A linter that deletes instructions has to be conservative. A warning that keeps both rules costs the user a second look; a silently dropped rule costs a wrong analysis.
- Text from a data file is untrusted input to the model and needs escaping, like user input in a SQL query.
- Agents finish reliably when "done" is a command that can fail: fmt, clippy with `-D warnings`, and a test suite large enough to matter (13,000 seeded QC cases, 33 scenarios, 16 graph end-to-end tests, 1,000 QA runs). Tests did not catch everything, though: the review rounds found state bugs (stale results, two undo stacks over one field) that no test covered yet.
- Parallel agents work when each one owns separate files in its own worktree and all of them follow one written brief. The merge and the integration review stay with one session.
- Rules that matter belong in the build. A missing Korean translation is a compile error and Hangul in a `.rs` file fails `cargo test`, so neither rule depends on an agent remembering it.
- Check Kiro's config formats in the docs instead of guessing them, and start Kiro in the project folder so the project's hooks apply.

## What's next

- Streaming LLM output with a real cancel for Anthropic, OpenAI and Gemini.
- MCP client: tool discovery (`tools/list`), a pool that reuses one server process per command, and a Windows Job Object so MCP servers started by Candy-Corn exit when it exits or crashes.
- Graph tool: undo/redo and parallel branches.
- A command palette (Ctrl+K) with Korean initial-consonant search, plus a minimap, alignment and snapping in both canvases.
- MCP server: accept `data_path` only for data files inside allowed folders (`--allow-path`). Blueprints: atomic saves.

---

## Getting started

### Install

Download `Candy-Corn-Setup-0.1.0.exe` from the [Releases](https://github.com/lightgo-la/Kiroween-2026_Candy-Corn/releases) page, or build it with [Build the installer](#build-the-installer). It installs Candy-Corn on 64-bit Windows 10 or 11 for the current user, without an admin prompt.

- Files go to `%LOCALAPPDATA%\Programs\Candy-Corn`: `Candy-Corn.exe`, `README.md`, `LICENSE.txt` and `samples\`.
- Start Menu shortcuts open the studio, the graph tool (`--canvas`) and the samples folder. A desktop shortcut is optional.
- The wizard is in English or Korean, following the Windows display language.
- `/ALLUSERS` installs for all users into Program Files (needs admin); `/VERYSILENT` installs without the wizard.
- Uninstall from Settings > Apps. Your settings in `%APPDATA%\Candy-Corn` are kept.

The installer is not code-signed, so SmartScreen may warn before it runs (More info > Run anyway). The installed exe links the C runtime statically, so it does not need the Visual C++ Redistributable. To build the installer yourself, see [Build the installer](#build-the-installer).

### Requirements

- Rust 2024 edition. Developed with rustc 1.98 on Windows (`x86_64-pc-windows-msvc`).
- `rc.exe` from the Windows SDK to embed the exe icon (without it the build only warns).
- `C:\Windows\Fonts\malgun.ttf` is used as the Korean font in the GUI.
- `.cargo/config.toml` puts the build output in `C:/cargo-target/candy-corn`.
- Inno Setup 6 to build the installer (`winget install JRSoftware.InnoSetup`).

### Build

```
cargo build --release
```

### Build the installer

```
.\installer\build.ps1
```

The script builds a release exe with the C runtime linked statically, in its own target directory (`C:/cargo-target/candy-corn/installer`, so the regular build cache stays valid). It checks that the exe does not import `VCRUNTIME140.dll`, then compiles `installer/candy-corn.iss` into `dist\Candy-Corn-Setup-<version>.exe`, with the version from `Cargo.toml`. `-SkipBuild` packages the last build again.

### Quick start

After installing, run these in `%LOCALAPPDATA%\Programs\Candy-Corn`.

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

### Studio (GUI)

1. Start `Candy-Corn.exe` (or the Start Menu shortcut). The studio has three panes: inputs on the left, the canvas and X-Ray in the center, the compiled prompt and the LLM run on the right.
2. Click **🔑 API keys** at the top of the left pane and paste a key for the provider you want to use (optional; without a key the LLM run shows a mock reply).
3. Load a CSV/JSON file with **📂 Load CSV/JSON** or drop it on the window, then write the role, constraints and output fields.
4. Press Ctrl+Enter to compile, read the lint log and the token report, and run the prompt against the model from the right pane. VIII (LLM run result) shows the required fields as ✔/⚠/✖ chips, input/output token bars and the share of the output limit used, the reply as a JSON tree with tables or a Markdown table grid (📊 Structured) or as text (📝 Text), and a chart and table of the last 12 runs. **Save reply…** writes the reply to a file, and **↗ Separate window** opens VIII in its own window (**↙ Back to the pane** or closing the window puts it back).
5. Save everything as a `.kiro` blueprint with Ctrl+S.

Shortcuts:

| Keys | Action |
|---|---|
| Ctrl+Enter | Compile |
| Ctrl+S / Ctrl+Shift+S | Save blueprint / Save as |
| Ctrl+O | Open blueprint |
| Ctrl+Z / Ctrl+Y (Ctrl+Shift+Z) | Undo / Redo |

Themes (`--theme 1-7` or the Theme picker): 01 Classic Halloween, 02 Jack-o'-Lantern Glow, 03 Pumpkin Patch Light, 04 Premium Noir (default), 05 Premium Velvet, 06 Misty Graveyard, 07 Misty Haunted Manor. The fog effect of 06 and 07 can be turned off.

### API keys and endpoints

The **🔑 API keys** window opens from the top of the left pane, the blueprint settings node, the LLM node and the graph tool's top bar.

- **Key**: each provider (Anthropic, OpenAI, Gemini) has its own card. Paste the key and press Save or Enter. It is saved in Windows Credential Manager as `Candy-Corn/<VARIABLE>` for the current Windows user, and only its last 4 characters are shown. A saved key is used before the environment variable. `--run-graph` reads only the environment.
- **Endpoint (optional)**: a custom base URL, for example a proxy or a local OpenAI-compatible server. It must be https (http only for localhost, 127.0.0.1 and [::1]), because the key is sent there. Leave it empty for the provider's own address. It is saved under `base_urls` in `settings.json`, not in blueprints, and is used by the studio's LLM run.
- **Model**: each card has a model picker for Test connection.
- **Test connection**: sends a 16-token `ping` with the saved key and the picked model to the address in the field and shows whether it worked.

### Models

Every model field (the LLM node, the LLM run result and the graph tool's LLM node) has a **▾** button that lists the latest models of the provider; any other model ID can still be typed. Defaults are in bold.

| Provider | Models |
|---|---|
| Anthropic | `claude-fable-5-2`, `claude-opus-5-5`, **`claude-sonnet-5-5`**, `claude-haiku-5-5` |
| OpenAI | `gpt-6.2-astra`, **`gpt-6.2-sol`**, `gpt-6.1-terra`, `gpt-6-luna` |
| Gemini | `gemini-4-carbon`, `gemini-4-argon`, **`gemini-4-neon`**, `gemini-4-hydrogen` |

The list comes from the providers' model pages (November 2026) and lives in `Provider::models` in `src/canvas/graph.rs`. Blueprints keep the model they were saved with.

### Settings file

`%APPDATA%\Candy-Corn\settings.json` holds the UI language (`lang`) and the custom endpoints (`base_urls`). The `CANDY_CORN_SETTINGS` environment variable points to a different file, and an empty value turns the settings file off. API keys are never stored there.

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

### MCP server in Kiro

To call `compress_prompt` from Kiro, add the server to `.kiro/settings/mcp.json` in a workspace, or to `~/.kiro/settings/mcp.json` for all workspaces:

```json
{
  "mcpServers": {
    "candy-corn": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\Programs\\Candy-Corn\\Candy-Corn.exe",
      "args": ["--mcp"]
    }
  }
}
```

The path is the installer's default. With a source build, use the path of your `Candy-Corn.exe`.

## Troubleshooting

- **SmartScreen warning**: the installer is not code-signed. Choose More info > Run anyway.
- **Korean text shows as boxes**: the GUI needs `malgun.ttf`, `NanumGothic.ttf` or `gulim.ttc` in `C:\Windows\Fonts`.
- **`No ... API key`**: add a key in 🔑 API keys or set the environment variable, then use Test connection.
- **Custom endpoint rejected**: use https, or http only for localhost.
- **Build fails to link during development**: a GUI started with `cargo run` locks the debug exe; close it first.

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
installer/            Windows installer: Inno Setup script (candy-corn.iss) and build script (build.ps1)
.kiro/                steering docs, specs, agent and hooks
```

## License

MIT. See [LICENSE](LICENSE).
