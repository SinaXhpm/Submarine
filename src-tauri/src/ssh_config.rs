//! What the user's OpenSSH client config (`~/.ssh/config`) says about a
//! connection (#2). Only `IdentityAgent` for now: which ssh-agent to use.
//!
//! Read-only and on demand: the file is read when a login needs an agent, and
//! nothing from it is stored or synced. The parsing follows OpenSSH for what it
//! covers. Settings before the first `Host` apply to every host. A `Host` line
//! applies when one of its patterns matches and none of its `!` patterns does.
//! The first value obtained wins. `Match` blocks (other than `Match all`) and
//! `Include` are not evaluated: their settings are skipped.

use std::path::Path;

/// The largest config read; a real one is a few KB.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// What `IdentityAgent` says for a host.
#[derive(Debug, PartialEq)]
pub(crate) enum IdentityAgent {
    /// `IdentityAgent none`: no agent for this host.
    Disabled,
    /// The agent's socket (or named pipe on Windows), expanded.
    At(String),
    /// Set, but it can't be used as written: the value and why.
    Unusable(String, String),
}

/// `IdentityAgent` in `~/.ssh/config` for a connection to a host known by
/// `names` (the node's address and its name). `None` when there's no config
/// or no matching block sets it.
pub(crate) async fn identity_agent(names: Vec<String>) -> Option<IdentityAgent> {
    if cfg!(target_os = "android") {
        return None;
    }
    let home = directories::UserDirs::new()?.home_dir().to_path_buf();
    let path = home.join(".ssh").join("config");
    let text = tokio::task::spawn_blocking(move || read_config(&path)).await.ok()??;
    let names: Vec<&str> = names.iter().map(String::as_str).filter(|n| !n.is_empty()).collect();
    let value = identity_agent_value(&text, &names)?;
    Some(resolve(&value, &home.to_string_lossy(), |var| std::env::var(var).ok()))
}

/// A regular file only (a FIFO would block the read), and not an absurd size.
fn read_config(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The first `IdentityAgent` value that applies to a host called any of
/// `names`, as written (quotes removed).
fn identity_agent_value(config: &str, names: &[&str]) -> Option<String> {
    let mut applies = true;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = split_directive(line);
        match key.to_ascii_lowercase().as_str() {
            "host" => applies = host_line_matches(rest, names),
            "match" => applies = rest.trim().eq_ignore_ascii_case("all"),
            "identityagent" if applies => {
                if let Some(value) = first_argument(rest) {
                    return Some(value);
                }
            }
            _ => {}
        }
    }
    None
}

/// `Directive value` or `Directive=value`.
fn split_directive(line: &str) -> (&str, &str) {
    match line.find(|c: char| c.is_whitespace() || c == '=') {
        Some(i) => (&line[..i], line[i..].trim_start_matches(|c: char| c.is_whitespace() || c == '=')),
        None => (line, ""),
    }
}

/// The arguments of a line, split on whitespace, a double-quoted one kept
/// whole. Backslashes are left as they are (Windows pipe paths are full of
/// them).
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

/// Whether a `Host` line's patterns take a host called any of `names`.
fn host_line_matches(rest: &str, names: &[&str]) -> bool {
    let mut matched = false;
    for pattern in arguments(rest) {
        if let Some(negated) = pattern.strip_prefix('!') {
            if names.iter().any(|n| glob_matches(negated, n)) {
                return false;
            }
        } else if names.iter().any(|n| glob_matches(&pattern, n)) {
            matched = true;
        }
    }
    matched
}

/// OpenSSH's host patterns: `*` any run of characters, `?` one, case
/// ignored.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
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

    fn value(config: &str, names: &[&str]) -> Option<String> {
        identity_agent_value(config, names)
    }

    #[test]
    fn a_host_star_block_applies_to_every_host() {
        let cfg = "Host *\n  IdentityAgent \"~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock\"\n";
        assert_eq!(
            value(cfg, &["db.example.com"]).as_deref(),
            Some("~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock")
        );
    }

    #[test]
    fn the_first_value_that_applies_wins() {
        let cfg = "Host web*\n IdentityAgent /a.sock\nHost *\n IdentityAgent /b.sock\n";
        assert_eq!(value(cfg, &["web1"]).as_deref(), Some("/a.sock"));
        assert_eq!(value(cfg, &["db1"]).as_deref(), Some("/b.sock"));
        // Before any Host line: every host.
        assert_eq!(value("IdentityAgent=/top.sock\nHost *\n IdentityAgent /b.sock\n", &["x"]).as_deref(), Some("/top.sock"));
    }

    #[test]
    fn host_patterns_match_like_openssh() {
        let cfg = "Host *.teleport.example.com !bastion.teleport.example.com\n IdentityAgent /tp.sock\n";
        assert_eq!(value(cfg, &["node1.teleport.example.com"]).as_deref(), Some("/tp.sock"));
        assert_eq!(value(cfg, &["NODE1.Teleport.Example.com"]).as_deref(), Some("/tp.sock"));
        assert_eq!(value(cfg, &["bastion.teleport.example.com"]), None, "a ! pattern excludes the host");
        assert_eq!(value(cfg, &["example.com"]), None);
        assert_eq!(value("Host db? \n IdentityAgent /q.sock\n", &["db1"]).as_deref(), Some("/q.sock"));
        assert_eq!(value("Host db? \n IdentityAgent /q.sock\n", &["db12"]), None);
    }

    #[test]
    fn the_node_name_counts_as_well_as_its_address() {
        // An imported node keeps its ssh alias as its name; its address is the HostName.
        let cfg = "Host prod\n  HostName 10.0.0.5\n  IdentityAgent /prod.sock\n";
        assert_eq!(value(cfg, &["10.0.0.5", "prod"]).as_deref(), Some("/prod.sock"));
        assert_eq!(value(cfg, &["10.0.0.5", "staging"]), None);
    }

    #[test]
    fn match_blocks_are_skipped_except_match_all() {
        let cfg = "Match host db* exec \"true\"\n IdentityAgent /m.sock\nMatch all\n IdentityAgent /all.sock\n";
        assert_eq!(value(cfg, &["db1"]).as_deref(), Some("/all.sock"));
    }

    #[test]
    fn comments_and_other_settings_are_ignored() {
        let cfg = "# IdentityAgent /commented.sock\nHost *\n  ForwardAgent no\n  identityagent   /x.sock   \n";
        assert_eq!(value(cfg, &["h"]).as_deref(), Some("/x.sock"));
        assert_eq!(value("Host *\n User me\n", &["h"]), None);
    }

    #[test]
    fn windows_pipe_paths_keep_their_backslashes() {
        let cfg = "Host *\n IdentityAgent \\\\.\\pipe\\openssh-ssh-agent\n";
        assert_eq!(value(cfg, &["h"]).as_deref(), Some(r"\\.\pipe\openssh-ssh-agent"));
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
    fn values_resolve_like_openssh() {
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
}
