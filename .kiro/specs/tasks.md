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
- [ ] Report LLM stop/finish reason (truncated replies currently show as format errors)
- [ ] Markdown table check without leading pipes
