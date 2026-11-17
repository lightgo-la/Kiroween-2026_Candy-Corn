//! Optional tool: connect and test prompts, LLM APIs and MCP tools on a node canvas.
//! Run: `--canvas [file]` (GUI), `--run-graph file` (headless).
//!
//! - `graph`: graph data model, validation, auto layout, duplication. `.canvas.json` (nodes, edges, test cases).
//! - `exec`: execution engine in topological order. Stop signal, trust gates (MCP execution, custom endpoints, key
//!   environment variable names).
//! - `llm`: per-provider API calls (the studio's LLM run uses them too).
//! - `ui`: egui canvas (zoom and pan), inspector, prompt test panel.

pub mod exec;
pub mod graph;
pub mod llm;
pub mod mcp_client;
pub mod ui;
