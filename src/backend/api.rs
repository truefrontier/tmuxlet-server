use super::{ApiBackend, BackendError, DispatchResult};
use crate::env::Env;
use crate::http_client;
use std::time::Duration;

/// Split a base_url into (scheme, host, port, path).
pub fn split_url(base_url: &str) -> (String, String, u16, String) {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("http", base_url));
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    // U-19: a bracketed IPv6 literal, e.g. `[::1]:11434`.
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        match after_bracket.split_once(']') {
            Some((h, tail)) => {
                let port = tail
                    .strip_prefix(':')
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(default_port);
                (h.to_string(), port)
            }
            None => (after_bracket.to_string(), default_port),
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
            None => (authority.to_string(), default_port),
        }
    };
    (scheme.to_string(), host, port, path.to_string())
}

/// Merge order: caller payload -> shallow-merge extra_body (overrides caller) ->
/// pin model last (neither caller nor extra_body can override). `stream` is
/// dropped (V1 always requests non-streamed upstream).
pub fn build_body(
    caller: &serde_json::Value,
    extra: &serde_json::Value,
    model: &str,
) -> serde_json::Value {
    let mut body = caller.clone();
    if let Some(obj) = body.as_object_mut() {
        if let Some(ex) = extra.as_object() {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        obj.insert("model".into(), serde_json::json!(model));
        obj.remove("stream");
    }
    body
}

pub fn extract_content(name: &str, resp: &str) -> Result<String, BackendError> {
    let v: serde_json::Value =
        serde_json::from_str(resp).map_err(|e| BackendError::Parse(name.into(), e.to_string()))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| BackendError::Parse(name.into(), "no choices[0].message.content".into()))
}

/// U-7: condense an upstream error body to a single-line, control-stripped
/// snippet (first 300 chars) so the user sees the upstream's actual message
/// ("invalid key", "out of credits", "unknown model") instead of a bare status.
fn error_snippet(body: &str) -> String {
    let spaced: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    spaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

/// Forward to an OpenAI-compatible HTTP upstream: build the merged body, POST
/// it, then re-extract `choices[0].message.content`. Non-2xx becomes an Http
/// error so the chain falls through to the next backend.
pub fn dispatch(
    b: &ApiBackend,
    raw_messages: &serde_json::Value,
    env: &Env,
    timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    let (scheme, host, port, base_path) = split_url(&b.base_url);
    let path = format!("{}/chat/completions", base_path.trim_end_matches('/'));
    let body = build_body(raw_messages, &b.extra_body, &b.model).to_string();
    let bearer = b
        .api_key_env
        .as_ref()
        .and_then(|k| env.get(k).map(str::to_string));
    let (status, resp, retry_after) = http_client::post_json(
        &scheme,
        &host,
        port,
        &path,
        bearer.as_deref(),
        &body,
        timeout,
        b.max_response_bytes as usize,
        b.ca_file.as_deref(),
    )
    .map_err(|e| {
        let m = e.to_string();
        // S-5: an over-cap response is a Parse-class failure (chain advances);
        // everything else (connect/read) is Spawn-class. The cap is signalled by
        // ErrorKind::FileTooLarge, not the message text.
        if e.kind() == std::io::ErrorKind::FileTooLarge {
            BackendError::Parse(b.name.clone(), m)
        } else {
            BackendError::Spawn(b.name.clone(), m)
        }
    })?;
    if !(200..300).contains(&status) {
        let detail = error_snippet(&resp);
        // §4.3: match against the FULL body, not the snippet — error_snippet caps
        // at 300 chars, and an upstream that wraps its message in a verbose
        // envelope could push the marker past the cap. The snippet is still what
        // we store.
        if super::is_context_overflow(status, &resp) {
            return Err(BackendError::Overflow(b.name.clone(), detail));
        }
        if status == 429 {
            return Err(BackendError::RateLimited(
                b.name.clone(),
                retry_after.map(Duration::from_secs),
                detail,
            ));
        }
        return Err(BackendError::Http(b.name.clone(), status, detail));
    }
    let content = extract_content(&b.name, &resp)?;
    Ok(DispatchResult {
        content,
        model_label: b.model.clone(),
    })
}

/// §9.2: dispatch with `stream: true` upstream and relay deltas as they arrive.
///
/// **The commit point is the first delta.** Everything that can fail — connect,
/// TLS, a non-2xx status, an unparseable first frame — fails before `on_delta`
/// is ever called, so the chain can still fall through with nothing written to
/// the client. After the first delta there is no un-sending it, and this backend
/// owns the response.
///
/// Returns the accumulated text so the caller can still log and record it, and
/// so a stream that produced nothing is indistinguishable from an empty reply
/// (and is rejected by the same NonEmptyGate).
pub fn dispatch_stream(
    b: &ApiBackend,
    raw_messages: &serde_json::Value,
    env: &Env,
    timeout: Duration,
    on_delta: &mut dyn FnMut(&str) -> bool,
) -> Result<DispatchResult, BackendError> {
    let (scheme, host, port, base_path) = split_url(&b.base_url);
    let path = format!("{}/chat/completions", base_path.trim_end_matches('/'));
    let mut body = build_body(raw_messages, &b.extra_body, &b.model);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".into(), serde_json::json!(true));
    }
    let bearer = b
        .api_key_env
        .as_ref()
        .and_then(|k| env.get(k).map(str::to_string));

    let mut content = String::new();
    let mut carry = String::new();
    let mut alive = true;
    let (status, resp, retry_after) = http_client::post_json_stream(
        &scheme,
        &host,
        port,
        &path,
        bearer.as_deref(),
        &body.to_string(),
        timeout,
        b.max_response_bytes as usize,
        b.ca_file.as_deref(),
        &mut |bytes| {
            carry.push_str(&String::from_utf8_lossy(bytes));
            // SSE frames are separated by a blank line; hold any partial tail.
            while let Some(i) = carry.find("\n\n") {
                let frame: String = carry.drain(..i + 2).collect();
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Some(delta) = delta_text(data) {
                        content.push_str(&delta);
                        if !on_delta(&delta) {
                            alive = false;
                        }
                    }
                }
            }
            alive
        },
    )
    .map_err(|e| {
        let m = e.to_string();
        if e.kind() == std::io::ErrorKind::FileTooLarge {
            BackendError::Parse(b.name.clone(), m)
        } else {
            BackendError::Spawn(b.name.clone(), m)
        }
    })?;

    if !(200..300).contains(&status) {
        let detail = error_snippet(&resp);
        if super::is_context_overflow(status, &resp) {
            return Err(BackendError::Overflow(b.name.clone(), detail));
        }
        if status == 429 {
            return Err(BackendError::RateLimited(
                b.name.clone(),
                retry_after.map(Duration::from_secs),
                detail,
            ));
        }
        return Err(BackendError::Http(b.name.clone(), status, detail));
    }
    Ok(DispatchResult {
        content,
        model_label: b.model.clone(),
    })
}

/// Pull `choices[0].delta.content` out of one SSE data payload. `None` for the
/// role-priming frame and anything else without text — those are not errors.
fn delta_text(data: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    v.get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_text_reads_content_and_ignores_the_rest() {
        // §9.2: the role-priming frame and finish-reason frame carry no text and
        // are not errors — treating them as failures would break every stream.
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#).as_deref(),
            Some("Hel")
        );
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
            None
        );
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            None
        );
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{"content":""}}]}"#),
            None
        );
        assert_eq!(delta_text("not json"), None);
        assert_eq!(delta_text("{}"), None);
    }

    #[test]
    fn merges_extra_body_then_pins_model() {
        let caller = serde_json::json!({"model":"caller-wins?","messages":[{"role":"user","content":"hi"}],"think":false});
        let extra = serde_json::json!({"think":true,"foo":1});
        let body = build_body(&caller, &extra, "pinned-model");
        assert_eq!(body["model"], serde_json::json!("pinned-model"));
        assert_eq!(body["think"], serde_json::json!(true));
        assert_eq!(body["foo"], serde_json::json!(1));
        assert!(body["messages"].is_array());
    }

    #[test]
    fn splits_base_url() {
        assert_eq!(
            split_url("http://127.0.0.1:11434/v1"),
            ("http".into(), "127.0.0.1".into(), 11434, "/v1".into())
        );
        assert_eq!(
            split_url("https://openrouter.ai/api/v1"),
            (
                "https".into(),
                "openrouter.ai".into(),
                443,
                "/api/v1".into()
            )
        );
        // U-19: bracketed IPv6 literals.
        assert_eq!(
            split_url("http://[::1]:11434/v1"),
            ("http".into(), "::1".into(), 11434, "/v1".into())
        );
        assert_eq!(
            split_url("http://[fe80::1]/v1"),
            ("http".into(), "fe80::1".into(), 80, "/v1".into())
        );
    }

    #[test]
    fn extracts_content_from_completion() {
        let r = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert_eq!(extract_content("api", r).unwrap(), "hello");
    }

    #[test]
    fn error_snippet_condenses_body() {
        let s = error_snippet("  {\n  \"error\": \"bad key\"\n}\t");
        assert_eq!(s, "{ \"error\": \"bad key\" }");
        assert!(error_snippet(&"x".repeat(500)).chars().count() <= 300);
    }
}
