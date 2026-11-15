//! What differs between the servers that speak OpenAI's Chat Completions.
//! They share the request and the stream, and part ways on the
//! details that turn a working chat into a 400: which field caps the output,
//! which reasoning field must go back with each assistant turn, which request
//! fields a server takes at all. The profile comes from the preset the user
//! picked and stays when the address is edited (Ollama on a LAN address is
//! still Ollama).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    OpenRouter,
    DeepSeek,
    Groq,
    Gemini,
    Ollama,
    LmStudio,
    /// OpenAI itself on Chat Completions: only when the user points this kind
    /// at api.openai.com; the OpenAI kind (Responses API) is the right one.
    OpenAiChat,
    Custom,
}

/// How a profile takes the chat's effort setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    /// Not offered.
    None,
    /// `reasoning_effort: <level>`.
    ReasoningEffort,
    /// OpenRouter's own `reasoning: {effort}`.
    OpenRouter,
    /// DeepSeek: thinking on (its default) or off (`thinking: {type: "disabled"}`).
    DeepSeekToggle,
}

impl Profile {
    pub const ALL: [Profile; 8] = [
        Profile::OpenRouter,
        Profile::DeepSeek,
        Profile::Groq,
        Profile::Gemini,
        Profile::Ollama,
        Profile::LmStudio,
        Profile::OpenAiChat,
        Profile::Custom,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::OpenRouter => "open_router",
            Profile::DeepSeek => "deep_seek",
            Profile::Groq => "groq",
            Profile::Gemini => "gemini",
            Profile::Ollama => "ollama",
            Profile::LmStudio => "lm_studio",
            Profile::OpenAiChat => "open_ai_chat",
            Profile::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Profile> {
        Profile::ALL.iter().copied().find(|p| p.as_str() == s)
    }

    /// The field that caps the reply's length.
    pub fn max_tokens_field(&self) -> &'static str {
        match self {
            Profile::Groq | Profile::Gemini | Profile::OpenAiChat => "max_completion_tokens",
            _ => "max_tokens",
        }
    }

    /// OpenRouter puts the usage on the final chunk by itself and ignores the
    /// option; everyone else needs it asked for.
    pub fn sends_stream_options(&self) -> bool {
        !matches!(self, Profile::OpenRouter)
    }

    /// Ollama doesn't take `tool_choice`.
    pub fn sends_tool_choice(&self) -> bool {
        !matches!(self, Profile::Ollama)
    }

    /// Fields of an assistant message, besides `role`, `content` and
    /// `tool_calls`, that go back with it on the next request. A missing one is
    /// a 400 on DeepSeek (thinking mode with tools) and on Gemini 3 (the thought
    /// signature, carried on the tool calls themselves and sometimes on the
    /// message); others take them back to keep their reasoning. Everything else
    /// the server sent stays in the stored copy and isn't sent.
    pub fn echo_fields(&self) -> &'static [&'static str] {
        match self {
            Profile::OpenRouter => &["reasoning", "reasoning_details"],
            Profile::DeepSeek => &["reasoning_content"],
            Profile::Groq => &["reasoning"],
            Profile::Gemini => &["extra_content"],
            Profile::Ollama => &["reasoning"],
            Profile::LmStudio => &["reasoning_content", "reasoning"],
            Profile::OpenAiChat | Profile::Custom => &[],
        }
    }

    pub fn effort(&self) -> Effort {
        match self {
            Profile::OpenRouter => Effort::OpenRouter,
            Profile::DeepSeek => Effort::DeepSeekToggle,
            Profile::Groq | Profile::Gemini | Profile::Ollama | Profile::LmStudio => Effort::ReasoningEffort,
            Profile::OpenAiChat | Profile::Custom => Effort::None,
        }
    }

    /// The effort levels the chat header offers for this profile.
    pub fn effort_levels(&self) -> Vec<&'static str> {
        match self.effort() {
            Effort::None => vec![],
            Effort::DeepSeekToggle => vec!["none", "high"],
            Effort::OpenRouter | Effort::ReasoningEffort => vec!["low", "medium", "high"],
        }
    }

    /// A server on the user's own machine or network, which keeps nothing.
    pub fn local(&self) -> bool {
        matches!(self, Profile::Ollama | Profile::LmStudio)
    }

    /// The context a model gets there by default. Ollama gives 4k on machines
    /// with less than 24 GB of graphics memory, and silently drops the start of
    /// a chat that doesn't fit; LM Studio's is set when the model is loaded.
    pub fn default_context(&self) -> u32 {
        if self.local() {
            4096
        } else {
            128_000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for p in Profile::ALL {
            assert_eq!(Profile::parse(p.as_str()), Some(p));
            assert_eq!(serde_json::to_value(p).unwrap(), serde_json::json!(p.as_str()));
        }
        assert_eq!(Profile::parse("nope"), None);
    }

    #[test]
    fn the_researched_differences() {
        assert_eq!(Profile::DeepSeek.max_tokens_field(), "max_tokens");
        assert_eq!(Profile::Groq.max_tokens_field(), "max_completion_tokens");
        assert!(Profile::DeepSeek.echo_fields().contains(&"reasoning_content"));
        assert!(Profile::Gemini.echo_fields().contains(&"extra_content"));
        assert!(!Profile::Ollama.sends_tool_choice());
        assert!(!Profile::OpenRouter.sends_stream_options());
        assert_eq!(Profile::Ollama.default_context(), 4096);
    }
}
