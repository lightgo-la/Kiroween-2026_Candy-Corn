//! 옵션 도구: 노드 캔버스에서 프롬프트 · LLM API · MCP 도구를 연결하고 테스트한다.
//! 실행: `--canvas [파일]`(GUI), `--run-graph 파일`(헤드리스).

pub mod exec;
pub mod graph;
pub mod llm;
pub mod mcp_client;
pub mod ui;
