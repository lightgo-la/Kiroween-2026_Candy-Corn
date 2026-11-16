//! 그래프 실행 엔진: 위상 정렬 순서로 노드를 실행하고 이벤트를 콜백으로 알린다.
//! 노드 실패 시 그 하위 노드는 건너뛰고, 다른 가지는 계속 실행한다.

use super::graph::{Graph, NodeKind, PromptFormat, PromptTarget};
use super::{llm, mcp_client};
use crate::ast::{OutputFormat, TargetModel};
use crate::pipeline::{self, CompileRequest};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// false면 MCP 노드는 서버 프로세스를 띄우지 않고 실패 처리한다(신뢰하지 않는 그래프 보호).
    pub allow_mcp: bool,
    /// (노드 id, 텍스트): 해당 Text 노드의 값을 이번 실행에서만 대체한다(테스트 입력).
    pub input_override: Option<(u32, String)>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Started(u32),
    Done { id: u32, output: String, ms: u128 },
    Failed { id: u32, error: String },
    Skipped(u32),
}

#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub outputs: HashMap<u32, String>,
    pub errors: HashMap<u32, String>,
    /// Output 노드 결과(없으면 마지막으로 성공한 노드) 합본
    pub final_text: String,
    pub total_ms: u128,
}

impl RunReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// `{{input}}`을 치환한다. 자리표시자가 없고 입력이 있으면 끝에 덧붙인다.
pub fn fill_input(template: &str, input: &str) -> String {
    if template.contains("{{input}}") {
        template.replace("{{input}}", input)
    } else if input.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{input}")
    }
}

/// JSON 템플릿용 치환: 입력을 JSON 문자열 이스케이프해서 넣는다.
pub fn fill_json_template(template: &str, input: &str) -> Result<Value, String> {
    let escaped = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let inner = &escaped[1..escaped.len() - 1];
    serde_json::from_str(&template.replace("{{input}}", inner)).map_err(|e| format!("arguments JSON 오류: {e}"))
}

pub fn run_graph(g: &Graph, opts: &RunOptions, mut on_event: impl FnMut(Event)) -> Result<RunReport, String> {
    let order = g.topo_order()?;
    let t0 = Instant::now();
    let mut rep = RunReport::default();
    let mut last_ok = None;
    for id in order {
        let ups = g.upstream(id);
        if ups.iter().any(|u| !rep.outputs.contains_key(u)) {
            on_event(Event::Skipped(id));
            continue;
        }
        let input = ups
            .iter()
            .map(|u| rep.outputs[u].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let node = g.node(id).expect("topo ids exist");
        on_event(Event::Started(id));
        let t = Instant::now();
        match exec_node(&node.kind, id, &input, opts) {
            Ok(out) => {
                on_event(Event::Done {
                    id,
                    output: out.clone(),
                    ms: t.elapsed().as_millis(),
                });
                rep.outputs.insert(id, out);
                last_ok = Some(id);
            }
            Err(error) => {
                on_event(Event::Failed {
                    id,
                    error: error.clone(),
                });
                rep.errors.insert(id, error);
            }
        }
    }
    let sinks: Vec<&str> = g
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Output))
        .filter_map(|n| rep.outputs.get(&n.id).map(String::as_str))
        .collect();
    rep.final_text = if sinks.is_empty() {
        last_ok.and_then(|i| rep.outputs.get(&i).cloned()).unwrap_or_default()
    } else {
        sinks.join("\n\n")
    };
    rep.total_ms = t0.elapsed().as_millis();
    Ok(rep)
}

fn exec_node(kind: &NodeKind, id: u32, input: &str, opts: &RunOptions) -> Result<String, String> {
    match kind {
        NodeKind::Text { text } => match &opts.input_override {
            Some((oid, t)) if *oid == id => Ok(t.clone()),
            _ => Ok(text.clone()),
        },
        NodeKind::Output => Ok(input.to_string()),
        NodeKind::Prompt {
            role,
            constraints,
            fields,
            target,
            format,
            data_path,
        } => {
            let data = if data_path.trim().is_empty() {
                None
            } else {
                Some(pipeline::load_data(Path::new(data_path.trim()), 8)?)
            };
            let mut fields = pipeline::parse_fields(fields);
            if fields.is_empty() {
                fields = pipeline::parse_fields(pipeline::DEFAULT_FIELDS);
            }
            let req = CompileRequest {
                target: match target {
                    PromptTarget::Claude => TargetModel::Claude,
                    PromptTarget::Gpt => TargetModel::Gpt,
                    PromptTarget::Gemini => TargetModel::Gemini,
                },
                role: fill_input(role, input),
                constraints: constraints
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect(),
                format: match format {
                    PromptFormat::Json => OutputFormat::StrictJson,
                    PromptFormat::Table => OutputFormat::MarkdownTable,
                },
                fields,
                data,
            };
            let r = pipeline::compile(&req);
            if let Some(d) = r.ast.lint_warnings.iter().find(|d| d.severity == "ERROR") {
                return Err(format!("린트 오류: {}", d.message));
            }
            Ok(r.output)
        }
        NodeKind::Llm {
            provider,
            model,
            system,
            key_env,
            base_url,
            max_tokens,
            temperature,
            dry_run,
        } => {
            if input.trim().is_empty() {
                return Err("LLM 입력이 비어 있습니다 (프롬프트 노드를 연결하세요)".into());
            }
            if *dry_run {
                return Ok(format!(
                    "[dry-run] {}/{} max_tokens={} 프롬프트 {}자\n---\n{}",
                    provider.label(),
                    model,
                    max_tokens,
                    input.chars().count(),
                    input
                ));
            }
            let key =
                std::env::var(key_env.trim()).map_err(|_| format!("환경 변수 {key_env} 가 설정되지 않았습니다"))?;
            llm::call(&llm::LlmRequest {
                provider: *provider,
                model,
                system,
                prompt: input,
                api_key: &key,
                base_url,
                max_tokens: *max_tokens,
                temperature: *temperature,
            })
        }
        NodeKind::Mcp {
            command,
            args,
            tool,
            arguments,
        } => {
            if !opts.allow_mcp {
                return Err("MCP 실행이 허용되지 않았습니다 (상단의 'MCP 실행 허용' 체크 또는 --allow-mcp)".into());
            }
            if tool.trim().is_empty() {
                return Err("MCP tool 이름이 비어 있습니다".into());
            }
            let args_json = fill_json_template(arguments, input)?;
            let mut c = mcp_client::McpClient::spawn(command, args, Duration::from_secs(30))?;
            c.call_tool(tool.trim(), args_json)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rules() {
        assert_eq!(fill_input("a {{input}} b", "X"), "a X b");
        assert_eq!(fill_input("a", "X"), "a\n\nX");
        assert_eq!(fill_input("a", ""), "a");
        let v = fill_json_template(r#"{"t":"{{input}}"}"#, "He said \"hi\"\n").unwrap();
        assert_eq!(v["t"], "He said \"hi\"\n");
    }

    #[test]
    fn runs_text_prompt_llm_dry_output() {
        let mut g = Graph::default();
        let t = g.add_node(NodeKind::Text { text: "원본".into() }, [0.0; 2]);
        let p = g.add_node(NodeKind::new_prompt(), [0.0; 2]);
        let l = g.add_node(NodeKind::new_llm(), [0.0; 2]);
        let o = g.add_node(NodeKind::Output, [0.0; 2]);
        for (a, b) in [(t, p), (p, l), (l, o)] {
            g.connect(a, b).unwrap();
        }
        let opts = RunOptions {
            allow_mcp: false,
            input_override: Some((t, "테스트입력".into())),
        };
        let mut events = 0;
        let r = run_graph(&g, &opts, |_| events += 1).unwrap();
        assert!(r.ok(), "{:?}", r.errors);
        assert!(r.final_text.starts_with("[dry-run]"));
        assert!(r.final_text.contains("테스트입력"));
        assert_eq!(events, 8);
    }

    #[test]
    fn failure_skips_downstream_only() {
        let mut g = Graph::default();
        let m = g.add_node(NodeKind::new_mcp(), [0.0; 2]);
        let o1 = g.add_node(NodeKind::Output, [0.0; 2]);
        let t = g.add_node(NodeKind::Text { text: "ok".into() }, [0.0; 2]);
        let o2 = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(m, o1).unwrap();
        g.connect(t, o2).unwrap();
        let mut skipped = vec![];
        let r = run_graph(&g, &RunOptions::default(), |e| {
            if let Event::Skipped(i) = e {
                skipped.push(i)
            }
        })
        .unwrap();
        assert!(r.errors.contains_key(&m));
        assert_eq!(skipped, vec![o1]);
        assert_eq!(r.final_text, "ok");
    }
}
