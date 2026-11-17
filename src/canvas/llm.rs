//! LLM API 클라이언트(Anthropic / OpenAI / Gemini). 키는 환경 변수에서만 읽는다.

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

/// 요청 URL, 헤더, 본문을 만든다. 네트워크 없이 단위 테스트할 수 있도록 분리했다.
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

/// 제공자가 보고한 토큰 사용량. Gemini의 output은 사고 토큰을 포함한다(출력 한도에 함께 잡힌다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// LLM 응답 본문과 제공자가 알려 준 종료 이유.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LlmReply {
    pub text: String,
    /// 종료 이유 원문: Anthropic `stop_reason`, OpenAI `finish_reason`, Gemini `finishReason`. 응답에 없으면 None
    pub stop_reason: Option<String>,
    /// 제공자가 보고한 토큰 사용량 (응답에 없으면 None)
    pub usage: Option<Usage>,
}

/// 종료 이유 분류. 제공자마다 값이 겹치지 않아 원문만으로 판별한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// 정상 종료 (end_turn, stop_sequence, stop, STOP)
    Complete,
    /// 출력 토큰 한도에서 잘림 (max_tokens, length, MAX_TOKENS, CONTINUATION)
    MaxTokens,
    /// 모델 컨텍스트 창이 가득 차 잘림 (model_context_window_exceeded)
    ContextWindow,
    /// 거부·안전 필터 등 그 밖의 이유로 중단 (refusal, content_filter, SAFETY 등)
    Stopped,
    /// 응답에 종료 이유가 없음
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

/// 제공자별 응답에서 본문과 종료 이유를 꺼낸다.
/// 본문 없이 잘리거나 중단된 응답(사고 토큰으로 한도를 다 쓴 Gemini 등)은 빈 본문과 종료 이유를 돌려주고,
/// 정상 종료이거나 종료 이유를 모르는데 본문이 없으면 응답 원문을 담은 오류로 본다.
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
        Finish::Complete | Finish::Unknown => Err(trf!(
            "응답에서 텍스트를 찾지 못했습니다: {}",
            "No text found in the response: {}",
            truncate(&v.to_string(), 300)
        )),
    }
}

/// 제공자별 usage 필드를 읽는다. 입력·출력 중 하나도 없으면 None.
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

/// 잠시 뒤 다시 시도하면 될 수 있는 HTTP 상태 (요청 한도, 서버 과부하·일시 오류).
fn retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// 다시 시도하기 전 대기 시간. `retry-after`(초)가 있으면 따르되 10초를 넘기지 않는다.
fn retry_delay(retry_after: Option<&str>, attempt: u32) -> Duration {
    let fallback = Duration::from_secs(1 << attempt.min(3)); // 1초, 2초, 4초...
    retry_after
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(fallback, |s| Duration::from_secs(s.min(10)))
}

/// 일시 오류(429/5xx/529)는 최대 이만큼 다시 시도한다.
const MAX_RETRIES: u32 = 2;

/// 요청 전체 제한 시간. 출력 한도가 클수록 생성이 오래 걸리므로 늘린다(2048 → 120초, 32000 → 약 14분).
fn timeout_for(max_tokens: u32) -> Duration {
    Duration::from_secs((60 + u64::from(max_tokens) / 40).clamp(120, 900))
}

pub fn call(r: &LlmRequest) -> Result<LlmReply, String> {
    if r.api_key.is_empty() {
        return Err(tr!("API 키가 비어 있습니다", "The API key is empty").into());
    }
    let (url, headers, body) = build_request(r);
    // 리디렉션은 따라가지 않는다: ureq는 Authorization만 떼고 x-api-key·x-goog-api-key는
    // 리디렉션 대상에게 그대로 보내므로, 3xx는 오류로 돌려준다.
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
        let mut resp = req
            .send_json(&body)
            .map_err(|e| trf!("네트워크 오류: {e}", "Network error: {e}"))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .map(String::from);
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| trf!("응답 읽기 실패: {e}", "Failed to read the response: {e}"))?;
        if retryable(status) && attempt < MAX_RETRIES {
            std::thread::sleep(retry_delay(retry_after.as_deref(), attempt));
            attempt += 1;
            continue;
        }
        break (status, text);
    };
    read_reply(r.provider, r.api_key, status, attempt, &text)
}

/// 마지막 HTTP 응답을 결과로 바꾼다(네트워크 없이 단위 테스트할 수 있도록 분리했다).
/// 2xx가 아니면 상태 코드·재시도 횟수·본문 앞부분을 담은 오류, 2xx면 JSON에서 본문과 종료 이유를 꺼낸다.
fn read_reply(provider: Provider, api_key: &str, status: u16, retries: u32, text: &str) -> Result<LlmReply, String> {
    if !(200..300).contains(&status) {
        // 응답 본문에 요청 키가 에코될 가능성에 대비해 키를 가린다.
        let safe = if api_key.is_empty() {
            text.to_string()
        } else {
            text.replace(api_key, "***")
        };
        let retried = match retries {
            0 => String::new(),
            1 => tr!(" (1회 다시 시도 후)", " (after 1 retry)").into(),
            n => trf!(" ({n}회 다시 시도 후)", " (after {n} retries)"),
        };
        return Err(format!("HTTP {status}{retried}: {}", truncate(&safe, 500)));
    }
    let v: Value = serde_json::from_str(text).map_err(|e| trf!("JSON 파싱 실패: {e}", "JSON parse error: {e}"))?;
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
        // 사고 토큰으로 한도를 다 써서 parts 없이 끝난 Gemini 응답
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
        // 정상 종료인데 본문이 없으면(거부 문구만 온 경우 등) 원문을 보여 주는 오류로 둔다
        let refusal = json!({"choices":[{"message":{"content":null,"refusal":"거부"},"finish_reason":"stop"}]});
        assert!(extract_reply(Provider::OpenAi, &refusal).unwrap_err().contains("거부"));
        // 종료 이유가 없거나 빈 문자열이면 Unknown
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
        // Gemini 출력 토큰 = 본문 + 사고 토큰
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
        assert_eq!(retry_delay(Some("120"), 0), Duration::from_secs(10)); // 상한
        assert_eq!(retry_delay(Some("Wed, 21 Oct"), 1), Duration::from_secs(2)); // 날짜 형식은 기본 간격
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
        let ok = r#"{"choices":[{"message":{"content":"x"},"finish_reason":"stop"}]}"#;
        assert_eq!(read_reply(Provider::OpenAi, "K", 200, 2, ok).unwrap().text, "x");
        // 본문에 에코된 키는 가린다.
        assert_eq!(
            read_reply(Provider::OpenAi, "K", 401, 0, "bad key K").unwrap_err(),
            "HTTP 401: bad key ***"
        );
        assert_eq!(
            read_reply(Provider::OpenAi, "K", 529, 1, "busy").unwrap_err(),
            "HTTP 529 (1회 다시 시도 후): busy"
        );
        assert_eq!(
            read_reply(Provider::OpenAi, "K", 503, 2, "busy").unwrap_err(),
            "HTTP 503 (2회 다시 시도 후): busy"
        );
        let err = read_reply(Provider::OpenAi, "K", 200, 0, "not json").unwrap_err();
        assert!(err.starts_with("JSON 파싱 실패: "), "{err}");
        let mut r = req(Provider::Anthropic);
        r.api_key = "";
        assert_eq!(call(&r).unwrap_err(), "API 키가 비어 있습니다");
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
