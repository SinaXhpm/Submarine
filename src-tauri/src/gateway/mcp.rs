//! The Model Context Protocol, as much of it as the gateway speaks: JSON-RPC
//! 2.0 messages, one a line, for both generations of clients. Nothing here
//! does I/O or knows about servers and grants: a line comes in, and what to
//! do about it comes out. The bridge uses the same code to answer by itself
//! when the app can't be reached.
//!
//! Read against the protocol's own schema files on 2026-11-10:
//! - 2026-07-28 ("modern"): no handshake. Every request names its protocol
//!   version and the client's capabilities in `params._meta`,
//!   `server/discover` lists what the server speaks, and every result has a
//!   `resultType`. There is no `ping`.
//! - 2025-11-25 and earlier ("legacy"): `initialize` comes first and picks
//!   the version for the connection.
//!
//! One connection may use either, and both at once. What is left out on
//! purpose: resources, prompts, sampling, and everything that asks the user
//! something through the agent's own window. Questions to the user are asked
//! in Submarine's window only.

use crate::ai::shape::cut;
use serde_json::{json, Map, Value};

pub const MODERN: &str = "2026-07-28";
/// The handshake versions, newest first. Their `initialize` is the same in
/// all four; what a tool's definition may carry differs (`ToolDef::shown`).
/// 2025-03-26 also had JSON-RPC batches, which no known client sends: a batch
/// is refused in every version.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// The protocol's own code for a version the server doesn't speak. The codes
/// from -32020 to -32099 belong to the protocol: no other one is used here.
pub const UNSUPPORTED_VERSION: i64 = -32022;

const KEY_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const KEY_CLIENT: &str = "io.modelcontextprotocol/clientInfo";
const KEY_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const KEY_SERVER: &str = "io.modelcontextprotocol/serverInfo";
const KEY_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";

/// How long a client may keep the tool list and the discover result: both
/// are the same for every caller and change only with the app's version.
const TTL_MS: u64 = 60 * 60 * 1000;
/// The longest id that is given back. A longer one is refused, not echoed.
const MAX_ID: usize = 256;
const MAX_CLIENT_NAME: usize = 64;
const MAX_CLIENT_VERSION: usize = 32;

/// A request's id: a string or a whole number, given back as it came.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Id {
    Number(i128),
    Text(String),
}

impl Id {
    pub(super) fn read(v: &Value) -> Option<Id> {
        match v {
            Value::String(s) if s.len() <= MAX_ID => Some(Id::Text(s.clone())),
            Value::Number(n) => n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from)).map(Id::Number),
            _ => None,
        }
    }

    fn value(&self) -> Value {
        match self {
            Id::Text(s) => Value::String(s.clone()),
            Id::Number(n) => match i64::try_from(*n) {
                Ok(v) => Value::from(v),
                Err(_) => Value::from(u64::try_from(*n).unwrap_or(u64::MAX)),
            },
        }
    }
}

/// Which generation a request is served in, and for a legacy one, the version
/// its connection agreed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    Modern,
    Legacy(&'static str),
}

impl Era {
    fn version(self) -> &'static str {
        match self {
            Era::Modern => MODERN,
            Era::Legacy(v) => v,
        }
    }
}

/// What a client says about itself. Its own claim: shown as one, never used
/// for a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub version: String,
}

impl Client {
    fn read(v: &Value) -> Option<Client> {
        let name = v.get("name")?.as_str()?;
        let version = v.get("version").and_then(Value::as_str).unwrap_or("");
        Some(Client { name: cut(name, MAX_CLIENT_NAME).to_string(), version: cut(version, MAX_CLIENT_VERSION).to_string() })
    }
}

/// One tool as `tools/list` shows it.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The JSON Schema of its arguments, `type: object` at the root.
    pub input: Value,
    pub read_only: bool,
    /// It can remove or overwrite something. Means something only when the
    /// tool isn't read-only.
    pub destructive: bool,
    /// Calling it again with the same arguments changes nothing more.
    pub idempotent: bool,
    /// It works on a server, and what a server holds is the outside world.
    pub open_world: bool,
}

impl ToolDef {
    /// The definition with the fields the version knows: `annotations` came
    /// with 2025-03-26, a `title` of its own with 2025-06-18.
    fn shown(&self, version: &str) -> Value {
        let mut t = Map::new();
        t.insert("name".into(), json!(self.name));
        t.insert("description".into(), json!(self.description));
        t.insert("inputSchema".into(), self.input.clone());
        if version >= "2025-06-18" {
            t.insert("title".into(), json!(self.title));
        }
        if version >= "2025-03-26" {
            t.insert(
                "annotations".into(),
                json!({
                    "title": self.title,
                    "readOnlyHint": self.read_only,
                    "destructiveHint": self.destructive,
                    "idempotentHint": self.idempotent,
                    "openWorldHint": self.open_world,
                }),
            );
        }
        Value::Object(t)
    }
}

/// What the server says about itself and offers. The same for every caller.
#[derive(Clone, Debug)]
pub struct About {
    /// The app's version.
    pub version: String,
    pub instructions: String,
    pub tools: Vec<ToolDef>,
}

impl About {
    fn info(&self, with_title: bool) -> Value {
        let mut i = Map::new();
        i.insert("name".into(), json!("submarine"));
        i.insert("version".into(), json!(self.version));
        if with_title {
            i.insert("title".into(), json!("Submarine"));
        }
        Value::Object(i)
    }

    fn discover(&self) -> Map<String, Value> {
        let mut b = Map::new();
        b.insert("supportedVersions".into(), json!(versions()));
        b.insert("capabilities".into(), capabilities());
        b.insert("instructions".into(), json!(self.instructions));
        cacheable(&mut b);
        b
    }

    fn list(&self, era: Era) -> Map<String, Value> {
        let mut b = Map::new();
        b.insert("tools".into(), Value::Array(self.tools.iter().map(|t| t.shown(era.version())).collect()));
        if era == Era::Modern {
            cacheable(&mut b);
        }
        b
    }
}

/// Every version, newest first: the list `server/discover` and an
/// unsupported-version error give.
fn versions() -> Vec<&'static str> {
    std::iter::once(MODERN).chain(LEGACY).collect()
}

/// Tools, and no notice when the list changes: it never does while the app
/// runs.
fn capabilities() -> Value {
    json!({"tools": {}})
}

/// The caching hints the modern generation wants on results that can be
/// kept. `public`: nothing in them belongs to one caller.
fn cacheable(body: &mut Map<String, Value>) {
    body.insert("ttlMs".into(), json!(TTL_MS));
    body.insert("cacheScope".into(), json!("public"));
}

/// A tool call that passed the protocol's own checks: the tool exists and
/// its arguments are an object. What they hold is checked by the tool.
#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: Id,
    pub era: Era,
    pub name: String,
    pub arguments: Map<String, Value>,
}

/// What to do about one line from a client.
#[derive(Debug)]
pub enum Step {
    /// Send these lines back, in order.
    Reply(Vec<String>),
    /// Carry out a tool call, then answer it with `tool_result`, or with
    /// nothing at all when the client cancelled it first.
    Call(ToolCall),
    /// The client no longer wants the answer to this request: stop the work,
    /// and send nothing more for it.
    Cancel(Id),
    Nothing,
}

/// What one connection remembers of the protocol: whether a legacy handshake
/// happened, and the last thing the client said about itself. Grants and
/// requests are not kept here.
#[derive(Debug, Default)]
pub struct Session {
    legacy: Option<&'static str>,
    client: Option<Client>,
}

struct Refusal {
    code: i64,
    message: String,
    data: Option<Value>,
}

fn refusal(code: i64, message: impl Into<String>) -> Refusal {
    Refusal { code, message: message.into(), data: None }
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// The client's own account of itself, from the handshake or from the
    /// last request that carried one.
    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    /// One line in, what to do out. Never panics, whatever the line holds.
    pub fn handle(&mut self, line: &str, about: &About) -> Step {
        match read(line) {
            Message::Bad(None) => Step::Nothing,
            Message::Bad(Some(reply)) => Step::Reply(vec![reply]),
            Message::Notification { method, params } => notified(&method, &params),
            Message::Request { id, method, params } => self.asked(id, &method, params, about),
        }
    }

    fn asked(&mut self, id: Id, method: &str, mut params: Map<String, Value>, about: &About) -> Step {
        if method == "initialize" {
            return reply(self.initialize(&id, &params, about));
        }
        let era = match self.era(method, &params) {
            Ok(era) => era,
            Err(r) => return reply(error(Some(&id), r.code, &r.message, r.data)),
        };
        match (method, era) {
            ("ping", Era::Legacy(_)) => reply(result(&id, era, about, Map::new())),
            ("server/discover", Era::Modern) => reply(result(&id, era, about, about.discover())),
            ("tools/list", _) => {
                // The list is one page and no cursor is ever handed out, so
                // any cursor is a wrong one.
                if params.get("cursor").is_some_and(|c| !c.is_null()) {
                    return reply(error(Some(&id), INVALID_PARAMS, "Invalid cursor", None));
                }
                reply(result(&id, era, about, about.list(era)))
            }
            ("tools/call", _) => {
                let name = match params.get("name") {
                    Some(Value::String(n)) => n.clone(),
                    _ => return reply(error(Some(&id), INVALID_PARAMS, "tools/call needs the tool's name", None)),
                };
                if !about.tools.iter().any(|t| t.name == name) {
                    return reply(error(Some(&id), INVALID_PARAMS, &format!("Unknown tool: {}", cut(&name, 64)), None));
                }
                let arguments = match params.remove("arguments") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(a)) => a,
                    Some(_) => return reply(error(Some(&id), INVALID_PARAMS, "the tool's arguments must be an object", None)),
                };
                Step::Call(ToolCall { id, era, name, arguments })
            }
            // A stream of notices about changes. Nothing here changes, so the
            // stream is opened as the protocol asks, with nothing in it, and
            // nothing ever comes on it.
            ("subscriptions/listen", Era::Modern) => reply(acknowledged(&id)),
            _ => reply(error(Some(&id), METHOD_NOT_FOUND, &format!("Method not found: {}", cut(method, 64)), None)),
        }
    }

    /// The legacy handshake. The version asked for is the answer when it is
    /// one of ours; if not, our newest handshake version is, and the client
    /// decides whether to go on.
    fn initialize(&mut self, id: &Id, params: &Map<String, Value>, about: &About) -> String {
        let Some(Value::String(asked)) = params.get("protocolVersion") else {
            return error(Some(id), INVALID_PARAMS, "initialize needs protocolVersion", None);
        };
        let version = LEGACY.iter().copied().find(|v| *v == asked.as_str()).unwrap_or(LEGACY[0]);
        self.legacy = Some(version);
        if let Some(c) = params.get("clientInfo").and_then(Client::read) {
            self.client = Some(c);
        }
        let mut body = Map::new();
        body.insert("protocolVersion".into(), json!(version));
        body.insert("capabilities".into(), capabilities());
        body.insert("serverInfo".into(), about.info(version >= "2025-06-18"));
        body.insert("instructions".into(), json!(about.instructions));
        result(id, Era::Legacy(version), about, body)
    }

    /// Which generation a request belongs to.
    /// - It names the modern version in `_meta`: modern, and it must carry
    ///   the client's capabilities too.
    /// - It names nothing: legacy, if this connection shook hands. A `ping`
    ///   may come before the handshake.
    /// - It names a handshake version without having shaken hands: an error
    ///   that isn't one of the modern ones, so a client that speaks both
    ///   falls back to `initialize`, as the protocol tells it to.
    /// - It names a version we don't know: the modern error, with our list.
    fn era(&mut self, method: &str, params: &Map<String, Value>) -> Result<Era, Refusal> {
        let meta = params.get("_meta").and_then(Value::as_object);
        match meta.and_then(|m| m.get(KEY_VERSION)) {
            Some(Value::String(v)) if v == MODERN => {
                if !meta.and_then(|m| m.get(KEY_CAPABILITIES)).is_some_and(Value::is_object) {
                    return Err(refusal(INVALID_PARAMS, format!("Invalid params: _meta needs \"{KEY_CAPABILITIES}\" (an object, which may be empty)")));
                }
                if let Some(c) = meta.and_then(|m| m.get(KEY_CLIENT)).and_then(Client::read) {
                    self.client = Some(c);
                }
                Ok(Era::Modern)
            }
            Some(Value::String(v)) if LEGACY.contains(&v.as_str()) => match self.legacy {
                Some(chosen) => Ok(Era::Legacy(chosen)),
                None => Err(refusal(INVALID_REQUEST, format!("protocol version {v} starts with an initialize request"))),
            },
            Some(Value::String(v)) => Err(Refusal {
                code: UNSUPPORTED_VERSION,
                message: "Unsupported protocol version".into(),
                data: Some(json!({"supported": versions(), "requested": cut(v, 64)})),
            }),
            Some(_) => Err(refusal(INVALID_PARAMS, format!("Invalid params: \"{KEY_VERSION}\" must be a string"))),
            None => match self.legacy {
                Some(chosen) => Ok(Era::Legacy(chosen)),
                None if method == "ping" => Ok(Era::Legacy(LEGACY[0])),
                None => Err(refusal(
                    INVALID_PARAMS,
                    format!("Invalid params: the request names no protocol version. Put \"{KEY_VERSION}\" in params._meta, or start with an initialize request"),
                )),
            },
        }
    }
}

fn notified(method: &str, params: &Map<String, Value>) -> Step {
    match method {
        "notifications/cancelled" => match params.get("requestId").and_then(Id::read) {
            Some(id) => Step::Cancel(id),
            None => Step::Nothing,
        },
        // `notifications/initialized` needs nothing done, and a notification
        // is never answered, a wrong or unknown one too.
        _ => Step::Nothing,
    }
}

fn reply(line: String) -> Step {
    Step::Reply(vec![line])
}

enum Message {
    Request { id: Id, method: String, params: Map<String, Value> },
    Notification { method: String, params: Map<String, Value> },
    /// Not a message to act on, with the answer it gets, if it gets one.
    Bad(Option<String>),
}

/// One line as a message. A line that isn't one gets the JSON-RPC error for
/// it, with the id when one could be read; what looks like a notification,
/// or like a response (we never ask a client anything), gets no answer.
fn read(line: &str) -> Message {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Message::Bad(Some(error(None, PARSE_ERROR, "Parse error: the line isn't JSON", None)));
    };
    let Value::Object(mut o) = v else {
        return Message::Bad(Some(error(None, INVALID_REQUEST, "Invalid request: one JSON object a line (batches aren't supported)", None)));
    };
    let has_id = o.contains_key("id");
    let id = o.get("id").and_then(Id::read);
    let method = o.remove("method");
    if o.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        if method.is_some() && !has_id {
            return Message::Bad(None);
        }
        return Message::Bad(Some(error(id.as_ref(), INVALID_REQUEST, "Invalid request: jsonrpc must be \"2.0\"", None)));
    }
    let method = match method {
        Some(Value::String(m)) => m,
        Some(_) => return Message::Bad(Some(error(id.as_ref(), INVALID_REQUEST, "Invalid request: the method must be a string", None))),
        None if o.contains_key("result") || o.contains_key("error") => return Message::Bad(None),
        None => return Message::Bad(Some(error(id.as_ref(), INVALID_REQUEST, "Invalid request: no method", None))),
    };
    let params = match o.remove("params") {
        None => Some(Map::new()),
        Some(Value::Object(p)) => Some(p),
        Some(_) => None,
    };
    if !has_id {
        return match params {
            Some(params) => Message::Notification { method, params },
            None => Message::Bad(None),
        };
    }
    let Some(id) = id else {
        return Message::Bad(Some(error(None, INVALID_REQUEST, "Invalid request: the id must be a string or a whole number", None)));
    };
    match params {
        Some(params) => Message::Request { id, method, params },
        None => Message::Bad(Some(error(Some(&id), INVALID_PARAMS, "Invalid params: params must be an object", None))),
    }
}

/// An error answer, as one line. Without an id when none could be read.
pub fn error(id: Option<&Id>, code: i64, message: &str, data: Option<Value>) -> String {
    let mut e = Map::new();
    e.insert("code".into(), json!(code));
    e.insert("message".into(), json!(message));
    if let Some(d) = data {
        e.insert("data".into(), d);
    }
    let mut m = Map::new();
    m.insert("jsonrpc".into(), json!("2.0"));
    if let Some(id) = id {
        m.insert("id".into(), id.value());
    }
    m.insert("error".into(), Value::Object(e));
    Value::Object(m).to_string()
}

/// A result, as one line. A modern one says that it is complete and who
/// answered; a legacy one has neither field.
fn result(id: &Id, era: Era, about: &About, mut body: Map<String, Value>) -> String {
    if era == Era::Modern {
        body.insert("resultType".into(), json!("complete"));
        let mut meta = Map::new();
        meta.insert(KEY_SERVER.into(), about.info(true));
        body.insert("_meta".into(), Value::Object(meta));
    }
    let mut m = Map::new();
    m.insert("jsonrpc".into(), json!("2.0"));
    m.insert("id".into(), id.value());
    m.insert("result".into(), Value::Object(body));
    Value::Object(m).to_string()
}

/// The answer to a tool call: one piece of text. `failed` is for a call that
/// was refused, denied or went wrong: the agent reads the sentence and can
/// act on it, which it can't with a protocol error.
pub fn tool_result(id: &Id, era: Era, about: &About, text: &str, failed: bool) -> String {
    let mut body = Map::new();
    body.insert("content".into(), json!([{"type": "text", "text": text}]));
    body.insert("isError".into(), json!(failed));
    result(id, era, about, body)
}

/// The first and only message on a `subscriptions/listen` stream: the stream
/// is open, and none of the notices asked for will come.
fn acknowledged(id: &Id) -> String {
    let mut meta = Map::new();
    meta.insert(KEY_SUBSCRIPTION.into(), id.value());
    let mut params = Map::new();
    params.insert("_meta".into(), Value::Object(meta));
    params.insert("notifications".into(), json!({}));
    json!({"jsonrpc": "2.0", "method": "notifications/subscriptions/acknowledged", "params": params}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn about() -> About {
        About {
            version: "9.9.9".into(),
            instructions: "Ask first.".into(),
            tools: vec![
                ToolDef {
                    name: "status",
                    title: "Status",
                    description: "Says what is open.",
                    input: json!({"type": "object", "additionalProperties": false}),
                    read_only: true,
                    destructive: false,
                    idempotent: true,
                    open_world: false,
                },
                ToolDef {
                    name: "run_command",
                    title: "Run a command",
                    description: "Runs one command.",
                    input: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
                    read_only: false,
                    destructive: true,
                    idempotent: false,
                    open_world: true,
                },
            ],
        }
    }

    fn meta() -> Value {
        json!({KEY_VERSION: MODERN, KEY_CAPABILITIES: {}, KEY_CLIENT: {"name": "Tester", "version": "1.2"}})
    }

    /// The one line a step answers with, read back.
    fn answer(step: Step) -> Value {
        match step {
            Step::Reply(lines) => {
                assert_eq!(lines.len(), 1, "{lines:?}");
                assert!(!lines[0].contains('\n') && !lines[0].contains('\r'));
                serde_json::from_str(&lines[0]).unwrap()
            }
            other => panic!("not a reply: {other:?}"),
        }
    }

    fn ask(s: &mut Session, message: Value) -> Value {
        answer(s.handle(&message.to_string(), &about()))
    }

    #[test]
    fn a_line_that_isnt_a_message_gets_the_error_for_it() {
        let mut s = Session::new();
        let a = answer(s.handle("{\"jsonrpc\": \"2.0\", ", &about()));
        assert_eq!(a["error"]["code"], PARSE_ERROR);
        assert!(a.get("id").is_none());

        let a = answer(s.handle("[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]", &about()));
        assert_eq!(a["error"]["code"], INVALID_REQUEST);
        let a = answer(s.handle("\"hello\"", &about()));
        assert_eq!(a["error"]["code"], INVALID_REQUEST);

        let a = ask(&mut s, json!({"jsonrpc": "1.0", "id": 7, "method": "ping"}));
        assert_eq!((a["error"]["code"].as_i64(), a["id"].as_i64()), (Some(INVALID_REQUEST), Some(7)));
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 7}));
        assert_eq!((a["error"]["code"].as_i64(), a["id"].as_i64()), (Some(INVALID_REQUEST), Some(7)));
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 7, "method": 5}));
        assert_eq!(a["error"]["code"], INVALID_REQUEST);
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 7, "method": "ping", "params": [1, 2]}));
        assert_eq!((a["error"]["code"].as_i64(), a["id"].as_i64()), (Some(INVALID_PARAMS), Some(7)));
    }

    #[test]
    fn an_id_is_a_string_or_a_whole_number() {
        let mut s = Session::new();
        for bad in [json!(null), json!(1.5), json!({"a": 1}), json!([1]), json!(true), json!("x".repeat(MAX_ID + 1))] {
            let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": bad, "method": "ping"}));
            assert_eq!(a["error"]["code"], INVALID_REQUEST, "{bad}");
            assert!(a.get("id").is_none(), "{bad}");
        }
        for good in [json!(0), json!(-3), json!(u64::MAX), json!(""), json!("abc-1"), json!("1")] {
            let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": good, "method": "ping"}));
            assert_eq!(a["id"], good);
            assert!(a["result"].is_object(), "{a}");
        }
        // The number 1 and the string "1" are two ids.
        assert_ne!(Id::read(&json!(1)), Id::read(&json!("1")));
    }

    #[test]
    fn what_needs_no_answer_gets_none() {
        let mut s = Session::new();
        for quiet in [
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}),
            json!({"jsonrpc": "2.0", "method": "something/else", "params": {"a": 1}}),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": [1]}),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": null}}),
            json!({"jsonrpc": "1.0", "method": "notifications/initialized"}),
            // A response: we never asked the client anything.
            json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
            json!({"jsonrpc": "2.0", "id": 4, "error": {"code": 1, "message": "x"}}),
        ] {
            assert!(matches!(s.handle(&quiet.to_string(), &about()), Step::Nothing), "{quiet}");
        }
    }

    #[test]
    fn a_modern_client_is_served_without_a_handshake() {
        let mut s = Session::new();
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": "d1", "method": "server/discover", "params": {"_meta": meta()}}));
        let r = &a["result"];
        assert_eq!(a["id"], "d1");
        assert_eq!(r["resultType"], "complete");
        assert_eq!(r["supportedVersions"], json!(["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]));
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["instructions"], "Ask first.");
        assert_eq!((r["ttlMs"].as_u64(), r["cacheScope"].as_str()), (Some(TTL_MS), Some("public")));
        assert_eq!(r["_meta"][KEY_SERVER], json!({"name": "submarine", "title": "Submarine", "version": "9.9.9"}));
        assert_eq!(s.client(), Some(&Client { name: "Tester".into(), version: "1.2".into() }));

        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta()}}));
        let r = &a["result"];
        assert_eq!(r["resultType"], "complete");
        assert_eq!((r["ttlMs"].as_u64(), r["cacheScope"].as_str()), (Some(TTL_MS), Some("public")));
        assert!(r.get("nextCursor").is_none());
        let tools = r["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[1]["name"], "run_command");
        assert_eq!(tools[1]["title"], "Run a command");
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["command"]));
        assert_eq!(tools[1]["annotations"], json!({"title": "Run a command", "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": true}));
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);

        let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"_meta": meta(), "name": "run_command", "arguments": {"command": "df -h"}}});
        let Step::Call(c) = s.handle(&call.to_string(), &about()) else { panic!("not a call") };
        assert_eq!((c.id.clone(), c.era, c.name.as_str()), (Id::Number(3), Era::Modern, "run_command"));
        assert_eq!(c.arguments.get("command"), Some(&json!("df -h")));
        let done: Value = serde_json::from_str(&tool_result(&c.id, c.era, &about(), "Filesystem\n/dev/sda1", false)).unwrap();
        assert_eq!(done["id"], 3);
        assert_eq!(done["result"]["resultType"], "complete");
        assert_eq!(done["result"]["content"], json!([{"type": "text", "text": "Filesystem\n/dev/sda1"}]));
        assert_eq!(done["result"]["isError"], false);
        assert_eq!(done["result"]["_meta"][KEY_SERVER]["name"], "submarine");
    }

    #[test]
    fn a_modern_request_must_say_what_it_speaks() {
        let mut s = Session::new();
        // Nothing named, and no handshake before it.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": {}}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        // The version without the capabilities.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {KEY_VERSION: MODERN}}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {KEY_VERSION: MODERN, KEY_CAPABILITIES: "all"}}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {KEY_VERSION: 20260728, KEY_CAPABILITIES: {}}}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        // A version nobody knows: the modern error, with the list to pick from.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": {KEY_VERSION: "2030-01-01", KEY_CAPABILITIES: {}}}}));
        assert_eq!(a["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(a["error"]["data"]["requested"], "2030-01-01");
        assert_eq!(a["error"]["data"]["supported"][0], MODERN);
        assert_eq!(a["error"]["data"]["supported"].as_array().unwrap().len(), 5);
        // A handshake version, with no handshake: not a modern error, so a
        // client that speaks both goes on to `initialize`.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {KEY_VERSION: "2025-11-25", KEY_CAPABILITIES: {}}}}));
        assert_eq!(a["error"]["code"], INVALID_REQUEST);
        assert!(a["error"]["message"].as_str().unwrap().contains("initialize"));
        // No client info is fine.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": {KEY_VERSION: MODERN, KEY_CAPABILITIES: {}}}}));
        assert!(a["result"]["tools"].is_array());
        assert_eq!(s.client(), None);
    }

    #[test]
    fn a_legacy_client_shakes_hands_first() {
        let mut s = Session::new();
        // A ping may come before the handshake; nothing else may.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}));
        assert_eq!(a["result"], json!({}));
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 0, "method": "tools/call", "params": {"name": "status"}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);

        let a = ask(
            &mut s,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {"roots": {"listChanged": true}}, "clientInfo": {"name": "claude-code", "version": "2.1.0"}}}),
        );
        let r = &a["result"];
        assert_eq!(r["protocolVersion"], "2025-06-18");
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["serverInfo"], json!({"name": "submarine", "title": "Submarine", "version": "9.9.9"}));
        assert_eq!(r["instructions"], "Ask first.");
        assert!(r.get("resultType").is_none() && r.get("_meta").is_none());
        assert_eq!(s.client().map(|c| c.name.as_str()), Some("claude-code"));

        assert!(matches!(s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(), &about()), Step::Nothing));
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        let r = &a["result"];
        assert!(r.get("resultType").is_none() && r.get("ttlMs").is_none() && r.get("cacheScope").is_none() && r.get("_meta").is_none());
        assert_eq!(r["tools"][0]["title"], "Status");
        assert!(r["tools"][0]["annotations"].is_object());
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}));
        assert!(a["result"]["tools"].is_array());
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}));
        assert_eq!(a["result"], json!({}));

        let call = json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call", "params": {"name": "status", "_meta": {"progressToken": 5}}});
        let Step::Call(c) = s.handle(&call.to_string(), &about()) else { panic!("not a call") };
        assert_eq!((c.era, c.arguments.len()), (Era::Legacy("2025-06-18"), 0));
        let done: Value = serde_json::from_str(&tool_result(&c.id, c.era, &about(), "the user denied this", true)).unwrap();
        assert_eq!(done["result"], json!({"content": [{"type": "text", "text": "the user denied this"}], "isError": true}));
    }

    #[test]
    fn a_handshake_for_a_version_we_dont_have_gets_our_newest() {
        for asked in ["2023-01-01", "2026-07-28", "", "2025-11-26"] {
            let mut s = Session::new();
            let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": asked, "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}}));
            assert_eq!(a["result"]["protocolVersion"], "2025-11-25", "{asked}");
        }
        let mut s = Session::new();
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        // The failed handshake left nothing behind.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn an_old_version_gets_only_the_fields_it_knows() {
        let hello = |v: &str| json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": v, "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}});
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});

        let mut s = Session::new();
        let a = ask(&mut s, hello("2024-11-05"));
        assert_eq!(a["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(a["result"]["serverInfo"], json!({"name": "submarine", "version": "9.9.9"}));
        let a = ask(&mut s, list.clone());
        let t = a["result"]["tools"][1].as_object().unwrap();
        let mut keys: Vec<&str> = t.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["description", "inputSchema", "name"]);

        let mut s = Session::new();
        ask(&mut s, hello("2025-03-26"));
        let a = ask(&mut s, list.clone());
        let t = a["result"]["tools"][1].as_object().unwrap();
        assert!(t.contains_key("annotations") && !t.contains_key("title"));

        let mut s = Session::new();
        ask(&mut s, hello("2025-11-25"));
        let a = ask(&mut s, list);
        let t = a["result"]["tools"][1].as_object().unwrap();
        assert!(t.contains_key("annotations") && t.contains_key("title"));
    }

    #[test]
    fn a_call_is_checked_before_it_is_handed_on() {
        let mut s = Session::new();
        let call = |params: Value| {
            let mut p = params;
            p["_meta"] = meta();
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": p})
        };
        let a = ask(&mut s, call(json!({"name": "format_disk", "arguments": {}})));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        assert_eq!(a["error"]["message"], "Unknown tool: format_disk");
        let a = ask(&mut s, call(json!({"arguments": {}})));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, call(json!({"name": 5})));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, call(json!({"name": "status", "arguments": "none"})));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        let a = ask(&mut s, call(json!({"name": "status", "arguments": [1]})));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
        // A long name is cut in the answer.
        let a = ask(&mut s, call(json!({"name": "x".repeat(5000)})));
        assert!(a["error"]["message"].as_str().unwrap().len() < 100);
        // No arguments, or null: an empty object.
        for p in [json!({"name": "status"}), json!({"name": "status", "arguments": null})] {
            let Step::Call(c) = s.handle(&call(p).to_string(), &about()) else { panic!("not a call") };
            assert!(c.arguments.is_empty());
        }
    }

    #[test]
    fn a_cancel_names_the_request_and_is_never_answered() {
        let mut s = Session::new();
        let c = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "abc", "reason": "the user pressed stop"}});
        assert!(matches!(s.handle(&c.to_string(), &about()), Step::Cancel(Id::Text(t)) if t == "abc"));
        let c = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 12}});
        assert!(matches!(s.handle(&c.to_string(), &about()), Step::Cancel(Id::Number(12))));
    }

    #[test]
    fn both_generations_can_share_a_connection() {
        let mut s = Session::new();
        ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "old", "version": "1"}}}));
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta()}}));
        assert_eq!(a["result"]["resultType"], "complete");
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
        assert!(a["result"].get("resultType").is_none());
        // After a handshake, a request that names the handshake's version in
        // `_meta` is a legacy one.
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {"_meta": {KEY_VERSION: "2025-06-18"}}}));
        assert!(a["result"]["tools"].is_array() && a["result"].get("resultType").is_none());
        // The last account the client gave of itself is the one kept.
        assert_eq!(s.client().map(|c| c.name.as_str()), Some("Tester"));
    }

    #[test]
    fn what_isnt_offered_is_not_found() {
        let mut s = Session::new();
        for method in ["resources/list", "resources/read", "prompts/list", "prompts/get", "completion/complete", "ping", "logging/setLevel", "tools/run", "x".repeat(4000).as_str()] {
            let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {"_meta": meta()}}));
            assert_eq!(a["error"]["code"], METHOD_NOT_FOUND, "{method}");
            assert!(a["error"]["message"].as_str().unwrap().len() < 100);
        }
        ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "old", "version": "1"}}}));
        for method in ["resources/list", "prompts/list", "logging/setLevel", "server/discover", "subscriptions/listen", "resources/subscribe"] {
            let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": method}));
            assert_eq!(a["error"]["code"], METHOD_NOT_FOUND, "{method}");
        }
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"cursor": "page-2"}}));
        assert_eq!(a["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn a_stream_of_notices_opens_with_nothing_in_it() {
        let mut s = Session::new();
        let a = ask(&mut s, json!({"jsonrpc": "2.0", "id": 11, "method": "subscriptions/listen", "params": {"_meta": meta(), "notifications": {"toolsListChanged": true}}}));
        assert_eq!(a["method"], "notifications/subscriptions/acknowledged");
        assert!(a.get("id").is_none());
        assert_eq!(a["params"]["_meta"][KEY_SUBSCRIPTION], 11);
        assert_eq!(a["params"]["notifications"], json!({}));
    }

    #[test]
    fn what_a_client_says_about_itself_is_kept_short() {
        let mut s = Session::new();
        ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "n".repeat(900), "version": "v".repeat(900)}}}));
        let c = s.client().unwrap();
        assert_eq!((c.name.len(), c.version.len()), (MAX_CLIENT_NAME, MAX_CLIENT_VERSION));
        // A name that isn't a string is no name.
        let mut s = Session::new();
        ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": {"a": 1}}}}));
        assert_eq!(s.client(), None);
    }

    #[test]
    fn every_line_out_is_one_line() {
        let line = tool_result(&Id::Text("a\nb".into()), Era::Modern, &about(), "one\r\ntwo\u{2028}three\n", false);
        assert!(!line.contains('\n') && !line.contains('\r'));
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["id"], "a\nb");
        assert_eq!(back["result"]["content"][0]["text"], "one\r\ntwo\u{2028}three\n");
        let line = error(Some(&Id::Number(1)), INTERNAL_ERROR, "line one\nline two", Some(json!({"why": "x\ny"})));
        assert!(!line.contains('\n'));
    }
}
