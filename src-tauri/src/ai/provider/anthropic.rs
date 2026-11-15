//! Anthropic's Messages API.
//!
//! The reply is stored exactly as it streams in, block by block: thinking
//! blocks with their signatures (even with empty text), text, tool calls with
//! their parsed input. On Opus 5.5, Sonnet 5.5, Haiku 5.5 and Fable 5.1 a
//! replayed thinking block is checked against everything before it, so the
//! stored copy is what goes back, unchanged, on every later request.

use super::{n, plain_header, s, secret_header, Endpoint, Ev, HttpRequest, ModelCaps, ModelInfo, ProviderError, Reply, Request, Stop, Usage, MAX_REPLY_TEXT};
use crate::ai::canon::{Block, Role, Turn};
use crate::ai::sse;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const VERSION: &str = "2026-10-21";
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const OFFICIAL: &str = "https://api.anthropic.com";
/// The models with server-side refusal fallback. Haiku 5.5 has none, and the
/// field means nothing to older models.
const FALLBACK_MODELS: &[&str] = &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// A 400 naming one of these is answered by sending the request again without
/// the field. Matched on the lower-cased error message, in this order.
pub const DROPPABLE: &[(&str, &str)] = &[
    ("fallbacks", "fallbacks"),
    ("anthropic-beta", "fallbacks"),
    ("server-side-fallback", "fallbacks"),
    ("eager_input_streaming", "eager_input_streaming"),
    ("display", "thinking.display"),
    ("output_config", "output_config"),
    ("effort", "output_config"),
    ("thinking", "thinking"),
    ("cache_control", "cache_control"),
];

pub fn official(base: &str) -> bool {
    base.trim_end_matches('/') == OFFICIAL
}

fn fallback_wanted(ep: &Endpoint, req: &Request) -> bool {
    req.settings.fallback
        && official(&ep.base)
        && FALLBACK_MODELS.contains(&req.settings.model.as_str())
        && !req.dropped.contains("fallbacks")
}

pub fn build(ep: &Endpoint, req: &Request) -> Result<HttpRequest, String> {
    let caps = req.caps.cloned().unwrap_or_default();
    let dropped = |f: &str| req.dropped.contains(f);
    let mut body = Map::new();
    body.insert("model".into(), json!(req.settings.model));
    let mut max = req.settings.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    if let Some(cap) = caps.max_tokens {
        max = max.min(cap);
    }
    body.insert("max_tokens".into(), json!(max));
    body.insert("stream".into(), json!(true));
    if !dropped("cache_control") {
        body.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
    body.insert("system".into(), json!([{"type": "text", "text": req.system}]));
    if let Some(tools) = req.tools {
        body.insert("tools".into(), tools_for(tools, !dropped("eager_input_streaming")));
        body.insert("tool_choice".into(), json!({"type": "auto"}));
    }
    if caps.adaptive != Some(false) && !dropped("thinking") {
        let mut thinking = Map::new();
        thinking.insert("type".into(), json!("adaptive"));
        if !dropped("thinking.display") {
            thinking.insert("display".into(), json!(if req.settings.show_reasoning { "summarized" } else { "omitted" }));
        }
        body.insert("thinking".into(), Value::Object(thinking));
    }
    if let Some(effort) = req.settings.effort.as_deref().filter(|e| !e.is_empty()) {
        let allowed = caps.efforts.as_ref().map_or(true, |list| list.iter().any(|l| l == effort));
        if allowed && !dropped("output_config") {
            body.insert("output_config".into(), json!({"effort": effort}));
        }
    }
    let fallback = fallback_wanted(ep, req);
    if fallback {
        body.insert("fallbacks".into(), json!("default"));
    }
    body.insert("messages".into(), Value::Array(req.history.clone()));

    let key = ep.key.as_ref().ok_or("[AI] NO_KEY: this provider has no API key")?;
    let mut headers = vec![
        ("x-api-key", secret_header(key)?),
        ("anthropic-version", plain_header(VERSION)),
    ];
    if fallback {
        headers.push(("anthropic-beta", plain_header(FALLBACK_BETA)));
    }
    Ok(HttpRequest { url: format!("{}/v1/messages", ep.base), headers, body: Value::Object(body) })
}

/// The neutral tool list (`name`, `description`, `input_schema`) is already
/// Anthropic's shape. Eager input streaming makes a long `write_file` stream as
/// it is written; the API then skips its own validation, which is done locally
/// anyway.
pub fn tools_for(tools: &Value, eager: bool) -> Value {
    let list = tools.as_array().cloned().unwrap_or_default();
    Value::Array(
        list.into_iter()
            .map(|mut t| {
                if eager {
                    if let Some(o) = t.as_object_mut() {
                        o.insert("eager_input_streaming".into(), json!(true));
                    }
                }
                t
            })
            .collect(),
    )
}

#[derive(Default)]
pub struct Parser {
    blocks: Vec<Option<Value>>,
    partial_json: HashMap<usize, String>,
    /// Tool calls whose input wasn't valid JSON: index -> what arrived.
    invalid: HashMap<usize, String>,
    /// The bytes of text, thinking and tool input so far, signatures,
    /// redacted thinking and citations included.
    held: usize,
    model: Option<String>,
    stop_reason: Option<String>,
    refusal: Option<String>,
    usage: Usage,
    /// The index of the last `fallback` block, and the model it switched to.
    fallback_at: Option<usize>,
    answered_by: Option<String>,
    done: bool,
}

impl Parser {
    fn slot(&mut self, index: usize) -> Result<&mut Value, ProviderError> {
        self.blocks
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| ProviderError::bad_reply(format!("a delta for content block {index}, which never started")))
    }

    /// Counts what the reply's text, thinking and tool input hold; past
    /// `MAX_REPLY_TEXT` the reply is refused.
    fn hold(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.held += bytes;
        if self.held > MAX_REPLY_TEXT {
            return Err(ProviderError::bad_reply("the reply's text, thinking and tool input passed 8 MiB"));
        }
        Ok(())
    }

    pub fn feed(&mut self, ev: &sse::Event, out: &mut Vec<Ev>) -> Result<(), ProviderError> {
        let v: Value = serde_json::from_str(&ev.data).map_err(|_| ProviderError::bad_reply("an event that isn't JSON"))?;
        match s(&v, "type") {
            "message_start" => {
                let m = &v["message"];
                if let Some(model) = m.get("model").and_then(Value::as_str) {
                    self.model = Some(model.to_string());
                    out.push(Ev::Model(model.to_string()));
                }
                self.read_usage(&m["usage"]);
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).ok_or_else(|| ProviderError::bad_reply("a content block without an index"))? as usize;
                if index > 10_000 {
                    return Err(ProviderError::bad_reply("too many content blocks"));
                }
                let mut block = v.get("content_block").cloned().unwrap_or(Value::Null);
                if !block.is_object() {
                    return Err(ProviderError::bad_reply("a content block that isn't an object"));
                }
                self.hold(["text", "thinking", "signature", "data"].iter().map(|k| s(&block, k).len()).sum())?;
                match s(&block, "type") {
                    "tool_use" => {
                        block["input"] = json!({});
                        self.partial_json.insert(index, String::new());
                        out.push(Ev::ToolStart { id: s(&block, "id").to_string(), name: s(&block, "name").to_string() });
                    }
                    "fallback" => {
                        self.fallback_at = Some(index);
                        let to = block.pointer("/to/model").and_then(Value::as_str).map(str::to_string);
                        if to.is_some() {
                            self.answered_by = to;
                        }
                    }
                    _ => {}
                }
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, None);
                }
                self.blocks[index] = Some(block);
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
                let d = &v["delta"];
                match s(d, "type") {
                    "text_delta" => {
                        let t = s(d, "text").to_string();
                        self.hold(t.len())?;
                        append(self.slot(index)?, "text", &t);
                        out.push(Ev::Text(t));
                    }
                    "thinking_delta" => {
                        let t = s(d, "thinking").to_string();
                        self.hold(t.len())?;
                        append(self.slot(index)?, "thinking", &t);
                        out.push(Ev::Thinking(t));
                    }
                    "signature_delta" => {
                        let t = s(d, "signature").to_string();
                        self.hold(t.len())?;
                        append(self.slot(index)?, "signature", &t);
                    }
                    "input_json_delta" => {
                        self.slot(index)?;
                        let t = s(d, "partial_json");
                        self.hold(t.len())?;
                        let p = self.partial_json.entry(index).or_default();
                        p.push_str(t);
                        if p.len() > 4 * 1024 * 1024 {
                            return Err(ProviderError::bad_reply("a tool call's input passed 4 MiB"));
                        }
                    }
                    "citations_delta" => {
                        if let Some(c) = d.get("citation").cloned() {
                            self.hold(c.to_string().len())?;
                            let b = self.slot(index)?;
                            if !b["citations"].is_array() {
                                b["citations"] = json!([]);
                            }
                            if let Some(list) = b["citations"].as_array_mut() {
                                list.push(c);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
                if let Some(raw) = self.partial_json.remove(&index) {
                    // With eager input streaming the API doesn't check the input;
                    // it is parsed strictly here, and a bad one never runs.
                    let parsed = if raw.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(&raw) };
                    match parsed {
                        Ok(input) if input.is_object() => self.slot(index)?["input"] = input,
                        _ => {
                            self.invalid.insert(index, raw);
                        }
                    }
                }
            }
            "message_delta" => {
                let d = &v["delta"];
                if let Some(r) = d.get("stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_string());
                }
                if let Some(c) = d.pointer("/stop_details/category").and_then(Value::as_str) {
                    self.refusal = Some(c.to_string());
                }
                self.read_usage(&v["usage"]);
            }
            "message_stop" => self.done = true,
            "error" => {
                let e = &v["error"];
                return Err(ProviderError { status: None, kind: s(e, "type").to_string(), message: s(e, "message").to_string(), request_id: None, retry_after: None });
            }
            _ => {} // ping, and anything newer
        }
        Ok(())
    }

    fn read_usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        if u.get("input_tokens").is_some() {
            self.usage.input = n(u, "input_tokens");
        }
        if u.get("cache_read_input_tokens").is_some() {
            self.usage.cache_read = n(u, "cache_read_input_tokens");
        }
        if u.get("cache_creation_input_tokens").is_some() {
            self.usage.cache_write = n(u, "cache_creation_input_tokens");
        }
        if u.get("output_tokens").is_some() {
            self.usage.output = n(u, "output_tokens");
        }
    }

    pub fn finish(self) -> Result<Reply, ProviderError> {
        if !self.done && self.stop_reason.is_none() {
            return Err(ProviderError::ours("connection", "the reply stopped before it was finished"));
        }
        let mut content = Vec::new();
        let mut blocks = Vec::new();
        for (i, b) in self.blocks.into_iter().enumerate() {
            let Some(b) = b else { continue };
            let kind = s(&b, "type").to_string();
            // After a mid-reply fallback, the declined model's reasoning and
            // tool calls before the last fallback block are left out of what
            // is sent back, as the API asks; its text stays.
            if self.fallback_at.is_some_and(|f| i < f) && matches!(kind.as_str(), "thinking" | "redacted_thinking" | "tool_use" | "server_tool_use") {
                continue;
            }
            match kind.as_str() {
                "text" => blocks.push(Block::Text { text: s(&b, "text").to_string() }),
                "thinking" => {
                    let t = s(&b, "thinking");
                    if !t.is_empty() {
                        blocks.push(Block::Reasoning { text: t.to_string() });
                    }
                }
                "tool_use" => {
                    let input = if self.invalid.contains_key(&i) {
                        json!({"INVALID_JSON": self.invalid[&i]})
                    } else {
                        b["input"].clone()
                    };
                    blocks.push(Block::ToolCall { id: s(&b, "id").to_string(), name: s(&b, "name").to_string(), input });
                }
                _ => {}
            }
            content.push(b);
        }
        let stop = match self.stop_reason.as_deref() {
            Some("tool_use") => Stop::ToolUse,
            Some("max_tokens") | Some("model_context_window_exceeded") => Stop::CutOff,
            Some("refusal") => Stop::Refused,
            _ => Stop::End,
        };
        Ok(Reply {
            wire: vec![json!({"role": "assistant", "content": content})],
            turn: Turn { role: Role::Assistant, blocks },
            stop,
            usage: self.usage,
            model: self.model,
            answered_by: self.answered_by,
            refusal: self.refusal,
        })
    }
}

/// Adds a delta to a block's field in place: copying the whole field for each
/// delta would cost the square of the reply's length.
fn append(block: &mut Value, key: &str, text: &str) {
    match block.get_mut(key) {
        Some(Value::String(cur)) => cur.push_str(text),
        _ => block[key] = json!(text),
    }
}

/// A model's limits and features.
pub fn caps_from(m: &Value) -> ModelCaps {
    let c = &m["capabilities"];
    let adaptive = c.pointer("/thinking/types/adaptive/supported").and_then(Value::as_bool);
    let efforts = c.get("effort").filter(|e| e.is_object()).map(|e| {
        ["low", "medium", "high", "xhigh", "max"]
            .iter()
            .filter(|l| e.pointer(&format!("/{l}/supported")).and_then(Value::as_bool) == Some(true))
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
    });
    ModelCaps {
        adaptive,
        efforts,
        max_tokens: m.get("max_tokens").and_then(Value::as_u64).map(|v| v.min(u32::MAX as u64) as u32),
        max_input: m.get("max_input_tokens").and_then(Value::as_u64),
    }
}

pub fn model_info(m: &Value) -> Option<ModelInfo> {
    let id = m.get("id")?.as_str()?.to_string();
    let caps = caps_from(m);
    Some(ModelInfo {
        id,
        name: m.get("display_name").and_then(Value::as_str).map(str::to_string),
        context: caps.max_input,
        max_output: caps.max_tokens.map(u64::from),
        efforts: caps.efforts.unwrap_or_default(),
    })
}

pub fn models_headers(ep: &Endpoint) -> Result<Vec<(&'static str, reqwest::header::HeaderValue)>, String> {
    let key = ep.key.as_ref().ok_or("[AI] NO_KEY: this provider has no API key")?;
    Ok(vec![("x-api-key", secret_header(key)?), ("anthropic-version", plain_header(VERSION))])
}

#[cfg(test)]
mod tests {
    use super::super::profile::Profile;
    use super::super::{Kind, Settings};
    use super::*;
    use std::collections::HashSet;

    fn ep(base: &str) -> Endpoint {
        Endpoint { kind: Kind::Anthropic, profile: Profile::Custom, base: base.into(), key: Some(zeroize::Zeroizing::new("sk-ant-x".into())), allow_data_collection: false }
    }

    fn settings(model: &str) -> Settings {
        Settings { model: model.into(), effort: Some("medium".into()), max_tokens: None, show_reasoning: true, fallback: true }
    }

    fn run(events: &[&str]) -> (Vec<Ev>, Result<Reply, ProviderError>) {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for e in events {
            p.feed(&sse::Event { event: None, data: e.to_string() }, &mut out).unwrap();
        }
        (out, p.finish())
    }

    #[test]
    fn request_body_for_the_official_endpoint() {
        let dropped = HashSet::new();
        let st = settings("claude-opus-5-5");
        let caps = ModelCaps { adaptive: Some(true), efforts: Some(vec!["low".into(), "medium".into()]), max_tokens: Some(32_000), max_input: Some(1_000_000) };
        let req = Request { system: "sys", tools: None, history: vec![json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})], settings: &st, caps: Some(&caps), dropped: &dropped };
        let r = build(&ep(OFFICIAL), &req).unwrap();
        assert_eq!(r.url, "https://api.anthropic.com/v1/messages");
        let b = &r.body;
        assert_eq!(b["max_tokens"], 32_000, "capped at the model's limit");
        assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(b["output_config"], json!({"effort": "medium"}));
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("temperature").is_none());
        assert!(r.headers.iter().any(|(k, v)| *k == "anthropic-beta" && v == FALLBACK_BETA));
        assert!(r.headers.iter().any(|(k, v)| *k == "x-api-key" && v.is_sensitive()));
        // Same input, same bytes.
        assert_eq!(serde_json::to_string(b).unwrap(), serde_json::to_string(&build(&ep(OFFICIAL), &req).unwrap().body).unwrap());
    }

    #[test]
    fn no_fallback_off_the_official_endpoint_or_for_other_models() {
        let dropped = HashSet::new();
        for (base, model) in [("https://gateway.example", "claude-opus-5-5"), (OFFICIAL, "claude-haiku-5-5")] {
            let st = settings(model);
            let req = Request { system: "s", tools: None, history: vec![], settings: &st, caps: None, dropped: &dropped };
            let r = build(&ep(base), &req).unwrap();
            assert!(r.body.get("fallbacks").is_none(), "{base} {model}");
            assert!(!r.headers.iter().any(|(k, _)| *k == "anthropic-beta"));
        }
    }

    #[test]
    fn dropped_fields_stay_out() {
        let dropped: HashSet<String> = ["thinking.display".to_string(), "output_config".to_string()].into();
        let st = settings("claude-opus-5-5");
        let req = Request { system: "s", tools: None, history: vec![], settings: &st, caps: None, dropped: &dropped };
        let b = build(&ep(OFFICIAL), &req).unwrap().body;
        assert_eq!(b["thinking"], json!({"type": "adaptive"}));
        assert!(b.get("output_config").is_none());
    }

    #[test]
    fn a_streamed_reply_with_thinking_text_and_a_tool_call() {
        let (evs, reply) = run(&[
            r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":12,"cache_read_input_tokens":4,"cache_creation_input_tokens":0,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan."}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"/etc/hosts\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let r = reply.unwrap();
        assert_eq!(r.stop, Stop::ToolUse);
        assert_eq!(r.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(r.usage, Usage { input: 12, cache_read: 4, cache_write: 0, output: 40, cost: None });
        let c = &r.wire[0]["content"];
        assert_eq!(c[0], json!({"type": "thinking", "thinking": "Plan.", "signature": "sig=="}));
        assert_eq!(c[1]["text"], "Hello");
        assert_eq!(c[2]["input"], json!({"path": "/etc/hosts"}));
        assert_eq!(r.turn.blocks[2], Block::ToolCall { id: "toolu_1".into(), name: "read_file".into(), input: json!({"path": "/etc/hosts"}) });
        assert!(evs.contains(&Ev::Text("Hel".into())));
        assert!(evs.contains(&Ev::ToolStart { id: "toolu_1".into(), name: "read_file".into() }));
    }

    #[test]
    fn bad_tool_input_is_kept_for_an_invalid_json_result() {
        let (_, reply) = run(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_2","name":"run_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ls"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ]);
        let r = reply.unwrap();
        assert_eq!(r.wire[0]["content"][0]["input"], json!({}), "the stored copy keeps an object");
        match &r.turn.blocks[0] {
            Block::ToolCall { input, .. } => assert_eq!(input["INVALID_JSON"], "{\"command\": \"ls"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_fallback_drops_the_declined_models_reasoning_and_calls() {
        let (_, reply) = run(&[
            r#"{"type":"message_start","message":{"model":"claude-opus-4-8","usage":{}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"a"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Part"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":"Rest"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let r = reply.unwrap();
        let types: Vec<&str> = r.wire[0]["content"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["text", "fallback", "text"]);
        assert_eq!(r.answered_by.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn refusal_cut_off_and_a_broken_stream() {
        let (_, r) = run(&[r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"}}}"#]);
        let r = r.unwrap();
        assert_eq!((r.stop, r.refusal.as_deref()), (Stop::Refused, Some("cyber")));
        let (_, r) = run(&[r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#]);
        assert_eq!(r.unwrap().stop, Stop::CutOff);
        let (_, r) = run(&[r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"x"}}"#]);
        assert!(r.is_err(), "no stop reason and no message_stop");
        let mut p = Parser::default();
        let e = p.feed(&sse::Event { event: None, data: r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#.into() }, &mut Vec::new());
        assert_eq!(e.unwrap_err().kind, "overloaded_error");
        assert!(Parser::default().feed(&sse::Event { event: None, data: "not json".into() }, &mut Vec::new()).is_err());
    }

    #[test]
    fn a_reply_past_the_text_cap_is_refused() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        let mut feed = |p: &mut Parser, v: Value| p.feed(&sse::Event { event: None, data: v.to_string() }, &mut out);
        feed(&mut p, json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}})).unwrap();
        feed(&mut p, json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})).unwrap();
        let half = "x".repeat(MAX_REPLY_TEXT / 2);
        feed(&mut p, json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": half}})).unwrap();
        feed(&mut p, json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": half}})).unwrap();
        let e = feed(&mut p, json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "y"}})).unwrap_err();
        assert!(e.message.contains("8 MiB"), "{}", e.message);
        // A block that starts with its text counts too.
        let mut p = Parser::default();
        let big = "x".repeat(MAX_REPLY_TEXT + 1);
        assert!(feed(&mut p, json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": big}})).is_err());
    }

    #[test]
    fn tool_input_and_citations_count_toward_the_cap() {
        let mut out = Vec::new();
        let mut feed = |p: &mut Parser, v: Value| p.feed(&sse::Event { event: None, data: v.to_string() }, &mut out);
        let start = |index: usize, block: Value| json!({"type": "content_block_start", "index": index, "content_block": block});
        let half = "x".repeat(MAX_REPLY_TEXT / 2);
        // Two calls of 4 MiB: neither passes its own cap, and together they
        // fill the reply's.
        let mut p = Parser::default();
        for i in 0..2 {
            feed(&mut p, start(i, json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": "write_file", "input": {}}))).unwrap();
            feed(&mut p, json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": half}})).unwrap();
        }
        feed(&mut p, start(2, json!({"type": "text", "text": ""}))).unwrap();
        let e = feed(&mut p, json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "y"}})).unwrap_err();
        assert!(e.message.contains("tool input") && e.message.contains("8 MiB"), "{}", e.message);
        // A citation counts too.
        let mut p = Parser::default();
        feed(&mut p, start(0, json!({"type": "text", "text": half}))).unwrap();
        let cite = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "citations_delta", "citation": {"type": "char_location", "cited_text": half}}});
        assert!(feed(&mut p, cite).is_err());
    }

    #[test]
    fn caps_from_the_models_api() {
        let m = json!({"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1000000, "max_tokens": 128000,
            "capabilities": {"thinking": {"supported": true, "types": {"adaptive": {"supported": true}}},
                             "effort": {"supported": true, "low": {"supported": true}, "medium": {"supported": true}, "high": {"supported": true}, "xhigh": {"supported": false}, "max": {"supported": true}}}});
        let c = caps_from(&m);
        assert_eq!(c.adaptive, Some(true));
        assert_eq!(c.efforts.unwrap(), ["low", "medium", "high", "max"]);
        assert_eq!(c.max_tokens, Some(128_000));
        assert_eq!(model_info(&m).unwrap().name.as_deref(), Some("Claude Opus 5.5"));
    }
}
