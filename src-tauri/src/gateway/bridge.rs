//! `submarine mcp`: the program an agent starts to talk to Submarine. Agents
//! speak MCP over a child's stdin and stdout; the app listens on a pipe.
//! The bridge is this same program file, started without a window, standing
//! between the two.
//!
//! It holds nothing: no profile, no key, no file. While the app answers, it
//! passes each line on as it is. While the app can't be reached, it answers
//! by itself, with the real tool list and a sentence that says what the user
//! has to do, and it tries again on the next line. So an agent keeps its
//! Submarine tools across the app being closed and opened, and a call that
//! the app never answered gets an answer too.
//!
//! The app trusts a bridge no more than any other program that opens its
//! pipe: every check is the app's own.

use super::ipc::Unreachable;
use super::mcp::{self, About, Era, Id, Session, Step};
use super::serve::{read_line, write_lines, Line};
use super::tools;
use crate::ai::shape::cut;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::mpsc;

/// The first argument that makes this program the bridge.
pub const ARGUMENT: &str = "mcp";
/// How often, at most, the app is looked for while it can't be reached.
const PACE: Duration = Duration::from_secs(1);
const OUT_QUEUE: usize = 32;
const WRITER_GRACE: Duration = Duration::from_secs(2);

/// What a call gets when the app went away before answering it.
pub const CUT_OFF: &str = "Submarine closed the connection before this call was answered (Submarine was closed, or AI agent access was turned off). Whether the call ran isn't known: check before you repeat it. Every access this connection held has ended.";

/// This process was started as the bridge.
pub fn wanted() -> bool {
    std::env::args_os().nth(1).is_some_and(|a| a == ARGUMENT)
}

/// Runs the bridge until the agent closes its end, then ends the process.
/// Nothing of the app is started: no window, no web view, no profile.
pub fn main() -> ! {
    let code = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => {
            runtime.block_on(run(tokio::io::stdin(), tokio::io::stdout(), Pipe, PACE));
            0
        }
        Err(_) => 1,
    };
    // Not a return: a read of stdin that is still waiting can't be taken
    // back, and would keep the process alive.
    std::process::exit(code)
}

/// How the bridge reaches the app. A trait so the tests can stand in for it.
pub trait Dial: Send + Sync + 'static {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    fn dial(&self) -> impl Future<Output = Result<Self::Stream, Unreachable>> + Send;
}

/// The app's own endpoint: this user's, this copy's.
struct Pipe;

impl Dial for Pipe {
    type Stream = super::ipc::ClientStream;

    async fn dial(&self) -> Result<Self::Stream, Unreachable> {
        let name = super::ipc::endpoint().map_err(Unreachable::Other)?;
        super::ipc::connect(&name).await
    }
}

/// What the bridge's two sides share about the link to the app.
#[derive(Default)]
struct Shared {
    /// The number of the link in use; 0 when there is none.
    link: u64,
    /// Requests passed to the app and not answered yet. `Some`: a tool call,
    /// with the generation its answer is written in.
    flying: HashMap<Id, Option<Era>>,
    /// Ids of handshakes the bridge sent by itself: their answers are the
    /// bridge's to read, not the agent's.
    swallow: HashSet<String>,
}

struct Link<S> {
    number: u64,
    to_app: WriteHalf<S>,
    reading: tokio::task::JoinHandle<()>,
}

fn sentence(why: &Unreachable) -> String {
    match why {
        Unreachable::Absent => "Submarine isn't running, or AI agent access is turned off in it. Ask the user to open Submarine and turn on Settings → AI agents, then try again.".into(),
        Unreachable::Busy => "Submarine has too many agent connections open right now. Ask the user to close one, then try again.".into(),
        Unreachable::NotOurs => "Another program answers under Submarine's name on this computer, so nothing was sent to it. Tell the user: something on this computer is pretending to be Submarine.".into(),
        Unreachable::Other(e) => format!("Submarine couldn't be reached: {}", cut(e, 200)),
    }
}

/// The bridge itself, between any two streams and any way of reaching the
/// app. Ends when the agent's side closes.
pub async fn run<I, O, D>(input: I, output: O, dial: D, pace: Duration)
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    D: Dial,
{
    let about = Arc::new(tools::about());
    let (out, out_rx) = mpsc::channel::<String>(OUT_QUEUE);
    let mut writer = tokio::spawn(write_lines(output, out_rx));
    let shared = Arc::new(StdMutex::new(Shared::default()));
    let mut reader = BufReader::new(input);
    let mut buf = Vec::new();
    // The same reading of the agent's lines as the app's own: it knows which
    // generation each request is in, and answers when the app can't.
    let mut shadow = Session::new();
    // The agent's handshake, kept to introduce it to an app that comes later.
    let mut hello: Option<Value> = None;
    let mut link: Option<Link<D::Stream>> = None;
    let mut links = 0u64;
    let mut tried: Option<Instant> = None;
    let mut why = Unreachable::Absent;
    loop {
        let text = match read_line(&mut reader, &mut buf).await {
            Line::Text(t) => t,
            Line::NotText => {
                if out.send(mcp::error(None, mcp::PARSE_ERROR, "Parse error: the line isn't UTF-8", None)).await.is_err() {
                    break;
                }
                continue;
            }
            Line::TooLong | Line::End => break,
        };
        if text.trim().is_empty() {
            continue;
        }
        let step = shadow.handle(&text, &about);
        let asked = request_of(&text);
        let greeting = matches!(asked, Some((_, true)));
        if greeting {
            hello = serde_json::from_str(&text).ok();
        }

        // A link whose other side closed is let go of.
        if link.as_ref().is_some_and(|l| !shared.lock().is_ok_and(|s| s.link == l.number)) {
            if let Some(l) = link.take() {
                l.reading.abort();
            }
        }
        if link.is_none() && tried.is_none_or(|t| t.elapsed() >= pace) {
            tried = Some(Instant::now());
            match dial.dial().await {
                Ok(stream) => {
                    links += 1;
                    let (from_app, to_app) = tokio::io::split(stream);
                    if let Ok(mut s) = shared.lock() {
                        s.link = links;
                    }
                    let reading = tokio::spawn(from_app_to_agent(from_app, links, shared.clone(), out.clone(), about.clone()));
                    let mut fresh = Link { number: links, to_app, reading };
                    // An agent that shook hands before this app was there is
                    // introduced to it: the same handshake, under an id of
                    // the bridge's own, whose answer the bridge keeps.
                    let mut usable = true;
                    if let (Some(h), false) = (&hello, greeting) {
                        let id = format!("submarine-bridge-hello-{links}");
                        let mut again = h.clone();
                        if let Some(o) = again.as_object_mut() {
                            o.insert("id".into(), json!(id));
                        }
                        if let Ok(mut s) = shared.lock() {
                            s.swallow.insert(id);
                        }
                        usable = send(&mut fresh.to_app, &again.to_string()).await;
                    }
                    if usable {
                        link = Some(fresh);
                    } else {
                        fresh.reading.abort();
                        why = Unreachable::Absent;
                    }
                }
                Err(e) => why = e,
            }
        }

        // Pass the line on, or answer it here.
        let mut passed = false;
        let mut cut_off = false;
        if let Some(l) = link.as_mut() {
            let track: Option<(Id, Option<Era>)> = match (&step, &asked) {
                (Step::Call(c), _) => Some((c.id.clone(), Some(c.era))),
                (_, Some((id, _))) => Some((id.clone(), None)),
                _ => None,
            };
            // Noted before it is sent: the answer may come at once.
            let live = match shared.lock() {
                Ok(mut s) if s.link == l.number => {
                    if let Some((id, kind)) = &track {
                        s.flying.insert(id.clone(), *kind);
                    }
                    true
                }
                _ => false,
            };
            if live {
                if send(&mut l.to_app, &text).await {
                    passed = true;
                } else {
                    // The app went away under this line. If the reader's
                    // goodbye already answered it, it is answered.
                    let still_ours = match (&track, shared.lock()) {
                        (Some((id, _)), Ok(mut s)) => s.flying.remove(id).is_some(),
                        (None, _) => true,
                        _ => false,
                    };
                    passed = !still_ours;
                    cut_off = still_ours;
                }
            }
        }
        if !passed {
            let mut gone = false;
            match step {
                Step::Reply(lines) => {
                    for l in lines {
                        gone = gone || out.send(l).await.is_err();
                    }
                }
                Step::Call(c) => {
                    let text = if cut_off { CUT_OFF.to_string() } else { sentence(&why) };
                    gone = out.send(mcp::tool_result(&c.id, c.era, &about, &text, true)).await.is_err();
                }
                Step::Cancel(_) | Step::Nothing => {}
            }
            if gone {
                break;
            }
        }
    }
    if let Some(l) = link.take() {
        l.reading.abort();
    }
    drop(out);
    if tokio::time::timeout(WRITER_GRACE, &mut writer).await.is_err() {
        writer.abort();
    }
}

async fn send<W: AsyncWrite + Unpin>(to: &mut W, line: &str) -> bool {
    to.write_all(line.as_bytes()).await.is_ok() && to.write_all(b"\n").await.is_ok() && to.flush().await.is_ok()
}

/// The app's lines, passed to the agent. When the app's side closes, every
/// request it never answered is answered here, once.
async fn from_app_to_agent<R>(from: R, number: u64, shared: Arc<StdMutex<Shared>>, out: mpsc::Sender<String>, about: Arc<About>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(from);
    let mut buf = Vec::new();
    loop {
        let Line::Text(text) = read_line(&mut reader, &mut buf).await else { break };
        let id = answer_id(&text);
        let ours_to_keep = match (&id, shared.lock()) {
            (Some(v), Ok(mut s)) => {
                if v.as_str().is_some_and(|t| s.swallow.remove(t)) {
                    true
                } else {
                    if let Some(id) = Id::read(v) {
                        s.flying.remove(&id);
                    }
                    false
                }
            }
            _ => false,
        };
        if !ours_to_keep && out.send(text).await.is_err() {
            break;
        }
    }
    let left: Vec<(Id, Option<Era>)> = match shared.lock() {
        Ok(mut s) if s.link == number => {
            s.link = 0;
            s.flying.drain().collect()
        }
        _ => Vec::new(),
    };
    for (id, kind) in left {
        let line = match kind {
            Some(era) => mcp::tool_result(&id, era, &about, CUT_OFF, true),
            None => mcp::error(Some(&id), mcp::INTERNAL_ERROR, "Submarine closed the connection", None),
        };
        if out.send(line).await.is_err() {
            break;
        }
    }
}

/// A request's id, and whether it is the legacy handshake.
fn request_of(line: &str) -> Option<(Id, bool)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let method = v.get("method")?.as_str()?;
    let id = Id::read(v.get("id")?)?;
    Some((id, method == "initialize"))
}

/// The id a line answers, when the line is an answer.
fn answer_id(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("result").is_none() && v.get("error").is_none() {
        return None;
    }
    v.get("id").cloned()
}

#[cfg(test)]
mod tests {
    use super::super::serve::{serve, Calls};
    use super::super::tools::Call;
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, DuplexStream, ReadHalf};
    use tokio::sync::watch;

    /// The app's side: `status` answers, `list_access` waits to be stopped.
    #[derive(Default)]
    struct App {
        started: AtomicUsize,
        clients: StdMutex<Vec<String>>,
    }

    impl Calls for App {
        async fn call(&self, _connection: u64, call: Call, mut stop: watch::Receiver<bool>) -> (String, bool) {
            self.started.fetch_add(1, Ordering::SeqCst);
            match call {
                Call::Status => ("ready".into(), false),
                Call::ListAccess => {
                    while !*stop.borrow() {
                        if stop.changed().await.is_err() {
                            break;
                        }
                    }
                    ("stopped".into(), true)
                }
                _ => ("no".into(), true),
            }
        }

        fn client(&self, _connection: u64, client: &mcp::Client) {
            self.clients.lock().unwrap().push(client.name.clone());
        }

        fn closed(&self, _connection: u64) {}
    }

    /// The way to the app, for tests: shut, served by the real `serve`, or
    /// handed to the test to play the app by hand.
    struct Door {
        open: Arc<AtomicBool>,
        by_hand: Arc<AtomicBool>,
        app: Arc<App>,
        hands: mpsc::UnboundedSender<DuplexStream>,
        dials: Arc<AtomicUsize>,
    }

    impl Dial for Door {
        type Stream = DuplexStream;

        async fn dial(&self) -> Result<DuplexStream, Unreachable> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            if !self.open.load(Ordering::SeqCst) {
                return Err(Unreachable::Absent);
            }
            let (ours, theirs) = tokio::io::duplex(256 * 1024);
            if self.by_hand.load(Ordering::SeqCst) {
                let _ = self.hands.send(theirs);
            } else {
                // Nobody kicks: the sender is gone at once, which is "never".
                let (_, never) = watch::channel(false);
                tokio::spawn(serve(theirs, 1, self.app.clone(), Arc::new(tools::about()), never));
            }
            Ok(ours)
        }
    }

    struct Rig {
        to: WriteHalf<DuplexStream>,
        from: BufReader<ReadHalf<DuplexStream>>,
        open: Arc<AtomicBool>,
        by_hand: Arc<AtomicBool>,
        app: Arc<App>,
        hands: mpsc::UnboundedReceiver<DuplexStream>,
        dials: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    fn rig(open: bool) -> Rig {
        let (agent, bridge) = tokio::io::duplex(256 * 1024);
        let (bridge_in, bridge_out) = tokio::io::split(bridge);
        let (hands_tx, hands) = mpsc::unbounded_channel();
        let door = Door {
            open: Arc::new(AtomicBool::new(open)),
            by_hand: Arc::new(AtomicBool::new(false)),
            app: Arc::new(App::default()),
            hands: hands_tx,
            dials: Arc::new(AtomicUsize::new(0)),
        };
        let (open, by_hand, app, dials) = (door.open.clone(), door.by_hand.clone(), door.app.clone(), door.dials.clone());
        let task = tokio::spawn(run(bridge_in, bridge_out, door, Duration::ZERO));
        let (from, to) = tokio::io::split(agent);
        Rig { to, from: BufReader::new(from), open, by_hand, app, hands, dials, task }
    }

    impl Rig {
        async fn say(&mut self, message: Value) {
            self.to.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        }

        async fn hear(&mut self) -> Option<Value> {
            let mut line = String::new();
            match tokio::time::timeout(Duration::from_secs(5), self.from.read_line(&mut line)).await {
                Ok(Ok(n)) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
                _ => None,
            }
        }

        async fn hello(&mut self) -> Value {
            self.say(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "tester", "version": "1.0"}}})).await;
            self.hear().await.unwrap()
        }

        async fn status(&mut self, id: i64) -> Value {
            self.say(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "status", "arguments": {}}})).await;
            self.hear().await.unwrap()
        }
    }

    fn text(answer: &Value) -> &str {
        answer["result"]["content"][0]["text"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn with_no_app_the_bridge_answers_by_itself() {
        let mut r = rig(false);
        let hi = r.hello().await;
        assert_eq!(hi["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(hi["result"]["serverInfo"]["name"], "submarine");
        r.say(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        let list = r.hear().await.unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), tools::definitions().len());
        let a = r.status(2).await;
        assert_eq!(a["result"]["isError"], true);
        assert!(text(&a).contains("isn't running") && text(&a).contains("Settings"));
        // A call the app would refuse on its form is still told where the
        // app is, not judged here.
        r.say(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "run_command", "arguments": {"command": "id"}}})).await;
        assert!(text(&r.hear().await.unwrap()).contains("isn't running"));
        // What the protocol refuses is refused the same way as by the app.
        r.say(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "format_disk"}})).await;
        assert_eq!(r.hear().await.unwrap()["error"]["code"], mcp::INVALID_PARAMS);
        assert!(r.dials.load(Ordering::SeqCst) >= 4);
    }

    #[tokio::test]
    async fn with_the_app_there_lines_pass_both_ways() {
        let mut r = rig(true);
        let hi = r.hello().await;
        assert_eq!(hi["id"], 0);
        assert_eq!(hi["result"]["protocolVersion"], "2025-11-25");
        let a = r.status(1).await;
        assert_eq!((a["id"].as_i64(), text(&a)), (Some(1), "ready"));
        assert_eq!(*r.app.clients.lock().unwrap(), ["tester"]);
        // One link serves them all.
        r.status(2).await;
        assert_eq!(r.dials.load(Ordering::SeqCst), 1);
        // The agent closing its side ends the bridge.
        let Rig { to, from, task, .. } = r;
        drop(to);
        drop(from);
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_app_that_comes_later_is_found_and_told_who_the_agent_is() {
        let mut r = rig(false);
        r.hello().await;
        assert!(text(&r.status(1).await).contains("isn't running"));
        r.open.store(true, Ordering::SeqCst);
        // The same agent, no new handshake from it: the call is answered by
        // the app, and the agent sees one answer, the call's.
        let a = r.status(2).await;
        assert_eq!((a["id"].as_i64(), text(&a)), (Some(2), "ready"));
        assert_eq!(*r.app.clients.lock().unwrap(), ["tester"]);
        let a = r.status(3).await;
        assert_eq!((a["id"].as_i64(), text(&a)), (Some(3), "ready"));
        // An id the agent used for its handshake is its own to use again.
        let a = r.status(0).await;
        assert_eq!((a["id"].as_i64(), text(&a)), (Some(0), "ready"));
    }

    #[tokio::test]
    async fn a_call_the_app_never_answered_is_answered_when_it_goes() {
        let mut r = rig(true);
        r.by_hand.store(true, Ordering::SeqCst);
        // A modern agent: nothing to shake.
        let meta = json!({"io.modelcontextprotocol/protocolVersion": mcp::MODERN, "io.modelcontextprotocol/clientCapabilities": {}});
        r.say(json!({"jsonrpc": "2.0", "id": "a", "method": "tools/call", "params": {"name": "list_access", "_meta": meta}})).await;
        r.say(json!({"jsonrpc": "2.0", "id": "b", "method": "tools/list", "params": {"_meta": meta}})).await;
        // The test is the app: it reads both lines as they were sent, and goes.
        let app_side = tokio::time::timeout(Duration::from_secs(5), r.hands.recv()).await.unwrap().unwrap();
        let mut app = BufReader::new(app_side);
        let mut got = String::new();
        app.read_line(&mut got).await.unwrap();
        assert!(got.contains("\"list_access\""));
        got.clear();
        app.read_line(&mut got).await.unwrap();
        assert!(got.contains("tools/list"));
        drop(app);
        let mut answers = vec![r.hear().await.unwrap(), r.hear().await.unwrap()];
        answers.sort_by_key(|a| a["id"].as_str().unwrap().to_string());
        assert_eq!(answers[0]["id"], "a");
        assert_eq!(answers[0]["result"]["isError"], true);
        assert_eq!(text(&answers[0]), CUT_OFF);
        assert_eq!(answers[0]["result"]["resultType"], "complete");
        assert_eq!((answers[1]["id"].as_str(), answers[1]["error"]["code"].as_i64()), (Some("b"), Some(mcp::INTERNAL_ERROR)));
        // Each got one answer, and the bridge goes on: the app is looked for
        // again on the next line.
        r.by_hand.store(false, Ordering::SeqCst);
        r.say(json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call", "params": {"name": "status", "_meta": meta}})).await;
        let a = r.hear().await.unwrap();
        assert_eq!((a["id"].as_str(), text(&a)), (Some("c"), "ready"));
        assert_eq!(r.dials.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_bridges_own_handshake_answer_never_reaches_the_agent() {
        let mut r = rig(false);
        r.hello().await;
        r.open.store(true, Ordering::SeqCst);
        r.by_hand.store(true, Ordering::SeqCst);
        r.say(json!({"jsonrpc": "2.0", "id": 5, "method": "ping"})).await;
        let app_side = tokio::time::timeout(Duration::from_secs(5), r.hands.recv()).await.unwrap().unwrap();
        let (app_from, mut app_to) = tokio::io::split(app_side);
        let mut app = BufReader::new(app_from);
        // First the agent's handshake again, under the bridge's id; then the ping.
        let mut first = String::new();
        app.read_line(&mut first).await.unwrap();
        let first: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first["method"], "initialize");
        assert_eq!(first["id"], "submarine-bridge-hello-1");
        assert_eq!(first["params"]["clientInfo"]["name"], "tester");
        let mut second = String::new();
        app.read_line(&mut second).await.unwrap();
        assert!(second.contains("\"ping\""));
        app_to.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"submarine-bridge-hello-1\",\"result\":{\"protocolVersion\":\"2025-11-25\"}}\n").await.unwrap();
        app_to.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n").await.unwrap();
        assert_eq!(r.hear().await.unwrap(), json!({"jsonrpc": "2.0", "id": 5, "result": {}}));
    }

    #[test]
    fn each_reason_has_its_sentence() {
        assert!(sentence(&Unreachable::Absent).contains("Settings"));
        assert!(sentence(&Unreachable::Busy).contains("too many"));
        assert!(sentence(&Unreachable::NotOurs).contains("pretending"));
        assert!(sentence(&Unreachable::Other("x".repeat(5000))).len() < 300);
        assert_eq!(request_of("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}"), Some((Id::Number(1), true)));
        assert_eq!(request_of("{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"tools/list\"}"), Some((Id::Text("x".into()), false)));
        assert_eq!(request_of("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}"), None);
        assert_eq!(request_of("not json"), None);
        assert_eq!(answer_id("{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}"), Some(json!(7)));
        assert_eq!(answer_id("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}"), None);
        assert_eq!(answer_id("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}"), None);
    }
}
