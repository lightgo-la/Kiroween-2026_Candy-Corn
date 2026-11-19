//! Graph execution engine: runs the nodes in topological order and reports events through a callback.
//! When a node fails, its downstream nodes are skipped and the other branches keep running.
//! MCP execution, API key environment variable names and LLM endpoints are checked before running, so that opening
//! an untrusted graph file cannot leak secrets.

use super::graph::{Graph, NodeKind, PromptFormat, PromptTarget, Provider};
use super::{llm, mcp_client};
use crate::ast::{OutputFormat, TargetModel, codegen};
use crate::keys;
use crate::pipeline::{self, CompileRequest};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// If false, MCP nodes fail without starting a server process (protects against untrusted graphs).
    pub allow_mcp: bool,
    /// If false, LLM nodes send the API key only to the provider's default address. A custom Base URL needs true.
    pub allow_custom_endpoint: bool,
    /// (node id, text): replaces the value of that Text node for this run only (test input).
    pub input_override: Option<(u32, String)>,
    /// Base folder for resolving relative data_path values of Prompt nodes (the folder of the graph file). None
    /// means the current working folder.
    pub base_dir: Option<PathBuf>,
    /// Stop signal. run_graph checks it between nodes and skips the remaining nodes once it is set.
    pub cancel: Option<Arc<AtomicBool>>,
    /// If true (the GUI), an LLM node may use the key saved in the 🔑 API keys window, but only when its key variable
    /// is the provider's own (ANTHROPIC_API_KEY for Anthropic, ...). `--run-graph` leaves it false.
    pub saved_keys: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    Started(u32),
    /// `note`: extra information from LLM nodes, such as token usage and truncation or stop warnings
    Done {
        id: u32,
        output: String,
        ms: u128,
        note: Option<String>,
    },
    Failed {
        id: u32,
        error: String,
    },
    Skipped(u32),
}

#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub outputs: HashMap<u32, String>,
    pub errors: HashMap<u32, String>,
    pub notes: HashMap<u32, String>,
    /// Nodes not run because an upstream node failed or the run was stopped (in run order)
    pub skipped: Vec<u32>,
    /// Whether nodes were skipped because of the stop signal
    pub cancelled: bool,
    /// Combined result of the Output nodes. Only a graph without Output nodes uses the result of the last node that
    /// succeeded.
    pub final_text: String,
    pub total_ms: u128,
}

impl RunReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty() && !self.cancelled
    }
}

/// The result of running one node.
struct NodeOut {
    text: String,
    note: Option<String>,
}

impl From<String> for NodeOut {
    fn from(text: String) -> Self {
        NodeOut { text, note: None }
    }
}

/// Replaces `{{input}}`. If there is no placeholder and the input is not empty, it is appended at the end.
pub fn fill_input(template: &str, input: &str) -> String {
    if template.contains("{{input}}") {
        template.replace("{{input}}", input)
    } else if input.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{input}")
    }
}

/// Replacement for JSON templates: the input is inserted with JSON string escaping.
pub fn fill_json_template(template: &str, input: &str) -> Result<Value, String> {
    let escaped = serde_json::to_string(input).map_err(|e| e.to_string())?;
    let inner = &escaped[1..escaped.len() - 1];
    serde_json::from_str(&template.replace("{{input}}", inner)).map_err(|e| trf!("arguments JSON error: {e}", e = e))
}

/// Turns the panic value of a worker thread into a readable sentence.
pub(super) fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| e.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| tr!("unknown error").into())
}

fn is_cancelled(opts: &RunOptions) -> bool {
    opts.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
}

pub fn run_graph(g: &Graph, opts: &RunOptions, mut on_event: impl FnMut(Event)) -> Result<RunReport, String> {
    let order = g.topo_order()?;
    let t0 = Instant::now();
    let mut rep = RunReport::default();
    let mut last_ok = None;
    for id in order {
        if is_cancelled(opts) {
            rep.cancelled = true;
        }
        let ups = g.upstream(id);
        if rep.cancelled || ups.iter().any(|u| !rep.outputs.contains_key(u)) {
            rep.skipped.push(id);
            on_event(Event::Skipped(id));
            continue;
        }
        let input = ups
            .iter()
            .map(|u| rep.outputs[u].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let Some(node) = g.node(id) else { continue }; // topo_order guarantees that the node exists
        on_event(Event::Started(id));
        let t = Instant::now();
        // A panic in one node becomes that node's failure, so it does not abort the whole run.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| exec_node(&node.kind, id, &input, opts)))
            .unwrap_or_else(|e| Err(trf!("Internal error while running the node: {}", panic_text(e))));
        let ms = t.elapsed().as_millis();
        match result {
            Ok(out) => {
                on_event(Event::Done {
                    id,
                    output: out.text.clone(),
                    ms,
                    note: out.note.clone(),
                });
                if let Some(note) = out.note {
                    rep.notes.insert(id, note);
                }
                rep.outputs.insert(id, out.text);
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
    let is_output = |kind: &NodeKind| matches!(kind, NodeKind::Output);
    rep.final_text = if g.nodes.iter().any(|n| is_output(&n.kind)) {
        // With Output nodes, only their results are used. If they all failed or were skipped, it is an empty string.
        g.nodes
            .iter()
            .filter(|n| is_output(&n.kind))
            .filter_map(|n| rep.outputs.get(&n.id).map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        // Without an Output node the last successful node is the result, but only when nothing failed or was
        // stopped: after a failure it would be a text from upstream (the input, a compiled prompt) shown as the result.
        last_ok
            .filter(|_| rep.errors.is_empty() && !rep.cancelled)
            .and_then(|i| rep.outputs.get(&i).cloned())
            .unwrap_or_default()
    };
    rep.total_ms = t0.elapsed().as_millis();
    Ok(rep)
}

// ---------- Paths ----------

/// Resolves the data_path of a Prompt node to the actual path. Relative paths start from the folder of the graph
/// file (`base`); without `base` they stay relative to the current working folder.
pub fn resolve_data_path(data_path: &str, base: Option<&Path>) -> PathBuf {
    let p = Path::new(data_path.trim());
    match base {
        Some(b) if !b.as_os_str().is_empty() && p.is_relative() => b.join(p),
        _ => p.to_path_buf(),
    }
}

/// The folder of the graph file (absolute path). For a bare file name it is the current working folder.
pub fn base_dir_of(graph_file: &Path) -> Option<PathBuf> {
    let parent = graph_file.parent()?;
    if parent.as_os_str().is_empty() {
        return std::env::current_dir().ok();
    }
    Some(std::path::absolute(parent).unwrap_or_else(|_| parent.to_path_buf()))
}

// ---------- Security checks ----------

/// A key variable name for an error message. Anything that is not a short plain name is hidden (only its last
/// characters are shown), because an API key pasted into the name field would otherwise end up in logs and status
/// lines.
fn shown_key_env(name: &str) -> String {
    if name.chars().count() <= 30 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        name.to_string()
    } else {
        crate::keys::tail(name)
    }
}

/// Checks the name of the environment variable the API key is read from: `^[A-Z][A-Z0-9_]*_API_KEY$`.
/// This keeps a graph file from pointing at PATH or another secret variable and sending it out.
pub fn valid_key_env(name: &str) -> bool {
    let Some(prefix) = name.strip_suffix("_API_KEY") else {
        return false;
    };
    let mut chars = prefix.chars();
    matches!(chars.next(), Some('A'..='Z')) && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

fn is_port(p: &str) -> bool {
    !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
}

/// The API key of an LLM node. With `saved_keys` (the GUI), the key saved in the 🔑 API keys window comes first, but
/// only for the provider's own variable (ANTHROPIC_API_KEY for Anthropic, ...), so a graph file cannot send the key
/// saved for one provider to another provider's address. Otherwise the environment variable is read.
fn node_key(provider: Provider, key_env: &str, saved_keys: bool) -> Result<String, String> {
    let use_saved = saved_keys && key_env == provider.default_key_env();
    match keys::lookup(key_env, use_saved) {
        Some((key, _)) => Ok(key),
        None if use_saved => Err(trf!(
            "No {} API key. Add one in 🔑 API keys or set {}",
            provider.label(),
            key_env
        )),
        None => Err(trf!("Environment variable {key_env} is not set", key_env = key_env)),
    }
}

/// Reads (scheme, host) from an address, in lowercase. None if there is user info (`@`), a backslash, whitespace or
/// a control character, or if the format is odd. The parsing is strict to stop tricks where the checked host differs
/// from the host actually connected to.
fn url_scheme_host(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| matches!(c, '@' | '\\') || c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let host = if let Some(inner) = authority.strip_prefix('[') {
        // IPv6: "[::1]" or "[::1]:port"
        let (addr, after) = inner.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(is_port)) {
            return None;
        }
        format!("[{addr}]")
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        };
        if host.is_empty() || host.contains(':') || port.is_some_and(|p| !is_port(p)) {
            return None;
        }
        host.to_string()
    };
    Some((scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Base URL policy for LLM nodes. An empty URL or the provider's default address always passes.
/// Any other address receives the API key, so it needs `allow_custom` (explicit trust),
/// and it must be https unless it is localhost/127.0.0.1/[::1].
pub fn check_endpoint(provider: Provider, base_url: &str, allow_custom: bool) -> Result<(), String> {
    let url = base_url.trim();
    if url.is_empty()
        || url
            .trim_end_matches('/')
            .eq_ignore_ascii_case(provider.default_base_url())
    {
        return Ok(());
    }
    if !allow_custom {
        return Err(trf!(
            "Custom endpoint ({url}) is not allowed. The API key would be sent to that address (tick 'Allow custom endpoint' at the top or use --allow-custom-endpoint)",
            url = url
        ));
    }
    // A query or fragment would swallow the API path that is appended to the address
    if url.contains(['?', '#']) {
        return Err(trf!("Invalid endpoint address: {url}", url = url));
    }
    match url_scheme_host(url) {
        Some((scheme, _)) if scheme == "https" => Ok(()),
        Some((scheme, host)) if scheme == "http" && is_loopback(&host) => Ok(()),
        Some(_) => Err(trf!(
            "Custom endpoints must use https (http is allowed for localhost, 127.0.0.1 and [::1]): {url}",
            url = url
        )),
        None => Err(trf!("Invalid endpoint address: {url}", url = url)),
    }
}

// ---------- Node execution ----------

/// The Prompt node settings used for compiling.
struct PromptCfg<'a> {
    role: &'a str,
    constraints: &'a str,
    fields: &'a str,
    target: PromptTarget,
    format: PromptFormat,
    data_path: &'a str,
}

/// Start of the marker temporarily put in place of `{{input}}` (a hex number follows). It is alphanumeric only, so
/// the linter reads it as one word together with the adjacent characters ("really<marker>" is not a filler, and the
/// spaces stay as they are) and the code generator does not change it either. The uppercase `C` appears only in the
/// first two characters, so the marker cannot overlap the adjacent characters and be read at the wrong position.
const TOKEN_PREFIX: &str = "CCinput";

/// Puts the marker at the `{{input}}` positions of the instruction (at the end if there are none). Returns
/// (the instruction with the marker, the marker, how many times the marker must appear after compiling).
/// The marker gets a number that appears in neither the instruction, the input nor `others` (constraints, fields,
/// data profile text).
fn mark_input(template: &str, input: &str, others: &[&str]) -> (String, String, usize) {
    let taken = |t: &str| template.contains(t) || input.contains(t) || others.iter().any(|o| o.contains(t));
    let mut n = 0u64;
    let token = loop {
        let t = format!("{TOKEN_PREFIX}{n:x}");
        if !taken(&t) {
            break t;
        }
        n += 1;
    };
    let slots = template.matches("{{input}}").count();
    let (role, expected) = if slots > 0 {
        (fill_input(template, &token), slots)
    } else if input.is_empty() {
        (template.to_string(), 0)
    } else {
        (fill_input(template, &token), 1)
    };
    (role, token, expected)
}

/// The data profile text that goes into the prompt (checked so that the marker does not collide with it).
fn data_texts(d: &crate::ast::DataSubAst) -> Vec<&str> {
    let mut v = vec![d.source_name.as_str()];
    for c in &d.columns {
        v.extend([c.name.as_str(), c.dtype.as_str(), c.summary_stats.as_str()]);
    }
    v.extend(d.anomalies.iter().map(String::as_str));
    v.extend(d.sample_rows.iter().map(String::as_str));
    v
}

/// Puts the upstream input at the marker positions of the compiled prompt. The input skips the linter (fillers and
/// spaces stay as they are) but gets the same processing the code generator applies to the role instruction: after
/// line break and control character cleanup (multi_line), Claude gets one XML escape; for GPT and Gemini, the line
/// that held the marker is split into lines again together with the input, and every line gets the structure-line
/// guard (GPT `## Output` and code fences, Gemini `=== … ===`). Lines the code generator already guarded start with
/// `\`, so they stay as they are.
fn insert_input(compiled: &str, token: &str, input: &str, target: TargetModel) -> String {
    let text = codegen::multi_line(input);
    let Some(guard) = codegen::role_guard(target) else {
        return compiled.replace(token, &codegen::xml_escape(&text));
    };
    compiled
        .split('\n')
        .map(|line| {
            if !line.contains(token) {
                return line.to_string();
            }
            line.replace(token, &text)
                .split('\n')
                .map(|l| guard(l).into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Upstream input skips the linter. The prompt is compiled with the marker in place, then the input is processed
/// once for the target model and put at the marker positions of the finished prompt (`insert_input`).
fn compile_prompt(cfg: &PromptCfg, input: &str, base_dir: Option<&Path>) -> Result<String, String> {
    let data = if cfg.data_path.trim().is_empty() {
        None
    } else {
        let path = resolve_data_path(cfg.data_path, base_dir);
        // A graph file is not trusted: a network path would send the user's sign-in hash to that host
        if let Some(why) = pipeline::untrusted_path_problem(&path) {
            return Err(trf!("Could not read the data file ({}): {e}", path.display(), e = why));
        }
        Some(
            pipeline::load_data(&path, 8)
                .map_err(|e| trf!("Could not read the data file ({}): {e}", path.display(), e = e))?,
        )
    };
    let mut fields = pipeline::parse_fields(cfg.fields);
    if fields.is_empty() {
        fields = pipeline::parse_fields(pipeline::DEFAULT_FIELDS);
    }
    let constraints: Vec<String> = cfg
        .constraints
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let mut others: Vec<&str> = constraints.iter().chain(fields.iter()).map(String::as_str).collect();
    if let Some(d) = &data {
        others.extend(data_texts(&d.data));
    }
    let (role, token, expected) = mark_input(cfg.role, input, &others);
    let target = match cfg.target {
        PromptTarget::Claude => TargetModel::Claude,
        PromptTarget::Gpt => TargetModel::Gpt,
        PromptTarget::Gemini => TargetModel::Gemini,
    };
    let req = CompileRequest {
        target,
        role,
        constraints,
        format: match cfg.format {
            PromptFormat::Json => OutputFormat::StrictJson,
            PromptFormat::Table => OutputFormat::MarkdownTable,
        },
        fields,
        data,
    };
    let r = pipeline::compile(&req);
    if let Some(d) = r.ast.lint_warnings.iter().find(|d| d.severity == "ERROR") {
        return Err(trf!("Lint error: {}", d.message));
    }
    let found = r.output.matches(&token).count();
    if found != expected {
        return Err(trf!(
            "Input position markers left after compiling: {found} (expected {expected}). They conflicted with the lint or serialization rules",
            found = found,
            expected = expected
        ));
    }
    Ok(insert_input(&r.output, &token, input, target))
}

/// Turns an LLM response into the node output. An empty body is a failure (with the stop reason); a response that
/// was truncated or stopped by the token limit, a full context window, a refusal etc. keeps its body and gets a
/// warning. Token usage is noted too when present.
fn judge_reply(reply: llm::LlmReply, max_tokens: u32) -> Result<NodeOut, String> {
    let reason = reply.stop_reason.as_deref();
    if reply.text.trim().is_empty() {
        return Err(match reason {
            Some(r) => trf!("The LLM response body is empty (stop reason: {r})", r = r),
            None => tr!("The LLM response body is empty").into(),
        });
    }
    let r = reason.unwrap_or_default();
    let mut parts = Vec::new();
    match reply.finish() {
        llm::Finish::MaxTokens => parts.push(trf!(
            "⚠ Truncated at the output token limit ({max_tokens}) ({r})",
            max_tokens = max_tokens,
            r = r
        )),
        llm::Finish::ContextWindow => parts.push(trf!("⚠ Truncated: the model's context window is full ({r})", r = r)),
        llm::Finish::Stopped => parts.push(trf!("⚠ The response did not finish (stop reason: {r})", r = r)),
        llm::Finish::Complete | llm::Finish::Unknown => {}
    }
    if let Some(u) = reply.usage {
        parts.push(trf!("tokens {}→{}", u.input, u.output));
    }
    Ok(NodeOut {
        note: (!parts.is_empty()).then(|| parts.join(" · ")),
        text: reply.text,
    })
}

fn exec_node(kind: &NodeKind, id: u32, input: &str, opts: &RunOptions) -> Result<NodeOut, String> {
    match kind {
        NodeKind::Text { text } => {
            let t = match &opts.input_override {
                Some((oid, t)) if *oid == id => t.clone(),
                _ => text.clone(),
            };
            Ok(t.into())
        }
        NodeKind::Output => Ok(input.to_string().into()),
        NodeKind::Prompt {
            role,
            constraints,
            fields,
            target,
            format,
            data_path,
        } => compile_prompt(
            &PromptCfg {
                role,
                constraints,
                fields,
                target: *target,
                format: *format,
                data_path,
            },
            input,
            opts.base_dir.as_deref(),
        )
        .map(Into::into),
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
            // The checks are done before the key is read or the network is used.
            let key_env = key_env.trim();
            if !valid_key_env(key_env) {
                return Err(trf!(
                    "Key environment variable name '{key_env}' is not allowed (use uppercase letters, digits and underscores, ending in _API_KEY; e.g. {})",
                    provider.default_key_env(),
                    key_env = shown_key_env(key_env)
                ));
            }
            if !*dry_run {
                check_endpoint(*provider, base_url, opts.allow_custom_endpoint)?;
            }
            if input.trim().is_empty() {
                return Err(tr!("The LLM input is empty (connect a Prompt node)").into());
            }
            if *dry_run {
                // The dry run passes, but warn up front if the settings would be blocked in a real run.
                let note = check_endpoint(*provider, base_url, opts.allow_custom_endpoint)
                    .err()
                    .map(|e| trf!("⚠ Blocked in a real run: {e}", e = e));
                // Only the words of the summary line are translated. The `[dry-run]` head and the prompt text are the
                // same in every language.
                let chars = input.chars().count();
                let unit = if chars == 1 { tr!("char") } else { tr!("chars") };
                return Ok(NodeOut {
                    text: trf!(
                        "[dry-run] {}/{} max_tokens={} prompt {chars} {unit}\n---\n{}",
                        provider.label(),
                        model,
                        max_tokens,
                        input,
                        chars = chars,
                        unit = unit
                    ),
                    note,
                });
            }
            let key = node_key(*provider, key_env, opts.saved_keys)?;
            // The endpoint saved in the 🔑 API keys window applies to the provider's own key when the node has no
            // address of its own (the GUI only; --run-graph never reads saved settings)
            let saved_url;
            let base_url = if opts.saved_keys && base_url.trim().is_empty() && key_env == provider.default_key_env() {
                saved_url = crate::keys::base_url(*provider)?;
                saved_url.as_str()
            } else {
                base_url.as_str()
            };
            let reply = llm::call(&llm::LlmRequest {
                provider: *provider,
                model,
                system,
                prompt: input,
                api_key: &key,
                base_url,
                max_tokens: *max_tokens,
                // Values outside the range the provider accepts give HTTP 400, so clamp them to the limit.
                temperature: temperature.map(|t| provider.clamp_temperature(t)),
            })?;
            judge_reply(reply, *max_tokens)
        }
        NodeKind::Mcp {
            command,
            args,
            tool,
            arguments,
        } => {
            if !opts.allow_mcp {
                return Err(tr!(
                    "MCP execution is not allowed (tick 'Allow MCP execution' at the top or use --allow-mcp)"
                )
                .into());
            }
            if tool.trim().is_empty() {
                return Err(tr!("The MCP tool name is empty").into());
            }
            let args_json = fill_json_template(arguments, input)?;
            let mut c = mcp_client::McpClient::spawn(command, args, Duration::from_secs(30))?;
            c.call_tool(tool.trim(), args_json).map(Into::into)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};
    use std::sync::atomic::AtomicBool;

    /// Input with line breaks, fillers, a markdown table and XML special characters.
    const NASTY: &str = ko_fixture!("graph.nasty_input");

    const TARGETS: [PromptTarget; 3] = [PromptTarget::Claude, PromptTarget::Gpt, PromptTarget::Gemini];

    fn cfg<'a>(role: &'a str, target: PromptTarget) -> PromptCfg<'a> {
        PromptCfg {
            role,
            constraints: ko_fixture!("graph.exec.constraints"),
            fields: "summary",
            target,
            format: PromptFormat::Json,
            data_path: "",
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!("cc_exec_{}_{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn llm_node(key_env: &str, base_url: &str, dry_run: bool) -> NodeKind {
        NodeKind::Llm {
            provider: Provider::Anthropic,
            model: "m".into(),
            system: String::new(),
            key_env: key_env.into(),
            base_url: base_url.into(),
            max_tokens: 100,
            temperature: None,
            dry_run,
        }
    }

    fn run(kind: &NodeKind, input: &str, opts: &RunOptions) -> Result<String, String> {
        exec_node(kind, 1, input, opts).map(|o| o.text)
    }

    // ---------- Input replacement ----------

    #[test]
    fn fill_rules() {
        assert_eq!(fill_input("a {{input}} b", "X"), "a X b");
        assert_eq!(fill_input("a", "X"), "a\n\nX");
        assert_eq!(fill_input("a", ""), "a");
        let v = fill_json_template(r#"{"t":"{{input}}"}"#, "He said \"hi\"\n").unwrap();
        assert_eq!(v["t"], "He said \"hi\"\n");
    }

    #[test]
    fn placeholder_survives_lint_and_codegen() {
        // Even as the linter and the code generator change, the marker must remain exactly once. The same goes when
        // it is glued to a filler.
        let templates = [ko_fixture!("graph.exec.template_fillers")]
            .into_iter()
            .chain(ko_fixture!("graph.glued_roles").iter().copied())
            .chain(["{{input}}very"]);
        for template in templates {
            let (role, token, expected) = mark_input(template, "x", &[]);
            assert!(token.starts_with(TOKEN_PREFIX) && token.chars().all(|c| c.is_ascii_alphanumeric()));
            assert_eq!(expected, 1);
            for target in TargetModel::ALL {
                for format in [OutputFormat::StrictJson, OutputFormat::MarkdownTable] {
                    let req = CompileRequest {
                        target,
                        role: role.clone(),
                        constraints: vec![
                            "very important please".into(),
                            ko_fixture!("graph.exec.lint_constraint").into(),
                        ],
                        format,
                        fields: vec!["a".into(), "b".into()],
                        data: None,
                    };
                    let out = pipeline::compile(&req).output;
                    assert_eq!(
                        out.matches(&token).count(),
                        1,
                        "{template} {target:?}/{format:?}\n{out}"
                    );
                }
            }
        }
    }

    /// Review repro: the marker was a private-use character, so the filler check treated it as punctuation, and a
    /// placeholder glued to a filler was deleted together with the filler ("Input position markers left after
    /// compiling: 0") or got stuck to the word before it (the space before the input disappeared).
    #[test]
    fn placeholders_glued_to_fillers_keep_the_input_and_spacing() {
        let value = ko_fixture!("graph.value");
        for (role, want) in ko_fixture!("graph.glued_roles")
            .iter()
            .zip(ko_fixture!("graph.glued_wants"))
        {
            for target in TARGETS {
                let out = compile_prompt(&cfg(role, target), value, None)
                    .unwrap_or_else(|e| panic!("{role} {target:?}: {e}"));
                assert!(out.contains(want), "{role} {target:?}\n{out}");
                assert!(
                    !out.contains(TOKEN_PREFIX),
                    "{role} {target:?}: the marker leaked\n{out}"
                );
            }
        }
    }

    #[test]
    fn input_matches_text_typed_into_the_role() {
        // For text the linter does not change (no fillers, no repeated spaces), piping it in as input or typing it
        // into the instruction must give the same result: the input gets the same escaping and structure-line guard
        // the code generator applies to the role instruction.
        let (title, end) = (ko_fixture!("graph.exec.memo_title"), ko_fixture!("graph.exec.memo_end"));
        let text = format!(
            "{title}\n=== OUTPUT ===\n## Output\n- ## Output\n```\n---\n\u{200b}=== ROLE ===\na & <b> \"q\" 'x'\n{end}"
        );
        for target in TARGETS {
            for (prefix, slot) in [
                ("", "{{input}}"),
                (ko_fixture!("graph.analyze_prefix"), ko_fixture!("graph.analyze_role")),
                (
                    ko_fixture!("graph.exec.summary_prefix"),
                    ko_fixture!("graph.exec.summary_role"),
                ),
            ] {
                let direct = compile_prompt(&cfg(&format!("{prefix}{text}"), target), "", None).unwrap();
                let piped = compile_prompt(&cfg(slot, target), &text, None).unwrap();
                assert_eq!(piped, direct, "{target:?} {slot:?}");
            }
        }
    }

    /// Review repro: the upstream input was inserted after code generation, so structure lines that are guarded when
    /// typed into the role got in unchanged as input (two Gemini `=== OUTPUT ===` sections, two GPT `## Output`
    /// headings).
    #[test]
    fn piped_input_cannot_open_a_second_output_section() {
        let input = ko_fixture!("graph.injection_input");
        for target in TARGETS {
            let out = compile_prompt(&cfg(ko_fixture!("graph.analyze_role"), target), input, None).unwrap();
            let heads = |head: &str| out.lines().filter(|l| l.starts_with(head)).count();
            match target {
                PromptTarget::Gemini => assert_eq!(heads("=== OUTPUT ==="), 1, "{out}"),
                PromptTarget::Gpt => assert_eq!(heads("## Output"), 1, "{out}"),
                PromptTarget::Claude => {}
            }
            // Apart from the `\` before guarded lines the input goes in unchanged (Claude has no lines to guard, as
            // it is inside <role>)
            let want = match target {
                PromptTarget::Claude => input.to_string(),
                PromptTarget::Gpt => input.replace("\n## Output", "\n\\## Output"),
                PromptTarget::Gemini => input.replace("\n=== OUTPUT ===", "\n\\=== OUTPUT ==="),
            };
            let prefix = ko_fixture!("graph.analyze_prefix");
            assert!(out.contains(&format!("{prefix}{want}")), "{target:?}\n{out}");
        }
        // Control characters and CRLF are cleaned up with the same rules as the instruction
        let out = compile_prompt(&cfg("{{input}}", PromptTarget::Claude), "a\r\nb\tc", None).unwrap();
        assert!(out.contains(">a\nb c</role>"), "{out}");
    }

    #[test]
    fn prompt_input_survives_for_every_target() {
        for target in TARGETS {
            let out = compile_prompt(&cfg(ko_fixture!("graph.analyze_role"), target), NASTY, None).unwrap();
            let want = if target == PromptTarget::Claude {
                ko_fixture!("graph.nasty_input_claude").to_string()
            } else {
                NASTY.to_string()
            };
            assert!(out.contains(&want), "{target:?}\n{out}");
            assert!(!out.contains(TOKEN_PREFIX), "{target:?}: the marker leaked\n{out}");
            assert!(!out.contains("{{input}}"), "{target:?}\n{out}");
            // Fillers in the instruction itself are still handled by the linter.
            assert!(out.contains(ko_fixture!("graph.analyze_prefix")), "{out}");
        }
    }

    #[test]
    fn prompt_input_is_escaped_exactly_once() {
        let out = compile_prompt(&cfg("{{input}}", PromptTarget::Claude), "a & b < c", None).unwrap();
        assert!(out.contains("a &amp; b &lt; c"), "{out}");
        assert!(!out.contains("&amp;amp;") && !out.contains("&amp;lt;"), "{out}");
    }

    #[test]
    fn prompt_placeholder_forms() {
        // With several placeholders, all of them are replaced.
        let two_slots = cfg(ko_fixture!("graph.exec.two_slots"), PromptTarget::Gpt);
        let out = compile_prompt(&two_slots, ko_fixture!("graph.value"), None).unwrap();
        assert!(out.contains(ko_fixture!("graph.exec.two_slots_filled")), "{out}");
        // Without a placeholder the input is appended at the end (the same rule as fill_input). The linter keeps the
        // line breaks of the role, so even the blank line stays.
        let instruction = ko_fixture!("graph.exec.instruction");
        let out = compile_prompt(&cfg(instruction, PromptTarget::Gpt), NASTY, None).unwrap();
        assert!(out.contains(&format!("{instruction}\n\n{NASTY}")), "{out}");
        // With an empty input and no placeholder, only the instruction remains.
        let out = compile_prompt(&cfg(instruction, PromptTarget::Gpt), "", None).unwrap();
        assert!(out.contains(&format!("Analyst: {instruction}\n")), "{out}");
        // Even with an empty input the placeholder is replaced, with an empty string. The spaces of the instruction
        // stay.
        let out = compile_prompt(&cfg(ko_fixture!("graph.exec.one_slot"), PromptTarget::Gpt), "", None).unwrap();
        assert!(
            out.contains(ko_fixture!("graph.exec.one_slot_empty")) && !out.contains(TOKEN_PREFIX),
            "{out}"
        );
        // Text in the input that looks like {{input}} or a marker is not replaced again (the marker gets a number that
        // is not in the input).
        let tricky = format!("{{{{input}}}} {TOKEN_PREFIX}0 {TOKEN_PREFIX}1");
        let (_, token, _) = mark_input("[{{input}}]", &tricky, &[]);
        assert_eq!(token, format!("{TOKEN_PREFIX}2"));
        let out = compile_prompt(&cfg("[{{input}}]", PromptTarget::Gpt), &tricky, None).unwrap();
        assert!(out.contains(&format!("[{tricky}]")), "{out}");
        assert_eq!(
            out.matches(TOKEN_PREFIX).count(),
            2,
            "only the input's text remains: {out}"
        );
    }

    #[test]
    fn placeholder_never_collides_with_template() {
        let mine = format!("{TOKEN_PREFIX}0");
        let (my_text, and) = (ko_fixture!("graph.exec.my_text"), ko_fixture!("graph.exec.and"));
        let template = format!("{my_text} {mine} {and} {{{{input}}}}");
        let (role, token, expected) = mark_input(&template, "x", &[]);
        assert_ne!(token, mine);
        assert_eq!(expected, 1);
        assert_eq!(role.matches(&token).count(), 1);
        let value = ko_fixture!("graph.value");
        let out = compile_prompt(&cfg(&template, PromptTarget::Gpt), value, None).unwrap();
        assert!(
            out.contains(&format!("{my_text} {mine} {and} {value}")),
            "the user's text stays as is: {out}"
        );
        // It also avoids text found in the constraints.
        let (_, token, _) = mark_input("a {{input}}", "x", &[&mine]);
        assert_ne!(token, mine);
        assert_eq!(mark_input("a", "", &[]).2, 0);
        assert_eq!(mark_input("a", "x", &[]).2, 1);
        assert_eq!(mark_input("{{input}}{{input}}", "", &[]).2, 2);
        // The marker is not read across the adjacent characters (the position right after "CCinput")
        let out = compile_prompt(&cfg("CCinput{{input}}0", PromptTarget::Gpt), value, None).unwrap();
        assert!(out.contains(&format!("Analyst: CCinput{value}0\n")), "{out}");
    }

    #[test]
    fn placeholder_avoids_text_from_the_data_file() {
        // Even if the data profile (column names) contains the marker text, another number is chosen and it does not
        // fail.
        let tmp = TempDir::new("token_data");
        std::fs::write(tmp.0.join("d.csv"), format!("{TOKEN_PREFIX}0,amount\nx,10\ny,20\n")).unwrap();
        for target in TARGETS {
            let c = PromptCfg {
                data_path: "d.csv",
                ..cfg(ko_fixture!("graph.exec.analyze_inline_role"), target)
            };
            let out = compile_prompt(&c, ko_fixture!("graph.value"), Some(&tmp.0)).unwrap();
            assert!(
                out.contains(&format!("{TOKEN_PREFIX}0"))
                    && out.contains(ko_fixture!("graph.exec.analyze_inline_filled")),
                "{target:?}\n{out}"
            );
        }
    }

    fn two_prompt_chain(first: PromptTarget, second: PromptTarget) -> String {
        let prompt = |role: &str, target| NodeKind::Prompt {
            role: role.into(),
            constraints: String::new(),
            fields: "summary".into(),
            target,
            format: PromptFormat::Json,
            data_path: String::new(),
        };
        let mut g = Graph::default();
        let t = g.add(NodeKind::Text { text: NASTY.into() }, [0.0; 2]);
        let p1 = g.add(prompt(ko_fixture!("graph.analyze_role"), first), [0.0; 2]);
        let p2 = g.add(prompt("{{input}}", second), [0.0; 2]);
        let o = g.add(NodeKind::Output, [0.0; 2]);
        for (a, b) in [(t, p1), (p1, p2), (p2, o)] {
            g.connect(a, b).unwrap();
        }
        let r = run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
        assert!(r.ok(), "{:?}", r.errors);
        r.final_text
    }

    #[test]
    fn prompt_chains_do_not_escape_twice() {
        // The original text reaches the second prompt unchanged.
        let out = two_prompt_chain(PromptTarget::Gpt, PromptTarget::Gpt);
        assert!(out.contains(NASTY), "{out}");
        // When a GPT prompt receives the output of a Claude prompt (escaped once), it does not escape it again.
        let out = two_prompt_chain(PromptTarget::Claude, PromptTarget::Gpt);
        assert!(out.contains(ko_fixture!("graph.exec.nasty_tail_claude")), "{out}");
        assert!(!out.contains("&amp;lt;") && !out.contains("&amp;amp;"), "{out}");
        assert!(!out.contains(TOKEN_PREFIX));
        // When a GPT prompt goes into another GPT prompt, the heading of the first prompt becomes plain text (there
        // is only one real "## Output").
        let out = two_prompt_chain(PromptTarget::Gpt, PromptTarget::Gpt);
        assert_eq!(out.lines().filter(|l| *l == "## Output").count(), 1, "{out}");
        assert!(out.contains("\n\\## Output\n"), "{out}");
    }

    // ---------- Graph runs ----------

    #[test]
    fn runs_text_prompt_llm_dry_output() {
        let mut g = Graph::default();
        let t = g.add(
            NodeKind::Text {
                text: "original".into(),
            },
            [0.0; 2],
        );
        let p = g.add(NodeKind::new_prompt(), [0.0; 2]);
        let l = g.add(NodeKind::new_llm(), [0.0; 2]);
        let o = g.add(NodeKind::Output, [0.0; 2]);
        for (a, b) in [(t, p), (p, l), (l, o)] {
            g.connect(a, b).unwrap();
        }
        let opts = RunOptions {
            input_override: Some((t, "test input".into())),
            ..RunOptions::default()
        };
        let mut events = 0;
        let r = run_graph(&g, &opts, |_| events += 1).unwrap();
        assert!(r.ok(), "{:?}", r.errors);
        assert!(r.final_text.starts_with("[dry-run]"));
        assert!(r.final_text.contains("test input"));
        assert_eq!(events, 8);
    }

    #[test]
    fn failure_skips_downstream_only() {
        let mut g = Graph::default();
        let m = g.add(NodeKind::new_mcp(), [0.0; 2]);
        let o1 = g.add(NodeKind::Output, [0.0; 2]);
        let t = g.add(NodeKind::Text { text: "ok".into() }, [0.0; 2]);
        let o2 = g.add(NodeKind::Output, [0.0; 2]);
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
        assert_eq!(r.skipped, vec![o1]);
        assert_eq!(r.final_text, "ok");
    }

    #[test]
    fn failed_outputs_do_not_fall_back_to_last_success() {
        // Text -> MCP (not allowed, fails) -> Output: the original text must not come out as the final result.
        let mut g = Graph::default();
        let t = g.add(
            NodeKind::Text {
                text: "original text".into(),
            },
            [0.0; 2],
        );
        let m = g.add(NodeKind::new_mcp(), [0.0; 2]);
        let o = g.add(NodeKind::Output, [0.0; 2]);
        g.connect(t, m).unwrap();
        g.connect(m, o).unwrap();
        let r = run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
        assert!(!r.ok() && r.errors.contains_key(&m));
        assert_eq!(
            r.final_text, "",
            "with an Output node, the last successful node is not used instead"
        );
        // Only when there is no Output node at all is the result of the last successful node used.
        g.remove_node(o);
        g.remove_node(m);
        let r = run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
        assert_eq!(r.final_text, "original text");
        // No nodes to run gives an empty result
        assert_eq!(
            run_graph(&Graph::default(), &RunOptions::default(), |_| {})
                .unwrap()
                .final_text,
            ""
        );
    }

    fn chain4() -> (Graph, [u32; 4]) {
        let mut g = Graph::default();
        let ids = [
            g.add(
                NodeKind::Text {
                    text: "input text".into(),
                },
                [0.0; 2],
            ),
            g.add(NodeKind::new_prompt(), [0.0; 2]),
            g.add(NodeKind::new_llm(), [0.0; 2]),
            g.add(NodeKind::Output, [0.0; 2]),
        ];
        for w in ids.windows(2) {
            g.connect(w[0], w[1]).unwrap();
        }
        (g, ids)
    }

    #[test]
    fn cancel_before_start_skips_everything() {
        let (g, ids) = chain4();
        let opts = RunOptions {
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..RunOptions::default()
        };
        let mut events = vec![];
        let r = run_graph(&g, &opts, |e| events.push(e)).unwrap();
        assert!(r.cancelled && !r.ok());
        assert_eq!(r.skipped, ids.to_vec());
        assert!(r.outputs.is_empty() && r.errors.is_empty());
        assert!(events.iter().all(|e| matches!(e, Event::Skipped(_))) && events.len() == 4);
        assert_eq!(r.final_text, "");
    }

    #[test]
    fn cancel_between_nodes_reports_the_rest_as_skipped() {
        let (g, ids) = chain4();
        let flag = Arc::new(AtomicBool::new(false));
        let opts = RunOptions {
            cancel: Some(flag.clone()),
            ..RunOptions::default()
        };
        let mut log = vec![];
        let r = run_graph(&g, &opts, |e| {
            match &e {
                // The same as pressing stop the moment the second node finishes.
                Event::Done { id, .. } if *id == ids[1] => flag.store(true, Ordering::Relaxed),
                _ => {}
            }
            log.push(e);
        })
        .unwrap();
        assert!(r.cancelled && !r.ok());
        assert_eq!(r.outputs.len(), 2, "results of nodes that already finished are kept");
        assert_eq!(r.skipped, vec![ids[2], ids[3]]);
        let skipped: Vec<u32> = log
            .iter()
            .filter_map(|e| if let Event::Skipped(i) = e { Some(*i) } else { None })
            .collect();
        assert_eq!(skipped, vec![ids[2], ids[3]]);
        assert!(r.errors.is_empty());
        assert_eq!(r.final_text, "", "the run never reached Output, so there is no result");
    }

    #[test]
    fn cancel_after_the_last_node_is_not_a_cancellation() {
        let (g, ids) = chain4();
        let flag = Arc::new(AtomicBool::new(false));
        let opts = RunOptions {
            cancel: Some(flag.clone()),
            ..RunOptions::default()
        };
        let r = run_graph(&g, &opts, |e| {
            if matches!(e, Event::Done { id, .. } if id == ids[3]) {
                flag.store(true, Ordering::Relaxed);
            }
        })
        .unwrap();
        assert!(r.ok() && !r.cancelled && r.skipped.is_empty());
        assert!(r.final_text.starts_with("[dry-run]"));
    }

    #[test]
    fn panic_text_reads_both_payload_kinds() {
        let a = std::panic::catch_unwind(|| panic!("fixed text")).unwrap_err();
        assert_eq!(panic_text(a), "fixed text");
        let n = 7;
        let b = std::panic::catch_unwind(|| panic!("value {n}")).unwrap_err();
        assert_eq!(panic_text(b), "value 7");
        assert_eq!(panic_text(Box::new(5u8)), "unknown error");
    }

    // ---------- Data paths ----------

    #[test]
    fn data_path_resolution() {
        let base = Path::new("base");
        assert_eq!(resolve_data_path(" d.csv ", Some(base)), base.join("d.csv"));
        assert_eq!(resolve_data_path("sub/d.csv", Some(base)), base.join("sub/d.csv"));
        assert_eq!(resolve_data_path("d.csv", None), PathBuf::from("d.csv"));
        assert_eq!(resolve_data_path("d.csv", Some(Path::new(""))), PathBuf::from("d.csv"));
        let abs = std::env::temp_dir().join("x.csv");
        assert_eq!(
            resolve_data_path(abs.to_str().unwrap(), Some(base)),
            abs,
            "absolute paths stay as they are"
        );
    }

    #[test]
    fn base_dir_comes_from_the_graph_file() {
        let dir = base_dir_of(Path::new("a/b/flow.canvas.json")).unwrap();
        assert!(dir.is_absolute() && dir.ends_with(Path::new("a").join("b")), "{dir:?}");
        assert_eq!(base_dir_of(Path::new("flow.canvas.json")), std::env::current_dir().ok());
        let abs = std::env::temp_dir().join("g").join("flow.canvas.json");
        assert_eq!(base_dir_of(&abs), Some(std::env::temp_dir().join("g")));
    }

    #[test]
    fn relative_data_path_is_read_from_the_graph_folder() {
        let tmp = TempDir::new("data");
        std::fs::write(tmp.0.join("d.csv"), "id,amount\n1,10\n2,20\n3,30\n").unwrap();
        let c = PromptCfg {
            data_path: "d.csv",
            ..cfg(ko_fixture!("graph.analyze"), PromptTarget::Gpt)
        };
        let out = compile_prompt(&c, "", Some(&tmp.0)).unwrap();
        assert!(out.contains("rows: 3"), "{out}");

        // The file is not in the working folder, so without a base folder it fails, and the error contains the
        // resolved path.
        let err = compile_prompt(&c, "", None).unwrap_err();
        assert!(
            err.contains("Could not read the data file") && err.contains("d.csv"),
            "{err}"
        );
        let missing = PromptCfg {
            data_path: "sub/nope.csv",
            ..cfg(ko_fixture!("graph.analyze"), PromptTarget::Gpt)
        };
        let err = compile_prompt(&missing, "", Some(&tmp.0)).unwrap_err();
        let resolved = tmp.0.join("sub/nope.csv");
        assert!(err.contains(&resolved.display().to_string()), "{err}");
    }

    // ---------- LLM security checks ----------

    #[test]
    fn key_env_must_look_like_an_api_key() {
        for ok in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "MY_PROXY_2_API_KEY",
            "A_API_KEY",
            "A__API_KEY",
        ] {
            assert!(valid_key_env(ok), "{ok}");
        }
        for bad in [
            "",
            "PATH",
            "USERPROFILE",
            "AWS_SECRET_ACCESS_KEY",
            "API_KEY",
            "_API_KEY",
            "anthropic_api_key",
            "Anthropic_API_KEY",
            "ANTHROPIC_API_KEY ",
            " ANTHROPIC_API_KEY",
            "1ABC_API_KEY",
            "AB-C_API_KEY",
            "ABC_API_KEYS",
            "ABC_API_KEY_",
            "ABC.API_KEY",
            ko_fixture!("graph.exec.hangul_key_env"),
        ] {
            assert!(!valid_key_env(bad), "{bad:?}");
        }
    }

    #[test]
    fn llm_node_rejects_foreign_env_names_before_anything_else() {
        let opts = RunOptions {
            allow_custom_endpoint: true,
            ..RunOptions::default()
        };
        for name in [
            "PATH",
            "USERPROFILE",
            "AWS_SECRET_ACCESS_KEY",
            "anthropic_api_key",
            "API_KEY",
            "",
        ] {
            for dry_run in [false, true] {
                let err = run(&llm_node(name, "", dry_run), "hi", &opts).unwrap_err();
                assert!(
                    err.contains("Key environment variable name") && err.contains("is not allowed"),
                    "{name}: {err}"
                );
            }
        }
        // Leading and trailing spaces are tolerated, but the name rule still applies.
        let err = run(&llm_node(" PATH ", "", false), "hi", &opts).unwrap_err();
        assert!(err.contains("PATH"), "{err}");
        // With a valid name, the dry run passes without reading the environment variable.
        let ok = run(&llm_node(" OPENAI_API_KEY ", "", true), "hi", &opts).unwrap();
        assert!(ok.starts_with("[dry-run]"), "{ok}");
    }

    const UNSET_KEY: &str = "CC_EXEC_TEST_UNSET_API_KEY";

    #[test]
    fn saved_keys_are_used_only_in_the_gui_and_only_for_the_providers_own_variable() {
        assert!(std::env::var(UNSET_KEY).is_err(), "the test variable must not be set");
        // The in-memory test store (one per thread) stands in for Windows Credential Manager.
        keys::save("ANTHROPIC_API_KEY", "sk-saved-for-exec-test").unwrap();
        keys::save(UNSET_KEY, "sk-saved-custom-name").unwrap();
        // GUI: the saved key wins over an environment variable of the same name.
        assert_eq!(
            node_key(Provider::Anthropic, "ANTHROPIC_API_KEY", true).as_deref(),
            Ok("sk-saved-for-exec-test")
        );
        // --run-graph: never the saved key.
        assert_ne!(
            node_key(Provider::Anthropic, "ANTHROPIC_API_KEY", false).as_deref(),
            Ok("sk-saved-for-exec-test")
        );
        // Another provider naming that variable does not get the saved key.
        assert_ne!(
            node_key(Provider::OpenAi, "ANTHROPIC_API_KEY", true).as_deref(),
            Ok("sk-saved-for-exec-test")
        );
        // A custom variable name reads only the environment.
        let err = node_key(Provider::Anthropic, UNSET_KEY, true).unwrap_err();
        assert!(err.contains(UNSET_KEY) && err.contains("is not set"), "{err}");
        keys::remove("ANTHROPIC_API_KEY").unwrap();
        keys::remove(UNSET_KEY).unwrap();
    }

    #[test]
    fn custom_endpoint_needs_explicit_trust() {
        assert!(
            std::env::var(UNSET_KEY).is_err(),
            "if the test environment variable is set, the network could be used"
        );
        let trusted = RunOptions {
            allow_custom_endpoint: true,
            ..RunOptions::default()
        };
        let node = llm_node(UNSET_KEY, "https://llm.example.com", false);
        // Without trust it is refused before the key is read.
        let err = run(&node, "hi", &RunOptions::default()).unwrap_err();
        assert!(
            err.contains("is not allowed") && err.contains("--allow-custom-endpoint"),
            "{err}"
        );
        // With trust it passes the endpoint check and stops at the key environment variable check (no network call).
        let err = run(&node, "hi", &trusted).unwrap_err();
        assert!(err.contains(UNSET_KEY) && err.contains("is not set"), "{err}");
        // http is refused even with trust, except for localhost.
        let http = llm_node(UNSET_KEY, "http://llm.example.com", false);
        let err = run(&http, "hi", &trusted).unwrap_err();
        assert!(err.contains("must use https"), "{err}");
        let local = llm_node(UNSET_KEY, "http://127.0.0.1:8080", false);
        let err = run(&local, "hi", &trusted).unwrap_err();
        assert!(err.contains("is not set"), "{err}");
        // The default address passes without trust (it gets as far as the key environment variable check).
        let default = llm_node(UNSET_KEY, "https://api.anthropic.com/", false);
        let err = run(&default, "hi", &RunOptions::default()).unwrap_err();
        assert!(err.contains("is not set"), "{err}");
        // A dry run uses no network, so it passes, but settings that would be blocked in a real run get a warning note.
        let dry = llm_node("OPENAI_API_KEY", "http://evil.example.com", true);
        let out = exec_node(&dry, 1, "hi", &RunOptions::default()).unwrap();
        assert!(out.text.starts_with("[dry-run]"));
        let note = out.note.expect("settings that would be blocked get a note");
        assert!(
            note.starts_with("⚠ Blocked in a real run") && note.contains("is not allowed"),
            "{note}"
        );
        let out = exec_node(&dry, 1, "hi", &trusted).unwrap();
        assert!(
            out.note.is_some_and(|n| n.contains("must use https")),
            "http is blocked even with trust"
        );
        // The default address or a trusted https address gives no note.
        let fine = llm_node("OPENAI_API_KEY", "https://llm.example.com", true);
        assert!(exec_node(&fine, 1, "hi", &trusted).unwrap().note.is_none());
        let plain = llm_node("OPENAI_API_KEY", "", true);
        assert!(
            exec_node(&plain, 1, "hi", &RunOptions::default())
                .unwrap()
                .note
                .is_none()
        );
    }

    #[test]
    fn endpoint_policy_table() {
        let p = Provider::Anthropic;
        for url in [
            "",
            "   ",
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            " HTTPS://API.ANTHROPIC.COM// ",
        ] {
            assert!(check_endpoint(p, url, false).is_ok(), "{url:?}");
        }
        for url in [
            "https://api.openai.com",
            "http://api.anthropic.com",
            "https://api.anthropic.com/v1",
            "https://llm.example.com",
            "http://localhost:11434",
        ] {
            let err = check_endpoint(p, url, false).unwrap_err();
            assert!(err.contains("is not allowed"), "{url}: {err}");
        }
        for url in [
            "https://llm.example.com",
            "https://llm.example.com/v1/",
            "https://localhost",
            "http://localhost:11434",
            "http://127.0.0.1:8080/",
            "http://[::1]:8080",
            "http://[::1]",
            "HTTP://LOCALHOST",
            "  https://x.example  ",
        ] {
            assert!(check_endpoint(p, url, true).is_ok(), "{url:?}");
        }
        for url in [
            "http://evil.example.com",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://evil.com/localhost",
            "http://evil.com#@localhost",
            "http://localhost@evil.com",
            "http://localhost:80@evil.com",
            "http://localhost\\@evil.com",
            "http://localhost.:8080",
            "http://[::2]:8080",
            "http://0.0.0.0:8080",
            "ftp://localhost",
            "localhost:8080",
            "http://",
            "https:///x",
            "http://[::1",
            "http://localhost:80x",
            "http://localhost:",
            "http://local host",
        ] {
            assert!(check_endpoint(p, url, true).is_err(), "{url:?}");
        }
    }

    // ---------- LLM reply verdicts ----------

    fn reply(text: &str, stop: Option<&str>, usage: Option<(u64, u64)>) -> llm::LlmReply {
        llm::LlmReply {
            text: text.into(),
            stop_reason: stop.map(String::from),
            usage: usage.map(|(input, output)| llm::Usage { input, output }),
        }
    }

    #[test]
    fn empty_reply_fails_the_node() {
        for stop in [Some("end_turn"), Some("stop"), None] {
            for text in ["", "  \n\t"] {
                let err = judge_reply(reply(text, stop, Some((1, 0))), 100)
                    .err()
                    .expect("it must fail");
                assert!(err.contains("is empty"), "{err}");
                match stop {
                    Some(s) => assert!(err.contains(s), "the stop reason must be included: {err}"),
                    None => assert!(!err.contains("stop reason"), "{err}"),
                }
            }
        }
        let err = judge_reply(reply("", Some("max_tokens"), None), 100).err().unwrap();
        assert!(err.contains("max_tokens"), "{err}");
    }

    #[test]
    fn truncated_or_stopped_reply_keeps_text_with_a_warning() {
        let cut = judge_reply(reply("first part", Some("max_tokens"), Some((812, 403))), 1024).unwrap();
        assert_eq!(cut.text, "first part");
        assert_eq!(
            cut.note.as_deref(),
            Some("⚠ Truncated at the output token limit (1024) (max_tokens) · tokens 812→403")
        );
        let n = |stop| judge_reply(reply("body", Some(stop), None), 50).unwrap().note.unwrap();
        assert!(n("length").contains("output token limit (50)"));
        assert!(n("MAX_TOKENS").starts_with('⚠'));
        assert!(n("model_context_window_exceeded").contains("context window"));
        let refusal = n("refusal");
        assert!(
            refusal.starts_with('⚠') && refusal.contains("stop reason: refusal"),
            "{refusal}"
        );
        assert!(n("SAFETY").contains("SAFETY"));
    }

    #[test]
    fn normal_reply_has_usage_note_only() {
        let ok = judge_reply(reply("answer", Some("end_turn"), Some((812, 403))), 1024).unwrap();
        assert_eq!(ok.note.as_deref(), Some("tokens 812→403"));
        let plain = judge_reply(reply("answer", Some("stop"), None), 1024).unwrap();
        assert_eq!(plain.note, None);
        let unknown = judge_reply(reply("answer", None, None), 1024).unwrap();
        assert_eq!((unknown.text.as_str(), unknown.note), ("answer", None));
    }

    #[test]
    fn llm_without_input_is_reported_before_any_call() {
        let err = run(&llm_node("OPENAI_API_KEY", "", true), "  ", &RunOptions::default()).unwrap_err();
        assert!(err.contains("connect a Prompt node"), "{err}");
    }

    // ---------- English ----------

    #[test]
    fn trust_gate_and_node_errors_in_english() {
        with_lang(Lang::En, || {
            let opts = RunOptions::default();
            assert_eq!(
                run(&NodeKind::new_mcp(), "x", &opts).unwrap_err(),
                "MCP execution is not allowed (tick 'Allow MCP execution' at the top or use --allow-mcp)"
            );
            let allow_mcp = RunOptions {
                allow_mcp: true,
                ..RunOptions::default()
            };
            let mcp = |tool: &str, arguments: &str| NodeKind::Mcp {
                command: "cc-not-started".into(),
                args: String::new(),
                tool: tool.into(),
                arguments: arguments.into(),
            };
            // Both stop before a server is started.
            assert_eq!(
                run(&mcp(" ", "{}"), "x", &allow_mcp).unwrap_err(),
                "The MCP tool name is empty"
            );
            let err = run(&mcp("t", "{"), "x", &allow_mcp).unwrap_err();
            assert!(err.starts_with("arguments JSON error: "), "{err}");

            assert_eq!(
                run(&llm_node("PATH", "", true), "hi", &opts).unwrap_err(),
                "Key environment variable name 'PATH' is not allowed (use uppercase letters, digits and \
                 underscores, ending in _API_KEY; e.g. ANTHROPIC_API_KEY)"
            );
            let p = Provider::Anthropic;
            assert_eq!(
                check_endpoint(p, "https://llm.example.com", false).unwrap_err(),
                "Custom endpoint (https://llm.example.com) is not allowed. The API key would be sent to that \
                 address (tick 'Allow custom endpoint' at the top or use --allow-custom-endpoint)"
            );
            assert_eq!(
                check_endpoint(p, "http://llm.example.com", true).unwrap_err(),
                "Custom endpoints must use https (http is allowed for localhost, 127.0.0.1 and [::1]): \
                 http://llm.example.com"
            );
            assert_eq!(
                check_endpoint(p, "localhost:8080", true).unwrap_err(),
                "Invalid endpoint address: localhost:8080"
            );
            assert_eq!(
                run(&llm_node("OPENAI_API_KEY", "", true), " ", &opts).unwrap_err(),
                "The LLM input is empty (connect a Prompt node)"
            );
            let missing = PromptCfg {
                data_path: "cc-no-such-data.csv",
                ..cfg("x", PromptTarget::Gpt)
            };
            let err = compile_prompt(&missing, "", None).unwrap_err();
            assert!(
                err.starts_with("Could not read the data file (cc-no-such-data.csv): "),
                "{err}"
            );
            assert_eq!(panic_text(Box::new(5u8)), "unknown error");
        });
    }

    #[test]
    fn dry_run_summary_and_reply_notes_in_english() {
        with_lang(Lang::En, || {
            let opts = RunOptions::default();
            // The `[dry-run]` head and the prompt text stay the same; only the summary words are English.
            let out = exec_node(
                &llm_node("OPENAI_API_KEY", "http://evil.example.com", true),
                1,
                "hi",
                &opts,
            )
            .unwrap();
            assert_eq!(out.text, "[dry-run] Anthropic/m max_tokens=100 prompt 2 chars\n---\nhi");
            let note = out.note.expect("settings that would be blocked get a note");
            assert!(
                note.starts_with("⚠ Blocked in a real run: Custom endpoint (http://evil.example.com) is not allowed."),
                "{note}"
            );
            let hangul = ko_fixture!("graph.exec.one_char");
            let one = run(&llm_node("OPENAI_API_KEY", "", true), hangul, &opts).unwrap();
            assert_eq!(
                one,
                format!("[dry-run] Anthropic/m max_tokens=100 prompt 1 char\n---\n{hangul}")
            );

            let cut = judge_reply(reply("first part", Some("max_tokens"), Some((812, 403))), 1024).unwrap();
            assert_eq!(
                cut.note.as_deref(),
                Some("⚠ Truncated at the output token limit (1024) (max_tokens) · tokens 812→403")
            );
            let n = |stop| judge_reply(reply("body", Some(stop), None), 50).unwrap().note.unwrap();
            assert_eq!(
                n("model_context_window_exceeded"),
                "⚠ Truncated: the model's context window is full (model_context_window_exceeded)"
            );
            assert_eq!(n("refusal"), "⚠ The response did not finish (stop reason: refusal)");
            let err = judge_reply(reply("", Some("end_turn"), None), 50).err().unwrap();
            assert_eq!(err, "The LLM response body is empty (stop reason: end_turn)");
            let err = judge_reply(reply(" ", None, None), 50).err().unwrap();
            assert_eq!(err, "The LLM response body is empty");
        });
        // The Korean summary line is the same as before.
        let ko = with_lang(Lang::Ko, || {
            run(&llm_node("OPENAI_API_KEY", "", true), "hi", &RunOptions::default())
        });
        assert_eq!(ko.unwrap(), ko_fixture!("graph.exec.dry_run_summary_ko"));
    }

    /// Default Prompt nodes created in English also compile without lint errors, and the whole graph runs.
    #[test]
    fn english_default_nodes_run_end_to_end() {
        with_lang(Lang::En, || {
            let (g, _) = chain4();
            let r = run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
            assert!(r.ok(), "{:?}", r.errors);
            assert!(
                r.final_text
                    .starts_with("[dry-run] Anthropic/claude-sonnet-5-5 max_tokens=1024 prompt "),
                "{}",
                r.final_text
            );
            assert!(
                r.final_text.contains("Analyze the following:\ninput text"),
                "{}",
                r.final_text
            );
            assert!(r.final_text.contains("Do not infer anything that is not in the data"));
        });
    }

    #[test]
    fn a_failed_run_without_an_output_node_has_no_result() {
        // Text -> MCP (not allowed): the original text must not be shown as the result of the graph
        let mut g = Graph::default();
        let t = g.add(
            NodeKind::Text {
                text: "secret original".into(),
            },
            [0.0; 2],
        );
        let m = g.add(NodeKind::new_mcp(), [0.0; 2]);
        g.connect(t, m).unwrap();
        let r = run_graph(&g, &RunOptions::default(), |_| {}).unwrap();
        assert!(r.errors.contains_key(&m));
        assert_eq!(r.final_text, "");
    }

    #[test]
    fn a_prompt_node_does_not_open_network_paths() {
        if !cfg!(windows) {
            return;
        }
        let node = NodeKind::Prompt {
            role: "r".into(),
            constraints: String::new(),
            fields: "a".into(),
            target: PromptTarget::Claude,
            format: PromptFormat::Json,
            data_path: r"\\evil.example\share\a.csv".into(),
        };
        let e = with_lang(Lang::En, || run(&node, "x", &RunOptions::default())).unwrap_err();
        assert!(e.contains("Network and device paths"), "{e}");
    }

    #[test]
    fn a_pasted_api_key_is_not_echoed_in_the_name_error() {
        let key = "sk-ant-api03-FAKEKEYFORTESTING0000000000";
        let e = with_lang(Lang::En, || run(&llm_node(key, "", true), "hi", &RunOptions::default())).unwrap_err();
        assert!(!e.contains(key) && !e.contains("FAKEKEY"), "{e}");
        // A short plain name that is merely wrong is still shown, so the user can see what to fix
        assert_eq!(shown_key_env("anthropic_api_key"), "anthropic_api_key");
        assert_eq!(shown_key_env(key), crate::keys::tail(key));
    }

    #[test]
    fn an_endpoint_with_a_query_or_fragment_is_refused() {
        for url in [
            "https://gw.example.com/?k=x",
            "https://gw.example.com/v1#frag",
            "http://localhost:1234?x=1",
        ] {
            assert!(check_endpoint(Provider::OpenAi, url, true).is_err(), "{url}");
        }
        assert!(check_endpoint(Provider::OpenAi, "https://gw.example.com/v1", true).is_ok());
    }
}
