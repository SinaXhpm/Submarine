//! OpenAI's Responses API.
//!
//! OpenAI's own docs: "Starting with GPT-5.4, Chat Completions does not support
//! tool calling with `reasoning_effort` values other than `none`", and GPT-6
//! Astra and GPT-6.1 Sol need the Responses API for tool calling at all. So
//! api.openai.com gets this adapter, and every other OpenAI-compatible server
//! the Chat Completions one.
//!
//! Requests go with `store: false`: OpenAI keeps no conversation. They ask
//! for `reasoning.encrypted_content`, so reasoning items come back with it, and
//! every output item is sent back as it came, in order: the model keeps its
//! reasoning across tool calls. A reasoning item that came without it can't
//! go back, and is left out.

use super::{n, plain_header, s, secret_header, Endpoint, Ev, HttpRequest, ModelInfo, ProviderError, Reply, Request, Stop, Usage, MAX_REPLY_TEXT};
use crate::ai::canon::{Block, Role, Turn};
use crate::ai::sse;
use serde_json::{json, Map, Value};

pub const DROPPABLE: &[(&str, &str)] = &[
    ("reasoning.summary", "reasoning.summary"),
    ("summary", "reasoning.summary"),
    // An organization that isn't verified: "…to generate reasoning summaries".
    ("reasoning summaries", "reasoning.summary"),
    // A model that doesn't reason refuses the encrypted reasoning asked for.
    // It is left out for that model only (`include_key`).
    ("encrypted content is not supported", "include"),
    ("reasoning", "reasoning"),
    ("max_output_tokens", "max_output_tokens"),
];

/// What a refused `include` is remembered by. One model refuses it, not the
/// provider: its other models still need it.
pub fn include_key(model: &str) -> String {
    format!("include:{model}")
}

/// Whether a history item goes back. Without a stored conversation a
/// reasoning item is read from its encrypted content alone: one without it
/// would be looked for and not found, and a model that refused encrypted
/// content can't read one. Its summary stays in the chat's neutral copy.
fn goes_back(item: &Value, include: bool) -> bool {
    s(item, "type") != "reasoning" || (include && !s(item, "encrypted_content").is_empty())
}

pub fn build(ep: &Endpoint, req: &Request) -> Result<HttpRequest, String> {
    let dropped = |f: &str| req.dropped.contains(f);
    let include = !req.dropped.contains(&include_key(&req.settings.model));
    let mut body = Map::new();
    body.insert("model".into(), json!(req.settings.model));
    body.insert("instructions".into(), json!(req.system));
    body.insert("input".into(), Value::Array(req.history.iter().filter(|h| goes_back(h, include)).cloned().collect()));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    // Without a stored conversation, a reasoning item can only go back with
    // its encrypted content. A model reasons at its default effort even when
    // the request doesn't ask, so this goes unless the model refused it.
    if include {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    if let Some(tools) = req.tools {
        body.insert("tools".into(), tools_for(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    let effort = req.settings.effort.as_deref().filter(|e| !e.is_empty());
    if !dropped("reasoning") && (effort.is_some() || req.settings.show_reasoning) {
        let mut r = Map::new();
        if let Some(e) = effort {
            r.insert("effort".into(), json!(e));
        }
        if req.settings.show_reasoning && !dropped("reasoning.summary") {
            r.insert("summary".into(), json!("auto"));
        }
        if !r.is_empty() {
            body.insert("reasoning".into(), Value::Object(r));
        }
    }
    if let Some(max) = req.settings.max_tokens {
        if !dropped("max_output_tokens") {
            body.insert("max_output_tokens".into(), json!(max));
        }
    }
    let key = ep.key.as_ref().ok_or("[AI] NO_KEY: this provider has no API key")?;
    Ok(HttpRequest {
        url: format!("{}/responses", ep.base),
        headers: vec![("authorization", secret_header(&format!("Bearer {}", key.as_str()))?)],
        body: Value::Object(body),
    })
}

/// The neutral tool list (`name`, `description`, `input_schema`) as Responses
/// API function tools.
pub fn tools_for(tools: &Value) -> Value {
    let list = tools.as_array().cloned().unwrap_or_default();
    Value::Array(
        list.iter()
            .map(|t| json!({"type": "function", "name": t["name"], "description": t["description"], "parameters": t["input_schema"]}))
            .collect(),
    )
}

#[derive(Default)]
pub struct Parser {
    model: Option<String>,
    /// The bytes of streamed text and reasoning so far.
    held: usize,
    /// The final response object, from `response.completed` (or `.incomplete`).
    response: Option<Value>,
}

impl Parser {
    /// Counts what the reply streams of its text and reasoning; past
    /// `MAX_REPLY_TEXT` the reply is refused, so the text kept from a failed
    /// or stopped reply stays under it too. The final response is one event,
    /// and smaller.
    fn hold(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.held += bytes;
        if self.held > MAX_REPLY_TEXT {
            return Err(ProviderError::bad_reply("the reply's text and reasoning passed 8 MiB"));
        }
        Ok(())
    }

    pub fn feed(&mut self, ev: &sse::Event, out: &mut Vec<Ev>) -> Result<(), ProviderError> {
        let v: Value = serde_json::from_str(&ev.data).map_err(|_| ProviderError::bad_reply("an event that isn't JSON"))?;
        let kind = v.get("type").and_then(Value::as_str).or(ev.event.as_deref()).unwrap_or("");
        match kind {
            "response.created" | "response.in_progress" => {
                if let Some(m) = v.pointer("/response/model").and_then(Value::as_str) {
                    if self.model.is_none() {
                        self.model = Some(m.to_string());
                        out.push(Ev::Model(m.to_string()));
                    }
                }
            }
            "response.output_text.delta" => {
                let t = s(&v, "delta");
                self.hold(t.len())?;
                out.push(Ev::Text(t.to_string()));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let t = s(&v, "delta");
                self.hold(t.len())?;
                out.push(Ev::Thinking(t.to_string()));
            }
            "response.output_item.added" => {
                let item = &v["item"];
                if s(item, "type") == "function_call" {
                    out.push(Ev::ToolStart { id: s(item, "call_id").to_string(), name: s(item, "name").to_string() });
                }
            }
            "response.completed" | "response.incomplete" => {
                self.response = Some(v.get("response").cloned().unwrap_or(Value::Null));
            }
            "response.failed" => {
                let e = v.pointer("/response/error").cloned().unwrap_or(Value::Null);
                return Err(ProviderError { status: None, kind: s(&e, "code").to_string(), message: s(&e, "message").to_string(), request_id: None, retry_after: None });
            }
            "error" => {
                let e = v.get("error").unwrap_or(&v);
                let code = e.get("code").and_then(Value::as_str).or_else(|| e.get("type").and_then(Value::as_str)).unwrap_or("error");
                return Err(ProviderError { status: None, kind: code.to_string(), message: s(e, "message").to_string(), request_id: None, retry_after: None });
            }
            _ => {} // deltas for function arguments, content parts, and anything newer
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Reply, ProviderError> {
        let response = self.response.ok_or_else(|| ProviderError::ours("connection", "the reply stopped before it was finished"))?;
        let items = response.get("output").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut blocks = Vec::new();
        let mut refused = false;
        let mut calls = false;
        for item in &items {
            match s(item, "type") {
                "reasoning" => {
                    let text: Vec<&str> = item.get("summary").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
                    if !text.is_empty() {
                        blocks.push(Block::Reasoning { text: text.join("\n\n") });
                    }
                }
                "message" => {
                    for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                        match s(part, "type") {
                            "output_text" => blocks.push(Block::Text { text: s(part, "text").to_string() }),
                            "refusal" => refused = true,
                            _ => {}
                        }
                    }
                }
                "function_call" => {
                    calls = true;
                    let raw = s(item, "arguments");
                    let input = match serde_json::from_str::<Value>(raw) {
                        Ok(v) if v.is_object() => v,
                        _ => json!({"INVALID_JSON": raw}),
                    };
                    blocks.push(Block::ToolCall { id: s(item, "call_id").to_string(), name: s(item, "name").to_string(), input });
                }
                _ => {}
            }
        }
        let reason = response.pointer("/incomplete_details/reason").and_then(Value::as_str).unwrap_or("");
        let stop = if refused || reason == "content_filter" {
            Stop::Refused
        } else if s(&response, "status") == "incomplete" {
            Stop::CutOff
        } else if calls {
            Stop::ToolUse
        } else {
            Stop::End
        };
        let u = &response["usage"];
        let usage = Usage {
            input: n(u, "input_tokens"),
            cache_read: u.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0),
            cache_write: 0,
            output: n(u, "output_tokens"),
            cost: None,
        };
        let model = response.get("model").and_then(Value::as_str).map(str::to_string).or(self.model);
        Ok(Reply { wire: items, turn: Turn { role: Role::Assistant, blocks }, stop, usage, model, answered_by: None, refusal: None })
    }
}

/// OpenAI lists every model, chat or not; the picker keeps the ones that chat.
pub fn model_info(m: &Value) -> Option<ModelInfo> {
    let id = m.get("id")?.as_str()?;
    const NOT_CHAT: &[&str] = &["embedding", "whisper", "tts", "dall-e", "moderation", "image", "audio", "realtime", "transcribe", "search", "babbage", "davinci", "sora"];
    if NOT_CHAT.iter().any(|w| id.contains(w)) {
        return None;
    }
    Some(ModelInfo { id: id.to_string(), name: None, context: None, max_output: None, efforts: vec![] })
}

pub fn models_headers(ep: &Endpoint) -> Result<Vec<(&'static str, reqwest::header::HeaderValue)>, String> {
    let key = ep.key.as_ref().ok_or("[AI] NO_KEY: this provider has no API key")?;
    Ok(vec![("authorization", secret_header(&format!("Bearer {}", key.as_str()))?), ("accept", plain_header("application/json"))])
}

#[cfg(test)]
mod tests {
    use super::super::profile::Profile;
    use super::super::{drop_key, parse_error, Kind, Settings};
    use super::*;
    use std::collections::HashSet;

    fn ep() -> Endpoint {
        Endpoint { kind: Kind::OpenAi, profile: Profile::Custom, base: "https://api.openai.com/v1".into(), key: Some(zeroize::Zeroizing::new("sk-x".into())), allow_data_collection: false }
    }

    #[test]
    fn request_is_stateless_and_asks_for_summaries() {
        let st = Settings { model: "gpt-6.1-sol".into(), effort: Some("high".into()), max_tokens: Some(9000), show_reasoning: true, fallback: true };
        let dropped = HashSet::new();
        let tools = json!([{"name": "read_file", "description": "d", "input_schema": {"type": "object"}}]);
        let req = Request { system: "sys", tools: Some(&tools), history: vec![json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]})], settings: &st, caps: None, dropped: &dropped };
        let r = build(&ep(), &req).unwrap();
        assert_eq!(r.url, "https://api.openai.com/v1/responses");
        assert_eq!(r.body["store"], false);
        assert_eq!(r.body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(r.body["instructions"], "sys");
        assert_eq!(r.body["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(r.body["max_output_tokens"], 9000);
        assert_eq!(r.body["tools"][0], json!({"type": "function", "name": "read_file", "description": "d", "parameters": {"type": "object"}}));
        assert!(r.headers[0].1.is_sensitive());
    }

    #[test]
    fn a_refused_include_is_left_out_for_that_model_only() {
        let body = |model: &str, dropped: &HashSet<String>| {
            let st = Settings { model: model.into(), effort: Some("high".into()), max_tokens: None, show_reasoning: true, fallback: true };
            let req = Request { system: "s", tools: None, history: vec![], settings: &st, caps: None, dropped };
            build(&ep(), &req).unwrap().body
        };
        // Remembered as a run does: the field the 400 names, under its key.
        let e = parse_error(400, r#"{"error":{"message":"Encrypted content is not supported with this model.","type":"invalid_request_error","param":"include"}}"#, None, None);
        let field = e.droppable_field(DROPPABLE).unwrap();
        assert_eq!(field, "include");
        let dropped: HashSet<String> = [drop_key(field, "gpt-4.1")].into();
        assert_eq!(drop_key("reasoning".into(), "gpt-4.1"), "reasoning", "the rest are kept for the provider");
        let b = body("gpt-4.1", &dropped);
        assert!(b.get("include").is_none());
        assert_eq!(b["store"], false);
        assert_eq!(b["reasoning"]["effort"], "high");
        // The provider's other models still get it.
        assert_eq!(body("gpt-6.1-sol", &dropped)["include"], json!(["reasoning.encrypted_content"]));

        // A refused effort or summary leaves out the reasoning settings, never `include`.
        for (message, field) in [
            ("Unsupported value: 'reasoning.effort' does not support 'max' with this model.", "reasoning"),
            ("Your organization must be verified to generate reasoning summaries.", "reasoning.summary"),
        ] {
            let e = parse_error(400, &json!({"error": {"message": message, "type": "invalid_request_error"}}).to_string(), None, None);
            assert_eq!(e.droppable_field(DROPPABLE).as_deref(), Some(field), "{message}");
        }
        let dropped: HashSet<String> = ["reasoning".to_string()].into();
        let b = body("gpt-6.1-sol", &dropped);
        assert!(b.get("reasoning").is_none());
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn reasoning_items_go_back_only_with_their_encrypted_content() {
        let st = Settings { model: "gpt-6.1-sol".into(), effort: None, max_tokens: None, show_reasoning: false, fallback: true };
        let history = vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"}),
            json!({"type": "reasoning", "id": "rs_2", "summary": [{"type": "summary_text", "text": "Checking."}]}),
            json!({"type": "reasoning", "id": "rs_3", "summary": [], "encrypted_content": ""}),
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read_file", "arguments": "{}"}),
        ];
        let ids = |dropped: &HashSet<String>| -> Vec<String> {
            let req = Request { system: "s", tools: None, history: history.clone(), settings: &st, caps: None, dropped };
            build(&ep(), &req).unwrap().body["input"].as_array().unwrap().iter().map(|i| s(i, "id").to_string()).collect()
        };
        // Sent back without their encrypted content, rs_2 and rs_3 would be
        // looked for and not found.
        assert_eq!(ids(&HashSet::new()), ["", "rs_1", "fc_1"]);
        // A model that refused encrypted content gets no reasoning items.
        assert_eq!(ids(&[include_key("gpt-6.1-sol")].into()), ["", "fc_1"]);
    }

    #[test]
    fn a_reply_with_reasoning_text_and_a_call_is_kept_whole() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for e in [
            r#"{"type":"response.created","response":{"model":"gpt-6.1-sol"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"Checking."}"#,
            r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.completed","response":{"model":"gpt-6.1-sol","status":"completed","output":[
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Checking."}],"encrypted_content":"enc"},
                {"type":"message","id":"msg_1","role":"assistant","content":[{"type":"output_text","text":"Hi"}]},
                {"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"/etc\"}"}],
                "usage":{"input_tokens":30,"input_tokens_details":{"cached_tokens":10},"output_tokens":5}}}"#,
        ] {
            p.feed(&sse::Event { event: None, data: e.into() }, &mut out).unwrap();
        }
        let r = p.finish().unwrap();
        assert_eq!(r.stop, Stop::ToolUse);
        assert_eq!(r.wire.len(), 3, "every output item goes back");
        assert_eq!(r.wire[0]["encrypted_content"], "enc");
        assert_eq!(r.usage.cache_read, 10);
        assert_eq!(r.turn.blocks[2], Block::ToolCall { id: "call_1".into(), name: "read_file".into(), input: json!({"path": "/etc"}) });
        assert!(out.contains(&Ev::Thinking("Checking.".into())));
    }

    #[test]
    fn incomplete_refused_failed_and_cut_streams() {
        let fin = |resp: &str| {
            let mut p = Parser::default();
            p.feed(&sse::Event { event: None, data: format!(r#"{{"type":"response.incomplete","response":{resp}}}"#) }, &mut Vec::new()).unwrap();
            p.finish().unwrap().stop
        };
        assert_eq!(fin(r#"{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}"#), Stop::CutOff);
        assert_eq!(fin(r#"{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}"#), Stop::Refused);
        assert_eq!(fin(r#"{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"no"}]}]}"#), Stop::Refused);
        let mut p = Parser::default();
        assert!(p.feed(&sse::Event { event: None, data: r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"x"}}}"#.into() }, &mut Vec::new()).is_err());
        assert!(Parser::default().finish().is_err());
    }

    #[test]
    fn a_reply_past_the_text_cap_is_refused() {
        let delta = |kind: &str, t: &str| sse::Event { event: None, data: json!({"type": kind, "delta": t}).to_string() };
        let half = "x".repeat(MAX_REPLY_TEXT / 2);
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(&delta("response.reasoning_summary_text.delta", &half), &mut out).unwrap();
        p.feed(&delta("response.output_text.delta", &half), &mut out).unwrap();
        let e = p.feed(&delta("response.reasoning_text.delta", "y"), &mut out).unwrap_err();
        assert!(e.message.contains("8 MiB"), "{}", e.message);
        assert_eq!(out.len(), 2, "the piece past the cap isn't passed on");
    }

    #[test]
    fn the_picker_skips_models_that_dont_chat() {
        assert!(model_info(&json!({"id": "text-embedding-3-large"})).is_none());
        assert!(model_info(&json!({"id": "gpt-6.1-sol"})).is_some());
    }
}
