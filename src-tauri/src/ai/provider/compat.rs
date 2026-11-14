//! Servers that speak OpenAI's Chat Completions: OpenRouter,
//! DeepSeek, Groq, Gemini's OpenAI endpoint, Ollama, LM Studio, and any other
//! address. They differ in the details a profile records (`profile.rs`).
//!
//! The streamed assistant message is put back together whole, fields the app
//! doesn't know included (Gemini's thought signature arrives as `extra_content`
//! on a tool call, OpenRouter's reasoning as `reasoning_details`), because some
//! servers refuse the next request when what they sent doesn't come back.

use super::profile::{Effort, Profile};
use super::{n, plain_header, s, secret_header, Endpoint, Ev, HttpRequest, ModelInfo, ProviderError, Reply, Request, Stop, Usage, MAX_REPLY_TEXT};
use crate::ai::canon::{Block, Role, Turn};
use crate::ai::sse;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const DROPPABLE: &[(&str, &str)] = &[
    ("stream_options", "stream_options"),
    ("include_usage", "stream_options"),
    ("reasoning_content", "echo:reasoning_content"),
    ("reasoning_details", "echo:reasoning_details"),
    ("extra_content", "echo:extra_content"),
    ("reasoning_effort", "effort"),
    ("reasoning", "effort"),
    ("thinking", "effort"),
    ("tool_choice", "tool_choice"),
    ("max_completion_tokens", "max_tokens"),
    ("max_tokens", "max_tokens"),
    // Never `provider`: it carries OpenRouter's `data_collection: "deny"`, and
    // its errors say "Provider returned error". Dropping it on such an error
    // would quietly send the chat to providers that may keep it.
];

pub fn build(ep: &Endpoint, req: &Request) -> Result<HttpRequest, String> {
    let p = ep.profile;
    let dropped = |f: &str| req.dropped.contains(f);
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for m in &req.history {
        messages.push(outgoing(m, p, req));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.settings.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    if p.sends_stream_options() && !dropped("stream_options") {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(tools) = req.tools {
        body.insert("tools".into(), tools_for(tools));
        if p.sends_tool_choice() && !dropped("tool_choice") {
            body.insert("tool_choice".into(), json!("auto"));
        }
        // OpenAI on Chat Completions takes tools only without reasoning
        // (GPT-5.4 and later).
        if p == Profile::OpenAiChat {
            body.insert("reasoning_effort".into(), json!("none"));
        }
    }
    if let Some(max) = req.settings.max_tokens {
        if !dropped("max_tokens") {
            body.insert(p.max_tokens_field().into(), json!(max));
        }
    }
    let effort = req.settings.effort.as_deref().filter(|e| !e.is_empty());
    if let (Some(e), false) = (effort, dropped("effort")) {
        match p.effort() {
            Effort::None => {}
            Effort::ReasoningEffort => {
                body.insert("reasoning_effort".into(), json!(e));
            }
            Effort::OpenRouter => {
                body.insert("reasoning".into(), json!({"effort": e}));
            }
            Effort::DeepSeekToggle => {
                if e == "none" {
                    body.insert("thinking".into(), json!({"type": "disabled"}));
                }
            }
        }
    }
    if p == Profile::OpenRouter && !ep.allow_data_collection {
        // OpenRouter routes to many upstream providers, and some may keep or
        // train on what they get; this asks it to use only those that don't.
        body.insert("provider".into(), json!({"data_collection": "deny"}));
    }
    let mut headers = Vec::new();
    if let Some(key) = ep.key.as_ref().filter(|k| !k.is_empty()) {
        headers.push(("authorization", secret_header(&format!("Bearer {}", key.as_str()))?));
    }
    Ok(HttpRequest { url: format!("{}/chat/completions", ep.base), headers, body: Value::Object(body) })
}

/// An entry of the history as it goes out. Assistant messages keep `role`,
/// `content`, `tool_calls` and the profile's own reasoning fields; the rest of
/// what the server once sent stays in the stored copy only.
fn outgoing(m: &Value, p: Profile, req: &Request) -> Value {
    if s(m, "role") != "assistant" {
        return m.clone();
    }
    let mut out = Map::new();
    out.insert("role".into(), json!("assistant"));
    out.insert("content".into(), m.get("content").cloned().unwrap_or(Value::Null));
    if let Some(calls) = m.get("tool_calls").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
        let calls: Vec<Value> = calls
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let mut c = c.clone();
                if let Some(o) = c.as_object_mut() {
                    o.remove("index");
                    if req.dropped.contains("echo:extra_content") {
                        o.remove("extra_content");
                    }
                }
                c
            })
            .collect();
        out.insert("tool_calls".into(), Value::Array(calls));
    }
    for f in p.echo_fields() {
        if req.dropped.contains(&format!("echo:{f}")) {
            continue;
        }
        if let Some(v) = m.get(*f) {
            out.insert((*f).into(), v.clone());
        }
    }
    Value::Object(out)
}

pub fn tools_for(tools: &Value) -> Value {
    let list = tools.as_array().cloned().unwrap_or_default();
    Value::Array(
        list.iter()
            .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}}))
            .collect(),
    )
}

#[derive(Default)]
struct Call {
    index: Option<u64>,
    id: String,
    name: String,
    arguments: String,
    /// The call as received, besides the joined fields above.
    extra: Map<String, Value>,
}

#[derive(Default)]
pub struct Parser {
    content: String,
    /// String reasoning fields (`reasoning_content`, `reasoning`), joined.
    reasoning: Map<String, Value>,
    details: Vec<Value>,
    /// Where in `details` the detail with each `index` is.
    detail_at: HashMap<u64, usize>,
    calls: Vec<Call>,
    /// Other fields of the message, last value wins (Gemini's `extra_content`).
    extra: Map<String, Value>,
    /// The bytes of text, reasoning and tool input so far.
    held: usize,
    finish: Option<String>,
    usage: Usage,
    model: Option<String>,
    done: bool,
}

impl Parser {
    pub fn feed(&mut self, ev: &sse::Event, out: &mut Vec<Ev>) -> Result<(), ProviderError> {
        let data = ev.data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).map_err(|_| ProviderError::bad_reply("a chunk that isn't JSON"))?;
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            let message = e.get("message").and_then(Value::as_str).or_else(|| e.as_str()).unwrap_or("error in the stream");
            let kind = e.get("type").and_then(Value::as_str).or_else(|| e.get("code").and_then(Value::as_str)).unwrap_or("");
            return Err(ProviderError { status: None, kind: kind.to_string(), message: message.to_string(), request_id: None, retry_after: None });
        }
        if self.model.is_none() {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                self.model = Some(m.to_string());
                out.push(Ev::Model(m.to_string()));
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.read_usage(u);
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
            return Ok(());
        };
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(f.to_string());
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return Ok(());
        };
        for (k, val) in delta {
            match k.as_str() {
                "role" => {}
                "content" => {
                    if let Some(t) = val.as_str() {
                        self.held += t.len();
                        self.content.push_str(t);
                        out.push(Ev::Text(t.to_string()));
                    }
                }
                "reasoning_content" | "reasoning" => {
                    if let Some(t) = val.as_str() {
                        self.held += t.len();
                        // Joined in place: a copy per piece would cost the
                        // square of the reasoning's length.
                        match self.reasoning.get_mut(k.as_str()) {
                            Some(Value::String(cur)) => cur.push_str(t),
                            _ => {
                                self.reasoning.insert(k.clone(), json!(t));
                            }
                        }
                        out.push(Ev::Thinking(t.to_string()));
                    }
                }
                "reasoning_details" => {
                    for d in val.as_array().into_iter().flatten() {
                        self.merge_detail(d, out);
                    }
                }
                "tool_calls" => {
                    for c in val.as_array().into_iter().flatten() {
                        self.merge_call(c, out);
                    }
                }
                _ => {
                    if !val.is_null() {
                        self.extra.insert(k.clone(), val.clone());
                    }
                }
            }
        }
        if self.held > MAX_REPLY_TEXT {
            return Err(ProviderError::bad_reply("the reply's text, reasoning and tool input passed 8 MiB"));
        }
        Ok(())
    }

    fn read_usage(&mut self, u: &Value) {
        self.usage.input = n(u, "prompt_tokens");
        self.usage.output = n(u, "completion_tokens");
        self.usage.cache_read = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
            .unwrap_or(0);
        self.usage.cache_write = u.pointer("/prompt_tokens_details/cache_write_tokens").and_then(Value::as_u64).unwrap_or(0);
        self.usage.cost = u.get("cost").and_then(Value::as_f64);
    }

    /// OpenRouter streams its reasoning details in pieces; pieces with the same
    /// `index` belong to one detail, and their text fields are joined.
    fn merge_detail(&mut self, d: &Value, out: &mut Vec<Ev>) {
        for key in ["text", "summary"] {
            if let Some(t) = d.get(key).and_then(Value::as_str) {
                out.push(Ev::Thinking(t.to_string()));
            }
        }
        self.held += detail_len(d);
        let idx = d.get("index").and_then(Value::as_u64);
        let pos = idx.and_then(|i| self.detail_at.get(&i).copied());
        match (pos, d.as_object()) {
            (Some(p), Some(piece)) => {
                let e = &mut self.details[p];
                for (k, v) in piece {
                    let joins = matches!(k.as_str(), "text" | "summary" | "data" | "signature");
                    match (joins, e.get_mut(k), v.as_str()) {
                        (true, Some(Value::String(a)), Some(b)) => a.push_str(b),
                        _ => e[k] = v.clone(),
                    }
                }
            }
            _ => {
                if let Some(i) = idx {
                    self.detail_at.insert(i, self.details.len());
                }
                self.details.push(d.clone());
            }
        }
    }

    /// Fragments of a call are grouped by `index`. Gemini sends none, and older
    /// Ollama sent `index: 0` for every parallel call, so a new `id` on an index
    /// already in use starts a new call too.
    fn merge_call(&mut self, c: &Value, out: &mut Vec<Ev>) {
        let index = c.get("index").and_then(Value::as_u64);
        let id = c.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let pos = match (index, id) {
            (_, Some(id)) if self.calls.iter().any(|k| k.id == id) => self.calls.iter().position(|k| k.id == id),
            (Some(i), Some(_)) => self.calls.iter().position(|k| k.index == Some(i) && k.id.is_empty()),
            (Some(i), None) => self.calls.iter().rposition(|k| k.index == Some(i)),
            (None, Some(_)) => None,
            (None, None) => self.calls.len().checked_sub(1),
        };
        let pos = match pos {
            Some(p) => p,
            None => {
                self.calls.push(Call { index, ..Default::default() });
                self.calls.len() - 1
            }
        };
        let call = &mut self.calls[pos];
        if let (true, Some(id)) = (call.id.is_empty(), id) {
            call.id = id.to_string();
        }
        if let Some(f) = c.get("function") {
            if let Some(name) = f.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                if call.name.is_empty() {
                    call.name = name.to_string();
                    out.push(Ev::ToolStart { id: call.id.clone(), name: call.name.clone() });
                }
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                self.held += a.len();
                call.arguments.push_str(a);
            }
        }
        if let Some(o) = c.as_object() {
            for (k, v) in o {
                if !matches!(k.as_str(), "index" | "id" | "function" | "type") && !v.is_null() {
                    call.extra.insert(k.clone(), v.clone());
                }
            }
        }
    }

    pub fn finish(mut self) -> Result<Reply, ProviderError> {
        if !self.done && self.finish.is_none() {
            return Err(ProviderError::ours("connection", "the reply stopped before it was finished"));
        }
        // A call the server sent without an id gets one of its own: a run
        // never runs two calls with the same id, and the result must name
        // the call it answers.
        for c in self.calls.iter_mut().filter(|c| c.id.is_empty()) {
            c.id = new_call_id();
        }
        let mut blocks = Vec::new();
        let mut msg = Map::new();
        msg.insert("role".into(), json!("assistant"));
        msg.insert("content".into(), if self.content.is_empty() { Value::Null } else { json!(self.content) });
        let reasoning_text = self
            .reasoning
            .get("reasoning_content")
            .or_else(|| self.reasoning.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !reasoning_text.is_empty() {
            blocks.push(Block::Reasoning { text: reasoning_text });
        }
        if !self.content.is_empty() {
            blocks.push(Block::Text { text: self.content.clone() });
        }
        if !self.calls.is_empty() {
            let mut calls = Vec::new();
            for c in &self.calls {
                let input = match serde_json::from_str::<Value>(if c.arguments.trim().is_empty() { "{}" } else { &c.arguments }) {
                    Ok(v) if v.is_object() => v,
                    _ => json!({"INVALID_JSON": c.arguments}),
                };
                blocks.push(Block::ToolCall { id: c.id.clone(), name: c.name.clone(), input });
                let mut w = c.extra.clone();
                w.insert("id".into(), json!(c.id));
                w.insert("type".into(), json!("function"));
                w.insert("function".into(), json!({"name": c.name, "arguments": c.arguments}));
                calls.push(Value::Object(w));
            }
            msg.insert("tool_calls".into(), Value::Array(calls));
        }
        for (k, v) in self.reasoning {
            msg.insert(k, v);
        }
        if !self.details.is_empty() {
            msg.insert("reasoning_details".into(), Value::Array(self.details));
        }
        for (k, v) in self.extra {
            msg.entry(k).or_insert(v);
        }
        let stop = match self.finish.as_deref() {
            Some("length") => Stop::CutOff,
            Some("content_filter") => Stop::Refused,
            Some("tool_calls") | Some("function_call") => Stop::ToolUse,
            _ if !self.calls.is_empty() => Stop::ToolUse,
            _ => Stop::End,
        };
        Ok(Reply { wire: vec![Value::Object(msg)], turn: Turn { role: Role::Assistant, blocks }, stop, usage: self.usage, model: self.model, answered_by: None, refusal: None })
    }
}

/// A call id for a call that came without one: random, so no two calls of a
/// chat share it, in the form every provider takes (`[a-zA-Z0-9_-]`).
fn new_call_id() -> String {
    use rand::Rng;
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    format!("call_{}", hex::encode(b))
}

/// The text one piece of reasoning details carries.
fn detail_len(d: &Value) -> usize {
    match d {
        Value::String(t) => t.len(),
        Value::Object(o) => o.values().filter_map(Value::as_str).map(str::len).sum(),
        _ => 0,
    }
}

pub fn model_info(m: &Value) -> Option<ModelInfo> {
    let id = m.get("id")?.as_str()?.to_string();
    let context = m
        .get("context_length")
        .and_then(Value::as_u64)
        .or_else(|| m.get("context_window").and_then(Value::as_u64));
    let max_output = m
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .or_else(|| m.pointer("/top_provider/max_completion_tokens").and_then(Value::as_u64));
    Some(ModelInfo { id, name: m.get("name").and_then(Value::as_str).map(str::to_string), context, max_output, efforts: vec![] })
}

pub fn models_url(ep: &Endpoint) -> String {
    if ep.profile == Profile::OpenRouter {
        format!("{}/models?supported_parameters=tools", ep.base)
    } else {
        format!("{}/models", ep.base)
    }
}

pub fn models_headers(ep: &Endpoint) -> Result<Vec<(&'static str, reqwest::header::HeaderValue)>, String> {
    let mut h = vec![("accept", plain_header("application/json"))];
    if let Some(key) = ep.key.as_ref().filter(|k| !k.is_empty()) {
        h.push(("authorization", secret_header(&format!("Bearer {}", key.as_str()))?));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, Settings};
    use super::*;
    use std::collections::HashSet;

    fn ep(p: Profile) -> Endpoint {
        Endpoint { kind: Kind::Compat, profile: p, base: "https://x.example/v1".into(), key: Some(zeroize::Zeroizing::new("k".into())), allow_data_collection: false }
    }

    fn st(effort: Option<&str>) -> Settings {
        Settings { model: "m".into(), effort: effort.map(str::to_string), max_tokens: Some(100), show_reasoning: true, fallback: true }
    }

    fn parse(chunks: &[&str]) -> (Vec<Ev>, Reply) {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for c in chunks {
            p.feed(&sse::Event { event: None, data: c.to_string() }, &mut out).unwrap();
        }
        (out, p.finish().unwrap())
    }

    #[test]
    fn profiles_shape_the_request() {
        let dropped = HashSet::new();
        let s = st(Some("high"));
        let req = |h: Vec<Value>| Request { system: "sys", tools: None, history: h, settings: &s, caps: None, dropped: &dropped };
        let b = build(&ep(Profile::Groq), &req(vec![])).unwrap().body;
        assert_eq!(b["max_completion_tokens"], 100);
        assert_eq!(b["reasoning_effort"], "high");
        assert_eq!(b["stream_options"], json!({"include_usage": true}));
        let b = build(&ep(Profile::OpenRouter), &req(vec![])).unwrap().body;
        assert_eq!(b["reasoning"], json!({"effort": "high"}));
        assert_eq!(b["provider"], json!({"data_collection": "deny"}));
        assert!(b.get("stream_options").is_none());
        let b = build(&ep(Profile::Custom), &req(vec![])).unwrap().body;
        assert_eq!(b["max_tokens"], 100);
        assert!(b.get("reasoning_effort").is_none());
        let s2 = st(Some("none"));
        let r2 = Request { system: "s", tools: None, history: vec![], settings: &s2, caps: None, dropped: &dropped };
        assert_eq!(build(&ep(Profile::DeepSeek), &r2).unwrap().body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn assistant_turns_go_back_with_the_profiles_own_fields_only() {
        let dropped = HashSet::new();
        let s = st(None);
        let stored = json!({"role": "assistant", "content": null, "reasoning_content": "r", "reasoning": "q", "extra_content": {"google": {"thought_signature": "sig"}},
            "tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}, "extra_content": {"google": {"thought_signature": "s2"}}}]});
        let req = Request { system: "s", tools: None, history: vec![stored.clone()], settings: &s, caps: None, dropped: &dropped };
        let deep = build(&ep(Profile::DeepSeek), &req).unwrap().body;
        let m = &deep["messages"][1];
        assert_eq!(m["reasoning_content"], "r");
        assert!(m.get("reasoning").is_none() && m.get("extra_content").is_none());
        assert!(m["tool_calls"][0].get("index").is_none());
        let gem = build(&ep(Profile::Gemini), &req).unwrap().body;
        let m = &gem["messages"][1];
        assert_eq!(m["extra_content"]["google"]["thought_signature"], "sig");
        assert_eq!(m["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "s2");
        assert!(m.get("reasoning_content").is_none());
    }

    #[test]
    fn text_reasoning_and_usage_on_any_chunk() {
        let (evs, r) = parse(&[
            r#"{"model":"deepseek-v4-pro","choices":[{"delta":{"role":"assistant","reasoning_content":"Think"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"prompt_cache_hit_tokens":4}}"#,
            "[DONE]",
        ]);
        assert_eq!(r.stop, Stop::End);
        assert_eq!(r.wire[0]["content"], "Hello");
        assert_eq!(r.wire[0]["reasoning_content"], "Think");
        assert_eq!((r.usage.input, r.usage.output, r.usage.cache_read), (9, 2, 4));
        assert!(evs.contains(&Ev::Thinking("Think".into())));
        // OpenAI's own usage chunk has no choices at all.
        let (_, r) = parse(&[r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#, r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":2}}}"#, "[DONE]"]);
        assert_eq!((r.usage.input, r.usage.cache_read), (5, 2));
    }

    #[test]
    fn tool_calls_by_index_without_index_and_with_a_reused_index() {
        let (_, r) = parse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"one","arguments":"{\"x\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","type":"function","function":{"name":"two","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"c","type":"function","function":{"name":"three","arguments":"{}"},"extra_content":{"google":{"thought_signature":"s"}}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        let ids: Vec<&str> = r.wire[0]["tool_calls"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(r.wire[0]["tool_calls"][0]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(r.wire[0]["tool_calls"][2]["extra_content"]["google"]["thought_signature"], "s");
        assert_eq!(r.stop, Stop::ToolUse);
        assert_eq!(r.turn.blocks[0], Block::ToolCall { id: "a".into(), name: "one".into(), input: json!({"x": 1}) });
    }

    #[test]
    fn calls_without_an_id_get_one_of_their_own() {
        let reply = || {
            parse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"run_command","arguments":"{\"command\":\"uptime\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            ])
            .1
        };
        let (a, b) = (reply(), reply());
        let ids = |r: &Reply| -> Vec<String> {
            r.turn.blocks.iter().filter_map(|b| if let Block::ToolCall { id, .. } = b { Some(id.clone()) } else { None }).collect()
        };
        let (ia, ib) = (ids(&a), ids(&b));
        assert_eq!(ia.len(), 2);
        for id in ia.iter().chain(&ib) {
            assert!(id.len() == 29 && id.starts_with("call_") && id[5..].bytes().all(|c| c.is_ascii_hexdigit()), "{id}");
        }
        assert!(ia[0] != ia[1] && !ib.contains(&ia[0]) && !ib.contains(&ia[1]), "no two alike, in a reply or across replies");
        // The stored copy names the same calls the results will answer.
        let wire: Vec<&str> = a.wire[0]["tool_calls"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(wire, ia);
        // An id the server sent is kept.
        let (_, r) = parse(&[r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c9","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#]);
        assert_eq!(ids(&r), ["c9"]);
    }

    #[test]
    fn openrouter_reasoning_details_are_joined_by_index() {
        let (_, r) = parse(&[
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"Lo"}]}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","index":0,"text":"ok","signature":"zz"}]}}]}"#,
            r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(r.wire[0]["reasoning_details"], json!([{"type": "reasoning.text", "index": 0, "text": "Look", "signature": "zz"}]));
    }

    #[test]
    fn reasoning_in_pieces_and_details_interleaved_by_index() {
        let (_, r) = parse(&[
            r#"{"choices":[{"delta":{"reasoning":"Th","reasoning_details":[{"type":"reasoning.text","index":0,"text":"A"}]}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"ink","reasoning_details":[{"type":"reasoning.encrypted","index":1,"data":"x"},{"type":"reasoning.text","index":0,"text":"B"}]}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","index":1,"data":"y"}]},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(r.wire[0]["reasoning"], "Think");
        assert_eq!(r.turn.blocks[0], Block::Reasoning { text: "Think".into() });
        assert_eq!(r.wire[0]["reasoning_details"], json!([{"type": "reasoning.text", "index": 0, "text": "AB"}, {"type": "reasoning.encrypted", "index": 1, "data": "xy"}]));
    }

    #[test]
    fn a_reply_past_the_text_cap_is_refused() {
        let chunk = |field: &str, t: &str| {
            let mut delta = Map::new();
            delta.insert(field.to_string(), json!(t));
            sse::Event { event: None, data: json!({"choices": [{"delta": Value::Object(delta)}]}).to_string() }
        };
        let half = "x".repeat(MAX_REPLY_TEXT / 2);
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(&chunk("reasoning_content", &half), &mut out).unwrap();
        p.feed(&chunk("content", &half), &mut out).unwrap();
        let e = p.feed(&chunk("content", "y"), &mut out).unwrap_err();
        assert!(e.message.contains("8 MiB"), "{}", e.message);
    }

    #[test]
    fn tool_input_counts_toward_the_cap() {
        let args = |t: &str| sse::Event { event: None, data: json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "write_file", "arguments": t}}]}}]}).to_string() };
        let half = "x".repeat(MAX_REPLY_TEXT / 2);
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(&sse::Event { event: None, data: json!({"choices": [{"delta": {"content": half}}]}).to_string() }, &mut out).unwrap();
        p.feed(&args(&half), &mut out).unwrap();
        let e = p.feed(&args("y"), &mut out).unwrap_err();
        assert!(e.message.contains("tool input") && e.message.contains("8 MiB"), "{}", e.message);
    }

    #[test]
    fn errors_and_cut_streams() {
        let mut p = Parser::default();
        assert!(p.feed(&sse::Event { event: None, data: r#"{"error":{"message":"overloaded","type":"server_error"}}"#.into() }, &mut Vec::new()).is_err());
        let mut p = Parser::default();
        p.feed(&sse::Event { event: None, data: r#"{"choices":[{"delta":{"content":"half"}}]}"#.into() }, &mut Vec::new()).unwrap();
        assert!(p.finish().is_err());
        let (_, r) = parse(&[r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#]);
        assert_eq!(r.stop, Stop::CutOff);
    }
}
