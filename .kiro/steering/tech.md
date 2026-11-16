# Tech

- Rust 2024 edition, rustc 1.98, `x86_64-pc-windows-msvc`.
- GUI: eframe/egui 0.37. `App::ui`(update 아님), `egui::Panel`, `egui::Scene`. `Id::new`는 deprecated이므로 `Id::unique` 또는 `id_salt`를 쓴다.
- 한글 폰트: 시작 시 `C:\Windows\Fonts\malgun.ttf`를 fallback으로 등록한다.
- CLI: clap 4 (derive). 데이터: csv, serde_json(preserve_order), encoding_rs(UTF-8/CP949 자동 판별).
- 토큰: tiktoken-rs o200k_base. 모든 벤더에 같은 근사치를 쓴다.
- 파일 대화상자: rfd. 캔버스 LLM 호출: ureq 3.
- exe 아이콘: build.rs + winresource (Windows SDK의 rc.exe 필요, 없으면 경고만).
- 빌드 산출물: `C:/cargo-target/candy-corn` (`.cargo/config.toml`, OneDrive 동기화 충돌 방지).

## 규칙
- 프로파일링·컴파일·LLM 호출은 워커 스레드에서 돌리고 mpsc로 결과만 UI에 넘긴다.
- UI 클로저 안에서 블로킹 호출 금지. 파일 대화상자는 프레임 끝에서 연다.
- 주석과 UI 문구는 한국어.
- `cargo clippy --all-targets -- -D warnings`와 `cargo fmt --all -- --check`가 통과해야 한다.

## 명령
- `cargo test`: 단위 테스트 + QC(시드 고정 무작위 10,000건, 약 20초) + 시나리오 21개 + 블루프린트 CLI
- 핵심 기능을 바꾸면 그 기능의 테스트를 함께 추가·수정한다. 테스트가 통과해야 완료다 (Stop 훅이 검사).
- `cargo test --test qa -- --ignored`: QA(실제 바이너리 1,000회 실행, 약 50초)
- `cargo run -- --cli --data samples/sales.csv --constraint "..."`
- `cargo run -- --cli --blueprint plan.kiro`
- 배포: `cargo build --release` 후 `C:/cargo-target/candy-corn/release/Candy-Corn.exe`를 프로젝트 폴더로 복사
