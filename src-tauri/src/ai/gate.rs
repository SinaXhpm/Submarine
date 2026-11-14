//! The policy gate: at each permission level, which tool calls run
//! by themselves, which ask the user, and which are refused.
//!
//! Pure functions with no I/O, so every rule is unit-tested here. File paths
//! arrive already resolved (SFTP realpath where the path exists). Commands
//! arrive as the model wrote them.
//!
//! The always-ask groups and the read-only classifier are speed bumps, not a
//! security boundary. Anything the classifier can't prove read-only
//! asks.
//!
//! Commands are read by a small POSIX shell-word parser: quotes, escapes,
//! operators and here-documents, with nothing expanded. Where it can't follow
//! the shell it gives up, and the classifier says no.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::LazyLock;

/// A server's permission level. `Full` is never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Suggest,
    AskAll,
    Ask,
    Auto,
    Full,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Suggest => "suggest",
            Level::AskAll => "ask_all",
            Level::Ask => "ask",
            Level::Auto => "auto",
            Level::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "suggest" => Level::Suggest,
            "ask_all" => Level::AskAll,
            "ask" => Level::Ask,
            "auto" => Level::Auto,
            "full" => Level::Full,
            _ => return None,
        })
    }
}

/// One tool call, as the gate sees it.
#[derive(Clone, Copy, Debug)]
pub enum Call<'a> {
    Run { command: &'a str },
    List,
    Read { path: &'a str },
    /// `write_file` and `edit_file`.
    Write { path: &'a str },
}

/// What the gate knows about the chat and its server.
#[derive(Clone, Copy, Debug)]
pub struct Ctx<'a> {
    /// The server's session is open (a chat continued from the AI page may
    /// have none).
    pub connected: bool,
    /// The facts probe found a POSIX shell.
    pub posix: bool,
    /// The login user's home folder, for `~` and relative paths.
    pub home: &'a str,
    /// Files the assistant wrote in this chat, with the file tools or with
    /// its commands (`written_by`), as resolved paths.
    pub written: &'a [String],
    /// The server's always-run list: exact commands, matched byte for byte.
    pub always_run: &'a [String],
    /// The server runs a Linux kernel (the facts probe's `uname`). A
    /// single-dash `ps` option that selects the environment on other kernels
    /// is harmless here.
    pub linux: bool,
    /// Files only, no commands at all (an FTP server): Auto's checks, which
    /// are for commands in a POSIX shell, have nothing to check there, so
    /// Auto stays Auto for its file changes.
    pub files_only: bool,
}

/// How a call that runs without asking was let through. Shown on its card and
/// in the AI terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    ReadOnly,
    Always,
    Auto,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow(How),
    /// Ask the user. `red` names the always-ask group and what matched it
    /// ("removal: rm -rf"), for a red card.
    Ask { reason: String, red: Option<String> },
    Refuse(String),
}

pub const NOT_CONNECTED: &str = "the server isn't connected; ask the user to open it";
pub const SUGGEST_ONLY: &str = "not allowed at the Suggest level";

/// The longest command the classifier accepts, in characters.
pub const MAX_READ_ONLY_CHARS: usize = 1000;

fn ask(reason: &str) -> Decision {
    Decision::Ask { reason: reason.into(), red: None }
}

/// The decision for each level and kind of call.
pub fn decide(level: Level, call: &Call, ctx: &Ctx) -> Decision {
    if !ctx.connected {
        return Decision::Refuse(NOT_CONNECTED.into());
    }
    if level == Level::Suggest {
        return Decision::Refuse(SUGGEST_ONLY.into());
    }
    // Auto without a POSIX shell behaves like Ask: its rules are
    // POSIX rules.
    let level = if level == Level::Auto && !ctx.posix && !ctx.files_only { Level::Ask } else { level };
    match *call {
        Call::List => Decision::Allow(How::ReadOnly),
        Call::Read { path } => {
            if !sensitive(path, ctx.home, false) {
                Decision::Allow(How::ReadOnly)
            } else if level == Level::Full {
                Decision::Allow(How::Full)
            } else {
                ask("This file may hold secrets")
            }
        }
        Call::Write { path } => match level {
            Level::Full => Decision::Allow(How::Full),
            Level::Auto if sensitive(path, ctx.home, true) => ask("This file may hold secrets"),
            Level::Auto if persistence(path, ctx.home) => ask("This file runs at login, at boot or on a schedule"),
            Level::Auto if kernel_file(path, ctx.home) => ask("This file controls the kernel or a device"),
            Level::Auto => Decision::Allow(How::Auto),
            _ => ask("Changes a file"),
        },
        Call::Run { command } => {
            if level == Level::Full {
                return Decision::Allow(How::Full);
            }
            // Before the always-run list: a red-card command never runs
            // through it.
            if let Some(group) = always_ask(command, ctx) {
                return Decision::Ask { reason: "Always asks below Full access".into(), red: Some(group) };
            }
            // At Ask every command, a command on the list waits too.
            if level != Level::AskAll && ctx.always_run.iter().any(|c| c == command) {
                return Decision::Allow(How::Always);
            }
            match level {
                Level::AskAll => ask("Every command asks at this level"),
                Level::Auto if hides_program(command) => {
                    ask("The program this command runs comes from a variable or a pattern, so it can't be checked")
                }
                Level::Auto => Decision::Allow(How::Auto),
                _ if !ctx.posix => ask("No POSIX shell answered on this server, so every command asks"),
                _ if read_only(command, ctx.home, ctx.linux) => Decision::Allow(How::ReadOnly),
                _ => ask("This command may change the server"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The shell-word parser

/// One word of a command, after quote removal.
#[derive(Debug, Default, Clone)]
struct Word {
    /// The text without quotes and escapes. Expansions such as `$HOME` stay
    /// as written.
    text: String,
    /// The same with parameter expansions (`$NAME`, `${…}`, `$1`) left out,
    /// as the shell expands an unset variable.
    bare: String,
    /// Some part was quoted or escaped.
    quoted: bool,
    /// A `$` outside single quotes, or a backtick.
    dollar: bool,
    /// An unquoted `*`, `?` or `[`.
    glob: bool,
    /// An unquoted `{` or `}`.
    brace: bool,
}

#[derive(Debug, Clone)]
enum Tok {
    Word(Word),
    Op(&'static str),
    /// A here-document's lines.
    Body(String),
    /// A `#` comment, to the end of its line.
    Comment,
}

/// Operators, longest first.
const OPS: [&str; 24] = [
    "&>>", "<<<", "<<-", "||", "|&", "&&", ";;", "&>", "<<", "<&", "<>", "<(", ">>", ">&", ">|", ">(", "|", "&",
    ";", "<", ">", "(", ")", "\n",
];

struct Lexer {
    c: Vec<char>,
    /// For each position, where the next `}` is (for `${…}`), so a long
    /// command without one stays linear.
    close: Vec<usize>,
    i: usize,
    toks: Vec<Tok>,
    w: Word,
    in_word: bool,
    /// After `<<` (false) or `<<-` (true): the next word is a delimiter.
    want_delim: Option<bool>,
    /// Here-documents whose lines start after the next newline.
    docs: Vec<(String, bool)>,
}

impl Lexer {
    fn new(s: &str) -> Lexer {
        let c: Vec<char> = s.chars().collect();
        let mut close = Vec::with_capacity(c.len());
        let mut next = c.len();
        for (k, ch) in c.iter().enumerate().rev() {
            if *ch == '}' {
                next = k;
            }
            close.push(next);
        }
        close.reverse();
        Lexer { c, close, i: 0, toks: Vec::new(), w: Word::default(), in_word: false, want_delim: None, docs: Vec::new() }
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }

    fn push(&mut self, ch: char) {
        self.w.text.push(ch);
        self.w.bare.push(ch);
        self.in_word = true;
    }

    fn flush(&mut self) {
        if !self.in_word {
            return;
        }
        let w = std::mem::take(&mut self.w);
        self.in_word = false;
        if let Some(strip) = self.want_delim.take() {
            self.docs.push((w.text.clone(), strip));
        }
        self.toks.push(Tok::Word(w));
    }

    fn op_at(&self) -> Option<&'static str> {
        OPS.iter().copied().find(|op| op.chars().enumerate().all(|(k, ch)| self.peek(k) == Some(ch)))
    }

    fn op(&mut self, op: &'static str) -> Option<()> {
        let redirect = op.starts_with(['<', '>']) || op.starts_with("&>");
        let fd = self.in_word && !self.w.quoted && !self.w.text.is_empty() && self.w.text.bytes().all(|b| b.is_ascii_digit());
        if redirect && fd {
            // `2>`: the digits are the redirection's file descriptor.
            self.w = Word::default();
            self.in_word = false;
        } else {
            self.flush();
        }
        if self.want_delim.is_some() {
            return None; // `<<` without a delimiter
        }
        self.toks.push(Tok::Op(op));
        if op == "<<" || op == "<<-" {
            self.want_delim = Some(op == "<<-");
        }
        self.i += op.chars().count();
        if op == "\n" {
            self.bodies();
        }
        Some(())
    }

    /// After a newline: the lines of the here-documents opened on that line.
    fn bodies(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.docs) {
            let mut body = String::new();
            while self.i < self.c.len() {
                let start = self.i;
                let mut end = start;
                while self.c.get(end).is_some_and(|c| *c != '\n') {
                    end += 1;
                }
                let line: String = self.c.get(start..end).unwrap_or_default().iter().collect();
                self.i = (end + 1).min(self.c.len());
                let cmp = if strip { line.trim_start_matches('\t') } else { line.as_str() };
                if cmp == delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            self.toks.push(Tok::Body(body));
        }
    }

    /// Inside double quotes, after the opening `"`.
    fn dquote(&mut self) -> Option<()> {
        self.w.quoted = true;
        self.in_word = true;
        loop {
            let ch = self.peek(0)?;
            match ch {
                '"' => {
                    self.i += 1;
                    return Some(());
                }
                '\\' => {
                    let next = self.peek(1)?;
                    if matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                        self.i += 2;
                        if next == '$' || next == '`' {
                            self.w.dollar = true;
                        }
                        if next != '\n' {
                            self.push(next);
                        }
                    } else {
                        self.push('\\');
                        self.i += 1;
                    }
                }
                '$' => self.dollar(true)?,
                '`' => {
                    self.w.dollar = true;
                    self.push('`');
                    self.i += 1;
                }
                _ => {
                    self.push(ch);
                    self.i += 1;
                }
            }
        }
    }

    /// A `$` at the current position. `dq`: inside double quotes.
    fn dollar(&mut self, dq: bool) -> Option<()> {
        self.w.dollar = true;
        self.in_word = true;
        match self.peek(1) {
            Some('\'') if !dq => {
                self.i += 2;
                self.ansi_c()
            }
            Some('"') if !dq => {
                // `$"…"` is a double-quoted string.
                self.i += 2;
                self.dquote()
            }
            Some('{') => {
                let end = self.close.get(self.i + 2).copied().unwrap_or(self.c.len());
                if end >= self.c.len() {
                    self.push('$');
                    self.i += 1;
                } else {
                    for k in self.i..=end {
                        if let Some(&ch) = self.c.get(k) {
                            self.w.text.push(ch);
                        }
                    }
                    self.i = end + 1;
                }
                Some(())
            }
            Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {
                self.w.text.push('$');
                self.i += 1;
                while let Some(ch) = self.peek(0) {
                    if ch != '_' && !ch.is_ascii_alphanumeric() {
                        break;
                    }
                    self.w.text.push(ch);
                    self.i += 1;
                }
                Some(())
            }
            Some(ch) if ch.is_ascii_digit() || "@*#?-$!".contains(ch) => {
                self.w.text.push('$');
                self.w.text.push(ch);
                self.i += 2;
                Some(())
            }
            _ => {
                self.push('$');
                self.i += 1;
                Some(())
            }
        }
    }

    /// Inside `$'…'`, after the opening quote. Escapes are decoded, so
    /// `$'\x72m'` reads as `rm`.
    fn ansi_c(&mut self) -> Option<()> {
        self.w.quoted = true;
        loop {
            let ch = self.peek(0)?;
            self.i += 1;
            match ch {
                '\'' => return Some(()),
                '\\' => {
                    let e = self.peek(0)?;
                    self.i += 1;
                    let out = match e {
                        'a' => Some('\u{7}'),
                        'b' => Some('\u{8}'),
                        'e' | 'E' => Some('\u{1b}'),
                        'f' => Some('\u{c}'),
                        'n' => Some('\n'),
                        'r' => Some('\r'),
                        't' => Some('\t'),
                        'v' => Some('\u{b}'),
                        '\\' | '\'' | '"' | '?' => Some(e),
                        'x' => self.number(16, 2),
                        'u' => self.number(16, 4),
                        'U' => self.number(16, 8),
                        '0'..='7' => {
                            self.i = self.i.saturating_sub(1);
                            self.number(8, 3)
                        }
                        'c' => {
                            let x = self.peek(0)?;
                            self.i += 1;
                            char::from_u32(u32::from(x) & 0x1f)
                        }
                        _ => {
                            self.push('\\');
                            Some(e)
                        }
                    };
                    if let Some(o) = out {
                        self.push(o);
                    }
                }
                _ => self.push(ch),
            }
        }
    }

    /// Up to `max` digits in `radix`, as a character.
    fn number(&mut self, radix: u32, max: usize) -> Option<char> {
        let mut v: u32 = 0;
        let mut n = 0;
        while n < max {
            let Some(d) = self.peek(0).and_then(|c| c.to_digit(radix)) else { break };
            v = v.saturating_mul(radix).saturating_add(d);
            self.i += 1;
            n += 1;
        }
        if n == 0 {
            None
        } else {
            char::from_u32(v)
        }
    }
}

/// Splits a command into words and operators. `None` when it can't be read:
/// an unbalanced quote, a trailing backslash, `<<` without a delimiter.
fn parse(s: &str) -> Option<Vec<Tok>> {
    let mut lx = Lexer::new(s);
    while let Some(ch) = lx.peek(0) {
        match ch {
            '\'' => {
                lx.w.quoted = true;
                lx.in_word = true;
                lx.i += 1;
                loop {
                    let c = lx.peek(0)?;
                    lx.i += 1;
                    if c == '\'' {
                        break;
                    }
                    lx.push(c);
                }
            }
            '"' => {
                lx.i += 1;
                lx.dquote()?;
            }
            '\\' => {
                let next = lx.peek(1)?;
                lx.i += 2;
                if next != '\n' {
                    lx.w.quoted = true;
                    if next == '$' || next == '`' {
                        lx.w.dollar = true;
                    }
                    lx.push(next);
                }
            }
            '$' => lx.dollar(false)?,
            '`' => {
                lx.w.dollar = true;
                lx.push('`');
                lx.i += 1;
            }
            ' ' | '\t' => {
                lx.flush();
                lx.i += 1;
            }
            '#' if !lx.in_word => {
                lx.toks.push(Tok::Comment);
                while lx.peek(0).is_some_and(|c| c != '\n') {
                    lx.i += 1;
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\n' => {
                let op = lx.op_at()?;
                lx.op(op)?;
            }
            '*' | '?' | '[' => {
                lx.w.glob = true;
                lx.push(ch);
                lx.i += 1;
            }
            '{' | '}' => {
                lx.w.brace = true;
                lx.push(ch);
                lx.i += 1;
            }
            _ => {
                lx.push(ch);
                lx.i += 1;
            }
        }
    }
    lx.flush();
    if lx.want_delim.is_some() {
        return None;
    }
    Some(lx.toks)
}

/// The words of a command joined by spaces, operators and here-documents
/// included: the text the always-ask patterns read a second time.
fn join_words(toks: &[Tok]) -> String {
    let mut s = String::new();
    for t in toks {
        let part = match t {
            Tok::Word(w) => w.bare.as_str(),
            Tok::Op(o) => *o,
            Tok::Body(b) => b.as_str(),
            Tok::Comment => continue,
        };
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(part);
    }
    s
}

/// A simple command: its words and its redirections.
#[derive(Default)]
struct Simple<'a> {
    words: Vec<&'a Word>,
    /// The operator and its target, when one follows.
    redirs: Vec<(&'static str, Option<&'a Word>)>,
}

fn is_redirect(op: &str) -> bool {
    matches!(op, "<" | ">" | ">>" | ">|" | "<>" | "<&" | ">&" | "&>" | "&>>" | "<<" | "<<-" | "<<<")
}

/// The redirection writes to the file `target`.
fn writes(op: &str, target: &str) -> bool {
    match op {
        ">" | ">>" | ">|" | "<>" | "&>" | "&>>" => true,
        ">&" => !(target == "-" || target.bytes().all(|b| b.is_ascii_digit())),
        _ => false,
    }
}

/// The simple commands of a command, split at every operator that isn't a
/// redirection (pipes, lists, subshells, process substitution).
fn simples(toks: &[Tok]) -> Vec<Simple<'_>> {
    let mut out = Vec::new();
    let mut cur = Simple::default();
    let mut redir: Option<&'static str> = None;
    for t in toks {
        match t {
            Tok::Word(w) => match redir.take() {
                Some(op) => cur.redirs.push((op, Some(w))),
                None => cur.words.push(w),
            },
            Tok::Op(op) if is_redirect(op) => {
                if let Some(prev) = redir.replace(*op) {
                    cur.redirs.push((prev, None));
                }
            }
            Tok::Op(_) => {
                if let Some(prev) = redir.take() {
                    cur.redirs.push((prev, None));
                }
                out.push(std::mem::take(&mut cur));
            }
            Tok::Body(_) | Tok::Comment => {}
        }
    }
    if let Some(prev) = redir.take() {
        cur.redirs.push((prev, None));
    }
    out.push(cur);
    out.retain(|s| !s.words.is_empty() || !s.redirs.is_empty());
    out
}

/// A word's text, and its text without expansions when that differs.
fn texts(w: &Word) -> Vec<&str> {
    if w.bare == w.text {
        vec![w.text.as_str()]
    } else {
        vec![w.text.as_str(), w.bare.as_str()]
    }
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// `NAME=value`.
fn is_assignment(t: &str) -> bool {
    t.split_once('=').is_some_and(|(n, _)| {
        n.chars().next().is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && n.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

/// A program that runs the command after it, with its short options that
/// take a value.
fn wrapper(base: &str) -> Option<&'static str> {
    Some(match base {
        "sudo" => "ugCDhprtUTR",
        "doas" => "uC",
        "env" => "uCS",
        "exec" => "a",
        "nice" | "ionice" => "ncpt",
        "xargs" => "adEeIiLlnPs",
        "watch" => "nd",
        "time" => "fo",
        "timeout" => "sk",
        "flock" => "wEn",
        "strace" | "ltrace" => "eoOpsuSaPI",
        "run0" | "nohup" | "command" | "builtin" | "stdbuf" | "setsid" | "chrt" | "taskset" | "busybox"
        | "unbuffer" | "chroot" => "",
        _ => return None,
    })
}

/// The index of a simple command's program, past `NAME=value` words and
/// wrappers such as `sudo` and `env`; and whether there was a wrapper.
fn program_index(words: &[&Word]) -> Option<(usize, bool)> {
    let mut i = 0;
    let mut wrapped = false;
    while words.get(i).is_some_and(|w| is_assignment(&w.text)) {
        i += 1;
    }
    loop {
        let base = basename(&words.get(i)?.text).to_ascii_lowercase();
        let Some(valued) = wrapper(&base) else { return Some((i, wrapped)) };
        wrapped = true;
        i += 1;
        while let Some(w) = words.get(i) {
            let t = w.text.as_str();
            if t == "--" {
                i += 1;
                break;
            }
            if base == "env" && is_assignment(t) {
                i += 1;
            } else if let Some(s) = short(t) {
                i += if value_next(s, valued) { 2 } else { 1 };
            } else if t.starts_with("--") {
                i += 1;
            } else {
                break;
            }
        }
        // These take one operand before the command.
        if matches!(base.as_str(), "timeout" | "chrt" | "taskset" | "flock" | "chroot") {
            i += 1;
        }
    }
}

/// The index of the command's program when its name is one of `names`;
/// behind a wrapper (`sudo`, `env`, …), the first word with such a name.
fn find_program(s: &Simple, names: &[&str]) -> Option<usize> {
    let (i, wrapped) = program_index(&s.words)?;
    let named =
        |k: usize| s.words.get(k).is_some_and(|w| names.contains(&basename(&w.text).to_ascii_lowercase().as_str()));
    if named(i) {
        return Some(i);
    }
    if wrapped {
        return (i..s.words.len()).find(|&k| named(k));
    }
    None
}

/// A simple command whose program comes from an expansion, a pattern or
/// braces (`c=rm; $c -rf x`, `/bin/r?`): what runs can't be read off the
/// text, so Auto asks. A command that doesn't parse counts as one.
fn hides_program(command: &str) -> bool {
    let Some(toks) = parse(command) else { return true };
    simples(&toks).iter().any(|s| match program_index(&s.words) {
        Some((i, _)) => s.words.get(i).is_some_and(|w| w.dollar || w.glob || w.brace),
        None => false,
    })
}

/// A shell or an interpreter that runs a script file.
fn interpreter(base: &str) -> bool {
    let versioned = |pre: &str| {
        base.strip_prefix(pre).is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
    };
    matches!(
        base,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" | "mksh" | "yash" | "fish" | "csh" | "tcsh" | "busybox"
            | "perl" | "ruby" | "node" | "nodejs" | "php"
    ) || versioned("python")
        || versioned("pypy")
        || versioned("lua")
}

// ---------------------------------------------------------------------------
// Options

/// `-abc` gives `abc` (not `--x`, not `-`).
fn short(a: &str) -> Option<&str> {
    let s = a.strip_prefix('-')?;
    (!s.is_empty() && !s.starts_with('-')).then_some(s)
}

/// `--name=value` gives `name`.
fn long(a: &str) -> Option<&str> {
    let s = a.strip_prefix("--")?;
    (!s.is_empty()).then(|| s.split_once('=').map_or(s, |(n, _)| n))
}

/// `x` is one of `names`, or a prefix getopt would expand to one.
fn abbrev(x: &str, names: &[&str]) -> bool {
    !x.is_empty() && names.iter().any(|n| n.starts_with(x))
}

/// A letter in `bad` comes before any letter in `valued` (whose value is the
/// rest of the cluster).
fn cluster_has(s: &str, bad: &str, valued: &str) -> bool {
    for c in s.chars() {
        if bad.contains(c) {
            return true;
        }
        if valued.contains(c) {
            return false;
        }
    }
    false
}

/// Every letter is in `flags`, up to one in `valued` (whose value follows).
fn cluster_ok(s: &str, flags: &str, valued: &str) -> bool {
    for c in s.chars() {
        if valued.contains(c) {
            return true;
        }
        if !flags.contains(c) {
            return false;
        }
    }
    true
}

/// The cluster's first letter that takes a value is its last letter: the
/// value is the next word.
fn value_next(s: &str, valued: &str) -> bool {
    s.char_indices().find(|(_, c)| valued.contains(*c)).is_some_and(|(k, c)| k + c.len_utf8() == s.len())
}

/// No option in `bad` (long names, getopt abbreviations included) and no
/// short letter in `bad_short`.
fn options_ok(args: &[&str], bad: &[&str], bad_short: &str, valued: &str) -> bool {
    args.iter().all(|a| match (long(a), short(a)) {
        (Some(l), _) => !abbrev(l, bad),
        (_, Some(s)) => !cluster_has(s, bad_short, valued),
        _ => true,
    })
}

// ---------------------------------------------------------------------------
// Paths

/// The login user's home folder, normalised. An unknown home is `/`.
fn home_dir(home: &str) -> String {
    if home.starts_with('/') {
        normalize(home)
    } else {
        normalize(&format!("/{home}"))
    }
}

/// Lexical normalisation: no `.`, `..` or empty parts; `..` stops at `/`.
fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(c),
        }
    }
    let mut out = String::with_capacity(p.len() + 1);
    for c in &parts {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn join(dir: &str, rel: &str) -> String {
    if dir == "/" {
        format!("/{rel}")
    } else {
        format!("{dir}/{rel}")
    }
}

/// What follows `~`, `$HOME` or `${HOME}` at the start of a path.
fn strip_home(p: &str) -> Option<&str> {
    for pre in ["~", "$HOME", "${HOME}"] {
        if let Some(rest) = p.strip_prefix(pre) {
            if rest.is_empty() {
                return Some("");
            }
            if let Some(r) = rest.strip_prefix('/') {
                return Some(r);
            }
        }
    }
    None
}

/// `p` made absolute against the home folder `h` (already normalised) and
/// normalised. Commands start in the home folder, so relative paths are
/// relative to it.
fn resolve_in(p: &str, h: &str) -> String {
    resolve_at(p, h, h)
}

/// `p` made absolute and normalised: `~` is the home folder `h`, and a
/// relative path starts in `cwd` (both already normalised).
fn resolve_at(p: &str, h: &str, cwd: &str) -> String {
    let abs = if let Some(rest) = strip_home(p) {
        join(h, rest)
    } else if let Some(t) = p.strip_prefix('~') {
        let (user, rest) = t.split_once('/').unwrap_or((t, ""));
        match user {
            "+" => join(cwd, rest),
            "-" => join(h, rest),
            "root" => join("/root", rest),
            _ => join(&format!("/home/{user}"), rest),
        }
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        join(cwd, p)
    };
    normalize(&abs)
}

/// `p` is `dir` or inside it.
fn under(p: &str, dir: &str) -> bool {
    p.strip_prefix(dir).is_some_and(|r| r.is_empty() || r.starts_with('/'))
}

/// `p` is `f`, or `f` with a suffix such as `-`, `.bak` or `.d/…`.
fn named(p: &str, f: &str) -> bool {
    p.strip_prefix(f).is_some_and(|r| !r.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
}

/// Folders whose every file is a secret, wherever they are.
const SECRET_FOLDERS: [&str; 3] = ["/.gnupg/", "/.azure/", "/.config/gcloud/"];

/// The rules that hold anywhere: a `.ssh` folder (public keys aside), a
/// `.gnupg`, `.azure` or `.config/gcloud` folder, key and secret file names,
/// shell and tool history files, and the credential files of known tools.
/// A backslash counts as a separator too, as on Windows (and for MSYS tools
/// there): `C:\Users\u\.aws\credentials` and `app\.env` are names to guard.
/// Taken that way where it is only a character, it can add a card, never
/// remove one.
fn secret_by_name(p: &str) -> bool {
    let lower = p.to_ascii_lowercase().replace('\\', "/");
    let name = lower.rsplit('/').next().unwrap_or_default();
    let public = name.ends_with(".pub");
    let padded = format!("/{lower}/");
    (!public && lower.split('/').any(|c| c == ".ssh"))
        || SECRET_FOLDERS.iter().any(|d| padded.contains(d))
        || [".pem", ".key", ".p12", ".pfx", ".kdbx"].iter().any(|e| name.ends_with(e))
        || (!public && ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"].iter().any(|k| name.starts_with(k)))
        || name == ".env"
        || name.starts_with(".env.")
        || [
            ".netrc", ".pgpass", ".git-credentials", ".my.cnf", ".npmrc", ".pypirc", ".vault-token",
            ".bash_history", ".zsh_history", ".sh_history", ".ash_history", "fish_history", ".mysql_history",
            ".psql_history", ".python_history", ".node_repl_history",
        ]
        .contains(&name)
        || [
            "/.aws/credentials",
            "/.docker/config.json",
            "/.kube/config",
            "/.git/config",
            "/.config/gh/hosts.yml",
            "/.cargo/credentials",
            "/.cargo/credentials.toml",
        ]
        .iter()
        .any(|s| lower.ends_with(s))
}

/// System folders where everything is sensitive: host keys (Dropbear's folder
/// on OpenWrt also holds root's login keys), root's home, the boot files, TLS
/// keys, and VPN and cluster credentials.
const SECRET_PLACES: [&str; 9] = [
    "/etc/ssh",
    "/root",
    "/boot",
    "/etc/ssl/private",
    "/etc/letsencrypt",
    "/etc/wireguard",
    "/etc/kubernetes",
    "/etc/rancher",
    "/etc/dropbear",
];

/// System files that hold secrets, with their backups (`/etc/shadow-`):
/// password hashes, sudo rules, MySQL's maintenance login, and OpenWrt's
/// Wi-Fi and network keys.
const SECRET_FILES: [&str; 6] = [
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/sudoers",
    "/etc/mysql/debian.cnf",
    "/etc/config/wireless",
    "/etc/config/network",
];

/// The rules for system places. Debian keeps copies of the password hashes
/// in `/var/backups`.
fn secret_by_place(p: &str, write: bool) -> bool {
    SECRET_PLACES.iter().any(|d| under(p, d))
        || SECRET_FILES.iter().any(|f| named(p, f))
        || p.strip_prefix("/var/backups/").is_some_and(|r| r.contains("shadow"))
        || proc_secret(p)
        || (write && ["/etc/passwd", "/etc/group"].iter().any(|f| named(p, f)))
}

/// Under `/proc`: the kernel's memory, a process's environment and memory, and
/// its links to its root folder, its working folder and its open and mapped
/// files, which lead to any file (`/proc/self/root/etc/shadow`). Only the name
/// right after the process or thread counts, so `/proc/*/status` stays
/// harmless.
fn proc_secret(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/proc/") else { return false };
    let parts: Vec<&str> = rest.split('/').collect();
    let link = |k: usize| {
        parts.get(k).is_some_and(|c| matches!(*c, "environ" | "mem" | "root" | "cwd" | "fd" | "map_files"))
    };
    rest == "kcore" || link(1) || (parts.get(1) == Some(&"task") && link(3))
}

/// A secret path for a login whose home is `home`. A root login's own home
/// (`/root`) is where its work is, so the place rule for `/root` leaves it
/// out; the name rules (`.ssh`, keys, `.env`) still hold there.
fn secret(p: &str, write: bool, home: &str) -> bool {
    secret_by_name(p) || (secret_by_place(p, write) && !(home == "/root" && under(p, "/root")))
}

/// A sensitive path. `write` adds the paths that are sensitive only
/// to write (`/etc/passwd`, `/etc/group`). `~` and relative paths are taken
/// from `home`; `.` and `..` are normalised.
pub fn sensitive(path: &str, home: &str, write: bool) -> bool {
    let h = home_dir(home);
    secret(&resolve_in(path, &h), write, &h)
}

/// A persistence path: a write there runs later, at login, at boot or
/// on a schedule.
pub fn persistence(path: &str, home: &str) -> bool {
    let h = home_dir(home);
    persists(&resolve_in(path, &h), &h)
}

/// Files in a home folder that run later: the shells' startup and logout
/// files, desktop login files, the environment read at login, and git's
/// settings (`core.fsmonitor` and `core.hooksPath` name programs). Lower
/// case.
const HOME_FILES: [&str; 22] = [
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".cshrc",
    ".tcshrc",
    ".login",
    ".logout",
    ".kshrc",
    ".mkshrc",
    ".xprofile",
    ".xinitrc",
    ".xsession",
    ".xsessionrc",
    ".pam_environment",
    ".gitconfig",
];

/// Folders in a home folder whose files run later, and the ones on `PATH`,
/// where a program runs in place of the system's. Lower case.
const HOME_FOLDERS: [&str; 9] = [
    ".config/fish",
    ".config/systemd",
    ".config/autostart",
    ".config/environment.d",
    ".config/git",
    ".local/share/systemd",
    ".local/bin",
    "bin",
    "library/launchagents",
];

/// System files that run later, or that scripts read as shell code
/// (`os-release`). Lower case.
const SYSTEM_FILES: [&str; 28] = [
    "/etc/profile",
    "/etc/bash.bashrc",
    "/etc/bashrc",
    "/etc/environment",
    "/etc/zshrc",
    "/etc/zprofile",
    "/etc/zshenv",
    "/etc/zlogin",
    "/etc/zlogout",
    "/etc/csh.cshrc",
    "/etc/csh.login",
    "/etc/csh.logout",
    "/etc/ksh.kshrc",
    "/etc/mkshrc",
    "/etc/rc.local",
    "/etc/rc.conf",
    "/etc/rc.conf.local",
    "/etc/inittab",
    "/etc/anacrontab",
    "/etc/firewall.user",
    "/etc/ld.so.preload",
    "/etc/ld.so.conf",
    "/etc/modules",
    "/etc/sysctl.conf",
    "/etc/security/pam_env.conf",
    "/etc/gitconfig",
    "/etc/os-release",
    "/usr/lib/os-release",
];

/// System folders whose files run later: at login, at boot, on a schedule,
/// when a device, a network or a package comes or goes. Then the folders on
/// `PATH`. Lower case.
const SYSTEM_FOLDERS: [&str; 56] = [
    "/etc/profile.d",
    "/etc/zsh",
    "/etc/init.d",
    "/etc/init",
    "/etc/rc.d",
    "/etc/conf.d",
    "/etc/runlevels",
    "/etc/local.d",
    "/etc/uci-defaults",
    "/etc/hotplug.d",
    "/etc/sv",
    "/etc/service",
    "/etc/runit",
    "/usr/local/etc/rc.d",
    "/etc/systemd",
    "/lib/systemd",
    "/usr/lib/systemd",
    "/usr/local/lib/systemd",
    "/run/systemd",
    "/etc/xdg/autostart",
    "/var/spool/cron",
    "/var/spool/at",
    "/etc/periodic",
    "/etc/logrotate.d",
    "/etc/update-motd.d",
    "/etc/pam.d",
    "/etc/ld.so.conf.d",
    "/etc/modprobe.d",
    "/etc/modules-load.d",
    "/etc/sysctl.d",
    "/etc/binfmt.d",
    "/etc/udev/rules.d",
    "/etc/networkmanager/dispatcher.d",
    "/etc/network/if-up.d",
    "/etc/network/if-down.d",
    "/etc/network/if-pre-up.d",
    "/etc/network/if-post-down.d",
    "/etc/ppp/ip-up.d",
    "/etc/apt",
    "/etc/yum.repos.d",
    "/etc/zypp/repos.d",
    "/etc/apk",
    "/etc/opkg",
    "/library/launchagents",
    "/library/launchdaemons",
    "/system/library/launchagents",
    "/system/library/launchdaemons",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/snap/bin",
];

/// Where `p` lies inside a home folder, for each home folder it is in: the
/// login's own (`h`), root's (`/root`, macOS's `/var/root`), and any user's
/// (`/home/<name>`, `/Users/<name>`). Both `canon` and in lower case.
fn in_homes<'a>(p: &'a str, h: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for base in [h, "/root", "/var/root"] {
        let rest = if base == "/" { p.strip_prefix('/') } else { p.strip_prefix(base).and_then(|r| r.strip_prefix('/')) };
        out.extend(rest);
    }
    for top in ["/home/", "/users/"] {
        if let Some((_, rest)) = p.strip_prefix(top).and_then(|r| r.split_once('/')) {
            out.push(rest);
        }
    }
    out
}

/// Letter case doesn't count: macOS's file system ignores it, so
/// `~/.ZSHRC` is `~/.zshrc` there.
fn persists(p: &str, h: &str) -> bool {
    let p = canon(&p.to_ascii_lowercase());
    let p = p.as_str();
    let name = p.rsplit('/').next().unwrap_or_default();
    let in_home = |rel: &str| HOME_FILES.contains(&rel) || HOME_FOLDERS.iter().any(|d| under(rel, d));
    // Python runs `.pth` files and these modules at every start.
    let python = p.split('/').any(|c| c == "site-packages" || c == "dist-packages")
        && (name.ends_with(".pth") || name == "sitecustomize.py" || name == "usercustomize.py");
    in_homes(p, &canon(&h.to_ascii_lowercase())).into_iter().any(in_home)
        || SYSTEM_FILES.contains(&p)
        || SYSTEM_FOLDERS.iter().any(|d| under(p, d))
        || p.starts_with("/etc/cron")
        || p.ends_with("/.git/hooks")
        || p.contains("/.git/hooks/")
        || name.starts_with("authorized_keys")
        || name.starts_with("authorized_principals")
        || python
}

/// A kernel or device file: under `/proc`, `/sys` or `/dev` (but `/dev/shm`,
/// a plain memory file system). Writing one acts on the machine:
/// `/sys/power/state` suspends it, `/proc/sys/kernel/core_pattern` names a
/// program the kernel runs as root.
fn kernel_file(path: &str, home: &str) -> bool {
    let p = resolve_in(path, &home_dir(home));
    (under(&p, "/proc") || under(&p, "/sys") || under(&p, "/dev")) && !under(&p, "/dev/shm")
}

/// The home folder, as commands see it.
struct Home {
    dir: String,
    /// The folder relative paths start in: the home folder, or a folder a
    /// `cd` earlier in the command names.
    cwd: String,
    /// That folder is inside a sensitive folder (`cd /etc/ssh`).
    sensitive: bool,
    /// Glob expansions left for this call, shared by its folders, so huge
    /// commands stay fast.
    budget: Rc<Cell<usize>>,
}

impl Home {
    fn new(home: &str) -> Home {
        let dir = home_dir(home);
        let sensitive = secret(&dir, false, &dir);
        Home { cwd: dir.clone(), dir, sensitive, budget: Rc::new(Cell::new(4096)) }
    }

    /// The same login, in the folder `cwd` (absolute and normalised).
    fn at(&self, cwd: &str) -> Home {
        let sensitive = secret(cwd, false, &self.dir);
        Home { dir: self.dir.clone(), cwd: cwd.to_string(), sensitive, budget: Rc::clone(&self.budget) }
    }

    /// `p` made absolute: `~` is the home folder, a relative path starts in
    /// `cwd`.
    fn resolve(&self, p: &str) -> String {
        resolve_at(p, &self.dir, &self.cwd)
    }
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Names a glob is tried against: where a sensitive or persistence path can
/// hide. `true` marks names that only a glob with some literal text counts for
/// (a bare `*` isn't taken to aim at `id_rsa`).
const SAMPLES: [(&str, bool); 99] = [
    ("shadow", false),
    ("gshadow", false),
    ("sudoers", false),
    ("sudoers.d", false),
    ("ssh", false),
    ("root", false),
    ("boot", false),
    ("passwd", false),
    ("group", false),
    ("etc", false),
    ("home", false),
    ("credentials", false),
    ("config.json", false),
    ("config", false),
    ("authorized_keys", false),
    ("crontab", false),
    ("cron.d", false),
    ("cron", false),
    ("spool", false),
    ("systemd", false),
    ("system", false),
    ("profile", false),
    ("profile.d", false),
    ("init.d", false),
    ("rc.local", false),
    ("ld.so.preload", false),
    ("environment", false),
    ("bash.bashrc", false),
    ("hooks", false),
    ("fish", false),
    ("lib", false),
    ("usr", false),
    ("var", false),
    ("ssl", false),
    ("private", false),
    ("letsencrypt", false),
    ("wireguard", false),
    ("kubernetes", false),
    ("rancher", false),
    ("dropbear", false),
    ("mysql", false),
    ("debian.cnf", false),
    ("wireless", false),
    ("network", false),
    ("backups", false),
    ("dev", false),
    ("proc", false),
    ("task", false),
    ("cwd", false),
    ("fd", false),
    ("map_files", false),
    ("mem", false),
    ("kcore", false),
    ("gh", false),
    ("hosts.yml", false),
    ("gcloud", false),
    ("credentials.toml", false),
    (".ssh", false),
    (".aws", false),
    (".docker", false),
    (".kube", false),
    (".git", false),
    (".config", false),
    (".env", false),
    (".env.local", false),
    (".netrc", false),
    (".pgpass", false),
    (".git-credentials", false),
    (".my.cnf", false),
    (".npmrc", false),
    (".pypirc", false),
    (".gnupg", false),
    (".bash_history", false),
    (".zsh_history", false),
    (".sh_history", false),
    (".mysql_history", false),
    (".psql_history", false),
    (".python_history", false),
    (".node_repl_history", false),
    (".ash_history", false),
    (".vault-token", false),
    (".azure", false),
    (".cargo", false),
    ("environ", false),
    (".bashrc", false),
    (".bash_profile", false),
    (".profile", false),
    (".zshrc", false),
    (".zprofile", false),
    ("id_rsa", true),
    ("id_dsa", true),
    ("id_ecdsa", true),
    ("id_ed25519", true),
    ("fish_history", true),
    ("x.pem", true),
    ("x.key", true),
    ("x.p12", true),
    ("x.pfx", true),
    ("x.kdbx", true),
];

/// Shell pattern matching for one path component: `*`, `?`, `[…]`. A
/// leading `.` must be matched by a literal `.`.
fn glob_match(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut back: Option<(usize, usize)> = None;
    while let Some(&c) = n.get(ni) {
        let step = match p.get(pi) {
            Some('*') => {
                back = Some((pi, ni));
                pi += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match class(p.get(pi..).unwrap_or_default(), c) {
                Some((true, len)) => Some(len),
                Some((false, _)) => None,
                None => (c == '[').then_some(1),
            },
            Some(&x) => (x == c).then_some(1),
            None => None,
        };
        match (step, back) {
            (Some(len), _) => {
                pi += len;
                ni += 1;
            }
            (None, Some((bp, bn))) => {
                pi = bp + 1;
                ni = bn + 1;
                back = Some((bp, bn + 1));
            }
            (None, None) => return false,
        }
    }
    p.get(pi..).unwrap_or_default().iter().all(|&c| c == '*')
}

/// A bracket expression at the start of `p`: whether it matches `c`, and its
/// length. `None` when it isn't closed (then `[` is literal).
fn class(p: &[char], c: char) -> Option<(bool, usize)> {
    let negate = matches!(p.get(1), Some('!') | Some('^'));
    let mut k = if negate { 2 } else { 1 };
    let mut hit = false;
    let mut first = true;
    loop {
        let x = *p.get(k)?;
        if x == ']' && !first {
            return Some((hit != negate, k + 1));
        }
        first = false;
        match (p.get(k + 1), p.get(k + 2)) {
            (Some('-'), Some(&y)) if y != ']' => {
                if x <= c && c <= y {
                    hit = true;
                }
                k += 3;
            }
            _ => {
                if x == c {
                    hit = true;
                }
                k += 1;
            }
        }
    }
}

/// At most this many concrete paths are tried for one path with globs. One
/// `*` alone tries every sample.
const MAX_INSTANCES: usize = 256;

/// A path with globs longer than this isn't tried: it counts as one that may
/// name anything. Real patterns are far shorter, and trying a long one
/// against every rule costs time for each sample.
const MAX_GLOB_PATH: usize = 1024;

/// Concrete paths a path with globs may stand for. `None` when there are too
/// many to try, the path is too long, or the call's budget is spent.
fn glob_instances(p: &str, home: &Home) -> Option<Vec<String>> {
    if p.len() > MAX_GLOB_PATH {
        return None;
    }
    let mut acc: Vec<String> = vec![String::new()];
    for comp in p.split('/').filter(|c| !c.is_empty()) {
        let names: Vec<&str> = if has_glob(comp) {
            let left = home.budget.get().checked_sub(1)?;
            home.budget.set(left);
            let mut v = vec!["x"];
            // A name longer than 255 bytes can't exist.
            if comp.len() <= 255 {
                let wild = comp.chars().all(|c| c == '*' || c == '?');
                for (n, literal) in SAMPLES {
                    if !(literal && wild) && glob_match(comp, n) {
                        v.push(n);
                    }
                }
                let cwd = if home.cwd == home.dir { "" } else { home.cwd.as_str() };
                for n in home.dir.split('/').chain(cwd.split('/')).filter(|c| !c.is_empty()) {
                    if glob_match(comp, n) {
                        v.push(n);
                    }
                }
            }
            v
        } else {
            vec![comp]
        };
        if acc.len().saturating_mul(names.len()) > MAX_INSTANCES {
            return None;
        }
        if let [n] = names.as_slice() {
            // One name: each path grows in place, so a long path stays cheap.
            for a in &mut acc {
                a.push('/');
                a.push_str(n);
            }
            continue;
        }
        let mut next = Vec::with_capacity(acc.len() * names.len());
        for a in &acc {
            for n in &names {
                next.push(format!("{a}/{n}"));
            }
        }
        acc = next;
    }
    Some(acc.into_iter().map(|a| if a.is_empty() { "/".to_string() } else { a }).collect())
}

/// The longest path Linux opens (`PATH_MAX`); longer words name nothing.
const PATH_MAX: usize = 4096;

/// A word that may be a path names a sensitive one.
fn names_secret(cand: &str, home: &Home, write: bool) -> bool {
    if cand.len() > PATH_MAX {
        return false;
    }
    let plain = !cand.contains('/') && !cand.starts_with(['~', '$']);
    if plain && home.sensitive {
        // A root login: every plain word would be inside /root, so a plain
        // word is checked by its name only.
        if !has_glob(cand) {
            return secret_by_name(cand);
        }
        let wild = cand.chars().all(|c| c == '*' || c == '?');
        return SAMPLES.iter().any(|&(n, literal)| !(literal && wild) && glob_match(cand, n) && secret_by_name(n));
    }
    let p = home.resolve(cand);
    if has_glob(&p) {
        glob_instances(&p, home).is_none_or(|v| v.iter().any(|x| secret(x, write, &home.dir)))
    } else {
        secret(&p, write, &home.dir)
    }
}

/// A word that may be a path names a persistence path.
fn names_persistence(cand: &str, home: &Home) -> bool {
    if cand.len() > PATH_MAX {
        return false;
    }
    let p = home.resolve(cand);
    if has_glob(&p) {
        glob_instances(&p, home).is_none_or(|v| v.iter().any(|x| persists(x, &home.dir)))
    } else {
        persists(&p, &home.dir)
    }
}

/// A word that may be a path names a device: a path inside `/dev` other than
/// `/dev/null`. Opening one can act on the hardware (opening `/dev/watchdog`
/// arms it).
fn names_device(cand: &str, home: &Home) -> bool {
    if cand.len() > PATH_MAX {
        return false;
    }
    let device = |x: &str| x != "/dev/null" && x.starts_with("/dev/");
    let p = home.resolve(cand);
    if has_glob(&p) {
        glob_instances(&p, home).is_none_or(|v| v.iter().any(|x| device(x)))
    } else {
        device(&p)
    }
}

/// At most this many parts of one word are checked as paths.
const MAX_CANDIDATES: usize = 64;

fn add(out: &mut Vec<String>, s: &str) {
    if !s.is_empty() && out.len() < MAX_CANDIDATES && !out.iter().any(|o| o == s) {
        out.push(s.to_string());
    }
}

/// The parts of a word that may be paths: the word, what follows a `=`, an
/// option's text from its first `/` or `~` (`-f/etc/x`), the parts between
/// `:`, and its brace expansions.
fn candidates(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    let forms = if word.contains('{') && word.len() <= 1024 { braces(word) } else { vec![word.to_string()] };
    for w in &forms {
        let w = w.as_str();
        if w.starts_with('-') {
            if let Some((_, v)) = w.split_once('=') {
                add(&mut out, v);
            }
            if let Some(k) = w.find(['/', '~']) {
                add(&mut out, w.get(k..).unwrap_or_default());
            }
        } else {
            add(&mut out, w);
            if let Some((_, v)) = w.split_once('=') {
                add(&mut out, v);
            }
        }
        if w.contains(':') {
            for part in w.split(':').take(MAX_CANDIDATES) {
                add(&mut out, part);
            }
        }
    }
    out
}

/// Brace expansion (`{a,b}`), at most 32 results.
fn braces(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![word.to_string()];
    while let Some(w) = todo.pop() {
        if out.len() + todo.len() >= 32 {
            out.push(w);
            continue;
        }
        match brace_split(&w) {
            Some((pre, alts, post)) => {
                for a in alts.iter().take(32).rev() {
                    todo.push(format!("{pre}{a}{post}"));
                }
            }
            None => out.push(w),
        }
    }
    out
}

/// The first `{…}` with a comma at its top level: the text before it, the
/// choices, the text after it.
fn brace_split(w: &str) -> Option<(&str, Vec<&str>, &str)> {
    let b = w.as_bytes();
    let mut from = 0;
    while let Some(off) = b.get(from..)?.iter().position(|&c| c == b'{') {
        let open = from + off;
        let mut depth = 0usize;
        let mut cuts = vec![open];
        let mut close = None;
        for (k, &c) in b.iter().enumerate().skip(open) {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(k);
                        break;
                    }
                }
                b',' if depth == 1 => cuts.push(k),
                _ => {}
            }
        }
        let close = close?;
        if cuts.len() > 1 {
            cuts.push(close);
            let alts = cuts
                .windows(2)
                .filter_map(|pair| match pair {
                    [a, b] => w.get(a + 1..*b),
                    _ => None,
                })
                .collect();
            return Some((w.get(..open)?, alts, w.get(close + 1..)?));
        }
        from = open + 1;
    }
    None
}

/// The word names a sensitive path.
fn word_secret(w: &Word, home: &Home, write: bool) -> bool {
    texts(w).iter().any(|t| candidates(t).iter().any(|c| names_secret(c, home, write)))
}

/// The word names a device (`names_device`).
fn word_device(w: &Word, home: &Home) -> bool {
    texts(w).iter().any(|t| candidates(t).iter().any(|c| names_device(c, home)))
}

/// Folders that hold secret files wherever they are: the credential folders
/// of known tools, and git folders (`.git/config` can hold a token).
const CREDENTIAL_FOLDERS: [&str; 9] =
    ["/.ssh/", "/.gnupg/", "/.azure/", "/.config/gcloud/", "/.config/gh/", "/.aws/", "/.docker/", "/.kube/", "/.git/"];

/// The folder, resolved, is a credential or git folder or inside one, or is
/// under `/proc`.
fn secret_folder(p: &str) -> bool {
    let padded = format!("/{}/", p.to_ascii_lowercase());
    under(p, "/proc") || CREDENTIAL_FOLDERS.iter().any(|d| padded.contains(d))
}

// ---------------------------------------------------------------------------
// The read-only classifier

/// Control, invisible and bidi characters, and blanks other than space and
/// tab.
fn hidden(c: char) -> bool {
    (c.is_control() && c != '\t')
        || (c.is_whitespace() && c != ' ' && c != '\t')
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// A letter or mark of the Arabic script, which Persian, Urdu and Kurdish
/// are written in.
fn arabic_letter(c: char) -> bool {
    matches!(
        c,
        '\u{0610}'..='\u{061A}'
            | '\u{0620}'..='\u{065F}'
            | '\u{066E}'..='\u{06D3}'
            | '\u{06D5}'..='\u{06DC}'
            | '\u{06DF}'..='\u{06E8}'
            | '\u{06EA}'..='\u{06EF}'
            | '\u{06FA}'..='\u{06FC}'
            | '\u{06FF}'
            | '\u{0750}'..='\u{077F}'
            | '\u{0870}'..='\u{0887}'
            | '\u{0889}'..='\u{088E}'
            | '\u{0898}'..='\u{08E1}'
            | '\u{08E3}'..='\u{08FF}'
            | '\u{FB50}'..='\u{FBB1}'
            | '\u{FBD3}'..='\u{FD3D}'
            | '\u{FD50}'..='\u{FD8F}'
            | '\u{FD92}'..='\u{FDC7}'
            | '\u{FDF0}'..='\u{FDFB}'
            | '\u{FE70}'..='\u{FE74}'
            | '\u{FE76}'..='\u{FEFC}'
    )
}

/// Whether the character at `i` is hidden. The zero-width non-joiner between
/// two Arabic-script letters is the Persian half-space, part of how a word is
/// spelled, so there it isn't.
fn hidden_at(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    if c == '\u{200C}' && i > 0 && chars.get(i + 1).is_some_and(|n| arabic_letter(*n)) && arabic_letter(chars[i - 1]) {
        return false;
    }
    hidden(c)
}

fn has_hidden(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    (0..chars.len()).any(|i| hidden_at(&chars, i))
}

/// The read-only classifier. True only when the command is proven
/// read-only; anything it can't parse or doesn't know is false.
pub fn read_only(command: &str, home: &str, linux: bool) -> bool {
    if command.len() > 4 * MAX_READ_ONLY_CHARS || command.chars().count() > MAX_READ_ONLY_CHARS {
        return false;
    }
    if command.contains('`') || has_hidden(command) {
        return false;
    }
    let Some(toks) = parse(command) else { return false };
    let home = Home::new(home);
    let mut stages: Vec<Vec<&Word>> = vec![Vec::new()];
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Word(w) => {
                if let Some(s) = stages.last_mut() {
                    s.push(w);
                }
            }
            Tok::Op("|") => stages.push(Vec::new()),
            Tok::Op(op) if discards(op, toks.get(i + 1)) => i += 1,
            _ => return false,
        }
        i += 1;
    }
    stages.iter().all(|s| stage_read_only(s, &home, linux))
}

/// A redirection that writes nothing: output to (or input from) `/dev/null`,
/// or one stream joined to another (`2>&1`). Models add these a lot.
fn discards(op: &str, next: Option<&Tok>) -> bool {
    let Some(Tok::Word(w)) = next else { return false };
    if w.dollar || w.brace || w.glob {
        return false;
    }
    match op {
        ">" | ">>" | ">|" | "&>" | "&>>" | "<" => w.text == "/dev/null",
        ">&" => !w.text.is_empty() && w.text.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

/// One stage of a pipeline: an allowed program, named bare, in an allowed
/// form, with no expansion and no argument that names a sensitive path or a
/// device.
fn stage_read_only(words: &[&Word], home: &Home, linux: bool) -> bool {
    let Some((prog, args)) = words.split_first() else { return false };
    if prog.quoted || prog.glob || words.iter().any(|w| w.dollar || w.brace) {
        return false;
    }
    // A pattern can expand to one of `find`'s actions (`-exe[c]`), or to a
    // second file, which `uniq` writes.
    if matches!(prog.text.as_str(), "find" | "uniq") && args.iter().any(|w| w.glob) {
        return false;
    }
    // A device among the arguments counts only for a program that opens it.
    let opens = !NAME_ONLY.contains(&prog.text.as_str());
    if args.iter().any(|w| word_secret(w, home, false) || (opens && word_device(w, home))) {
        return false;
    }
    let args: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
    form_ok(&prog.text, &args, home, linux)
}

/// Programs that look only at a path's name or its metadata and never open
/// it, so a device among their arguments reads nothing from it.
const NAME_ONLY: [&str; 6] = ["ls", "stat", "df", "du", "lsblk", "findmnt"];

/// `prog` with `args` is one of the read-only forms.
fn form_ok(prog: &str, args: &[&str], home: &Home, linux: bool) -> bool {
    match prog {
        "ls" | "stat" | "df" | "free" | "uptime" | "uname" | "whoami" | "id" | "groups" | "pwd" | "which"
        | "echo" | "printf" | "cat" | "head" | "cut" | "tr" | "column" | "nl" | "pgrep" | "netstat" | "lsblk"
        | "findmnt" | "lscpu" => true,
        // `--files0-from` reads the files listed in another file or on stdin,
        // whose names no check sees.
        "wc" | "du" => options_ok(args, &["files0-from"], "", ""),
        "ps" => ps_ok(args, linux),
        "hostname" | "mount" => args.is_empty(),
        // `file -C` writes a compiled magic file.
        "file" => options_ok(args, &["compile"], "C", "efFmP"),
        "date" => date_ok(args),
        "tail" => tail_ok(args),
        "grep" | "egrep" | "fgrep" => grep_ok(args),
        // `-o` writes a file, `--compress-program` runs one, and
        // `--files0-from` and `--random-source` read files no check sees.
        "sort" => options_ok(args, &["output", "compress-program", "files0-from", "random-source"], "o", "ktTS"),
        "uniq" => uniq_ok(args),
        "top" => top_ok(args),
        // `ss -K` closes sockets; `-D` writes a file.
        "ss" => options_ok(args, &["kill", "diag"], "KD", "fAFN"),
        "ip" => ip_ok(args),
        "systemctl" => systemctl_ok(args),
        "journalctl" => journalctl_ok(args),
        "docker" => docker_ok(args),
        "find" => find_ok(args, home),
        "dpkg" => {
            matches!(args.first(), Some(&"-l") | Some(&"--list")) && args.iter().skip(1).all(|a| !a.starts_with('-'))
        }
        "rpm" => args.first() == Some(&"-qa") && args.iter().skip(1).all(|a| *a == "--last" || !a.starts_with('-')),
        "apt" => {
            args.first() == Some(&"list")
                && args.contains(&"--installed")
                && args.iter().skip(1).all(|a| *a == "--installed" || !a.starts_with('-'))
        }
        "apk" => args.first() == Some(&"info") && args.iter().skip(1).all(|a| apk_info_arg(a)),
        _ => false,
    }
}

/// `ps` must only list processes, never print their environment. A BSD-style
/// word (no leading `-`) with an `e`, such as `ps axeww` or `ps e`, prints it
/// on every `ps`. A `-E` does on the BSD and macOS families, and a
/// single-dash `-e` does on FreeBSD, OpenBSD and NetBSD, while on Linux `-e`
/// only selects all processes; so `E` is refused anywhere, and an `e` in a
/// single-dash cluster on any kernel but Linux. The value word after an option that takes one (`-o
/// pid,etime`, `-u deploy`) is skipped, not read as BSD options.
fn ps_ok(args: &[&str], linux: bool) -> bool {
    // Single-dash options whose last letter takes the next word as its value.
    const VALUED_SHORT: &str = "oOpqsuUgGCtk";
    // Long options (written without `=`) whose value is the next word.
    const VALUED_LONG: [&str; 15] = [
        "format", "sort", "pid", "ppid", "user", "User", "group", "Group", "tty", "sid", "cols", "columns",
        "width", "lines", "rows",
    ];
    if args.iter().any(|a| a.contains('E')) {
        return false;
    }
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if let Some(l) = long(a) {
            if !a.contains('=') && VALUED_LONG.contains(&l) {
                i += 1;
            }
            continue;
        }
        if let Some(s) = short(a) {
            // `linux` is the server's own answer. A server that lies about it
            // only shows the environments of its own processes.
            if !linux && s.contains('e') {
                return false;
            }
            if value_next(s, VALUED_SHORT) {
                i += 1;
            }
            continue;
        }
        if a.contains('e') {
            return false;
        }
    }
    true
}

/// `date` that only prints: no `-s`/`--set`, and no operand other than
/// `+FORMAT` (a bare operand sets the clock).
fn date_ok(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if a == "--" {
            return args.get(i..).unwrap_or_default().iter().all(|x| x.starts_with('+'));
        }
        if a.starts_with('+') {
            continue;
        }
        if let Some(l) = long(a) {
            if abbrev(l, &["set"]) {
                return false;
            }
            if !a.contains('=') && l.len() >= 2 && abbrev(l, &["date", "file", "reference", "rfc-3339"]) {
                i += 1;
            }
            continue;
        }
        let Some(s) = short(a) else { return false };
        for (k, c) in s.char_indices() {
            match c {
                'u' | 'R' => {}
                'd' | 'f' | 'r' => {
                    if k + 1 == s.len() {
                        i += 1;
                    }
                    break;
                }
                'I' => break,
                _ => return false,
            }
        }
    }
    true
}

/// `tail` without following: no `-f`, `-F`, `--follow`, `-5f` or `+5f`.
fn tail_ok(args: &[&str]) -> bool {
    args.iter().all(|a| match (long(a), short(a)) {
        (Some(l), _) => !abbrev(l, &["follow"]),
        (_, Some(s)) => !cluster_has(s, "fF", "ncs"),
        _ => !(a.starts_with('+') && a.contains(['f', 'F'])),
    })
}

/// Long `grep` options that take a value.
const GREP_VALUED: [&str; 15] = [
    "regexp",
    "file",
    "max-count",
    "label",
    "after-context",
    "before-context",
    "context",
    "binary-files",
    "devices",
    "directories",
    "exclude",
    "exclude-from",
    "exclude-dir",
    "include",
    "group-separator",
];

/// `grep`, but never a recursive one: it reads every file below its folders,
/// and any project folder can hold `.env`, key or `.git/config` files.
fn grep_ok(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if a == "--" {
            break;
        }
        if let Some(l) = long(a) {
            if abbrev(l, &["recursive", "dereference-recursive"]) {
                return false;
            }
            let value = match a.split_once('=') {
                Some((_, v)) => Some(v),
                None if abbrev(l, &GREP_VALUED) => {
                    i += 1;
                    args.get(i - 1).copied()
                }
                None => None,
            };
            if abbrev(l, &["directories"]) && value.is_some_and(|v| v.starts_with("rec")) {
                return false;
            }
            continue;
        }
        if let Some(s) = short(a) {
            for (k, c) in s.char_indices() {
                match c {
                    'r' | 'R' => return false,
                    'e' | 'f' | 'm' | 'A' | 'B' | 'C' | 'd' | 'D' => {
                        let rest = s.get(k + 1..).unwrap_or_default();
                        let value = if rest.is_empty() {
                            i += 1;
                            args.get(i - 1).copied()
                        } else {
                            Some(rest)
                        };
                        if c == 'd' && value.is_some_and(|v| v.starts_with("rec")) {
                            return false;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    true
}

/// `uniq` with at most one operand (a second one is a file it writes).
fn uniq_ok(args: &[&str]) -> bool {
    uniq_operands(args).len() <= 1
}

/// Where `uniq`'s operands (its files) are in `args`.
fn uniq_operands(args: &[&str]) -> Vec<usize> {
    let mut operands = Vec::new();
    let mut options = true;
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        let at = i;
        i += 1;
        if options && a == "--" {
            options = false;
            continue;
        }
        if options {
            if let Some(l) = long(a) {
                if !a.contains('=') && abbrev(l, &["skip-fields", "skip-chars", "check-chars"]) {
                    i += 1;
                }
                continue;
            }
            if let Some(s) = short(a) {
                if value_next(s, "fsw") {
                    i += 1;
                }
                continue;
            }
        }
        operands.push(at);
    }
    operands
}

/// `find` that only lists: no action that runs, deletes or writes, and no
/// starting point in a credential or git folder or under `/proc`, nor
/// starting points read from a file (`-files0-from`).
fn find_ok(args: &[&str], home: &Home) -> bool {
    let bad = args.iter().any(|a| {
        matches!(*a, "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls" | "-files0-from")
            || a.starts_with("-fprint")
    });
    !bad && find_starts(args).iter().all(|p| !secret_folder(&home.resolve(p)))
}

/// `find`'s starting points: the words before its expression, past the
/// options that come first (`-L`, `-D opts`, `-O3`, BSD's `-x`), and BSD's
/// `-f path`. None means `.`.
fn find_starts<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut starts: Vec<&'a str> = Vec::new();
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        match a {
            "-H" | "-L" | "-P" | "-E" | "-X" | "-d" | "-s" | "-x" | "--" => {}
            "-D" => i += 1,
            "-f" => {
                starts.extend(args.get(i));
                i += 1;
            }
            _ if a.starts_with("-O") => {}
            _ if a.starts_with('-') || matches!(a, "(" | ")" | "!" | ",") => break,
            _ => starts.push(a),
        }
    }
    if starts.is_empty() {
        starts.push(".");
    }
    starts
}

/// `top -b -n 1` only, however its options are spelled (`-bn1`).
fn top_ok(args: &[&str]) -> bool {
    let (mut batch, mut once) = (false, false);
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        let Some(s) = short(a) else { return false };
        for (k, c) in s.char_indices() {
            match c {
                'b' => batch = true,
                'n' => {
                    let rest = s.get(k + 1..).unwrap_or_default();
                    let value = if rest.is_empty() {
                        i += 1;
                        args.get(i - 1).copied()
                    } else {
                        Some(rest)
                    };
                    once = value == Some("1");
                    break;
                }
                _ => return false,
            }
        }
    }
    batch && once
}

/// `ip [options] addr|route|link [show|list …]`. Its `-batch` runs commands
/// from a file.
fn ip_ok(args: &[&str]) -> bool {
    const FLAGS: [&str; 33] = [
        "4", "6", "0", "br", "brief", "j", "json", "p", "pretty", "s", "stats", "statistics", "d", "details", "c",
        "color", "o", "oneline", "h", "human", "human-readable", "iec", "t", "timestamp", "ts", "tshort", "r",
        "resolve", "N", "Numeric", "a", "all", "colour",
    ];
    const VALUED: [&str; 8] = ["f", "family", "n", "netns", "l", "loops", "rc", "rcvbuf"];
    let mut i = 0;
    let object = loop {
        let Some(&a) = args.get(i) else { return false };
        i += 1;
        let Some(o) = a.strip_prefix('-') else { break a };
        let o = o.strip_prefix('-').unwrap_or(o);
        let name = o.split_once('=').map_or(o, |(n, _)| n);
        if FLAGS.contains(&name) {
            continue;
        }
        if VALUED.contains(&name) {
            if !o.contains('=') {
                i += 1;
            }
            continue;
        }
        return false;
    };
    if object.is_empty() || !["address", "route", "link"].iter().any(|o| o.starts_with(object)) {
        return false;
    }
    match args.get(i) {
        None => true,
        Some(v) => ["show", "sh", "list", "lst", "ls"].contains(v),
    }
}

/// `systemctl` with a read-only verb and known options (`-H` reaches another
/// host).
fn systemctl_ok(args: &[&str]) -> bool {
    const VERBS: [&str; 9] =
        ["status", "is-active", "is-enabled", "is-failed", "list-units", "list-unit-files", "list-timers", "show", "cat"];
    const FLAGS: [&str; 19] = [
        "no-pager",
        "full",
        "all",
        "user",
        "system",
        "quiet",
        "plain",
        "no-legend",
        "value",
        "recursive",
        "reverse",
        "after",
        "before",
        "failed",
        "with-dependencies",
        "show-types",
        "no-ask-password",
        "help",
        "version",
    ];
    const VALUED: [&str; 7] = ["type", "state", "property", "output", "lines", "legend", "timestamp"];
    let mut verb = false;
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if let Some(l) = long(a) {
            if VALUED.contains(&l) {
                if !a.contains('=') {
                    i += 1;
                }
                continue;
            }
            if FLAGS.contains(&l) && !a.contains('=') {
                continue;
            }
            return false;
        }
        if let Some(s) = short(a) {
            if !cluster_ok(s, "laqrh", "tnopP") {
                return false;
            }
            if value_next(s, "tnopP") {
                i += 1;
            }
            continue;
        }
        if !verb {
            if !VERBS.contains(&a) {
                return false;
            }
            verb = true;
        }
    }
    true
}

/// `journalctl` without the options that follow, rotate, vacuum, flush or
/// write: getopt abbreviations (`--fol`) and short clusters (`-fu`) included.
fn journalctl_ok(args: &[&str]) -> bool {
    const BAD: [&str; 12] = [
        "follow",
        "rotate",
        "vacuum-size",
        "vacuum-files",
        "vacuum-time",
        "flush",
        "sync",
        "relinquish-var",
        "smart-relinquish-var",
        "setup-keys",
        "update-catalog",
        "cursor-file",
    ];
    const VALUED: [&str; 23] = [
        "since",
        "until",
        "cursor",
        "after-cursor",
        "unit",
        "user-unit",
        "identifier",
        "exclude-identifier",
        "priority",
        "facility",
        "grep",
        "output",
        "output-fields",
        "field",
        "directory",
        "file",
        "root",
        "image",
        "image-policy",
        "machine",
        "namespace",
        "verify-key",
        "interval",
    ];
    let num = |w: &str| {
        let t = w.trim_start_matches(['+', '-']);
        !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())
    };
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if let Some(l) = long(a) {
            // `--cursor` is an option of its own, not short for `--cursor-file`.
            if l != "cursor" && abbrev(l, &BAD) {
                return false;
            }
            if !a.contains('=') {
                if abbrev(l, &VALUED) {
                    i += 1;
                } else if abbrev(l, &["lines", "boot"]) && args.get(i).is_some_and(|n| num(n)) {
                    i += 1;
                }
            }
            continue;
        }
        if num(a) {
            continue;
        }
        let Some(s) = short(a) else { continue };
        for (k, c) in s.char_indices() {
            if "kerxaqlmNh".contains(c) {
                continue;
            }
            let last = k + 1 == s.len();
            if "SUcutTpgoFDMiI".contains(c) {
                if last {
                    i += 1;
                }
            } else if "bn".contains(c) {
                // Their value is optional: taken only when it is a number.
                if last && args.get(i).is_some_and(|n| num(n)) {
                    i += 1;
                }
            } else {
                return false;
            }
            break;
        }
    }
    true
}

/// `docker ps|images|version|info`, `logs` without following,
/// `stats --no-stream`, `network ls`, `volume ls`; never another daemon.
/// `inspect` prints a container's environment, so it asks.
fn docker_ok(args: &[&str]) -> bool {
    let Some((&sub, rest)) = args.split_first() else { return false };
    let remote = rest.iter().any(|a| {
        short(a).is_some_and(|s| s.contains('H')) || long(a).is_some_and(|l| abbrev(l, &["host", "context", "config"]))
    });
    !remote
        && match sub {
            "ps" | "images" | "version" | "info" => true,
            "logs" => options_ok(rest, &["follow"], "f", "n"),
            "stats" => rest.contains(&"--no-stream") || rest.contains(&"--no-stream=true"),
            "network" | "volume" => matches!(rest.first(), Some(&"ls") | Some(&"list")),
            _ => false,
        }
}

/// An argument of `apk info`: a name, or one of its own options (not the
/// global ones, such as `-U`, which fetch from the network).
fn apk_info_arg(a: &str) -> bool {
    const LONG: [&str; 17] = [
        "contents",
        "installed",
        "who-owns",
        "depends",
        "provides",
        "rdepends",
        "replaces",
        "install-if",
        "rinstall-if",
        "size",
        "webpage",
        "description",
        "license",
        "triggers",
        "all",
        "verbose",
        "quiet",
    ];
    match (long(a), short(a)) {
        (Some(l), _) => LONG.contains(&l) && !a.contains('='),
        (_, Some(s)) => s.chars().all(|c| "LeWRPriIswdtavq".contains(c)),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Always-ask groups

const PRIVILEGE: &str = "privilege";
const REMOVAL: &str = "removal";
const DISKS: &str = "disks";
const POWER: &str = "power";
const REMOTE: &str = "remote access, firewall";
const ACCOUNTS: &str = "accounts";
const PERMISSIONS: &str = "mass permission changes";
const SCHEDULED: &str = "scheduled jobs";
const NETWORK: &str = "network, data leaving";
const OTHER_CODE: &str = "running other code";
const HIDDEN_CODE: &str = "hidden code";
const PERSISTENCE: &str = "persistence";
const FORK_BOMB: &str = "fork bomb";
const SENSITIVE: &str = "sensitive paths";
const OWN_FILES: &str = "the assistant's own files";

const SHELLS: &str = "sh|bash|zsh|dash|ksh|ash|mksh|yash|fish|csh|tcsh";
const INTERPRETERS: &str = "python[0-9.]*|pypy[0-9.]*|perl|ruby|node|nodejs|php|lua[0-9.]*";
const WRAPPERS: &str = "sudo|doas|run0|env|nohup|exec|command|builtin|time|nice|ionice|stdbuf|timeout|setsid|xargs";
const SERVICE_TOOLS: &str = r"systemctl|service|rc-service|rc-update|sv|initctl|update-rc\.d|chkconfig";
const SSH_VERBS: &str =
    "stop|restart|try-restart|reload-or-restart|try-reload-or-restart|condrestart|force-reload|reload|disable|mask|kill|del|delete|off|down";
/// Services the user reaches the server through: SSH servers (Dropbear on
/// OpenWrt) and the network itself.
const REMOTE_SERVICES: &str = "sshd?|dropbear|networking|network|networkmanager|systemd-networkd|wpa_supplicant";
/// npm's verbs (and their aliases) that install, update or remove.
const NPM_VERBS: &str =
    "i|in|ins|inst|insta|instal|install|isnt|isnta|isntal|isntall|add|un|unlink|uninstall|remove|rm|r|up|update|upgrade|udpate|link|ln";
const NPM_GLOBAL: &str = "-g|--global|--location[= ]global";

/// Tools whose arguments name services, not programs.
const SERVICE_PROGRAMS: [&str; 9] =
    ["systemctl", "service", "journalctl", "rc-service", "rc-update", "sv", "initctl", "update-rc.d", "chkconfig"];

/// Tools that only search for their arguments (`grep sudo auth.log`,
/// `pgrep ssh`), never run them.
const SEARCH_PROGRAMS: [&str; 8] = ["grep", "egrep", "fgrep", "pgrep", "pidof", "which", "whereis", "journalctl"];

/// Tools that only print their arguments (`echo "meet at 5"`). What they print
/// runs only through a pipe or a file, and the other rules look at those.
const PRINT_PROGRAMS: [&str; 2] = ["echo", "printf"];

/// One pattern, matched on lower-case text.
struct Pat {
    re: Regex,
    /// A list of program names: a name that is only part of a path, a file
    /// name or an option doesn't count.
    words: bool,
    /// Nor a service named to a service manager (`systemctl status ssh`).
    units: bool,
    /// Shown instead of the matched text.
    label: Option<&'static str>,
    /// The pattern has a `show` group: only that part is shown.
    part: bool,
}

/// `\b` in these patterns is the ASCII word boundary, which keeps the regex
/// engine on its fast path whatever the text holds.
fn pat(p: &str, words: bool, units: bool, label: Option<&'static str>) -> Pat {
    let re = Regex::new(&p.replace(r"\b", r"(?-u:\b)")).expect("the always-ask patterns are constants");
    let part = re.capture_names().any(|n| n == Some("show"));
    Pat { re, words, units, label, part }
}

fn text(p: &str) -> Pat {
    pat(p, false, false, None)
}

fn words(p: &str) -> Pat {
    pat(p, true, false, None)
}

fn labeled(p: &str, label: &'static str) -> Pat {
    pat(p, false, false, Some(label))
}

struct Group {
    name: &'static str,
    pats: Vec<Pat>,
}

impl Group {
    /// The first match in `text` (`lower` is the same text in lower case).
    fn find(&self, text: &str, lower: &str) -> Option<String> {
        for p in &self.pats {
            for m in p.re.find_iter(lower) {
                if p.words && excluded(lower.as_bytes(), m.start(), m.end(), p.units) {
                    continue;
                }
                if let Some(l) = p.label {
                    return Some(l.to_string());
                }
                let range = match p.part {
                    true => p.re.captures_at(lower, m.start()).and_then(|c| c.name("show")).map_or(m.range(), |g| g.range()),
                    false => m.range(),
                };
                return Some(show(text.get(range).unwrap_or_default()));
            }
        }
        None
    }
}

/// The groups, in a fixed order. The groups without
/// patterns are checked on the parsed command (`structured`).
static GROUPS: LazyLock<Vec<Group>> = LazyLock::new(|| {
    vec![
        Group { name: PRIVILEGE, pats: vec![words(r"\b(sudo|sudoedit|su|doas|pkexec|run0)\b")] },
        Group {
            name: REMOVAL,
            // Every `rm`, `unlink` and `rmdir` is found on the parsed command.
            pats: vec![
                text(r"\brm\b[^\n;&|]*?\s(-[a-z]*[rf][a-z]*|--recursive|--force)\b"),
                labeled(r"(?s)\bfind\b.*-delete\b", "find -delete"),
                words(r"\b(shred|wipefs|truncate|dropdb|dropuser)\b"),
                // containers, volumes and images; cluster objects; untracked files
                text(r"\b(docker|podman|nerdctl)\b[^\n;&|]*?\s(rm|rmi|prune)\b"),
                text(r"\b(docker|podman|docker-compose|podman-compose)\b[^\n;&|]*?\sdown\b[^\n;&|]*\s(-v|--volumes)\b"),
                text(r"\b(kubectl|oc|helm)\b[^\n;&|]*?\s(delete|uninstall)\b"),
                text(r"\bgit\b[^\n;&|]*?\sclean\b"),
                // a database dropped or emptied, from whatever program sends it
                words(r"\b(drop\s+(database|schema|table|keyspace|collection)|flushall|flushdb)\b"),
                text(r"\.(dropdatabase|drop)\("),
                text(r"\bmysqladmin\b[^\n;&|]*\sdrop\b"),
            ],
        },
        Group {
            name: DISKS,
            // Writes into devices are found on the parsed command.
            pats: vec![
                text(r"\bmkfs[a-z0-9._-]*"),
                labeled(r"(?s)\bdd\b.*\bof=", "dd of="),
                words(
                    r"\b(fdisk|sfdisk|cfdisk|gdisk|parted|sgdisk|lvremove|vgremove|pvremove|mdadm|blkdiscard|mke2fs|mkswap|mkdosfs|mkntfs|mkexfatfs|pvcreate|lvreduce|lvresize)\b",
                ),
                text(r">[|&]?\s*/dev/(sd|nvme|vd|xvd|hd|mmcblk)"),
                text(r"\bzpool\b[^\n;&|]*?\s(destroy|create|labelclear)\b"),
                text(r"\bzfs\b[^\n;&|]*?\s(destroy|rollback)\b"),
                text(r"\bcryptsetup\b[^\n;&|]*?\s(luksformat|erase|lukserase|reencrypt)\b"),
                text(r"\bnvme\b[^\n;&|]*?\s(format|sanitize)\b"),
                text(r"\bbadblocks\b[^\n;&|]*\s-[a-z]*w"),
                text(r"\bhdparm\b[^\n;&|]*\s--(security-erase|trim-sector-ranges|write-sector|fwdownload)"),
            ],
        },
        Group {
            name: POWER,
            // Writes into /proc and /sys are found on the parsed command.
            pats: vec![
                words(r"\b(shutdown|reboot|poweroff|halt)\b"),
                text(r"\b(tel)?init\s+[06]\b"),
                text(
                    r"\bsystemctl(\s+-\S+)*\s+(reboot|soft-reboot|poweroff|halt|kexec|isolate|rescue|emergency|suspend|hibernate|hybrid-sleep|suspend-then-hibernate)\b",
                ),
                words(r"\b(kexec|killall5)\b"),
                // `kill` with a negative number after its first word: -1 is
                // every process the user may signal, -N a whole group.
                text(r"\bkill\b([ \t]+[^\s;&|]+)*?[ \t]+[^\s;&|]+[ \t]+-[0-9$]"),
                // every process of a user
                text(r"\b(pkill|killall)\b[^\n;&|]*\s(-u(\s|[0-9])|--e?uid\b|--user\b)"),
            ],
        },
        Group {
            name: REMOTE,
            pats: vec![
                words(r"\b(iptables|ip6tables|nft|ufw|firewall-cmd|ifdown)\b"),
                text(&format!(r"\b({SERVICE_TOOLS})\b[^\n;&|]*\b({SSH_VERBS})\b[^\n;&|]*\b({REMOTE_SERVICES})\b")),
                text(&format!(r"\b({SERVICE_TOOLS})\b[^\n;&|]*\b({REMOTE_SERVICES})\b[^\n;&|]*\b({SSH_VERBS})\b")),
                text(r"\binit\.d/(sshd?|dropbear|network|networking)\s+(stop|restart|force-reload|reload|disable)\b"),
                text(r"\b(pkill|killall)\b[^\n;&|]*\b(sshd?|dropbear)\b"),
                // a network link, address or route taken away
                text(r"\bip\b[^\n;&|]*?\s(l|li|lin|link)\s+(s|se|set)\s[^\n;&|]*\bdown\b"),
                text(
                    r"\bip\b[^\n;&|]*?\s(l|li|lin|link|a|ad|add|addr|addre|addres|address|r|ro|rou|rout|route)\s+(f|fl|flu|flus|flush|d|de|del|dele|delet|delete)\b",
                ),
                text(r"\bip\b[^\n;&|]*?\s(r|ro|rou|rout|route)\s+(repl|repla|replac|replace|c|ch|cha|chan|chang|change)\b"),
                text(r"\bifconfig\b[^\n;&|]*\sdown\b"),
                text(r"\bnmcli\b[^\n;&|]*\s(off|down|disconnect|delete)\b"),
                text(r"\bwifi\s+down\b"),
                // the SSH server removed
                text(
                    r"\b(apt|apt-get|aptitude|dnf|yum|microdnf|zypper|pacman|apk|opkg|pkg|rpm|dpkg|snap|brew)\b[^\n;&|]*?\s(remove|purge|erase|del|delete|rm|uninstall|autoremove|-[a-z]*[rep][a-z]*|--remove|--purge|--erase)\b[^\n;&|]*\b(openssh|sshd?|dropbear)",
                ),
            ],
        },
        Group {
            name: ACCOUNTS,
            pats: vec![words(
                r"\b(passwd|chpasswd|usermod|userdel|useradd|adduser|deluser|groupdel|groupadd|groupmod|addgroup|delgroup|gpasswd|chage|chsh|chfn|vipw|vigr|newusers|visudo)\b",
            )],
        },
        Group {
            name: PERMISSIONS,
            pats: vec![text(r"\b(chmod|chown|chgrp|setfacl|chattr)\b[^\n;&|]*\s(-[a-z]*r[a-z]*|--recursive)\b")],
        },
        Group { name: SCHEDULED, pats: vec![words(r"\b(at|batch|systemd-run)\b")] },
        Group {
            name: NETWORK,
            pats: vec![
                pat(
                    r"\b(curl|wget|nc|ncat|netcat|socat|telnet|ftp|tftp|lftp|ssh|scp|sftp|rsync|autossh|dbclient|mosh|ngrok|cloudflared|rclone|s3cmd|gsutil|swaks|aria2c|lynx|w3m|elinks|npx|bunx|uvx)\b",
                    true,
                    true,
                    None,
                ),
                // git's verbs that reach a remote, whatever options come first
                text(r"\bgit\b[^\n;&|]*?\s(push|pull|fetch|clone|remote|ls-remote|submodule|send-email|request-pull|daemon|instaweb)\b"),
                pat(r"\b(ping|ping6|dig|nslookup|host|drill|traceroute|tracepath|mtr|whois)\b", true, true, None),
                text(r"/dev/(tcp|udp)/"),
                // mail sent: a mail program run (not a folder named mail)
                text(&format!(
                    r"(^|[\n;&|(){{}}!])\s*((\S*/)?({WRAPPERS})\b[^\n;&|]*?\s)?(?P<show>(\S*/)?(mail|mailx|s-nail|sendmail|mutt|msmtp))\b"
                )),
                // a folder published, or a connection opened
                text(r"\b(python[0-9.]*|pypy[0-9.]*)\b[^\n;&|]*\s-m\s*(http\.server|simplehttpserver|cgihttpserver|smtpd|pyftpdlib|uploadserver)\b"),
                text(r"\bphp[0-9.]*\b[^\n;&|]*\s-s\s*[^\s:]*:[0-9]"),
                text(r"\bbusybox\s+(httpd|ftpd|telnetd|ftpget|ftpput|sendmail)\b"),
                text(r"\bopenssl\b[^\n;&|]*\s(s_client|s_server|s_time)\b"),
                // cloud storage, and container images
                text(r"\b(aws|awslocal)\b[^\n;&|]*?\s(s3|s3api)\b"),
                text(r"\b(gcloud|az)\b[^\n;&|]*?\sstorage\b"),
                text(r"\b(docker|podman)\b[^\n;&|]*?\s(push|pull|login)\b"),
                // packages installed, updated or removed: they come from the
                // network and run their own scripts
                text(
                    r"\b(apt|apt-get|aptitude)\b[^\n;&|]*?\s(install|reinstall|remove|purge|upgrade|dist-upgrade|full-upgrade|autoremove|autopurge|build-dep|source|download|satisfy)\b",
                ),
                text(
                    r"\b(dnf|dnf5|yum|microdnf|tdnf)\b[^\n;&|]*?\s(install|in|reinstall|localinstall|groupinstall|remove|rm|erase|upgrade|up|update|update-minimal|upgrade-minimal|distro-sync|distrosync|downgrade|swap|autoremove)\b",
                ),
                text(r"\bzypper\b[^\n;&|]*?\s(install|in|reinstall|remove|rm|update|up|dist-upgrade|dup|patch|source-install|si)\b"),
                text(r"\b(pacman|yay|paru)\b[^\n;&|]*?\s(-[sru][a-z]*|--sync|--remove|--upgrade)\b"),
                text(r"\b(apk|opkg|pkg)\b[^\n;&|]*?\s(add|del|delete|install|remove|upgrade|autoremove|fix)\b"),
                text(
                    r"\b(snap|flatpak|brew)\b[^\n;&|]*?\s(install|reinstall|remove|rm|uninstall|refresh|revert|update|upgrade|tap|untap|bundle)\b",
                ),
                text(r"\b(pip[0-9.]*|pipx)\b[^\n;&|]*?\s(install|uninstall|download|wheel|inject|upgrade|reinstall|run)\b"),
                text(r"\b(gem|cargo)\b[^\n;&|]*?\s(install|uninstall)\b"),
                text(&format!(r"\b(npm|pnpm|bun)\b[^\n;&|]*?\s({NPM_GLOBAL})\b[^\n;&|]*\s({NPM_VERBS})\b")),
                text(&format!(r"\b(npm|pnpm|bun)\b[^\n;&|]*?\s({NPM_VERBS})\b[^\n;&|]*\s({NPM_GLOBAL})\b")),
                text(r"\byarn\b[^\n;&|]*?\sglobal\s+(add|remove|upgrade)\b"),
            ],
        },
        Group {
            name: OTHER_CODE,
            pats: vec![
                words(r"\b(eval|exec|source|xargs|env)\b"),
                // a leading `.` command
                text(r"(^|[\n;&|(){}!]|\bthen|\bdo|\belse)[ \t]*(?P<show>\.[ \t]+[^\s;&|]+)"),
                text(&format!(r"\b({SHELLS})\b(\s+-\S*)*?\s+-[a-z]*c[a-z]*\b")),
                text(&format!(r"\b({INTERPRETERS})\b(\s+-\S*)*?\s+(-[a-z]*[cerp][a-z]*|--eval|--print|--command|-)(\s|$)")),
                labeled(r"(?s)\bfind\b.*-(exec|ok)", "find -exec"),
                labeled(r#"(?s)\b[gmn]?awk\b.*(system|getline|\|\s*")"#, "awk system"),
                // gawk's counterpart of `sed -i`: it rewrites its files
                labeled(r"\b[gmn]?awk\b[^\n;&|]*\s(-i\s*inplace|--include[= ]inplace)\b", "awk -i inplace"),
                // output piped into a shell or an interpreter
                text(&format!(r"\|&?\s*((\S*/)?({WRAPPERS})\b[^|;&\n]*?\s)?(\S*/)?({SHELLS}|{INTERPRETERS}|busybox)\b")),
                // a shell or an interpreter reading a script from a redirection
                text(&format!(
                    r"(^|[\n;&|()])\s*((\S*/)?({WRAPPERS})\b[^\n;&|]*?\s)?(\S*/)?({SHELLS}|{INTERPRETERS})\b[^\n;&|]*<"
                )),
            ],
        },
        Group {
            name: HIDDEN_CODE,
            pats: vec![
                text(r"\$\("),
                text(r"`"),
                text(r"[<>]\("),
                labeled(r"\$'", "$'…'"),
                text(r"\bbase(64|32)\b[^\n;&|]*\s(-[a-z]*d[a-z]*|--decode)\b"),
                text(r"\bxxd\b[^\n;&|]*\s(-[a-z]*r[a-z]*|-revert)\b"),
                text(r"\bopenssl\b[^\n;&|]*\b(enc|base64)\b[^\n;&|]*\s-d\b"),
            ],
        },
        Group {
            name: PERSISTENCE,
            // Writes into persistence paths are found on the parsed command;
            // enabling a service makes it start at every boot.
            pats: vec![text(r"\bsystemctl(\s+-\S+)*\s+(enable|reenable|preset|link)\b")],
        },
        Group { name: FORK_BOMB, pats: vec![text(r":\s*\(\s*\)\s*\{")] },
        Group { name: SENSITIVE, pats: vec![] },
        Group { name: OWN_FILES, pats: vec![] },
    ]
});

/// A program name that is only part of something else: a folder in a path
/// (`/etc/ssh/`), a dotted name (`.env`), an option (`--host`, `ls -su`), the
/// account file (`/etc/passwd`, `getent passwd`), what a search looks for
/// (`pgrep ssh`), or (`units`) a service named to a service manager
/// (`systemctl status ssh`).
fn excluded(t: &[u8], start: usize, end: usize, units: bool) -> bool {
    if t.get(end) == Some(&b'/') {
        return true;
    }
    if start > 0 && t.get(start - 1) == Some(&b'.') {
        return true;
    }
    if token_start(t, start).is_some_and(|k| k < start && t.get(k) == Some(&b'-')) {
        return true;
    }
    let passwd = t.get(start..end) == Some(&b"passwd"[..]);
    if passwd && t.get(..start).is_some_and(|b| b.ends_with(b"etc/")) {
        return true;
    }
    // An argument of a program that doesn't run its arguments.
    let Some((prog, at)) = segment_program(t, start) else { return false };
    let is = |names: &[&str]| names.iter().any(|s| s.as_bytes() == prog);
    // Printing only counts as harmless from the plain builtin, and only when
    // what it prints goes to the screen: a path (`./echo`) can be any program,
    // and a redirection or a pipe can put the text in a file that runs later.
    let printed = is(&PRINT_PROGRAMS) && (at == 0 || t.get(at - 1) != Some(&b'/')) && {
        let end = segment_end(t, at);
        let piped = t.get(end) == Some(&b'|') && t.get(end + 1) != Some(&b'|');
        !piped && !t.get(at..end).is_some_and(|s| s.contains(&b'>'))
    };
    at != start && (is(&SEARCH_PROGRAMS) || printed || (units && is(&SERVICE_PROGRAMS)) || (passwd && prog == b"getent"))
}

/// Where the simple command holding byte `at` ends: at the next operator.
fn segment_end(t: &[u8], at: usize) -> usize {
    let mut k = at;
    while k < t.len() && !b"\n;&|()`".contains(&t[k]) {
        k += 1;
    }
    k
}

/// Where the token holding byte `at` starts; `None` when that is more than
/// 256 bytes back.
fn token_start(t: &[u8], at: usize) -> Option<usize> {
    let mut k = at;
    while k > 0 {
        let b = *t.get(k - 1)?;
        if b.is_ascii_whitespace() || b"'\"=;|&()<>`".contains(&b) {
            break;
        }
        if at - k >= 256 {
            return None;
        }
        k -= 1;
    }
    Some(k)
}

/// The program (its base name) of the simple command around byte `at`, and
/// where that name starts. `None` past 1 KiB either way.
fn segment_program(t: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let mut k = at;
    while k > 0 {
        let b = *t.get(k - 1)?;
        if b"\n;&|()`".contains(&b) {
            break;
        }
        if at - k >= 1024 {
            return None;
        }
        k -= 1;
    }
    while t.get(k).is_some_and(|b| b.is_ascii_whitespace()) {
        k += 1;
    }
    let mut e = k;
    while t.get(e).is_some_and(|b| !b.is_ascii_whitespace()) {
        if e - k >= 1024 {
            return None;
        }
        e += 1;
    }
    let tok = t.get(k..e)?;
    let base = tok.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1);
    Some((tok.get(base..)?, k + base))
}

/// What matched, for the red card: quotes and backslashes left out, blanks
/// collapsed, hidden characters replaced, at most 60 characters.
fn show(s: &str) -> String {
    let mut out = String::new();
    for part in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let chars: Vec<char> = part.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
        out.extend((0..chars.len()).map(|i| if hidden_at(&chars, i) { '?' } else { chars[i] }));
        if out.len() > 4 * 60 {
            break;
        }
    }
    if out.chars().count() > 60 {
        out = out.chars().take(59).collect();
        out.push('…');
    }
    out
}

/// The word `w` in lower-case `text`, between ASCII word boundaries.
fn has_word(text: &str, w: &str) -> bool {
    let b = text.as_bytes();
    let word = |k: usize| b.get(k).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    text.match_indices(w).any(|(k, _)| (k == 0 || !word(k - 1)) && !word(k + w.len()))
}

/// The always-ask group a command falls in, named with what matched
/// it, such as "removal: rm -rf". `None` when it falls in none.
///
/// The patterns are matched on the command as written and on its words after
/// quote removal (so `c''url` is `curl`), case-insensitively. When the command
/// can't be parsed, only the text as written is matched.
pub fn always_ask(command: &str, ctx: &Ctx) -> Option<String> {
    let home = Home::new(ctx.home);
    let toks = parse(command);
    let cmds = toks.as_deref().map(simples);
    let places = cmds.as_deref().map(|c| places(c, &home));
    // The files the assistant wrote, and the ones this command writes: a
    // command that writes a script and runs it runs the assistant's file.
    let mut written: Vec<String> = ctx.written.iter().map(|w| canon(&resolve_in(w, &home.dir))).collect();
    if let (Some(c), Some(p)) = (cmds.as_deref(), places.as_ref()) {
        written.extend(file_targets(c, p).iter().map(|w| canon(w)));
    }
    let joined = toks.as_deref().map(join_words);
    let mut texts = vec![command];
    if let Some(j) = joined.as_deref() {
        if j != command {
            texts.push(j);
        }
    }
    let lowers: Vec<String> = texts.iter().map(|t| t.to_ascii_lowercase()).collect();
    let parsed = cmds.as_deref().zip(places.as_ref());
    for g in GROUPS.iter() {
        let hit = texts
            .iter()
            .zip(&lowers)
            .find_map(|(t, l)| g.find(t, l))
            .or_else(|| structured(g.name, command, parsed, &home, &written));
        if let Some(what) = hit {
            return Some(format!("{}: {}", g.name, what));
        }
    }
    None
}

/// The files a command writes, as absolute paths, for the list of files the
/// assistant wrote: what its redirections, `tee`, `cp`, `mv`, `install`,
/// `ln`, `dd`, `sort -o`, `uniq`, `patch` and the editors write. A relative
/// path counts in the home folder `home` and in each folder an earlier `cd`
/// names. Left out: a folder a copy may land in, a file a link points to,
/// and a path with an expansion or a pattern.
pub fn written_by(command: &str, home: &str) -> Vec<String> {
    let Some(toks) = parse(command) else { return Vec::new() };
    let cmds = simples(&toks);
    let home = Home::new(home);
    file_targets(&cmds, &places(&cmds, &home))
}

/// At most this many files one command writes are kept.
const MAX_TARGETS: usize = 64;

/// The files the simple commands write (`Target::file`), made absolute in
/// each folder they may run in.
fn file_targets(cmds: &[Simple], places: &Places) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    places.find(cmds, |s, h, _, _| {
        for t in write_targets(s).into_iter().filter(|t| t.file && t.path.len() <= PATH_MAX) {
            if out.len() >= MAX_TARGETS {
                return Some(());
            }
            let p = h.resolve(&t.path);
            if !out.contains(&p) {
                out.push(p);
            }
        }
        None
    });
    out
}

/// At most this many folders a command moves into are followed.
const MAX_FOLDERS: usize = 8;

/// The folders a command's simple commands may run in.
struct Places {
    /// The home folder, then each folder a `cd` or `pushd` names, in order.
    dirs: Vec<Home>,
    /// For each simple command: how many of `dirs` it may run in (the ones
    /// named before it), and whether a `cd` before it went somewhere the
    /// text doesn't say.
    at: Vec<(usize, bool)>,
}

impl Places {
    /// The first hit of `f` on a simple command, tried in each folder it may
    /// run in. `f` also gets the folder's number and whether it is lost.
    fn find<'a, T>(&self, cmds: &[Simple<'a>], mut f: impl FnMut(&Simple<'a>, &Home, usize, bool) -> Option<T>) -> Option<T> {
        for (s, &(n, lost)) in cmds.iter().zip(&self.at) {
            for (k, h) in self.dirs.iter().take(n).enumerate() {
                if let Some(hit) = f(s, h, k, lost) {
                    return Some(hit);
                }
            }
        }
        None
    }
}

/// Where each simple command may run. Commands start in the home folder;
/// `cd DIR` and `pushd DIR` add DIR, made absolute against each folder so
/// far. A `cd` in a subshell (`(cd /tmp); …`) doesn't move what follows, so
/// every folder stays a candidate. A `cd` to an expansion, a pattern or `-`,
/// and `popd`, lose the folder.
fn places(cmds: &[Simple], home: &Home) -> Places {
    let mut dirs = vec![home.at(&home.dir)];
    let mut lost = false;
    let mut at = Vec::with_capacity(cmds.len());
    for s in cmds {
        at.push((dirs.len(), lost));
        let Some(i) = find_program(s, &["cd", "pushd", "popd", "chdir"]) else { continue };
        if s.words.get(i).is_some_and(|w| basename(&w.text).eq_ignore_ascii_case("popd")) {
            lost = true;
            continue;
        }
        let Some(w) = cd_target(s.words.get(i + 1..).unwrap_or_default()) else { continue };
        let t = w.text.as_str();
        if !literal(w) || t.starts_with(['-', '+']) {
            lost = true;
            continue;
        }
        let new: Vec<String> = dirs.iter().map(|d| d.resolve(t)).collect();
        for p in new {
            if dirs.iter().any(|d| d.cwd == p) {
                continue;
            }
            if dirs.len() > MAX_FOLDERS {
                lost = true;
                break;
            }
            dirs.push(home.at(&p));
        }
    }
    Places { dirs, at }
}

/// The folder a `cd` or `pushd` goes to: its first word that isn't an
/// option (`-L`, `-P`, `-e`, `-@`, `-n`). `None`: no word, the home folder.
fn cd_target<'a>(args: &[&'a Word]) -> Option<&'a Word> {
    let mut options = true;
    for &w in args {
        let t = w.text.as_str();
        if options && t == "--" {
            options = false;
            continue;
        }
        if options && t.len() > 1 && t.starts_with('-') && t.chars().skip(1).all(|c| c.is_ascii_alphabetic() || c == '@') {
            continue;
        }
        return Some(w);
    }
    None
}

/// The checks that need the command's structure, each simple command in each
/// folder it may run in. Without a parse (`parsed` is `None`), each falls
/// back to the text as written, erring towards a match.
fn structured(group: &str, cmd: &str, parsed: Option<(&[Simple], &Places)>, home: &Home, written: &[String]) -> Option<String> {
    match (group, parsed) {
        (REMOVAL, Some((c, _))) => c.iter().find_map(removes),
        (REMOVAL, None) => {
            let lower = cmd.to_ascii_lowercase();
            ["rm", "unlink", "rmdir"].into_iter().find(|w| has_word(&lower, w)).map(String::from)
        }
        (DISKS, Some((c, p))) => p.find(c, |s, h, _, _| writes_into(s, h, disk_device)),
        (DISKS, None) => raw_writes_into(cmd, home, disk_device),
        (POWER, Some((c, p))) => p.find(c, |s, h, _, _| writes_into(s, h, kernel_path)),
        (POWER, None) => raw_writes_into(cmd, home, kernel_path),
        (PERMISSIONS, Some((c, p))) => p.find(c, |s, h, _, _| home_mode(s, h)),
        (SCHEDULED, Some((c, _))) => c.iter().find_map(crontab),
        (SCHEDULED, None) => has_word(&cmd.to_ascii_lowercase(), "crontab").then(|| "crontab".to_string()),
        (OTHER_CODE, Some((c, _))) => c.iter().find_map(sed),
        (OTHER_CODE, None) => has_word(&cmd.to_ascii_lowercase(), "sed").then(|| "sed".to_string()),
        (PERSISTENCE, Some((c, p))) => p.find(c, |s, h, _, lost| persistence_write(s, h, lost)),
        (PERSISTENCE, None) => raw_persistence(cmd, home),
        (SENSITIVE, parsed) => {
            let mut seen = HashSet::new();
            parsed
                .and_then(|(c, p)| p.find(c, |s, h, k, _| names_sensitive(s, h, k, &mut seen)))
                .or_else(|| raw_sensitive(cmd, home))
        }
        (OWN_FILES, Some((c, p))) => p.find(c, |s, h, _, _| runs_written(s, h, written)),
        (OWN_FILES, None) => raw_own(cmd, home, written),
        _ => None,
    }
}

/// `rm`, `unlink` or `rmdir` in any form: a removal can't be undone, and a
/// pattern (`rm ~/*`) can take much more than it names.
fn removes(s: &Simple) -> Option<String> {
    let i = find_program(s, &["rm", "unlink", "rmdir"])?;
    let shown: Vec<&str> = s.words.get(i..)?.iter().take(4).map(|w| w.text.as_str()).collect();
    Some(show(&shown.join(" ")))
}

/// A device a write can damage: anything in `/dev` but the streams every
/// shell writes to, the memory file system, and bash's network paths (the
/// network group's).
fn disk_device(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/dev/") else { return false };
    let top = rest.split('/').next().unwrap_or_default();
    !rest.is_empty()
        && !matches!(
            top,
            "null" | "zero" | "full" | "random" | "urandom" | "tty" | "stdin" | "stdout" | "stderr" | "fd" | "pts" | "shm" | "tcp" | "udp"
        )
}

/// The kernel's files: a write into `/proc` or `/sys` acts on the machine
/// (`/proc/sysrq-trigger` restarts it at once, `/sys/power/state` suspends
/// it, `/proc/sys/kernel/core_pattern` names a program the kernel runs).
fn kernel_path(p: &str) -> bool {
    under(p, "/proc") || under(p, "/sys")
}

/// A write (`write_targets`) into a path `kind` accepts.
fn writes_into(s: &Simple, home: &Home, kind: fn(&str) -> bool) -> Option<String> {
    write_targets(s)
        .into_iter()
        .find(|t| candidates(&t.path).iter().any(|c| c.len() <= PATH_MAX && kind(&home.resolve(c))))
        .map(|t| show(&format!("{} {}", t.how, t.path)))
}

/// Without a parse: a command that may write, and names a path `kind`
/// accepts.
fn raw_writes_into(cmd: &str, home: &Home, kind: fn(&str) -> bool) -> Option<String> {
    if !raw_writes(cmd) {
        return None;
    }
    fragments(cmd).find(|f| candidates(f).iter().any(|c| c.len() <= PATH_MAX && kind(&home.resolve(c)))).map(|f| show(&f))
}

/// `chmod`, `chown`, `chgrp`, `setfacl` or `chattr` on a home folder or a
/// folder above one: sshd refuses key logins when others can write there.
fn home_mode(s: &Simple, home: &Home) -> Option<String> {
    let i = find_program(s, &["chmod", "chown", "chgrp", "setfacl", "chattr"])?;
    let prog = basename(&s.words.get(i)?.text).to_ascii_lowercase();
    s.words
        .get(i + 1..)?
        .iter()
        .filter(|w| !w.text.starts_with('-'))
        .find(|w| texts(w).iter().any(|t| t.len() <= PATH_MAX && holds_home(&home.resolve(t), home)))
        .map(|w| show(&format!("{prog} {}", w.text)))
}

/// `p` is a home folder (the login's, root's, a user's in `/home` or
/// `/Users`) or a folder above one.
fn holds_home(p: &str, home: &Home) -> bool {
    p == "/"
        || [home.dir.as_str(), "/root", "/var/root", "/home", "/Users", "/usr/home"].iter().any(|d| under(d, p))
        || ["/home/", "/Users/", "/usr/home/"].iter().any(|t| p.strip_prefix(t).is_some_and(|r| !r.is_empty() && !r.contains('/')))
}

/// `crontab` with `-r`, `-e`, a file, or nothing (it reads stdin); `-l` only
/// lists.
fn crontab(s: &Simple) -> Option<String> {
    let i = find_program(s, &["crontab"])?;
    let args = s.words.get(i + 1..).unwrap_or_default();
    let mut listed = false;
    let mut k = 0;
    while let Some(w) = args.get(k) {
        k += 1;
        match w.text.as_str() {
            "-u" => k += 1,
            "-l" | "-V" | "--version" | "-h" | "--help" => listed = true,
            other => return Some(show(&format!("crontab {other}"))),
        }
    }
    (!listed).then(|| "crontab".to_string())
}

/// `sed` with `-i`, `-f` (a script it can't see), or a script that runs a
/// command (`e`) or writes a file (`w`).
fn sed(s: &Simple) -> Option<String> {
    let i = find_program(s, &["sed", "gsed"])?;
    let args = s.words.get(i + 1..).unwrap_or_default();
    let mut scripts: Vec<&str> = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut k = 0;
    while let Some(w) = args.get(k) {
        k += 1;
        let a = w.text.as_str();
        if a == "--" {
            operands.extend(args.get(k..).unwrap_or_default().iter().map(|w| w.text.as_str()));
            break;
        }
        if let Some(l) = long(a) {
            if abbrev(l, &["in-place"]) {
                return Some("sed -i".into());
            }
            if abbrev(l, &["file"]) {
                return Some("sed -f".into());
            }
            if abbrev(l, &["expression"]) {
                match a.split_once('=') {
                    Some((_, v)) => scripts.push(v),
                    None => {
                        if let Some(v) = args.get(k) {
                            scripts.push(&v.text);
                        }
                        k += 1;
                    }
                }
            } else if abbrev(l, &["line-length"]) && !a.contains('=') {
                k += 1;
            }
            continue;
        }
        if let Some(c) = short(a) {
            for (j, ch) in c.char_indices() {
                match ch {
                    'i' => return Some("sed -i".into()),
                    'f' => return Some("sed -f".into()),
                    'e' | 'l' => {
                        let rest = c.get(j + 1..).unwrap_or_default();
                        let value = if rest.is_empty() {
                            k += 1;
                            args.get(k - 1).map(|w| w.text.as_str())
                        } else {
                            Some(rest)
                        };
                        if ch == 'e' {
                            scripts.extend(value);
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        operands.push(a);
    }
    if scripts.is_empty() {
        scripts.extend(operands.first());
    }
    scripts.iter().find_map(|s| sed_script(s)).map(String::from)
}

/// A sed script runs a command (`e`) or writes a file (`w`, `W`). A script it
/// can't read counts as one that might.
fn sed_script(script: &str) -> Option<&'static str> {
    let c: Vec<char> = script.chars().collect();
    let at = |k: usize| c.get(k).copied();
    let blank = |x: char| x == ' ' || x == '\t';
    let mut i = 0;
    loop {
        while at(i).is_some_and(|x| x.is_whitespace() || x == ';') {
            i += 1;
        }
        at(i)?;
        // Up to two addresses, then `!`.
        for _ in 0..2 {
            let Some(k) = sed_address(&c, i) else { return Some("sed") };
            i = k;
            while at(i).is_some_and(blank) {
                i += 1;
            }
            if at(i) != Some(',') {
                break;
            }
            i += 1;
            while at(i).is_some_and(blank) {
                i += 1;
            }
        }
        while at(i).is_some_and(|x| x == '!' || blank(x)) {
            i += 1;
        }
        let cmd = at(i)?;
        i += 1;
        match cmd {
            '{' | '}' | '=' | 'd' | 'D' | 'g' | 'G' | 'h' | 'H' | 'l' | 'n' | 'N' | 'p' | 'P' | 'x' | 'z' | 'F' => {}
            'q' | 'Q' | 'L' => {
                while at(i).is_some_and(|x| x.is_ascii_digit() || blank(x)) {
                    i += 1;
                }
            }
            'e' => return Some("sed e"),
            'w' | 'W' => return Some("sed w"),
            // text, or a file to read, up to the end of the line
            'a' | 'i' | 'c' | 'r' | 'R' | '#' => i = line_end(&c, i),
            ':' | 'b' | 't' | 'T' | 'v' => {
                while at(i).is_some_and(|x| x != ';' && x != '\n') {
                    i += 1;
                }
            }
            's' | 'y' => {
                let Some(d) = at(i).filter(|d| *d != '\n' && *d != '\\') else { return Some("sed") };
                let Some(k) = sed_delim(&c, i + 1, d).and_then(|k| sed_delim(&c, k, d)) else { return Some("sed") };
                i = k;
                if cmd == 's' {
                    while let Some(x) = at(i) {
                        match x {
                            ';' | '\n' | '}' => break,
                            'e' => return Some("sed e"),
                            'w' | 'W' => return Some("sed w"),
                            'g' | 'p' | 'i' | 'I' | 'm' | 'M' | ' ' | '\t' | '0'..='9' => i += 1,
                            _ => return Some("sed"),
                        }
                    }
                }
            }
            _ => return Some("sed"),
        }
    }
}

/// Past a sed address at `i` (none is fine). `None` for an unclosed regex.
fn sed_address(c: &[char], mut i: usize) -> Option<usize> {
    match c.get(i) {
        Some(x) if x.is_ascii_digit() => {
            while c.get(i).is_some_and(|x| x.is_ascii_digit() || *x == '~') {
                i += 1;
            }
        }
        Some('$') => i += 1,
        Some('+') | Some('~') => {
            i += 1;
            while c.get(i).is_some_and(|x| x.is_ascii_digit()) {
                i += 1;
            }
        }
        Some('/') => i = sed_delim(c, i + 1, '/')?,
        Some('\\') => {
            let d = *c.get(i + 1)?;
            i = sed_delim(c, i + 2, d)?;
        }
        _ => return Some(i),
    }
    while c.get(i).is_some_and(|x| *x == 'I' || *x == 'M') {
        i += 1;
    }
    Some(i)
}

/// Past the next unescaped `d` from `i`.
fn sed_delim(c: &[char], mut i: usize, d: char) -> Option<usize> {
    while let Some(&x) = c.get(i) {
        if x == '\\' {
            i += 2;
            continue;
        }
        if x == d {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// The end of the line at `i`, past backslash-newline continuations.
fn line_end(c: &[char], mut i: usize) -> usize {
    while let Some(&x) = c.get(i) {
        if x == '\\' {
            i += 2;
            continue;
        }
        if x == '\n' {
            return i;
        }
        i += 1;
    }
    i
}

/// A file a simple command writes.
struct Target {
    /// How: `>`, `tee`, `cp`…
    how: String,
    /// The path as written.
    path: String,
    /// The command writes this very file, and the path says which: not a
    /// folder a copy may land in, not a file a link points to, and no
    /// expansion or pattern in it.
    file: bool,
}

/// A path the shell takes as written: no pattern, no braces, and no
/// expansion but a leading `~` or `$HOME`.
fn literal(w: &Word) -> bool {
    !w.glob && !w.brace && (!w.dollar || strip_home(&w.text).is_some_and(|r| !r.contains(['$', '`'])))
}

/// Programs whose files `write_targets` knows.
const WRITERS: [&str; 16] =
    ["tee", "cp", "mv", "install", "ln", "dd", "sort", "uniq", "patch", "ed", "red", "ex", "vi", "vim", "nvim", "sponge"];

/// Editors and `sponge`: every file they are given, they may write.
const EDITORS: [&str; 7] = ["ed", "red", "ex", "vi", "vim", "nvim", "sponge"];

/// Where a simple command writes: `>` and the like; `tee`'s files; what
/// `cp`, `mv`, `install` and `ln` write (`copies`); `dd of=`; `sort -o`;
/// `uniq`'s second file; `patch`'s files; and the files of `ed`, `ex`,
/// `vim` and `sponge`.
fn write_targets(s: &Simple) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for (op, w) in &s.redirs {
        if let Some(w) = w {
            if writes(op, &w.text) {
                for t in texts(w) {
                    out.push(Target { how: op.to_string(), path: t.to_string(), file: t == w.text && literal(w) });
                }
            }
        }
    }
    let Some(i) = find_program(s, &WRITERS) else { return out };
    let prog = s.words.get(i).map(|w| basename(&w.text).to_ascii_lowercase()).unwrap_or_default();
    let args = s.words.get(i + 1..).unwrap_or_default();
    let plain = |w: &&Word| (w.text.clone(), literal(w));
    let found: Vec<(String, bool)> = match prog.as_str() {
        "tee" => args.iter().filter(|w| !w.text.starts_with('-')).map(plain).collect(),
        "dd" => args.iter().filter_map(|w| w.text.strip_prefix("of=").map(|v| (v.to_string(), literal(w)))).collect(),
        "sort" => sort_output(args),
        "uniq" => uniq_output(args).iter().map(plain).collect(),
        "patch" => patch_files(args),
        p if EDITORS.contains(&p) => args.iter().filter(|w| !w.text.starts_with(['-', '+'])).map(plain).collect(),
        _ => copies(&prog, args),
    };
    out.extend(found.into_iter().map(|(path, file)| Target { how: prog.clone(), path, file }));
    out
}

/// What `cp`, `mv`, `install` and `ln` write: the target, and the target as
/// the folder each source lands in under its own name. The target itself
/// counts as a file (`Target::file`) only when it can't be a folder (one
/// source, no `/` at its end) and doesn't look like one: the source's name,
/// or a name with a dot (`/etc/nginx` is most likely a folder). Then `ln`'s
/// sources, and those of `cp -l` and `cp -s`: a link gives the same file a
/// second name.
fn copies(prog: &str, args: &[&Word]) -> Vec<(String, bool)> {
    let valued = if prog == "install" { "mogS" } else { "S" };
    let mut operands: Vec<&Word> = Vec::new();
    let mut dirs: Vec<(&str, &Word)> = Vec::new();
    let mut links = prog == "ln";
    let mut k = 0;
    while let Some(&w) = args.get(k) {
        k += 1;
        let a = w.text.as_str();
        if a == "--" {
            operands.extend(args.get(k..).unwrap_or_default());
            break;
        }
        if let Some(v) = a.strip_prefix("--target-directory=") {
            dirs.push((v, w));
        } else if a == "--target-directory" {
            if let Some(&n) = args.get(k) {
                dirs.push((n.text.as_str(), n));
            }
            k += 1;
        } else if let Some(l) = long(a) {
            links |= prog == "cp" && abbrev(l, &["link", "symbolic-link"]);
        } else if let Some(c) = short(a) {
            links |= prog == "cp" && cluster_has(c, "ls", "St");
            if let Some(t) = c.find('t') {
                let rest = c.get(t + 1..).unwrap_or_default();
                if rest.is_empty() {
                    if let Some(&n) = args.get(k) {
                        dirs.push((n.text.as_str(), n));
                    }
                    k += 1;
                } else {
                    dirs.push((rest, w));
                }
            } else if value_next(c, valued) {
                k += 1;
            }
        } else {
            operands.push(w);
        }
    }
    let no_target_dir = dirs.is_empty();
    let (dests, sources): (Vec<(&str, &Word)>, &[&Word]) = if no_target_dir {
        match operands.split_last() {
            Some((last, rest)) => (vec![(last.text.as_str(), *last)], rest),
            None => (Vec::new(), &[]),
        }
    } else {
        (dirs, &operands[..])
    };
    let mut out = Vec::new();
    for (d, dw) in &dests {
        let folder = !no_target_dir || sources.len() > 1 || d.ends_with('/');
        let name = basename(d);
        let file = !folder && sources.first().is_some_and(|s| basename(&s.text) == name || name.contains('.'));
        out.push((d.to_string(), file && literal(dw)));
        for src in sources {
            out.push((format!("{}/{}", d.trim_end_matches('/'), basename(&src.text)), literal(dw) && literal(src)));
        }
    }
    if links {
        out.extend(sources.iter().map(|s| (s.text.clone(), false)));
    }
    // `ln -s /x/.bashrc` makes the link in the current folder.
    if let ("ln", true, [only]) = (prog, no_target_dir, operands.as_slice()) {
        out.push((basename(&only.text).to_string(), literal(only)));
    }
    out
}

/// `sort`'s `-o` and `--output` files.
fn sort_output(args: &[&Word]) -> Vec<(String, bool)> {
    const VALUED: [&str; 10] = [
        "key",
        "field-separator",
        "temporary-directory",
        "buffer-size",
        "files0-from",
        "compress-program",
        "random-source",
        "parallel",
        "batch-size",
        "sort",
    ];
    let mut out = Vec::new();
    let mut k = 0;
    while let Some(&w) = args.get(k) {
        k += 1;
        let a = w.text.as_str();
        if a == "--" {
            break;
        }
        if let Some(l) = long(a) {
            let output = abbrev(l, &["output"]);
            match a.split_once('=') {
                Some((_, v)) if output => out.push((v.to_string(), literal(w))),
                None if output || abbrev(l, &VALUED) => {
                    if let Some(&n) = args.get(k).filter(|_| output) {
                        out.push((n.text.clone(), literal(n)));
                    }
                    k += 1;
                }
                _ => {}
            }
            continue;
        }
        let Some(c) = short(a) else { continue };
        if let Some((j, ch)) = c.char_indices().find(|(_, ch)| *ch == 'o' || "ktTS".contains(*ch)) {
            let rest = c.get(j + ch.len_utf8()..).unwrap_or_default();
            let value = if rest.is_empty() {
                k += 1;
                args.get(k - 1).map(|n| (n.text.clone(), literal(n)))
            } else {
                Some((rest.to_string(), literal(w)))
            };
            if ch == 'o' {
                out.extend(value);
            }
        }
    }
    out
}

/// `uniq`'s second file, which it writes.
fn uniq_output<'a>(args: &[&'a Word]) -> Option<&'a Word> {
    let texts: Vec<&str> = args.iter().map(|w| w.text.as_str()).collect();
    uniq_operands(&texts).get(1).and_then(|&k| args.get(k)).copied()
}

/// The files `patch` writes: the file it is given, and its `-o` and `-r`
/// files. Files named only inside the patch can't be known here.
fn patch_files(args: &[&Word]) -> Vec<(String, bool)> {
    const VALUED: [&str; 13] = [
        "strip",
        "directory",
        "ifdef",
        "fuzz",
        "input",
        "output",
        "reject-file",
        "suffix",
        "prefix",
        "basename-prefix",
        "version-control",
        "get",
        "quoting-style",
    ];
    let mut out = Vec::new();
    let mut operands = 0;
    let mut options = true;
    let mut k = 0;
    while let Some(&w) = args.get(k) {
        k += 1;
        let a = w.text.as_str();
        if options && a == "--" {
            options = false;
            continue;
        }
        if options {
            if let Some(l) = long(a) {
                let writes = abbrev(l, &["output", "reject-file"]);
                match a.split_once('=') {
                    Some((_, v)) if writes => out.push((v.to_string(), literal(w))),
                    None if abbrev(l, &VALUED) => {
                        if let Some(&n) = args.get(k).filter(|_| writes) {
                            out.push((n.text.clone(), literal(n)));
                        }
                        k += 1;
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(c) = short(a) {
                if let Some((j, ch)) = c.char_indices().find(|(_, ch)| "pdDFiorzBVYg".contains(*ch)) {
                    let rest = c.get(j + ch.len_utf8()..).unwrap_or_default();
                    let value = if rest.is_empty() {
                        k += 1;
                        args.get(k - 1).map(|n| (n.text.clone(), literal(n)))
                    } else {
                        Some((rest.to_string(), literal(w)))
                    };
                    if ch == 'o' || ch == 'r' {
                        out.extend(value);
                    }
                }
                continue;
            }
        }
        // The first operand is the file to change, the second the patch.
        if operands == 0 {
            out.push((w.text.clone(), literal(w)));
        }
        operands += 1;
    }
    out
}

/// A write into a persistence path. Once a `cd` went somewhere the text
/// doesn't say (`lost`), any relative path may be one.
fn persistence_write(s: &Simple, home: &Home, lost: bool) -> Option<String> {
    write_targets(s)
        .into_iter()
        .find(|t| (lost && relative(&t.path)) || candidates(&t.path).iter().any(|c| names_persistence(c, home)))
        .map(|t| show(&format!("{} {}", t.how, t.path)))
}

/// A path that starts in the current folder.
fn relative(p: &str) -> bool {
    !p.starts_with(['/', '~']) && strip_home(p).is_none()
}

/// A word of the command, or a file it writes, names a sensitive path.
/// `seen` holds the words already checked in each folder (`view`): a long
/// command repeats them.
fn names_sensitive<'a>(s: &Simple<'a>, home: &Home, view: usize, seen: &mut HashSet<(&'a str, bool, usize)>) -> Option<String> {
    let targets = write_targets(s);
    let target = |t: &str| targets.iter().any(|x| x.path == t);
    let mut check = |t: &'a str, write: bool| {
        seen.insert((t, write, view)) && candidates(t).iter().any(|c| names_secret(c, home, write))
    };
    for w in &s.words {
        for t in texts(*w) {
            if check(t, target(t)) {
                return Some(show(t));
            }
        }
    }
    for (op, w) in &s.redirs {
        let Some(w) = w else { continue };
        if matches!(*op, "<<" | "<<-") {
            continue; // a here-document's delimiter
        }
        for t in texts(*w) {
            if check(t, writes(op, t)) {
                return Some(show(t));
            }
        }
    }
    targets.iter().find(|x| names_secret(&x.path, home, true)).map(|x| show(&x.path))
}

/// The pieces of the command as written that may be paths.
fn fragments(cmd: &str) -> impl Iterator<Item = String> + '_ {
    let mut seen = HashSet::new();
    cmd.split(|c: char| c.is_whitespace() || "'\"`;|&<>(){},=$:".contains(c))
        .filter(move |f| !f.is_empty() && seen.insert(*f))
        .map(|f| f.replace('\\', ""))
}

fn raw_sensitive(cmd: &str, home: &Home) -> Option<String> {
    fragments(cmd).find(|f| candidates(f).iter().any(|c| names_secret(c, home, false))).map(|f| show(&f))
}

/// Without a parse: the command may write a file (`>`, or a program
/// `write_targets` knows).
fn raw_writes(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    lower.contains('>') || WRITERS.iter().any(|w| has_word(&lower, w))
}

fn raw_persistence(cmd: &str, home: &Home) -> Option<String> {
    if !raw_writes(cmd) {
        return None;
    }
    fragments(cmd).find(|f| candidates(f).iter().any(|c| names_persistence(c, home))).map(|f| show(&f))
}

/// The path as every system names it: macOS keeps /etc, /var and /tmp in
/// /private, and FreeBSD keeps the home folders in /usr/home.
fn canon(p: &str) -> String {
    match p.strip_prefix("/private") {
        Some(r) if ["/etc/", "/var/", "/tmp/"].iter().any(|d| r.starts_with(d)) => r.to_string(),
        _ => match p.strip_prefix("/usr/home/") {
            Some(r) => format!("/home/{r}"),
            None => p.to_string(),
        },
    }
}

/// A path names a file the assistant wrote (`written` holds `canon` paths):
/// by its resolved path; a bare name by the file's name; and a relative path
/// with a `/` (`./x`, `sub/x`) by its last parts, since the folder it starts
/// in may not be the one the gate knows.
fn is_written(t: &str, home: &Home, written: &[String]) -> bool {
    if t.is_empty() || t.len() > PATH_MAX {
        return false;
    }
    let p = canon(&home.resolve(t));
    let tail = if t.contains('/') && relative(t) { path_tail(t) } else { None };
    written.iter().any(|x| *x == p || (!t.contains('/') && basename(x) == t) || tail.as_deref().is_some_and(|s| x.ends_with(s)))
}

/// The parts of a relative path after its last `..`, as `/a/b`; `None` when
/// none are left.
fn path_tail(t: &str) -> Option<String> {
    let parts: Vec<&str> = t.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let from = parts.iter().rposition(|c| *c == "..").map_or(0, |k| k + 1);
    let rest = parts.get(from..).unwrap_or_default();
    (!rest.is_empty()).then(|| rest.iter().map(|c| format!("/{c}")).collect())
}

fn word_written(w: &Word, home: &Home, written: &[String]) -> bool {
    texts(w).iter().any(|t| is_written(t, home, written))
}

/// Extensions a script program finds a file by: `node tool` runs tool.js,
/// `python3 -m tool` runs tool.py.
const SCRIPT_EXTENSIONS: [&str; 11] = [".py", ".sh", ".js", ".mjs", ".cjs", ".ts", ".rb", ".pl", ".php", ".lua", ".awk"];

/// A script's word names a file the assistant wrote, as written or, when
/// its name has no extension, with one.
fn script_written(t: &str, home: &Home, written: &[String]) -> bool {
    is_written(t, home, written)
        || (!t.is_empty()
            && !basename(t).contains('.')
            && SCRIPT_EXTENSIONS.iter().any(|e| is_written(&format!("{t}{e}"), home, written)))
}

/// The program files `awk` runs: `-f prog.awk`, and what gawk's `-E`, `-i`
/// and `-l` load.
fn awk_programs<'a>(args: &[&'a Word]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut k = 0;
    while let Some(&w) = args.get(k) {
        k += 1;
        let a = w.text.as_str();
        if a == "--" {
            break;
        }
        let next = |k: &mut usize| {
            *k += 1;
            args.get(*k - 1).map(|n| n.text.as_str())
        };
        if let Some(l) = long(a) {
            if abbrev(l, &["file", "exec", "include", "load"]) {
                out.extend(match a.split_once('=') {
                    Some((_, v)) => Some(v),
                    None => next(&mut k),
                });
            }
        } else if let Some(c) = short(a).filter(|c| c.starts_with(['f', 'E', 'i', 'l'])) {
            let rest = c.get(1..).unwrap_or_default();
            out.extend(if rest.is_empty() { next(&mut k) } else { Some(rest) });
        }
    }
    out
}

/// The command runs a file the assistant wrote: as its program (behind
/// `sudo`, `env`, `nohup`… any later word counts), as a script of a shell,
/// an interpreter, `source` or `.` (any of their words), as an `awk`
/// program file, or fed to one on stdin.
fn runs_written(s: &Simple, home: &Home, written: &[String]) -> Option<String> {
    if written.is_empty() {
        return None;
    }
    let (i, wrapped) = program_index(&s.words)?;
    let prog = s.words.get(i)?;
    if word_written(prog, home, written) {
        return Some(show(&prog.text));
    }
    let base = basename(&prog.text).to_ascii_lowercase();
    let script = interpreter(&base) || base == "source" || base == ".";
    let after = s.words.get(i + 1..).unwrap_or_default();
    if wrapped {
        if let Some(w) = after.iter().find(|w| word_written(w, home, written)) {
            return Some(show(&w.text));
        }
    } else if script {
        // Every word: an option's value can come first (`bash -o errexit x.sh`).
        for w in after.iter().take(MAX_CANDIDATES) {
            let t = w.text.as_str();
            if t.starts_with('-') && t.len() > 1 {
                if t.split_once('=').is_some_and(|(_, v)| script_written(v, home, written)) {
                    return Some(show(t));
                }
                continue;
            }
            if texts(w).iter().any(|x| script_written(x, home, written)) {
                return Some(show(t));
            }
        }
    } else if matches!(base.as_str(), "awk" | "gawk" | "mawk" | "nawk") {
        if let Some(f) = awk_programs(after).into_iter().find(|f| script_written(f, home, written)) {
            return Some(show(f));
        }
    }
    if script || wrapped {
        for (op, w) in &s.redirs {
            if let (&"<", Some(w)) = (op, w) {
                if word_written(w, home, written) {
                    return Some(show(&w.text));
                }
            }
        }
    }
    None
}

fn raw_own(cmd: &str, home: &Home, written: &[String]) -> Option<String> {
    if written.is_empty() {
        return None;
    }
    fragments(cmd).find(|f| is_written(f, home, written)).map(|f| show(&f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const HOME: &str = "/home/u";

    fn ctx<'a>(written: &'a [String], always_run: &'a [String]) -> Ctx<'a> {
        Ctx { connected: true, posix: true, linux: true, home: HOME, written, always_run, files_only: false }
    }

    fn ro(c: &str) -> bool {
        read_only(c, HOME, true)
    }

    fn group(c: &str) -> Option<String> {
        always_ask(c, &ctx(&[], &[]))
    }

    /// The exact group and what matched.
    fn expect(c: &str, want: &str) {
        assert_eq!(group(c).as_deref(), Some(want), "{c:?}");
    }

    fn in_group(c: &str, g: &str) {
        let got = group(c);
        assert!(got.as_deref().is_some_and(|s| s.starts_with(&format!("{g}: "))), "{c:?} should be in {g:?}, got {got:?}");
    }

    fn clean(c: &str) {
        assert_eq!(group(c), None, "{c:?}");
    }

    /// What a level gives a call: refused, runs, asks or red.
    fn outcome(d: &Decision) -> &'static str {
        match d {
            Decision::Refuse(_) => "refused",
            Decision::Allow(_) => "runs",
            Decision::Ask { red: Some(_), .. } => "red",
            Decision::Ask { red: None, .. } => "asks",
        }
    }

    const LEVELS: [Level; 5] = [Level::Suggest, Level::AskAll, Level::Ask, Level::Auto, Level::Full];

    fn row(call: Call, c: &Ctx) -> [&'static str; 5] {
        LEVELS.map(|l| outcome(&decide(l, &call, c)))
    }

    #[test]
    fn levels_round_trip() {
        for l in LEVELS {
            assert_eq!(Level::parse(l.as_str()), Some(l));
        }
        assert_eq!(Level::parse("Ask"), None);
    }

    #[test]
    fn the_table() {
        let written = vec!["/home/u/deploy.sh".to_string()];
        let c = ctx(&written, &[]);
        let run = |command| Call::Run { command };
        assert_eq!(row(Call::List, &c), ["refused", "runs", "runs", "runs", "runs"]);
        // Listing a sensitive folder shows names only, so it runs too.
        assert_eq!(row(Call::List, &c), ["refused", "runs", "runs", "runs", "runs"]);
        assert_eq!(row(Call::Read { path: "/etc/nginx/nginx.conf" }, &c), ["refused", "runs", "runs", "runs", "runs"]);
        assert_eq!(row(Call::Read { path: "/etc/passwd" }, &c), ["refused", "runs", "runs", "runs", "runs"]);
        assert_eq!(row(Call::Read { path: "/home/u/.ssh/id_ed25519" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(run("ls -la /var/log"), &c), ["refused", "asks", "runs", "runs", "runs"]);
        assert_eq!(row(run("apt-get update"), &c), ["refused", "asks", "asks", "runs", "runs"]);
        assert_eq!(row(run("rm -rf /tmp/build"), &c), ["refused", "red", "red", "red", "runs"]);
        assert_eq!(row(run("cat ~/.ssh/id_rsa"), &c), ["refused", "red", "red", "red", "runs"]);
        assert_eq!(row(Call::Write { path: "/etc/nginx/nginx.conf" }, &c), ["refused", "asks", "asks", "runs", "runs"]);
        assert_eq!(row(Call::Write { path: "/home/u/.bashrc" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(Call::Write { path: "/etc/passwd" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(Call::Write { path: "/etc/systemd/system/x.service" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(run("./deploy.sh"), &c), ["refused", "red", "red", "red", "runs"]);
        assert_eq!(row(run("bash deploy.sh"), &c), ["refused", "red", "red", "red", "runs"]);
        assert_eq!(row(run("cat deploy.sh"), &c), ["refused", "asks", "runs", "runs", "runs"]);
    }

    #[test]
    fn how_calls_are_let_through() {
        let c = ctx(&[], &[]);
        let d = |l, call| decide(l, &call, &c);
        assert_eq!(d(Level::Ask, Call::Run { command: "df -h" }), Decision::Allow(How::ReadOnly));
        assert_eq!(d(Level::Auto, Call::Run { command: "apt-get update" }), Decision::Allow(How::Auto));
        assert_eq!(d(Level::Full, Call::Run { command: "sudo reboot" }), Decision::Allow(How::Full));
        assert_eq!(d(Level::Full, Call::Read { path: "/etc/shadow" }), Decision::Allow(How::Full));
        assert_eq!(d(Level::Ask, Call::Read { path: "/etc/hosts" }), Decision::Allow(How::ReadOnly));
        assert_eq!(d(Level::Auto, Call::Write { path: "/srv/app/x.conf" }), Decision::Allow(How::Auto));
        assert_eq!(d(Level::Suggest, Call::List), Decision::Refuse(SUGGEST_ONLY.into()));
        assert_eq!(
            d(Level::Ask, Call::Run { command: "sudo ls" }),
            Decision::Ask { reason: "Always asks below Full access".into(), red: Some("privilege: sudo".into()) }
        );
    }

    #[test]
    fn not_connected_refuses_everything() {
        let mut c = ctx(&[], &[]);
        c.connected = false;
        let calls = [
            Call::Run { command: "ls" },
            Call::List,
            Call::Read { path: "/etc/hosts" },
            Call::Write { path: "/tmp/x" },
        ];
        for l in LEVELS {
            for call in calls {
                assert_eq!(decide(l, &call, &c), Decision::Refuse(NOT_CONNECTED.into()));
            }
        }
    }

    #[test]
    fn on_a_files_only_server_auto_writes_files_and_still_asks_for_secrets() {
        let mut c = ctx(&[], &[]);
        c.posix = false;
        c.files_only = true;
        // An FTP server: Auto stays Auto for files (no command can run there to be checked).
        assert_eq!(row(Call::Write { path: "/home/u/site/index.html" }, &c), ["refused", "asks", "asks", "runs", "runs"]);
        assert_eq!(row(Call::Write { path: "/home/u/.bashrc" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        // A server with no POSIX shell that does run commands: Auto is Ask, for files too.
        c.files_only = false;
        assert_eq!(row(Call::Write { path: "/home/u/site/index.html" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
    }

    #[test]
    fn without_a_posix_shell_every_command_asks() {
        let mut c = ctx(&[], &[]);
        c.posix = false;
        let run = |command| Call::Run { command };
        assert_eq!(row(run("ls -la"), &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(run("dir C:\\"), &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(run("rm -rf x"), &c), ["refused", "red", "red", "red", "runs"]);
        // Auto behaves like Ask there, for files too.
        assert_eq!(row(Call::Write { path: "/srv/x" }, &c), ["refused", "asks", "asks", "asks", "runs"]);
        assert_eq!(row(Call::Read { path: "/etc/hosts" }, &c), ["refused", "runs", "runs", "runs", "runs"]);
        assert_eq!(row(Call::List, &c), ["refused", "runs", "runs", "runs", "runs"]);
    }

    #[test]
    fn the_always_run_list() {
        let always = vec![
            "systemctl restart nginx".to_string(),
            "sudo systemctl restart nginx".to_string(),
            "uptime".to_string(),
        ];
        let c = ctx(&[], &always);
        let run = |command| Call::Run { command };
        let d = |l, command| decide(l, &run(command), &c);
        for l in [Level::Ask, Level::Auto] {
            assert_eq!(d(l, "systemctl restart nginx"), Decision::Allow(How::Always), "{l:?}");
        }
        // At Ask every command, a listed command waits like any other.
        assert_eq!(d(Level::AskAll, "uptime"), ask("Every command asks at this level"));
        assert_eq!(d(Level::AskAll, "systemctl restart nginx"), ask("Every command asks at this level"));
        assert_eq!(d(Level::Suggest, "uptime"), Decision::Refuse(SUGGEST_ONLY.into()));
        // Byte for byte: no prefix, no extra space.
        for near in [
            "systemctl restart nginx ",
            " systemctl restart nginx",
            "systemctl  restart nginx",
            "systemctl restart nginx2",
            "systemctl restart",
            "systemctl restart nginx && id",
            "Systemctl restart nginx",
        ] {
            assert_eq!(outcome(&d(Level::Ask, near)), "asks", "{near:?}");
        }
        assert_eq!(outcome(&d(Level::Ask, "systemctl restart nginx; reboot")), "red");
        // A red group wins over the list.
        assert_eq!(
            d(Level::Ask, "sudo systemctl restart nginx"),
            Decision::Ask { reason: "Always asks below Full access".into(), red: Some("privilege: sudo".into()) }
        );
        assert_eq!(outcome(&d(Level::Auto, "sudo systemctl restart nginx")), "red");
    }

    const READ_ONLY: &[&str] = &[
        "ls",
        "ls -la /var/log",
        "ls *.log",
        "ls -la ~",
        "stat /etc/hosts",
        "file /bin/ls",
        "file -b --mime-type x.png",
        "wc -l /etc/hosts",
        "du -sh /var/log",
        "df -h",
        "free -m",
        "uptime",
        "uname -a",
        "whoami",
        "id",
        "id www-data",
        "groups",
        "date",
        "date +%Y-%m-%d",
        "date -u",
        "date -d yesterday +%s",
        "date --date='2 days ago'",
        "date -Iseconds",
        "hostname",
        "pwd",
        "which nginx",
        "echo hello world",
        "echo '$HOME'",
        "printf '%s\\n' x",
        "cat /etc/os-release",
        "cat /etc/passwd",
        "cat /etc/group",
        "head -n 20 /var/log/syslog",
        "tail -n 50 /var/log/syslog",
        "tail -c 100 f",
        "tail +5 f",
        "grep -i error /var/log/syslog",
        "grep -c 'a b' f",
        "egrep 'a|b' f",
        "fgrep -x foo f",
        "cut -d: -f1 /etc/passwd",
        "sort -n f",
        "sort -rnk3 f",
        "sort -t, -k2 f",
        "uniq -c f",
        "uniq -f 1 f",
        "tr a-z A-Z",
        "column -t f",
        "nl f",
        "ps aux",
        "ps -ef",
        "ps -eo pid,user,%mem,cmd --sort=-%mem",
        "ps -o pid,etime",
        "ps -fu deploy",
        "ps -U postgres",
        "ps -C apache2",
        "pgrep -a nginx",
        "top -b -n 1",
        "top -bn1",
        "top -n 1 -b",
        "ss -tlnp",
        "netstat -tulpn",
        "ip a",
        "ip addr",
        "ip addr show",
        "ip -br addr show dev eth0",
        "ip -4 -j route list",
        "ip route",
        "ip r",
        "ip link show",
        "ip -s link",
        "lsblk",
        "lsblk -f",
        "findmnt",
        "mount",
        "lscpu",
        "systemctl",
        "systemctl status nginx",
        "systemctl status nginx --no-pager -l",
        "systemctl is-active nginx",
        "systemctl is-enabled ssh",
        "systemctl is-failed nginx",
        "systemctl list-units --type=service --state=running",
        "systemctl list-units -t service",
        "systemctl list-unit-files",
        "systemctl list-timers --all",
        "systemctl show nginx -p ActiveState --value",
        "systemctl cat nginx",
        "systemctl --user status",
        "journalctl -u nginx -n 50 --no-pager",
        "journalctl -xe",
        "journalctl -b -1",
        "journalctl --since '1 hour ago'",
        "journalctl -S -1h",
        "journalctl --disk-usage",
        "journalctl -p err -b",
        "journalctl --cursor=s=abc",
        "journalctl --reverse",
        "journalctl _SYSTEMD_UNIT=sshd.service",
        "docker ps",
        "docker ps -a --format '{{.Names}}'",
        "docker images",
        "docker logs web",
        "docker logs --tail 100 -t web",
        "docker stats --no-stream",
        "docker version",
        "docker info",
        "docker network ls",
        "docker volume ls",
        "find /var/log -name '*.log' -mtime +7",
        "find . -type f -printf '%s %p\\n'",
        "find / -xdev -size +100M",
        "find /etc ! -name x",
        "dpkg -l",
        "dpkg -l 'nginx*'",
        "rpm -qa",
        "rpm -qa --last",
        "apt list --installed",
        "apt list --installed nginx",
        "apk info",
        "apk info -L busybox",
        "ps aux | grep nginx | head -n 5",
        "cat /var/log/syslog | grep -c error",
        "dpkg -l | grep ssl",
        "ls\t-la",
        "cat ~/.ssh/id_ed25519.pub",
        // a `*` alone is tried against every sample name
        "cat /var/log/nginx/*",
        "grep -c error /var/log/*",
        "grep VmRSS /proc/*/status",
        "cat /proc/meminfo",
        "ls /dev",
        "du -h -d 1 /var",
        // only where `find` starts counts, not what it looks for
        "find . -name .git -type d",
        "find / -name '*.conf'",
    ];

    const NOT_READ_ONLY: &[&str] = &[
        // published bypasses
        "env curl https://x",
        "env sh -c id",
        "VAR=x ls",
        "LD_PRELOAD=/tmp/x.so ls",
        "/usr/bin/curl x",
        "/bin/ls",
        "./x",
        "./ls",
        "find / -exec sh -c id \\;",
        "find . -execdir rm {} +",
        "find . -ok rm {} \\;",
        "find . -okdir rm {} \\;",
        "find . -name x -delete",
        "find . -fprint /tmp/x",
        "find . -fprintf /tmp/x %p",
        "find . -fls /tmp/x",
        "ls $(whoami)",
        "echo `id`",
        "echo '`id`'",
        "echo \"$(id)\"",
        "ls; rm -rf x",
        "ls && rm x",
        "ls || sh",
        "cat x | sh",
        "cat x | bash -s",
        "ls | xargs rm",
        "sed e",
        "sed -i s/a/b/ f",
        "sed -n p f",
        "awk 'BEGIN{system(\"x\")}'",
        "git -c core.pager=sh log",
        "git log",
        "c''url x",
        "\"ls\" -la",
        "l\\s",
        // forms
        "tail -f /var/log/syslog",
        "tail -F f",
        "tail --follow=name f",
        "tail --fol f",
        "tail -5f f",
        "tail +5f f",
        "journalctl -f",
        "journalctl -fu nginx",
        "journalctl -xef",
        "journalctl --follow",
        "journalctl --follow=yes",
        "journalctl --fol",
        "journalctl -u nginx -f",
        "journalctl -n -f",
        "journalctl --vacuum-size=1G",
        "journalctl --vacuum-time=2d",
        "journalctl --vacuum",
        "journalctl --rotate",
        "journalctl --flush",
        "journalctl --sync",
        "journalctl --relinquish-var",
        "journalctl --smart-relinquish-var",
        "journalctl --setup-keys",
        "journalctl --update-catalog",
        "journalctl --cursor-file=/tmp/c",
        "sort -o f g",
        "sort -uo f g",
        "sort --output=f g",
        "sort --out f g",
        "sort --compress-program=sh f",
        "uniq a b",
        "uniq -c a b",
        "date -s 2020-01-01",
        "date --set=x",
        "date --se x",
        "date 010112002020",
        "hostname evil",
        "hostname -F /tmp/x",
        "mount /dev/sdb1 /mnt",
        "mount -a",
        "top",
        "top -n 1",
        "top -b",
        "top -b -n 5",
        "ps e",
        "ps eww",
        "ps axeww",
        "ps -e e",
        "ps -E",
        "ps auxE",
        "file -C -m x",
        "file --compile -m x",
        "ss -K dst 1.2.3.4",
        "ss -tK",
        "ss --kill",
        "ss -D /tmp/x",
        "ip",
        "ip addr add 10.0.0.1/24 dev eth0",
        "ip a a 10.0.0.1/24 dev eth0",
        "ip link set eth0 down",
        "ip l s eth0 down",
        "ip route del default",
        "ip route get 1.1.1.1",
        "ip -batch /tmp/x",
        "ip -b /tmp/x",
        "ip -force -batch x",
        "ip neigh",
        "ip rule",
        "systemctl restart nginx",
        "systemctl stop sshd",
        "systemctl -H root@host status",
        "systemctl --host=x status",
        "systemctl enable x",
        "systemctl daemon-reload",
        "docker run alpine",
        "docker exec web sh",
        "docker logs -f web",
        "docker logs --follow web",
        "docker logs -tf web",
        "docker stats",
        "docker rm web",
        "docker network create x",
        "docker -H tcp://h ps",
        "docker ps -H tcp://h",
        "docker ps --context remote",
        "docker container ls",
        "docker inspect web",
        "docker container inspect web",
        "dpkg -i x.deb",
        "dpkg -l -i x.deb",
        "rpm -e x",
        "rpm -qa --eval '%(id)'",
        "apt install x",
        "apt list --upgradable",
        "apk add x",
        "apk info -U",
        "apk --update-cache info",
        // operators, redirections, odd syntax
        "ls > out",
        "ls >> out",
        "cat < /etc/hosts",
        "cat <<EOF\nx\nEOF",
        "cat <<< x",
        "diff <(ls) <(ls a)",
        "ls &",
        "ls\nrm x",
        "(ls)",
        "ls # comment",
        "ls {a,b}",
        "echo {}",
        "! ls",
        // sensitive paths
        "cat ~/.ssh/id_rsa",
        "cat ~/.ssh/authorized_keys",
        "cat .ssh/config",
        "cat .env",
        "cat .env.production",
        "cat /etc/shadow",
        "cat /etc/shadow-",
        "cat /etc/sudoers.d/x",
        "head /root/x",
        "ls /etc/ssh",
        "cat ~/.aws/credentials",
        "cat 'C:\\Users\\u\\.aws\\credentials'",
        "cat 'app\\.env'",
        "cat /srv/app/key.pem",
        "cat ~/.netrc",
        "cat .e*",
        "cat /etc/sha*",
        "cat /etc/*",
        "cat ~/.ssh/*",
        "cat ~/../u/.ssh/id_rsa",
        "cat -- ~/.ssh/id_ed25519",
        "grep -f/etc/shadow x f",
        "wc --files0-from=/etc/shadow",
        // Every recursive search: any folder can hold secrets below it.
        "grep -rn listen /etc/nginx",
        "grep -r --include '*.conf' server_name /etc/nginx",
        "grep -r -e x -e y /var/log",
        "grep -dread -r x /srv/app",
        "grep --directories=recurse x /srv/app",
        "grep -r foo ~",
        "grep -r foo .",
        "grep -r foo",
        "grep -R x /",
        "grep -rn x /etc",
        "grep --recursive x /home",
        "grep -d recurse x /etc",
        "grep -r --include '*.conf' x",
        // folders that hold secrets wherever they are, and the folders above
        // known ones
        "grep -r . ~/.aws",
        "grep -r aws_secret_access_key ~/.aws",
        "grep -r '' .aws",
        "grep -rn . ~/.docker",
        "grep -r . ~/.kube",
        "grep -r --include=config . ~/.kube",
        "grep -r url /srv/app/.git",
        "grep -r x /srv/app/.git/refs",
        "grep -r token ~/.config",
        "grep -r token ~/.config/gh",
        "grep -r x ~/.config/gcloud",
        "grep -r x ~/.azure",
        "grep -r x ~/.cargo",
        "grep -r x ~/.local/share",
        "grep -r x ~/.c*",
        "grep -r x /home/bob",
        "grep -r x /Users/bob",
        "grep -r x /var",
        "grep -r x /var/backups",
        "grep -r x /etc/ssl",
        "grep -r x /etc/config",
        "grep -r x /etc/mysql",
        "grep -R '' /proc/self",
        "grep -r '' /proc/self/root/home/u",
        "grep -r -A 60 'PRIVATE KEY' /proc/self/cwd | tr - _",
        "grep -ra --include=environ . /proc",
        "find ~/.aws -type f",
        "find ~/.kube -type f",
        "find -L ~/.docker",
        "find -x -f ~/.aws",
        "find /srv/app/.git -name config",
        "find /proc -name environ",
        "find /proc/self/root/home/u",
        "find -files0-from list -type f",
        // more secret files and places
        "cat /proc/self/root/etc/shadow",
        "cat /proc/1/root/etc/ssh/ssh_host_ed25519_key",
        "cat /proc/self/cwd/notes",
        "cat /proc/*/root/etc/hosts",
        "cat /proc/self/r??t/etc/hosts",
        "ls -l /proc/1/fd",
        "cat /proc/1/map_files/x",
        "tail -c +4096 /proc/self/mem",
        "grep -a x /proc/kcore",
        "cat /proc/1/task/1/environ",
        "cat /var/backups/shadow.bak",
        "cat /var/backups/gshadow.bak",
        "cat /var/backups/*",
        "cat /etc/wireguard/wg0.conf",
        "cat /etc/config/wireless",
        "cat /etc/config/network",
        "cat /etc/config/*",
        "cat /etc/mysql/debian.cnf",
        "cat /etc/mysql/*",
        "cat /etc/kubernetes/admin.conf",
        "cat /etc/rancher/k3s/k3s.yaml",
        "cat /etc/letsencrypt/accounts/x/private_key.json",
        "ls /etc/ssl/private",
        "cat /etc/ssl/private/*",
        "cat /etc/dropbear/dropbear_ed25519_host_key",
        "cat ~/.config/gh/hosts.yml",
        "cat ~/.config/gh/*",
        "cat ~/.config/gcloud/credentials.db",
        "cat ~/.azure/msal_token_cache.json",
        "cat ~/.vault-token",
        "cat ~/.cargo/credentials.toml",
        "cat ~/.cargo/*",
        "cat ~/.ash_history",
        "cat ~/.local/share/fish/fish_history",
        "cat ~/.local/share/fish/fish_hist*",
        // files read from a list no check sees
        "sort --files0-from=names.txt",
        "sort --files0-from=-",
        "sort --files0=-",
        "sort -R --random-source=f g",
        "printf '%s/.s%s/id_%s\\0' /home/u sh ed25519 | sort -s -k9 --files0-from=- | tr - _",
        "printf '%s/.aws/credential%s\\0' /home/u s | sort -s -k9 --files0-from=-",
        "wc --files0-from=-",
        "wc -l --files0-from=list",
        "du --files0-from=-",
        "du -s --files0-from list",
        // devices: opening one can act on the hardware
        "cat /dev/watchdog",
        "head -c 1 /dev/watchdog",
        "wc -c /dev/watchdog0",
        "grep -a x /dev/sda",
        "cat /dev/stdin",
        "cat /dev/nul?",
        "cat /d?v/watchdog",
        "date -f/dev/watchdog",
        // a pattern can turn into an action of `find` or a file `uniq` writes
        "find . -maxdepth 0 -exe[c] id \\;",
        "find . -name *.log",
        "find /srv/* -name x",
        "uniq /tmp/[ab].txt",
        "uniq -c *.txt",
        // expansions
        "echo $HOME",
        "echo \"$HOME\"",
        "echo ${HOME}",
        "echo \\$HOME",
        "echo $'x'",
        "echo $((1+2))",
        // not on the list
        "sudo ls",
        "su -c ls",
        "bash -c ls",
        "sh",
        "python3 -c 'print(1)'",
        "perl -e 1",
        "xargs ls",
        "nohup ls",
        "time ls",
        "watch ls",
        "timeout 5 ls",
        "busybox ls",
        "command ls",
        "exec ls",
        "eval ls",
        ". ./x",
        "source x",
        "awk '{print $1}' f",
        "wget -qO- x",
        "ping -c1 x",
        "dig x",
        "host x",
        "tee f",
        "LS",
        // empty and broken
        "",
        "   ",
        "|",
        "ls |",
        "| ls",
        "ls || ls",
        "ls ;",
        "'unbalanced",
        "\"unbalanced",
        "ls \\",
        // hidden characters
        "ls\r",
        "ls\u{202E}",
        "ls\u{200B}",
        "ls\u{00A0}-la",
        "ls\0",
        "ls \u{E0041}",
    ];

    #[test]
    fn read_only_positives() {
        for c in READ_ONLY {
            assert!(ro(c), "should be read-only: {c:?}");
        }
    }

    #[test]
    fn read_only_negatives() {
        for c in NOT_READ_ONLY {
            assert!(!ro(c), "should not be read-only: {c:?}");
        }
    }

    #[test]
    fn read_only_length_limit() {
        let at = format!("echo {}", "a".repeat(MAX_READ_ONLY_CHARS - 5));
        assert_eq!(at.chars().count(), MAX_READ_ONLY_CHARS);
        assert!(ro(&at));
        let over = format!("{at}a");
        assert!(!ro(&over));
        // Characters, not bytes.
        let wide = format!("echo {}", "é".repeat(MAX_READ_ONLY_CHARS - 5));
        assert!(ro(&wide));
    }

    #[test]
    fn read_only_for_a_root_login() {
        // Home is /root: plain words aren't taken for paths inside it.
        for c in ["systemctl status nginx", "grep error /var/log/syslog", "ps aux | grep python", "ls"] {
            assert!(read_only(c, "/root", true), "{c:?}");
        }
        // A root login's own home is its work folder; secrets by name still count.
        for c in ["cat ~/notes.txt", "cat ./notes.txt", "ls /root", "ls ~"] {
            assert!(read_only(c, "/root", true), "{c:?}");
        }
        for c in ["cat .env", "cat id_rsa", "cat ~/.ssh/id_rsa", "cat /root/.ssh/authorized_keys"] {
            assert!(!read_only(c, "/root", true), "{c:?}");
        }
    }

    #[test]
    fn ps_selects_no_environment() {
        // On Linux a single-dash `e` only selects all processes.
        for c in ["ps -ef", "ps -eo pid,cmd", "ps -eo pid,user,%mem,cmd --sort=-%mem"] {
            assert!(read_only(c, HOME, true), "{c:?}");
            // Not on other kernels: on the BSDs `-e` prints the environment.
            assert!(!read_only(c, HOME, false), "{c:?}");
        }
        // `E` prints it on every ps, and so does a BSD-style `e` word.
        for c in ["ps -E", "ps auxE", "ps e", "ps axeww"] {
            assert!(!read_only(c, HOME, true), "{c:?}");
            assert!(!read_only(c, HOME, false), "{c:?}");
        }
        // Option values are skipped, and a BSD-style word with no `e` is fine:
        // these stay read-only on either kernel.
        for c in ["ps aux", "ps -o pid,etime", "ps -fu deploy", "ps -U postgres", "ps -C apache2"] {
            assert!(read_only(c, HOME, true), "{c:?}");
            assert!(read_only(c, HOME, false), "{c:?}");
        }
    }

    /// What Ask gives a call for a login whose home is `home`.
    fn at_ask(home: &str, call: Call) -> &'static str {
        let c = Ctx { connected: true, posix: true, linux: true, home, written: &[], always_run: &[], files_only: false };
        outcome(&decide(Level::Ask, &call, &c))
    }

    #[test]
    fn searches_where_secrets_are_ask() {
        let run = |command| Call::Run { command };
        for (home, cmd) in [
            (HOME, "grep -r aws_secret_access_key ~/.aws"),
            (HOME, "grep -rn . ~/.docker"),
            (HOME, "grep -r . ~/.kube"),
            (HOME, "grep -r token ~/.config"),
            (HOME, "find ~/.aws -type f"),
            (HOME, "grep -r '' .aws"),
            (HOME, "grep -r url /srv/app/.git"),
            (HOME, "grep -r '' /proc/self/root/home/u"),
            (HOME, "grep -r -A 60 'PRIVATE KEY' /proc/self/cwd | tr - _"),
            (HOME, "grep -R '' /proc/self"),
            ("/root", "cat /proc/self/root/etc/shadow"),
            ("/root", "cat /proc/self/root/etc/ssh/ssh_host_ed25519_key"),
            ("/root", "cat /proc/1/root/etc/ssh/ssh_host_ed25519_key"),
            ("/root", "grep -r . /etc/ssl/private"),
            ("/root", "grep -r . /home/bob"),
            ("/root", "grep -ra --include=environ . /proc"),
            ("/root", "grep -r '' /etc/letsencrypt | tr - _"),
            ("/root", "cat /etc/wireguard/wg0.conf"),
            ("/root", "cat /dev/watchdog"),
            (HOME, "sort --files0-from=names.txt"),
            (HOME, "printf '%s/.s%s/id_%s\\0' /home/u sh ed25519 | sort -s -k9 --files0-from=- | tr - _"),
            (HOME, "printf '%s/.aws/credential%s\\0' /home/u s | sort -s -k9 --files0-from=-"),
            (HOME, "wc --files0-from=-"),
            (HOME, "find . -maxdepth 0 -exe[c] id \\;"),
            (HOME, "uniq /tmp/[ab].txt"),
        ] {
            assert_ne!(at_ask(home, run(cmd)), "runs", "{cmd:?} with home {home}");
        }
        // Read through `/proc`, a path is red like the path it leads to.
        assert_eq!(at_ask("/root", run("cat /proc/self/root/etc/shadow")), "red");
        assert_eq!(at_ask("/root", run("cat /etc/shadow")), "red");
        // Every recursive search asks, a root login's own work in /root too.
        assert_eq!(at_ask("/root", run("grep -rn listen /root/app")), "asks");
        // A device among the arguments of a program that never opens it.
        assert_eq!(at_ask(HOME, run("ls -l /dev/sda")), "runs");
        assert_eq!(at_ask(HOME, run("stat /dev/sda1")), "runs");
        assert_ne!(at_ask(HOME, run("cat /dev/sda")), "runs");
    }

    #[test]
    fn more_secret_files_ask() {
        for (home, path) in [
            ("/root", "/var/backups/shadow.bak"),
            ("/root", "/etc/wireguard/wg0.conf"),
            ("/root", "/etc/config/wireless"),
            ("/root", "/root/.ash_history"),
            ("/root", "/etc/kubernetes/admin.conf"),
            ("/root", "/etc/mysql/debian.cnf"),
            ("/root", "/etc/rancher/k3s/k3s.yaml"),
            ("/root", "/etc/dropbear/dropbear_ed25519_host_key"),
            (HOME, "/home/u/.config/gh/hosts.yml"),
        ] {
            assert_eq!(at_ask(home, Call::Read { path }), "asks", "{path:?} with home {home}");
        }
        // Dropbear keeps root's login keys there on OpenWrt.
        let c = Ctx { connected: true, posix: true, linux: true, home: "/root", written: &[], always_run: &[], files_only: false };
        let write = Call::Write { path: "/etc/dropbear/authorized_keys" };
        assert_eq!(outcome(&decide(Level::Auto, &write, &c)), "asks");
        let command = "echo 'ecdsa-sha2-nistp256 AAAAE2VjZHNh x' >> /etc/dropbear/authorized_keys";
        assert_eq!(outcome(&decide(Level::Auto, &Call::Run { command }, &c)), "red");
    }

    #[test]
    fn find_starting_points() {
        assert_eq!(find_starts(&["/var/log", "-name", "x"]), ["/var/log"]);
        assert_eq!(find_starts(&["-L", "a", "b", "(", "-name", "x", ")"]), ["a", "b"]);
        assert_eq!(find_starts(&["-D", "tree", "-O3", "a"]), ["a"]);
        assert_eq!(find_starts(&["-x", "-f", "a", "b"]), ["a", "b"]);
        assert_eq!(find_starts(&["!", "-name", "x"]), ["."]);
        assert_eq!(find_starts(&["-name", "x"]), ["."]);
        assert_eq!(find_starts(&[]), ["."]);
    }

    #[test]
    fn a_lone_star_tries_every_sample() {
        // Every sample, and each part of a deep home, is one path to try.
        let deep = "/srv/apps/team/deploy/home/x";
        for c in ["cat /var/log/nginx/*", "grep -c error /var/log/*"] {
            assert!(read_only(c, deep, true), "{c:?}");
        }
        assert!(!read_only("cat /etc/config/*", deep, true));
        // Two of them are too many to try, so they count as a match.
        assert!(!read_only("cat /var/*/*", HOME, true));
    }

    #[test]
    fn privilege() {
        expect("sudo apt update", "privilege: sudo");
        expect("SUDO ls", "privilege: SUDO");
        expect("su -", "privilege: su");
        expect("doas reboot", "privilege: doas");
        expect("pkexec visudo", "privilege: pkexec");
        expect("/usr/bin/sudo -i", "privilege: sudo");
        expect("sudo-rs ls", "privilege: sudo");
        expect("ls | sudo tee x", "privilege: sudo");
        expect("grep x f && sudo id", "privilege: sudo");
        expect("sudoedit /etc/hosts", "privilege: sudoedit");
        expect("SUDO_EDITOR='cp x' sudoedit /etc/hosts", "privilege: sudoedit");
        clean("echo pseudo");
        clean("ls -su");
        clean("grep sudo /var/log/auth.log");
    }

    #[test]
    fn removal() {
        expect("rm -rf /tmp/x", "removal: rm -rf");
        expect("rm -r d", "removal: rm -r");
        expect("rm -f x", "removal: rm -f");
        expect("rm -Rf d", "removal: rm -Rf");
        expect("rm --recursive d", "removal: rm --recursive");
        expect("rm x -rf", "removal: rm x -rf");
        expect("/bin/rm -fr /", "removal: rm -fr");
        expect("\"rm\" -rf x", "removal: rm -rf");
        expect("r\\m -rf x", "removal: rm -rf");
        expect("find /tmp -name '*.tmp' -delete", "removal: find -delete");
        expect("shred -u f", "removal: shred");
        expect("wipefs -a /dev/sdb", "removal: wipefs");
        expect("truncate -s 0 /var/log/x", "removal: truncate");
        // Every rm, unlink and rmdir, whatever its options.
        expect("rmdir d", "removal: rmdir d");
        expect("rm x", "removal: rm x");
        expect("rm -i x", "removal: rm -i x");
        expect("rm ~/*", "removal: rm ~/*");
        expect("rm --recur /srv/data", "removal: rm --recur /srv/data");
        expect("unlink /srv/app/db.sqlite3", "removal: unlink /srv/app/db.sqlite3");
        expect("cd /srv && xargs rm < list", "removal: rm");
        // Containers, cluster objects, untracked files, databases.
        expect("docker volume rm pgdata", "removal: docker volume rm");
        expect("docker system prune -af --volumes", "removal: docker system prune");
        expect("kubectl delete namespace prod", "removal: kubectl delete");
        expect("helm uninstall web", "removal: helm uninstall");
        expect("git clean -fdx", "removal: git clean");
        expect("mysql -e 'DROP DATABASE shop'", "removal: DROP DATABASE");
        expect("redis-cli FLUSHALL", "removal: FLUSHALL");
        for c in [
            "docker image prune -a",
            "podman rmi alpine",
            "docker compose down -v",
            "psql -c 'drop table users'",
            "echo 'DROP SCHEMA x' | mysql",
            "mongosh --eval 'db.dropDatabase()'",
            "mysqladmin -f drop shop",
            "dropdb shop",
        ] {
            in_group(c, REMOVAL);
        }
        clean("docker run --rm alpine true");
        clean("docker compose down");
        clean("grep 'DROP TABLE' schema.sql");
        clean("kubectl get pods");
        clean("git status");
        clean("grep -r foo /srv");
        clean("find . -name '*.log'");
    }

    #[test]
    fn disks() {
        expect("mkfs.ext4 /dev/sdb1", "disks: mkfs.ext4");
        expect("dd if=/dev/zero of=/dev/sda bs=1M", "disks: dd of=");
        expect("fdisk -l", "disks: fdisk");
        expect("sfdisk /dev/sda < t", "disks: sfdisk");
        expect("parted /dev/sda print", "disks: parted");
        expect("mdadm --detail /dev/md0", "disks: mdadm");
        expect("lvremove vg/lv", "disks: lvremove");
        expect("echo x > /dev/sda", "disks: > /dev/sd");
        expect("cat img >/dev/nvme0n1", "disks: >/dev/nvme");
        expect("mke2fs -F /dev/sdb1", "disks: mke2fs");
        expect("mkswap /dev/sdb1", "disks: mkswap");
        expect("zpool destroy tank", "disks: zpool destroy");
        // Any write into a device, however it is written.
        expect("cp disk.img /dev/sdb", "disks: cp /dev/sdb");
        expect("cat disk.img > /dev/md0", "disks: > /dev/md0");
        expect("cat disk.img | tee /dev/sdb >/dev/null", "disks: tee /dev/sdb");
        expect("cat img > /dev/mapper/vg-root", "disks: > /dev/mapper/vg-root");
        expect("cd /dev && cat img > sdb", "disks: > sdb");
        for c in [
            "zfs destroy -r tank/data",
            "pvcreate -ff -y /dev/sdb",
            "cryptsetup erase -q /dev/sdb",
            "cryptsetup luksFormat /dev/sdb",
            "nvme format /dev/nvme0n1",
            "badblocks -wsv /dev/sdb",
            "hdparm --security-erase x /dev/sda",
            "cat img > /dev/disk/by-id/x",
            "dd if=img of=/dev/dm-0",
        ] {
            in_group(c, DISKS);
        }
        clean("dd if=/dev/sda bs=512 count=1");
        clean("ls -l /dev/sda");
        clean("echo x > /dev/null");
        clean("ls 2>/dev/null >/dev/stderr");
        clean("echo x > /dev/fd/2; cat x > /dev/tty; echo y > /dev/shm/y");
        clean("zpool status");
        clean("zfs list");
        clean("nvme list");
    }

    #[test]
    fn power() {
        expect("reboot", "power: reboot");
        expect("shutdown -h now", "power: shutdown");
        expect("poweroff", "power: poweroff");
        expect("halt", "power: halt");
        expect("init 0", "power: init 0");
        expect("telinit 6", "power: telinit 6");
        expect("systemctl reboot", "power: reboot");
        expect("systemctl isolate rescue.target", "power: systemctl isolate");
        expect("systemctl --force kexec", "power: systemctl --force kexec");
        expect("systemctl suspend", "power: systemctl suspend");
        expect("kexec -e", "power: kexec");
        expect("killall5 -9", "power: killall5");
        expect("kill -9 -1", "power: kill -9 -1");
        expect("pkill -u deploy", "power: pkill -u");
        // A write into the kernel's files.
        expect("echo b > /proc/sysrq-trigger", "power: > /proc/sysrq-trigger");
        expect("echo mem > /sys/power/state", "power: > /sys/power/state");
        expect("echo '|/tmp/x' | tee /proc/sys/kernel/core_pattern", "power: tee /proc/sys/kernel/core_pattern");
        for c in [
            "kill -- -1",
            "kill -s KILL -1",
            "kill -TERM -4321",
            "kill 1234 -1",
            "killall -u bob",
            "pkill -U 1000",
            "pkill --euid deploy",
            "cd /proc && echo o > sysrq-trigger",
            "cp x /sys/class/net/eth0/mtu",
        ] {
            in_group(c, POWER);
        }
        clean("init 3");
        clean("systemctl status nginx");
        clean("kill -9 1234");
        clean("kill -HUP 1234");
        clean("kill -1");
        clean("kill %1");
        clean("pkill -USR1 nginx");
        clean("cat /proc/sys/kernel/core_pattern");
    }

    #[test]
    fn remote_access_firewall() {
        let g = "remote access, firewall";
        expect("iptables -F", "remote access, firewall: iptables");
        for c in ["ip6tables -L", "nft flush ruleset", "ufw disable", "firewall-cmd --reload"] {
            in_group(c, g);
        }
        expect("systemctl stop sshd", "remote access, firewall: systemctl stop sshd");
        for c in [
            "systemctl restart ssh",
            "systemctl disable --now ssh.service",
            "systemctl mask ssh.socket",
            "systemctl kill sshd",
            "service ssh stop",
            "service sshd restart",
            "/etc/init.d/ssh stop",
            "pkill sshd",
            "killall -9 sshd",
            "rc-service sshd stop",
            "sv down sshd",
            // Dropbear (OpenWrt) and the network itself
            "/etc/init.d/dropbear stop",
            "/etc/init.d/dropbear disable",
            "service dropbear stop",
            "/etc/init.d/network restart",
            "ip link set eth0 down",
            "ip l s dev eth0 down",
            "ip -4 addr flush dev eth0",
            "ip route del default",
            "ip r replace default via 10.0.0.1",
            "ip link delete br0",
            "ifdown eth0",
            "ifconfig eth0 down",
            "nmcli networking off",
            "nmcli con down id x",
            "systemctl stop networking",
            "systemctl restart NetworkManager",
            "systemctl disable --now systemd-networkd",
            "wifi down",
            "apt-get remove -y openssh-server",
            "apt purge openssh-server",
            "opkg remove dropbear",
            "dnf erase openssh-server",
            "pacman -Rns openssh",
        ] {
            in_group(c, g);
        }
        expect("ip link set eth0 down", "remote access, firewall: ip link set eth0 down");
        clean("ip link show");
        clean("ip -br addr show dev eth0");
        clean("ip route get 1.1.1.1");
        clean("ip link set eth0 up");
        clean("nmcli dev status");
        clean("systemctl status networking");
        clean("/etc/init.d/dropbear status");
        clean("systemctl status sshd");
        clean("systemctl status ssh");
        clean("systemctl is-active ssh.service");
        clean("journalctl -u ssh -n 20");
        clean("systemctl restart nginx");
        clean("systemctl restart nginx sshguard");
    }

    #[test]
    fn accounts() {
        expect("passwd root", "accounts: passwd");
        expect("echo 'u:p' | chpasswd", "accounts: chpasswd");
        expect("usermod -aG docker bob", "accounts: usermod");
        expect("userdel -r bob", "accounts: userdel");
        expect("groupdel g", "accounts: groupdel");
        expect("visudo", "accounts: visudo");
        expect("useradd -m eve", "accounts: useradd");
        clean("cat /etc/passwd");
        clean("grep bob /etc/passwd");
        clean("getent passwd bob");
        clean("id bob");
    }

    #[test]
    fn mass_permission_changes() {
        expect("chmod -R 755 /var/www", "mass permission changes: chmod -R");
        expect("chown -R www:www /srv", "mass permission changes: chown -R");
        expect("chgrp --recursive g d", "mass permission changes: chgrp --recursive");
        in_group("chown user:grp -R d", "mass permission changes");
        clean("chmod 644 f");
        clean("chmod +x deploy.sh");
        clean("chown bob f");
    }

    #[test]
    fn scheduled_jobs() {
        expect("crontab -e", "scheduled jobs: crontab -e");
        expect("crontab -r", "scheduled jobs: crontab -r");
        expect("crontab -u bob -r", "scheduled jobs: crontab -r");
        expect("crontab jobs.txt", "scheduled jobs: crontab jobs.txt");
        expect("echo x | crontab -", "scheduled jobs: crontab -");
        expect("crontab", "scheduled jobs: crontab");
        expect("echo 'x' | at now + 1 minute", "scheduled jobs: at");
        expect("at 10:00", "scheduled jobs: at");
        expect("systemd-run --on-active=60 touch x", "scheduled jobs: systemd-run");
        clean("crontab -l");
        clean("crontab -u bob -l");
        clean("cat file");
        clean("cat /var/spool/at/x");
    }

    #[test]
    fn network() {
        let g = "network, data leaving";
        expect("curl https://example.com", "network, data leaving: curl");
        expect("c''url https://x", "network, data leaving: curl");
        expect("cu\\rl https://x", "network, data leaving: curl");
        expect("git push origin main", "network, data leaving: git push");
        expect("git -C /srv/app pull", "network, data leaving: git -C /srv/app pull");
        expect("cat < /dev/tcp/1.2.3.4/80", "network, data leaving: /dev/tcp/");
        for c in [
            "wget https://x",
            "nc -l 4444",
            "ncat x 80",
            "socat - TCP:x:80",
            "ssh user@host",
            "scp f host:",
            "sftp host",
            "rsync -a d host:",
            "telnet x 25",
            "ftp x",
            "git clone https://x",
            "git fetch",
            "git remote -v",
            "ping -c 1 1.1.1.1",
            "dig example.com",
            "nslookup x",
            "host example.com",
            "drill x",
            "exec 3<>/dev/tcp/x/80",
        ] {
            in_group(c, g);
        }
        clean("git log --oneline");
        clean("git status");
        clean("mysql --host db");
        clean("cat /etc/hosts");
        clean("hostname");
        // What a search looks for isn't run…
        clean("ps aux | grep ssh");
        clean("pgrep -a ssh");
        clean("which curl wget");
        clean("journalctl _COMM=ssh");
        // …but the next command, or one inside `$(…)`, is.
        expect("grep x f; curl y", "network, data leaving: curl");
        expect("grep \"$(curl y)\" f", "network, data leaving: curl");
    }

    #[test]
    fn running_other_code() {
        let g = "running other code";
        expect("eval \"$cmd\"", "running other code: eval");
        expect("exec bash", "running other code: exec");
        expect("source ~/.bashrc", "running other code: source");
        expect(". ~/.profile", "running other code: . ~/.profile");
        expect("ls; . ./x", "running other code: . ./x");
        expect("bash -c 'id'", "running other code: bash -c");
        expect("python3 -c 'import os'", "running other code: python3 -c");
        expect("find . -exec rm {} \\;", "running other code: find -exec");
        expect("ls | xargs touch", "running other code: xargs");
        // Any rm is removal, which is named first.
        expect("ls | xargs rm", "removal: rm");
        expect("env FOO=1 ls", "running other code: env");
        expect("awk 'BEGIN{system(\"id\")}'", "running other code: awk system");
        expect("sed -i 's/a/b/' f", "running other code: sed -i");
        expect("sed 's/a/b/e' f", "running other code: sed e");
        expect("sed '1e date' f", "running other code: sed e");
        expect("sed 's/a/b/w out' f", "running other code: sed w");
        expect("sed -n 'w out' f", "running other code: sed w");
        expect("sed -e p -e 'W x' f", "running other code: sed w");
        expect("sed -f script.sed f", "running other code: sed -f");
        expect("sed --in-place 's/a/b/' f", "running other code: sed -i");
        expect("cat x | sh", "running other code: | sh");
        expect("echo id | /bin/sh", "running other code: | /bin/sh");
        for c in [
            "sh -c id",
            "bash -lc id",
            "perl -e 'print 1'",
            "perl -pi -e 's/a/b/' f",
            "ruby -e 1",
            "node -e 1",
            "php -r 'echo 1;'",
            "/usr/bin/env python3 x.py",
            "cat x | bash -s",
            "bash < script.sh",
            "python3 - <<'EOF'\nprint(1)\nEOF",
        ] {
            in_group(c, g);
        }
        clean("sed -n 's/a/b/p' f");
        clean("sed 's|/usr|/opt|g' f");
        clean("sed -n '/start/,/end/{p}' f");
        clean("sed -E 's/(a|b)+/x/2' f");
        clean("bash script.sh");
        clean("python3 x.py");
        clean("ps aux | grep python");
        clean("find . -name '*.py'");
        clean("docker run --env A=1 x");
        clean("cat .envrc");
    }

    #[test]
    fn hidden_code() {
        expect("echo $(id)", "hidden code: $(");
        expect("echo `id`", "hidden code: `");
        expect("diff <(ls a) <(ls b)", "hidden code: <(");
        expect("echo $'\\x41'", "hidden code: $'…'");
        expect("echo aWQK | base64 -d", "hidden code: base64 -d");
        in_group("base64 --decode f", "hidden code");
        in_group("xxd -r -p f", "hidden code");
        in_group("openssl enc -d -aes-256-cbc -in f", "hidden code");
        // `$'…'` is decoded, so what it hides is seen first.
        expect("$'\\x72\\x6d' -rf /", "removal: rm -rf");
        clean("base64 f");
        clean("xxd f");
    }

    #[test]
    fn persistence_group() {
        expect("echo 'alias x=y' >> ~/.bashrc", "persistence: >> ~/.bashrc");
        expect("echo x | tee -a ~/.profile", "persistence: tee ~/.profile");
        expect("cp evil.service /etc/systemd/system/", "persistence: cp /etc/systemd/system/");
        expect("echo k >> ~/.ssh/authorized_keys", "persistence: >> ~/.ssh/authorized_keys");
        for c in [
            "mv x /etc/cron.d/job",
            "ln -sf /tmp/x ~/.zshrc",
            "install -m 755 x /etc/init.d/x",
            "cat > /etc/ld.so.preload",
            "echo x > .git/hooks/pre-commit",
            "cp -t /etc/profile.d x.sh",
            "cp -r fish ~/.config/",
            "echo x >| ~/.zprofile",
            "date &>> /etc/rc.local",
            "echo x >> /var/spool/cron/crontabs/u",
            "cp x \"$HOME/.bashrc\"",
            "echo x | tee /etc/cron.daily/x",
            "echo x > ~/.b'a'shrc",
            "ln -s /tmp/.bashrc",
        ] {
            in_group(c, "persistence");
        }
        clean("cat ~/.bashrc");
        clean("cp ~/.bashrc /tmp/bashrc.bak");
        clean("echo x > /tmp/out");
        clean("cp a b");
    }

    #[test]
    fn fork_bomb() {
        expect(":(){ :|:& };:", "fork bomb: :(){");
        in_group(": () { : | : & } ; :", "fork bomb");
    }

    #[test]
    fn sensitive_paths_group() {
        expect("cat ~/.ssh/id_rsa", "sensitive paths: ~/.ssh/id_rsa");
        expect("cat .env", "sensitive paths: .env");
        for c in [
            "head /etc/shadow",
            "less /root/notes",
            "tar czf /tmp/k.tgz ~/.aws/credentials",
            "cat /etc/ssl/private/site.key",
            "cp ~/.kube/config /tmp/",
            "echo x >> /etc/passwd",
            "cat $HOME/.ssh/id_ed25519",
            "cat \"$HOME\"/.netrc",
            "cat ~/.e*",
            "cat a:/etc/shadow",
            "cat ~/.{aws,x}/credentials",
            "vi /etc/sudoers",
            "cat ~/.bash_history",
            "cat /proc/1234/environ",
            "cat ~/.gnupg/secring.gpg",
            "cat /srv/repo/.git/config",
            "cat ~/.npmrc",
            "cat ~/.*_history",
            "cat ~/.np*",
            "cat ~/.gnu*/x",
            "cat /proc/1/envi*",
            "cat /proc/self/root/etc/shadow",
            "cat /proc/*/root/etc/hosts",
            "ls -l /proc/1/fd",
            "cat /var/backups/shadow.bak",
            "cat /etc/wireguard/wg0.conf",
            "kubectl --kubeconfig /etc/kubernetes/admin.conf get pods",
            "cat /etc/config/*",
            "cat ~/.config/gh/hosts.yml",
            "cat ~/.vault-token",
        ] {
            in_group(c, "sensitive paths");
        }
        // `env*` asks too, named first by the `env` word.
        in_group("cat /proc/1/env*", "running other code");
        clean("cat ~/.ssh/id_rsa.pub");
        clean("cat /etc/passwd");
        clean("ls ~");
        clean("cat /etc/shadowsocks.conf");
        clean("grep VmRSS /proc/*/status");
        clean("cat /etc/mysql/my.cnf");
    }

    #[test]
    fn own_files() {
        let written = vec!["/home/u/deploy.sh".to_string(), "/opt/app/run.py".to_string()];
        let c = ctx(&written, &[]);
        let own = |cmd: &str| always_ask(cmd, &c);
        assert_eq!(own("./deploy.sh").as_deref(), Some("the assistant's own files: ./deploy.sh"));
        for cmd in [
            "bash deploy.sh",
            "sh ~/deploy.sh",
            "nohup ./deploy.sh &",
            "deploy.sh",
            "python3 /opt/app/run.py",
            "python3 run.py",
            "python3 -u run.py",
            "cd /tmp; /home/u/deploy.sh",
            "timeout 60 ./deploy.sh",
            "FOO=1 ./deploy.sh",
            "/home/u/./x/../deploy.sh",
            "ls && ~/deploy.sh",
        ] {
            assert!(own(cmd).is_some_and(|g| g.starts_with("the assistant's own files: ")), "{cmd:?}: {:?}", own(cmd));
        }
        for cmd in ["cat deploy.sh", "ls ./deploy.sh", "chmod +x deploy.sh", "cp deploy.sh /tmp/"] {
            assert_eq!(own(cmd), None, "{cmd:?}");
        }
        // Nothing written: nothing to match.
        clean("./deploy.sh");
    }

    #[test]
    fn unparsable_commands_match_the_text_only() {
        // An unbalanced quote: no words, but the text still matches.
        expect("curl 'x", "network, data leaving: curl");
        in_group("echo x >> ~/.bashrc '", "persistence");
        in_group("cat ~/.ssh/id_rsa \"", "sensitive paths");
        in_group("sed -n p f '", "running other code");
        in_group("crontab -l '", "scheduled jobs");
        let written = vec!["/home/u/deploy.sh".to_string()];
        assert!(always_ask("./deploy.sh '", &ctx(&written, &[])).is_some());
    }

    #[test]
    fn sensitive_paths() {
        for (p, write) in [
            ("~/.ssh/id_ed25519", false),
            ("/home/u/.ssh/config", false),
            ("/home/u/.ssh", false),
            ("~/.ssh/authorized_keys", false),
            ("~/.ssh/authorized_keys", true),
            (".ssh/known_hosts", false),
            ("/home/bob/.ssh/authorized_keys", true),
            ("/etc/ssh/sshd_config", false),
            ("/etc/ssh/ssh_host_ed25519_key", false),
            ("/etc/shadow", false),
            ("/etc/shadow-", false),
            ("/etc/gshadow", false),
            ("/etc/sudoers", false),
            ("/etc/sudoers.d/90-cloud", false),
            ("/etc/passwd", true),
            ("/etc/passwd-", true),
            ("/etc/group", true),
            ("/root/.bash_history", false),
            ("/root", false),
            ("~root/.profile", false),
            ("/boot/grub/grub.cfg", false),
            ("/srv/tls/site.pem", false),
            ("/x/y.KEY", false),
            ("cert.p12", false),
            ("a.pfx", false),
            ("vault.kdbx", false),
            ("/opt/backup/id_rsa", false),
            ("/tmp/id_ed25519_old", false),
            ("id_ecdsa", false),
            ("/srv/app/.env", false),
            ("/srv/app/.env.production", false),
            (".netrc", false),
            ("~/.pgpass", false),
            ("/home/u/.git-credentials", false),
            ("~/.aws/credentials", false),
            ("/home/bob/.docker/config.json", false),
            ("~/.kube/config", false),
            ("/home/u/.bash_history", false),
            ("~/.zsh_history", false),
            ("/home/u/.mysql_history", false),
            ("/home/u/.node_repl_history", false),
            ("~/.my.cnf", false),
            ("/home/u/.npmrc", false),
            ("~/.pypirc", false),
            ("/srv/repo/.git/config", false),
            ("~/.gnupg/secring.gpg", false),
            ("/home/u/.gnupg", false),
            ("/proc/1234/environ", false),
            ("/proc/self/environ", false),
            ("/proc/1/task/2/environ", false),
            ("/proc/self/root/etc/shadow", false),
            ("/proc/1/root", false),
            ("/proc/thread-self/cwd", false),
            ("/proc/1/task/2/root/etc/hosts", false),
            ("/proc/1/fd/3", false),
            ("/proc/1/map_files/7f00-7f01", false),
            ("/proc/self/mem", false),
            ("/proc/kcore", false),
            ("/var/backups/shadow.bak", false),
            ("/var/backups/gshadow.bak", false),
            ("/etc/ssl/private/site", false),
            ("/etc/letsencrypt/accounts/x/private_key.json", false),
            ("/etc/wireguard/wg0.conf", false),
            ("/etc/kubernetes/admin.conf", false),
            ("/etc/rancher/k3s/k3s.yaml", false),
            ("/etc/dropbear/dropbear_ed25519_host_key", false),
            ("/etc/dropbear/authorized_keys", true),
            ("/etc/mysql/debian.cnf", false),
            ("/etc/config/wireless", false),
            ("/etc/config/network", false),
            ("~/.ash_history", false),
            ("/home/u/.local/share/fish/fish_history", false),
            ("~/.config/gh/hosts.yml", false),
            ("/home/bob/.config/gcloud/credentials.db", false),
            ("~/.config/gcloud", false),
            ("~/.azure/msal_token_cache.json", false),
            ("~/.vault-token", false),
            ("~/.cargo/credentials", false),
            ("~/.cargo/credentials.toml", false),
            ("../../etc/shadow", false),
            ("/var/www/../../etc/shadow", false),
            ("~/x/../.ssh/id_rsa", false),
            ("//etc//./shadow", false),
            // Written with backslashes, as a Windows server (or MSYS there)
            // reads them.
            (r"C:\Users\u\.aws\credentials", false),
            (r"/C:/Users/u/.ssh\id_rsa", false),
            (r"app\.env", false),
            (r"site\conf\server.key", false),
        ] {
            assert!(sensitive(p, HOME, write), "should be sensitive: {p:?} write={write}");
        }
        for (p, write) in [
            ("~/.ssh/id_ed25519.pub", false),
            ("/home/u/.ssh/id_rsa.pub", true),
            ("/opt/keys/id_rsa.pub", false),
            ("/etc/passwd", false),
            ("/etc/group", false),
            ("/etc/hosts", true),
            ("/etc/shadowsocks/config.json", false),
            ("/srv/app/.envrc", false),
            ("/srv/app/env", false),
            ("/home/u/notes.txt", true),
            ("/rootfs/x", false),
            ("/bootstrap.sh", false),
            ("/home/u/.kube/config.bak", false),
            ("/home/u/.aws/config", false),
            ("/etc/sshd", false),
            ("/etc/ssh/../hosts", false),
            ("/home/u/.gitconfig", false),
            ("/proc/cpuinfo", false),
            ("/proc/1/cmdline", false),
            ("/proc/1/status", false),
            ("/proc/1/fdinfo/3", false),
            ("/proc/root/status", false),
            ("/proc/sys/kernel/hostname", false),
            ("/proc", false),
            ("/var/backups/dpkg.status.0", false),
            ("/etc/ssl/certs/ca-certificates.crt", false),
            ("/etc/mysql/my.cnf", false),
            ("/etc/config/system", false),
            ("/home/u/.config/nvim/init.lua", false),
            ("/srv/ansible/hosts.yml", false),
            ("/home/u/.cargo/config.toml", false),
            ("/home/u/.aws", false),
        ] {
            assert!(!sensitive(p, HOME, write), "should not be sensitive: {p:?} write={write}");
        }
        // Root's own home is its work folder, but these are secret there too.
        for p in ["~/.ash_history", "/proc/self/root/root/.ssh/id_rsa", "/var/backups/shadow.bak", "~/.config/gh/hosts.yml"] {
            assert!(sensitive(p, "/root", false), "{p:?}");
        }
        // Home from the server; `~` follows it.
        assert!(!sensitive("~/notes", "/root", false), "a root login's own home");
        assert!(sensitive("~/.ssh/id_ed25519", "/root", false));
        assert!(sensitive("/root/notes", "/home/u", false), "another login reading /root");
        assert!(!sensitive("~/notes", "/home/u", false));
        assert!(sensitive("~/.ssh/id_rsa", "", false));
    }

    #[test]
    fn persistence_paths() {
        for p in [
            "~/.bashrc",
            "/home/u/.bash_profile",
            ".profile",
            "~/.zshrc",
            "~/.zprofile",
            "~/.config/fish/config.fish",
            "~/.config/fish",
            "~/.config/systemd/user/x.service",
            "/etc/profile",
            "/etc/profile.d/x.sh",
            "/etc/bash.bashrc",
            "/etc/environment",
            "/etc/crontab",
            "/etc/cron.d/job",
            "/etc/cron.daily/x",
            "/var/spool/cron/crontabs/root",
            "/etc/systemd/system/x.service",
            "/etc/systemd",
            "/lib/systemd/system/x.service",
            "/usr/lib/systemd/system/x.service",
            "/etc/init.d/x",
            "/etc/rc.local",
            "/srv/repo/.git/hooks/pre-commit",
            "/srv/repo/.git/hooks",
            "/etc/ld.so.preload",
            "~/.ssh/authorized_keys",
            "~/x/../.bashrc",
            "/home/u/./.zshrc",
        ] {
            assert!(persistence(p, HOME), "should be a persistence path: {p:?}");
        }
        for p in [
            "~/.bashrc.bak",
            "/home/u/x/.bashrc",
            "/etc/profile.old",
            "/etc/hosts",
            "/srv/repo/.git/config",
            "/etc/systemd-x",
            "/var/spool/cronx",
            "~/.config/fishy",
            "/usr/lib/systemdx/user",
        ] {
            assert!(!persistence(p, HOME), "should not be a persistence path: {p:?}");
        }
        assert!(persistence("/.bashrc", "/"));
        assert!(persistence("~/.bashrc", ""));
        assert!(persistence(".bashrc", "/root"));
    }

    #[test]
    fn parser_basics() {
        let words = |s: &str| -> Vec<String> {
            parse(s)
                .unwrap_or_default()
                .into_iter()
                .map(|t| match t {
                    Tok::Word(w) => w.text,
                    Tok::Op(o) => format!("<{o}>"),
                    Tok::Body(b) => format!("[{b}]"),
                    Tok::Comment => "#".into(),
                })
                .collect()
        };
        assert_eq!(words("a 'b c' \"d $e\" f\\ g"), ["a", "b c", "d $e", "f g"]);
        assert_eq!(words("\"a\\\"b\\n\""), ["a\"b\\n"]);
        assert_eq!(words("a|b||c&&d;e&f|&g"), ["a", "<|>", "b", "<||>", "c", "<&&>", "d", "<;>", "e", "<&>", "f", "<|&>", "g"]);
        assert_eq!(words("a 2>/dev/null >>x &>y <<<z"), ["a", "<>>", "/dev/null", "<>>>", "x", "<&>>", "y", "<<<<>", "z"]);
        assert_eq!(words("a #b c\nd"), ["a", "#", "<\n>", "d"]);
        assert_eq!(words("a#b"), ["a#b"]);
        assert_eq!(words("cat <<'EOF'\nit's $x\nEOF\nls"), ["cat", "<<<>", "EOF", "<\n>", "[it's $x\n]", "ls"]);
        assert_eq!(words("cat <<-EOF\n\tx\n\tEOF"), ["cat", "<<<->", "EOF", "<\n>", "[\tx\n]"]);
        assert_eq!(words("echo $'a\\x41\\101\\n'"), ["echo", "aAA\n"]);
        assert_eq!(words("a\\\nb"), ["ab"]);
        for bad in ["'", "\"", "a\\", "$'x", "cat <<", "cat << ;"] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
        let bare = parse("c${x}url $y'z'").unwrap_or_default();
        let joined = join_words(&bare);
        assert_eq!(joined, "curl z");
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.log", "a.log"));
        assert!(!glob_match("*.log", "a.txt"));
        assert!(glob_match(".e*", ".env"));
        assert!(!glob_match("*", ".env"));
        assert!(glob_match("[a-c]?d", "bxd"));
        assert!(!glob_match("[!a-c]?d", "bxd"));
        assert!(glob_match("id_*", "id_rsa"));
        assert!(glob_match("a[", "a["));
        assert!(glob_match("**x**", "abxcd"));
        assert_eq!(braces("~/.{ssh,x}/id_{rsa,dsa}").len(), 4);
        assert_eq!(braces("a{b}c"), ["a{b}c"]);
    }

    /// A small xorshift generator: no crate needed for the fuzz test.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    const PIECES: &[&str] = &[
        "'", "\"", "\\", "$", "$(", "${", "}", "`", "(", ")", "{", "[", "]", "*", "?", "~", "/", ".", "..", "-",
        "--", "=", ":", ",", ";", "&", "&&", "|", "||", "|&", ">", ">>", "<", "<<", "<<-", "<<<", ">&", "&>",
        "2>", "<(", ">(", "#", "!", " ", " ", " ", "\t", "\n", "\r", "\0", "é", "ſ", "\u{212A}", "\u{202E}",
        "\u{200B}", "\u{E0041}", "😀", "$'", "\\x", "\\u", "\\U", "\\c", "\\0", "EOF", "sudo", "rm", "-rf",
        "curl", "sed", "-i", "s/a/b/e", "s", "crontab", "find", "-exec", "-delete", "tee", "cp", "mv", "ln",
        "install", "~/.bashrc", "/etc/shadow", ".ssh", "id_rsa", ".env", "bash", "-c", "python3", "env", "ls",
        "cat", "grep", "-r", "journalctl", "-f", "systemctl", "status", "ssh", "docker", "logs", "ip", "addr",
        "date", "-s", "top", "-bn1", "*.pem", ".e*", "a{b,c}", "x.sh", "./x.sh", "passwd", "/etc/passwd", "at",
        "dd", "of=", ":(){", "base64", "-d", "uniq", "sort", "-o", "tail", "-n", "-t", "--", "$HOME", "${HOME}",
        "~root", "~+", "[!", "[a-", "/root", "-f/etc/shadow", "/proc/self/root", "/proc/1/task/2/", "~/.aws",
        "/dev/watchdog", "--files0-from=-", "-exe[c]", "-L", "-f", "/home/bob", "/var/backups",
    ];

    fn all_functions(s: &str, home: &str, written: &[String]) {
        let always = vec![s.to_string()];
        let c = Ctx { connected: true, posix: s.len() % 2 == 0, linux: s.len() % 3 == 0, home, written, always_run: &always, files_only: false };
        let _ = read_only(s, home, s.len() % 2 == 1);
        let _ = always_ask(s, &c);
        let _ = sensitive(s, home, true);
        let _ = sensitive(s, home, false);
        let _ = persistence(s, home);
        for l in LEVELS {
            for call in [Call::Run { command: s }, Call::List, Call::Read { path: s }, Call::Write { path: s }] {
                let _ = decide(l, &call, &c);
            }
        }
    }

    #[test]
    fn no_panics_on_odd_input() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let written = vec!["/home/u/x.sh".to_string(), "x.sh".to_string(), String::new(), "~/../..".to_string()];
        let homes = ["/home/u", "/root", "", "/", "rel/x", "/home/u/../x", "~", "\0", "/a b/'c"];
        for n in 0..4000 {
            let mut s = String::new();
            for _ in 0..rng.below(40) {
                s.push_str(PIECES[rng.below(PIECES.len())]);
            }
            let home = homes[n % homes.len()];
            all_functions(&s, home, &written);
            // Random bytes, as UTF-8.
            let bytes: Vec<u8> = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
            all_functions(&String::from_utf8_lossy(&bytes), home, &written);
        }
        let deep: String = (0..5000).map(|k| if k % 2 == 0 { "'\"" } else { "\"'" }).collect();
        for s in [
            deep.as_str(),
            "\0\0\0",
            "$'\\",
            "$'\\x",
            "$'\\u{",
            "$'\\c",
            "${",
            "${}",
            "$",
            "\\",
            "<<",
            "<<EOF",
            "cat <<EOF\n",
            "[",
            "[]",
            "[!]",
            "{,}",
            "{{{{,}}}}",
            "~",
            "~/",
            "~x",
            "-",
            "--",
            "--=",
            "-=",
            "=",
            ":",
            "#",
            "\n\n\n",
            "\u{FEFF}",
            "sed",
            "sed -e",
            "sed 's",
            "sed 's/a",
            "sed 'y/a/b/'",
            "sed '\\'",
            "sed '/x/I,+3!{p}'",
            "crontab -u",
            "cp -t",
            "ln",
            "tee",
            "top -n",
            "date -d",
            "grep -r -e",
            "ip -n",
            "systemctl -t",
            "journalctl -u",
            "docker",
            "uniq -f",
        ] {
            for home in homes {
                all_functions(s, home, &written);
            }
        }
    }

    #[test]
    fn huge_commands_stay_fast() {
        let mut rng = Rng(42);
        let random: String = (0..200_000)
            .map(|_| {
                let p = PIECES[rng.below(PIECES.len())];
                p.chars().next().unwrap_or(' ')
            })
            .collect();
        let random_pieces: String = (0..40_000).map(|_| PIECES[rng.below(PIECES.len())]).collect::<Vec<_>>().join(" ");
        // Different words, each three globs and then a long path.
        let deep_globs = (0..48).map(|k| format!("x*/x*/x*/{}{k}", "a/".repeat(2_000))).collect::<Vec<_>>().join(" ");
        let cases = [
            "a".repeat(200_000),
            "sudo ".repeat(40_000),
            "-su ".repeat(50_000),
            "x/ssh ".repeat(33_000),
            "${".repeat(100_000),
            "{".repeat(200_000),
            "a{b,".repeat(50_000),
            "*".repeat(200_000),
            "*/".repeat(100_000),
            "x* ".repeat(66_000),
            "-".repeat(200_000),
            "/".repeat(200_000),
            "../".repeat(66_000),
            "'".repeat(200_001),
            "\\".repeat(200_001),
            "<<".repeat(100_000),
            "<<EOF\n".repeat(30_000),
            "\n".repeat(200_000),
            "systemctl status ssh ssh ".repeat(8_000),
            "a:".repeat(100_000),
            "rm ".repeat(66_000),
            "find ".repeat(40_000),
            "sed 's/a/b/' ".repeat(14_000),
            format!("cat {}", "a/".repeat(100_000)),
            format!("cat {deep_globs}"),
            random,
            random_pieces,
        ];
        let written = vec!["/home/u/x.sh".to_string()];
        let c = ctx(&written, &[]);
        let mut slowest = Duration::ZERO;
        for s in &cases {
            let t = Instant::now();
            let _ = always_ask(s, &c);
            let _ = read_only(s, HOME, true);
            let took = t.elapsed();
            eprintln!("{took:>12?}  {:?}", s.get(..24).unwrap_or("(not on a char boundary)"));
            slowest = slowest.max(took);
            // Generous for an unoptimised test build; the point is no blow-up.
            assert!(took < Duration::from_secs(5), "{} bytes starting {:?} took {took:?}", s.len(), s.get(..20));
        }
        eprintln!("slowest 200 KB command: {slowest:?}");
    }

    #[test]
    fn every_group_is_named_in_order() {
        let names: Vec<&str> = GROUPS.iter().map(|g| g.name).collect();
        assert_eq!(
            names,
            [
                "privilege",
                "removal",
                "disks",
                "power",
                "remote access, firewall",
                "accounts",
                "mass permission changes",
                "scheduled jobs",
                "network, data leaving",
                "running other code",
                "hidden code",
                "persistence",
                "fork bomb",
                "sensitive paths",
                "the assistant's own files",
            ]
        );
    }

    #[test]
    fn output_thrown_away_or_joined_is_still_read_only() {
        for c in ["ls /etc/nginx 2>/dev/null", "cat /etc/os-release 2>&1 | head -3", "df -h >/dev/null", "grep -c x /var/log/syslog 2> /dev/null", "ps aux &>/dev/null", "cat </dev/null"] {
            assert!(ro(c), "{c:?}");
        }
        for c in ["ls > /tmp/x", "ls 2>/tmp/err", "cat x >& /tmp/f", "ls >/dev/null$x", "ls >/dev/nul*", "cat < /etc/hosts", "ls 2>>/var/log/x"] {
            assert!(!ro(c), "{c:?}");
        }
    }

    #[test]
    fn more_places_that_run_later() {
        for p in ["~/.zshenv", "~/.zlogin", "~/.bash_login", "/etc/update-motd.d/99-x", "/etc/pam.d/sshd"] {
            assert!(persistence(p, HOME), "{p:?}");
        }
        in_group("systemctl enable nginx", PERSISTENCE);
        in_group("systemctl --now enable foo.service", PERSISTENCE);
        clean("systemctl is-enabled nginx");
    }

    #[test]
    fn words_that_echo_or_printf_only_print() {
        for c in ["echo \"meet at 5\"", "printf 'job at %s\\n' now", "echo use sudo for that", "echo hi || echo at noon"] {
            clean(c);
        }
        // Run, they still count, whatever printed before them.
        for (c, g) in [
            ("at now", SCHEDULED),
            ("echo 'rm x' | at now", SCHEDULED),
            ("echo hi; at 5pm", SCHEDULED),
            ("echo hi && at now", SCHEDULED),
            ("echo $(at now)", SCHEDULED),
            ("echo x | sudo tee /etc/x", PRIVILEGE),
            ("command echo hi; reboot", POWER),
        ] {
            in_group(c, g);
        }
        // Printed into a file or a pipe, the words may run later; and a path
        // to an `echo` can be any program.
        for (c, g) in [
            ("echo reboot > f; sh f", POWER),
            ("echo reboot | tee f", POWER),
            ("printf 'at now' >> notes", SCHEDULED),
            ("echo hi |& at now", SCHEDULED),
            ("./echo reboot", POWER),
            ("/bin/echo please reboot later", POWER),
        ] {
            in_group(c, g);
        }
        // A program that runs its arguments isn't one that prints them.
        in_group("watch -n 60 at now", SCHEDULED);
        in_group("bash -c 'at now'", SCHEDULED);
    }

    #[test]
    fn auto_asks_when_the_program_is_hidden() {
        let c = ctx(&[], &[]);
        for cmd in ["c=rm; $c -rf ~/app", "printf -v c reboot; $c", "$HOME/bin/tool --go", "/usr/bin/r?boot", "{rm,-rf,/tmp/x}", "x='unclosed"] {
            assert_eq!(outcome(&decide(Level::Auto, &Call::Run { command: cmd }, &c)), "asks", "{cmd:?}");
        }
        for cmd in ["ls *.txt", "touch /tmp/x", "tar -xf a.tar -C /tmp", "FOO=1 make build", "mkdir -p /tmp/{a,b}"] {
            assert_eq!(outcome(&decide(Level::Auto, &Call::Run { command: cmd }, &c)), "runs", "{cmd:?}");
        }
    }

    #[test]
    fn the_persian_half_space_is_part_of_a_word() {
        assert!(ro("cat '/home/u/فایل\u{200C}ها.txt'"));
        assert!(ro("ls /home/u/می\u{200C}خواهم"));
        // Anywhere else it is a hidden character.
        for c in ["cat /home/u/a\u{200C}b", "ls \u{200C}/etc", "ls /etc\u{200C}", "cat /home/u/فایل\u{200C} x", "cat /home/u/ab\u{200C}ها"] {
            assert!(!ro(c), "{c:?}");
        }
        // And other hidden characters next to Persian letters still are.
        assert!(!ro("cat /home/u/فایل\u{200B}ها"));
        assert!(!ro("cat /home/u/فایل\u{200D}ها"));
        assert_eq!(show("فایل\u{200C}ها a\u{200C}b"), "فایل\u{200C}ها a?b");
    }
}
