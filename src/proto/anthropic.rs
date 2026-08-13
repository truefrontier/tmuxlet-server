//! §9.1: the Anthropic Messages surface (`POST /v1/messages`), **text-only**.
//!
//! Our backends are text-in/text-out, so inbound translation is shallow: parse
//! `messages` plus the top-level `system` field, run the same flattening the
//! OpenAI surface already does, and emit an Anthropic-shaped envelope. That is
//! ~250 LOC against Switchyard's 11.5k-line translation crate, precisely
//! because we refuse tool-call round-tripping (§1 non-goal 2).
//!
//! **The ceiling, stated here so nobody has to infer it.** A client that sends
//! `tools` and expects `tool_use` blocks back **will not work** — there is no
//! tool-call translation anywhere in this module, in either direction. This
//! endpoint serves *text-shaped* Anthropic clients (and `api` backends); it
//! does not make tmuxlet-server a drop-in Anthropic provider. `has_tools`
//! exists so the transport can answer a clear `400 invalid_request_error`
//! naming that limitation, rather than silently returning prose where the
//! caller expected a tool call. Silent wrongness is the failure mode worth
//! refusing.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::proto::openai::{ChatMessage, MessageContent};

// ---------- Request ----------

/// Inbound Anthropic Messages request. Text-only.
///
/// Unknown top-level fields (`temperature`, `metadata`, `tool_choice`, …) are
/// swept into `extra` so they parse without error, the same way
/// `openai::ChatRequest` handles arbitrary OpenAI params.
#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Anthropic allows `system` to be a bare string *or* an array of text
    /// blocks. Both are accepted; see `to_openai_messages`.
    #[serde(default)]
    pub system: Option<Content>,
    /// Required by Anthropic, optional here: our backends do not honor an
    /// output-token cap, so enforcing it would be theater. Parsed for
    /// request-shape completeness only.
    #[serde(default)]
    #[allow(dead_code)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    /// Parsed only so `has_tools` can refuse the request; the definitions
    /// themselves are never translated. See the module doc comment.
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    // Captures arbitrary extra Anthropic params so they parse without error,
    // mirroring `openai::ChatRequest::extra`. Not read in production code.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: HashMap<String, Value>,
}

/// One inbound Anthropic message. Anthropic has no per-message `name`, and
/// carries no fields beyond `role`/`content` that a text-only surface can use.
#[derive(Debug, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Content,
}

/// Anthropic content: a bare string, or an array of blocks. Used for both a
/// message's `content` and the top-level `system`, which share this shape.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// `image`, `tool_use`, `tool_result`, `thinking`, … — parsed so the
    /// request does not fail, then dropped with a warning.
    #[serde(other)]
    Other,
}

/// Flatten Anthropic content to text, matching `openai::content_to_text`: join
/// text blocks with `\n` (not `""`, which glues words), and surface dropped
/// non-text blocks rather than losing them silently.
fn content_to_text(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Blocks(bs) => {
            let mut texts = Vec::new();
            let mut dropped = 0usize;
            for b in bs {
                match b {
                    ContentBlock::Text { text } => texts.push(text.as_str()),
                    ContentBlock::Other => dropped += 1,
                }
            }
            if dropped > 0 {
                eprintln!(
                    "[warn] dropped {dropped} non-text content block(s) (image/tool_use/other) from an Anthropic message; only text is forwarded"
                );
            }
            texts.join("\n")
        }
    }
}

impl MessagesRequest {
    /// The model id, for routing — same meaning as OpenAI's `model`.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// True when the client asked for SSE.
    pub fn stream(&self) -> bool {
        self.stream
    }

    /// Translate to the OpenAI message list the rest of the server speaks.
    ///
    /// **This is the reason the design is shallow.** The top-level `system`
    /// becomes a leading `role: "system"` message and each Anthropic message
    /// maps straight through, after which `route::resolve`, the classifier,
    /// `signals::extract`, and `PromptMode` shaping all work on this request
    /// with *no changes at all* — they only ever see `openai::ChatMessage`.
    /// Adding a protocol costs one translation function, not a second routing
    /// stack.
    ///
    /// Content is collapsed to a single text string here rather than preserved
    /// as parts, because every consumer downstream flattens it anyway.
    /// A `system` that is empty or whitespace-only is dropped instead of
    /// emitted, so it cannot add a bare `System:` line to the flattened prompt.
    pub fn to_openai_messages(&self) -> Vec<ChatMessage> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if let Some(sys) = &self.system {
            let text = content_to_text(sys);
            if !text.trim().is_empty() {
                out.push(ChatMessage {
                    role: "system".to_string(),
                    content: MessageContent::Text(text),
                    name: None,
                });
            }
        }
        for m in &self.messages {
            out.push(ChatMessage {
                role: m.role.clone(),
                content: MessageContent::Text(content_to_text(&m.content)),
                name: None,
            });
        }
        out
    }

    /// True when the client sent a non-empty `tools` array — see the module
    /// doc comment for why that is a refusal and not a best-effort.
    ///
    /// An empty array is treated as "no tools": clients and SDK wrappers that
    /// always include `tools: []` are asking for nothing, and refusing them
    /// would break text-shaped usage for no benefit.
    pub fn has_tools(&self) -> bool {
        self.tools.as_ref().is_some_and(|t| !t.is_empty())
    }
}

// ---------- Non-streaming response ----------

/// The Anthropic-shaped success envelope.
///
/// **`usage` is zeroed deliberately.** The backends do not report token counts,
/// and a fabricated `len/4` estimate would feed cost trackers plausible-but-wrong
/// numbers — worse than no numbers, because nothing downstream can tell the
/// difference. Zeros mean "no data". The README already makes the same promise
/// for `/v1/chat/completions`.
pub fn build_message(id: String, model: String, content: String) -> Value {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{ "type": "text", "text": content }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 0, "output_tokens": 0 },
    })
}

/// The Anthropic-shaped error envelope: `{"type":"error","error":{...}}`.
/// Note this differs from OpenAI's, which has no top-level `type`.
pub fn error_envelope(etype: &str, message: &str) -> Value {
    json!({
        "type": "error",
        "error": { "type": etype, "message": message },
    })
}

// ---------- Streaming ----------

/// SSE frames for a completed reply, in Anthropic's event order:
/// `message_start`, `content_block_start`, `content_block_delta`,
/// `content_block_stop`, `message_delta`, `message_stop`.
///
/// Anthropic's SSE frames carry **both** an `event:` line and a `data:` line,
/// unlike OpenAI's data-only frames — clients switch on the `event:` name — and
/// there is no `[DONE]` sentinel; `message_stop` ends the stream. See
/// `openai::stream_frames` for the shape of this helper, not for the protocol.
///
/// The reply is already buffered by the time we get here, so the whole text
/// ships in one `content_block_delta`. All six events are emitted even for an
/// empty reply (as a zero-length delta), so the frame sequence a client sees
/// never varies.
pub fn stream_events(id: &str, model: &str, content: &str) -> Vec<String> {
    let frame = |event: &str, data: Value| {
        format!(
            "event: {event}\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap()
        )
    };
    vec![
        frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                },
            }),
        ),
        frame(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        ),
        frame(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": content },
            }),
        ),
        frame(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": { "output_tokens": 0 },
            }),
        ),
        frame("message_stop", json!({ "type": "message_stop" })),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> MessagesRequest {
        serde_json::from_str(s).unwrap()
    }

    fn text_of(m: &ChatMessage) -> &str {
        match &m.content {
            MessageContent::Text(s) => s.as_str(),
            MessageContent::Parts(_) => panic!("translation must produce flat text"),
        }
    }

    #[test]
    fn parses_string_content() {
        let r = parse(
            r#"{"model":"agy","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(r.model(), "agy");
        assert_eq!(r.max_tokens, Some(1024));
        assert!(!r.stream());
        assert_eq!(r.to_openai_messages().len(), 1);
        assert_eq!(text_of(&r.to_openai_messages()[0]), "hi");
    }

    #[test]
    fn parses_block_array_content_and_drops_non_text_blocks() {
        // Mirrors openai::content_to_text: text joined with `\n`, other block
        // types dropped (with a warning) rather than failing the request.
        let r = parse(
            r#"{"model":"agy","messages":[{"role":"user","content":[
                {"type":"text","text":"a"},
                {"type":"image","source":{"type":"base64","data":"x"}},
                {"type":"text","text":"b"}
            ]}]}"#,
        );
        assert_eq!(text_of(&r.to_openai_messages()[0]), "a\nb");
    }

    #[test]
    fn system_as_string() {
        let r = parse(
            r#"{"model":"agy","system":"be terse","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let msgs = r.to_openai_messages();
        assert_eq!(msgs[0].role, "system");
        assert_eq!(text_of(&msgs[0]), "be terse");
    }

    #[test]
    fn system_as_block_array() {
        // Anthropic allows both forms; the array form is what prompt-caching
        // clients send (each block may carry cache_control, which we ignore).
        let r = parse(
            r#"{"model":"agy","system":[
                {"type":"text","text":"one","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"two"}
            ],"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let msgs = r.to_openai_messages();
        assert_eq!(msgs[0].role, "system");
        assert_eq!(text_of(&msgs[0]), "one\ntwo");
    }

    #[test]
    fn empty_system_is_dropped() {
        let r =
            parse(r#"{"model":"agy","system":"   ","messages":[{"role":"user","content":"hi"}]}"#);
        let msgs = r.to_openai_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn unknown_top_level_fields_parse_cleanly() {
        let r = parse(
            r#"{"model":"agy","messages":[{"role":"user","content":"hi"}],
                "temperature":0.5,"metadata":{"user_id":"u1"},"tool_choice":{"type":"auto"},
                "thinking":{"type":"adaptive"}}"#,
        );
        assert!(r.extra.contains_key("temperature"));
        assert!(r.extra.contains_key("metadata"));
        assert!(r.extra.contains_key("thinking"));
    }

    #[test]
    fn missing_model_or_messages_fails_to_parse() {
        let no_model = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let no_messages = r#"{"model":"agy"}"#;
        assert!(serde_json::from_str::<MessagesRequest>(no_model).is_err());
        assert!(serde_json::from_str::<MessagesRequest>(no_messages).is_err());
    }

    #[test]
    fn to_openai_messages_puts_system_first_and_preserves_order() {
        // This ordering is what lets route::resolve, the classifier, and
        // signals::extract run unchanged on an Anthropic request.
        let r = parse(
            r#"{"model":"agy","system":"S","messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":"mid"},
                {"role":"user","content":"last"}
            ]}"#,
        );
        let msgs = r.to_openai_messages();
        let shape: Vec<(&str, &str)> = msgs.iter().map(|m| (m.role.as_str(), text_of(m))).collect();
        assert_eq!(
            shape,
            vec![
                ("system", "S"),
                ("user", "first"),
                ("assistant", "mid"),
                ("user", "last"),
            ]
        );
    }

    #[test]
    fn has_tools_flags_only_a_non_empty_tools_array() {
        let none = parse(r#"{"model":"agy","messages":[{"role":"user","content":"hi"}]}"#);
        let empty =
            parse(r#"{"model":"agy","messages":[{"role":"user","content":"hi"}],"tools":[]}"#);
        let some = parse(
            r#"{"model":"agy","messages":[{"role":"user","content":"hi"}],
                "tools":[{"name":"get_weather","input_schema":{"type":"object"}}]}"#,
        );
        assert!(!none.has_tools());
        assert!(!empty.has_tools());
        assert!(some.has_tools());
    }

    #[test]
    fn stream_flag_is_read() {
        let r =
            parse(r#"{"model":"agy","stream":true,"messages":[{"role":"user","content":"hi"}]}"#);
        assert!(r.stream());
    }

    #[test]
    fn message_envelope_has_exact_key_set_and_zeroed_usage() {
        let v = build_message("msg_1".into(), "agy".into(), "hello".into());
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "content",
                "id",
                "model",
                "role",
                "stop_reason",
                "stop_sequence",
                "type",
                "usage"
            ]
        );
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["stop_reason"], "end_turn");
        assert!(v["stop_sequence"].is_null());
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
        // Zeroed deliberately: the backends report no token counts, and a
        // fabricated estimate would feed cost trackers wrong numbers.
        assert_eq!(v["usage"]["input_tokens"], 0);
        assert_eq!(v["usage"]["output_tokens"], 0);
    }

    #[test]
    fn empty_content_still_produces_a_well_formed_envelope() {
        let v = build_message("msg_1".into(), "agy".into(), String::new());
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["text"], "");
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[test]
    fn multibyte_text_survives_translation_and_both_envelopes() {
        let r = parse(
            r#"{"model":"agy","system":"日本語","messages":[{"role":"user","content":"héllo → 🌍"}]}"#,
        );
        let msgs = r.to_openai_messages();
        assert_eq!(text_of(&msgs[0]), "日本語");
        assert_eq!(text_of(&msgs[1]), "héllo → 🌍");

        let v = build_message("msg_1".into(), "agy".into(), "héllo → 🌍".into());
        assert_eq!(v["content"][0]["text"], "héllo → 🌍");

        let frames = stream_events("msg_1", "agy", "héllo → 🌍");
        let payload: Value = serde_json::from_str(
            frames[2]
                .lines()
                .nth(1)
                .unwrap()
                .trim_start_matches("data: "),
        )
        .unwrap();
        assert_eq!(payload["delta"]["text"], "héllo → 🌍");
    }

    #[test]
    fn error_envelope_has_anthropic_shape() {
        let v = error_envelope("invalid_request_error", "tools are not supported");
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "tools are not supported");
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["error", "type"]);
    }

    #[test]
    fn stream_events_emit_all_six_types_in_order() {
        let frames = stream_events("msg_1", "agy", "Hello");
        let names: Vec<&str> = frames
            .iter()
            .map(|f| f.lines().next().unwrap().trim_start_matches("event: "))
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert!(
            frames[2]
                .lines()
                .nth(1)
                .unwrap()
                .contains(r#""text":"Hello""#)
        );
        assert_eq!(
            serde_json::from_str::<Value>(
                frames[4]
                    .lines()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches("data: ")
            )
            .unwrap()["delta"]["stop_reason"],
            "end_turn"
        );
    }

    #[test]
    fn every_frame_carries_event_and_data_lines_with_parseable_json() {
        // Anthropic's SSE format, unlike OpenAI's, names the event on its own
        // line; clients dispatch on it. Each frame is two lines + a blank line.
        for frame in stream_events("msg_1", "agy", "Hello") {
            assert!(
                frame.ends_with("\n\n"),
                "frame not blank-line terminated: {frame:?}"
            );
            let mut lines = frame.trim_end().lines();
            let event = lines.next().unwrap();
            let data = lines.next().unwrap();
            assert!(lines.next().is_none(), "unexpected extra line in {frame:?}");
            assert!(
                event.starts_with("event: "),
                "missing event line: {frame:?}"
            );
            assert!(data.starts_with("data: "), "missing data line: {frame:?}");
            let payload: Value =
                serde_json::from_str(data.trim_start_matches("data: ")).expect("data is JSON");
            // The event name and the payload's own `type` must agree.
            assert_eq!(payload["type"], event.trim_start_matches("event: "));
        }
    }
}
