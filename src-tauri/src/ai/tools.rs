//! The assistant's tools: the list a server chat sends with every
//! request, fixed when the chat is created, and the checks on what the model
//! asks for. `strict` is never sent: the input is checked here, in full.

use serde::Deserialize;
use serde_json::{json, Value};

pub const DEFAULT_TIMEOUT: u64 = 60;
pub const MAX_TIMEOUT: u64 = 600;
pub const DEFAULT_LINES: u64 = 400;
pub const MAX_LINES: u64 = 2000;
/// The most a write or an edit carries.
pub const MAX_WRITE: usize = 1024 * 1024;
const MAX_COMMAND: usize = 64 * 1024;
const MAX_PATH: usize = 4096;

/// The tools of a chat about an FTP server: the file tools, as FTP runs no
/// commands.
pub fn file_definitions() -> Value {
    let mut all = definitions();
    if let Some(list) = all.as_array_mut() {
        list.retain(|t| t["name"] != "run_command");
    }
    all
}

/// The tool list in its neutral form (Anthropic's shape; the other kinds are
/// rendered from it). Its order and wording never change for a chat that has
/// it: a different list would make the provider re-read the whole chat.
pub fn definitions() -> Value {
    json!([
        {"name": "run_command",
         "description": "Run one shell command on this session's server and get its exit code and output. Use it to inspect the system (ps, df, free, systemctl status, journalctl -n 100, ss -tlpn, docker ps) and, when the user wants a change, to make it. The command runs non-interactively in bash (sh where there is no bash; the server's own shell where there is no POSIX shell, see the server facts), in the home directory, with no terminal and no input: use non-interactive flags (-y, --no-pager, DEBIAN_FRONTEND=noninteractive) and absolute paths or `cd dir && …`. Programs that wait for input or never end (top, vim, less, tail -f) are stopped at the timeout. The user can watch each command live and may have to approve it.",
         "input_schema": {"type": "object", "additionalProperties": false,
           "properties": {
             "command": {"type": "string", "description": "The command line, as you would type it in bash."},
             "timeout_seconds": {"type": "integer", "description": "Seconds to wait before stopping it. Default 60, at most 600."}},
           "required": ["command"]}},
        {"name": "list_directory",
         "description": "List a folder on this session's server: names, types, sizes, modification times. Use it to find files before reading them.",
         "input_schema": {"type": "object", "additionalProperties": false,
           "properties": {"path": {"type": "string", "description": "Absolute path, or relative to the home folder; ~ is the home folder."}},
           "required": ["path"]}},
        {"name": "read_file",
         "description": "Read lines of a text file on this session's server. Read a file before you change it: write_file and edit_file refuse files you haven't read in this chat.",
         "input_schema": {"type": "object", "additionalProperties": false,
           "properties": {
             "path": {"type": "string"},
             "start_line": {"type": "integer", "description": "First line to read, from 1. Default 1."},
             "max_lines": {"type": "integer", "description": "Default 400, at most 2000."}},
           "required": ["path"]}},
        {"name": "write_file",
         "description": "Create a file, or replace a whole file, with the given text. For a small change to an existing file use edit_file. The user may have to approve it and sees a diff.",
         "input_schema": {"type": "object", "additionalProperties": false,
           "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
           "required": ["path", "content"]}},
        {"name": "edit_file",
         "description": "Replace one exact piece of text in a file you have read. old_text must appear exactly once in the file; include enough surrounding lines to make it unique. The user may have to approve it and sees a diff.",
         "input_schema": {"type": "object", "additionalProperties": false,
           "properties": {"path": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}},
           "required": ["path", "old_text", "new_text"]}}
    ])
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunIn {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListIn {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadIn {
    path: String,
    #[serde(default)]
    start_line: Option<i64>,
    #[serde(default)]
    max_lines: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteIn {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditIn {
    path: String,
    old_text: String,
    new_text: String,
}

/// A tool call, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tool {
    Run { command: String, timeout: u64 },
    List { path: String },
    Read { path: String, start_line: u64, max_lines: u64 },
    Write { path: String, content: String },
    Edit { path: String, old_text: String, new_text: String },
}

impl Tool {
    pub fn path(&self) -> Option<&str> {
        match self {
            Tool::Run { .. } => None,
            Tool::List { path } | Tool::Read { path, .. } | Tool::Write { path, .. } | Tool::Edit { path, .. } => Some(path),
        }
    }
}

/// Checks a call's input against its tool. The error is what the model gets
/// back.
pub fn parse(name: &str, input: &Value) -> Result<Tool, String> {
    if let Some(raw) = input.get("INVALID_JSON") {
        let raw = raw.as_str().unwrap_or("");
        return Err(format!("INVALID_JSON: the input wasn't valid JSON: {}", super::shape::cut(raw, 2000)));
    }
    let bad = |e: serde_json::Error| format!("invalid input for {name}: {e}");
    match name {
        "run_command" => {
            let i: RunIn = serde_json::from_value(input.clone()).map_err(bad)?;
            if i.command.trim().is_empty() {
                return Err("the command is empty".into());
            }
            if i.command.len() > MAX_COMMAND {
                return Err("the command is longer than 64 KiB".into());
            }
            if i.command.contains('\0') {
                return Err("the command contains a NUL character".into());
            }
            let timeout = match i.timeout_seconds {
                None => DEFAULT_TIMEOUT,
                Some(t) if t < 1 => return Err("timeout_seconds must be at least 1".into()),
                Some(t) => (t as u64).min(MAX_TIMEOUT),
            };
            Ok(Tool::Run { command: i.command, timeout })
        }
        "list_directory" => {
            let i: ListIn = serde_json::from_value(input.clone()).map_err(bad)?;
            Ok(Tool::List { path: check_path(i.path)? })
        }
        "read_file" => {
            let i: ReadIn = serde_json::from_value(input.clone()).map_err(bad)?;
            let start_line = match i.start_line {
                None => 1,
                Some(n) if n < 1 => return Err("start_line counts from 1".into()),
                Some(n) => n as u64,
            };
            let max_lines = match i.max_lines {
                None => DEFAULT_LINES,
                Some(n) if n < 1 => return Err("max_lines must be at least 1".into()),
                Some(n) => (n as u64).min(MAX_LINES),
            };
            Ok(Tool::Read { path: check_path(i.path)?, start_line, max_lines })
        }
        "write_file" => {
            let i: WriteIn = serde_json::from_value(input.clone()).map_err(bad)?;
            if i.content.len() > MAX_WRITE {
                return Err("the content is larger than 1 MiB".into());
            }
            Ok(Tool::Write { path: check_path(i.path)?, content: i.content })
        }
        "edit_file" => {
            let i: EditIn = serde_json::from_value(input.clone()).map_err(bad)?;
            if i.old_text.is_empty() {
                return Err("old_text is empty".into());
            }
            if i.old_text.len() > MAX_WRITE || i.new_text.len() > MAX_WRITE {
                return Err("old_text or new_text is larger than 1 MiB".into());
            }
            Ok(Tool::Edit { path: check_path(i.path)?, old_text: i.old_text, new_text: i.new_text })
        }
        other => Err(format!("there is no tool named {}", super::shape::cut(other, 100))),
    }
}

/// A path's own checks: not empty, no NUL, line break or backslash, not too
/// long. Every check on a path here splits it at `/` only, while a Windows
/// server (and some Windows FTP servers) also take `\` as a separator: there
/// `/srv/app/..\..\Users` would pass for one name inside `/srv/app` and lead
/// out of it. So a path with `\` isn't taken at all.
pub fn check_path(path: String) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("the path is empty".into());
    }
    if path.contains(['\0', '\n', '\r']) {
        return Err("the path contains a NUL or a line break".into());
    }
    if path.contains('\\') {
        return Err("the path contains a backslash: write paths with / between folders, on a Windows server too (/C:/Users/...)".into());
    }
    if path.len() > MAX_PATH {
        return Err("the path is too long".into());
    }
    Ok(path)
}

/// A path as the model gave it, made absolute: `~` and relative paths are
/// under the home folder. `.` and `..` are resolved by the server (realpath),
/// not here.
pub fn absolute(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if path == "~" {
        return if home.is_empty() { "/".into() } else { home.into() };
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if path.starts_with('/') {
        return path.into();
    }
    format!("{home}/{path}")
}

/// The path as the model gave it, made absolute and normalised as written:
/// no `.`, `..` or empty parts, `..` stopping at `/`, no link followed. What
/// the gate checks besides the resolved path, since a rule may name either.
pub fn lexical(path: &str, home: &str) -> String {
    let abs = absolute(path, home);
    let mut parts: Vec<&str> = Vec::new();
    for c in abs.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(c),
        }
    }
    if parts.is_empty() {
        return "/".into();
    }
    parts.iter().map(|c| format!("/{c}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_stable_and_well_formed() {
        let a = definitions().to_string();
        assert_eq!(a, definitions().to_string());
        let list = definitions();
        let names: Vec<&str> = list.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["run_command", "list_directory", "read_file", "write_file", "edit_file"]);
        for t in list.as_array().unwrap() {
            assert_eq!(t["input_schema"]["additionalProperties"], false);
        }
    }

    /// An FTP server's chat: the file tools only (FTP runs no commands), its
    /// facts without a probe, and a prompt that names only those tools.
    #[test]
    fn an_ftp_servers_chat_has_the_file_tools_only() {
        let list = file_definitions();
        let names: Vec<&str> = list.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_directory", "read_file", "write_file", "edit_file"]);
        let facts = crate::ai::exec::ftp_facts("web", "/htdocs");
        assert!(!facts.posix && facts.home == "/htdocs" && facts.real_home == "/htdocs" && facts.user == "web");
        assert_eq!(crate::ai::exec::ftp_facts("x", "relative").home, "/");
        let p = crate::ai::prompt::ftp_server(Some(&facts), "1a2b3c4d");
        assert!(p.contains("list_directory, read_file, write_file, edit_file"), "{p}");
        // run_command is named once: to say there is none (a tool's error can point to it).
        assert_eq!(p.matches("run_command").count(), 1, "{p}");
        assert!(p.contains("no run_command tool"), "{p}");
        assert!(p.contains("[Submarine 1a2b3c4d:") && p.contains("start folder: /htdocs"));
        assert!(facts.files_only);
    }

    #[test]
    fn inputs_are_checked() {
        assert_eq!(parse("run_command", &json!({"command": "df -h"})).unwrap(), Tool::Run { command: "df -h".into(), timeout: 60 });
        assert_eq!(parse("run_command", &json!({"command": "x", "timeout_seconds": 9000})).unwrap(), Tool::Run { command: "x".into(), timeout: 600 });
        assert!(parse("run_command", &json!({"command": "x", "timeout_seconds": 0})).is_err());
        assert!(parse("run_command", &json!({"command": "  "})).is_err());
        assert!(parse("run_command", &json!({"command": "x", "shell": "zsh"})).unwrap_err().contains("unknown field"));
        assert!(parse("run_command", &json!({"cmd": "x"})).is_err());
        assert!(parse("run_command", &json!({"INVALID_JSON": "{\"command\": \"ls"})).unwrap_err().starts_with("INVALID_JSON"));
        assert_eq!(
            parse("read_file", &json!({"path": "/etc/hosts", "max_lines": 5000})).unwrap(),
            Tool::Read { path: "/etc/hosts".into(), start_line: 1, max_lines: 2000 }
        );
        assert!(parse("read_file", &json!({"path": "a\nb"})).is_err());
        assert!(parse("read_file", &json!({"path": ""})).is_err());
        // A backslash, which a Windows server takes as a separator where the
        // checks here see one name.
        for path in [r"/C:/srv/app/..\..\Users\u\.aws\credentials", r"app\.env", r"C:\Users"] {
            assert!(parse("read_file", &json!({ "path": path })).unwrap_err().contains("backslash"), "{path}");
            assert!(parse("write_file", &json!({ "path": path, "content": "x" })).unwrap_err().contains("backslash"), "{path}");
            assert!(check_path(path.to_string()).is_err(), "{path}");
        }
        assert_eq!(check_path("/C:/srv/app/x.txt".into()).unwrap(), "/C:/srv/app/x.txt");
        assert!(parse("edit_file", &json!({"path": "f", "old_text": "", "new_text": "x"})).is_err());
        assert!(parse("write_file", &json!({"path": "f", "content": "x".repeat(MAX_WRITE + 1)})).is_err());
        assert!(parse("delete_everything", &json!({})).is_err());
        assert!(parse("list_directory", &json!(["not", "an", "object"])).is_err());
    }

    #[test]
    fn paths_are_made_absolute_under_home() {
        assert_eq!(absolute("~", "/home/u"), "/home/u");
        assert_eq!(absolute("~/x/y", "/home/u/"), "/home/u/x/y");
        assert_eq!(absolute("logs/a.log", "/home/u"), "/home/u/logs/a.log");
        assert_eq!(absolute("/etc/hosts", "/home/u"), "/etc/hosts");
        assert_eq!(absolute("x", "/"), "/x");
    }

    #[test]
    fn the_path_as_written_is_normalised_without_the_server() {
        assert_eq!(lexical("~/.bashrc", "/home/u"), "/home/u/.bashrc");
        assert_eq!(lexical("./a/../.profile", "/home/u/"), "/home/u/.profile");
        assert_eq!(lexical("/etc//init.d/./x", "/root"), "/etc/init.d/x");
        assert_eq!(lexical("../../../etc/rc.local", "/home/u"), "/etc/rc.local");
        assert_eq!(lexical("/..", "/home/u"), "/");
        assert_eq!(lexical("~", ""), "/");
    }
}
