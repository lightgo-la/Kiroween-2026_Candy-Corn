# Requirements (EARS)

1. When the user loads a CSV/JSON/NDJSON file, the system shall profile it in a worker thread and report rows, columns, null ratios and outliers without blocking the UI.
2. When the user presses Compile & Run, the system shall build a RootPromptAst from the form inputs and the loaded data profile.
3. While compiling, the system shall run lint passes: filler compression, duplicate removal, contradiction detection (later instruction wins), schema integrity.
4. When a contradiction or duplicate is removed, the system shall record a WARN diagnostic that names the removed text.
5. If the output schema has no required fields, then the system shall record an ERROR diagnostic and the CLI shall exit with a non-zero code.
6. When the target model is Claude, GPT or Gemini, the system shall serialize the AST as XML, Markdown or delimited sections respectively.
7. The system shall display original vs. compressed token counts (o200k_base approximation) and the estimated input-cost saving.
8. Where the binary is started with `--cli`, the system shall run the pipeline in the terminal; where started with `--mcp`, it shall serve the `compress_prompt` tool over stdio JSON-RPC.
9. The system shall render Korean text by registering a system Korean font at startup.
10. The system shall show its UI text and diagnostics in English by default; where the user selects Korean (the language picker or `--lang ko`), it shall show them in Korean, and the compiled prompt shall be the same in both languages.
