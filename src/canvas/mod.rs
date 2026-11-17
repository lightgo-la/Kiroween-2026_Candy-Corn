//! 옵션 도구: 노드 캔버스에서 프롬프트 · LLM API · MCP 도구를 연결하고 테스트한다.
//! 실행: `--canvas [파일]`(GUI), `--run-graph 파일`(헤드리스).
//!
//! - `graph`: 그래프 데이터 모델과 검증, 자동 정렬, 복제. `.canvas.json`(노드·간선·테스트 케이스).
//! - `exec`: 위상 순서 실행 엔진. 중지 신호, 신뢰 게이트(MCP 실행, 사용자 지정 엔드포인트, 키 환경 변수 이름).
//! - `llm`: 제공자별 API 호출(스튜디오의 LLM 실행도 같이 쓴다).
//! - `ui`: egui 캔버스(확대·이동), 인스펙터, 프롬프트 테스트 창.

pub mod exec;
pub mod graph;
pub mod llm;
pub mod mcp_client;
pub mod ui;
