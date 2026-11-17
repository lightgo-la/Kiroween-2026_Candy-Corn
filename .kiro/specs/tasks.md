# Tasks

- [x] AST types and constraint classification
- [x] Linter passes (filler, dedup, contradiction incl. antonyms, schema) with unit tests
- [x] Codegen for Claude / GPT / Gemini (XML/Markdown escaping)
- [x] CSV/JSON streaming profiler (UTF-8/CP949, JSON array streaming, ID columns excluded from outliers)
- [x] Tokenizer and cost calculator
- [x] egui 3-pane UI with Korean font and mpsc workers (generation counters, worker panic handling)
- [x] Blueprint canvas: settings, data, role, constraints, output, compiler, result, LLM nodes
- [x] Blueprint files (.kiro): save/open, 5 built-in blueprints, portable data paths, CLI `--blueprint`
- [x] 7 themes from 디자인샘플
- [x] Undo/redo (Ctrl+Z / Ctrl+Shift+Z) for role, constraints, output, target
- [x] Real LLM run with output format / required field check (API key from env only)
- [x] CLI (`--cli`) and MCP (`--mcp`) modes, MCP batch requests
- [x] Candy-Corn name, exe icon and window icon
- [x] Tests: unit, QC 10,000 randomized cases, 21 scenario tests, QA 1,000 runs
- [x] Report LLM stop/finish reason (truncated replies currently show as format errors)
- [x] Markdown table check without leading pipes

## Round 5 (full review: core, studio, graph canvas, I/O)
- [x] Linter: word-boundary negations, Korean stemming, certain-only contradiction removal, keep-last dedup, removal WARNs name text, `LintDiagnostic.indices`
- [x] Codegen: data-derived prompt-structure injection blocked for all targets
- [x] Profiler: BOM-first encoding, CP949 after 64 KiB, UTF-16, delimiter sniffing, binary rejection, stable empty JSON keys, ID patterns
- [x] MCP server: -32700/-32600/-32602 handling, argument validation, version negotiation; MCP client: lossy UTF-8, ping, PATHEXT, process-tree kill
- [x] Studio: saved-coordinate hang, drop routing, layout fixes, stale key, LLM options (max_tokens/temperature/system, blueprint v3), cancel, 2x retry, usage/latency, Ctrl+S/Ctrl+O, unsaved marker and confirm dialog, idle repaint, target comparison
- [x] Graph canvas: input placeholder, key/endpoint trust gate (`--allow-custom-endpoint`), run generation + cancel, Scene zoom/fit/auto-layout, duplicate, saved test cases, per-node notes
- [x] LLM client: stop reasons, usage, retries, redirects off, timeout scaled by max_tokens
- [x] Tests: QC 12,000 (compile 6,000 + profiler 4,000 + response check 2,000), 29 scenarios, 15 graph e2e, QA 1,000

## Open
- [ ] Windows Job Object for MCP child processes (survive a GUI crash today)
- [ ] Graph canvas: undo/redo, parallel branches
