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

/// 제공자별 응답에서 텍스트를 추출한다.
pub fn extract_text(provider: Provider, v: &Value) -> Result<String, String> {
    let text = match provider {
        Provider::Anthropic => v["content"]
            .as_array()
            .map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")),
        Provider::OpenAi => v["choices"][0]["message"]["content"].as_str().map(String::from),
        Provider::Gemini => v["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")),
    };
    text.ok_or_else(|| format!("응답에서 텍스트를 찾지 못했습니다: {}", truncate(&v.to_string(), 300)))
}

pub fn call(r: &LlmRequest) -> Result<String, String> {
    if r.api_key.is_empty() {
        return Err("API 키가 비어 있습니다".into());
    }
    let (url, headers, body) = build_request(r);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into();
    let mut req = agent.post(&url);
    for (k, v) in &headers {
        req = req.header(*k, v);
    }
    let mut resp = req.send_json(&body).map_err(|e| format!("네트워크 오류: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("응답 읽기 실패: {e}"))?;
    if !(200..300).contains(&status) {
        // 응답 본문에 요청 키가 에코될 가능성에 대비해 키를 가린다.
        let safe = text.replace(r.api_key, "***");
        return Err(format!("HTTP {status}: {}", truncate(&safe, 500)));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("JSON 파싱 실패: {e}"))?;
    extract_text(r.provider, &v)
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

    #[test]
    fn extracts_text() {
        let a = json!({"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]});
        assert_eq!(extract_text(Provider::Anthropic, &a).unwrap(), "ab");
        let o = json!({"choices":[{"message":{"content":"x"}}]});
        assert_eq!(extract_text(Provider::OpenAi, &o).unwrap(), "x");
        let g = json!({"candidates":[{"content":{"parts":[{"text":"y"}]}}]});
        assert_eq!(extract_text(Provider::Gemini, &g).unwrap(), "y");
        assert!(extract_text(Provider::OpenAi, &json!({})).is_err());
    }
}
