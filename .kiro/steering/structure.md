# Structure

```
src/
  main.rs              clap 진입점: GUI / --blueprint / --cli / --mcp / --canvas / --run-graph, --lang(영어 도움말 포함), 창 아이콘
i18n.rs              언어(한국어/영어): tr!/trf! 매크로, Tr 상수 쌍, 전역 언어 + 테스트용 스레드 덮어쓰기, 설정 파일(%APPDATA%\Candy-Corn\settings.json), 번역 누락 검사
  app.rs               GUI: 3-pane, 워커(mpsc, 세대 번호로 오래된 결과 무시), 블루프린트 적용, LLM 실행, 실행 취소
  blueprint.rs         블루프린트(.kiro, v3: llm_options) 형식, 좌표 정리, 데이터 경로 해석, 내장 블루프린트 5종
  history.rs           디바운스 실행 취소/다시 실행 기록
  studio_canvas.rs     블루프린트 캔버스 (설정·데이터·역할·제약·출력 -> 컴파일러 -> 결과 -> LLM)
  theme.rs             디자인샘플 01~07 팔레트, 카드, 글로우 버튼, 안개, 앱 아이콘
  pipeline.rs          GUI/CLI/MCP 공용 컴파일 파이프라인, steering 불릿 파서(코드 펜스 제외), LLM 응답 검사(check_reply), 원본 토큰 캐시, 타겟 비교
  ast/mod.rs           RootPromptAst와 서브 AST
  ast/linter.rs        Visitor 패스: 수식어, 중복, 모순, 스키마
  ast/codegen.rs       Claude XML / GPT Markdown / Gemini 섹션
  profiler/csv_profiler.rs  CSV/JSON/NDJSON 스트리밍 통계, 샘플링, z-score 이상치
  tokenizer.rs         토큰 수와 비용
  mcp_server.rs        stdio JSON-RPC MCP 서버
  canvas/              별도 그래프 도구 (exec, graph, llm, mcp_client, ui). llm.rs는 GUI LLM 실행에도 쓴다
  qc.rs                QC 무작위 불변식 테스트 13,000건: 컴파일 6,000 + 프로파일러 4,000 + LLM 응답 검사 2,000 + 영어 모드 컴파일 1,000 (test 빌드 전용)
tests/scenarios.rs     시나리오 33개: 입력 준비 -> 실행 파일 실행 -> 출력 검증 (cargo test에 포함, s30~s33은 --lang en)
tests/graph.rs         --run-graph 종단 테스트 16개 (자리표시자 입력, 신뢰 게이트, test_node, --lang en 등)
tests/qa.rs            QA 1,000회 실행(--ignored) + 블루프린트 CLI 테스트
assets/                icon.ico(exe), icon-256.png(창·헤더). 원본: pumpkin_premium.png
samples/               sales.csv, flow.canvas.json(--canvas용 그래프, 테스트 케이스 2개 포함)
build.rs               exe 아이콘·버전 리소스
```

`.kiro/steering/compiler-rules.md`의 `- ` 불릿은 앱이 제약으로 직접 파싱한다. 이 파일들(product/tech/structure)은 파싱 대상이 아니다.
