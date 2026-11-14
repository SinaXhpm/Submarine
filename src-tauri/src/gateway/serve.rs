//! One agent connection, whatever it runs over (a pipe, a socket, a test's
//! in-memory stream): lines in, lines out. The protocol is `mcp`'s, the
//! checks on a call's arguments are `tools`'; what a call does is behind
//! `Calls`, which the app implements.
//!
//! The agent on the other end is not trusted with anything here: a line has
//! a size limit, only so many calls run at once, and nothing it sends can
//! make this task panic or wait forever.

use super::mcp::{self, About, Client, Id, Session, Step};
use super::tools::{self, Call};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

/// The longest line a client may send. A write carries at most 1 MiB of
/// text, which JSON can double.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// Calls of one connection that may be in flight at once, the waiting ones
/// included.
pub const MAX_CALLS: usize = 8;
/// Lines waiting to be written. A client that doesn't read its answers
/// stops being read from once this fills.
const OUT_QUEUE: usize = 32;
/// How long the last answers get to go out after the client's side closed.
const WRITER_GRACE: Duration = Duration::from_secs(2);

pub const TOO_MANY_CALLS: &str = "too many calls are running on this connection at once; wait for one to end";

/// What the app does for a connection.
pub trait Calls: Send + Sync + 'static {
    /// Carries out one call. `stop` turns true when the client cancels the
    /// call or goes away: the work ends then, and what is returned is thrown
    /// away. Returns the text for the agent, and whether the call failed.
    fn call(&self, connection: u64, call: Call, stop: watch::Receiver<bool>) -> impl Future<Output = (String, bool)> + Send;

    /// The client said who it is, or changed what it says. Its own claim.
    fn client(&self, connection: u64, client: &Client);

    /// The connection is over; everything it started was told to stop.
    fn closed(&self, connection: u64);
}

type Flying = Arc<StdMutex<HashMap<Id, watch::Sender<bool>>>>;

/// Serves one connection until the client goes away, sends what ends it, or
/// `kick` turns true (the user disconnected it, or agent access was turned
/// off).
pub async fn serve<S, C>(stream: S, connection: u64, calls: Arc<C>, about: Arc<About>, mut kick: watch::Receiver<bool>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Calls,
{
    let (read, write) = tokio::io::split(stream);
    let (out, out_rx) = mpsc::channel::<String>(OUT_QUEUE);
    let writer = tokio::spawn(write_lines(write, out_rx));
    let mut reader = BufReader::new(read);
    let mut session = Session::new();
    let mut said: Option<Client> = None;
    let flying: Flying = Arc::new(StdMutex::new(HashMap::new()));
    let mut buf = Vec::new();
    loop {
        // A line half read when the kick comes is of no interest any more.
        let next = tokio::select! {
            biased;
            _ = kicked(&mut kick) => break,
            next = read_line(&mut reader, &mut buf) => next,
        };
        let line = match next {
            Line::Text(line) => line,
            Line::NotText => {
                if out.send(mcp::error(None, mcp::PARSE_ERROR, "Parse error: the line isn't UTF-8", None)).await.is_err() {
                    break;
                }
                continue;
            }
            Line::TooLong => {
                let _ = out.send(mcp::error(None, mcp::INVALID_REQUEST, "Invalid request: a message is larger than this server reads", None)).await;
                break;
            }
            Line::End => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let step = session.handle(&line, &about);
        if session.client() != said.as_ref() {
            said = session.client().cloned();
            if let Some(c) = &said {
                calls.client(connection, c);
            }
        }
        match step {
            Step::Nothing => {}
            Step::Reply(lines) => {
                let mut gone = false;
                for l in lines {
                    gone = gone || out.send(l).await.is_err();
                }
                if gone {
                    break;
                }
            }
            Step::Cancel(id) => {
                if let Ok(f) = flying.lock() {
                    if let Some(stop) = f.get(&id) {
                        let _ = stop.send(true);
                    }
                }
            }
            Step::Call(call) => {
                let (stop_tx, stop_rx) = watch::channel(false);
                // Decided under one lock: is the id free, is there room.
                let refused = match flying.lock() {
                    Ok(f) if f.contains_key(&call.id) => Some(mcp::error(Some(&call.id), mcp::INVALID_REQUEST, "Invalid request: that id is still in use by a call that hasn't ended", None)),
                    Ok(f) if f.len() >= MAX_CALLS => Some(mcp::tool_result(&call.id, call.era, &about, TOO_MANY_CALLS, true)),
                    Ok(mut f) => {
                        f.insert(call.id.clone(), stop_tx);
                        None
                    }
                    Err(_) => Some(mcp::error(Some(&call.id), mcp::INTERNAL_ERROR, "Internal error", None)),
                };
                if let Some(line) = refused {
                    if out.send(line).await.is_err() {
                        break;
                    }
                    continue;
                }
                let (calls, about, out, flying) = (calls.clone(), about.clone(), out.clone(), flying.clone());
                tokio::spawn(async move {
                    let mcp::ToolCall { id, era, name, arguments } = call;
                    let (text, failed) = match tools::parse(&name, arguments) {
                        Ok(parsed) => calls.call(connection, parsed, stop_rx.clone()).await,
                        Err(why) => (why, true),
                    };
                    if let Ok(mut f) = flying.lock() {
                        f.remove(&id);
                    }
                    // A cancelled call gets no answer at all, a late one too.
                    if !*stop_rx.borrow() {
                        let _ = out.send(mcp::tool_result(&id, era, &about, &text, failed)).await;
                    }
                });
            }
        }
    }
    // The client is gone: what it started stops, and nothing more is written.
    if let Ok(f) = flying.lock() {
        for stop in f.values() {
            let _ = stop.send(true);
        }
    }
    drop(out);
    calls.closed(connection);
    // The writer ends when the last call has let go of it. One that doesn't
    // stop isn't waited for.
    let mut writer = writer;
    if tokio::time::timeout(WRITER_GRACE, &mut writer).await.is_err() {
        writer.abort();
    }
}

/// Ends when `kick` turns true. A kick that can no longer come (its sender
/// is gone) never ends it.
async fn kicked(kick: &mut watch::Receiver<bool>) {
    loop {
        if *kick.borrow() {
            return;
        }
        if kick.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub(super) async fn write_lines<W: AsyncWrite + Unpin>(mut to: W, mut lines: mpsc::Receiver<String>) {
    while let Some(line) = lines.recv().await {
        if to.write_all(line.as_bytes()).await.is_err() || to.write_all(b"\n").await.is_err() || to.flush().await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

pub(super) enum Line {
    Text(String),
    /// A whole line that isn't UTF-8.
    NotText,
    TooLong,
    /// The stream ended, cleanly or in the middle of a line, or failed.
    End,
}

/// The next line, without its line break. Never reads more than `MAX_LINE`
/// bytes and one into memory.
pub(super) async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> Line {
    buf.clear();
    let mut limited = (&mut *reader).take(MAX_LINE as u64 + 1);
    match limited.read_until(b'\n', buf).await {
        Ok(0) | Err(_) => Line::End,
        Ok(_) => {
            if buf.last() != Some(&b'\n') {
                return if buf.len() > MAX_LINE { Line::TooLong } else { Line::End };
            }
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            match std::str::from_utf8(buf) {
                Ok(s) => Line::Text(s.to_string()),
                Err(_) => Line::NotText,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::DuplexStream;

    const ACCESS: &str = "0123456789abcdef0123456789abcdef";

    /// The app's side, for tests: `status` answers at once, `list_access`
    /// waits until it is told to stop, anything else fails.
    #[derive(Default)]
    struct Fake {
        calls: StdMutex<Vec<Call>>,
        started: AtomicUsize,
        stopped: AtomicUsize,
        closed: AtomicUsize,
        clients: StdMutex<Vec<String>>,
    }

    impl Calls for Fake {
        async fn call(&self, _connection: u64, call: Call, mut stop: watch::Receiver<bool>) -> (String, bool) {
            self.calls.lock().unwrap().push(call.clone());
            self.started.fetch_add(1, Ordering::SeqCst);
            match call {
                Call::Status => ("ready".into(), false),
                Call::ListAccess => {
                    while !*stop.borrow() {
                        if stop.changed().await.is_err() {
                            break;
                        }
                    }
                    self.stopped.fetch_add(1, Ordering::SeqCst);
                    ("stopped".into(), true)
                }
                _ => ("this fake does nothing else".into(), true),
            }
        }

        fn client(&self, _connection: u64, client: &Client) {
            self.clients.lock().unwrap().push(format!("{} {}", client.name, client.version));
        }

        fn closed(&self, _connection: u64) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Wire {
        to: tokio::io::WriteHalf<DuplexStream>,
        from: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        fake: Arc<Fake>,
        task: tokio::task::JoinHandle<()>,
        kick: watch::Sender<bool>,
    }

    fn start() -> Wire {
        let (ours, theirs) = tokio::io::duplex(256 * 1024);
        let fake = Arc::new(Fake::default());
        let (kick, kicked) = watch::channel(false);
        let task = tokio::spawn(serve(theirs, 1, fake.clone(), Arc::new(tools::about()), kicked));
        let (from, to) = tokio::io::split(ours);
        Wire { to, from: BufReader::new(from), fake, task, kick }
    }

    impl Wire {
        async fn say(&mut self, message: Value) {
            self.to.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        }

        async fn raw(&mut self, bytes: &[u8]) {
            self.to.write_all(bytes).await.unwrap();
        }

        /// The next line the server wrote, or `None` when it wrote nothing
        /// for a while, or closed.
        async fn hear(&mut self) -> Option<Value> {
            let mut line = String::new();
            match tokio::time::timeout(Duration::from_secs(5), self.from.read_line(&mut line)).await {
                Ok(Ok(n)) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
                _ => None,
            }
        }

        async fn hello(&mut self) {
            self.say(json!({"jsonrpc": "2.0", "id": "hello", "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "tester", "version": "1.0"}}})).await;
            assert_eq!(self.hear().await.unwrap()["id"], "hello");
        }

        async fn call(&mut self, id: i64, name: &str, arguments: Value) {
            self.say(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})).await;
        }

        /// Calls `status` under `id` until it runs: a call that just ended
        /// takes a moment to give up its place and its id.
        async fn status_when_free(&mut self, id: i64) -> Value {
            for _ in 0..200 {
                self.call(id, "status", json!({})).await;
                let a = self.hear().await.unwrap();
                if a["result"]["isError"] == false {
                    return a;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the place never came free");
        }

        /// Waits until the fake has seen `n` calls start.
        async fn started(&self, n: usize) {
            for _ in 0..500 {
                if self.fake.started.load(Ordering::SeqCst) >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("only {} calls started", self.fake.started.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn a_list_and_a_call_are_answered() {
        let mut w = start();
        w.hello().await;
        assert_eq!(*w.fake.clients.lock().unwrap(), ["tester 1.0"]);
        w.say(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        let list = w.hear().await.unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), tools::definitions().len());
        w.call(2, "status", json!({})).await;
        let done = w.hear().await.unwrap();
        assert_eq!(done["id"], 2);
        assert_eq!(done["result"]["content"][0]["text"], "ready");
        assert_eq!(done["result"]["isError"], false);
        // Blank lines and a Windows line end change nothing.
        w.raw(b"\n   \n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\r\n").await;
        assert_eq!(w.hear().await.unwrap(), json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
    }

    #[tokio::test]
    async fn a_call_that_fails_its_checks_never_reaches_the_app() {
        let mut w = start();
        w.hello().await;
        w.call(1, "run_command", json!({"command": "id"})).await;
        let a = w.hear().await.unwrap();
        assert_eq!(a["result"]["isError"], true);
        assert!(a["result"]["content"][0]["text"].as_str().unwrap().contains("needs `access`"));
        w.call(2, "request_access", json!({"purpose": "x", "operations": ["facts"], "server": "prod"})).await;
        let a = w.hear().await.unwrap();
        assert_eq!(a["result"]["isError"], true);
        w.call(3, "no_such_tool", json!({})).await;
        assert_eq!(w.hear().await.unwrap()["error"]["code"], mcp::INVALID_PARAMS);
        assert!(w.fake.calls.lock().unwrap().is_empty());
        // One that passes them does.
        w.call(4, "run_command", json!({"access": ACCESS, "command": "id"})).await;
        assert_eq!(w.hear().await.unwrap()["result"]["isError"], true);
        assert_eq!(*w.fake.calls.lock().unwrap(), [Call::Run { access: ACCESS.into(), command: "id".into(), timeout: 60 }]);
    }

    #[tokio::test]
    async fn a_cancelled_call_stops_and_is_never_answered() {
        let mut w = start();
        w.hello().await;
        w.call(5, "list_access", json!({})).await;
        w.started(1).await;
        w.say(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5, "reason": "changed my mind"}})).await;
        // A cancel for a call that isn't there is nothing.
        w.say(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 99}})).await;
        w.say(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"})).await;
        // The only thing that comes back is the ping's answer.
        assert_eq!(w.hear().await.unwrap()["id"], 6);
        for _ in 0..500 {
            if w.fake.stopped.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(w.fake.stopped.load(Ordering::SeqCst), 1);
        // The id comes free again, and nothing was sent for the cancelled
        // call: the next answer under that id is the new call's.
        let a = w.status_when_free(5).await;
        assert_eq!((a["id"].as_i64(), a["result"]["content"][0]["text"].as_str()), (Some(5), Some("ready")));
    }

    #[tokio::test]
    async fn a_client_that_goes_away_stops_what_it_started() {
        let mut w = start();
        w.hello().await;
        w.call(1, "list_access", json!({})).await;
        w.call(2, "list_access", json!({})).await;
        w.started(2).await;
        let Wire { to, from, fake, task, kick: _kick } = w;
        drop(to);
        drop(from);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert_eq!(fake.closed.load(Ordering::SeqCst), 1);
        for _ in 0..500 {
            if fake.stopped.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.stopped.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn only_so_many_calls_run_at_once() {
        let mut w = start();
        w.hello().await;
        for id in 0..MAX_CALLS as i64 {
            w.call(id, "list_access", json!({})).await;
        }
        w.started(MAX_CALLS).await;
        w.call(100, "status", json!({})).await;
        let a = w.hear().await.unwrap();
        assert_eq!(a["id"], 100);
        assert_eq!(a["result"]["isError"], true);
        assert_eq!(a["result"]["content"][0]["text"], TOO_MANY_CALLS);
        // An id that is still in use isn't taken twice.
        w.call(0, "status", json!({})).await;
        let a = w.hear().await.unwrap();
        assert_eq!((a["id"].as_i64(), a["error"]["code"].as_i64()), (Some(0), Some(mcp::INVALID_REQUEST)));
        assert_eq!(w.fake.started.load(Ordering::SeqCst), MAX_CALLS);
        // One ends, and there is room again.
        w.say(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 0}})).await;
        let a = w.status_when_free(101).await;
        assert_eq!(a["result"]["content"][0]["text"], "ready");
        assert_eq!(w.fake.stopped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_line_that_isnt_text_is_answered_and_the_connection_goes_on() {
        let mut w = start();
        w.raw(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\xff\xfeng\"}\n").await;
        assert_eq!(w.hear().await.unwrap()["error"]["code"], mcp::PARSE_ERROR);
        w.raw(b"this is not json\n").await;
        assert_eq!(w.hear().await.unwrap()["error"]["code"], mcp::PARSE_ERROR);
        w.say(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})).await;
        assert_eq!(w.hear().await.unwrap()["id"], 2);
    }

    #[tokio::test]
    async fn a_line_over_the_limit_ends_the_connection() {
        let Wire { mut to, mut from, fake, task, kick: _kick } = start();
        // The server stops reading at the limit, so the rest may not go
        // through: write from a task of its own.
        let writing = tokio::spawn(async move {
            let _ = to.write_all(&vec![b'x'; MAX_LINE + 2]).await;
            to
        });
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(30), from.read_line(&mut line)).await.unwrap().unwrap();
        let a: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(a["error"]["code"], mcp::INVALID_REQUEST);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert_eq!(fake.closed.load(Ordering::SeqCst), 1);
        writing.abort();
    }

    #[tokio::test]
    async fn a_line_at_the_limit_is_still_read() {
        let mut w = start();
        // As long as a line may be: blanks around a small message.
        let message = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";
        let mut line = vec![b' '; MAX_LINE - message.len()];
        line.extend_from_slice(message);
        line.push(b'\n');
        w.raw(&line).await;
        assert_eq!(w.hear().await.unwrap()["id"], 1);
    }

    #[tokio::test]
    async fn a_kick_ends_the_connection_and_what_it_was_doing() {
        let mut w = start();
        w.hello().await;
        w.call(1, "list_access", json!({})).await;
        w.started(1).await;
        w.kick.send(true).unwrap();
        let Wire { to: _to, mut from, fake, task, kick: _kick } = w;
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert_eq!(fake.closed.load(Ordering::SeqCst), 1);
        for _ in 0..500 {
            if fake.stopped.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.stopped.load(Ordering::SeqCst), 1);
        // The agent's side sees the end, with no answer to the stopped call.
        let mut rest = String::new();
        let _ = from.read_to_string(&mut rest).await;
        assert_eq!(rest, "");
    }

    #[tokio::test]
    async fn a_kick_that_can_no_longer_come_changes_nothing() {
        let mut w = start();
        let (gone, _) = watch::channel(false);
        drop(std::mem::replace(&mut w.kick, gone));
        w.hello().await;
        w.call(1, "status", json!({})).await;
        assert_eq!(w.hear().await.unwrap()["result"]["content"][0]["text"], "ready");
    }

    #[tokio::test]
    async fn a_stream_that_ends_in_the_middle_of_a_line_just_ends() {
        let mut w = start();
        w.raw(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"").await;
        let Wire { mut to, mut from, fake, task, kick: _kick } = w;
        // A split half alone never closes the stream: the agent's side says it
        // is done writing, as a closed pipe does.
        to.shutdown().await.unwrap();
        drop(to);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        let mut rest = String::new();
        let _ = from.read_to_string(&mut rest).await;
        assert_eq!(rest, "");
        assert_eq!(fake.closed.load(Ordering::SeqCst), 1);
    }
}
