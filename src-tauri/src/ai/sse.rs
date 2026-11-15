//! Server-sent events, framed from the chunks reqwest hands over.
//!
//! Every provider streams its reply this way (Anthropic, OpenAI's Responses
//! API, the Chat Completions servers). A chunk can end anywhere: inside a line,
//! inside a UTF-8 character, between the two halves of a `\r\n`. Lines are only
//! split on `\n`, so a character is never cut, and each line is decoded whole.
//!
//! The reply comes from a server the user picked, through proxies we don't
//! know, so nothing here trusts its size: an event and a whole reply have caps,
//! and going past either is an error rather than an allocation.

/// The most one event's data may hold.
pub const MAX_EVENT: usize = 1024 * 1024;
/// The most one streamed reply may hold, all events together.
pub const MAX_RESPONSE: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` line, when the server sent one (the Responses API and
    /// Anthropic do; Chat Completions servers don't).
    pub event: Option<String>,
    /// The `data:` lines, joined with `\n`.
    pub data: String,
}

#[derive(Default)]
pub struct Framer {
    /// The start of a line whose `\n` hasn't arrived yet.
    partial: Vec<u8>,
    data: String,
    has_data: bool,
    event: Option<String>,
    total: usize,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the next chunk and returns the events it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Event>, String> {
        self.total += chunk.len();
        if self.total > MAX_RESPONSE {
            return Err("[AI] REPLY_TOO_LARGE: the provider's reply passed 32 MiB".into());
        }
        let mut out = Vec::new();
        let mut rest = chunk;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            let (line, after) = rest.split_at(i);
            rest = &after[1..];
            if self.partial.is_empty() {
                self.line(line, &mut out)?;
            } else {
                let mut whole = std::mem::take(&mut self.partial);
                whole.extend_from_slice(line);
                self.line(&whole, &mut out)?;
            }
        }
        if !rest.is_empty() {
            if self.partial.len() + rest.len() > MAX_EVENT {
                return Err("[AI] EVENT_TOO_LARGE: one line of the provider's reply passed 1 MiB".into());
            }
            self.partial.extend_from_slice(rest);
        }
        Ok(out)
    }

    /// The stream ended: an event the server didn't close with a blank line
    /// still counts.
    pub fn finish(&mut self) -> Result<Option<Event>, String> {
        let mut out = Vec::new();
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.line(&line, &mut out)?;
        }
        self.dispatch(&mut out);
        Ok(out.pop())
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<Event>) -> Result<(), String> {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        let line = std::str::from_utf8(raw).map_err(|_| "[AI] BAD_REPLY: the provider sent text that isn't UTF-8".to_string())?;
        if line.starts_with(':') {
            return Ok(()); // a comment, often a keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                if self.data.len() > MAX_EVENT {
                    return Err("[AI] EVENT_TOO_LARGE: one event of the provider's reply passed 1 MiB".into());
                }
            }
            "event" => self.event = Some(value.to_string()),
            _ => {} // id, retry, and anything else: not used
        }
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<Event>) {
        if self.has_data || self.event.is_some() {
            out.push(Event { event: self.event.take(), data: std::mem::take(&mut self.data) });
        }
        self.has_data = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut f = Framer::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(f.push(c).unwrap());
        }
        out.extend(f.finish().unwrap());
        out
    }

    #[test]
    fn events_split_anywhere_come_out_whole() {
        let text = "event: message_start\ndata: {\"a\":1}\n\ndata: {\"b\":\"é\"}\n\n";
        let bytes = text.as_bytes();
        for cut in 1..bytes.len() {
            let got = all(&[&bytes[..cut], &bytes[cut..]]);
            assert_eq!(got.len(), 2, "cut at {cut}");
            assert_eq!(got[0], Event { event: Some("message_start".into()), data: "{\"a\":1}".into() });
            assert_eq!(got[1].data, "{\"b\":\"é\"}");
        }
    }

    #[test]
    fn crlf_comments_and_multiline_data() {
        let got = all(&[b": keep-alive\r\ndata: one\r\ndata: two\r\n\r\ndata:three\n\n"]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, "one\ntwo");
        assert_eq!(got[1].data, "three");
    }

    #[test]
    fn an_event_without_a_closing_blank_line_still_counts() {
        let got = all(&[b"data: [DONE]"]);
        assert_eq!(got, vec![Event { event: None, data: "[DONE]".into() }]);
    }

    #[test]
    fn caps_are_errors() {
        let mut f = Framer::new();
        let big = vec![b'x'; MAX_EVENT + 10];
        assert!(f.push(&big).is_err(), "one line past 1 MiB");

        let mut f = Framer::new();
        let mut line = b"data: ".to_vec();
        line.extend(vec![b'y'; MAX_EVENT]);
        line.push(b'\n');
        assert!(f.push(&line).is_ok(), "exactly 1 MiB is allowed");
        let mut f = Framer::new();
        let mut line = b"data: ".to_vec();
        line.extend(vec![b'y'; MAX_EVENT + 1]);
        line.push(b'\n');
        assert!(f.push(&line).is_err(), "one event past 1 MiB");

        let mut f = Framer::new();
        let chunk = vec![b'\n'; 1024 * 1024];
        let mut failed = false;
        for _ in 0..40 {
            if f.push(&chunk).is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed, "a reply past 32 MiB");
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_panic() {
        let mut f = Framer::new();
        assert!(f.push(b"data: \xff\xfe\n\n").is_err());
    }
}
