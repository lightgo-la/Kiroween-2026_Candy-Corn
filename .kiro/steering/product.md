# Product: Candy-Corn

데이터 분석용 프롬프트 컴파일러. 역할, 제약, CSV/JSON 데이터 프로파일을 받아 린트(수식어 압축, 중복·모순 제거, 스키마 검증)한 뒤 타겟 모델에 맞는 짧은 프롬프트로 직렬화한다. Claude는 XML, GPT는 Markdown, Gemini는 구분자 섹션. 압축 전후 토큰 수와 절감액을 보여 주고, API 키가 있으면 실제 LLM으로 실행해 응답이 출력 형식·필수 필드를 지켰는지 검사한다.

## 실행 모드 (실행 파일 하나: Candy-Corn.exe)
- 인자 없음: GUI. 3-pane, 중앙은 블루프린트 캔버스와 AST X-Ray 탭, 할로윈 테마 7종, 실행 취소(Ctrl+Z).
- `--blueprint 파일.kiro`: 블루프린트(전체 설정)를 적용해 GUI를 연다. `.kiro` 파일을 그냥 넘겨도 된다.
- `--cli [--blueprint 파일]`: 터미널 린터. ERROR 진단이 있으면 종료 코드 1.
- `--mcp`: stdio JSON-RPC MCP 서버, 도구 `compress_prompt`.
- `--canvas [그래프.json]`, `--run-graph 그래프.json`: 프롬프트·LLM API·MCP 도구를 잇는 별도 그래프 도구 (`src/canvas/`). 확대·화면 맞춤·자동 정렬, Ctrl+D 복제, ■ 중지, 테스트 케이스 저장. MCP 노드는 `--allow-mcp`(GUI 체크), 기본 주소가 아닌 LLM Base URL은 `--allow-custom-endpoint`(GUI 체크)가 있어야 실행되고, `key_env`는 `*_API_KEY` 이름만 허용한다.

## 블루프린트
캔버스의 모든 노드 값이 곧 블루프린트다: 데이터 경로, 샘플 행 수, 역할, 제약, 출력 포맷·필드, 타겟, 테마, LLM 모델, LLM 옵션(max_tokens·온도·시스템 프롬프트, v3), 노드 배치. Ctrl+S 저장, Ctrl+O 열기, 저장하지 않은 변경은 ●로 표시하고 다른 블루프린트를 열거나 창을 닫기 전에 묻는다. `.kiro`(JSON)로 저장·열기, 내장 블루프린트 5종(🧩 메뉴), CLI `--blueprint`가 같은 형식을 쓴다. 데이터가 블루프린트 폴더(또는 하위 폴더) 안에 있으면 상대 경로로 저장되어 폴더째 옮겨도 열리고, 다른 곳에 있으면 절대 경로로 저장된다. GUI는 블루프린트를 열 때 작업 폴더의 steering 규칙 중 블루프린트에 없는 것을 덧붙이고, 저장하면 그 규칙(꺼 둔 상태 포함)이 블루프린트에 들어간다. CLI의 `--steering`은 규칙이 들어 있지 않은 블루프린트에만 쓴다.

## LLM 실행
- LLM 실행: 종료 이유(stop_reason)·토큰 사용량·지연 시간을 보여 주고, 출력 토큰 한도에서 잘린 응답은 형식 위반이 아니라 잘림으로 알린다(통과 아님, "한도 2배로 다시 실행"). 실행 중 ■ 취소.

## 데모 시나리오 (3분)
1. `Candy-Corn.exe samples/sales.csv`: 데이터 노드에 301 rows, amount 이상치 1건.
2. 🧩 블루프린트 → "매출 이상치 분석" 적용, 제약 "이상치 탐지하지 마" 추가.
3. Ctrl+Enter: 린트 로그에 모순 제거 경고, 토큰 절감률 표시. Ctrl+Z로 추가한 제약 되돌리기.
4. 블루프린트 "데이터 품질 점검"(GPT·표·안개 테마)으로 바꿔 다시 컴파일, "블루프린트 저장".
5. 터미널에서 `Candy-Corn.exe --cli --blueprint 저장한파일.kiro`로 같은 프롬프트 확인, MCP `tools/list` 응답 확인.

## 범위 밖
- API 키가 없으면 LLM 실행 결과는 스키마 골격(모의 응답)만 보여 준다. 키는 환경 변수(ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY)에서만 읽는다.
