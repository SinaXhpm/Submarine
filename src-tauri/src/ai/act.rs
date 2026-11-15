//! One step's tool calls: each goes through the gate, waits
//! for the user when the gate asks, runs, and is answered, in order. Every
//! call gets a result, one that was skipped, denied or stopped too: every API
//! needs an answer to each call before the next request.

use super::canon::Block;
use super::exec::{self, Facts, Stream};
use super::files::{self, Fs, Stamp};
use super::gate::{self, Call, Ctx, Decision, How, Level};
use super::tools::{self, Tool};
use super::{shape, shared, with_store_at, AiEvent};
use crate::ssh_manager::SshState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tauri::Manager;

/// What a card and the chat keep of a result: 64 KiB of each.
const KEEP: usize = 64 * 1024;

/// The stamp of a file the chat hasn't read as it is now: no file matches
/// it, so a change there reads the file first.
pub const UNREAD: Stamp = Stamp { size: i64::MAX as u64, mtime: None, sha256: None };

/// A write or edit, shown as a diff before it is approved.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preview {
    /// `None` for a new file.
    pub old: Option<String>,
    pub new: String,
}

/// One call as the chat shows it, live and after a reload.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallRecord {
    pub id: String,
    pub tool: String,
    pub input: Value,
    /// The path the call acted on, resolved on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The command that ran, when the user changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// How it was let through: read_only, approved, edited, always, auto, full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
    /// done, failed, denied, refused, skipped, stopped, interrupted.
    pub state: String,
    pub summary: String,
    /// What the model got.
    pub result: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub out: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub err: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub red: Option<String>,
    /// The level the call was decided at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

/// The user's answer to a card.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Answer {
    Allow {
        #[serde(default)]
        always: bool,
    },
    Deny {
        #[serde(default)]
        note: Option<String>,
    },
    Edit {
        command: String,
    },
}

/// The server side of a run: the session it may act through, and what the
/// gate needs to know.
pub struct Server {
    pub uuid: String,
    pub session_id: Option<String>,
    pub chat_id: i64,
    pub generation: u64,
    pub facts: Option<Facts>,
    conn: Option<Conn>,
    /// An FTP tab instead (no `conn` then): files only.
    ftp: Option<FtpReach>,
    fs: Option<Fs>,
    stamps: HashMap<String, Stamp>,
    written: Vec<String>,
    /// The ids of the calls this run was given. A call whose id comes again
    /// never runs: an answer to a card could otherwise reach another call
    /// than the one it showed.
    ids: HashSet<String>,
}

pub struct Conn {
    key: String,
    pub handle: crate::SessionHandleArc,
    /// `session_generation` of the connection and of the session, when the
    /// run took the connection.
    generations: (u64, u64),
}

/// The session's connection for the assistant: the tab's dedicated SFTP
/// connection when it has one (its own budget of channels), else the main
/// one. Never `::fwd`, never a terminal's own login.
pub async fn connection(app: &tauri::AppHandle, session_id: &str) -> Option<Conn> {
    let ssh = app.state::<SshState>();
    let (key, handle) = {
        let conns = ssh.connections.lock().await;
        let dedicated = format!("{session_id}::sftp");
        match conns.get(&dedicated) {
            Some(h) => (dedicated, h.clone()),
            None => (session_id.to_string(), conns.get(session_id)?.clone()),
        }
    };
    let g = ssh.session_generation.lock().await;
    let generations = (g.get(&key).copied().unwrap_or(0), g.get(session_id).copied().unwrap_or(0));
    Some(Conn { key, handle, generations })
}

impl Conn {
    /// The connection is still the one that was taken for `session_id`: the
    /// session wasn't closed, or connected anew, since.
    pub fn alive(&self, app: &tauri::AppHandle, session_id: &str) -> bool {
        let ssh = app.state::<SshState>();
        // Never wait on these locks here: a busy lock means nothing changed
        // that this batch must know about; the next one asks again.
        let same = match ssh.session_generation.try_lock() {
            Ok(g) => g.get(&self.key).copied().unwrap_or(0) == self.generations.0 && g.get(session_id).copied().unwrap_or(0) == self.generations.1,
            Err(_) => true,
        };
        let there = match ssh.connections.try_lock() {
            Ok(m) => m.contains_key(&self.key),
            Err(_) => true,
        };
        same && there
    }
}

/// An FTP tab's file access for the assistant (ftp_sftp.rs), shared with the
/// tab: the file tools work through it, and nothing runs commands.
pub struct FtpReach {
    sftp: std::sync::Arc<russh_sftp::client::SftpSession>,
    /// The tab's connect that made it (`ftp_sftp::Live::generation`).
    generation: u64,
    pub user: String,
    pub home: String,
}

/// The FTP tab's file access, when `session_id` is an FTP tab that is connected.
pub async fn ftp_reach(app: &tauri::AppHandle, session_id: &str) -> Option<FtpReach> {
    let ssh = app.state::<SshState>();
    let map = ssh.ftp_sessions.lock().await;
    let live = map.get(session_id)?;
    Some(FtpReach {
        sftp: std::sync::Arc::clone(&live.sftp),
        generation: live.generation,
        user: live.info.user.clone(),
        home: live.info.home.clone(),
    })
}

impl FtpReach {
    /// Still the tab's connection: not closed or connected anew since.
    pub fn alive(&self, app: &tauri::AppHandle, session_id: &str) -> bool {
        let ssh = app.state::<SshState>();
        // As for SSH: a busy lock means nothing changed that this must know.
        let same = match ssh.ftp_sessions.try_lock() {
            Ok(m) => m.get(session_id).is_some_and(|l| l.generation == self.generation),
            Err(_) => true,
        };
        same
    }

    pub fn fs(&self) -> Fs {
        Fs::shared(std::sync::Arc::clone(&self.sftp))
    }

    pub fn facts(&self) -> Facts {
        super::exec::ftp_facts(&self.user, &self.home)
    }
}

/// What a refused command says on an FTP server.
pub const NO_COMMANDS: &str = "This is an FTP server: it runs no commands. Only the file tools work here.";

impl Server {
    pub async fn new(app: &tauri::AppHandle, uuid: String, session_id: Option<String>, chat_id: i64, generation: u64, facts: Option<Facts>) -> Result<Server, String> {
        let (stamps, written) = with_store_at(generation, move |s| s.file_states(chat_id)).await?;
        let conn = match &session_id {
            Some(sid) => connection(app, sid).await,
            None => None,
        };
        let ftp = match (&conn, &session_id) {
            (None, Some(sid)) => ftp_reach(app, sid).await,
            _ => None,
        };
        Ok(Server { uuid, session_id, chat_id, generation, facts, conn, ftp, fs: None, stamps, written, ids: HashSet::new() })
    }

    /// The session is still the one the run started on.
    fn alive(&self, app: &tauri::AppHandle) -> bool {
        if let (Some(f), Some(sid)) = (&self.ftp, &self.session_id) {
            return f.alive(app, sid);
        }
        let (Some(c), Some(sid)) = (&self.conn, &self.session_id) else { return false };
        c.alive(app, sid)
    }

    pub fn connected(&self) -> bool {
        self.conn.is_some() || self.ftp.is_some()
    }

    /// The chat's server is an FTP tab: files only.
    pub fn is_ftp(&self) -> bool {
        self.ftp.is_some()
    }

    async fn facts(&mut self, stop: &tokio::sync::watch::Receiver<bool>) -> Result<Facts, String> {
        if let Some(f) = &self.facts {
            return Ok(f.clone());
        }
        if let Some(f) = &self.ftp {
            let facts = f.facts();
            self.facts = Some(facts.clone());
            return Ok(facts);
        }
        let c = self.conn.as_ref().ok_or(gate::NOT_CONNECTED)?;
        let f = exec::probe(&c.handle, stop.clone()).await?;
        // A probe Stop cut short found nothing to keep (it would read as a
        // server without a POSIX shell).
        if *stop.borrow() {
            return Err(STOPPED.into());
        }
        let (chat_id, fj) = (self.chat_id, serde_json::to_string(&f).unwrap_or_default());
        let _ = with_store_at(self.generation, move |s| s.set_chat_facts(chat_id, &fj)).await;
        self.facts = Some(f.clone());
        Ok(f)
    }

    async fn fs(&mut self) -> Result<&Fs, String> {
        // A session whose server side hangs on an open (a pipe, a device)
        // answers nothing more: the next call gets a new one.
        if self.fs.as_ref().is_some_and(Fs::stuck) {
            self.fs = None;
        }
        if self.fs.is_none() {
            if let Some(f) = &self.ftp {
                self.fs = Some(f.fs());
            } else {
                let c = self.conn.as_ref().ok_or(gate::NOT_CONNECTED)?;
                self.fs = Some(Fs::open(&c.handle).await?);
            }
        }
        self.fs.as_ref().ok_or_else(|| gate::NOT_CONNECTED.to_string())
    }

    /// The home folder for paths: the probe's, else SFTP's starting folder.
    async fn home(&mut self, stop: &tokio::sync::watch::Receiver<bool>) -> Result<String, String> {
        let f = self.facts(stop).await?;
        if !f.home.is_empty() {
            return Ok(f.home);
        }
        or_stop(stop, async { self.fs().await?.home().await }).await.unwrap_or_else(|| Err(STOPPED.into()))
    }
}

/// Everything one step needs.
pub struct Step<'a> {
    pub app: &'a tauri::AppHandle,
    pub run_id: u64,
    pub stop: tokio::sync::watch::Receiver<bool>,
    pub emit: &'a (dyn Fn(AiEvent) + Send + Sync),
}

fn record(id: &str, tool: &str, input: &Value) -> CallRecord {
    CallRecord { id: id.into(), tool: tool.into(), input: input.clone(), state: "pending".into(), ..Default::default() }
}

/// A call that won't run, answered with why.
pub fn unanswered(id: &str, tool: &str, input: &Value, why: &str) -> CallRecord {
    unrun(record(id, tool, input), "refused", why)
}

/// Answers a call without running it.
fn unrun(mut r: CallRecord, state: &str, why: &str) -> CallRecord {
    r.state = state.into();
    r.summary = why.into();
    r.result = why.into();
    r
}

fn how_name(how: How) -> &'static str {
    match how {
        How::ReadOnly => "read_only",
        How::Always => "always",
        How::Auto => "auto",
        How::Full => "full",
    }
}

/// Ends when the user stops the run.
async fn stopped(stop: &tokio::sync::watch::Receiver<bool>) {
    let mut stop = stop.clone();
    loop {
        if *stop.borrow() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

/// What `f` gives, or `None` when the user stops the run first: a file call
/// waits on the server, which may take minutes to answer. Stop wins when
/// both are there.
async fn or_stop<T>(stop: &tokio::sync::watch::Receiver<bool>, f: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = stopped(stop) => None,
        v = f => Some(v),
    }
}

/// Waits for the user's answer to a card, or for Stop.
async fn wait_answer(run_id: u64, call_id: &str, stop: &tokio::sync::watch::Receiver<bool>) -> Option<Answer> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Ok(mut w) = shared().waiters.lock() {
        w.insert((run_id, call_id.to_string()), tx);
    }
    let answer = or_stop(stop, rx).await.and_then(Result::ok);
    if let Ok(mut w) = shared().waiters.lock() {
        w.remove(&(run_id, call_id.to_string()));
    }
    answer
}

pub const STOPPED: &str = "stopped by the user before this ran";
const MAX_SESSIONS_HELP: &str = "The server refused another session on this connection (its MaxSessions limit). Close a terminal of this tab, or turn on Dedicated session for files.";
pub const SKIPPED: &str = "skipped: the user denied an earlier call in this step";
pub const REPEATED_ID: &str = "not run: an earlier call had the same id";

/// What becomes of a step's next call before the gate sees it.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Stopped,
    Skipped,
    /// Its id came before in this run: it never runs, so an answer meant for
    /// the earlier call's card can't reach it.
    Repeated,
    Run,
}

/// `ids`: the run's call ids so far; this call's goes in whatever happens
/// to it.
fn next(ids: &mut HashSet<String>, id: &str, stop_now: bool, skip_rest: bool) -> Next {
    let first = ids.insert(id.to_string());
    if stop_now {
        Next::Stopped
    } else if skip_rest {
        Next::Skipped
    } else if !first {
        Next::Repeated
    } else {
        Next::Run
    }
}

/// Runs one step's calls in order. Returns each call's record and its result
/// block, in the calls' order.
pub async fn run_calls(step: &Step<'_>, server: &mut Server, calls: &[(String, String, Value)]) -> Vec<CallRecord> {
    let mut out = Vec::new();
    let mut skip_rest = false;
    let mut told_max_sessions = false;
    for (id, name, input) in calls {
        let base = record(id, name, input);
        // Read first: borrowed in the match, the flag would stay locked
        // across its awaits.
        let stop_now = *step.stop.borrow();
        let r = match next(&mut server.ids, id, stop_now, skip_rest) {
            Next::Stopped => unrun(base, "stopped", STOPPED),
            Next::Skipped => unrun(base, "skipped", SKIPPED),
            Next::Repeated => {
                (step.emit)(AiEvent::ToolCall {
                    call_id: base.id.clone(),
                    tool: base.tool.clone(),
                    input: base.input.clone(),
                    needs: "none".into(),
                    reason: None,
                    red: None,
                    path: None,
                    preview: None,
                });
                log_action(server, unrun(base, "failed", REPEATED_ID), None).await
            }
            Next::Run => {
                let mut undo = None;
                let r = one(step, server, base, &mut undo).await;
                let r = log_action(server, r, undo).await;
                if r.state == "denied" {
                    skip_rest = true;
                }
                // A refused channel (a command, the probe, SFTP) means the
                // server's MaxSessions limit: the chat says what helps, once.
                if r.result == exec::MAX_SESSIONS && !told_max_sessions {
                    told_max_sessions = true;
                    let text = MAX_SESSIONS_HELP.to_string();
                    (step.emit)(AiEvent::Notice { kind: "max_sessions".into(), text: text.clone() });
                    let chat_id = server.chat_id;
                    let _ = with_store_at(server.generation, move |s| s.add_note(chat_id, "max_sessions", &text)).await;
                }
                r
            }
        };
        (step.emit)(AiEvent::ToolResult { call_id: r.id.clone(), record: r.clone() });
        out.push(r);
    }
    out
}

/// The result blocks for the model, in order. None carries Tag characters:
/// a path the server resolved or an error text it sent may hold them.
pub fn results(records: &[CallRecord]) -> Vec<Block> {
    records
        .iter()
        .map(|r| Block::ToolResult { id: r.id.clone(), text: shape::strip_tags(&r.result), is_error: r.state != "done" })
        .collect()
}

async fn one(step: &Step<'_>, server: &mut Server, mut r: CallRecord, undo: &mut Option<Undo>) -> CallRecord {
    let emit = step.emit;
    let call_event = |r: &CallRecord, needs: &str, reason: Option<String>, red: Option<String>| {
        emit(AiEvent::ToolCall { call_id: r.id.clone(), tool: r.tool.clone(), input: r.input.clone(), needs: needs.into(), reason, red, path: r.path.clone(), preview: r.preview.clone() });
    };
    // Answered without running, its card shown first.
    let not_run = |r: CallRecord, state: &str, why: &str| {
        call_event(&r, "none", None, None);
        unrun(r, state, why)
    };
    let tool = match tools::parse(&r.tool, &r.input) {
        Ok(t) => t,
        Err(e) => return not_run(r, "failed", &e),
    };
    // Not connected, or Suggest: refused before anything touches the server.
    let level = match level_now(server).await {
        Ok(l) => l,
        Err(e) => return not_run(r, "failed", &e),
    };
    r.level = Some(level.as_str().to_string());
    if !server.connected() || level == Level::Suggest {
        let why = if !server.connected() { gate::NOT_CONNECTED } else { gate::SUGGEST_ONLY };
        return not_run(r, "refused", why);
    }
    // An FTP server runs no commands: refused here, before anyone is asked to
    // approve one that couldn't run.
    if server.is_ftp() && matches!(tool, Tool::Run { .. }) {
        return not_run(r, "refused", NO_COMMANDS);
    }
    let facts = match server.facts(&step.stop).await {
        Ok(f) => f,
        Err(_) if *step.stop.borrow() => return not_run(r, "stopped", STOPPED),
        Err(e) => return not_run(r, "failed", &e),
    };
    let home = match server.home(&step.stop).await {
        Ok(h) => h,
        Err(_) if *step.stop.borrow() => return not_run(r, "stopped", STOPPED),
        Err(e) => return not_run(r, "failed", &e),
    };
    let always = always_list(server).await;
    // File tools act on the resolved path, which the card shows. The gate
    // checks the path as written too: a login file may be a link into a
    // dotfiles folder, a secret a link to an innocent name.
    let mut exists = false;
    let mut as_written = String::new();
    if let Some(p) = tool.path() {
        let abs = tools::absolute(p, &home);
        as_written = tools::lexical(p, &home);
        let resolved = or_stop(&step.stop, async { server.fs().await?.resolve(&abs).await }).await;
        match resolved {
            None => return not_run(r, "stopped", STOPPED),
            Some(Ok(res)) => {
                r.path = Some(res.path);
                exists = res.exists;
            }
            Some(Err(e)) => return not_run(r, "failed", &e),
        }
    }
    let path = r.path.clone().unwrap_or_default();

    // A write or an edit: check it can be done, and work out the new content
    // for the diff, before anyone is asked.
    let mut planned: Option<(Vec<u8>, Option<Vec<u8>>)> = None;
    if let Tool::Write { .. } | Tool::Edit { .. } = &tool {
        match or_stop(&step.stop, plan_change(server, &tool, &path, exists)).await {
            None => return not_run(r, "stopped", STOPPED),
            Some(Ok((new, old))) => {
                // Both sides are at most 1 MiB (plan_change): the card always
                // shows what will be written.
                r.preview = Some(Preview { old: old.as_ref().map(|o| String::from_utf8_lossy(o).into_owned()), new: String::from_utf8_lossy(&new).into_owned() });
                planned = Some((new, old));
            }
            Some(Err(e)) => return not_run(r, "failed", &e),
        }
    }

    // The level again, as it is now: the user may have changed it (or ended
    // Full access) while the server was asked about the path.
    let level = match level_now(server).await {
        Ok(l) => l,
        Err(e) => return not_run(r, "failed", &e),
    };
    r.level = Some(level.as_str().to_string());
    if level == Level::Suggest {
        return not_run(r, "refused", gate::SUGGEST_ONLY);
    }
    let written = server.written.clone();
    let ctx = Ctx { connected: true, posix: facts.posix, linux: facts.linux, home: &home, written: &written, always_run: &always, files_only: facts.files_only || server.is_ftp() };
    let homes = home_forms(&home, &facts);
    let paths = if as_written.is_empty() || as_written == path { vec![path.as_str()] } else { vec![path.as_str(), as_written.as_str()] };
    let decision = decide_strictest(level, &tool, &paths, &homes, ctx);
    let mut command_override: Option<String> = None;
    let how = match decision {
        Decision::Refuse(why) => {
            call_event(&r, "none", None, None);
            return unrun(r, "refused", &why);
        }
        Decision::Allow(how) => {
            call_event(&r, "none", None, None);
            if *step.stop.borrow() {
                return unrun(r, "stopped", STOPPED);
            }
            how_name(how).to_string()
        }
        Decision::Ask { reason, red } => {
            r.red = red.clone();
            call_event(&r, "approval", Some(reason), red.clone());
            let (add, edit) = match wait_answer(step.run_id, &r.id, &step.stop).await {
                None => return unrun(r, "stopped", STOPPED),
                Some(Answer::Deny { note }) => {
                    // The user's own words, as a message of theirs: without
                    // what can't be seen in them.
                    let note = note.map(|n| shape::strip_tags(&n).trim().to_string()).filter(|n| !n.is_empty());
                    let why = match &note {
                        Some(n) => format!("denied by the user: {}", shape::cut(n, 2000)),
                        None => "denied by the user".to_string(),
                    };
                    return unrun(r, "denied", &why);
                }
                Some(Answer::Allow { always }) => (always, None),
                Some(Answer::Edit { command }) => (false, Some(command)),
            };
            // The level may have changed while the card waited.
            let now = match level_now(server).await {
                Ok(l) => l,
                Err(e) => return unrun(r, "failed", &e),
            };
            let add = match after_yes(level, now, add, red.is_some()) {
                Ok(add) => add,
                Err(why) => {
                    r.level = Some(now.as_str().to_string());
                    return unrun(r, "refused", why);
                }
            };
            match edit {
                Some(command) => {
                    let Tool::Run { .. } = &tool else {
                        return unrun(r, "failed", "only a command can be changed");
                    };
                    if command.trim().is_empty() || command.len() > 64 * 1024 || command.contains('\0') {
                        return unrun(r, "failed", "the changed command was empty or not valid");
                    }
                    command_override = Some(command);
                    "edited".to_string()
                }
                None => {
                    if add {
                        if let Tool::Run { command, .. } = &tool {
                            add_always(server, command).await;
                        }
                    }
                    "approved".to_string()
                }
            }
        }
    };
    // The user was asked, and may have taken a while to answer.
    let asked = matches!(how.as_str(), "approved" | "edited");
    r.how = Some(how.clone());
    r.command = command_override.clone();
    emit(AiEvent::ToolRunning { call_id: r.id.clone(), how: how.clone(), command: command_override.clone() });

    let r = match tool {
        Tool::Run { command, timeout } => {
            let command = command_override.unwrap_or(command);
            run_command(step, server, r, &command, timeout, facts.posix.then_some(facts.shell.as_str()), &homes).await
        }
        Tool::List { .. } => {
            let listed = or_stop(&step.stop, async {
                let fs = server.fs().await?;
                if asked {
                    unmoved(fs, &path).await?;
                }
                fs.list(&path).await
            })
            .await;
            match listed {
                None => unrun(r, "stopped", STOPPED),
                Some(Ok(text)) => {
                    r.state = "done".into();
                    r.summary = text.lines().next().unwrap_or("").to_string();
                    r.result = text;
                    r
                }
                Some(Err(e)) => unrun(r, "failed", &e),
            }
        }
        Tool::Read { start_line, max_lines, .. } => {
            let read = or_stop(&step.stop, async {
                let fs = server.fs().await?;
                if asked {
                    unmoved(fs, &path).await?;
                }
                fs.read_lines(&path, start_line, max_lines).await
            })
            .await;
            match read {
                None => unrun(r, "stopped", STOPPED),
                Some(Ok((text, stamp))) => {
                    r.state = "done".into();
                    r.summary = text.lines().next().unwrap_or("").to_string();
                    r.result = text;
                    remember(server, &path, stamp, false).await;
                    r
                }
                Some(Err(e)) => unrun(r, "failed", &e),
            }
        }
        Tool::Write { .. } | Tool::Edit { .. } => {
            let (new, old) = planned.unwrap_or_default();
            let target = Target { path: &path, as_written: &as_written, existed: exists };
            let (r, u) = apply_change(server, r, target, new, old, &step.stop).await;
            *undo = u;
            r
        }
    };
    r
}

/// After the user's yes to a card decided at `decided`, with the server at
/// `now`: refused when it went to Suggest meanwhile (Suggest takes the
/// server back, even from a waiting card); otherwise whether "Always run"
/// adds the command. The list grows only at Ask, then and now, and never by
/// a red card: at Ask every command, every command asks, listed or not.
fn after_yes(decided: Level, now: Level, always: bool, red: bool) -> Result<bool, &'static str> {
    if now == Level::Suggest {
        return Err(gate::SUGGEST_ONLY);
    }
    Ok(always && !red && decided == Level::Ask && now == Level::Ask)
}

/// The home folder's forms for the gate: as the probe reported it, and with
/// its links resolved (`cd -P`) when that is another path.
pub fn home_forms<'a>(home: &'a str, facts: &'a Facts) -> Vec<&'a str> {
    let real = facts.real_home.as_str();
    if real.is_empty() || real.trim_end_matches('/') == home.trim_end_matches('/') {
        vec![home]
    } else {
        vec![home, real]
    }
}

/// How strict a decision is: a refusal, then a red card, a card, then what
/// lets a call through, from the least to the most.
fn rank(d: &Decision) -> u8 {
    match d {
        Decision::Allow(How::ReadOnly) => 0,
        Decision::Allow(How::Always) => 1,
        Decision::Allow(How::Auto) => 2,
        Decision::Allow(_) => 3,
        Decision::Ask { red: None, .. } => 4,
        Decision::Ask { red: Some(_), .. } => 5,
        Decision::Refuse(_) => 6,
    }
}

/// The gate's answer for every form of the call's path (resolved, and as
/// written) and of the home folder (as reported, and resolved), and the
/// strictest of them: a rule on a login file or a secret holds whichever
/// way the path, or the home folder, reaches it. A file's path counts in
/// lower case too: where the file system ignores case (macOS), ~/.ZSHRC is
/// ~/.zshrc. Where two are as strict, the first counts: the resolved path,
/// the reported home.
pub fn decide_strictest(level: Level, tool: &Tool, paths: &[&str], homes: &[&str], ctx: Ctx) -> Decision {
    let mut out: Option<Decision> = None;
    let mut take = |d: Decision| {
        out = match out.take() {
            Some(cur) if rank(&cur) >= rank(&d) => Some(cur),
            _ => Some(d),
        };
    };
    for &home in homes {
        match tool {
            Tool::Run { command, .. } => take(gate::decide(level, &Call::Run { command }, &Ctx { home, ..ctx })),
            Tool::List { .. } => take(gate::decide(level, &Call::List, &Ctx { home, ..ctx })),
            Tool::Read { .. } | Tool::Write { .. } | Tool::Edit { .. } => {
                for &path in paths {
                    let (lower_path, lower_home) = (path.to_ascii_lowercase(), home.to_ascii_lowercase());
                    for (p, h) in [(path, home), (lower_path.as_str(), lower_home.as_str())] {
                        let call = if let Tool::Read { .. } = tool { Call::Read { path: p } } else { Call::Write { path: p } };
                        take(gate::decide(level, &call, &Ctx { home: h, ..ctx }));
                    }
                }
            }
        }
    }
    out.unwrap_or_else(|| Decision::Refuse("there was no path to check".into()))
}

/// What Undo needs: the content before the write (`None`: the assistant
/// created the file), and the SHA-256 of what it wrote, so Undo can tell
/// whether the file changed since.
pub struct Undo {
    pub path: String,
    pub before: Option<Vec<u8>>,
    pub after_sha: String,
}

/// The new content of a write or an edit, and the file's current content
/// (`None` for a new file), after the staleness check.
async fn plan_change(server: &mut Server, tool: &Tool, path: &str, exists: bool) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let seen = server.stamps.get(path).cloned();
    let fs = server.fs().await?;
    plan_change_on(fs, seen.as_ref(), tool, path, exists).await
}

/// `plan_change` on a file access of its own, for the agent gateway too:
/// `seen` is the file's stamp when it was read, if it was.
pub async fn plan_change_on(fs: &Fs, seen: Option<&Stamp>, tool: &Tool, path: &str, exists: bool) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let old = if exists {
        let seen = seen.ok_or("read the file first")?;
        let (cur, now) = fs.read_small(path).await?;
        if !files::same(seen, &now, Some(&cur)) {
            return Err("the file changed since you read it; read it again".into());
        }
        Some(cur)
    } else {
        if let Tool::Edit { .. } = tool {
            return Err("no such file or folder".into());
        }
        None
    };
    let new = match tool {
        Tool::Write { content, .. } => content.clone().into_bytes(),
        Tool::Edit { old_text, new_text, .. } => {
            let cur = String::from_utf8(old.clone().unwrap_or_default()).map_err(|_| "the file isn't valid UTF-8 text; change it with run_command instead".to_string())?;
            edited(&cur, old_text, new_text)?.into_bytes()
        }
        _ => return Err("not a change".into()),
    };
    if new.len() > tools::MAX_WRITE {
        return Err("the new content is larger than 1 MiB".into());
    }
    // The model saw a private key as this marker; written back, it would
    // stand where the real key was.
    let marker = shape::KEY_REMOVED.as_bytes();
    let count = |b: &[u8]| b.windows(marker.len()).filter(|w| *w == marker).count();
    if count(&new) > old.as_deref().map_or(0, count) {
        return Err(format!(
            "the new content holds \"{}\", the text that stands in for a private key when a file is read; writing it would put that text where the key was. Leave that part of the file out, or ask the user to change it",
            shape::KEY_REMOVED
        ));
    }
    Ok((new, old))
}

/// The file after an edit: old_text is looked for in the file as the model
/// read it (`files::replace_once`), never in what a read left out.
fn edited(cur: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    files::replace_once(cur, old_text, new_text)
}

/// The file a write or an edit acts on.
struct Target<'a> {
    /// Resolved: what is written, and what the card shows.
    path: &'a str,
    /// As the model gave it, made absolute: the same file under another name
    /// when a link leads to it.
    as_written: &'a str,
    /// A file was there when the call was checked.
    existed: bool,
}

const STOPPED_WRITING: &str = "stopped by the user while writing";
/// The path resolves to another place than when the call was checked.
const MOVED: &str = "that path leads somewhere else now than when it was checked (a folder on the way changed); read the folder again";

/// `path` still leads where it did when the call was checked: the folders
/// on the way are resolved once more.
pub async fn unmoved(fs: &Fs, path: &str) -> Result<(), String> {
    if fs.resolve(path).await?.path != path {
        return Err(MOVED.to_string());
    }
    Ok(())
}

/// How `checked_write` ended.
#[derive(Debug)]
pub enum Wrote {
    /// Written: the file's stamp now.
    Done(Stamp),
    /// Nothing was written: a check said no ("failed"), or Stop came first
    /// ("stopped").
    NotRun { state: &'static str, why: String },
    /// The write failed. `after_sha` is `None` when the file is as it was,
    /// so nothing was written; otherwise the file may hold part of the
    /// write, and it is the SHA-256 of what is there now (empty when that
    /// couldn't be read).
    Failed { state: &'static str, why: String, after_sha: Option<String> },
}

/// Writes `new` to `path` (resolved) after the last checks: the approval may
/// have taken a while. `existed`: a file was there when the call was
/// checked. `old` and `seen`: the content the change was planned on, and the
/// file's stamp when it was read. A link, a folder or a device there now is
/// refused (`Fs::stat`). The assistant and the agent gateway both write
/// through here.
pub async fn checked_write(fs: &Fs, path: &str, existed: bool, old: Option<&[u8]>, seen: Option<&Stamp>, new: &[u8], stop: &tokio::sync::watch::Receiver<bool>) -> Wrote {
    let checked = or_stop(stop, async {
        // The folders on the way are resolved once more. Were one of them
        // swapped for a link while the card waited, the write would land
        // where the card and the checks never looked. SFTP has no way to
        // open a file under a folder it has already resolved, so this makes
        // the time for such a swap short; it can't make it nothing.
        unmoved(fs, path).await?;
        match (fs.stat(path).await?, existed) {
            (Some(_), false) => return Err(files::APPEARED.to_string()),
            (None, true) => return Err("the file is gone; read the folder again".to_string()),
            _ => {}
        }
        if let (Some(old), Some(seen)) = (old, seen) {
            let (cur, now) = fs.read_small(path).await?;
            if !files::same(seen, &now, Some(&cur)) || cur != old {
                return Err("the file changed since you read it; read it again".to_string());
            }
        }
        Ok(())
    })
    .await;
    match checked {
        None => return Wrote::NotRun { state: "stopped", why: STOPPED.into() },
        Some(Err(e)) => return Wrote::NotRun { state: "failed", why: e },
        Some(Ok(())) => {}
    }
    // Stop pressed during those checks: nothing is written.
    if *stop.borrow() {
        return Wrote::NotRun { state: "stopped", why: STOPPED.into() };
    }
    match or_stop(stop, fs.write(path, new, !existed)).await.unwrap_or_else(|| Err(STOPPED_WRITING.into())) {
        Ok(stamp) => Wrote::Done(stamp),
        Err(e) => {
            // An open the server never answered may still be carried out
            // there, and one for writing empties the file when it is: what
            // the file holds now says nothing. After Stop the server isn't
            // asked again either: what is there is unknown.
            let now = if *stop.borrow() || e == files::STUCK {
                Err(STOPPED_WRITING.to_string())
            } else {
                or_stop(stop, content_now(fs, path)).await.unwrap_or_else(|| Err(STOPPED_WRITING.into()))
            };
            match after_failed_write(old, &now) {
                None => Wrote::Failed { state: "failed", why: e, after_sha: None },
                Some(after_sha) => {
                    let state = if *stop.borrow() { "stopped" } else { "failed" };
                    Wrote::Failed { state, why: format!("{e}; the file may be partly written"), after_sha: Some(after_sha) }
                }
            }
        }
    }
}

async fn apply_change(
    server: &mut Server,
    mut r: CallRecord,
    target: Target<'_>,
    new: Vec<u8>,
    old: Option<Vec<u8>>,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> (CallRecord, Option<Undo>) {
    let Target { path, as_written, existed } = target;
    let seen = server.stamps.get(path).cloned();
    let wrote = match or_stop(stop, server.fs()).await {
        None => return (unrun(r, "stopped", STOPPED), None),
        Some(Err(e)) => return (unrun(r, "failed", &e), None),
        Some(Ok(fs)) => checked_write(fs, path, existed, old.as_deref(), seen.as_ref(), &new, stop).await,
    };
    // The path as written names the same file: it counts as the assistant's
    // under that name too.
    let other_name: Vec<String> = if as_written.is_empty() || as_written == path { Vec::new() } else { vec![as_written.to_string()] };
    match wrote {
        Wrote::NotRun { state, why } => (unrun(r, state, &why), None),
        Wrote::Done(stamp) => {
            let after_sha = stamp.sha256.clone().unwrap_or_default();
            remember(server, path, stamp, true).await;
            remember_unread(server, &other_name).await;
            r.state = "done".into();
            let size = shape::thousands(new.len() as u64);
            r.summary = if existed { format!("Wrote {size} bytes to {path}") } else { format!("Created {path} ({size} bytes)") };
            r.result = format!("Wrote {size} bytes to {}", shape::one_line(path));
            (r, Some(Undo { path: path.to_string(), before: old, after_sha }))
        }
        // When the file is as it was, nothing was written: there is no
        // Undo, and the chat's stamp stays.
        Wrote::Failed { state, why, after_sha: None } => (unrun(r, state, &why), None),
        // Otherwise it may hold part of the write: what Undo needs is kept,
        // with the hash of what is there now, so Undo asks only when it
        // changes again. The file counts as the assistant's (running it
        // asks), and a stamp no file can match means it is read again before
        // another change.
        Wrote::Failed { state, why, after_sha: Some(after_sha) } => {
            remember(server, path, UNREAD, true).await;
            remember_unread(server, &other_name).await;
            let undo = Undo { path: path.to_string(), before: old, after_sha };
            (unrun(r, state, &why), Some(undo))
        }
    }
}

/// What is at `path` now: `None` when there is no file.
async fn content_now(fs: &Fs, path: &str) -> Result<Option<Vec<u8>>, String> {
    match fs.stat(path).await? {
        None => Ok(None),
        Some(_) => fs.read_small(path).await.map(|(data, _)| Some(data)),
    }
}

/// After a write that failed: `None` when the file is as it was (`old`;
/// `None` for no file), so nothing was written; else the SHA-256 of what is
/// there now, empty when that couldn't be read.
fn after_failed_write(old: Option<&[u8]>, now: &Result<Option<Vec<u8>>, String>) -> Option<String> {
    match (old, now) {
        (Some(old), Ok(Some(cur))) if cur == old => None,
        (None, Ok(None)) => None,
        (_, Ok(Some(cur))) => Some(files::sha256(cur)),
        _ => Some(String::new()),
    }
}

async fn remember(server: &mut Server, path: &str, stamp: Stamp, written: bool) {
    server.stamps.insert(path.to_string(), stamp.clone());
    if written && !server.written.iter().any(|w| w == path) {
        server.written.push(path.to_string());
    }
    let (chat_id, p) = (server.chat_id, path.to_string());
    let _ = with_store_at(server.generation, move |s| s.set_file_state(chat_id, &p, &stamp, written)).await;
}

/// What every shell writes to (`2>/dev/null`, the terminal): never a file
/// the assistant wrote.
fn device(p: &str) -> bool {
    matches!(
        p,
        "/dev/null" | "/dev/zero" | "/dev/full" | "/dev/random" | "/dev/urandom" | "/dev/tty" | "/dev/console" | "/dev/stdin" | "/dev/stdout" | "/dev/stderr"
    ) || p.starts_with("/dev/fd/")
        || p.starts_with("/dev/pts/")
}

/// The files a command writes, made absolute against each form of the home
/// folder (relative paths start there), the shell's own devices left out.
pub fn command_writes(command: &str, homes: &[&str]) -> Vec<String> {
    let mut seen = HashSet::new();
    homes.iter().flat_map(|h| gate::written_by(command, h)).filter(|p| !device(p) && seen.insert(p.clone())).collect()
}

/// Files the chat wrote without reading them as they are now: what a
/// command writes, a written file's other name. They count as the
/// assistant's (running one asks), with a stamp no file matches (a change
/// there reads the file first). In memory and in the store, in one go.
async fn remember_unread(server: &mut Server, paths: &[String]) {
    let mut fresh = HashSet::new();
    let paths: Vec<String> = paths.iter().filter(|p| fresh.insert(p.as_str())).cloned().collect();
    if paths.is_empty() {
        return;
    }
    let mut known: HashSet<String> = server.written.iter().cloned().collect();
    for p in &paths {
        server.stamps.insert(p.clone(), UNREAD);
        if known.insert(p.clone()) {
            server.written.push(p.clone());
        }
    }
    let chat_id = server.chat_id;
    let _ = with_store_at(server.generation, move |s| {
        for p in &paths {
            s.set_file_state(chat_id, p, &UNREAD, true)?;
        }
        Ok(())
    })
    .await;
}

async fn run_command(step: &Step<'_>, server: &mut Server, mut r: CallRecord, command: &str, timeout: u64, shell: Option<&str>, homes: &[&str]) -> CallRecord {
    if server.is_ftp() {
        return unrun(r, "refused", NO_COMMANDS);
    }
    let Some(conn) = server.conn.as_ref() else {
        return unrun(r, "refused", gate::NOT_CONNECTED);
    };
    let id = r.id.clone();
    let emit = step.emit;
    let app = step.app.clone();
    let srv: &Server = server;
    let outcome = exec::run(
        &conn.handle,
        command,
        shell,
        Duration::from_secs(timeout),
        step.stop.clone(),
        || srv.alive(&app),
        |stream, text| emit(AiEvent::ToolOutput { call_id: id.clone(), stream: if stream == Stream::Out { "out".into() } else { "err".into() }, text }),
    )
    .await;
    match outcome {
        Ok(o) => {
            // What it writes is the assistant's from now on, whether it ran
            // to its end or not: running it later asks, as for a file tool.
            remember_unread(server, &command_writes(command, homes)).await;
            // A command that ran and failed is still a normal result for the
            // model: it reads the exit code and the output.
            r.state = match o.ended {
                exec::Ended::Exited => "done",
                exec::Ended::Stopped => "stopped",
                _ => "failed",
            }
            .into();
            r.exit = o.status;
            r.summary = o.summary();
            r.result = o.for_model();
            r.out = String::from_utf8_lossy(&o.stdout.ui).into_owned();
            r.err = String::from_utf8_lossy(&o.stderr.ui).into_owned();
            if command != r.input.get("command").and_then(Value::as_str).unwrap_or("") {
                r.result = format!("The user changed the command to: {}\n{}", shape::strip_tags(command), r.result);
            }
            r
        }
        Err(e) => {
            unrun(r, "failed", &e)
        }
    }
}

async fn level_now(server: &Server) -> Result<Level, String> {
    let uuid = server.uuid.clone();
    let sid = server.session_id.clone();
    let stored = with_store_at(server.generation, {
        let uuid = uuid.clone();
        move |s| s.level(&uuid)
    })
    .await?;
    Ok(super::effective_level(stored, &uuid, sid.as_deref(), server.generation))
}

async fn always_list(server: &Server) -> Vec<String> {
    let uuid = server.uuid.clone();
    with_store_at(server.generation, move |s| s.always(&uuid)).await.unwrap_or_default()
}

async fn add_always(server: &Server, command: &str) {
    let (uuid, c) = (server.uuid.clone(), command.to_string());
    let _ = with_store_at(server.generation, move |s| s.add_always(&uuid, &c)).await;
}

/// The activity log: every call that ran, how it was let through, at which
/// level. A write's row carries what Undo needs, and the card gets its id.
async fn log_action(server: &Server, mut r: CallRecord, undo: Option<Undo>) -> CallRecord {
    if matches!(r.state.as_str(), "skipped" | "stopped") && r.how.is_none() {
        return r;
    }
    let summary = match (&r.command, r.input.get("command").and_then(Value::as_str), &r.path) {
        (Some(c), _, _) => c.clone(),
        (None, Some(c), _) => c.to_string(),
        (None, None, Some(p)) => p.clone(),
        _ => String::new(),
    };
    let entry = super::store::ActionEntry {
        chat_id: server.chat_id,
        server_uuid: server.uuid.clone(),
        tool: r.tool.clone(),
        summary: shape::cut(&summary, 2000).to_string(),
        decision: match (&r.how, r.state.as_str()) {
            (Some(how), "done") => how.clone(),
            _ => r.state.clone(),
        },
        level: r.level.clone().unwrap_or_default(),
        exit_code: r.exit,
    };
    let has_undo = undo.is_some();
    if let Ok(id) = with_store_at(server.generation, move |s| log_entry(s, &entry, undo.as_ref())).await {
        if has_undo {
            r.action_id = Some(id);
        }
    }
    r
}

/// Logs a call. A row with what Undo needs reaches the file at once, not at
/// the end of the step: a crash or a kill meanwhile would leave a change on
/// the server with no record and no Undo.
fn log_entry(s: &mut super::store::Store, entry: &super::store::ActionEntry, undo: Option<&Undo>) -> Result<i64, String> {
    let id = s.log_action(entry, undo)?;
    if undo.is_some() {
        let _ = s.save();
    }
    Ok(id)
}

/// The chat keeps at most 64 KiB of each text, the long texts of a call's
/// input too, and a diff only when both its sides fit in that.
pub fn trimmed(mut r: CallRecord) -> CallRecord {
    for s in [&mut r.result, &mut r.out, &mut r.err] {
        if s.len() > KEEP {
            *s = shape::cut(s, KEEP).to_string();
        }
    }
    // The card shows a call's command and path, never what a write or an
    // edit sends; the model's copy keeps all of it.
    if let Some(input) = r.input.as_object_mut() {
        for k in ["content", "old_text", "new_text", "INVALID_JSON"] {
            if let Some(Value::String(s)) = input.get_mut(k) {
                if s.len() > KEEP {
                    *s = shape::cut(s, KEEP).to_string();
                }
            }
        }
    }
    if r.preview.as_ref().is_some_and(|p| p.new.len() > KEEP || p.old.as_ref().is_some_and(|o| o.len() > KEEP)) {
        r.preview = None;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::store::{file_for, ActionEntry, ChatSettings, Store};

    #[test]
    fn a_change_with_its_undo_copy_reaches_the_file_at_once() {
        let mut s = Store::for_tests();
        let vault = s.vault().to_path_buf();
        let entry = ActionEntry {
            chat_id: 0,
            server_uuid: "u".into(),
            tool: "write_file".into(),
            summary: "/etc/a".into(),
            decision: "approved".into(),
            level: "ask".into(),
            exit_code: None,
        };
        log_entry(&mut s, &entry, None).unwrap();
        assert!(!file_for(&vault).exists(), "a call without an Undo copy waits for the step's save");
        let undo = Undo { path: "/etc/a".into(), before: Some(b"old".to_vec()), after_sha: "x".into() };
        let id = log_entry(&mut s, &entry, Some(&undo)).unwrap();
        let (saved, _) = Store::open(&vault, &[7u8; 32], &[1u8; 16], 2).unwrap();
        assert_eq!(saved.undo_entry(id).unwrap().before.as_deref(), Some(&b"old"[..]));
    }

    #[test]
    fn a_failed_write_keeps_an_undo_only_when_the_file_changed() {
        let old = Some(&b"old"[..]);
        assert_eq!(after_failed_write(old, &Ok(Some(b"old".to_vec()))), None, "nothing was written");
        assert_eq!(after_failed_write(None, &Ok(None)), None, "nothing was created");
        assert_eq!(after_failed_write(old, &Ok(Some(b"ne".to_vec()))), Some(files::sha256(b"ne")), "part of it: Undo asks only if it changes again");
        assert_eq!(after_failed_write(None, &Ok(Some(Vec::new()))), Some(files::sha256(b"")), "an empty file was created");
        assert_eq!(after_failed_write(old, &Ok(None)), Some(String::new()), "gone: Undo asks");
        assert_eq!(after_failed_write(old, &Err("lost".into())), Some(String::new()), "unknown: Undo asks");
        assert_eq!(after_failed_write(None, &Err("lost".into())), Some(String::new()));
    }

    #[test]
    fn an_edit_that_hidden_characters_would_move_is_refused() {
        // The model saw "port 80" twice, and is told so. Made unique, the one
        // with a hidden character inside it is refused, the other changed.
        let tagged = "port 8\u{E0030}0 a\nport 80 b\n";
        assert!(edited(tagged, "port 80", "port 81").unwrap_err().contains("2 times"));
        assert!(edited(tagged, "port 80 a", "port 81 a").unwrap_err().contains("hidden characters"));
        assert_eq!(edited(tagged, "port 80 b", "port 81 b").unwrap(), "port 8\u{E0030}0 a\nport 81 b\n");
        assert!(edited("x\u{E0041}y\n", "xy", "z").unwrap_err().contains("hidden characters"), "seen, but not in the file");
        assert!(edited("x\u{E0041}y\n", "x\u{E0041}y", "z").unwrap_err().contains("doesn't appear"), "never seen by the model");
        // Away from them, an edit works as usual.
        assert_eq!(edited("a\u{E0041}\nport 80\n", "port 80", "port 81").unwrap(), "a\u{E0041}\nport 81\n");
        assert_eq!(edited("port 80\n", "port 80", "port 81").unwrap(), "port 81\n");
        assert!(edited("port 80\nport 80\n", "port 80", "port 81").unwrap_err().contains("2 times"));
    }

    #[test]
    fn results_for_the_model_carry_no_tag_characters() {
        let r = CallRecord { id: "c".into(), state: "done".into(), result: "/srv/a\u{E0049}\u{E0047}b · 2 entries".into(), ..Default::default() };
        let failed = CallRecord { id: "d".into(), state: "failed".into(), result: "denied\u{E007F}".into(), ..Default::default() };
        assert_eq!(
            results(&[r, failed]),
            vec![
                Block::ToolResult { id: "c".into(), text: "/srv/ab · 2 entries".into(), is_error: false },
                Block::ToolResult { id: "d".into(), text: "denied".into(), is_error: true },
            ]
        );
    }

    #[test]
    fn the_chat_keeps_the_start_of_a_long_input() {
        let long = "x".repeat(KEEP + 10);
        let r = CallRecord {
            input: serde_json::json!({"path": "/etc/a", "content": long, "old_text": long, "new_text": "short", "INVALID_JSON": long, "command": long}),
            ..Default::default()
        };
        let t = trimmed(r);
        for k in ["content", "old_text", "INVALID_JSON"] {
            assert_eq!(t.input[k].as_str().map(str::len), Some(KEEP), "{k}");
        }
        assert_eq!((t.input["path"].as_str(), t.input["new_text"].as_str()), (Some("/etc/a"), Some("short")));
        assert_eq!(t.input["command"].as_str().map(str::len), Some(KEEP + 10), "the card shows the command");
    }

    fn ctx(written: &[String]) -> Ctx<'_> {
        Ctx { connected: true, posix: true, linux: true, home: "/home/u", written, always_run: &[], files_only: false }
    }

    fn write(path: &str) -> Tool {
        Tool::Write { path: path.into(), content: String::new() }
    }

    fn command(text: &str) -> Tool {
        Tool::Run { command: text.into(), timeout: 60 }
    }

    fn asks(d: &Decision) -> bool {
        matches!(d, Decision::Ask { .. })
    }

    #[test]
    fn the_strictest_decision_wins() {
        let ask = |red: Option<&str>| Decision::Ask { reason: "r".into(), red: red.map(str::to_string) };
        let order = [
            Decision::Allow(How::ReadOnly),
            Decision::Allow(How::Always),
            Decision::Allow(How::Auto),
            Decision::Allow(How::Full),
            ask(None),
            ask(Some("removal: rm -rf")),
            Decision::Refuse("no".into()),
        ];
        for w in order.windows(2) {
            assert!(rank(&w[0]) < rank(&w[1]), "{:?} before {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn a_rule_holds_for_the_path_as_written_and_as_resolved() {
        // ~/.bashrc as a link into a dotfiles folder (stow): the realpath
        // names no login file, the path as written does.
        let d = decide_strictest(Level::Auto, &write("~/.bashrc"), &["/home/u/dotfiles/bash/.bashrc", "/home/u/.bashrc"], &["/home/u"], ctx(&[]));
        assert!(asks(&d), "{d:?}");
        // RHEL: /etc/rc.local is a link to /etc/rc.d/rc.local.
        let root = Ctx { home: "/root", ..ctx(&[]) };
        let d = decide_strictest(Level::Auto, &write("/etc/rc.local"), &["/etc/rc.d/rc.local", "/etc/rc.local"], &["/root"], root);
        assert!(asks(&d), "{d:?}");
        // A secret read through a link with a plain name asks at Ask.
        let read = Tool::Read { path: "~/.env".into(), start_line: 1, max_lines: 10 };
        let d = decide_strictest(Level::Ask, &read, &["/srv/app/shared/env", "/home/u/.env"], &["/home/u"], ctx(&[]));
        assert!(matches!(d, Decision::Ask { red: None, .. }), "{d:?}");
        // FreeBSD (/home a link to /usr/home): the home folder's rules hold
        // for its resolved form, even for a path given in that form.
        let homes = ["/home/u", "/usr/home/u"];
        let d = decide_strictest(Level::Auto, &write("/usr/home/u/.profile"), &["/usr/home/u/.profile"], &homes, ctx(&[]));
        assert!(asks(&d), "{d:?}");
        // macOS ignores letter case: ~/.ZSHRC is ~/.zshrc.
        let mac = Ctx { home: "/Users/u", ..ctx(&[]) };
        let d = decide_strictest(Level::Auto, &write("~/.ZSHRC"), &["/Users/u/.ZSHRC"], &["/Users/u"], mac);
        assert!(asks(&d), "{d:?}");
        // A plain file still goes through at Auto.
        let d = decide_strictest(Level::Auto, &write("/srv/app/x.conf"), &["/srv/app/x.conf"], &homes, ctx(&[]));
        assert_eq!(d, Decision::Allow(How::Auto));
    }

    #[test]
    fn a_file_written_under_two_names_is_the_assistants_under_both() {
        // macOS: write_file /tmp/x.sh lands on /private/tmp/x.sh; both names
        // are on the list.
        let both = ["/private/tmp/x.sh".to_string(), "/tmp/x.sh".to_string()];
        let d = decide_strictest(Level::Auto, &command("bash /tmp/x.sh"), &[], &["/Users/u"], Ctx { home: "/Users/u", ..ctx(&both) });
        assert!(matches!(d, Decision::Ask { red: Some(_), .. }), "{d:?}");
        // FreeBSD: a file recorded as resolved, run by a relative name from
        // the home folder, which is that file in the resolved home.
        let real = ["/usr/home/u/x.sh".to_string()];
        let d = decide_strictest(Level::Auto, &command("./x.sh"), &[], &["/home/u", "/usr/home/u"], ctx(&real));
        assert!(matches!(d, Decision::Ask { red: Some(_), .. }), "{d:?}");
    }

    #[test]
    fn a_continued_chat_still_asks_before_running_what_the_old_one_wrote() {
        let mut s = Store::for_tests();
        let old = s.create_chat("a", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        let new = s.create_chat("a (continued)", Some(("u", "web")), "sys", &Value::Null, &ChatSettings::default()).unwrap();
        s.set_file_state(old, "/home/u/deploy.sh", &Stamp { size: 3, mtime: Some(1), sha256: None }, true).unwrap();
        s.copy_written(old, new).unwrap();
        let (_, written) = s.file_states(new).unwrap();
        for c in ["bash /home/u/deploy.sh", "./deploy.sh"] {
            let d = decide_strictest(Level::Auto, &command(c), &[], &["/home/u"], ctx(&written));
            assert!(matches!(d, Decision::Ask { red: Some(_), .. }), "{c}: {d:?}");
        }
    }

    #[test]
    fn a_yes_after_suggest_is_refused_and_always_run_grows_only_at_ask() {
        assert_eq!(after_yes(Level::Ask, Level::Suggest, true, false), Err(gate::SUGGEST_ONLY));
        assert_eq!(after_yes(Level::Auto, Level::Suggest, false, true), Err(gate::SUGGEST_ONLY));
        assert_eq!(after_yes(Level::Ask, Level::Ask, true, false), Ok(true));
        assert_eq!(after_yes(Level::Ask, Level::Ask, false, false), Ok(false));
        assert_eq!(after_yes(Level::Ask, Level::Ask, true, true), Ok(false), "never a red card's command");
        assert_eq!(after_yes(Level::AskAll, Level::AskAll, true, false), Ok(false), "every command asks there");
        assert_eq!(after_yes(Level::Ask, Level::AskAll, true, false), Ok(false));
        assert_eq!(after_yes(Level::AskAll, Level::Ask, true, false), Ok(false));
        assert_eq!(after_yes(Level::Auto, Level::Ask, true, false), Ok(false));
    }

    #[tokio::test]
    async fn stop_ends_a_wait_on_the_server() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        assert_eq!(or_stop(&rx, async { 5 }).await, Some(5));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
        });
        assert_eq!(or_stop(&rx, std::future::pending::<u8>()).await, None, "a server that never answers");
        assert_eq!(or_stop(&rx, async { 5 }).await, None, "Stop wins when both are there");
    }

    #[test]
    fn a_call_id_used_before_in_the_run_never_runs_again() {
        let mut ids = HashSet::new();
        assert_eq!(next(&mut ids, "x", false, false), Next::Run);
        assert_eq!(next(&mut ids, "x", false, false), Next::Repeated, "in the same step");
        assert_eq!(next(&mut ids, "y", false, true), Next::Skipped);
        assert_eq!(next(&mut ids, "y", false, false), Next::Repeated, "in a later step: a skipped call's id counts too");
        assert_eq!(next(&mut ids, "z", true, false), Next::Stopped);
        assert_eq!(next(&mut ids, "", false, false), Next::Run);
        assert_eq!(next(&mut ids, "", false, false), Next::Repeated);
    }

    #[test]
    fn the_home_folder_and_what_a_command_writes() {
        let facts = |real: &str| Facts { real_home: real.into(), ..Default::default() };
        assert_eq!(home_forms("/home/u", &facts("")), ["/home/u"]);
        assert_eq!(home_forms("/home/u", &facts("/home/u/")), ["/home/u"]);
        assert_eq!(home_forms("/home/u", &facts("/usr/home/u")), ["/home/u", "/usr/home/u"]);
        // Under both forms of the home folder, each once; what every shell
        // writes to isn't a file.
        let w = command_writes("printf x > deploy.sh 2>/dev/null; echo y | tee -a /tmp/log >/dev/stderr", &["/home/u", "/usr/home/u"]);
        for p in ["/home/u/deploy.sh", "/usr/home/u/deploy.sh", "/tmp/log"] {
            assert_eq!(w.iter().filter(|x| *x == p).count(), 1, "{p} in {w:?}");
        }
        assert!(!w.iter().any(|x| x.starts_with("/dev/")), "{w:?}");
        for p in ["/dev/null", "/dev/stderr", "/dev/fd/2", "/dev/pts/0"] {
            assert!(device(p), "{p}");
        }
        assert!(!device("/dev/shm/x.sh") && !device("/dev/sdb"), "files and disks are kept");
    }
}
