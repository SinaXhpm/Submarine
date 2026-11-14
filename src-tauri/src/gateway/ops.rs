//! One operation of a grant, from the call to the answer. The grant is
//! checked, the assistant's gate decides, the user is asked when it says so,
//! the command runs through the assistant's own runner, the address is hidden
//! in what goes back, and the call is written to the activity log.
//!
//! An agent is an untrusted program that may be steered by what it read on a
//! server. Nothing here takes its word for anything: whether a command only
//! reads is the gate's finding, the level comes from the grant the user
//! gave, and the user's "always run" list, made for the built-in assistant,
//! doesn't count for it.

use super::grant::{self, Grant, Mode, Op};
use super::hub::Hub;
use super::mask::Mask;
use super::tools::FileTool;
use crate::ai::act::{self, Answer, CallRecord, Preview, Wrote};
use crate::ai::exec::{self, Ended, Facts, Stream};
use crate::ai::files::{Fs, Stamp};
use crate::ai::gate::{Ctx, Decision, How, Level};
use crate::ai::tools::{self as ai_tools, Tool};
use crate::ai::{shape, AiEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::watch;

/// Calls that may wait for their turn on one grant.
pub const MAX_QUEUE: usize = 8;
/// The files one grant remembers reading. More, and it starts over: a change
/// to a file then needs a read again.
const MAX_STAMPS: usize = 10_000;

pub const STOPPED: &str = "this call was stopped before it ran: it was cancelled, or its access ended";
const DENIED: &str = "the user denied this. Don't try another way: ask the user what they want";
const TOO_MANY_WAITING: &str = "too many calls are waiting on this access; wait for one to end";
pub const FTP_NO_COMMANDS: &str = "the server of this access is an FTP server: it runs no commands. Use the file tools (list_directory, read_file, write_file, edit_file, delete_file)";
const EACH_ASKS: &str = "Every operation asks with this access";
const DELETE_ASKS: &str = "Deleting a file always asks";

/// What a grant acts through: an SSH session's connection, or an FTP tab's
/// file access, where nothing runs commands.
pub enum Reach {
    Ssh(act::Conn),
    Ftp(act::FtpReach),
}

impl Reach {
    /// Still the session the grant was made on: not closed, or connected
    /// anew, since.
    pub fn alive(&self, app: &tauri::AppHandle, session: &str) -> bool {
        match self {
            Reach::Ssh(c) => c.alive(app, session),
            Reach::Ftp(f) => f.alive(app, session),
        }
    }

    /// A file access over it: an SFTP session of its own over SSH, the FTP
    /// tab's own over FTP.
    pub async fn open_fs(&self) -> Result<Fs, String> {
        match self {
            Reach::Ssh(c) => Fs::open(&c.handle).await,
            Reach::Ftp(f) => Ok(f.fs()),
        }
    }
}

/// What a grant needs while it stands, beside what it holds: what it acts
/// through, what was learned about the server, and the way to stop what
/// runs when it ends.
pub struct Live {
    pub reach: Reach,
    /// The session the connection was taken for.
    pub session: String,
    /// The values hidden in what the agent reads.
    pub mask: Mask,
    facts: tokio::sync::Mutex<Option<Facts>>,
    /// The file access, once a file call needed it (or the folders were
    /// resolved with it); a new one when it stops answering.
    fs: tokio::sync::Mutex<Option<Arc<Fs>>>,
    /// The files this grant read, as they were then: a change to a file
    /// that is there needs a read of it as it is, as for the assistant.
    stamps: StdMutex<HashMap<String, Stamp>>,
    /// Files this grant wrote, by a command or a file tool: running one of
    /// them asks, as it does for the assistant.
    written: StdMutex<Vec<String>>,
    /// One operation at a time; the others wait their turn.
    turn: tokio::sync::Mutex<()>,
    queued: AtomicUsize,
    /// Turned true when the grant ends.
    pub end: watch::Sender<bool>,
}

impl Live {
    pub fn new(reach: Reach, session: String, mask: Mask, fs: Option<Fs>) -> Live {
        Live {
            reach,
            session,
            mask,
            facts: tokio::sync::Mutex::new(None),
            fs: tokio::sync::Mutex::new(fs.map(Arc::new)),
            stamps: StdMutex::new(HashMap::new()),
            written: StdMutex::new(Vec::new()),
            turn: tokio::sync::Mutex::new(()),
            queued: AtomicUsize::new(0),
            end: watch::channel(false).0,
        }
    }

    /// Waits, at most `within`, until no call is running on this grant: a
    /// call that was just told to stop gets that long to write what it did.
    pub async fn idle(&self, within: Duration) {
        let _ = tokio::time::timeout(within, self.turn.lock()).await;
    }

    fn stamp(&self, path: &str) -> Option<Stamp> {
        self.stamps.lock().ok()?.get(path).cloned()
    }

    /// The file as this grant read or wrote it; `written`: this grant wrote it.
    fn remember(&self, path: &str, stamp: Stamp, written: bool) {
        if let Ok(mut s) = self.stamps.lock() {
            if s.len() >= MAX_STAMPS && !s.contains_key(path) {
                s.clear();
            }
            s.insert(path.to_string(), stamp);
        }
        if written {
            self.wrote(std::iter::once(path.to_string()));
        }
    }

    fn forget(&self, path: &str) {
        if let Ok(mut s) = self.stamps.lock() {
            s.remove(path);
        }
    }

    fn wrote(&self, paths: impl IntoIterator<Item = String>) {
        if let Ok(mut w) = self.written.lock() {
            for path in paths {
                if !w.contains(&path) {
                    w.push(path);
                }
            }
        }
    }
}

/// Ends when `rx` turns true. A sender that is gone can no longer say so.
pub(super) async fn on(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Aborts its task when it goes out of scope.
struct Watcher(tokio::task::JoinHandle<()>);

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One signal for "stop": the agent gave the call up (or went away), or the
/// grant ended.
fn halt(mut stop: watch::Receiver<bool>, mut end: watch::Receiver<bool>) -> (watch::Receiver<bool>, Watcher) {
    let already = *stop.borrow() || *end.borrow();
    let (tx, rx) = watch::channel(already);
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = on(&mut stop) => {}
            _ = on(&mut end) => {}
        }
        let _ = tx.send(true);
    });
    (rx, Watcher(task))
}

/// A place in the grant's line, given up when the call is done.
struct Queued<'a>(&'a AtomicUsize);

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn facts_of(live: &Live, halt: &watch::Receiver<bool>) -> Result<Facts, String> {
    let mut slot = live.facts.lock().await;
    if let Some(f) = slot.as_ref() {
        return Ok(f.clone());
    }
    let f = match &live.reach {
        // FTP has no shell to ask: what its connection reported.
        Reach::Ftp(r) => r.facts(),
        Reach::Ssh(c) => {
            let f = exec::probe(&c.handle, halt.clone()).await?;
            // A probe that was cut short found nothing to keep.
            if *halt.borrow() {
                return Err(STOPPED.into());
            }
            f
        }
    };
    *slot = Some(f.clone());
    Ok(f)
}

/// The grant's file access: the one it has, or a new one when there is none
/// yet or the last one stopped answering.
async fn fs_of(live: &Live) -> Result<Arc<Fs>, String> {
    let mut slot = live.fs.lock().await;
    if let Some(fs) = slot.as_ref().filter(|f| !f.stuck()) {
        return Ok(fs.clone());
    }
    let fs = Arc::new(live.reach.open_fs().await?);
    *slot = Some(fs.clone());
    Ok(fs)
}

/// What `f` gives, or `None` when the call is given up, or its grant ends,
/// first.
async fn until_halt<T>(halt: &watch::Receiver<bool>, f: impl std::future::Future<Output = T>) -> Option<T> {
    let mut halt = halt.clone();
    tokio::select! {
        biased;
        _ = on(&mut halt) => None,
        v = f => Some(v),
    }
}

fn stopped_or_failed(halt: &watch::Receiver<bool>) -> &'static str {
    if *halt.borrow() {
        "stopped"
    } else {
        "failed"
    }
}

/// Shows the user a card for a call (its path and diff with it, for a file
/// call) and waits for the answer. `None`: the call was given up, or its
/// grant ended, while it waited.
async fn ask_user(hub: &Hub, grant: &Grant, record: &CallRecord, reason: String, red: Option<String>, halt: &watch::Receiver<bool>) -> Option<Answer> {
    let answer = hub.open_card(grant, record, reason, red);
    let mut halt = halt.clone();
    let got = tokio::select! {
        biased;
        _ = on(&mut halt) => None,
        a = answer => a.ok(),
    };
    hub.close_card(&record.id);
    got
}

fn denied(note: Option<String>) -> String {
    // The user's own words, without what can't be seen in them.
    match note.map(|n| shape::strip_tags(&n).trim().to_string()).filter(|n| !n.is_empty()) {
        Some(n) => format!("the user denied this: {}", shape::cut(&n, 2000)),
        None => DENIED.to_string(),
    }
}

fn mode_name(grant: &Grant) -> String {
    serde_json::to_value(grant.mode).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn how_name(how: How) -> &'static str {
    match how {
        How::ReadOnly => "read_only",
        How::Always => "always",
        How::Auto => "auto",
        How::Full => "full",
    }
}

/// A call that ended without running: its card, its log row, its answer.
async fn unrun(hub: &Hub, grant: &Grant, record: CallRecord, state: &str, why: &str) -> (String, bool) {
    failed_with(hub, grant, record, state, why, None).await
}

/// A call that failed or ended early, with what Undo needs when it may have
/// changed a file all the same (a write cut short). Masked here, whoever
/// wrote it: an error can carry the server's address (an FTP connection's
/// "Couldn't reach host:port").
async fn failed_with(hub: &Hub, grant: &Grant, record: CallRecord, state: &str, why: &str, undo: Option<act::Undo>) -> (String, bool) {
    told_apart(hub, grant, record, state, why, why, undo).await
}

/// A call that ended early with the real reason on the user's card and log
/// row and a plainer one for the agent: where the difference would tell the
/// agent something about what is outside its folders.
async fn told_apart(hub: &Hub, grant: &Grant, mut record: CallRecord, state: &str, reason: &str, told: &str, undo: Option<act::Undo>) -> (String, bool) {
    let reason = grant.mask.apply(&shape::strip_tags(reason)).into_owned();
    record.state = state.into();
    record.summary = reason.clone();
    record.result = reason;
    hub.call_event(grant, AiEvent::ToolResult { call_id: record.id.clone(), record: record.clone() });
    hub.log_action(grant, &record, undo).await;
    (grant.mask.apply(&shape::strip_tags(told)).into_owned(), true)
}

/// `server_facts`: what the facts probe found, the way the assistant gets
/// it. No host name and no address is in it; the mask goes over it all the
/// same.
pub async fn facts(hub: &Hub, connection: u64, access: &str, stop: watch::Receiver<bool>) -> (String, bool) {
    let (grant, live) = match hub.grant(connection, access) {
        Ok(found) => found,
        Err(why) => return (why, true),
    };
    if !grant.covers(Op::Facts) {
        return (grant::not_covered(Op::Facts), true);
    }
    if let Err(why) = hub.log_ready(&grant).await {
        return (why, true);
    }
    let (halt, _watcher) = halt(stop, live.end.subscribe());
    if live.queued.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUE {
        live.queued.fetch_sub(1, Ordering::SeqCst);
        return (TOO_MANY_WAITING.into(), true);
    }
    let _place = Queued(&live.queued);
    let mut waiting = halt.clone();
    let _turn = tokio::select! {
        biased;
        _ = on(&mut waiting) => return (STOPPED.into(), true),
        turn = live.turn.lock() => turn,
    };
    let mut record = CallRecord { id: super::tools::new_id(16), tool: "server_facts".into(), input: json!({}), level: Some(mode_name(&grant)), ..Default::default() };
    // Reading the facts changes nothing on the server: it asks only where
    // every operation does.
    if grant.mode == grant::Mode::AskEach {
        match ask_user(hub, &grant, &record, "Every operation asks with this access".into(), None, &halt).await {
            None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
            Some(Answer::Deny { note }) => return unrun(hub, &grant, record, "denied", &denied(note)).await,
            Some(Answer::Allow { .. }) | Some(Answer::Edit { .. }) => record.how = Some("approved".into()),
        }
        // The user may have taken a while: is the grant still there?
        if let Err(why) = hub.grant(connection, access) {
            return unrun(hub, &grant, record, "refused", &why).await;
        }
    } else {
        hub.call_event(&grant, AiEvent::ToolCall { call_id: record.id.clone(), tool: record.tool.clone(), input: record.input.clone(), needs: "none".into(), reason: None, red: None, path: None, preview: None });
        record.how = Some("read_only".into());
    }
    hub.call_event(&grant, AiEvent::ToolRunning { call_id: record.id.clone(), how: record.how.clone().unwrap_or_default(), command: None });
    match facts_of(&live, &halt).await {
        Ok(f) => {
            let text = live.mask.apply(&shape::strip_tags(&f.text)).into_owned();
            record.state = "done".into();
            record.summary = text.lines().next().unwrap_or("").to_string();
            record.result = text.clone();
            hub.call_event(&grant, AiEvent::ToolResult { call_id: record.id.clone(), record: record.clone() });
            hub.log_action(&grant, &record, None).await;
            (text, false)
        }
        Err(e) => unrun(hub, &grant, record, if *halt.borrow() { "stopped" } else { "failed" }, &e).await,
    }
}

/// `run_command`.
pub async fn run(hub: &Hub, connection: u64, access: &str, command: String, timeout: u64, stop: watch::Receiver<bool>) -> (String, bool) {
    let Some(app) = hub.app() else { return ("Submarine isn't ready yet".into(), true) };
    let (grant, live) = match hub.grant(connection, access) {
        Ok(found) => found,
        Err(why) => return (why, true),
    };
    // Before "not covered", which would send the agent to ask for what no
    // FTP server can give.
    let Reach::Ssh(conn) = &live.reach else { return (FTP_NO_COMMANDS.into(), true) };
    if !grant.covers(Op::CommandsRead) && !grant.covers(Op::CommandsRun) {
        return (grant::not_covered(Op::CommandsRead), true);
    }
    // Nothing runs for an agent that can't be written down.
    if let Err(why) = hub.log_ready(&grant).await {
        return (why, true);
    }
    let (halt, _watcher) = halt(stop, live.end.subscribe());
    if live.queued.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUE {
        live.queued.fetch_sub(1, Ordering::SeqCst);
        return (TOO_MANY_WAITING.into(), true);
    }
    let _place = Queued(&live.queued);
    let mut waiting = halt.clone();
    let _turn = tokio::select! {
        biased;
        _ = on(&mut waiting) => return (STOPPED.into(), true),
        turn = live.turn.lock() => turn,
    };
    // Its turn came, maybe after a while: the grant, as it is now.
    let grant = match hub.grant(connection, access) {
        Ok((g, _)) => g,
        Err(why) => return (why, true),
    };
    let mut record = CallRecord { id: super::tools::new_id(16), tool: "run_command".into(), input: json!({"command": command, "timeout_seconds": timeout}), level: Some(mode_name(&grant)), ..Default::default() };

    let facts = match facts_of(&live, &halt).await {
        Ok(f) => f,
        Err(e) => return unrun(hub, &grant, record, if *halt.borrow() { "stopped" } else { "failed" }, &e).await,
    };
    let home = if facts.home.is_empty() { "/".to_string() } else { facts.home.clone() };
    let homes = act::home_forms(&home, &facts);
    let written = live.written.lock().map(|w| w.clone()).unwrap_or_default();
    // The user's always-run list is for the built-in assistant, under the
    // user's eye. An outside agent doesn't inherit it.
    let ctx = Ctx { connected: true, posix: facts.posix, linux: facts.linux, home: &home, written: &written, always_run: &[], files_only: facts.files_only };
    let tool = Tool::Run { command: command.clone(), timeout };
    // Does it only read? The gate's finding at the Ask level, never the
    // agent's word.
    let reads_only = matches!(act::decide_strictest(Level::Ask, &tool, &[], &homes, ctx), Decision::Allow(How::ReadOnly));
    if !reads_only && !grant.covers(Op::CommandsRun) {
        let why = format!("this command isn't one that plainly only reads, and {}", grant::not_covered(Op::CommandsRun));
        return unrun(hub, &grant, record, "refused", &why).await;
    }
    let mut decision = act::decide_strictest(grant.mode.level(), &tool, &[], &homes, ctx);
    // On top of the gate: a command whose purpose is to print the server's
    // addresses asks, whatever the grant says. The mask hides the addresses
    // Submarine knows; a server has others.
    let runs_unasked = matches!(decision, Decision::Allow(_));
    if let (true, Some(why)) = (runs_unasked, prints_addresses(&command)) {
        decision = Decision::Ask { reason: why.into(), red: None };
    }
    let mut ran = command.clone();
    let how = match decision {
        Decision::Refuse(why) => return unrun(hub, &grant, record, "refused", &why).await,
        Decision::Allow(how) => {
            hub.call_event(&grant, AiEvent::ToolCall { call_id: record.id.clone(), tool: record.tool.clone(), input: record.input.clone(), needs: "none".into(), reason: None, red: None, path: None, preview: None });
            how_name(how).to_string()
        }
        Decision::Ask { reason, red } => {
            record.red = red.clone();
            let how = match ask_user(hub, &grant, &record, reason, red, &halt).await {
                None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
                Some(Answer::Deny { note }) => return unrun(hub, &grant, record, "denied", &denied(note)).await,
                // "Always run" is never offered for an agent, and never kept.
                Some(Answer::Allow { .. }) => "approved",
                Some(Answer::Edit { command: changed }) => {
                    if changed.trim().is_empty() || changed.len() > 64 * 1024 || changed.contains('\0') {
                        return unrun(hub, &grant, record, "failed", "the changed command was empty or not valid").await;
                    }
                    ran = changed;
                    record.command = Some(ran.clone());
                    "edited"
                }
            };
            // The user may have taken a while to answer: the grant again.
            if let Err(why) = hub.grant(connection, access) {
                return unrun(hub, &grant, record, "refused", &why).await;
            }
            how.to_string()
        }
    };
    if *halt.borrow() {
        return unrun(hub, &grant, record, "stopped", STOPPED).await;
    }
    record.how = Some(how.clone());
    hub.call_event(&grant, AiEvent::ToolRunning { call_id: record.id.clone(), how, command: record.command.clone() });

    let call_id = record.id.clone();
    let outcome = exec::run(
        &conn.handle,
        &ran,
        facts.posix.then_some(facts.shell.as_str()),
        Duration::from_secs(timeout),
        halt.clone(),
        || live.reach.alive(app, &live.session),
        |stream, text| hub.call_event(&grant, AiEvent::ToolOutput { call_id: call_id.clone(), stream: if stream == Stream::Out { "out".into() } else { "err".into() }, text }),
    )
    .await;
    let o = match outcome {
        Ok(o) => o,
        Err(e) => return unrun(hub, &grant, record, if *halt.borrow() { "stopped" } else { "failed" }, &e).await,
    };
    // What it wrote is this grant's from now on, whether it ran to its end
    // or not: running one of those files asks, and changing one with a file
    // tool reads it first.
    let wrote = act::command_writes(&ran, &homes);
    for path in &wrote {
        live.remember(path, act::UNREAD, false);
    }
    live.wrote(wrote);
    // A command that ran and failed is a normal answer: the agent reads the
    // exit code and the output.
    record.state = match o.ended {
        Ended::Exited => "done",
        Ended::Stopped => "stopped",
        _ => "failed",
    }
    .into();
    record.exit = o.status;
    record.summary = o.summary();
    let mut text = o.for_model();
    if record.command.is_some() {
        text = format!("The user changed the command to: {ran}\n{text}");
    }
    // Hidden characters out, then the address: the agent gets this text and
    // nothing else of the output.
    let text = live.mask.apply(&shape::strip_tags(&text)).into_owned();
    record.result = text.clone();
    record.out = String::from_utf8_lossy(&o.stdout.ui).into_owned();
    record.err = String::from_utf8_lossy(&o.stderr.ui).into_owned();
    let failed = record.state != "done";
    hub.call_event(&grant, AiEvent::ToolResult { call_id: record.id.clone(), record: act::trimmed(record.clone()) });
    hub.log_action(&grant, &record, None).await;
    (text, failed)
}

/// A file tool: `list_directory`, `read_file`, `write_file`, `edit_file` or
/// `delete_file`. The path is resolved on the server and must lie inside the
/// grant's folders as resolved; the gate decides on it as for the assistant
/// (secrets and login files ask), "ask each time" asks for every call, and
/// a delete always asks. A change is planned before anyone is asked, so the
/// card shows its diff, and checked once more right before it is written.
pub async fn file(hub: &Hub, connection: u64, access: &str, tool: FileTool, stop: watch::Receiver<bool>) -> (String, bool) {
    let (grant, live) = match hub.grant(connection, access) {
        Ok(found) => found,
        Err(why) => return (why, true),
    };
    if !tool.ops().iter().any(|op| grant.covers(*op)) {
        return (grant::not_covered(tool.ops()[0]), true);
    }
    // Nothing runs for an agent that can't be written down.
    if let Err(why) = hub.log_ready(&grant).await {
        return (why, true);
    }
    let (halt, _watcher) = halt(stop, live.end.subscribe());
    if live.queued.fetch_add(1, Ordering::SeqCst) >= MAX_QUEUE {
        live.queued.fetch_sub(1, Ordering::SeqCst);
        return (TOO_MANY_WAITING.into(), true);
    }
    let _place = Queued(&live.queued);
    let mut waiting = halt.clone();
    let _turn = tokio::select! {
        biased;
        _ = on(&mut waiting) => return (STOPPED.into(), true),
        turn = live.turn.lock() => turn,
    };
    // Its turn came, maybe after a while: the grant, as it is now.
    let grant = match hub.grant(connection, access) {
        Ok((g, _)) => g,
        Err(why) => return (why, true),
    };
    let mut record = CallRecord { id: super::tools::new_id(16), tool: tool.name().into(), input: tool.input(), level: Some(mode_name(&grant)), ..Default::default() };

    // The home folder, for a path given relative to it.
    let facts = match facts_of(&live, &halt).await {
        Ok(f) => f,
        Err(e) => return unrun(hub, &grant, record, stopped_or_failed(&halt), &e).await,
    };
    let home = if facts.home.is_empty() { "/".to_string() } else { facts.home.clone() };
    let fs = match until_halt(&halt, fs_of(&live)).await {
        None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
        Some(Err(e)) => return unrun(hub, &grant, record, "failed", &e).await,
        Some(Ok(fs)) => fs,
    };
    let outside = |p: &str| format!("{} is outside the folders of this access ({}); ask for that folder with request_access", shape::one_line(p), grant.folders.join(", "));
    // The path as written (`..` taken out): outside the folders, it is
    // refused before the server is asked anything about it, so whether
    // something is there outside them is never told, and nothing outside
    // them is looked at.
    let as_written = ai_tools::lexical(tool.path(), &home);
    if !grant.holds(&as_written) {
        record.path = Some(as_written.clone());
        return unrun(hub, &grant, record, "refused", &outside(&as_written)).await;
    }
    // Then as the server resolves it, links followed: what is acted on, what
    // the card shows, and what must lie inside the folders too. A link out of
    // a folder leads outside it. The path as written is inside the folders,
    // so a failure from here on may have come from past a link that leads
    // out of them ("the folder doesn't exist" there). Whatever it was, and
    // wherever the path really leads, the agent gets this one answer, and so
    // learns nothing of what is or isn't outside its folders. The user's
    // card and log row say what it was.
    let unusable = format!(
        "{} can't be used inside the folders of this access ({}): a folder on the way isn't there, a link on the way leads outside them, or the server didn't answer. Check the path, or ask for the folder you need with request_access",
        shape::one_line(&as_written),
        grant.folders.join(", ")
    );
    let res = match until_halt(&halt, fs.resolve(&ai_tools::absolute(tool.path(), &home))).await {
        None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
        Some(Err(e)) => {
            record.path = Some(as_written.clone());
            return told_apart(hub, &grant, record, "failed", &format!("{}: {e}", shape::one_line(&as_written)), &unusable, None).await;
        }
        Some(Ok(r)) => r,
    };
    let path = res.path.clone();
    record.path = Some(path.clone());
    if !grant.holds(&path) {
        let reason = format!("{} leads to {}, outside the folders of this access", shape::one_line(&as_written), shape::one_line(&path));
        return told_apart(hub, &grant, record, "refused", &reason, &unusable, None).await;
    }
    let needs = tool.needs(res.exists);
    if !grant.covers(needs) {
        return unrun(hub, &grant, record, "refused", &grant::not_covered(needs)).await;
    }

    // A write or an edit: can it be done, and what will the file hold? Known
    // before anyone is asked: the card shows the diff.
    let seen = live.stamp(&path);
    let mut planned: Option<(Vec<u8>, Option<Vec<u8>>)> = None;
    match &tool {
        FileTool::Ai(t @ (Tool::Write { .. } | Tool::Edit { .. })) => match until_halt(&halt, act::plan_change_on(&fs, seen.as_ref(), t, &path, res.exists)).await {
            None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
            Some(Err(e)) => return unrun(hub, &grant, record, "failed", &e).await,
            Some(Ok((new, old))) => {
                record.preview = Some(Preview { old: old.as_ref().map(|o| String::from_utf8_lossy(o).into_owned()), new: String::from_utf8_lossy(&new).into_owned() });
                planned = Some((new, old));
            }
        },
        // One file as it is: not a folder, a link or a device.
        FileTool::Delete { .. } => match until_halt(&halt, fs.stat(&path)).await {
            None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
            Some(Ok(Some(_))) => {}
            Some(Ok(None)) => return unrun(hub, &grant, record, "failed", "no such file").await,
            Some(Err(e)) => return unrun(hub, &grant, record, "failed", &e).await,
        },
        _ => {}
    }

    // The gate, at the grant's level, on the path as resolved and as written:
    // a rule on a login file or a secret holds whichever way it is reached.
    // A delete is a change to its file for these rules.
    let written = live.written.lock().map(|w| w.clone()).unwrap_or_default();
    let ctx = Ctx { connected: true, posix: facts.posix, linux: facts.linux, home: &home, written: &written, always_run: &[], files_only: facts.files_only };
    let homes = act::home_forms(&home, &facts);
    let paths: Vec<&str> = if as_written == path { vec![path.as_str()] } else { vec![path.as_str(), as_written.as_str()] };
    let gate_tool = match &tool {
        FileTool::Ai(t) => t.clone(),
        FileTool::Delete { path } => Tool::Write { path: path.clone(), content: String::new() },
    };
    let mut decision = act::decide_strictest(grant.mode.level(), &gate_tool, &paths, &homes, ctx);
    // On top of the gate: a delete always asks, and with "ask each time"
    // every call does. A delete's card says why, unless the gate has a
    // warning of its own about the path.
    let delete = matches!(tool, FileTool::Delete { .. });
    decision = match decision {
        Decision::Allow(_) if delete => Decision::Ask { reason: DELETE_ASKS.into(), red: None },
        Decision::Allow(_) if grant.mode == Mode::AskEach => Decision::Ask { reason: EACH_ASKS.into(), red: None },
        Decision::Ask { red: None, .. } if delete => Decision::Ask { reason: DELETE_ASKS.into(), red: None },
        d => d,
    };
    let asked = matches!(decision, Decision::Ask { .. });
    let how = match decision {
        Decision::Refuse(why) => return unrun(hub, &grant, record, "refused", &why).await,
        Decision::Allow(how) => {
            hub.call_event(&grant, AiEvent::ToolCall { call_id: record.id.clone(), tool: record.tool.clone(), input: record.input.clone(), needs: "none".into(), reason: None, red: None, path: record.path.clone(), preview: record.preview.clone() });
            how_name(how).to_string()
        }
        Decision::Ask { reason, red } => {
            record.red = red.clone();
            match ask_user(hub, &grant, &record, reason, red, &halt).await {
                None => return unrun(hub, &grant, record, "stopped", STOPPED).await,
                Some(Answer::Deny { note }) => return unrun(hub, &grant, record, "denied", &denied(note)).await,
                Some(Answer::Allow { .. }) => {}
                // Only a command can be changed on its card.
                Some(Answer::Edit { .. }) => return unrun(hub, &grant, record, "failed", "only a command can be changed").await,
            }
            // The user may have taken a while to answer: the grant again.
            if let Err(why) = hub.grant(connection, access) {
                return unrun(hub, &grant, record, "refused", &why).await;
            }
            "approved".to_string()
        }
    };
    if *halt.borrow() {
        return unrun(hub, &grant, record, "stopped", STOPPED).await;
    }
    record.how = Some(how.clone());
    hub.call_event(&grant, AiEvent::ToolRunning { call_id: record.id.clone(), how, command: None });

    // What Undo needs for a change, as for the assistant's: the content
    // before (none: it made the file) and the hash of what it wrote.
    let mut undo: Option<act::Undo> = None;
    // The path still leads where it did when the user was asked: the
    // folders on the way are resolved once more (a write does that itself).
    let done: Result<String, (&'static str, String)> = match tool {
        FileTool::Ai(Tool::List { .. }) => match until_halt(&halt, async {
            if asked {
                act::unmoved(&fs, &path).await?;
            }
            fs.list(&path).await
        })
        .await
        {
            None => Err(("stopped", STOPPED.into())),
            Some(r) => r.map_err(|e| ("failed", e)),
        },
        FileTool::Ai(Tool::Read { start_line, max_lines, .. }) => match until_halt(&halt, async {
            if asked {
                act::unmoved(&fs, &path).await?;
            }
            fs.read_lines(&path, start_line, max_lines).await
        })
        .await
        {
            None => Err(("stopped", STOPPED.into())),
            Some(Ok((text, stamp))) => {
                live.remember(&path, stamp, false);
                Ok(text)
            }
            Some(Err(e)) => Err(("failed", e)),
        },
        FileTool::Ai(Tool::Write { .. } | Tool::Edit { .. }) => {
            let (new, old) = planned.unwrap_or_default();
            match act::checked_write(&fs, &path, res.exists, old.as_deref(), seen.as_ref(), &new, &halt).await {
                Wrote::Done(stamp) => {
                    undo = Some(act::Undo { path: path.clone(), before: old, after_sha: stamp.sha256.clone().unwrap_or_default() });
                    live.remember(&path, stamp, true);
                    // The same file under the name the agent gave: read
                    // again before another change under that name.
                    if as_written != path {
                        live.remember(&as_written, act::UNREAD, true);
                    }
                    let size = shape::thousands(new.len() as u64);
                    Ok(if res.exists { format!("Wrote {size} bytes to {}", shape::one_line(&path)) } else { format!("Created {} ({size} bytes)", shape::one_line(&path)) })
                }
                Wrote::NotRun { state: "stopped", .. } => Err(("stopped", STOPPED.into())),
                Wrote::NotRun { state, why } => Err((state, why)),
                Wrote::Failed { state, why, after_sha } => {
                    // It may hold part of the write: read again before
                    // another change, and Undo is kept.
                    if let Some(after_sha) = after_sha {
                        live.remember(&path, act::UNREAD, true);
                        undo = Some(act::Undo { path: path.clone(), before: old, after_sha });
                    }
                    Err((state, why))
                }
            }
        }
        FileTool::Delete { .. } => {
            // Where the card said, and still one file. Once sent, the
            // removal is waited for: a call given up then may have removed
            // it all the same.
            match until_halt(&halt, async {
                act::unmoved(&fs, &path).await?;
                fs.stat(&path).await?.ok_or_else(|| "the file is gone; read the folder again".to_string())
            })
            .await
            {
                None => Err(("stopped", STOPPED.into())),
                Some(Err(e)) => Err(("failed", e)),
                Some(Ok(_)) => match fs.remove(&path).await {
                    Ok(()) => {
                        live.forget(&path);
                        Ok(format!("Deleted {}", shape::one_line(&path)))
                    }
                    Err(e) => Err(("failed", e)),
                },
            }
        }
        FileTool::Ai(Tool::Run { .. }) => Err(("failed", "not a file tool".into())),
    };
    match done {
        Ok(text) => {
            // Hidden characters out, then the address: the agent gets this
            // text and nothing else of the file.
            let text = live.mask.apply(&shape::strip_tags(&text)).into_owned();
            record.state = "done".into();
            record.summary = text.lines().next().unwrap_or("").to_string();
            record.result = text.clone();
            hub.call_event(&grant, AiEvent::ToolResult { call_id: record.id.clone(), record: act::trimmed(record.clone()) });
            hub.log_action(&grant, &record, undo).await;
            (text, false)
        }
        Err((state, why)) => failed_with(hub, &grant, record, state, &why, undo).await,
    }
}

/// A command whose purpose is to print where the server is: its network
/// addresses, its names in the DNS, or its address as the outside sees it.
/// Returns why it asks. A list of the usual ways, not a proof: the mask and
/// the user's eye are the other two parts.
pub fn prints_addresses(command: &str) -> Option<&'static str> {
    const ADDRESSES: &str = "This command prints the server's network addresses";
    const LOOKUP: &str = "This command looks names up in the DNS, which can show the server's address";
    const OUTSIDE: &str = "This command asks an outside service for the server's public address";
    const FILES: &str = "This command reads files that hold the server's names and addresses";
    let lower = command.to_lowercase();
    for service in ["ifconfig.me", "ifconfig.co", "ifconfig.io", "ipinfo.io", "icanhazip", "ipify", "checkip", "ip.sb", "ipecho", "whatismyip", "myip.", "myexternalip", "ident.me", "ip-api.com", "wtfismyip", "opendns.com", "ip.me"] {
        if lower.contains(service) {
            return Some(OUTSIDE);
        }
    }
    for file in ["/etc/hosts", "/etc/hostname", "/proc/sys/kernel/hostname", "/etc/netplan", "/etc/network/interfaces", "/etc/sysconfig/network", "/etc/networkmanager", "/proc/net/fib_trie", "/etc/resolv.conf", "/etc/wireguard", "/sys/class/net"] {
        if lower.contains(file) {
            return Some(FILES);
        }
    }
    // Each simple command of the line, by its first word.
    for part in lower.split(|c: char| matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '{' | '}' | '\n' | '\r' | '<' | '>')) {
        let mut words = part.split_whitespace().map(|w| w.trim_matches(|c: char| matches!(c, '"' | '\'' | '\\' | '$'))).filter(|w| !w.is_empty()).peekable();
        // What stands before the program: assignments, and words that run
        // another program.
        let mut program = "";
        while let Some(w) = words.next() {
            let name = w.rsplit('/').next().unwrap_or(w);
            let is_assignment = w.contains('=') && !w.starts_with('-') && w.split('=').next().is_some_and(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            if is_assignment || matches!(name, "sudo" | "doas" | "env" | "command" | "exec" | "nohup" | "time" | "nice" | "ionice" | "busybox" | "stdbuf" | "setsid" | "then" | "do" | "else" | "if" | "while" | "!" | "xargs") || name.starts_with('-') {
                continue;
            }
            // `timeout 5 ip addr`: the number is skipped with it.
            if matches!(name, "timeout" | "watch") {
                while words.peek().is_some_and(|n| n.starts_with('-') || n.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b's' | b'm' | b'h'))) {
                    words.next();
                }
                continue;
            }
            program = name;
            break;
        }
        let rest: Vec<&str> = words.collect();
        match program {
            "ip" | "ifconfig" | "ipconfig" | "nmcli" | "networkctl" | "hostnamectl" | "resolvectl" | "ipmaddr" | "iwconfig" | "netplan" | "wg" => return Some(ADDRESSES),
            "hostname" if rest.iter().any(|a| a.starts_with('-')) => return Some(ADDRESSES),
            "dig" | "nslookup" | "host" | "drill" | "kdig" | "resolveip" => return Some(LOOKUP),
            "getent" if rest.iter().any(|a| a.starts_with("hosts") || a.starts_with("ahosts")) => return Some(LOOKUP),
            _ => {}
        }
    }
    None
}

/// What a call is about, in a line, for the activity log: the command, or
/// the file's path (as resolved where it was).
pub fn input_summary(record: &CallRecord) -> String {
    if let Some(c) = &record.command {
        return c.clone();
    }
    match record.tool.as_str() {
        "run_command" => record.input.get("command").and_then(Value::as_str).unwrap_or("").to_string(),
        "server_facts" => String::new(),
        _ => record.path.clone().or_else(|| record.input.get("path").and_then(Value::as_str).map(str::to_string)).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_that_tell_where_the_server_is_are_known() {
        for c in [
            "ip addr",
            "ip a",
            "ip -br -4 a",
            "/sbin/ip route get 1.1.1.1",
            "sudo ifconfig eth0",
            "ifconfig | grep inet",
            "hostname -I",
            "hostname --all-ip-addresses",
            "hostname -f",
            "echo start; hostname -i && echo done",
            "nmcli device show",
            "resolvectl status",
            "LC_ALL=C ip -o addr show",
            "env LANG=C /usr/sbin/ip a",
            "timeout 5 ip addr",
            "nohup ip addr &",
            "docker ps | xargs -n1 ip netns",
            "x=$(ip -4 addr)",
            "echo `ifconfig`",
            "if true; then ip a; fi",
            "dig +short myip.opendns.com @resolver1.opendns.com",
            "nslookup example.com",
            "host example.com",
            "getent hosts example.com",
            "getent ahostsv4 localhost",
            "curl -s ifconfig.me",
            "wget -qO- https://api.ipify.org",
            "curl https://ipinfo.io/ip",
            "CURL -S IFCONFIG.ME",
            "cat /etc/hosts",
            "grep -r . /etc/netplan/",
            "head -1 /etc/hostname",
            "cat /sys/class/net/eth0/address",
            "wg show",
        ] {
            assert!(prints_addresses(c).is_some(), "{c}");
        }
        for c in [
            "uptime",
            "df -h",
            "ps aux | grep nginx",
            "hostname",
            "systemctl status nginx",
            "journalctl -u ssh -n 50",
            "docker ps",
            "cat /etc/os-release",
            "ls -la /var/log",
            "echo ip addr",
            "grep -c ip /var/log/syslog",
            "tail -n 100 /var/log/nginx/access.log",
            "free -m && nproc",
            "git -C /srv/app log -3",
            "zip -r backup.zip /srv/app",
            "pip list",
            "skip=1 ./deploy.sh",
            "",
        ] {
            assert_eq!(prints_addresses(c), None, "{c}");
        }
    }

    #[test]
    fn the_log_names_the_command_or_the_file() {
        let r = |tool: &str, input: Value, path: Option<&str>, command: Option<&str>| CallRecord { tool: tool.into(), input, path: path.map(str::to_string), command: command.map(str::to_string), ..Default::default() };
        assert_eq!(input_summary(&r("run_command", json!({"command": "df -h"}), None, None)), "df -h");
        // The command the user changed it to.
        assert_eq!(input_summary(&r("run_command", json!({"command": "df -h"}), None, Some("df -h /"))), "df -h /");
        // A file call: the path as resolved, else as the agent wrote it.
        assert_eq!(input_summary(&r("read_file", json!({"path": "~/a"}), Some("/home/u/a"), None)), "/home/u/a");
        assert_eq!(input_summary(&r("delete_file", json!({"path": "x"}), None, None)), "x");
        assert_eq!(input_summary(&r("server_facts", json!({}), None, None)), "");
    }

    #[test]
    fn a_denial_carries_the_users_words_and_nothing_hidden() {
        assert_eq!(denied(None), DENIED);
        assert_eq!(denied(Some("   ".into())), DENIED);
        assert_eq!(denied(Some(" not on production ".into())), "the user denied this: not on production");
        // Tag characters, which can't be seen, are taken out of the note.
        assert_eq!(denied(Some("no\u{e0041}\u{e0042}".into())), "the user denied this: no");
        assert!(denied(Some("x".repeat(5000))).len() < 2100);
    }

    #[tokio::test]
    async fn either_signal_stops_a_call() {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (end_tx, end_rx) = watch::channel(false);
        let (rx, _w) = halt(stop_rx.clone(), end_rx.clone());
        assert!(!*rx.borrow());
        stop_tx.send(true).unwrap();
        let mut seen = rx.clone();
        tokio::time::timeout(Duration::from_secs(5), on(&mut seen)).await.unwrap();

        let (stop_tx2, stop_rx2) = watch::channel(false);
        let (rx, _w) = halt(stop_rx2, end_rx);
        end_tx.send(true).unwrap();
        let mut seen = rx.clone();
        tokio::time::timeout(Duration::from_secs(5), on(&mut seen)).await.unwrap();
        drop(stop_tx2);

        // One that was already given up starts out stopped.
        let (rx, _w) = halt(stop_rx, watch::channel(false).1);
        assert!(*rx.borrow());
    }
}
