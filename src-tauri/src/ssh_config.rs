//! What the user's OpenSSH client config (`~/.ssh/config`) says about a
//! connection (#2): the address and port an alias stands for, the user, the
//! ssh-agent, key and certificate files, known-hosts files, host key
//! algorithms, and ProxyCommand / ProxyJump.
//!
//! Read-only and on demand: the files are read when a connection starts, and
//! nothing from them is stored or synced. The device's "Use settings from
//! ~/.ssh/config" switch turns all of it off (`set_enabled`); Android has no
//! such file.
//!
//! The evaluation follows OpenSSH for what it covers:
//!   - Settings before the first `Host` or `Match` apply to every host. A
//!     `Host` line applies when one of its patterns matches and none of its
//!     `!` patterns does. The first value obtained wins (key and certificate
//!     files add up).
//!   - `Host` patterns are matched against the node's address (what one would
//!     type after `ssh`) and its name (an imported node keeps its alias as its
//!     name). `HostName` and `Port` are taken only from blocks that matched the
//!     address, so a node whose name happens to equal an alias isn't sent
//!     somewhere other than its own address.
//!   - `Include` reads files (globs allowed, relative to `~/.ssh`), at most 16
//!     levels deep, conditionally when inside a `Host` / `Match` block.
//!   - `Match` takes `all`, `host`, `originalhost`, `user`, `localuser`,
//!     `canonical` (never: no canonicalization here) and `final` (a second
//!     pass), each optionally negated with `!`. `exec` would run a command and
//!     is never evaluated; it, and any criterion not listed, skips its block
//!     with a note in the connect log.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The largest config file read; a real one is a few KB.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Nested `Include` levels, as OpenSSH.
const MAX_DEPTH: usize = 16;
/// Files read for one evaluation, the main one included.
const MAX_FILES: usize = 64;
/// Matches of one `Include` glob, and directory entries looked at per level.
const MAX_GLOB: usize = 256;
/// `IdentityFile` / `CertificateFile` values kept (OpenSSH's own cap).
const MAX_KEY_FILES: usize = 100;

static ENABLED: AtomicBool = AtomicBool::new(true);

/// The device's "Use settings from ~/.ssh/config" switch.
pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether `~/.ssh/config` is read at all on this device.
pub(crate) fn enabled() -> bool {
    !cfg!(target_os = "android") && ENABLED.load(Ordering::Relaxed)
}

/// `ProxyCommand` or `ProxyJump`, whichever came first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Proxy {
    /// `ProxyCommand none` / `ProxyJump none`: connect directly.
    Disabled,
    /// The command as written (tokens not yet expanded).
    Command(String),
    /// The `ProxyJump` list as written: `[user@]host[:port]`, comma-separated.
    Jump(String),
}

/// What the config says for one connection.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct HostConfig {
    /// `HostName`, `%h` expanded. Only from blocks that matched the address.
    pub hostname: Option<String>,
    /// `Port`. Only from blocks that matched the address.
    pub port: Option<u16>,
    pub user: Option<String>,
    /// `IdentityAgent` as written; see `resolve_identity_agent`.
    pub identity_agent: Option<String>,
    /// `IdentityFile` values as written, in order.
    pub identity_files: Vec<String>,
    /// `CertificateFile` values as written, in order.
    pub certificate_files: Vec<String>,
    pub identities_only: Option<bool>,
    pub proxy: Option<Proxy>,
    /// `UserKnownHostsFile`: the files of the first line that applies.
    pub user_known_hosts_files: Option<Vec<String>>,
    /// `HostKeyAlgorithms` as written (a list, maybe with a +, - or ^ prefix).
    pub host_key_algorithms: Option<String>,
    /// What was skipped and why, for the connect log.
    pub notes: Vec<String>,
}

/// The connection the config is evaluated for.
#[derive(Debug, Clone)]
pub(crate) struct Lookup {
    /// The node's address: the host argument of `ssh`.
    pub address: String,
    /// The node's name, if it has one.
    pub name: Option<String>,
    /// The node's username, if saved (for `Match user`).
    pub user: Option<String>,
}

/// The config for a connection, or `None` when the switch is off, there's no
/// `~/.ssh/config`, or this is Android.
pub(crate) async fn for_host(lookup: Lookup) -> Option<HostConfig> {
    if !enabled() {
        return None;
    }
    let home = home_dir()?;
    tokio::task::spawn_blocking(move || evaluate(&RealEnv { home }, &lookup)).await.ok()?
}

/// The home folder `~` stands for. Debug builds take `SUBMARINE_LAB_HOME`
/// instead when it's set, so the lab app can be tested against a config
/// without one being written to the tester's real home.
fn home_dir() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Some(home) = std::env::var("SUBMARINE_LAB_HOME").ok().filter(|h| !h.is_empty()) {
        return Some(home);
    }
    Some(directories::UserDirs::new()?.home_dir().to_string_lossy().into_owned())
}

/// Where the evaluation reads files and the environment from (the real
/// machine, or a fixture in the tests).
pub(crate) trait Env {
    fn home(&self) -> &str;
    fn var(&self, name: &str) -> Option<String>;
    fn local_user(&self) -> Option<String>;
    /// A regular file's text (a FIFO would block the read), up to the size cap.
    fn read(&self, path: &Path) -> Option<String>;
    /// The names in a directory.
    fn list(&self, dir: &Path) -> Vec<String>;
}

pub(crate) struct RealEnv {
    pub home: String,
}

impl Env for RealEnv {
    fn home(&self) -> &str {
        &self.home
    }
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn local_user(&self) -> Option<String> {
        std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok().filter(|u| !u.is_empty())
    }
    fn read(&self, path: &Path) -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }
    fn list(&self, dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries
            .flatten()
            .take(MAX_GLOB)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }
}

/// Evaluate `~/.ssh/config` (and what it includes) for one connection.
/// `None` when the file doesn't exist or can't be read.
pub(crate) fn evaluate(env: &dyn Env, lookup: &Lookup) -> Option<HostConfig> {
    let root = Path::new(env.home()).join(".ssh").join("config");
    let text = env.read(&root)?;
    let mut ev = Evaluator { env, lookup, out: HostConfig::default(), files: 1, want_final: false, final_pass: false };
    let start = State { active: true, via_address: true };
    ev.file("~/.ssh/config", &text, 0, false, start);
    if ev.want_final {
        // `Match final`: read everything again, now with `final` true. Values
        // already set stay (the first one obtained still wins).
        ev.final_pass = true;
        ev.files = 1;
        ev.file("~/.ssh/config", &text, 0, false, start);
    }
    Some(ev.out)
}

#[derive(Clone, Copy)]
struct State {
    /// The current `Host` / `Match` block applies.
    active: bool,
    /// ...and it applies because of the node's address (not only its name).
    via_address: bool,
}

struct Evaluator<'a> {
    env: &'a dyn Env,
    lookup: &'a Lookup,
    out: HostConfig,
    files: usize,
    want_final: bool,
    final_pass: bool,
}

impl Evaluator<'_> {
    fn note(&mut self, text: String) {
        if !self.out.notes.contains(&text) {
            self.out.notes.push(text);
        }
    }

    /// `never_match`: this file was included from a block that doesn't apply,
    /// so none of its own `Host` / `Match` blocks may apply either.
    fn file(&mut self, label: &str, text: &str, depth: usize, never_match: bool, start: State) {
        let mut st = start;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, rest) = split_directive(line);
            let at = format!("{label} line {}", n + 1);
            match key.to_ascii_lowercase().as_str() {
                "host" => {
                    let (matched, via_address) = host_line_matches(rest, self.lookup);
                    st = State { active: matched && !never_match, via_address };
                }
                "match" => {
                    let (matched, via_address) = self.match_line(rest, &at);
                    st = State { active: matched && !never_match, via_address };
                }
                "include" => self.include(rest, &at, depth, never_match || !st.active, st),
                key if st.active => self.apply(key, rest, st.via_address, &at),
                _ => {}
            }
        }
    }

    fn include(&mut self, rest: &str, at: &str, depth: usize, never_match: bool, st: State) {
        if depth + 1 >= MAX_DEPTH {
            self.note(format!("{at}: Include is nested too deeply; not read."));
            return;
        }
        for pattern in arguments(rest) {
            let expanded = match expand_basic(&pattern, self.env) {
                Ok(p) => p,
                Err(why) => {
                    self.note(format!("{at}: Include {pattern} {why}; not read."));
                    continue;
                }
            };
            let path = if is_absolute(&expanded) {
                PathBuf::from(&expanded)
            } else {
                Path::new(self.env.home()).join(".ssh").join(&expanded)
            };
            for file in glob_paths(self.env, &path) {
                if self.files >= MAX_FILES {
                    self.note(format!("{at}: too many included files; the rest aren't read."));
                    return;
                }
                if let Some(text) = self.env.read(&file) {
                    self.files += 1;
                    let label = file.to_string_lossy().into_owned();
                    self.file(&label, &text, depth + 1, never_match, st);
                }
            }
        }
    }

    /// `(applies, via_address)` for a `Match` line.
    fn match_line(&mut self, rest: &str, at: &str) -> (bool, bool) {
        let args = arguments(rest);
        if args.is_empty() {
            self.note(format!("{at}: Match without criteria; that block is skipped."));
            return (false, false);
        }
        let address = self.lookup.address.as_str();
        let name = self.lookup.name.as_deref().filter(|n| !n.is_empty());
        let mut all = true;
        let mut via_address = true;
        let mut i = 0;
        while i < args.len() {
            let word = args[i].to_ascii_lowercase();
            i += 1;
            let (negate, criterion) = match word.strip_prefix('!') {
                Some(c) => (true, c.to_string()),
                None => (false, word),
            };
            let result = match criterion.as_str() {
                "all" => true,
                "canonical" => false,
                "final" => {
                    self.want_final = true;
                    self.final_pass
                }
                "host" | "originalhost" | "user" | "localuser" => {
                    let Some(list) = args.get(i) else {
                        self.note(format!("{at}: Match {criterion} needs a value; that block is skipped."));
                        return (false, false);
                    };
                    i += 1;
                    match criterion.as_str() {
                        "host" | "originalhost" => {
                            let host = if criterion == "host" {
                                self.out.hostname.clone().unwrap_or_else(|| address.to_string())
                            } else {
                                address.to_string()
                            };
                            let by_host = pattern_list_matches(list, &host, true);
                            let by_name = name.is_some_and(|n| pattern_list_matches(list, n, true));
                            if !negate && !by_host && by_name {
                                via_address = false;
                            }
                            by_host || by_name
                        }
                        "user" => {
                            let user = self
                                .lookup
                                .user
                                .clone()
                                .filter(|u| !u.is_empty())
                                .or_else(|| self.out.user.clone())
                                .or_else(|| self.env.local_user())
                                .unwrap_or_default();
                            pattern_list_matches(list, &user, false)
                        }
                        _ => pattern_list_matches(list, &self.env.local_user().unwrap_or_default(), false),
                    }
                }
                other => {
                    self.note(format!(
                        "{at}: Match {other} isn't evaluated by Submarine (it never runs commands from the config), so that block is skipped."
                    ));
                    return (false, false);
                }
            };
            if result == negate {
                all = false;
            }
        }
        (all, via_address)
    }

    fn apply(&mut self, key: &str, rest: &str, via_address: bool, at: &str) {
        let out = &mut self.out;
        match key {
            "hostname" => {
                let Some(value) = first_argument(rest) else { return };
                if out.hostname.is_some() {
                    return;
                }
                let expanded = match expand_hostname(&value, &self.lookup.address) {
                    Ok(h) => h,
                    Err(why) => {
                        self.note(format!("{at}: HostName {value} {why}; not used."));
                        return;
                    }
                };
                if via_address {
                    self.out.hostname = Some(expanded);
                } else if !expanded.eq_ignore_ascii_case(&self.lookup.address) {
                    self.note(format!(
                        "{at}: HostName {expanded} applies to this server's name, not its address; the address {} is used.",
                        self.lookup.address
                    ));
                }
            }
            "port" => {
                let Some(value) = first_argument(rest) else { return };
                if out.port.is_some() {
                    return;
                }
                match value.parse::<u16>() {
                    Ok(p) if p > 0 => {
                        if via_address {
                            self.out.port = Some(p);
                        }
                    }
                    _ => self.note(format!("{at}: Port {value} isn't a port number; not used.")),
                }
            }
            "user" if out.user.is_none() => out.user = first_argument(rest),
            "identityagent" if out.identity_agent.is_none() => out.identity_agent = first_argument(rest),
            "identityfile" => {
                if let Some(v) = first_argument(rest) {
                    if out.identity_files.len() < MAX_KEY_FILES && !out.identity_files.contains(&v) {
                        out.identity_files.push(v);
                    }
                }
            }
            "certificatefile" => {
                if let Some(v) = first_argument(rest) {
                    if out.certificate_files.len() < MAX_KEY_FILES && !out.certificate_files.contains(&v) {
                        out.certificate_files.push(v);
                    }
                }
            }
            "identitiesonly" if out.identities_only.is_none() => {
                out.identities_only = first_argument(rest).and_then(|v| yes_no(&v));
            }
            "proxycommand" if out.proxy.is_none() => {
                let value = rest.trim();
                if !value.is_empty() {
                    out.proxy = Some(if value.eq_ignore_ascii_case("none") {
                        Proxy::Disabled
                    } else {
                        Proxy::Command(value.to_string())
                    });
                }
            }
            "proxyjump" if out.proxy.is_none() => {
                if let Some(value) = first_argument(rest) {
                    out.proxy = Some(if value.eq_ignore_ascii_case("none") {
                        Proxy::Disabled
                    } else {
                        Proxy::Jump(value)
                    });
                }
            }
            "userknownhostsfile" if out.user_known_hosts_files.is_none() => {
                let files = arguments(rest);
                if !files.is_empty() {
                    out.user_known_hosts_files = Some(files);
                }
            }
            "hostkeyalgorithms" if out.host_key_algorithms.is_none() => {
                out.host_key_algorithms = first_argument(rest);
            }
            _ => {}
        }
    }
}

fn yes_no(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "true" => Some(true),
        "no" | "false" => Some(false),
        _ => None,
    }
}

/// `Directive value` or `Directive=value`.
fn split_directive(line: &str) -> (&str, &str) {
    match line.find(|c: char| c.is_whitespace() || c == '=') {
        Some(i) => (&line[..i], line[i..].trim_start_matches(|c: char| c.is_whitespace() || c == '=')),
        None => (line, ""),
    }
}

/// The arguments of a line, split on whitespace, a double-quoted one kept
/// whole. Backslashes are left as they are (Windows paths are full of them).
fn arguments(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = rest.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            out.push(chars.by_ref().take_while(|&c| c != '"').collect());
        } else {
            let mut arg = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
            out.push(arg);
        }
    }
    out
}

fn first_argument(rest: &str) -> Option<String> {
    arguments(rest).into_iter().next().filter(|a| !a.is_empty())
}

/// `(matches, via_address)` for a `Host` line: one of its patterns matches the
/// node's address or name and none of its `!` patterns does.
fn host_line_matches(rest: &str, lookup: &Lookup) -> (bool, bool) {
    let name = lookup.name.as_deref().filter(|n| !n.is_empty());
    let (mut by_address, mut by_name) = (false, false);
    for pattern in arguments(rest) {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_matches(negated, &lookup.address, true) || name.is_some_and(|n| glob_matches(negated, n, true)) {
                return (false, false);
            }
        } else {
            by_address |= glob_matches(&pattern, &lookup.address, true);
            by_name |= name.is_some_and(|n| glob_matches(&pattern, n, true));
        }
    }
    (by_address || by_name, by_address)
}

/// OpenSSH's comma-separated pattern lists (`Match host a*,!ab*`): a `!`
/// pattern that matches rules the value out; otherwise any plain one matching
/// lets it in.
fn pattern_list_matches(list: &str, value: &str, ignore_case: bool) -> bool {
    let mut matched = false;
    for pattern in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_matches(negated, value, ignore_case) {
                return false;
            }
        } else if glob_matches(pattern, value, ignore_case) {
            matched = true;
        }
    }
    matched
}

/// OpenSSH's patterns: `*` any run of characters, `?` one.
fn glob_matches(pattern: &str, name: &str, ignore_case: bool) -> bool {
    let fold = |s: &str| if ignore_case { s.to_lowercase() } else { s.to_string() };
    let p: Vec<char> = fold(pattern).chars().collect();
    let n: Vec<char> = fold(name).chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// `HostName` takes `%h` (the address as given) and `%%`.
fn expand_hostname(value: &str, address: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(address),
            Some(t) => return Err(format!("uses %{t}, which HostName doesn't take")),
            None => return Err("ends with a lone %".into()),
        }
    }
    Ok(out)
}

/// `~` for the home folder and `${VAR}` (what `Include` and file paths take
/// before any `%` token).
fn expand_basic(value: &str, env: &dyn Env) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    if let Some(after) = rest.strip_prefix('~') {
        if after.is_empty() || after.starts_with('/') || after.starts_with('\\') {
            out.push_str(env.home());
            rest = after;
        } else {
            return Err("uses ~user, which isn't supported".into());
        }
    }
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let inner = &rest[i + 2..];
        let end = inner.find('}').ok_or("has a ${ that isn't closed")?;
        let var = &inner[..end];
        out.push_str(&env.var(var).ok_or_else(|| format!("uses ${{{var}}}, which isn't set"))?);
        rest = &inner[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute() || path.starts_with('/') || path.starts_with('\\')
}

/// The files a path with `*` / `?` in any part names, in lexical order.
fn glob_paths(env: &dyn Env, path: &Path) -> Vec<PathBuf> {
    let has_wild = |s: &str| s.contains('*') || s.contains('?');
    if !path.to_string_lossy().chars().any(|c| c == '*' || c == '?') {
        return vec![path.to_path_buf()];
    }
    // Case-insensitive on Windows, as its file system is.
    let ignore_case = cfg!(windows);
    let mut current: Vec<PathBuf> = vec![PathBuf::new()];
    for comp in path.components() {
        let part = comp.as_os_str().to_string_lossy().into_owned();
        if has_wild(&part) {
            let mut next = Vec::new();
            for dir in &current {
                let mut names: Vec<String> = env
                    .list(dir)
                    .into_iter()
                    // As glob(3): a leading dot only matches a pattern that starts with one.
                    .filter(|n| !n.starts_with('.') || part.starts_with('.'))
                    .filter(|n| glob_matches(&part, n, ignore_case))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| dir.join(n)));
                if next.len() >= MAX_GLOB {
                    next.truncate(MAX_GLOB);
                    break;
                }
            }
            current = next;
        } else {
            for dir in &mut current {
                dir.push(comp.as_os_str());
            }
        }
    }
    current
}

/// What `IdentityAgent` says for a host.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IdentityAgent {
    /// `IdentityAgent none`: no agent for this host.
    Disabled,
    /// The agent's socket (or named pipe on Windows), expanded.
    At(String),
    /// Set, but it can't be used as written: the value and why.
    Unusable(String, String),
}

/// An `IdentityAgent` value read on this machine.
pub(crate) fn resolve_identity_agent(value: &str) -> IdentityAgent {
    resolve(value, &home_dir().unwrap_or_default(), |var| std::env::var(var).ok())
}

/// What an `IdentityAgent` value names, as OpenSSH reads it: `none`;
/// `SSH_AUTH_SOCK` or `$VAR` for the socket an environment variable holds;
/// otherwise a path, where `~` is the home folder and `%d` (home), `%u` (local
/// user name), `%%` and `${VAR}` are expanded.
fn resolve(value: &str, home: &str, env: impl Fn(&str) -> Option<String>) -> IdentityAgent {
    let unusable = |why: String| IdentityAgent::Unusable(value.to_string(), why);
    if value.eq_ignore_ascii_case("none") {
        return IdentityAgent::Disabled;
    }
    let from_env = |var: &str| match env(var).filter(|v| !v.is_empty()) {
        Some(v) => IdentityAgent::At(v),
        None => unusable(format!("{var} isn't set")),
    };
    if value == "SSH_AUTH_SOCK" {
        return from_env("SSH_AUTH_SOCK");
    }
    if let Some(var) = value.strip_prefix('$').filter(|v| !v.starts_with('{')) {
        return from_env(var);
    }

    let mut out = String::new();
    let mut rest = value;
    if let Some(after) = rest.strip_prefix('~') {
        if after.is_empty() || after.starts_with('/') || after.starts_with('\\') {
            out.push_str(home);
            rest = after;
        } else {
            return unusable("only ~ for your own home folder is supported".into());
        }
    }
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => match chars.next() {
                Some('%') => out.push('%'),
                Some('d') => out.push_str(home),
                Some('u') => match env("USER").or_else(|| env("USERNAME")) {
                    Some(user) => out.push_str(&user),
                    None => return unusable("the local user name for %u isn't known".into()),
                },
                Some(t) => return unusable(format!("the %{t} token isn't supported here")),
                None => return unusable("it ends with a lone %".into()),
            },
            '$' if chars.as_str().starts_with('{') => {
                let inner = &chars.as_str()[1..];
                let Some(end) = inner.find('}') else {
                    return unusable("a ${ isn't closed".into());
                };
                let var = &inner[..end];
                match env(var) {
                    Some(v) => out.push_str(&v),
                    None => return unusable(format!("{var} isn't set")),
                }
                chars = inner[end + 1..].chars();
            }
            c => out.push(c),
        }
    }
    IdentityAgent::At(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Files under a fake home (`/home/alice`), the environment and the local user.
    struct Fixture {
        files: BTreeMap<String, String>,
        local_user: Option<String>,
    }

    impl Fixture {
        fn new(config: &str) -> Self {
            let mut files = BTreeMap::new();
            files.insert("/home/alice/.ssh/config".to_string(), config.to_string());
            Self { files, local_user: Some("alice".into()) }
        }
        fn with(mut self, path: &str, text: &str) -> Self {
            self.files.insert(path.to_string(), text.to_string());
            self
        }
        fn norm(path: &Path) -> String {
            path.to_string_lossy().replace('\\', "/")
        }
    }

    impl Env for Fixture {
        fn home(&self) -> &str {
            "/home/alice"
        }
        fn var(&self, name: &str) -> Option<String> {
            match name {
                "SSH_AUTH_SOCK" => Some("/tmp/ssh-XYZ/agent.1".into()),
                "MY_AGENT" => Some("/run/my.sock".into()),
                "CONF_D" => Some("conf.d".into()),
                "USER" => Some("alice".into()),
                _ => None,
            }
        }
        fn local_user(&self) -> Option<String> {
            self.local_user.clone()
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(&Self::norm(path)).cloned()
        }
        fn list(&self, dir: &Path) -> Vec<String> {
            let prefix = format!("{}/", Self::norm(dir).trim_end_matches('/'));
            let mut out: Vec<String> = self
                .files
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix))
                .map(|rest| rest.split('/').next().unwrap().to_string())
                .collect();
            out.dedup();
            out
        }
    }

    fn lookup(address: &str) -> Lookup {
        Lookup { address: address.into(), name: None, user: None }
    }

    fn eval(fx: &Fixture, l: &Lookup) -> HostConfig {
        evaluate(fx, l).expect("the config is read")
    }

    fn cfg(config: &str, address: &str) -> HostConfig {
        eval(&Fixture::new(config), &lookup(address))
    }

    #[test]
    fn no_config_file_means_none() {
        let mut fx = Fixture::new("");
        fx.files.clear();
        assert_eq!(evaluate(&fx, &lookup("h")), None);
    }

    #[test]
    fn a_host_star_block_applies_to_every_host() {
        let c = cfg("Host *\n  IdentityAgent \"~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock\"\n", "db.example.com");
        assert_eq!(c.identity_agent.as_deref(), Some("~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock"));
    }

    #[test]
    fn the_first_value_that_applies_wins() {
        let config = "Host web*\n IdentityAgent /a.sock\n Port 2201\nHost *\n IdentityAgent /b.sock\n Port 2202\n";
        assert_eq!(cfg(config, "web1").identity_agent.as_deref(), Some("/a.sock"));
        assert_eq!(cfg(config, "web1").port, Some(2201));
        assert_eq!(cfg(config, "db1").identity_agent.as_deref(), Some("/b.sock"));
        // Before any Host line: every host.
        assert_eq!(cfg("IdentityAgent=/top.sock\nHost *\n IdentityAgent /b.sock\n", "x").identity_agent.as_deref(), Some("/top.sock"));
    }

    #[test]
    fn host_patterns_match_like_openssh() {
        let config = "Host *.teleport.example.com !bastion.teleport.example.com\n IdentityAgent /tp.sock\n";
        assert_eq!(cfg(config, "node1.teleport.example.com").identity_agent.as_deref(), Some("/tp.sock"));
        assert_eq!(cfg(config, "NODE1.Teleport.Example.com").identity_agent.as_deref(), Some("/tp.sock"));
        assert_eq!(cfg(config, "bastion.teleport.example.com").identity_agent, None, "a ! pattern excludes the host");
        assert_eq!(cfg(config, "example.com").identity_agent, None);
        assert_eq!(cfg("Host db? \n IdentityAgent /q.sock\n", "db1").identity_agent.as_deref(), Some("/q.sock"));
        assert_eq!(cfg("Host db? \n IdentityAgent /q.sock\n", "db12").identity_agent, None);
    }

    #[test]
    fn an_alias_address_takes_hostname_port_and_user() {
        let config = "Host prod\n  HostName 10.0.0.5\n  Port 2222\n  User deploy\n";
        let c = cfg(config, "prod");
        assert_eq!((c.hostname.as_deref(), c.port, c.user.as_deref()), (Some("10.0.0.5"), Some(2222), Some("deploy")));
        assert!(c.notes.is_empty());
    }

    #[test]
    fn a_name_match_applies_everything_but_where_to_connect() {
        // An imported node: its name is the alias, its address the HostName.
        let config = "Host prod\n  HostName 10.0.0.5\n  Port 2222\n  IdentityAgent /prod.sock\n  ProxyCommand cloudflared access ssh --hostname %h\n";
        let imported = Lookup { address: "10.0.0.5".into(), name: Some("prod".into()), user: None };
        let c = eval(&Fixture::new(config), &imported);
        assert_eq!(c.identity_agent.as_deref(), Some("/prod.sock"));
        assert_eq!(c.proxy, Some(Proxy::Command("cloudflared access ssh --hostname %h".into())));
        assert_eq!((c.hostname, c.port), (None, None), "address and port stay the node's own");
        assert!(c.notes.is_empty(), "the HostName is the node's address: nothing to say");

        // A node whose name only happens to equal an alias keeps its own address.
        let other = Lookup { address: "192.168.1.9".into(), name: Some("prod".into()), user: None };
        let c = eval(&Fixture::new(config), &other);
        assert_eq!(c.hostname, None);
        assert_eq!(c.notes.len(), 1);
        assert!(c.notes[0].contains("192.168.1.9 is used"), "{:?}", c.notes);
        assert_eq!(eval(&Fixture::new(config), &Lookup { address: "x".into(), name: Some("staging".into()), user: None }).identity_agent, None);
    }

    #[test]
    fn hostname_takes_percent_h() {
        assert_eq!(cfg("Host *.int\n HostName %h.corp.example\n", "db.int").hostname.as_deref(), Some("db.int.corp.example"));
        let c = cfg("Host x\n HostName %r.example\n", "x");
        assert_eq!(c.hostname, None);
        assert!(c.notes[0].contains("%r"));
    }

    #[test]
    fn a_bad_port_is_skipped_with_a_note() {
        let c = cfg("Host x\n Port 99999\nHost *\n Port 2200\n", "x");
        assert_eq!(c.port, Some(2200));
        assert!(c.notes[0].contains("99999"));
    }

    #[test]
    fn proxycommand_and_proxyjump_the_first_one_wins() {
        assert_eq!(
            cfg("Host x\n ProxyCommand ssh -W %h:%p bastion\n ProxyJump other\n", "x").proxy,
            Some(Proxy::Command("ssh -W %h:%p bastion".into()))
        );
        assert_eq!(cfg("Host x\n ProxyJump a@b:2222,c\n ProxyCommand nc %h %p\n", "x").proxy, Some(Proxy::Jump("a@b:2222,c".into())));
        assert_eq!(cfg("Host x\n ProxyCommand none\nHost *\n ProxyJump b\n", "x").proxy, Some(Proxy::Disabled));
        assert_eq!(cfg("Host x\n ProxyJump none\nHost *\n ProxyJump b\n", "x").proxy, Some(Proxy::Disabled));
        // The command is the rest of the line, quotes and all.
        assert_eq!(
            cfg("Host x\n ProxyCommand \"C:\\Program Files\\cloudflared\\cloudflared.exe\" access ssh --hostname %h\n", "x").proxy,
            Some(Proxy::Command("\"C:\\Program Files\\cloudflared\\cloudflared.exe\" access ssh --hostname %h".into()))
        );
        assert_eq!(cfg("Host x\n ProxyCommand=nc %h %p\n", "x").proxy, Some(Proxy::Command("nc %h %p".into())));
    }

    #[test]
    fn key_files_add_up_and_the_rest_is_first_wins() {
        let config = "Host *.teleport.example.com teleport.example.com\n    UserKnownHostsFile \"/Users/u/.tsh/known_hosts\" /etc/kh\n    IdentityFile \"/Users/u/.tsh/keys/t/u\"\n    CertificateFile \"/Users/u/.tsh/keys/t/u-ssh/t-cert.pub\"\n    HostKeyAlgorithms rsa-sha2-512-cert-v01@openssh.com,rsa-sha2-256-cert-v01@openssh.com\n    IdentitiesOnly yes\nHost *\n    IdentityFile ~/.ssh/id_ed25519\n    UserKnownHostsFile ~/.ssh/other\n";
        let c = cfg(config, "n1.teleport.example.com");
        assert_eq!(c.identity_files, vec!["/Users/u/.tsh/keys/t/u".to_string(), "~/.ssh/id_ed25519".to_string()]);
        assert_eq!(c.certificate_files, vec!["/Users/u/.tsh/keys/t/u-ssh/t-cert.pub".to_string()]);
        assert_eq!(c.user_known_hosts_files, Some(vec!["/Users/u/.tsh/known_hosts".to_string(), "/etc/kh".to_string()]));
        assert_eq!(c.host_key_algorithms.as_deref(), Some("rsa-sha2-512-cert-v01@openssh.com,rsa-sha2-256-cert-v01@openssh.com"));
        assert_eq!(c.identities_only, Some(true));
    }

    #[test]
    fn include_reads_files_relative_to_ssh_in_order() {
        let fx = Fixture::new("Include conf.d/*.conf\nHost *\n User fallback\n")
            .with("/home/alice/.ssh/conf.d/20-b.conf", "Host b\n User bee\n")
            .with("/home/alice/.ssh/conf.d/10-a.conf", "Host *\n IdentityAgent /a.sock\n")
            .with("/home/alice/.ssh/conf.d/notes.txt", "Host *\n IdentityAgent /txt.sock\n");
        let c = eval(&fx, &lookup("b"));
        assert_eq!(c.identity_agent.as_deref(), Some("/a.sock"), "10-a before 20-b, .txt not included");
        assert_eq!(c.user.as_deref(), Some("bee"));
        assert_eq!(eval(&fx, &lookup("c")).user.as_deref(), Some("fallback"));
    }

    #[test]
    fn include_takes_absolute_paths_home_and_env() {
        let fx = Fixture::new("Include ~/extra/one\nInclude /etc/ssh/two ${CONF_D}/three\n")
            .with("/home/alice/extra/one", "Port 2001\n")
            .with("/etc/ssh/two", "User two\n")
            .with("/home/alice/.ssh/conf.d/three", "IdentityAgent /three.sock\n");
        let c = eval(&fx, &lookup("x"));
        assert_eq!((c.port, c.user.as_deref(), c.identity_agent.as_deref()), (Some(2001), Some("two"), Some("/three.sock")));
    }

    #[test]
    fn include_inside_a_block_applies_only_with_it() {
        let fx = Fixture::new("Host work-*\n  Include work.conf\nHost *\n  User other\n")
            .with("/home/alice/.ssh/work.conf", "User worker\nHost *\n  IdentityAgent /w.sock\n");
        let c = eval(&fx, &lookup("work-1"));
        assert_eq!((c.user.as_deref(), c.identity_agent.as_deref()), (Some("worker"), Some("/w.sock")));
        // Not a work host: nothing in work.conf applies, not even its own `Host *`.
        let c = eval(&fx, &lookup("home-1"));
        assert_eq!((c.user.as_deref(), c.identity_agent), (Some("other"), None));
    }

    #[test]
    fn the_including_block_resumes_after_the_include() {
        let fx = Fixture::new("Host a\n  Include x.conf\n  Port 2222\n").with("/home/alice/.ssh/x.conf", "Host zzz\n  User z\n");
        assert_eq!(eval(&fx, &lookup("a")).port, Some(2222));
        assert_eq!(eval(&fx, &lookup("b")).port, None);
    }

    #[test]
    fn include_loops_stop() {
        let fx = Fixture::new("Include config\nUser me\n");
        let c = eval(&fx, &lookup("x"));
        assert_eq!(c.user.as_deref(), Some("me"));
        assert!(c.notes.iter().any(|n| n.contains("too")), "{:?}", c.notes);
    }

    #[test]
    fn a_missing_include_is_ignored() {
        assert_eq!(cfg("Include nothing/here*\nInclude gone\nUser me\n", "x").user.as_deref(), Some("me"));
    }

    #[test]
    fn match_host_originalhost_and_negation() {
        let config = "Host prod\n  HostName db.prod.example\nMatch host *.prod.example !originalhost staging\n  User produser\nMatch originalhost prod\n  Port 2022\n";
        let c = cfg(config, "prod");
        assert_eq!((c.user.as_deref(), c.port), (Some("produser"), Some(2022)));
        let c = cfg("Match host a*,!ab*\n User a\n", "abc");
        assert_eq!(c.user, None, "a ! pattern in the list rules it out");
        assert_eq!(cfg("Match host a*,!ab*\n User a\n", "acc").user.as_deref(), Some("a"));
        assert_eq!(cfg("Match !host web*\n User notweb\n", "db").user.as_deref(), Some("notweb"));
        assert_eq!(cfg("Match !host web*\n User notweb\n", "web1").user, None);
    }

    #[test]
    fn match_user_and_localuser() {
        let fx = Fixture::new("Match user root\n  Port 2200\nMatch localuser alice\n  IdentityAgent /alice.sock\nMatch localuser bob\n  User bob\n");
        let as_root = Lookup { address: "h".into(), name: None, user: Some("root".into()) };
        let c = eval(&fx, &as_root);
        assert_eq!((c.port, c.identity_agent.as_deref(), c.user), (Some(2200), Some("/alice.sock"), None));
        // No saved user: the local one is who logs in, as with `ssh h`.
        assert_eq!(eval(&fx, &lookup("h")).port, None);
    }

    #[test]
    fn match_all_and_unknown_criteria() {
        let c = cfg("Match exec \"test -f /x\"\n User exec\nMatch all\n User all\n", "h");
        assert_eq!(c.user.as_deref(), Some("all"));
        assert!(c.notes[0].contains("exec"), "{:?}", c.notes);
        let c = cfg("Match localnetwork 10.0.0.0/8\n User ln\n", "h");
        assert_eq!(c.user, None);
        assert_eq!(cfg("Match canonical\n User c\n", "h").user, None);
        assert_eq!(cfg("Match host\n User c\n", "h").user, None, "no value: skipped");
    }

    #[test]
    fn match_final_reads_the_config_again() {
        let c = cfg("Match final\n  User late\n  Port 2001\nHost h\n  Port 2002\n", "h");
        // Pass 1: Port 2002 (final is false). Pass 2: User late; Port stays.
        assert_eq!((c.user.as_deref(), c.port), (Some("late"), Some(2002)));
    }

    #[test]
    fn match_by_name_counts_but_not_for_where_to_connect() {
        let fx = Fixture::new("Match originalhost prod\n  HostName 10.0.0.5\n  User deploy\n");
        let named = Lookup { address: "10.0.0.6".into(), name: Some("prod".into()), user: None };
        let c = eval(&fx, &named);
        assert_eq!((c.hostname, c.user.as_deref()), (None, Some("deploy")));
    }

    #[test]
    fn comments_and_other_settings_are_ignored() {
        let c = cfg("# IdentityAgent /commented.sock\nHost *\n  ForwardAgent no\n  identityagent   /x.sock   \n", "h");
        assert_eq!(c.identity_agent.as_deref(), Some("/x.sock"));
        assert_eq!(cfg("Host *\n User me\n", "h").identity_agent, None);
    }

    #[test]
    fn windows_pipe_paths_keep_their_backslashes() {
        let c = cfg("Host *\n IdentityAgent \\\\.\\pipe\\openssh-ssh-agent\n", "h");
        assert_eq!(c.identity_agent.as_deref(), Some(r"\\.\pipe\openssh-ssh-agent"));
    }

    fn env(var: &str) -> Option<String> {
        match var {
            "SSH_AUTH_SOCK" => Some("/tmp/ssh-XYZ/agent.1".into()),
            "MY_AGENT" => Some("/run/my.sock".into()),
            "USER" => Some("alice".into()),
            _ => None,
        }
    }

    #[test]
    fn identity_agent_values_resolve_like_openssh() {
        let home = "/home/alice";
        assert_eq!(resolve("none", home, env), IdentityAgent::Disabled);
        assert_eq!(resolve("SSH_AUTH_SOCK", home, env), IdentityAgent::At("/tmp/ssh-XYZ/agent.1".into()));
        assert_eq!(resolve("$MY_AGENT", home, env), IdentityAgent::At("/run/my.sock".into()));
        assert!(matches!(resolve("$UNSET_VAR", home, env), IdentityAgent::Unusable(..)));
        assert_eq!(resolve("~/.1p/agent.sock", home, env), IdentityAgent::At("/home/alice/.1p/agent.sock".into()));
        assert_eq!(resolve("%d/a-%u.sock", home, env), IdentityAgent::At("/home/alice/a-alice.sock".into()));
        assert_eq!(resolve("${MY_AGENT}.d/x", home, env), IdentityAgent::At("/run/my.sock.d/x".into()));
        assert_eq!(resolve("/odd/100%%.sock", home, env), IdentityAgent::At("/odd/100%.sock".into()));
        assert!(matches!(resolve("/tmp/%h.sock", home, env), IdentityAgent::Unusable(..)));
        assert!(matches!(resolve("~bob/agent.sock", home, env), IdentityAgent::Unusable(..)));
        assert_eq!(resolve(r"\\.\pipe\x", home, env), IdentityAgent::At(r"\\.\pipe\x".into()));
    }

    // The real file system: a temporary home with an Include glob, a nested
    // folder and a file that isn't matched.
    #[test]
    fn real_files_and_include_globs() {
        let home = std::env::temp_dir().join(format!("sublab-sshcfg-{}", std::process::id()));
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d").join("deep")).unwrap();
        std::fs::write(ssh.join("config"), "Include conf.d/*.conf conf.d/*/*.conf\nHost *\n  User fallback\n").unwrap();
        std::fs::write(ssh.join("conf.d").join("10-a.conf"), "Host real\n  HostName 10.1.1.1\n").unwrap();
        std::fs::write(ssh.join("conf.d").join("deep").join("20-b.conf"), "Host real\n  Port 2201\n").unwrap();
        std::fs::write(ssh.join("conf.d").join("skip.txt"), "Host *\n  User wrong\n").unwrap();
        let env = RealEnv { home: home.to_string_lossy().into_owned() };
        let c = evaluate(&env, &Lookup { address: "real".into(), name: None, user: None }).expect("read");
        assert_eq!((c.hostname.as_deref(), c.port, c.user.as_deref()), (Some("10.1.1.1"), Some(2201), Some("fallback")));
        // A folder where the config should be isn't read as one.
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh").join("config")).unwrap();
        assert_eq!(evaluate(&env, &Lookup { address: "real".into(), name: None, user: None }), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn globs_match_like_openssh() {
        assert!(glob_matches("*.example.com", "a.EXAMPLE.com", true));
        assert!(!glob_matches("*.example.com", "a.EXAMPLE.com", false));
        assert!(glob_matches("db?", "db1", true));
        assert!(glob_matches("*", "", true));
        assert!(!glob_matches("a*b", "acd", true));
        assert!(glob_matches("a*b*c", "aXbYbZc", true));
    }
}
