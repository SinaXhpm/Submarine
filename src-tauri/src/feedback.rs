//! Messages from "Send feedback" (Settings → Help & feedback, the profile
//! list's footer, Settings → Language → Suggest a fix): what the person typed,
//! a message and, if they gave them, a name and an email, sent to the
//! developer's API. Nothing else goes: no logs, no app version, no system
//! facts, no account token, no device id. The user agent is plain
//! `submarine-app`, without the version the cloud client normally sends.
//!
//! Always to Submarine's own API (`CLOUD_API_ROOT`), also when cloud sync uses
//! a server of the person's own: the message is for the app's developer. The
//! cloud proxy setting is used, as for sync.
//!
//! Lengths are counted in Unicode code points, as the form counts them
//! (`Array.from(text).length`) and the server does (`mb_strlen`): a Chinese
//! character or an emoji is one, as a letter is. Nothing the person wrote is
//! cut; a text over a limit is refused before anything is sent.

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, RETRY_AFTER, USER_AGENT};
use serde::{Deserialize, Serialize};

use crate::cloud::CLOUD_API_ROOT;
use crate::cloud_config::{self, ProxyConfig};

pub const MESSAGE_MIN: usize = 5;
pub const MESSAGE_MAX: usize = 4000;
pub const NAME_MAX: usize = 64;
pub const EMAIL_MAX: usize = 254;
/// The server refuses bodies over 32 KB. The largest valid message (4,000
/// four-byte characters, or 4,000 quotes escaped to two bytes each) stays far
/// under it; this is the check that it does.
const BODY_MAX: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Deserialize)]
pub struct FeedbackInput {
    pub message: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// What became of a message. Every way it can go is an answer the form
/// shows, not an error string to pick apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FeedbackOutcome {
    /// Stored; `id` is the report id shown to the person ("R-7K3QX9M2").
    Sent { id: String },
    /// A field is empty where it can't be, too long, or not an email.
    BadField { field: String },
    /// Too many messages from this network for now.
    RateLimited { retry_after: Option<u64> },
    /// The server takes no more messages today.
    InboxFull,
    /// Something on the way (a firewall, Cloudflare) answered instead of the
    /// API, with a web page.
    Blocked,
    /// The server couldn't be reached.
    Offline,
    /// Any other answer from the server.
    Server { code: u16 },
}

/// The same cleaning the server does: line breaks as `\n`, control
/// characters other than line breaks and tabs left out, no blanks at either
/// end. `single_line` turns line breaks and tabs into spaces (the name).
pub fn clean(text: &str, single_line: bool) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let kept: String = unified
        .chars()
        .filter_map(|c| match c {
            '\n' | '\t' if single_line => Some(' '),
            '\n' | '\t' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    // U+FEFF too, as PHP's and JavaScript's trims take it off.
    kept.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').to_string()
}

fn email_shaped(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else { return false };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The body to send, or the field that keeps it from being sent.
pub fn build_body(input: &FeedbackInput) -> Result<serde_json::Value, String> {
    let message = clean(&input.message, false);
    let n = message.chars().count();
    if !(MESSAGE_MIN..=MESSAGE_MAX).contains(&n) {
        return Err("message".into());
    }
    let name = clean(input.name.as_deref().unwrap_or(""), true);
    if name.chars().count() > NAME_MAX {
        return Err("name".into());
    }
    let email = input.email.as_deref().unwrap_or("").trim().to_string();
    // An address's limit is in bytes (RFC 5321), as the server counts it.
    if !email.is_empty() && (email.len() > EMAIL_MAX || !email_shaped(&email)) {
        return Err("email".into());
    }
    let opt = |s: String| if s.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(s) };
    Ok(serde_json::json!({ "v": 1, "message": message, "name": opt(name), "email": opt(email) }))
}

/// What an answer means. Kept apart from the request so it can be tested.
pub fn outcome(status: u16, content_type: &str, retry_after_header: Option<&str>, body: &[u8]) -> FeedbackOutcome {
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let field = |k: &str| json.as_ref().and_then(|j| j.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    let is_html = content_type.to_ascii_lowercase().starts_with("text/html");
    match status {
        200 | 201 => match field("id") {
            Some(id) => FeedbackOutcome::Sent { id },
            None => FeedbackOutcome::Server { code: status },
        },
        400 => match field("field") {
            Some(f) => FeedbackOutcome::BadField { field: f },
            None => FeedbackOutcome::Server { code: status },
        },
        413 => FeedbackOutcome::BadField { field: "message".into() },
        429 => {
            let from_body = json.as_ref().and_then(|j| j.get("retry_after")).and_then(|v| v.as_u64());
            let from_header = retry_after_header.and_then(|h| h.trim().parse::<u64>().ok());
            FeedbackOutcome::RateLimited { retry_after: from_body.or(from_header) }
        }
        503 if field("error").as_deref() == Some("inbox_full") => FeedbackOutcome::InboxFull,
        _ if is_html && json.is_none() => FeedbackOutcome::Blocked,
        _ => FeedbackOutcome::Server { code: status },
    }
}

fn endpoint() -> String {
    // A debug build (the lab copies) can send to the staging API instead.
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("SUBMARINE_LAB_FEEDBACK_URL") {
        if !url.trim().is_empty() {
            return url.trim().to_string();
        }
    }
    format!("{}/feedback", CLOUD_API_ROOT)
}

#[tauri::command]
pub async fn send_feedback(app: tauri::AppHandle, input: FeedbackInput) -> Result<FeedbackOutcome, String> {
    let body = match build_body(&input) {
        Ok(b) => serde_json::to_vec(&b).map_err(|e| format!("[FEEDBACK] {}", e))?,
        Err(field) => return Ok(FeedbackOutcome::BadField { field }),
    };
    if body.len() > BODY_MAX {
        return Ok(FeedbackOutcome::BadField { field: "message".into() });
    }
    let url = endpoint();
    let (config, _) = cloud_config::load(&app);
    // A saved proxy that can't be used falls back to the system's, as sync does.
    let http = cloud_config::build_client(&url, &config.proxy, TIMEOUT)
        .or_else(|_| cloud_config::build_client(&url, &ProxyConfig::default(), TIMEOUT))?;
    let res = match http
        .post(&url)
        .header(USER_AGENT, "submarine-app")
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Ok(FeedbackOutcome::Offline),
    };
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let retry = res.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = match res.bytes().await {
        Ok(b) => b,
        Err(_) => return Ok(FeedbackOutcome::Offline),
    };
    Ok(outcome(status, &content_type, retry.as_deref(), &bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(message: &str, name: Option<&str>, email: Option<&str>) -> FeedbackInput {
        FeedbackInput { message: message.into(), name: name.map(Into::into), email: email.map(Into::into) }
    }

    #[test]
    fn cleaning_matches_the_server() {
        assert_eq!(clean("  a\r\nb\rc\u{0}\u{7}\u{1b}[31m\u{9f}d\te  ", false), "a\nb\nc[31md\te");
        assert_eq!(clean(" Ana\nMaría\t ", true), "Ana María");
        assert_eq!(clean("\n\n", false), "");
        assert_eq!(clean("\u{feff} hi \u{3000}", false), "hi");
    }

    #[test]
    fn message_limits_count_code_points() {
        // 4,000 of a three-byte character, of a four-byte one, of a letter.
        for ch in ["界", "😀", "a"] {
            assert!(build_body(&input(&ch.repeat(MESSAGE_MAX), None, None)).is_ok(), "{ch} at the limit");
            assert_eq!(build_body(&input(&ch.repeat(MESSAGE_MAX + 1), None, None)), Err("message".into()), "{ch} over");
        }
        assert!(build_body(&input("abcde", None, None)).is_ok());
        assert_eq!(build_body(&input("abcd", None, None)), Err("message".into()));
        // Blanks and control characters don't count towards the minimum.
        assert_eq!(build_body(&input("  ab\u{0}\u{0}c  ", None, None)), Err("message".into()));
    }

    #[test]
    fn the_largest_valid_body_fits() {
        for ch in ["😀", "\"", "\\"] {
            let body = serde_json::to_vec(&build_body(&input(&ch.repeat(MESSAGE_MAX), Some(&"名".repeat(NAME_MAX)), Some(&format!("{}@example.com", "a".repeat(240))))).unwrap()).unwrap();
            assert!(body.len() <= BODY_MAX, "{ch}: {} bytes", body.len());
        }
    }

    #[test]
    fn name_and_email_are_optional_and_checked() {
        let ok = build_body(&input("hello there", Some("  "), Some(""))).unwrap();
        assert_eq!(ok["name"], serde_json::Value::Null);
        assert_eq!(ok["email"], serde_json::Value::Null);
        assert_eq!(ok["v"], 1);
        assert!(build_body(&input("hello there", Some(&"é".repeat(NAME_MAX)), None)).is_ok());
        assert_eq!(build_body(&input("hello there", Some(&"é".repeat(NAME_MAX + 1)), None)), Err("name".into()));
        for bad in ["a", "a@b", "@b.c", "a@.c", "a@b.", "a b@c.d", "a@b c.d"] {
            assert_eq!(build_body(&input("hello there", None, Some(bad))), Err("email".into()), "{bad}");
        }
        let long_local = format!("{}@example.com", "a".repeat(EMAIL_MAX - 12 + 1));
        assert_eq!(build_body(&input("hello there", None, Some(&long_local))), Err("email".into()));
        assert!(build_body(&input("hello there", Some("Ana"), Some(" ana@example.com "))).is_ok());
        // 254 bytes, not characters: a two-byte letter counts twice.
        let wide = format!("{}@example.com", "é".repeat(121)); // 242 + 12 = 254 bytes
        assert!(build_body(&input("hello there", None, Some(&wide))).is_ok());
        let wider = format!("{}@example.com", "é".repeat(122)); // 256 bytes, 134 characters
        assert_eq!(build_body(&input("hello there", None, Some(&wider))), Err("email".into()));
    }

    #[test]
    fn answers_map_to_outcomes() {
        assert_eq!(outcome(201, "application/json", None, br#"{"id":"R-7K3QX9M2"}"#), FeedbackOutcome::Sent { id: "R-7K3QX9M2".into() });
        assert_eq!(outcome(200, "application/json", None, br#"{"id":"R-AAAAAAAA"}"#), FeedbackOutcome::Sent { id: "R-AAAAAAAA".into() });
        assert_eq!(outcome(400, "application/json", None, br#"{"error":"bad_field","field":"email"}"#), FeedbackOutcome::BadField { field: "email".into() });
        assert_eq!(outcome(413, "application/json", None, b"{}"), FeedbackOutcome::BadField { field: "message".into() });
        assert_eq!(outcome(429, "application/json", Some("3600"), br#"{"error":"rate_limited","retry_after":1800}"#), FeedbackOutcome::RateLimited { retry_after: Some(1800) });
        assert_eq!(outcome(429, "application/json", Some("3600"), br#"{"error":"rate_limited"}"#), FeedbackOutcome::RateLimited { retry_after: Some(3600) });
        assert_eq!(outcome(503, "application/json", None, br#"{"error":"inbox_full"}"#), FeedbackOutcome::InboxFull);
        assert_eq!(outcome(503, "text/html", None, b"<html>busy</html>"), FeedbackOutcome::Blocked);
        assert_eq!(outcome(403, "text/html; charset=UTF-8", None, b"<!DOCTYPE html><title>Attention Required</title>"), FeedbackOutcome::Blocked);
        assert_eq!(outcome(500, "application/json", None, br#"{"error":"server"}"#), FeedbackOutcome::Server { code: 500 });
        assert_eq!(outcome(201, "application/json", None, b"not json"), FeedbackOutcome::Server { code: 201 });
    }
}
