//! The three kinds of provider: Anthropic's Messages API, OpenAI's
//! Responses API, and the servers that speak Chat Completions (one profile
//! each). Each adapter builds a request from the chat's fixed system prompt and
//! the history already shaped for its target, and parses the streamed reply
//! back into the reply's exact wire copy plus its neutral copy.

pub mod anthropic;
pub mod compat;
pub mod openai;
pub mod profile;

use super::canon::{Target, Turn};
use super::sse;
use profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    Compat,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
            Kind::Compat => "compat",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "anthropic" => Some(Kind::Anthropic),
            "openai" => Some(Kind::OpenAi),
            "compat" => Some(Kind::Compat),
            _ => None,
        }
    }
}

/// Where one provider's requests go, with its key. Two are equal when a
/// request would go out the same way with either.
#[derive(PartialEq)]
pub struct Endpoint {
    pub kind: Kind,
    pub profile: Profile,
    /// The base URL, already checked (`net::normalize_base`), no trailing `/`.
    pub base: String,
    pub key: Option<Zeroizing<String>>,
    /// OpenRouter only: let it route to providers that may keep or train on data.
    pub allow_data_collection: bool,
}

impl Endpoint {
    pub fn target(&self) -> Target {
        match self.kind {
            Kind::Anthropic => Target::Anthropic,
            Kind::OpenAi => Target::OpenAi,
            Kind::Compat => Target::Compat(self.profile),
        }
    }
}

/// A chat's settings for one request. They can change between
/// messages; the system prompt and the tool list can't.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    /// `None`: the provider's own default.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Anthropic: ask for readable reasoning summaries (`display: "summarized"`).
    #[serde(default = "yes")]
    pub show_reasoning: bool,
    /// Anthropic: retry a declined request on another model, server-side.
    #[serde(default = "yes")]
    pub fallback: bool,
}

fn yes() -> bool {
    true
}

/// What an Anthropic model takes, from `GET /v1/models/{id}`. Unknown fields
/// mean the request sends the setting and lets a 400 decide.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCaps {
    pub adaptive: Option<bool>,
    pub efforts: Option<Vec<String>>,
    pub max_tokens: Option<u32>,
    pub max_input: Option<u64>,
}

pub struct Request<'a> {
    pub system: &'a str,
    /// The tool list in its neutral form; `None` for plain chats.
    pub tools: Option<&'a Value>,
    /// The history, each entry already in this target's own shape.
    pub history: Vec<Value>,
    pub settings: &'a Settings,
    pub caps: Option<&'a ModelCaps>,
    /// Request fields this provider refused with a 400 that named them;
    /// they are left out from then on.
    pub dropped: &'a HashSet<String>,
}

pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(&'static str, reqwest::header::HeaderValue)>,
    pub body: Value,
}

/// A header value holding a secret. Marked sensitive, it stays out of debug
/// output, so a printed request never shows the key.
pub fn secret_header(value: &str) -> Result<reqwest::header::HeaderValue, String> {
    let mut v = reqwest::header::HeaderValue::from_str(value).map_err(|_| "[AI] BAD_KEY: the API key has characters a header can't carry".to_string())?;
    v.set_sensitive(true);
    Ok(v)
}

pub fn plain_header(value: &'static str) -> reqwest::header::HeaderValue {
    reqwest::header::HeaderValue::from_static(value)
}

/// The most text, reasoning and tool input one reply may hold, all together.
/// A long answer is a small part of this; a reply past it is refused rather
/// than kept, since the store keeps each reply about three times.
pub const MAX_REPLY_TEXT: usize = 8 * 1024 * 1024;

/// Shown where a provider's text repeats the API key.
const KEY_HIDDEN: &str = "[API key]";
/// A key shorter than this is a placeholder (Ollama's docs use `ollama`), and
/// hiding it would garble ordinary words.
const MIN_HIDDEN: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// What the provider says the request cost (OpenRouter reports it).
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    End,
    ToolUse,
    /// Cut off by the output limit. Tool calls in such a turn are not run.
    CutOff,
    /// Declined by the provider. Nothing from such a turn is kept in the history.
    Refused,
}

/// What a stream says while it runs, for the page.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    Text(String),
    Thinking(String),
    ToolStart { id: String, name: String },
    Model(String),
}

/// A finished reply.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The reply's entries exactly as the provider sent them, for this target.
    pub wire: Vec<Value>,
    /// The same reply, provider-neutral.
    pub turn: Turn,
    pub stop: Stop,
    pub usage: Usage,
    pub model: Option<String>,
    /// The model that answered after a server-side fallback.
    pub answered_by: Option<String>,
    /// Why the provider declined, when it says (shown for information only).
    pub refusal: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderError {
    pub status: Option<u16>,
    /// The provider's error type (`rate_limit_error`, …), or ours:
    /// `connection`, `timeout`, `bad_reply`.
    pub kind: String,
    pub message: String,
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub fn ours(kind: &str, message: impl Into<String>) -> Self {
        ProviderError { status: None, kind: kind.into(), message: message.into(), request_id: None, retry_after: None }
    }

    pub fn bad_reply(message: impl Into<String>) -> Self {
        Self::ours("bad_reply", message)
    }

    /// Worth trying again before any of the reply arrived.
    pub fn retryable(&self) -> bool {
        match self.status {
            Some(s) => matches!(s, 408 | 429 | 500 | 502 | 503 | 504 | 529),
            None => matches!(self.kind.as_str(), "connection" | "overloaded_error"),
        }
    }

    /// The field a 400 complains about, among those a request can do without.
    pub fn droppable_field(&self, candidates: &[(&str, &str)]) -> Option<String> {
        if self.status != Some(400) {
            return None;
        }
        let msg = self.message.to_ascii_lowercase();
        candidates.iter().find(|(needle, _)| msg.contains(needle)).map(|(_, field)| field.to_string())
    }

    /// The error with the request's secrets taken out of the provider's own
    /// text (its message, error type and request id), as `hide_keys` does.
    pub fn without_key(mut self, headers: &[(&'static str, reqwest::header::HeaderValue)]) -> Self {
        for t in [&mut self.message, &mut self.kind].into_iter().chain(self.request_id.as_mut()) {
            *t = hide_keys(t.as_str(), headers);
        }
        self
    }

    /// What the user is shown: the status, the error type and message, the
    /// request id. Never a header; a key the provider quoted is taken out
    /// first (`without_key`).
    pub fn to_user(&self) -> String {
        let mut s = match self.status {
            Some(401) => "[AI] KEY_REJECTED: the provider rejected the API key".to_string(),
            Some(st) => format!("[AI] PROVIDER_ERROR {st}"),
            None => "[AI] PROVIDER_ERROR".to_string(),
        };
        if !self.kind.is_empty() {
            s.push_str(&format!(" ({})", self.kind));
        }
        if !self.message.is_empty() {
            let m: String = self.message.chars().take(400).collect();
            s.push_str(&format!(": {m}"));
        }
        if let Some(id) = &self.request_id {
            s.push_str(&format!(" [request {id}]"));
        }
        s
    }
}

/// A provider's text with the request's secrets taken out: a gateway that
/// quotes the key it got has it shown and saved as "[API key]". The secrets
/// are the request's sensitive headers, and a `Bearer` header's key on its own.
pub fn hide_keys(text: &str, headers: &[(&'static str, reqwest::header::HeaderValue)]) -> String {
    let mut out = text.to_string();
    for (_, v) in headers.iter().filter(|(_, v)| v.is_sensitive()) {
        let Ok(full) = std::str::from_utf8(v.as_bytes()) else { continue };
        // The whole value first, so "Bearer <key>" goes as one.
        for secret in [full, full.strip_prefix("Bearer ").unwrap_or("")] {
            if secret.len() >= MIN_HIDDEN && out.contains(secret) {
                out = out.replace(secret, KEY_HIDDEN);
            }
        }
    }
    out
}

/// An error reply's body, whatever shape the provider gave it.
pub fn parse_error(status: u16, body: &str, request_id: Option<String>, retry_after: Option<Duration>) -> ProviderError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = v.get("error").unwrap_or(&v);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| err.as_str())
        .or_else(|| v.get("detail").and_then(Value::as_str))
        .or_else(|| v.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect());
    let kind = err
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| err.get("code").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let request_id = request_id.or_else(|| v.get("request_id").and_then(Value::as_str).map(str::to_string));
    ProviderError { status: Some(status), kind, message, request_id, retry_after }
}

pub fn build(ep: &Endpoint, req: &Request) -> Result<HttpRequest, String> {
    match ep.kind {
        Kind::Anthropic => anthropic::build(ep, req),
        Kind::OpenAi => openai::build(ep, req),
        Kind::Compat => compat::build(ep, req),
    }
}

/// The request fields a 400 may name, and what to leave out when it does.
pub fn droppable(kind: Kind) -> &'static [(&'static str, &'static str)] {
    match kind {
        Kind::Anthropic => anthropic::DROPPABLE,
        Kind::OpenAi => openai::DROPPABLE,
        Kind::Compat => compat::DROPPABLE,
    }
}

/// What a field a 400 named is remembered by, for the provider. One model
/// refuses `include`, not the provider, so it is kept for that model only.
pub fn drop_key(field: String, model: &str) -> String {
    if field == "include" {
        openai::include_key(model)
    } else {
        field
    }
}

pub enum Parser {
    Anthropic(anthropic::Parser),
    OpenAi(openai::Parser),
    Compat(compat::Parser),
}

impl Parser {
    pub fn new(kind: Kind) -> Self {
        match kind {
            Kind::Anthropic => Parser::Anthropic(anthropic::Parser::default()),
            Kind::OpenAi => Parser::OpenAi(openai::Parser::default()),
            Kind::Compat => Parser::Compat(compat::Parser::default()),
        }
    }

    pub fn feed(&mut self, ev: &sse::Event, out: &mut Vec<Ev>) -> Result<(), ProviderError> {
        match self {
            Parser::Anthropic(p) => p.feed(ev, out),
            Parser::OpenAi(p) => p.feed(ev, out),
            Parser::Compat(p) => p.feed(ev, out),
        }
    }

    pub fn finish(self) -> Result<Reply, ProviderError> {
        match self {
            Parser::Anthropic(p) => p.finish(),
            Parser::OpenAi(p) => p.finish(),
            Parser::Compat(p) => p.finish(),
        }
    }
}

/// A model in a provider's list, for the model picker.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub context: Option<u64>,
    pub max_output: Option<u64>,
    pub efforts: Vec<String>,
}

/// The effort levels the chat header offers for a provider and model.
pub fn effort_levels(ep: &Endpoint, caps: Option<&ModelCaps>) -> Vec<String> {
    match ep.kind {
        Kind::Anthropic => caps
            .and_then(|c| c.efforts.clone())
            .unwrap_or_else(|| ["low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect()),
        Kind::OpenAi => ["none", "minimal", "low", "medium", "high", "xhigh", "max"].iter().map(|s| s.to_string()).collect(),
        Kind::Compat => ep.profile.effort_levels().iter().map(|s| s.to_string()).collect(),
    }
}

/// Text from a JSON field, or empty.
pub(crate) fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_of_each_shape() {
        let a = parse_error(429, r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"},"request_id":"req_1"}"#, None, Some(Duration::from_secs(3)));
        assert_eq!((a.kind.as_str(), a.message.as_str(), a.request_id.as_deref()), ("rate_limit_error", "slow down", Some("req_1")));
        assert!(a.retryable());
        let o = parse_error(400, r#"{"error":{"message":"Unrecognized request argument supplied: stream_options","type":"invalid_request_error","param":"stream_options"}}"#, None, None);
        assert_eq!(o.droppable_field(compat::DROPPABLE).as_deref(), Some("stream_options"));
        assert!(!o.retryable());
        let plain = parse_error(502, "<html>bad gateway</html>", None, None);
        assert!(plain.message.contains("bad gateway"));
        assert!(plain.retryable());
        let k = parse_error(401, r#"{"error":"invalid key"}"#, None, None);
        assert!(k.to_user().starts_with("[AI] KEY_REJECTED"));
    }

    #[test]
    fn a_key_the_provider_quotes_is_hidden() {
        let bearer = vec![("authorization", secret_header("Bearer sk-live-0123456789").unwrap()), ("accept", plain_header("application/json"))];
        let e = parse_error(401, r#"{"error":{"message":"bad key: Bearer sk-live-0123456789 (sk-live-0123456789)","type":"auth_sk-live-0123456789"}}"#, Some("id-sk-live-0123456789".into()), None).without_key(&bearer);
        assert_eq!(e.message, "bad key: [API key] ([API key])");
        assert_eq!(e.kind, "auth_[API key]");
        assert_eq!(e.request_id.as_deref(), Some("id-[API key]"));
        assert!(!e.to_user().contains("sk-live"));

        let api_key = vec![("x-api-key", secret_header("sk-ant-0123456789").unwrap())];
        let e = ProviderError::ours("redirect", "a redirect to https://x.example/?k=sk-ant-0123456789").without_key(&api_key);
        assert_eq!(e.message, "a redirect to https://x.example/?k=[API key]");

        // A placeholder key, and the text of headers that aren't secret, stay.
        let placeholder = vec![("authorization", secret_header("Bearer ollama").unwrap())];
        assert_eq!(ProviderError::ours("x", "ollama: no such model").without_key(&placeholder).message, "ollama: no such model");
        assert_eq!(ProviderError::ours("x", "wants application/json").without_key(&bearer).message, "wants application/json");
    }

    #[test]
    fn a_key_is_hidden_before_a_body_is_cut() {
        let bearer = vec![("authorization", secret_header("Bearer sk-or-v1-0123456789abcdef").unwrap())];
        // Plain text, and JSON of no shape the message is read from (FastAPI's
        // detail list): the message is the body's first 300 characters, and
        // the key crosses that point.
        for body in [
            format!("{}Bearer sk-or-v1-0123456789abcdef", "x".repeat(280)),
            format!(r#"{{"detail":[{{"msg":"{}","input":"Bearer sk-or-v1-0123456789abcdef"}}]}}"#, "x".repeat(250)),
        ] {
            let cut_first = parse_error(422, &body, None, None).without_key(&bearer);
            assert!(cut_first.message.contains("sk-or-v1"), "cut first, the start of the key stays");
            let e = parse_error(422, &hide_keys(&body, &bearer), None, None).without_key(&bearer);
            assert!(e.message.contains("[API key]") && !e.message.contains("sk-or"), "{}", e.message);
        }
        assert_eq!(hide_keys("a Bearer sk-or-v1-0123456789abcdef, sk-or-v1-0123456789abcdef", &bearer), "a [API key], [API key]");
    }

    #[test]
    fn openrouters_data_rule_is_never_dropped() {
        // OpenRouter's usual text for an upstream failure.
        let e = parse_error(400, r#"{"error":{"message":"Provider returned error","code":400}}"#, None, None);
        assert_eq!(e.droppable_field(compat::DROPPABLE), None);
        let e = parse_error(400, r#"{"error":{"message":"provider: unknown field data_collection"}}"#, None, None);
        assert_eq!(e.droppable_field(compat::DROPPABLE), None);
    }
}
