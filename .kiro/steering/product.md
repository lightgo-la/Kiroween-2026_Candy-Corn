# Product: Sugar-Coat Studio

데이터 분석용 프롬프트 컴파일러. 역할, 제약, CSV/JSON 데이터 프로파일을 받아 린트(수식어 압축, 중복·모순 제거, 스키마 검증)한 뒤 타겟 모델에 맞는 짧은 프롬프트로 직렬화한다. Claude는 XML, GPT는 Markdown, Gemini는 구분자 섹션. 압축 전후 토큰 수와 절감액을 보여 준다.

## 실행 모드 (바이너리 하나)
- 인자 없음: GUI 스튜디오. 3-pane, 중앙은 노드 캔버스와 AST X-Ray 탭, 할로윈 테마 7종.
- `--cli`: 터미널 린터. ERROR 진단이 있으면 종료 코드 1.
- `--mcp`: stdio JSON-RPC MCP 서버, 도구 `compress_prompt`.
- `--canvas`, `--run-graph`: 프롬프트·LLM API·MCP 도구를 잇는 별도 그래프 도구 (`src/canvas/`).

## 데모 시나리오 (3분)
1. `cargo run -- samples/sales.csv`: 데이터 노드에 301 rows, amount 이상치 1건.
2. 캔버스에서 레시피 "매출 이상치 분석" 적용, 제약 "이상치 탐지하지 마" 추가.
3. Ctrl+Enter: 린트 로그에 모순 제거 경고, 토큰 절감률 표시.
4. 테마 06(안개)·타겟 GPT로 바꿔 다시 컴파일, 결과 복사·저장.
5. 터미널에서 `--cli` 실행, MCP `tools/list` 응답 확인.

## 범위 밖
- 스튜디오의 "LLM 실행 결과 미리보기"는 모의 응답(스키마 골격)이다. 실제 LLM 호출은 `--canvas`에만 있다.
