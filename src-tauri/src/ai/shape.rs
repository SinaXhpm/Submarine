//! Shaping what the server says before anyone sees it: private-key
//! blocks masked as the output streams, and the model's copy cut to its head
//! and tail, with terminal escapes, control characters and Unicode Tag
//! characters removed.

/// What a masked private-key block becomes, in the live output and in the
/// model's copy.
pub const KEY_REMOVED: &str = "[private key removed by Submarine]";
/// A block whose END line hasn't come within this much isn't a key (a real one
/// is a few KiB), and what was held back goes out.
const MAX_KEY: usize = 64 * 1024;
/// A PGP block can carry a whole secret keyring, so it is allowed to be larger.
const MAX_PGP_KEY: usize = 1024 * 1024;
/// The longest label between the opening dashes and the closing dashes.
const MAX_LABEL: usize = 64;
/// A PuTTY key file's first line, and the header of its private block.
const PUTTY_FILE: &[u8] = b"PuTTY-User-Key-File-";
const PUTTY_PRIVATE: &[u8] = b"Private-Lines: ";
/// A sane upper bound on a PuTTY `Private-Lines` count, and its digits.
const MAX_PUTTY_LINES: usize = 10_000;
const MAX_PUTTY_DIGITS: usize = 5;
/// PuTTY wraps its base64 at 64 characters.
const PUTTY_WIDTH: usize = 64;
/// The most text a PuTTY header line may have before `Private-Lines:`, such
/// as a path and a line number from grep.
const MAX_PREFIX: usize = 512;
/// Kept back when nothing matched, so an opening marker split across pieces is
/// caught next time. The longest opening token is `PuTTY-User-Key-File-`.
const HOLDBACK: usize = PUTTY_FILE.len() - 1;

/// The armoured fence a private key comes in. PEM uses five dashes and no
/// spaces; the ssh.com/Tectia form uses four dashes and a space on each side.
#[derive(Clone, Copy)]
enum Fence {
    Pem,
    Ssh2,
}

impl Fence {
    fn begin(self) -> &'static [u8] {
        match self {
            Fence::Pem => b"-----BEGIN ",
            Fence::Ssh2 => b"---- BEGIN ",
        }
    }
    fn end(self) -> &'static [u8] {
        match self {
            Fence::Pem => b"-----END ",
            Fence::Ssh2 => b"---- END ",
        }
    }
    fn dashes(self) -> &'static [u8] {
        match self {
            Fence::Pem => b"-----",
            Fence::Ssh2 => b"----",
        }
    }
}

/// What the masker is doing between pieces.
#[derive(Clone, Copy, Default)]
enum State {
    #[default]
    Scan,
    /// Inside a dash-fenced block, searching for its END line from `scanned`.
    Block { fence: Fence, cap: usize },
    /// Inside a PuTTY private block: `left` lines still to drop, `seen` bytes
    /// of the current line checked, and whether the marker has gone out.
    Putty { left: usize, seen: usize, marked: bool },
}

/// An opening marker in the output.
enum Opening {
    /// A dash fence whose marker begins at `start`; `end` is just past it.
    Fence { fence: Fence, start: usize, end: usize },
    /// A PuTTY key file's first line.
    Header(usize),
    /// A PuTTY private block: `body` is just past its `Private-Lines:` line.
    Putty { start: usize, body: usize, lines: usize },
    /// One may begin here, and the rest of it hasn't arrived yet.
    Partial(usize),
    None,
}

/// Masks private-key blocks in a byte stream that arrives in pieces: PEM
/// (`-----BEGIN … PRIVATE KEY-----` to `-----END …-----`, PGP included), the
/// four-dash ssh.com form, and a PuTTY key file's `Private-Lines` block. A
/// block that begins in one piece is held back until it ends.
#[derive(Default)]
pub struct Masker {
    held: Vec<u8>,
    state: State,
    /// Inside a dash block: where the search for its END line goes on from.
    scanned: usize,
    /// A PuTTY key file header has been seen in this stream.
    putty: bool,
    /// Inside a PuTTY block: what its header line has before `Private-Lines:`.
    prefix: Vec<u8>,
    /// The part of the current line that has gone out, up to `MAX_PREFIX + 1`
    /// bytes.
    line: Vec<u8>,
}

impl Masker {
    /// Takes the next piece; returns what can go out now.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        self.held.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            match self.state {
                State::Block { fence, cap } => {
                    let from = self.scanned;
                    match find_fenced(&self.held[from..], fence.end(), fence.dashes()) {
                        Marker::Found { end } => {
                            out.extend_from_slice(KEY_REMOVED.as_bytes());
                            self.held.drain(..from + end);
                            self.state = State::Scan;
                        }
                        _ if self.held.len() > cap => self.give_up(&mut out, fence),
                        Marker::Partial(at) => {
                            self.scanned = from + at;
                            return out;
                        }
                        Marker::None => {
                            self.scanned = self.held.len().saturating_sub(fence.end().len() - 1).max(from);
                            return out;
                        }
                    }
                }
                State::Putty { left, seen, marked } => {
                    if !self.putty_lines(&mut out, left, seen, marked) {
                        return out;
                    }
                }
                State::Scan => {
                    if !self.scan(&mut out) {
                        return out;
                    }
                }
            }
        }
    }

    /// Scans what is held, in one pass, for the next private-key opening and
    /// starts its block. Returns whether the loop should go on (a block began)
    /// or stop and wait for more bytes.
    fn scan(&mut self, out: &mut Vec<u8>) -> bool {
        let mut at = 0;
        loop {
            match next_opening(&self.held, at, self.putty) {
                Opening::Fence { fence, start, end } => {
                    let marker = &self.held[start..end];
                    if is_key(marker) {
                        let cap = cap_for(marker);
                        self.emit(out, start);
                        self.scanned = end - start;
                        self.state = State::Block { fence, cap };
                        return true;
                    }
                    // Only past the opening dashes: their close may open the
                    // next marker.
                    at = start + fence.begin().len();
                }
                Opening::Header(start) => {
                    self.putty = true;
                    at = start + PUTTY_FILE.len();
                }
                Opening::Putty { start, body, lines } => match self.line_prefix(start) {
                    Some(prefix) => {
                        self.prefix = prefix;
                        self.emit(out, body);
                        self.state = State::Putty { left: lines, seen: 0, marked: false };
                        return true;
                    }
                    None => at = start + PUTTY_PRIVATE.len(),
                },
                Opening::Partial(start) => return self.wait(out, start),
                Opening::None => {
                    // Keep enough to finish a marker split across pieces.
                    let keep = self.held.len().saturating_sub(HOLDBACK).max(at);
                    return self.wait(out, keep);
                }
            }
        }
    }

    /// What the line holding `held[start]` has before it, if that is short
    /// enough to be a path or a line number.
    fn line_prefix(&self, start: usize) -> Option<Vec<u8>> {
        let from = start.saturating_sub(MAX_PREFIX + 1);
        let prefix = match self.held[from..start].iter().rposition(|&b| b == b'\n') {
            Some(nl) => self.held[from + nl + 1..start].to_vec(),
            None if from == 0 => [&self.line[..], &self.held[..start]].concat(),
            None => return None,
        };
        (prefix.len() <= MAX_PREFIX).then_some(prefix)
    }

    /// Drops the whole lines of a PuTTY private block that have come. The
    /// block ends after its count, or sooner at a line that isn't one of its
    /// base64 lines (a file cut short); that line is then scanned as usual.
    /// Returns whether the loop should go on.
    fn putty_lines(&mut self, out: &mut Vec<u8>, mut left: usize, mut seen: usize, mut marked: bool) -> bool {
        let mut done = 0;
        let next = loop {
            match putty_line(&self.held[done..], &self.prefix, seen) {
                Line::Key(len) => {
                    if !marked {
                        out.extend_from_slice(KEY_REMOVED.as_bytes());
                        out.push(b'\n');
                        marked = true;
                    }
                    done += len;
                    seen = 0;
                    left -= 1;
                    if left == 0 {
                        break State::Scan;
                    }
                }
                Line::More(at) => break State::Putty { left, seen: at, marked },
                Line::Not => break State::Scan,
            }
        };
        self.held.drain(..done);
        self.state = next;
        matches!(next, State::Scan)
    }

    /// A dash block got no END line within its cap, so its opening wasn't a
    /// key. A later key opening in what was held may still be one: hold on
    /// from the last of them. Without one, what was held is scanned as usual.
    fn give_up(&mut self, out: &mut Vec<u8>, fence: Fence) {
        match last_key(&self.held, fence.begin().len()) {
            Some((next, start, end)) => {
                let cap = cap_for(&self.held[start..end]);
                self.emit(out, start);
                self.scanned = end - start;
                self.state = State::Block { fence: next, cap };
            }
            None => {
                self.emit(out, fence.begin().len());
                self.state = State::Scan;
            }
        }
    }

    fn emit(&mut self, out: &mut Vec<u8>, upto: usize) {
        let part = &self.held[..upto];
        let rest = match part.iter().rposition(|&b| b == b'\n') {
            Some(nl) => {
                self.line.clear();
                &part[nl + 1..]
            }
            None => part,
        };
        let room = (MAX_PREFIX + 1).saturating_sub(self.line.len());
        self.line.extend_from_slice(&rest[..rest.len().min(room)]);
        out.extend_from_slice(part);
        self.held.drain(..upto);
    }

    /// Emit up to `upto` and stop the loop until the next piece.
    fn wait(&mut self, out: &mut Vec<u8>, upto: usize) -> bool {
        self.emit(out, upto);
        false
    }

    /// The stream ended. A block still open is masked: it may be a key that
    /// was cut off.
    pub fn finish(&mut self) -> Vec<u8> {
        match std::mem::take(&mut self.state) {
            State::Block { .. } => {
                self.held.clear();
                KEY_REMOVED.as_bytes().to_vec()
            }
            // A line cut off past its prefix may be the rest of the key.
            State::Putty { marked, .. } if self.held.len() > self.prefix.len() => {
                self.held.clear();
                if marked { Vec::new() } else { KEY_REMOVED.as_bytes().to_vec() }
            }
            _ => std::mem::take(&mut self.held),
        }
    }
}

enum Marker {
    /// A whole marker; `end` is just past its closing dashes.
    Found { end: usize },
    /// A marker begins at this point, and the rest of it hasn't arrived yet.
    Partial(usize),
    None,
}

/// The first whole `<prefix>LABEL<dashes>` in `hay`, or the start of one still
/// arriving at its end.
fn find_fenced(hay: &[u8], prefix: &[u8], dashes: &[u8]) -> Marker {
    let mut from = 0;
    while let Some(i) = find(&hay[from..], prefix) {
        match fence_at(hay, from + i, prefix, dashes) {
            Marker::None => from += i + prefix.len(),
            m => return m,
        }
    }
    Marker::None
}

/// The marker at `start`, where `hay` has `prefix`: whole when a label of
/// capitals, digits and spaces and then `dashes` follow.
fn fence_at(hay: &[u8], start: usize, prefix: &[u8], dashes: &[u8]) -> Marker {
    let after = start + prefix.len();
    let rest = &hay[after..];
    let n = rest.iter().take(MAX_LABEL + 1).take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || **b == b' ').count();
    if n <= MAX_LABEL {
        let tail = &rest[n..];
        if tail.len() >= dashes.len() {
            if n > 0 && &tail[..dashes.len()] == dashes {
                return Marker::Found { end: after + n + dashes.len() };
            }
        } else if tail.iter().all(|&b| b == b'-') {
            return Marker::Partial(start);
        }
    }
    Marker::None
}

/// The first opening marker in `hay` at or after `from`. PuTTY private
/// headers count only when `putty`: a PuTTY key file began in this stream.
fn next_opening(hay: &[u8], from: usize, putty: bool) -> Opening {
    for (start, &b) in hay.iter().enumerate().skip(from) {
        if !matches!(b, b'-' | b'P') {
            continue;
        }
        let rest = &hay[start..];
        for fence in [Fence::Pem, Fence::Ssh2] {
            if rest.starts_with(fence.begin()) {
                match fence_at(hay, start, fence.begin(), fence.dashes()) {
                    Marker::Found { end } => return Opening::Fence { fence, start, end },
                    Marker::Partial(_) => return Opening::Partial(start),
                    Marker::None => {}
                }
            }
        }
        if rest.starts_with(PUTTY_FILE) {
            return Opening::Header(start);
        }
        if putty && rest.starts_with(PUTTY_PRIVATE) {
            match putty_at(hay, start) {
                Putty::Found { body, lines } => return Opening::Putty { start, body, lines },
                Putty::Partial => return Opening::Partial(start),
                Putty::None => {}
            }
        }
    }
    Opening::None
}

/// The last private-key fence opening in `hay` at or after `from`: its fence,
/// where it begins and where its marker ends.
fn last_key(hay: &[u8], from: usize) -> Option<(Fence, usize, usize)> {
    let mut at = from;
    let mut last = None;
    loop {
        match next_opening(hay, at, false) {
            Opening::Fence { fence, start, end } => {
                if is_key(&hay[start..end]) {
                    last = Some((fence, start, end));
                }
                at = start + fence.begin().len();
            }
            Opening::Header(start) | Opening::Putty { start, .. } => at = start + 1,
            Opening::Partial(_) | Opening::None => return last,
        }
    }
}

/// Whether a fence's opening marker opens a private key.
fn is_key(marker: &[u8]) -> bool {
    contains(marker, b"PRIVATE KEY")
}

/// How much a block that opens with `marker` may hold before it is let go.
fn cap_for(marker: &[u8]) -> usize {
    if contains(marker, b"PGP") { MAX_PGP_KEY } else { MAX_KEY }
}

enum Putty {
    /// `lines` base64 lines follow; `body` is just past the header line.
    Found { body: usize, lines: usize },
    /// The rest of the header line hasn't arrived yet.
    Partial,
    None,
}

/// The rest of a PuTTY key file's `Private-Lines: N` header line at `start`.
fn putty_at(hay: &[u8], start: usize) -> Putty {
    let after = start + PUTTY_PRIVATE.len();
    let digits = hay[after..].iter().take(MAX_PUTTY_DIGITS + 1).take_while(|b| b.is_ascii_digit()).count();
    let mut k = after + digits;
    if digits > MAX_PUTTY_DIGITS || (digits == 0 && k < hay.len()) {
        return Putty::None;
    }
    if hay.get(k) == Some(&b'\r') {
        k += 1;
    }
    match hay.get(k) {
        None => Putty::Partial,
        Some(&b'\n') => {
            let lines = hay[after..after + digits].iter().fold(0, |n, d| n * 10 + usize::from(*d - b'0'));
            if lines == 0 || lines > MAX_PUTTY_LINES {
                Putty::None
            } else {
                Putty::Found { body: k + 1, lines }
            }
        }
        Some(_) => Putty::None,
    }
}

/// How the line at the start of `hay` compares with a line of a PuTTY private
/// block so far.
enum Line {
    /// A whole line of the key, this long with its line end.
    Key(usize),
    /// It fits as far as it has come; checked up to here.
    More(usize),
    /// Not a line of the key.
    Not,
}

/// Checks the line at the start of `hay` from `seen` on: the header line's
/// `prefix`, then up to 64 base64 characters and the line end.
fn putty_line(hay: &[u8], prefix: &[u8], seen: usize) -> Line {
    let p = prefix.len();
    let mut i = seen;
    while let Some(&b) = hay.get(i) {
        if i < p {
            if !alike(b, prefix[i]) {
                return Line::Not;
            }
        } else if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=') {
            if i - p == PUTTY_WIDTH {
                return Line::Not;
            }
        } else {
            if i == p {
                return Line::Not;
            }
            let end = match (b, hay.get(i + 1)) {
                (b'\n', _) => i + 1,
                (b'\r', Some(&b'\n')) => i + 2,
                (b'\r', None) => return Line::More(i),
                _ => return Line::Not,
            };
            // The base64 part can't hold an opening marker, but the prefix
            // part might: such a line ends the block too.
            let opens =[Fence::Pem.begin(), Fence::Ssh2.begin(), PUTTY_FILE].iter().any(|t| contains(&hay[..p], t));
            return if opens { Line::Not } else { Line::Key(end) };
        }
        i += 1;
    }
    Line::More(i)
}

/// Whether a byte of a line's prefix matches the header line's: grep -n and
/// cat -n number each line, and grep marks context lines with `-` where it
/// marks matches with `:`.
fn alike(a: u8, b: u8) -> bool {
    a == b || (a.is_ascii_digit() && b.is_ascii_digit()) || (matches!(a, b':' | b'-') && matches!(b, b':' | b'-'))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// What is kept of one output stream: its first and last bytes for the model,
/// and up to `ui_cap` for the page.
pub struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    each: usize,
    pub total: u64,
    pub ui: Vec<u8>,
    ui_cap: usize,
}

impl Capture {
    pub fn new(each: usize, ui_cap: usize) -> Self {
        Capture { head: Vec::new(), tail: std::collections::VecDeque::new(), each, total: 0, ui: Vec::new(), ui_cap }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let room = self.ui_cap.saturating_sub(self.ui.len());
        self.ui.extend_from_slice(&data[..data.len().min(room)]);
        let to_head = self.each.saturating_sub(self.head.len()).min(data.len());
        self.head.extend_from_slice(&data[..to_head]);
        let rest = &data[to_head..];
        if rest.len() >= self.each {
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.each..]);
        } else {
            self.tail.extend(rest);
            while self.tail.len() > self.each {
                self.tail.pop_front();
            }
        }
    }

    /// The model's copy: shaped, with a marker where the middle was cut.
    pub fn for_model(&self) -> String {
        let cut = self.total.saturating_sub((self.head.len() + self.tail.len()) as u64);
        let mut s = clean(&self.head);
        if cut > 0 {
            s.push_str(&format!("\n[… {} bytes cut …]\n", thousands(cut)));
        }
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        s.push_str(&clean(&tail));
        s
    }
}

/// Text for the model: lossy UTF-8, without terminal escape sequences, control
/// characters (`\n` and `\t` stay) or Unicode Tag characters, which are
/// invisible on screen and a known way to hide instructions for a model.
pub fn clean(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            // C1 CSI: the one-byte form of ESC [.
            '\u{9b}' => skip_csi(&mut chars),
            '\n' | '\t' => out.push(c),
            '\r' => {
                // A lone CR redraws the line on a terminal; as text it is a break.
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            c if c.is_control() => {}
            '\u{E0000}'..='\u{E007F}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Removes Unicode Tag characters (U+E0000–U+E007F), which are invisible on
/// screen and a known way to hide instructions for a model. Unlike `clean`, it
/// changes nothing else, so line numbers, CRLF and byte offsets are kept.
pub fn strip_tags(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\u{E0000}'..='\u{E007F}')).collect()
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC, DCS, SOS, PM, APC: up to BEL or ESC \.
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Two-byte forms with an intermediate: ESC ( B, ESC # 8, …
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            chars.next();
        }
        _ => {}
    }
}

fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

/// A file name, a path or a server's message as one line for the model:
/// without Tag characters, and with everything that breaks a line or can't
/// be seen written out (`\n`, `\t`, `\x1b`, `\u{2028}`). A name is the
/// server's to choose, and one that held a line break would start a line of
/// its own, where it could pass for another entry, a header or a note.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{E0000}'..='\u{E007F}' => {}
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Text with every kind of line break as `\n`: CR LF, a lone CR, and the
/// ones only some readers honour (VT, FF, NEL, U+2028, U+2029). For text
/// that is laid out by line, where a break nobody counted would start one.
pub fn plain_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

/// `48211` as `48,211`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The first `max` bytes of `s`, cut at a character boundary.
pub fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ\n-----END OPENSSH PRIVATE KEY-----";

    fn masked(pieces: &[&[u8]]) -> String {
        let mut m = Masker::default();
        let mut out = Vec::new();
        for p in pieces {
            out.extend(m.push(p));
        }
        out.extend(m.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_key_is_masked_however_it_is_split() {
        let text = format!("before\n{KEY}\nafter\n");
        let want = format!("before\n{KEY_REMOVED}\nafter\n");
        assert_eq!(masked(&[text.as_bytes()]), want);
        for size in [1, 2, 3, 5, 7, 11, 40] {
            let pieces: Vec<&[u8]> = text.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
    }

    #[test]
    fn other_blocks_and_plain_text_pass() {
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        assert_eq!(masked(&[cert.as_bytes()]), cert);
        let plain = "a ----- b -----BEGIN lower----- c\n";
        assert_eq!(masked(&[plain.as_bytes()]), plain);
        assert_eq!(masked(&[b"short"]), "short");
    }

    #[test]
    fn a_cut_off_key_is_masked_and_a_huge_block_is_let_through() {
        assert_eq!(masked(&[b"x\n-----BEGIN RSA PRIVATE KEY-----\nMIIE"]), format!("x\n{KEY_REMOVED}"));
        let mut big = b"-----BEGIN EC PRIVATE KEY-----\n".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_KEY + 10));
        big.extend(b"\nrest");
        let out = masked(&[&big]);
        assert!(out.starts_with("-----BEGIN EC PRIVATE KEY-----") && out.ends_with("rest"));
    }

    #[test]
    fn a_grep_line_with_a_prefix() {
        let line = format!("/etc/ssl/k.pem:{KEY}\n");
        assert_eq!(masked(&[line.as_bytes()]), format!("/etc/ssl/k.pem:{KEY_REMOVED}\n"));
    }

    #[test]
    fn a_key_marker_right_after_another_marker() {
        let text = format!("-----BEGIN ABC-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----!");
        for size in [1, 4, 100] {
            let pieces: Vec<&[u8]> = text.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), format!("-----BEGIN ABC{KEY_REMOVED}!"), "pieces of {size}");
        }
    }

    #[test]
    fn byte_by_byte_into_a_long_block_stays_fast() {
        let mut m = Masker::default();
        let t = std::time::Instant::now();
        m.push(b"-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..(MAX_KEY + 100) {
            m.push(b"A");
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn cleaning_drops_escapes_controls_and_tags() {
        let raw = "\u{1b}[1;31mred\u{1b}[0m \u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7} a\u{0}b\r\nline\rover\u{E0041}\u{E0042}!\t\u{9b}2Jz";
        assert_eq!(clean(raw.as_bytes()), "red link ab\nline\nover!\tz");
        assert_eq!(clean(&[0xff, b'a']), "\u{fffd}a");
    }

    #[test]
    fn capture_keeps_head_and_tail_with_the_cut_counted() {
        let mut c = Capture::new(4, 6);
        for b in b"0123456789abcdef".chunks(3) {
            c.push(b);
        }
        assert_eq!(c.total, 16);
        assert_eq!(c.ui, b"012345");
        assert_eq!(c.for_model(), "0123\n[… 8 bytes cut …]\ncdef");
        let mut small = Capture::new(4, 100);
        small.push(b"abcdef");
        assert_eq!(small.for_model(), "abcdef");
        let mut big = Capture::new(4, 100);
        big.push(b"0123456789");
        assert_eq!(big.for_model(), "0123\n[… 2 bytes cut …]\n6789");
    }

    #[test]
    fn numbers_and_cuts() {
        assert_eq!(thousands(48211), "48,211");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(cut("aé", 2), "a");
    }

    #[test]
    fn a_pgp_block_is_masked_even_when_large() {
        let pgp = "before\n-----BEGIN PGP PRIVATE KEY BLOCK-----\nlQOYBF\n-----END PGP PRIVATE KEY BLOCK-----\nafter\n";
        let want = format!("before\n{KEY_REMOVED}\nafter\n");
        assert_eq!(masked(&[pgp.as_bytes()]), want);
        // A whole keyring export is bigger than MAX_KEY but still masked.
        let mut big = b"-----BEGIN PGP PRIVATE KEY BLOCK-----\n".to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_KEY + 1000));
        big.extend(b"\n-----END PGP PRIVATE KEY BLOCK-----\ntail");
        let want = format!("{KEY_REMOVED}\ntail");
        for size in [1usize, 64, 4096] {
            let pieces: Vec<&[u8]> = big.chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
    }

    #[test]
    fn an_ssh2_four_dash_key_is_masked() {
        let key = "---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----\nP2/56wA\nAAAB\n---- END SSH2 ENCRYPTED PRIVATE KEY ----";
        let text = format!("note\n{key}\nend\n");
        let want = format!("note\n{KEY_REMOVED}\nend\n");
        assert_eq!(masked(&[text.as_bytes()]), want);
        for size in [1usize, 3, 9, 50] {
            let pieces: Vec<&[u8]> = text.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
        // The public form (no "PRIVATE KEY" in the label) passes untouched.
        let pubk = "---- BEGIN SSH2 PUBLIC KEY ----\nAAAA\n---- END SSH2 PUBLIC KEY ----\n";
        assert_eq!(masked(&[pubk.as_bytes()]), pubk);
    }

    #[test]
    fn a_putty_private_block_is_masked() {
        let head = "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\nComment: a\nPublic-Lines: 2\nAAAAfoo\nbarbaz=\n";
        let ppk = format!("{head}Private-Lines: 1\nQUJDREVG\nPrivate-MAC: 00ff\n");
        let want = format!("{head}Private-Lines: 1\n{KEY_REMOVED}\nPrivate-MAC: 00ff\n");
        assert_eq!(masked(&[ppk.as_bytes()]), want);
        for size in [1usize, 5, 17, 64] {
            let pieces: Vec<&[u8]> = ppk.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
        // A two-line private block, and \r\n endings.
        let ppk2 = "PuTTY-User-Key-File-2: ssh-rsa\r\nPrivate-Lines: 2\r\nAAA\r\nBBB\r\nPrivate-MAC: x\r\n";
        let want2 = format!("PuTTY-User-Key-File-2: ssh-rsa\r\nPrivate-Lines: 2\r\n{KEY_REMOVED}\nPrivate-MAC: x\r\n");
        assert_eq!(masked(&[ppk2.as_bytes()]), want2);
        // Without the PuTTY header, a `Private-Lines:` line is only text.
        let plain = "Private-Lines: 1\nnot-a-secret\n";
        assert_eq!(masked(&[plain.as_bytes()]), plain);
    }

    #[test]
    fn a_cut_short_putty_block_ends_where_its_lines_do() {
        // head over a keys folder: a .ppk whose private block is cut short,
        // then a PEM key.
        let ppk = "PuTTY-User-Key-File-3: ssh-rsa\nEncryption: none\nComment: a\nPublic-Lines: 1\nAAAAB3Nz\nPrivate-Lines: 14\nQUJDREVG\nSElKS0xN\n";
        let text = format!("==> a.ppk <==\n{ppk}\n==> b <==\n{KEY}\nend\n");
        let kept = &ppk[..ppk.find("QUJD").unwrap()];
        let want = format!("==> a.ppk <==\n{kept}{KEY_REMOVED}\n\n==> b <==\n{KEY_REMOVED}\nend\n");
        for size in [1usize, 2, 3, 5, 8, 13, 64, 4096] {
            let pieces: Vec<&[u8]> = text.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
        // grep's forms: each line starts with the path, and with -n and -A
        // also a line number and ':' or '-'.
        let path = "backups/old-laptop/keys/a.ppk:";
        let grep = format!("{path}PuTTY-User-Key-File-3: ssh-ed25519\n{path}Private-Lines: 2\n{path}QUJDREVG\n{path}SElKS0xN\n{path}Private-MAC: 00ff\n");
        let want = format!("{path}PuTTY-User-Key-File-3: ssh-ed25519\n{path}Private-Lines: 2\n{KEY_REMOVED}\n{path}Private-MAC: 00ff\n");
        let numbered = "a.ppk:1:PuTTY-User-Key-File-3: ssh-ed25519\na.ppk:7:Private-Lines: 2\na.ppk-8-QUJDREVG\na.ppk-9-SElKS0xN\n--\n";
        let want_numbered = format!("a.ppk:1:PuTTY-User-Key-File-3: ssh-ed25519\na.ppk:7:Private-Lines: 2\n{KEY_REMOVED}\n--\n");
        for size in [1usize, 7, 31, 4096] {
            let pieces: Vec<&[u8]> = grep.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
            let pieces: Vec<&[u8]> = numbered.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want_numbered, "numbered, pieces of {size}");
        }
        // The stream ends inside a line of the block.
        let cut = "PuTTY-User-Key-File-3: x\nPrivate-Lines: 2\nQUJD";
        assert_eq!(masked(&[cut.as_bytes()]), format!("PuTTY-User-Key-File-3: x\nPrivate-Lines: 2\n{KEY_REMOVED}"));
        // An odd `Private-Lines:` line before the real one doesn't hide it.
        let two = "PuTTY-User-Key-File-3: x\nPrivate-Lines: none\nPrivate-Lines: 1\nQUJD\n";
        assert_eq!(masked(&[two.as_bytes()]), format!("PuTTY-User-Key-File-3: x\nPrivate-Lines: none\nPrivate-Lines: 1\n{KEY_REMOVED}\n"));
    }

    #[test]
    fn hostile_putty_output_is_not_held() {
        // Endless digits after the header aren't a count: nothing waits on them.
        let mut m = Masker::default();
        let mut input = b"PuTTY-User-Key-File-3: x\nPrivate-Lines: ".to_vec();
        let mut out = m.push(&input);
        let digits = vec![b'7'; 64 * 1024];
        for _ in 0..16 {
            out.extend(m.push(&digits));
            input.extend(&digits);
            assert!(m.held.len() <= HOLDBACK, "{} bytes held", m.held.len());
        }
        out.extend(m.finish());
        assert_eq!(out, input);
        // A base64 line longer than PuTTY writes ends the block.
        let mut m = Masker::default();
        m.push(b"PuTTY-User-Key-File-3: x\nPrivate-Lines: 3\n");
        let long = vec![b'A'; 64 * 1024];
        for _ in 0..16 {
            m.push(&long);
            assert!(m.held.len() <= HOLDBACK, "{} bytes held", m.held.len());
        }
    }

    #[test]
    fn many_markers_that_open_no_key_stay_fast() {
        let text = "-----BEGIN A-----".repeat(64 * 1024);
        let t = std::time::Instant::now();
        assert_eq!(masked(&[text.as_bytes()]), text);
        let pieces: Vec<&[u8]> = text.as_bytes().chunks(32 * 1024).collect();
        assert_eq!(masked(&pieces), text);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn at_the_cap_a_later_key_opening_is_kept() {
        // A stray opening with no END line (a grep hit, say), then a real key
        // whose END line comes only past the cap.
        let stray = "-----BEGIN RSA PRIVATE KEY-----\n";
        let filler = "x".repeat(MAX_KEY - 100);
        let key = format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----", "A".repeat(400));
        let text = format!("{stray}{filler}\n{key}\nafter\n");
        let want = format!("{stray}{filler}\n{KEY_REMOVED}\nafter\n");
        for size in [1usize, 64, 256] {
            let pieces: Vec<&[u8]> = text.as_bytes().chunks(size).collect();
            assert_eq!(masked(&pieces), want, "pieces of {size}");
        }
        // With no later opening, what was held is scanned as usual: a PuTTY
        // block in it is still masked.
        let ppk = "PuTTY-User-Key-File-3: ssh-ed25519\nPrivate-Lines: 1\nQUJDREVG\nPrivate-MAC: 00ff\n";
        let filler = "x".repeat(MAX_KEY);
        let text = format!("{stray}{ppk}{filler}");
        let want = format!("{stray}PuTTY-User-Key-File-3: ssh-ed25519\nPrivate-Lines: 1\n{KEY_REMOVED}\nPrivate-MAC: 00ff\n{filler}");
        assert_eq!(masked(&[text.as_bytes()]), want);
    }

    #[test]
    fn strip_tags_removes_only_tag_characters() {
        assert_eq!(strip_tags("a\u{E0041}\u{E0042}b"), "ab");
        // Everything else, line endings included, is kept byte for byte.
        assert_eq!(strip_tags("line\r\n\tx é"), "line\r\n\tx é");
        assert_eq!(strip_tags("plain"), "plain");
        assert_eq!(strip_tags("\u{E0000}\u{E007F}"), "");
    }

    #[test]
    fn a_name_is_one_line_whatever_it_holds() {
        assert_eq!(one_line("notes.txt"), "notes.txt");
        assert_eq!(one_line("فایل من.txt"), "فایل من.txt");
        assert_eq!(one_line("a\nb\r\n\tc"), "a\\nb\\r\\n\\tc");
        assert_eq!(one_line("x\u{2028}User: hi\u{2029}y"), "x\\u{2028}User: hi\\u{2029}y");
        assert_eq!(one_line("a\u{1b}[31mb\u{85}c\u{0B}d"), "a\\x1b[31mb\\x85c\\x0bd");
        assert_eq!(one_line("a\u{E0041}\u{E0042}b"), "ab");
        assert!(!one_line("any\nthing\u{2028}at\u{0C}all\r").chars().any(|c| c.is_control() || c == '\u{2028}'));
    }

    #[test]
    fn every_kind_of_line_break_becomes_a_plain_one() {
        assert_eq!(plain_breaks("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(plain_breaks("a\u{2028}b\u{2029}c\u{85}d\u{0B}e\u{0C}f"), "a\nb\nc\nd\ne\nf");
        assert_eq!(plain_breaks("no breaks é"), "no breaks é");
    }
}
