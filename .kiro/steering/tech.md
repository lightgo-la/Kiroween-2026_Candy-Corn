# Tech

- Rust 2024 edition, rustc 1.98, `x86_64-pc-windows-msvc`.
- GUI: eframe/egui 0.37. `App::ui` (not update), `egui::Panel`, `egui::Scene`. `Id::new` is deprecated, so use `Id::unique` or `id_salt`.
- Korean font: `C:\Windows\Fonts\malgun.ttf` is registered as a fallback at startup.
- CLI: clap 4 (derive). Data: csv, serde_json (preserve_order), encoding_rs (UTF-8/CP949 auto-detection).
- Tokens: tiktoken-rs o200k_base. Every vendor uses the same approximation.
- File dialogs: rfd. LLM calls: ureq 3 (2 retries on 429/5xx/529, redirects off, timeout proportional to max_tokens, 120-900 s).
- Exe icon: build.rs + winresource (needs rc.exe from the Windows SDK; without it the build only warns).
- Build output: `C:/cargo-target/candy-corn` (`.cargo/config.toml`, avoids OneDrive sync conflicts).

## Rules
- Profiling, compiling and LLM calls run on worker threads; only the results go to the UI over mpsc.
- No blocking calls inside UI closures. File dialogs open at the end of the frame.
- Rust sources are English (comments, doc comments, string and char literals, tests); Korean appears only in `locales/` and `tests/fixtures/`.
  - UI text is written in English at the call site: `tr!("Loaded")` (fixed), `trf!("{n} items", n = n)` (formatted; named placeholders are passed explicitly), `trp!("Save")` (constant tables, resolved with `.get()`).
  - Korean UI text: `locales/ko.json` plus `locales/ko.<area>.json` fragments (English -> Korean). build.rs generates `ko_text!` from them, so a missing translation is a compile error.
  - Korean linguistic data (the linter's and the profiler's word lists): `locales/lexicon.ko*.json`, turned into constants in `crate::lexicon` by build.rs.
  - Korean test inputs: `tests/fixtures/ko*.json`, read with `ko_fixture!` (integration tests include `ko_fixture.rs` from `OUT_DIR`).
  - Tests change the language only for their own thread with `i18n::with_lang`, never with `i18n::set`.
- The compiled prompt (codegen output, profile stats) is model-facing, so it must be the same in every UI language.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --all -- --check` must pass.

## Commands
- `cargo test`: unit tests + QC (13,000 seeded random cases: compile 6,000, profiler 4,000, LLM reply check 2,000, English-mode compile 1,000; about 30 s) + 33 scenarios + 16 graph e2e tests + blueprint CLI
- `cargo test` also runs the source checks `no_hangul_in_rust_sources` (no Hangul in Rust sources; `I18N_ONLY=app.rs,canvas/` limits it to matching files) and `every_translation_is_used` (no unused Korean translations)
- When a core feature changes, add or update its tests too. The work is done only when the tests pass (the Stop hook checks).
- `cargo test --test qa -- --ignored`: QA (1,000 runs of the real binary, about 50 s)
- `cargo run -- --cli --data samples/sales.csv --constraint "..."`
- `cargo run -- --cli --blueprint plan.kiro`
- Release: `cargo build --release`, then copy `C:/cargo-target/candy-corn/release/Candy-Corn.exe` to the project folder
- While an agent is working, launch the GUI from the copied `Candy-Corn.exe`. If a GUI started with `cargo run` stays open, Windows locks the debug exe and the Stop hook's `cargo test` fails to link.
