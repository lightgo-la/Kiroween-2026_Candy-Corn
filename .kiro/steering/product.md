# Product: Candy-Corn

A prompt compiler for data analysis. It takes a role, constraints and a CSV/JSON data profile, lints them (filler compression, duplicate and contradiction removal, schema validation) and serializes a short prompt for the target model: XML for Claude, Markdown for GPT, delimited sections for Gemini. It shows the token counts before and after compression and the saving, and with an API key it runs the prompt on a real LLM and checks that the reply kept the output format and the required fields.

## Language
- English (default) and Korean. The studio switches language in the top bar and in the settings node, the canvas in the top bar's Language picker; the choice is saved to %APPDATA%\Candy-Corn\settings.json. Switching changes the UI text at once, and if the result is current it recompiles so the lint messages are in the new language too (the LLM result is kept when the prompt is unchanged). Text the user wrote (role, constraints, node text) is not translated. New default content (starting role and constraints, built-in blueprints, constraint templates, new Prompt nodes) is created in the current language.
- `--lang en|ko`: without it, the GUI uses the saved setting (English if there is none) and --cli/--mcp/--run-graph use English (so script output does not depend on the settings). `--help --lang ko` prints the Korean help.
- The compiled prompt is the same in every language.

## Run modes (one executable: Candy-Corn.exe)
- No arguments: GUI. 3-pane layout; the center holds the blueprint canvas and the AST X-Ray tab; 7 Halloween themes; undo (Ctrl+Z).
- `--blueprint file.kiro`: opens the GUI with the blueprint (all settings) applied. Passing the `.kiro` file as the only argument also works.
- `--cli [--blueprint file]`: terminal linter. Exit code 1 when there is an ERROR diagnostic.
- `--mcp`: stdio JSON-RPC MCP server with the `compress_prompt` tool.
- `--canvas [graph.json]`, `--run-graph graph.json`: a separate graph tool that connects prompts, LLM APIs and MCP tools (`src/canvas/`). Zoom, fit to screen, auto layout, Ctrl+D duplicate, ■ stop, saved test cases. MCP nodes run only with `--allow-mcp` (a checkbox in the GUI), an LLM Base URL other than the default address only with `--allow-custom-endpoint` (a checkbox in the GUI), and `key_env` accepts only `*_API_KEY` names.

## Blueprints
The values of all canvas nodes are the blueprint: data path, sample rows, role, constraints, output format and fields, target, theme, LLM model, LLM options (max_tokens, temperature, system prompt; v3) and node layout. Ctrl+S saves, Ctrl+O opens; unsaved changes are marked with ●, and the app asks before opening another blueprint or closing the window. Blueprints are saved and opened as `.kiro` (JSON); the 5 built-in blueprints (🧩 menu) and the CLI `--blueprint` use the same format. Data inside the blueprint's folder (or a subfolder) is saved as a relative path, so the folder can be moved as a whole; data anywhere else is saved as an absolute path. When the GUI opens a blueprint it appends the working folder's steering rules that the blueprint does not have yet, and saving puts those rules (including disabled ones) into the blueprint. Use the CLI `--steering` only with blueprints that contain no rules.

## LLM run
- LLM run: shows the stop reason (stop_reason), token usage and latency. A reply cut off at the output token limit is reported as cut off, not as a format violation (it is not a pass; "Retry with 2× limit"). ■ cancels a running call.

## Demo scenario (3 minutes)
1. `Candy-Corn.exe samples/sales.csv`: the data node shows 301 rows and 1 outlier in amount.
2. 🧩 Blueprints → apply "Sales Outlier Analysis", then add the constraint "Do not detect outliers".
3. Ctrl+Enter: the lint log shows the contradiction-removal warning (`Removed contradicting instruction: "Detect outliers first" ↔ "Do not detect outliers" (kept the later one)`) and the token saving. Ctrl+Z undoes the added constraint.
4. Switch to the blueprint "Data Quality Check" (GPT · Table · Misty Graveyard theme), compile again, then "Save blueprint".
5. In a terminal, `Candy-Corn.exe --cli --blueprint saved-file.kiro` prints the same prompt; check the MCP `tools/list` response.

## Out of scope
- Without an API key the LLM run only shows the schema skeleton (a mock reply). Keys are read only from environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY).
