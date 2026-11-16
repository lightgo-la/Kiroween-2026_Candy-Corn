# Structure

```
src/
  main.rs              clap 진입점: GUI / --cli / --mcp / --canvas / --run-graph, 창 아이콘
  app.rs               스튜디오 GUI: 3-pane, 워커(mpsc), 프로젝트 저장/불러오기(.kiro JSON)
  studio_canvas.rs     스튜디오 노드 캔버스 (블록 -> RootPromptAST 컴파일러 -> 결과)
  theme.rs             디자인샘플 01~07 팔레트, 카드, 글로우 버튼, 안개, 호박 로고
  pipeline.rs          GUI/CLI/MCP 공용 컴파일 파이프라인, steering 불릿 파서
  ast/mod.rs           RootPromptAst와 서브 AST
  ast/linter.rs        Visitor 패스: 수식어, 중복, 모순, 스키마
  ast/codegen.rs       Claude XML / GPT Markdown / Gemini 섹션
  profiler/csv_profiler.rs  CSV/JSON/NDJSON 스트리밍 통계, 샘플링, z-score 이상치
  tokenizer.rs         토큰 수와 비용
  mcp_server.rs        stdio JSON-RPC MCP 서버
  canvas/              별도 그래프 도구 (exec, graph, llm, mcp_client, ui)
  qc.rs                QC 무작위 불변식 테스트 (test 빌드 전용)
tests/qa.rs            QA 종단 간 테스트 (기본 제외, --ignored)
assets/                icon.ico(exe), icon-256.png(창). 원본: pumpkin_premium.png
samples/               sales.csv, flow.canvas.json
build.rs               exe 아이콘 리소스
```

`.kiro/steering/compiler-rules.md`의 `- ` 불릿은 앱이 제약으로 직접 파싱한다. 이 파일들(product/tech/structure)은 파싱 대상이 아니다.
