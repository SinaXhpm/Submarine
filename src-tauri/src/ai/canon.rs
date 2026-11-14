//! The provider-neutral copy of a chat's turns.
//!
//! Every turn is stored twice: exactly as the provider that
//! produced it sent it (its wire copy, replayed to that provider unchanged), and
//! in this neutral form. When a chat moves to another provider, the turns that
//! provider never saw are rendered from the neutral form, and each rendering is
//! stored the first time it is used, so later requests send the same bytes. An
//! Anthropic thinking block is checked against everything before it, and a
//! prompt cache matches only on identical bytes: neither survives a history
//! whose earlier turns come out differently from one request to the next.

use super::provider::profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    /// What the provider showed of its reasoning (a summary, or nothing). Never
    /// sent to another provider: reasoning only means something to the model
    /// that wrote it, and only in the provider's own wire copy.
    Reasoning { text: String },
    ToolCall { id: String, name: String, input: Value },
    ToolResult {
        id: String,
        text: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Turn {
    pub fn user_text(text: &str) -> Self {
        Turn { role: Role::User, blocks: vec![Block::Text { text: text.to_string() }] }
    }

    /// The turn's visible text, the text blocks joined.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n\n")
    }
}

/// Where a request goes, as far as the shape of its history is concerned. Two
/// providers of the same target take each other's wire copies as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Anthropic,
    OpenAi,
    Compat(Profile),
}

impl Target {
    pub fn key(&self) -> String {
        match self {
            Target::Anthropic => "anthropic".into(),
            Target::OpenAi => "openai".into(),
            Target::Compat(p) => format!("compat:{}", p.as_str()),
        }
    }
}

/// A tool call id Anthropic takes (`[a-zA-Z0-9_-]{1,64}`). Others are replaced
/// by a hash of themselves: the same id always gives the same replacement, so
/// a call and its result still match, and nothing needs storing.
pub fn anthropic_id(id: &str) -> String {
    let ok = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        id.to_string()
    } else {
        format!("t_{}", &hex::encode(Sha256::digest(id.as_bytes()))[..24])
    }
}

/// The entries (messages, or Responses API items) that carry `turn` to
/// `target`. Used for turns that target has no wire copy of: our own user
/// turns, and turns another provider produced.
pub fn render(turn: &Turn, target: Target) -> Vec<Value> {
    match target {
        Target::Anthropic => render_anthropic(turn),
        Target::OpenAi => render_openai(turn),
        Target::Compat(profile) => render_compat(turn, profile),
    }
}

fn render_anthropic(turn: &Turn) -> Vec<Value> {
    let mut content = Vec::new();
    match turn.role {
        Role::User => {
            // Anthropic wants the tool results first in the message.
            for b in &turn.blocks {
                if let Block::ToolResult { id, text, is_error } = b {
                    let mut r = Map::new();
                    r.insert("type".into(), json!("tool_result"));
                    r.insert("tool_use_id".into(), json!(anthropic_id(id)));
                    r.insert("content".into(), json!(text));
                    if *is_error {
                        r.insert("is_error".into(), json!(true));
                    }
                    content.push(Value::Object(r));
                }
            }
            for b in &turn.blocks {
                if let Block::Text { text } = b {
                    content.push(json!({"type": "text", "text": text}));
                }
            }
        }
        Role::Assistant => {
            for b in &turn.blocks {
                match b {
                    Block::Text { text } if !text.trim().is_empty() => content.push(json!({"type": "text", "text": text})),
                    Block::ToolCall { id, name, input } => {
                        let input = if input.is_object() { input.clone() } else { json!({}) };
                        content.push(json!({"type": "tool_use", "id": anthropic_id(id), "name": name, "input": input}));
                    }
                    _ => {}
                }
            }
            if content.is_empty() {
                // Only a final assistant message may be empty.
                content.push(json!({"type": "text", "text": "(no reply)"}));
            }
        }
    }
    let role = match turn.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    vec![json!({"role": role, "content": content})]
}

fn render_openai(turn: &Turn) -> Vec<Value> {
    let mut items = Vec::new();
    match turn.role {
        Role::User => {
            for b in &turn.blocks {
                if let Block::ToolResult { id, text, is_error } = b {
                    items.push(json!({"type": "function_call_output", "call_id": id, "output": result_text(text, *is_error)}));
                }
            }
            let parts: Vec<Value> = turn
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(json!({"type": "input_text", "text": text})),
                    _ => None,
                })
                .collect();
            if !parts.is_empty() {
                items.push(json!({"type": "message", "role": "user", "content": parts}));
            }
        }
        Role::Assistant => {
            let text = turn.text();
            if !text.trim().is_empty() {
                items.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}));
            }
            for b in &turn.blocks {
                if let Block::ToolCall { id, name, input } = b {
                    items.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args_string(input)}));
                }
            }
        }
    }
    items
}

fn render_compat(turn: &Turn, profile: Profile) -> Vec<Value> {
    let mut out = Vec::new();
    match turn.role {
        Role::User => {
            for b in &turn.blocks {
                if let Block::ToolResult { id, text, is_error } = b {
                    out.push(json!({"role": "tool", "tool_call_id": id, "content": result_text(text, *is_error)}));
                }
            }
            let text = turn.text();
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text}));
            }
        }
        Role::Assistant => {
            let text = turn.text();
            let calls: Vec<Value> = turn
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::ToolCall { id, name, input } => {
                        Some(json!({"id": id, "type": "function", "function": {"name": name, "arguments": args_string(input)}}))
                    }
                    _ => None,
                })
                .collect();
            let mut m = Map::new();
            m.insert("role".into(), json!("assistant"));
            m.insert("content".into(), if text.is_empty() { Value::Null } else { json!(text) });
            if !calls.is_empty() {
                m.insert("tool_calls".into(), Value::Array(calls));
            }
            // DeepSeek in thinking mode refuses a history whose assistant turns
            // that called tools carry no reasoning_content; a turn another model
            // wrote has none, so it gets an empty one.
            if profile == Profile::DeepSeek {
                m.insert("reasoning_content".into(), json!(""));
            }
            out.push(Value::Object(m));
        }
    }
    out
}

/// A failed result for a kind with no error flag: it says so in its text.
fn result_text(text: &str, is_error: bool) -> String {
    if is_error {
        format!("Error: {text}")
    } else {
        text.to_string()
    }
}

fn args_string(input: &Value) -> String {
    if input.is_null() {
        "{}".into()
    } else {
        serde_json::to_string(input).unwrap_or_else(|_| "{}".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange() -> (Turn, Turn) {
        let assistant = Turn {
            role: Role::Assistant,
            blocks: vec![
                Block::Reasoning { text: "thinking about it".into() },
                Block::Text { text: "Let me look.".into() },
                Block::ToolCall { id: "call.9/x".into(), name: "read_file".into(), input: json!({"path": "/etc/hosts"}) },
            ],
        };
        let results = Turn {
            role: Role::User,
            blocks: vec![
                Block::Text { text: "level changed to Ask".into() },
                Block::ToolResult { id: "call.9/x".into(), text: "127.0.0.1 localhost".into(), is_error: false },
            ],
        };
        (assistant, results)
    }

    #[test]
    fn anthropic_gets_safe_ids_results_first_and_no_reasoning() {
        let (a, r) = exchange();
        let a = &render(&a, Target::Anthropic)[0];
        let r = &render(&r, Target::Anthropic)[0];
        let id = a["content"][1]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("t_") && id.len() == 26, "{id}");
        assert_eq!(a["content"].as_array().unwrap().len(), 2, "reasoning dropped");
        assert_eq!(r["content"][0]["type"], "tool_result");
        assert_eq!(r["content"][0]["tool_use_id"], id.as_str(), "call and result still match");
        assert!(r["content"][0].get("is_error").is_none());
        assert_eq!(r["content"][1]["type"], "text");
        assert_eq!(anthropic_id("toolu_01ABC-x"), "toolu_01ABC-x");
    }

    #[test]
    fn openai_items_and_compat_messages() {
        let (a, r) = exchange();
        let items = render(&a, Target::OpenAi);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call.9/x");
        assert_eq!(items[1]["arguments"], "{\"path\":\"/etc/hosts\"}");
        let items = render(&r, Target::OpenAi);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[1]["content"][0]["type"], "input_text");

        let m = render(&a, Target::Compat(Profile::OpenRouter));
        assert_eq!(m[0]["tool_calls"][0]["function"]["name"], "read_file");
        assert!(m[0].get("reasoning_content").is_none());
        let m = render(&a, Target::Compat(Profile::DeepSeek));
        assert_eq!(m[0]["reasoning_content"], "");
        let m = render(&r, Target::Compat(Profile::Custom));
        assert_eq!(m[0]["role"], "tool");
        let failed = Turn { role: Role::User, blocks: vec![Block::ToolResult { id: "c".into(), text: "denied by the user".into(), is_error: true }] };
        assert_eq!(render(&failed, Target::Compat(Profile::Custom))[0]["content"], "Error: denied by the user");
        assert_eq!(render(&failed, Target::OpenAi)[0]["output"], "Error: denied by the user");
        assert_eq!(render(&failed, Target::Anthropic)[0]["content"][0]["is_error"], true);
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn rendering_is_byte_stable() {
        let (a, r) = exchange();
        for t in [Target::Anthropic, Target::OpenAi, Target::Compat(Profile::Gemini)] {
            let one = serde_json::to_string(&render(&a, t)).unwrap() + &serde_json::to_string(&render(&r, t)).unwrap();
            let two = serde_json::to_string(&render(&a, t)).unwrap() + &serde_json::to_string(&render(&r, t)).unwrap();
            assert_eq!(one, two);
        }
    }

    #[test]
    fn an_empty_assistant_turn_still_has_content_for_anthropic() {
        let t = Turn { role: Role::Assistant, blocks: vec![Block::Reasoning { text: "x".into() }] };
        assert_eq!(render(&t, Target::Anthropic)[0]["content"][0]["text"], "(no reply)");
    }
}
