# Design

## Flow
Form / steering / data profile -> `pipeline::compile` -> RootPromptAst -> `linter::lint` (Visitor passes) -> `codegen::generate` -> `tokenizer::count`.

## Modules
- `ast/mod.rs`: RootPromptAst, DataSubAst, ConstraintNode, OutputSchemaAst, LintDiagnostic.
- `ast/linter.rs`: `AstVisitor` + `walk`; passes Filler, Dedup, Contradiction, Schema.
- `ast/codegen.rs`: Claude XML, GPT Markdown, Gemini sections.
- `profiler/csv_profiler.rs`: streaming Welford stats, reservoir sampling, z-score (>3) outliers.
- `app.rs`: egui 3-pane (left form, center AST X-ray + lint log, right output/benchmark). Workers send results over `mpsc`.
- `main.rs`: clap dual mode (GUI default, `--cli`, `--mcp`).

## Notes
- Contradiction detection is a heuristic: negation markers + Jaccard >= 0.6 on stemmed tokens. Korean tokens are cut to 2 chars.
- LLM preview is a local mock of the output schema; no LLM is called.
- Build cache is redirected to `C:/cargo-target/sugar-coat-studio` (`.cargo/config.toml`).
