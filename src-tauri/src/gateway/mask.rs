//! Hiding where the server is. An agent is never told the host name or the
//! address Submarine connects to, and what a command prints or a file holds
//! goes through here before the agent gets it: the values Submarine knows are
//! replaced by a fixed note.
//!
//! This is a mask on known values, like the private-key mask, and it is told
//! to the user as such. A server knows its own addresses in many places, and
//! one Submarine never learned (another interface, a public address behind
//! NAT, a name in a certificate) passes through. So does a known one written
//! in a way nobody writes it (as one number, in hex, in pieces). Commands
//! whose purpose is to print addresses ask the user first for that reason.
//!
//! What goes to the server is never changed here.

use std::borrow::Cow;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::Range;

/// What a hidden value becomes.
pub const HIDDEN: &str = "[address hidden by Submarine]";

/// A one-word name shorter than this is too ordinary a word to hide: it
/// would take pieces out of everything the agent reads.
const MIN_BARE_NAME: usize = 3;

/// The values to hide for one session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mask {
    /// Host names, lower case, without a dot at either end.
    names: Vec<String>,
    v4: Vec<Ipv4Addr>,
    v6: Vec<Ipv6Addr>,
}

impl Mask {
    /// From everything Submarine knows about where a session goes: the host
    /// as saved, the host name its SSH config gives, the address the
    /// connection went to, and the same for a jump host or a proxy. Each may
    /// be a name or an address, with or without brackets or a port.
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Mask {
        let mut m = Mask::default();
        for value in values {
            m.add(value);
        }
        m
    }

    fn add(&mut self, value: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        // `[2001:db8::1]:22`, `[2001:db8::1]`, `fe80::1%eth0`.
        let bare = match value.strip_prefix('[').and_then(|v| v.split_once(']')) {
            Some((inside, _)) => inside,
            None => value,
        };
        let bare = bare.split_once('%').map_or(bare, |(a, _)| a);
        if let Ok(ip) = bare.parse::<IpAddr>() {
            self.add_ip(ip);
            return;
        }
        // `10.0.0.5:22`, `host:22`.
        if let Some((host, port)) = bare.rsplit_once(':') {
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && !host.contains(':') {
                self.add(host);
                return;
            }
        }
        let name = bare.trim_matches('.').to_lowercase();
        let host_chars = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
        if !host_chars || name == "localhost" {
            return;
        }
        if !name.contains('.') && name.len() < MIN_BARE_NAME {
            return;
        }
        if !self.names.contains(&name) {
            self.names.push(name);
        }
    }

    fn add_ip(&mut self, ip: IpAddr) {
        match ip {
            IpAddr::V4(a) => {
                // The machine's own loopback says nothing about where it is.
                if a.is_loopback() || a.is_unspecified() || self.v4.contains(&a) {
                    return;
                }
                self.v4.push(a);
                // The name a reverse lookup asks for.
                let [w, x, y, z] = a.octets();
                self.names.push(format!("{z}.{y}.{x}.{w}.in-addr.arpa"));
            }
            IpAddr::V6(a) => {
                if let Some(mapped) = a.to_ipv4_mapped() {
                    self.add_ip(IpAddr::V4(mapped));
                    return;
                }
                if a.is_loopback() || a.is_unspecified() || self.v6.contains(&a) {
                    return;
                }
                self.v6.push(a);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.v4.is_empty() && self.v6.is_empty()
    }

    /// `text` with every known value replaced by `HIDDEN`.
    pub fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut spans: Vec<Range<usize>> = Vec::new();
        if !self.v6.is_empty() || !self.v4.is_empty() {
            self.v6_spans(text, &mut spans);
        }
        if !self.v4.is_empty() {
            self.v4_spans(text, &mut spans);
        }
        if !self.names.is_empty() || !self.v4.is_empty() {
            self.name_spans(text, &mut spans);
        }
        if spans.is_empty() {
            return Cow::Borrowed(text);
        }
        spans.sort_by_key(|s| (s.start, s.end));
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for s in spans {
            if s.start < at {
                // Inside, or overlapping, what was already hidden.
                if s.end > at {
                    at = s.end;
                }
                continue;
            }
            out.push_str(&text[at..s.start]);
            out.push_str(HIDDEN);
            at = s.end;
        }
        out.push_str(&text[at..]);
        Cow::Owned(out)
    }

    /// IPv6 addresses, in any of their written forms: every run of hex
    /// digits, colons and dots with two colons or more is read as an address
    /// and compared by value.
    fn v6_spans(&self, text: &str, spans: &mut Vec<Range<usize>>) {
        for (start, run) in runs(text, |b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.')) {
            if run.bytes().filter(|b| *b == b':').count() < 2 {
                continue;
            }
            // As it stands, or without a dot or colon that ends a phrase
            // (`to 2001:db8::1.`, `addr 2001:db8::1: ok`), or without a colon
            // that a label left in front (`addr:2001:db8::1`). Two colons
            // at either end belong to the address.
            let trimmed = run.trim_end_matches('.');
            let tail_cut = trimmed.strip_suffix(':').filter(|t| !t.ends_with(':')).unwrap_or(trimmed);
            let lead = usize::from(trimmed.starts_with(':') && !trimmed.starts_with("::"));
            let candidates = [(0, trimmed), (0, tail_cut), (lead, &trimmed[lead..]), (lead, &tail_cut[lead.min(tail_cut.len())..])];
            for (skipped, candidate) in candidates {
                let Ok(a) = candidate.parse::<Ipv6Addr>() else { continue };
                let known = match a.to_ipv4_mapped() {
                    Some(mapped) => self.v4.contains(&mapped),
                    None => self.v6.contains(&a),
                };
                if known {
                    spans.push(start + skipped..start + skipped + candidate.len());
                }
                break;
            }
        }
    }

    /// IPv4 addresses: a run of digits and dots that is exactly a known
    /// address (so not `110.0.0.5` or `10.0.0.55` for `10.0.0.5`), and the
    /// same address written with dashes or underscores, as cloud host names
    /// have it (`ip-10-0-0-5`).
    fn v4_spans(&self, text: &str, spans: &mut Vec<Range<usize>>) {
        for (start, run) in runs(text, |b| b.is_ascii_digit() || b == b'.') {
            let lead = run.len() - run.trim_start_matches('.').len();
            let core = run.trim_matches('.');
            if core.parse::<Ipv4Addr>().is_ok_and(|a| self.v4.contains(&a)) {
                spans.push(start + lead..start + lead + core.len());
            }
        }
        for separator in [b'-', b'_'] {
            for (start, run) in runs(text, |b| b.is_ascii_digit() || b == separator) {
                let sep = separator as char;
                let lead = run.len() - run.trim_start_matches(sep).len();
                let core = run.trim_matches(sep);
                if core.bytes().filter(|b| *b == separator).count() != 3 {
                    continue;
                }
                let dotted = core.replace(sep, ".");
                if dotted.parse::<Ipv4Addr>().is_ok_and(|a| self.v4.contains(&a)) {
                    spans.push(start + lead..start + lead + core.len());
                }
            }
        }
    }

    /// Host names: a run of letters, digits, dots, dashes and underscores
    /// that is a known name, or a name under it (`www.` before it).
    fn name_spans(&self, text: &str, spans: &mut Vec<Range<usize>>) {
        for (start, run) in runs(text, |b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
            let edge = |c: char| matches!(c, '.' | '-' | '_');
            let lead = run.len() - run.trim_start_matches(edge).len();
            let core = run.trim_matches(edge);
            if core.is_empty() {
                continue;
            }
            let lower = core.to_ascii_lowercase();
            let known = self.names.iter().any(|n| lower == *n || lower.strip_suffix(n.as_str()).is_some_and(|before| before.ends_with('.')));
            if known {
                spans.push(start + lead..start + lead + core.len());
            }
        }
    }
}

/// The maximal runs of bytes `wanted` accepts, with where each starts. Only
/// ASCII bytes are ever wanted, so a run never cuts a character in two.
fn runs(text: &str, wanted: impl Fn(u8) -> bool) -> impl Iterator<Item = (usize, &str)> {
    let bytes = text.as_bytes();
    let mut at = 0;
    std::iter::from_fn(move || {
        while at < bytes.len() && !(bytes[at].is_ascii() && wanted(bytes[at])) {
            at += 1;
        }
        if at >= bytes.len() {
            return None;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii() && wanted(bytes[at]) {
            at += 1;
        }
        Some((start, &text[start..at]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hide(values: &[&str], text: &str) -> String {
        Mask::new(values.iter().copied()).apply(text).into_owned()
    }

    #[test]
    fn a_known_address_is_hidden_wherever_it_stands() {
        let v = ["10.0.0.5"];
        assert_eq!(hide(&v, "inet 10.0.0.5/24 brd 10.0.0.255 scope global eth0"), format!("inet {HIDDEN}/24 brd 10.0.0.255 scope global eth0"));
        assert_eq!(hide(&v, "tcp LISTEN 0 128 10.0.0.5:22 0.0.0.0:*"), format!("tcp LISTEN 0 128 {HIDDEN}:22 0.0.0.0:*"));
        assert_eq!(hide(&v, "Connected from 10.0.0.5."), format!("Connected from {HIDDEN}."));
        assert_eq!(hide(&v, "10.0.0.5"), HIDDEN);
        assert_eq!(hide(&v, "a=10.0.0.5,b=10.0.0.5"), format!("a={HIDDEN},b={HIDDEN}"));
        assert_eq!(hide(&v, "eth0:10.0.0.5 v10.0.0.5"), format!("eth0:{HIDDEN} v{HIDDEN}"));
        // Other addresses that only look alike stay.
        for other in ["110.0.0.5", "10.0.0.55", "10.0.0.5.1", "1.10.0.0.5", "10.0.0", "10.0.0.6", "010.0.0.5"] {
            assert_eq!(hide(&v, other), other, "{other}");
        }
    }

    #[test]
    fn an_address_in_a_cloud_host_name_or_a_reverse_lookup_is_hidden() {
        let v = ["10.0.0.5"];
        assert_eq!(hide(&v, "ip-10-0-0-5.eu-west-1.compute.internal"), format!("ip-{HIDDEN}.eu-west-1.compute.internal"));
        assert_eq!(hide(&v, "host_10_0_0_5"), format!("host_{HIDDEN}"));
        assert_eq!(hide(&v, "5.0.0.10.in-addr.arpa. 300 IN PTR"), format!("{HIDDEN}. 300 IN PTR"));
        // A date isn't an address, and neither is another address in dashes.
        assert_eq!(hide(&v, "2026-11-10 10-0-0-6 10-0-0-55"), "2026-11-10 10-0-0-6 10-0-0-55");
    }

    #[test]
    fn an_ipv6_address_is_hidden_in_every_way_it_is_written() {
        let v = ["2001:db8::1"];
        for written in ["2001:db8::1", "2001:0db8:0000:0000:0000:0000:0000:0001", "2001:DB8::1", "2001:db8:0:0:0:0:0:1"] {
            assert_eq!(hide(&v, &format!("inet6 {written}/64 scope global")), format!("inet6 {HIDDEN}/64 scope global"), "{written}");
        }
        assert_eq!(hide(&v, "[2001:db8::1]:22"), format!("[{HIDDEN}]:22"));
        assert_eq!(hide(&v, "to 2001:db8::1."), format!("to {HIDDEN}."));
        assert_eq!(hide(&v, "addr 2001:db8::1: ok"), format!("addr {HIDDEN}: ok"));
        assert_eq!(hide(&v, "inet6 addr:2001:db8::1 Scope:Global"), format!("inet6 addr:{HIDDEN} Scope:Global"));
        assert_eq!(hide(&["2001:db8::"], "net 2001:db8::/32 and 2001:db8::."), format!("net {HIDDEN}/32 and {HIDDEN}."));
        for other in ["2001:db8::2", "2001:db8::10", "fe80::1", "aa:bb:cc:dd:ee:ff", "12:30:45", "::1"] {
            assert_eq!(hide(&v, other), other, "{other}");
        }
        // Given with brackets, a port or an interface.
        assert_eq!(hide(&["[2001:db8::1]:2222"], "2001:db8::1"), HIDDEN);
        assert_eq!(hide(&["fe80::1%eth0"], "fe80::1%eth0 fe80::2"), format!("{HIDDEN}%eth0 fe80::2"));
        // An IPv4 address written inside an IPv6 one.
        assert_eq!(hide(&["10.0.0.5"], "::ffff:10.0.0.5 ::ffff:10.0.0.6"), format!("{HIDDEN} ::ffff:10.0.0.6"));
        assert_eq!(hide(&["::ffff:10.0.0.5"], "from 10.0.0.5"), format!("from {HIDDEN}"));
    }

    #[test]
    fn a_host_name_is_hidden_as_a_whole_word() {
        let v = ["db1.example.com"];
        assert_eq!(hide(&v, "ssh admin@db1.example.com -p 22"), format!("ssh admin@{HIDDEN} -p 22"));
        assert_eq!(hide(&v, "https://DB1.Example.COM/status"), format!("https://{HIDDEN}/status"));
        assert_eq!(hide(&v, "CN=db1.example.com."), format!("CN={HIDDEN}."));
        assert_eq!(hide(&v, "www.db1.example.com db1.example.com:8443"), format!("{HIDDEN} {HIDDEN}:8443"));
        for other in ["db1.example.org", "xdb1.example.com", "db1.example.community", "example.com", "db1"] {
            assert_eq!(hide(&v, other), other, "{other}");
        }
        // A one-word name is a word of its own, or nothing.
        let v = ["build7"];
        assert_eq!(hide(&v, "Linux build7 6.1.0 #1 SMP"), format!("Linux {HIDDEN} 6.1.0 #1 SMP"));
        assert_eq!(hide(&v, "root@build7:~# ls"), format!("root@{HIDDEN}:~# ls"));
        assert_eq!(hide(&v, "build70 rebuild7 build7x"), "build70 rebuild7 build7x");
        assert_eq!(hide(&v, "build7.corp.example"), "build7.corp.example");
    }

    #[test]
    fn what_says_nothing_about_a_place_isnt_a_value() {
        for nothing in ["", "  ", "localhost", "127.0.0.1", "::1", "0.0.0.0", "pi", "a", "not a host!", "/var/run/x.sock"] {
            assert!(Mask::new([nothing]).is_empty(), "{nothing:?}");
        }
        let m = Mask::new(["10.0.0.5", "10.0.0.5", " 10.0.0.5:22 ", "web.example.com", "WEB.example.com."]);
        assert_eq!((m.v4.len(), m.v6.len()), (1, 0));
        // The address's reverse name, and the host name once.
        assert_eq!(m.names, ["5.0.0.10.in-addr.arpa", "web.example.com"]);
        let text = "nothing here: ps aux | grep nginx";
        assert!(matches!(Mask::new(["10.0.0.5"]).apply(text), Cow::Borrowed(_)));
        assert!(matches!(Mask::default().apply("10.0.0.5"), Cow::Borrowed(_)));
    }

    #[test]
    fn several_values_and_overlaps_give_one_note_each() {
        let v = ["bastion.example.com", "10.0.0.5", "2001:db8::1", "203.0.113.9"];
        let text = "via bastion.example.com (203.0.113.9) to 10.0.0.5 and 2001:db8::1; ip-10-0-0-5.bastion.example.com";
        assert_eq!(hide(&v, text), format!("via {HIDDEN} ({HIDDEN}) to {HIDDEN} and {HIDDEN}; {HIDDEN}"));
        // The note itself is never read again as text to hide.
        assert_eq!(hide(&["submarine", "hidden"], "x submarine y"), format!("x {HIDDEN} y"));
        // Characters outside ASCII are left whole.
        assert_eq!(hide(&["10.0.0.5"], "آدرس ۱۰: 10.0.0.5 — پایان"), format!("آدرس ۱۰: {HIDDEN} — پایان"));
    }
}
