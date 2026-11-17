# Tech

- Rust 2024 edition, rustc 1.98, `x86_64-pc-windows-msvc`.
- GUI: eframe/egui 0.37. `App::ui`(update 아님), `egui::Panel`, `egui::Scene`. `Id::new`는 deprecated이므로 `Id::unique` 또는 `id_salt`를 쓴다.
- 한글 폰트: 시작 시 `C:\Windows\Fonts\malgun.ttf`를 fallback으로 등록한다.
- CLI: clap 4 (derive). 데이터: csv, serde_json(preserve_order), encoding_rs(UTF-8/CP949 자동 판별).
- 토큰: tiktoken-rs o200k_base. 모든 벤더에 같은 근사치를 쓴다.
- 파일 대화상자: rfd. LLM 호출: ureq 3 (429/5xx/529 재시도 2회, 리디렉션 끔, 제한 시간은 max_tokens에 비례 120~900초).
- exe 아이콘: build.rs + winresource (Windows SDK의 rc.exe 필요, 없으면 경고만).
- 빌드 산출물: `C:/cargo-target/candy-corn` (`.cargo/config.toml`, OneDrive 동기화 충돌 방지).

## 규칙
- 프로파일링·컴파일·LLM 호출은 워커 스레드에서 돌리고 mpsc로 결과만 UI에 넘긴다.
- UI 클로저 안에서 블로킹 호출 금지. 파일 대화상자는 프레임 끝에서 연다.
- 주석은 한국어. 사용자에게 보이는 문구는 `tr!("한국어", "English")`(고정) / `trf!("{n}개", "{n} items")`(서식) / 상수 표는 `i18n::Tr(..)`로 두 언어를 함께 쓴다. 번역 대상이 아닌 한국어 데이터(린터 어휘 등)는 `// i18n-data`로 표시한다. `cargo test`의 `all_user_facing_korean_is_translated`가 누락을 잡는다.
- 컴파일된 프롬프트(codegen 출력, 프로파일 통계)는 모델용이라 UI 언어와 관계없이 같아야 한다.
- `cargo clippy --all-targets -- -D warnings`와 `cargo fmt --all -- --check`가 통과해야 한다.

## 명령
- `cargo test`: 단위 테스트 + QC(시드 고정 무작위 13,000건: 컴파일 6,000·프로파일러 4,000·LLM 응답 검사 2,000·영어 모드 컴파일 1,000, 약 30초) + 번역 누락 검사 + 시나리오 33개 + 그래프 e2e 16개 + 블루프린트 CLI
- 핵심 기능을 바꾸면 그 기능의 테스트를 함께 추가·수정한다. 테스트가 통과해야 완료다 (Stop 훅이 검사).
- `cargo test --test qa -- --ignored`: QA(실제 바이너리 1,000회 실행, 약 50초)
- `cargo run -- --cli --data samples/sales.csv --constraint "..."`
- `cargo run -- --cli --blueprint plan.kiro`
- 배포: `cargo build --release` 후 `C:/cargo-target/candy-corn/release/Candy-Corn.exe`를 프로젝트 폴더로 복사
- 에이전트 작업 중 GUI는 복사한 `Candy-Corn.exe`로 띄운다. `cargo run`으로 띄운 GUI가 열려 있으면 Windows가 debug exe를 잠가 Stop 훅의 `cargo test`가 링크에 실패한다.
