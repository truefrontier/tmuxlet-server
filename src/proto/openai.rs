use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------- Request ----------
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    // Captures arbitrary extra OpenAI params so they parse without error. The
    // api backend forwards the verbatim request JSON (see http.rs), so this
    // typed copy is intentionally not read in production code.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// Defaulted, because an assistant message that carries only `tool_calls`
    /// omits it entirely. See `MessageContent::Null`.
    #[serde(default)]
    pub content: MessageContent,
    // OpenAI per-message `name`; parsed for request-shape completeness but not
    // used when flattening messages to a single prompt.
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
    /// `"content": null`, or absent.
    ///
    /// This is what a real OpenAI client sends on an assistant message that
    /// carries only `tool_calls`. Without an arm for it, untagged deserialization
    /// matches neither `Text` nor `Parts` and the whole request 400s **before
    /// routing runs** — which would reject exactly the tool-history traffic §7's
    /// signal router exists to read.
    #[default]
    Null,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

fn content_to_text(c: &MessageContent) -> String {
    match c {
        MessageContent::Null => String::new(),
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(ps) => {
            // U-15: join text parts with `\n` (not `""`, which glues words), and
            // surface dropped non-text parts (images) rather than losing them
            // silently.
            let mut texts = Vec::new();
            let mut dropped = 0usize;
            for p in ps {
                match p {
                    ContentPart::Text { text } => texts.push(text.as_str()),
                    ContentPart::Other => dropped += 1,
                }
            }
            if dropped > 0 {
                eprintln!(
                    "[warn] dropped {dropped} non-text content part(s) (image/other) from a message; only text is forwarded"
                );
            }
            texts.join("\n")
        }
    }
}

pub fn flatten_to_prompt(messages: &[ChatMessage]) -> String {
    let mut blocks = Vec::with_capacity(messages.len());
    for m in messages {
        let label = match m.role.as_str() {
            "system" => "System".to_string(),
            "user" => "User".to_string(),
            "assistant" => "Assistant".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        };
        blocks.push(format!("{}: {}", label, content_to_text(&m.content)));
    }
    blocks.join("\n\n")
}

/// §5: the text of the last user message, for auto-router classification.
pub fn last_user_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| content_to_text(&m.content))
        .unwrap_or_default()
}

/// Re-serialize a message list as the OpenAI wire shape, flattening multi-part
/// content down to text.
///
/// §9.1 uses this so that a request arriving on the Anthropic surface reaches an
/// `api` backend as an **OpenAI** body — those upstreams speak OpenAI, and
/// forwarding Anthropic JSON to one would fail in a confusing way. It also means
/// routing, signal extraction, and prompt shaping see one shape regardless of
/// which endpoint the client used.
pub fn messages_to_json(messages: &[ChatMessage]) -> serde_json::Value {
    serde_json::Value::Array(
        messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": content_to_text(&m.content),
                })
            })
            .collect(),
    )
}

/// §4.2: the text of the *first* user message — the opening task. Paired with
/// `last_user_text` to build the classifier input, because a conversation's
/// intent lives at its start and the current ask lives at its end.
pub fn first_user_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .find(|m| m.role == "user")
        .map(|m| content_to_text(&m.content))
        .unwrap_or_default()
}

/// §4.2: how many `role == "user"` messages the conversation carries. One (or
/// zero) means there is no separate opening task to show the classifier.
pub fn user_message_count(messages: &[ChatMessage]) -> usize {
    messages.iter().filter(|m| m.role == "user").count()
}

/// U-14 `last_user` shaping: the final user message verbatim, with any system
/// messages prepended plainly (no role labels) so coding CLIs see the prompt
/// they'd get from a direct invocation.
pub fn last_user_prompt(messages: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| content_to_text(&m.content))
        .filter(|s| !s.trim().is_empty())
        .collect();
    let user = last_user_text(messages);
    if !user.is_empty() {
        parts.push(user);
    }
    parts.join("\n\n")
}

/// True when the OpenAI body carries a non-empty `tools` array.
pub fn request_has_tools(raw: &serde_json::Value) -> bool {
    raw.get("tools")
        .and_then(|t| t.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// Schemas to paste into a tmuxlet tools-protocol prompt.
pub fn tools_appendix_from_raw(raw: &serde_json::Value) -> Option<String> {
    let tools = raw.get("tools")?;
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let pretty = serde_json::to_string_pretty(tools).ok()?;
    Some(format!(
        "\n\nAvailable tools (JSON schemas). You are a completion model: do not use your own Bash/Edit/Read. Emit tool_calls or text through the tmuxlet turn protocol.\n{pretty}"
    ))
}

fn json_content_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        return arr
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str()).map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// Text to `tmuxlet send` on a held pane: trailing tool results, else last user.
pub fn continuation_text_from_raw(raw: &serde_json::Value) -> String {
    let Some(msgs) = raw.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    let mut tools = Vec::new();
    for m in msgs.iter().rev() {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" || role == "function" {
            tools.push(format!(
                "Tool result:\n{}",
                json_content_text(m.get("content").unwrap_or(&serde_json::Value::Null))
            ));
        } else {
            break;
        }
    }
    if !tools.is_empty() {
        tools.reverse();
        return tools.join("\n\n");
    }
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) == Some("user") {
            return json_content_text(m.get("content").unwrap_or(&serde_json::Value::Null));
        }
    }
    String::new()
}

// ---------- Non-streaming response ----------
#[derive(Debug, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: ResponseMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize)]
pub struct ResponseMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Debug, Serialize)]
pub struct ResponseToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: &'static str,
    pub function: ResponseFunction,
}

#[derive(Debug, Serialize)]
pub struct ResponseFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn build_completion(id: String, model: String, content: String) -> ChatCompletion {
    build_completion_with_tools(id, model, Some(content), None)
}

fn response_tool_calls(calls: &[(String, String, String)]) -> Vec<ResponseToolCall> {
    calls
        .iter()
        .map(|(id, name, arguments)| ResponseToolCall {
            id: id.clone(),
            r#type: "function",
            function: ResponseFunction {
                name: name.clone(),
                arguments: arguments.clone(),
            },
        })
        .collect()
}

/// OpenAI completion for a text reply and/or `tool_calls`.
pub fn build_completion_with_tools(
    id: String,
    model: String,
    content: Option<String>,
    tool_calls: Option<&[(String, String, String)]>,
) -> ChatCompletion {
    let has_tools = tool_calls.is_some_and(|c| !c.is_empty());
    ChatCompletion {
        id,
        object: "chat.completion",
        created: unix_now(),
        model,
        choices: vec![Choice {
            index: 0,
            message: ResponseMessage {
                role: "assistant",
                content: if has_tools && content.as_ref().is_none_or(|s| s.is_empty()) {
                    None
                } else {
                    content
                },
                tool_calls: if has_tools {
                    Some(response_tool_calls(tool_calls.unwrap_or(&[])))
                } else {
                    None
                },
            },
            finish_reason: if has_tools {
                "tool_calls".into()
            } else {
                "stop".into()
            },
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
    }
}

// ---------- Streaming ----------
#[derive(Debug, Serialize)]
pub struct ChatChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// V1 buffered streaming: role-prime, one content frame, final, then [DONE].
/// One SSE frame from a chunk choice.
fn chunk_frame(id: &str, model: &str, created: u64, c: ChunkChoice) -> String {
    let chunk = ChatChunk {
        id: id.into(),
        object: "chat.completion.chunk",
        created,
        model: model.into(),
        choices: vec![c],
    };
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

/// §9.2: the role-priming frame, emitted once at the commit point of a real
/// stream so the client sees a well-formed OpenAI stream rather than bare
/// content deltas.
pub fn stream_role_frame(id: &str, model: &str) -> String {
    chunk_frame(
        id,
        model,
        unix_now(),
        ChunkChoice {
            index: 0,
            delta: Delta {
                role: Some("assistant"),
                content: Some(String::new()),
            },
            finish_reason: None,
        },
    )
}

/// §9.2: one relayed token (or run of tokens).
pub fn stream_delta_frame(id: &str, model: &str, delta: &str) -> String {
    chunk_frame(
        id,
        model,
        unix_now(),
        ChunkChoice {
            index: 0,
            delta: Delta {
                role: None,
                content: Some(delta.to_string()),
            },
            finish_reason: None,
        },
    )
}

/// §9.2: the finish frame plus `[DONE]`, for a stream whose deltas already went
/// out live.
pub fn stream_tail_frames(id: &str, model: &str) -> Vec<String> {
    vec![
        chunk_frame(
            id,
            model,
            unix_now(),
            ChunkChoice {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                },
                finish_reason: Some("stop".into()),
            },
        ),
        "data: [DONE]\n\n".to_string(),
    ]
}

pub fn stream_frames(id: &str, model: &str, content: &str) -> Vec<String> {
    let created = unix_now();
    let frame = |c: ChunkChoice| {
        let chunk = ChatChunk {
            id: id.into(),
            object: "chat.completion.chunk",
            created,
            model: model.into(),
            choices: vec![c],
        };
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    };
    vec![
        frame(ChunkChoice {
            index: 0,
            delta: Delta {
                role: Some("assistant"),
                content: Some(String::new()),
            },
            finish_reason: None,
        }),
        frame(ChunkChoice {
            index: 0,
            delta: Delta {
                role: None,
                content: Some(content.to_string()),
            },
            finish_reason: None,
        }),
        frame(ChunkChoice {
            index: 0,
            delta: Delta {
                role: None,
                content: None,
            },
            finish_reason: Some("stop".into()),
        }),
        "data: [DONE]\n\n".to_string(),
    ]
}

/// Buffered SSE for a `tool_calls` turn. Text streaming stays on `stream_frames`.
pub fn stream_frames_with_tools(
    id: &str,
    model: &str,
    calls: &[(String, String, String)],
) -> Vec<String> {
    let created = unix_now();
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (call_id, name, arguments))| {
            serde_json::json!({
                "index": i,
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            })
        })
        .collect();
    let frame = |delta: serde_json::Value, finish: Option<&str>| {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": delta,
                    "finish_reason": finish
                }]
            })
        )
    };
    vec![
        frame(serde_json::json!({"role": "assistant"}), None),
        frame(serde_json::json!({"tool_calls": tool_calls}), None),
        frame(serde_json::json!({}), Some("tool_calls")),
        "data: [DONE]\n\n".to_string(),
    ]
}

// ---------- /v1/models ----------
#[derive(Debug, Serialize)]
pub struct ModelList {
    pub object: &'static str,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

pub fn model_list(ids: impl Iterator<Item = String>) -> ModelList {
    let created = unix_now();
    ModelList {
        object: "list",
        data: ids
            .map(|id| ModelInfo {
                id,
                object: "model",
                created,
                owned_by: "tmuxlet-server".into(),
            })
            .collect(),
    }
}

// ---------- Errors ----------
#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub error: ApiError,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub param: Option<String>,
    pub code: Option<String>,
    /// U-23: full per-leg error strings for a 503; omitted when empty. The
    /// OpenAI envelope tolerates extra fields, and clients that render only
    /// `message` still get the compact summary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

impl ErrorEnvelope {
    pub fn new(message: impl Into<String>, error_type: &str, code: Option<&str>) -> Self {
        ErrorEnvelope {
            error: ApiError {
                message: message.into(),
                error_type: error_type.into(),
                param: None,
                code: code.map(|s| s.to_string()),
                details: Vec::new(),
            },
        }
    }
    /// U-23: like `new`, but attaches the full per-leg error strings.
    pub fn with_details(
        message: impl Into<String>,
        error_type: &str,
        code: Option<&str>,
        details: Vec<String>,
    ) -> Self {
        let mut e = Self::new(message, error_type, code);
        e.error.details = details;
        e
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_calls_assistant_message_parses() {
        // A real OpenAI agent client sends `"content": null` (or omits it) on an
        // assistant message that carries only tool_calls. This used to 400 before
        // routing ever ran, which meant the §7 signal router could never see the
        // one kind of traffic it exists to read.
        let req: ChatRequest = serde_json::from_str(
            r#"{"model":"m","messages":[
                {"role":"user","content":"go"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"1","type":"function","function":{"name":"Read","arguments":"{}"}}]},
                {"role":"assistant","tool_calls":[{"id":"2","type":"function","function":{"name":"Write","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"1","content":"ok"}
            ]}"#,
        )
        .expect("a real agent transcript must parse");
        assert_eq!(req.messages.len(), 4);
        // A content-less assistant turn flattens to an empty string, not to the
        // literal "null".
        assert_eq!(content_to_text(&req.messages[1].content), "");
        assert_eq!(content_to_text(&req.messages[2].content), "");
        assert_eq!(last_user_text(&req.messages), "go");
    }

    #[test]
    fn parses_string_and_array_content() {
        let s =
            r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"stream":true,"seed":42}"#;
        let p =
            r#"{"model":"x","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let a: ChatRequest = serde_json::from_str(s).unwrap();
        let b: ChatRequest = serde_json::from_str(p).unwrap();
        assert!(a.stream);
        assert!(!b.stream);
        assert!(a.extra.contains_key("seed"));
    }

    #[test]
    fn multipart_text_joined_with_newlines() {
        let c: MessageContent = serde_json::from_str(
            r#"[{"type":"text","text":"a"},{"type":"image_url","image_url":{"url":"x"}},{"type":"text","text":"b"}]"#,
        )
        .unwrap();
        assert_eq!(content_to_text(&c), "a\nb");
    }

    #[test]
    fn error_envelope_with_details_serializes_array() {
        let j = ErrorEnvelope::with_details(
            "summary",
            "server_error",
            Some("all_backends_failed"),
            vec!["[a] x".into(), "[b] y".into()],
        )
        .to_json();
        assert!(j.contains("\"details\":["));
        assert!(j.contains("summary"));
        // empty details omit the field
        let j2 = ErrorEnvelope::new("m", "t", None).to_json();
        assert!(!j2.contains("details"));
    }

    #[test]
    fn flattens_messages_with_role_labels() {
        let req: ChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[{"role":"system","content":"S"},{"role":"user","content":"U"}]}"#,
        )
        .unwrap();
        // U-14: labels unified — `System:` (no brackets).
        assert_eq!(flatten_to_prompt(&req.messages), "System: S\n\nUser: U");
    }

    #[test]
    fn last_user_prompt_prepends_system_and_drops_labels() {
        // U-14 `last_user`: system text prepended plainly, final user verbatim.
        let req: ChatRequest = serde_json::from_str(
            r#"{"model":"x","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"},{"role":"assistant","content":"prev"},{"role":"user","content":"final ask"}]}"#,
        )
        .unwrap();
        assert_eq!(last_user_prompt(&req.messages), "be terse\n\nfinal ask");
    }

    #[test]
    fn final_stream_delta_serializes_empty() {
        let frames = stream_frames("id1", "agy", "Hello");
        assert_eq!(frames.len(), 4);
        assert!(frames[0].contains(r#""role":"assistant""#));
        assert!(frames[1].contains(r#""content":"Hello""#));
        assert!(frames[2].contains(r#""delta":{}"#));
        assert!(frames[2].contains(r#""finish_reason":"stop""#));
        assert_eq!(frames[3], "data: [DONE]\n\n");
    }

    #[test]
    fn completion_has_usage_object() {
        let c = build_completion("id1".into(), "agy".into(), "hi".into());
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains(r#""object":"chat.completion""#));
        assert!(j.contains(r#""usage""#));
    }

    #[test]
    fn completion_with_tools_sets_finish_reason() {
        let calls = vec![(
            "c1".into(),
            "bash".into(),
            r#"{"command":"ls"}"#.into(),
        )];
        let c = build_completion_with_tools("id1".into(), "grok".into(), None, Some(&calls));
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains(r#""finish_reason":"tool_calls""#));
        assert!(j.contains(r#""name":"bash""#));
        assert!(j.contains("command"));
    }

    #[test]
    fn continuation_text_prefers_trailing_tool_results() {
        let raw: serde_json::Value = serde_json::from_str(
            r#"{"messages":[
                {"role":"user","content":"list files"},
                {"role":"assistant","content":null},
                {"role":"tool","content":"a.txt"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(continuation_text_from_raw(&raw), "Tool result:\na.txt");
        assert!(request_has_tools(
            &serde_json::json!({"tools":[{"type":"function"}]})
        ));
        assert!(!request_has_tools(&serde_json::json!({"tools":[]})));
    }
}
