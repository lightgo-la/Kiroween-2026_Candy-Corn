//! LLM API client (Anthropic / OpenAI / Gemini). Keys are read only from environment variables.

use super::graph::Provider;
use serde_json::{Value, json};
use std::time::Duration;

pub struct LlmRequest<'a> {
    pub provider: Provider,
    pub model: &'a str,
    pub system: &'a str,
    pub prompt: &'a str,
    pub api_key: &'a str,
    pub base_url: &'a str,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

/// Builds the request URL, headers and body. Kept separate so it can be unit tested without a network.
pub fn build_request(r: &LlmRequest) -> (String, Vec<(&'static str, String)>, Value) {
    let base = if r.base_url.trim().is_empty() {
        r.provider.default_base_url()
    } else {
        r.base_url.trim()
    };
    let base = base.trim_end_matches('/');
    match r.provider {
        Provider::Anthropic => {
            let mut body = json!({
                "model": r.model,
                "max_tokens": r.max_tokens,
                "messages": [{ "role": "user", "content": r.prompt }],
            });
            if !r.system.is_empty() {
                body["system"] = json!(r.system);
            }
            if let Some(t) = r.temperature {
                body["temperature"] = json!(t);
            }
            (
                format!("{base}/v1/messages"),
                vec![
                    ("x-api-key", r.api_key.to_string()),
                    ("anthropic-version", "2023-06-01".into()),
                ],
                body,
            )
        }
        Provider::OpenAi => {
            let mut messages = vec![];
            if !r.system.is_empty() {
                messages.push(json!({ "role": "system", "content": r.system }));
            }
            messages.push(json!({ "role": "user", "content": r.prompt }));
            let mut body = json!({ "model": r.model, "messages": messages, "max_completion_tokens": r.max_tokens });
            if let Some(t) = r.temperature {
                body["temperature"] = json!(t);
            }
            (
                format!("{base}/v1/chat/completions"),
                vec![("authorization", format!("Bearer {}", r.api_key))],
                body,
            )
        }
        Provider::Gemini => {
            let mut cfg = json!({ "maxOutputTokens": r.max_tokens });
            if let Some(t) = r.temperature {
                cfg["temperature"] = json!(t);
            }
            let mut body = json!({
                "contents": [{ "role": "user", "parts": [{ "text": r.prompt }] }],
                "generationConfig": cfg,
            });
            if !r.system.is_empty() {
                body["systemInstruction"] = json!({ "parts": [{ "text": r.system }] });
            }
            (
                format!("{base}/v1beta/models/{}:generateContent", r.model),
                vec![("x-goog-api-key", r.api_key.to_string())],
                body,
            )
        }
    }
}

/// Token usage reported by the provider. Gemini's output includes thinking tokens (they count toward the output
/// limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// LLM response text and the stop reason reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LlmReply {
    pub text: String,
    /// Raw stop reason: Anthropic `stop_reason`, OpenAI `finish_reason`, Gemini `finishReason`. None if the
    /// response has none
    pub stop_reason: Option<String>,
    /// Token usage reported by the provider (None if the response has none)
    pub usage: Option<Usage>,
}

/// Stop reason category. The values do not overlap between providers, so the raw text alone decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Finished normally (end_turn, stop_sequence, stop, STOP)
    Complete,
    /// Truncated at the output token limit (max_tokens, length, MAX_TOKENS, CONTINUATION)
    MaxTokens,
    /// Truncated because the model's context window is full (model_context_window_exceeded)
    ContextWindow,
    /// Stopped for another reason such as a refusal or a safety filter (refusal, content_filter, SAFETY, ...)
    Stopped,
    /// The response has no stop reason
    Unknown,
}

impl LlmReply {
    pub fn finish(&self) -> Finish {
        match self.stop_reason.as_deref() {
            None => Finish::Unknown,
            Some("end_turn" | "stop_sequence" | "stop" | "STOP") => Finish::Complete,
            Some("max_tokens" | "length" | "MAX_TOKENS" | "CONTINUATION") => Finish::MaxTokens,
            Some("model_context_window_exceeded") => Finish::ContextWindow,
            Some(_) => Finish::Stopped,
        }
    }
}

/// Extracts the text and the stop reason from a provider's response.
/// A response truncated or stopped without text (e.g. Gemini using up the limit on thinking tokens) returns an empty
/// text and the stop reason; a response without text that finished normally or has an unknown stop reason is an
/// error that contains the raw response.
pub fn extract_reply(provider: Provider, v: &Value) -> Result<LlmReply, String> {
    let join = |blocks: &Value| {
        blocks
            .as_array()
            .map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""))
    };
    let (text, stop) = match provider {
        Provider::Anthropic => (join(&v["content"]), &v["stop_reason"]),
        Provider::OpenAi => (
            v["choices"][0]["message"]["content"].as_str().map(String::from),
            &v["choices"][0]["finish_reason"],
        ),
        Provider::Gemini => (
            join(&v["candidates"][0]["content"]["parts"]),
            &v["candidates"][0]["finishReason"],
        ),
    };
    let has_text = text.is_some();
    let reply = LlmReply {
        text: text.unwrap_or_default(),
        stop_reason: stop.as_str().filter(|s| !s.is_empty()).map(String::from),
        usage: extract_usage(provider, v),
    };
    match reply.finish() {
        _ if has_text => Ok(reply),
        Finish::MaxTokens | Finish::ContextWindow | Finish::Stopped => Ok(reply),
        Finish::Complete | Finish::Unknown => {
            Err(trf!("No text found in the response: {}", truncate(&v.to_string(), 300)))
        }
    }
}

/// Reads the provider's usage fields. None if neither the input nor the output count is present.
fn extract_usage(provider: Provider, v: &Value) -> Option<Usage> {
    let n = |x: &Value| x.as_u64();
    let (input, output) = match provider {
        Provider::Anthropic => (n(&v["usage"]["input_tokens"]), n(&v["usage"]["output_tokens"])),
        Provider::OpenAi => (n(&v["usage"]["prompt_tokens"]), n(&v["usage"]["completion_tokens"])),
        Provider::Gemini => {
            let m = &v["usageMetadata"];
            let out = match (n(&m["candidatesTokenCount"]), n(&m["thoughtsTokenCount"])) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            };
            (n(&m["promptTokenCount"]), out)
        }
    };
    (input.is_some() || output.is_some()).then(|| Usage {
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
    })
}

/// HTTP statuses that may succeed when retried a little later (rate limit, server overload or transient error).
fn retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// Wait before retrying. Follows `retry-after` (seconds) if present, but never longer than 10 seconds.
fn retry_delay(retry_after: Option<&str>, attempt: u32) -> Duration {
    let fallback = Duration::from_secs(1 << attempt.min(3)); // 1 s, 2 s, 4 s...
    retry_after
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(fallback, |s| Duration::from_secs(s.min(10)))
}

/// Transient errors (429/5xx/529) are retried at most this many times.
const MAX_RETRIES: u32 = 2;

/// Timeout for the whole request. Larger output limits take longer to generate, so it grows (2048 → 120 s,
/// 32000 → about 14 min).
fn timeout_for(max_tokens: u32) -> Duration {
    Duration::from_secs((60 + u64::from(max_tokens) / 40).clamp(120, 900))
}

pub fn call(r: &LlmRequest) -> Result<LlmReply, String> {
    if r.api_key.is_empty() {
        return Err(tr!("The API key is empty").into());
    }
    let (url, headers, body) = build_request(r);
    // Redirects are not followed: ureq strips only Authorization and would send x-api-key and x-goog-api-key to
    // the redirect target unchanged, so a 3xx is returned as an error.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout_for(r.max_tokens)))
        .build()
        .into();
    let mut attempt = 0;
    let (status, text) = loop {
        let mut req = agent.post(&url);
        for (k, v) in &headers {
            req = req.header(*k, v);
        }
        let mut resp = req.send_json(&body).map_err(|e| trf!("Network error: {e}", e = e))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(String::from);
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| trf!("Failed to read the response: {e}", e = e))?;
        if retryable(status) && attempt < MAX_RETRIES {
            std::thread::sleep(retry_delay(retry_after.as_deref(), attempt));
            attempt += 1;
            continue;
        }
        break (status, text);
    };
    read_reply(r.provider, r.api_key, status, attempt, &text)
}

/// Turns the last HTTP response into the result (kept separate so it can be unit tested without a network).
/// A non-2xx status is an error with the status code, the retry count and the start of the body; a 2xx response has
/// its text and stop reason read from the JSON.
fn read_reply(provider: Provider, api_key: &str, status: u16, retries: u32, text: &str) -> Result<LlmReply, String> {
    if !(200..300).contains(&status) {
        // Mask the key in case the response body echoes the request key.
        let safe = if api_key.is_empty() {
            text.to_string()
        } else {
            text.replace(api_key, "***")
        };
        let retried = match retries {
            0 => String::new(),
            1 => tr!(" (after 1 retry)").into(),
            n => trf!(" (after {n} retries)", n = n),
        };
        return Err(format!("HTTP {status}{retried}: {}", truncate(&safe, 500)));
    }
    let v: Value = serde_json::from_str(text).map_err(|e| trf!("JSON parse error: {e}", e = e))?;
    extract_reply(provider, &v)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, with_lang};

    fn req(p: Provider) -> LlmRequest<'static> {
        LlmRequest {
            provider: p,
            model: "m",
            system: "sys",
            prompt: "hi",
            api_key: "K",
            base_url: "",
            max_tokens: 10,
            temperature: None,
        }
    }

    #[test]
    fn anthropic_shape() {
        let (url, h, b) = build_request(&req(Provider::Anthropic));
        assert_eq!(url, "https://api.anthropic.com/v1/messages");
        assert!(h.iter().any(|(k, v)| *k == "x-api-key" && v == "K"));
        assert_eq!(b["system"], "sys");
        assert_eq!(b["messages"][0]["content"], "hi");
    }

    #[test]
    fn openai_and_gemini_shape() {
        let (url, h, b) = build_request(&req(Provider::OpenAi));
        assert!(url.ends_with("/v1/chat/completions"));
        assert!(h.iter().any(|(_, v)| v == "Bearer K"));
        assert_eq!(b["messages"][0]["role"], "system");
        let (url, _, b) = build_request(&req(Provider::Gemini));
        assert!(url.ends_with("/v1beta/models/m:generateContent"));
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "sys");
    }

    #[test]
    fn custom_base_url_trims_slash() {
        let mut r = req(Provider::OpenAi);
        r.base_url = "http://localhost:8080/";
        assert_eq!(build_request(&r).0, "http://localhost:8080/v1/chat/completions");
    }

    fn reply_of(p: Provider, v: Value) -> (String, Option<String>, Finish) {
        let r = extract_reply(p, &v).unwrap();
        let finish = r.finish();
        (r.text, r.stop_reason, finish)
    }

    fn owned(text: &str, stop: &str, finish: Finish) -> (String, Option<String>, Finish) {
        (text.to_string(), Some(stop.to_string()), finish)
    }

    #[test]
    fn extracts_text_and_stop_reason() {
        let a = json!({"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}],"stop_reason":"end_turn"});
        assert_eq!(
            reply_of(Provider::Anthropic, a),
            owned("ab", "end_turn", Finish::Complete)
        );
        let o = json!({"choices":[{"message":{"content":"x"},"finish_reason":"length"}]});
        assert_eq!(reply_of(Provider::OpenAi, o), owned("x", "length", Finish::MaxTokens));
        let g = json!({"candidates":[{"content":{"parts":[{"text":"y"}]},"finishReason":"STOP"}]});
        assert_eq!(reply_of(Provider::Gemini, g), owned("y", "STOP", Finish::Complete));
        assert!(extract_reply(Provider::OpenAi, &json!({})).is_err());
    }

    #[test]
    fn reply_without_text_keeps_stop_reason() {
        // A Gemini response that ended without parts after using up the limit on thinking tokens
        let g = json!({"candidates":[{"content":{"role":"model"},"finishReason":"MAX_TOKENS"}]});
        assert_eq!(
            reply_of(Provider::Gemini, g),
            owned("", "MAX_TOKENS", Finish::MaxTokens)
        );
        let blocked = json!({"candidates":[{"finishReason":"SAFETY"}]});
        assert_eq!(
            reply_of(Provider::Gemini, blocked),
            owned("", "SAFETY", Finish::Stopped)
        );
        // A normal finish without text (e.g. only a refusal message) stays an error that shows the raw response
        let refusal = json!({"choices":[{"message":{"content":null,"refusal":"refused"},"finish_reason":"stop"}]});
        assert!(
            extract_reply(Provider::OpenAi, &refusal)
                .unwrap_err()
                .contains("refused")
        );
        // A missing or empty stop reason is Unknown
        let none = json!({"content":[{"type":"text","text":"t"}],"stop_reason":""});
        assert_eq!(
            reply_of(Provider::Anthropic, none),
            ("t".to_string(), None, Finish::Unknown)
        );
    }

    #[test]
    fn classifies_stop_reasons() {
        use Finish::*;
        for (raw, want) in [
            ("end_turn", Complete),
            ("stop_sequence", Complete),
            ("stop", Complete),
            ("STOP", Complete),
            ("max_tokens", MaxTokens),
            ("length", MaxTokens),
            ("MAX_TOKENS", MaxTokens),
            ("CONTINUATION", MaxTokens),
            ("model_context_window_exceeded", ContextWindow),
            ("refusal", Stopped),
            ("content_filter", Stopped),
            ("RECITATION", Stopped),
        ] {
            let r = LlmReply {
                text: String::new(),
                stop_reason: Some(raw.into()),
                usage: None,
            };
            assert_eq!(r.finish(), want, "{raw}");
        }
    }

    #[test]
    fn extracts_usage_per_provider() {
        let a = json!({"content":[],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":3}});
        let u = |p, v: Value| extract_reply(p, &v).unwrap().usage;
        assert_eq!(u(Provider::Anthropic, a), Some(Usage { input: 12, output: 3 }));
        let o = json!({"choices":[{"message":{"content":"x"},"finish_reason":"stop"}],
                       "usage":{"prompt_tokens":7,"completion_tokens":2}});
        assert_eq!(u(Provider::OpenAi, o), Some(Usage { input: 7, output: 2 }));
        // Gemini output tokens = text + thinking tokens
        let g = json!({"candidates":[{"content":{"parts":[{"text":"y"}]},"finishReason":"STOP"}],
                       "usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":4,"thoughtsTokenCount":40}});
        assert_eq!(u(Provider::Gemini, g), Some(Usage { input: 9, output: 44 }));
        let none = json!({"choices":[{"message":{"content":"x"}}]});
        assert_eq!(u(Provider::OpenAi, none), None);
    }

    #[test]
    fn retry_policy() {
        for s in [429, 500, 502, 503, 504, 529] {
            assert!(retryable(s), "{s}");
        }
        for s in [200, 400, 401, 403, 404, 422] {
            assert!(!retryable(s), "{s}");
        }
        assert_eq!(retry_delay(Some("3"), 0), Duration::from_secs(3));
        assert_eq!(retry_delay(Some("120"), 0), Duration::from_secs(10)); // cap
        assert_eq!(retry_delay(Some("Wed, 21 Oct"), 1), Duration::from_secs(2)); // a date uses the default interval
        assert_eq!(retry_delay(None, 0), Duration::from_secs(1));
    }

    #[test]
    fn timeout_grows_with_output_limit() {
        assert_eq!(timeout_for(1024), Duration::from_secs(120));
        assert_eq!(timeout_for(2048), Duration::from_secs(120));
        assert_eq!(timeout_for(8192), Duration::from_secs(264));
        assert_eq!(timeout_for(32_000), Duration::from_secs(860));
        assert_eq!(timeout_for(u32::MAX), Duration::from_secs(900));
    }

    #[test]
    fn http_and_json_errors_keep_status_and_retry_count() {
        with_lang(Lang::Ko, || {
            let ok = r#"{"choices":[{"message":{"content":"x"},"finish_reason":"stop"}]}"#;
            assert_eq!(read_reply(Provider::OpenAi, "K", 200, 2, ok).unwrap().text, "x");
            // A key echoed in the body is masked.
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 401, 0, "bad key K").unwrap_err(),
                "HTTP 401: bad key ***"
            );
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 529, 1, "busy").unwrap_err(),
                format!("HTTP 529{}: busy", ko_text!(" (after 1 retry)"))
            );
            let retries = format!(ko_text!(" (after {n} retries)"), n = 2);
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 503, 2, "busy").unwrap_err(),
                format!("HTTP 503{retries}: busy")
            );
            let parse = serde_json::from_str::<Value>("not json").unwrap_err();
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 200, 0, "not json").unwrap_err(),
                format!(ko_text!("JSON parse error: {e}"), e = parse)
            );
            let mut r = req(Provider::Anthropic);
            r.api_key = "";
            assert_eq!(call(&r).unwrap_err(), ko_text!("The API key is empty"));
        });
    }

    #[test]
    fn client_errors_in_english() {
        with_lang(Lang::En, || {
            let mut r = req(Provider::Anthropic);
            r.api_key = "";
            assert_eq!(call(&r).unwrap_err(), "The API key is empty");
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 401, 0, "bad key K").unwrap_err(),
                "HTTP 401: bad key ***"
            );
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 529, 1, "busy").unwrap_err(),
                "HTTP 529 (after 1 retry): busy"
            );
            assert_eq!(
                read_reply(Provider::Gemini, "K", 503, 2, "busy").unwrap_err(),
                "HTTP 503 (after 2 retries): busy"
            );
            let err = read_reply(Provider::OpenAi, "K", 200, 0, "not json").unwrap_err();
            assert!(err.starts_with("JSON parse error: "), "{err}");
            assert_eq!(
                read_reply(Provider::OpenAi, "K", 200, 0, "{}").unwrap_err(),
                "No text found in the response: {}"
            );
        });
    }
}
