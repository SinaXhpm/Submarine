//! What the gateway knows while it runs: who is connected, what they asked
//! for, what the user gave them. All of it lives in memory and goes with the
//! app: nothing here is written anywhere.
//!
//! Plain logic, with the clocks and the rest of the app passed in, so every
//! rule has a test: an agent sees only its own requests and grants, a grant
//! is made only by `approve` (which only the user's side calls), and each way
//! a grant ends is here.

use super::grant::{self, End, Grant, Mode, Now, Op, Until, World};
use super::ipc::Peer;
use super::mcp::Client;
use super::tools::{self, Ask, ACCESS_ID_LEN, REQUEST_ID_LEN};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

/// Agent connections at one time.
pub const MAX_CONNECTIONS: usize = 8;
/// Requests of one connection that may wait for the user at one time.
pub const MAX_WAITING: usize = 4;
/// A request nobody answered goes after this long.
pub const REQUEST_LIFE: Duration = Duration::from_secs(10 * 60);
/// After this many denials in a row, a connection may ask once in
/// `AFTER_DENIALS`: an agent can't wear the user down.
pub const DENIALS_BEFORE_PACING: u32 = 2;
pub const AFTER_DENIALS: Duration = Duration::from_secs(60);
/// Grants a connection may hold at one time.
pub const MAX_GRANTS: usize = 8;
/// How many ended grants are remembered, to tell a late call why.
const ENDED_KEPT: usize = 64;
/// Answered requests kept for `request_status`, per connection.
const ANSWERED_KEPT: usize = 16;

pub const TOO_MANY_CONNECTIONS: &str = "too many agents are connected to Submarine at once";
pub const LOCKED: &str = "Submarine is locked: no profile is open. Ask the user to unlock it, then try again";
pub const NOT_YOURS: &str = "that isn't an access of this connection. Ask for access with request_access";
pub const NO_SUCH_REQUEST: &str = "that isn't a request of this connection";

/// One agent connection.
#[derive(Clone, Debug)]
pub struct Connection {
    pub id: u64,
    /// When it opened, in Unix seconds.
    pub since: i64,
    /// Who it is, as the system says.
    pub peer: Peer,
    /// Who it says it is. Its own claim.
    pub client: Option<Client>,
    denials: u32,
    asked: Option<Instant>,
    /// How many grants it was ever given, for `server-N` labels.
    granted: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answered {
    Waiting,
    Approved { access: String },
    Denied { note: Option<String> },
    /// Nobody answered in time.
    Expired,
    /// The agent took it back, or went away.
    Withdrawn,
    /// The profile was locked, or agent access turned off, before an answer.
    Closed,
}

/// Why a call's grant can't be used. `ended` holds the grant when it ended
/// at this very check: whoever asked stops what it was running and tells the
/// user's side.
#[derive(Clone, Debug)]
pub struct Gone {
    /// What the agent is told.
    pub told: String,
    pub ended: Option<(Grant, End)>,
}

/// One request for access.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: String,
    pub connection: u64,
    pub ask: Ask,
    /// When it was made, in Unix seconds.
    pub at: i64,
    made: Instant,
    pub state: Answered,
}

/// What the user chose on a request's card.
#[derive(Clone, Debug)]
pub struct Approval {
    /// The server's uuid in the profile.
    pub server: String,
    /// The open session the operations will go through.
    pub session: String,
    /// The server's name in Submarine and its host as saved: the label the
    /// agent sees is made from them unless the user typed one.
    pub server_name: String,
    pub server_host: String,
    pub label: Option<String>,
    pub ops: Vec<Op>,
    /// The folders file operations may touch: absolute, resolved on the
    /// server by the caller. Kept only with a file operation.
    pub folders: Vec<String>,
    /// An FTP server: files only, so no command operation is given.
    pub ftp: bool,
    pub mode: Mode,
    pub minutes: Option<u32>,
    /// Where the server is, hidden in what the agent is told.
    pub mask: super::mask::Mask,
}

#[derive(Debug, Default)]
pub struct State {
    connections: BTreeMap<u64, Connection>,
    last_connection: u64,
    /// Oldest first.
    requests: Vec<Request>,
    grants: Vec<Grant>,
    /// Grants that ended, newest last: id, connection, why.
    ended: VecDeque<(String, u64, End)>,
}

impl State {
    // --- connections ---------------------------------------------------------

    pub fn connect(&mut self, peer: Peer, unix: i64) -> Result<u64, &'static str> {
        if self.connections.len() >= MAX_CONNECTIONS {
            return Err(TOO_MANY_CONNECTIONS);
        }
        self.last_connection += 1;
        let id = self.last_connection;
        self.connections.insert(id, Connection { id, since: unix, peer, client: None, denials: 0, asked: None, granted: 0 });
        Ok(id)
    }

    /// The connection closed: its grants end, its waiting requests go.
    pub fn disconnect(&mut self, connection: u64) -> Vec<Grant> {
        self.connections.remove(&connection);
        self.requests.retain(|r| r.connection != connection);
        self.end_where(|g| g.connection == connection, End::Disconnected)
    }

    pub fn said(&mut self, connection: u64, client: Client) {
        if let Some(c) = self.connections.get_mut(&connection) {
            c.client = Some(client);
        }
    }

    pub fn connection(&self, connection: u64) -> Option<&Connection> {
        self.connections.get(&connection)
    }

    pub fn connections(&self) -> impl Iterator<Item = &Connection> {
        self.connections.values()
    }

    // --- requests ------------------------------------------------------------

    /// An agent asks. Returns the request's id; the same id again when the
    /// same thing is already waiting, so an agent that repeats itself doesn't
    /// fill the user's panel.
    pub fn ask(&mut self, connection: u64, ask: Ask, now: Instant, unix: i64) -> Result<String, String> {
        self.expire(now);
        let Some(c) = self.connections.get(&connection) else {
            return Err("this connection is closed".into());
        };
        if let Some(same) = self.requests.iter().find(|r| r.connection == connection && r.state == Answered::Waiting && r.ask == ask) {
            return Ok(same.id.clone());
        }
        if c.denials >= DENIALS_BEFORE_PACING && c.asked.is_some_and(|t| now.saturating_duration_since(t) < AFTER_DENIALS) {
            return Err("the user denied your last requests. Don't ask again now: tell the user what you need and let them decide. A new request is taken in a minute".into());
        }
        let waiting = self.requests.iter().filter(|r| r.connection == connection && r.state == Answered::Waiting).count();
        if waiting >= MAX_WAITING {
            return Err(format!("{MAX_WAITING} of your requests are waiting for the user already. Check them with request_status, and don't ask again until they are answered"));
        }
        if self.grants.iter().filter(|g| g.connection == connection).count() >= MAX_GRANTS {
            return Err(format!("this connection holds {MAX_GRANTS} accesses already. Give one back with release_access first"));
        }
        let id = tools::new_id(REQUEST_ID_LEN);
        self.requests.push(Request { id: id.clone(), connection, ask, at: unix, made: now, state: Answered::Waiting });
        if let Some(c) = self.connections.get_mut(&connection) {
            c.asked = Some(now);
        }
        self.forget_old_answers(connection);
        Ok(id)
    }

    /// A request, for the connection that made it and for no other.
    pub fn request_of(&self, connection: u64, id: &str) -> Option<&Request> {
        self.requests.iter().find(|r| r.id == id && r.connection == connection)
    }

    /// Every request that waits, oldest first: what the user's panel shows.
    pub fn waiting(&self) -> impl Iterator<Item = &Request> {
        self.requests.iter().filter(|r| r.state == Answered::Waiting)
    }

    /// The user approves. This is the only place a grant is made, and
    /// nothing an agent sends reaches it: the server, the operations, the
    /// way they run and the time all come from the user's side.
    pub fn approve(&mut self, request: &str, a: Approval, now: Now, generation: u64, longest_minutes: u32) -> Result<Grant, String> {
        self.expire(now.steady);
        let Some(r) = self.requests.iter_mut().find(|r| r.id == request) else {
            return Err("[GATEWAY] NO_REQUEST: that request is gone".into());
        };
        if r.state != Answered::Waiting {
            return Err("[GATEWAY] ANSWERED: that request was answered, withdrawn or timed out already".into());
        }
        let Some(c) = self.connections.get_mut(&r.connection) else {
            return Err("[GATEWAY] GONE: the agent that asked has disconnected".into());
        };
        // What this version offers; on an FTP server no command runs.
        let ops: BTreeSet<Op> = a.ops.iter().copied().filter(|o| tools::OFFERED.contains(o) && !(a.ftp && matches!(o, Op::CommandsRead | Op::CommandsRun))).collect();
        if ops.is_empty() {
            return Err("[GATEWAY] NO_OPERATIONS: choose at least one operation".into());
        }
        // The folders, each once, and only for file operations.
        let mut folders: Vec<String> = Vec::new();
        if ops.iter().any(|o| o.files()) {
            for f in a.folders {
                if !grant::canonical(&f) {
                    return Err(format!("[GATEWAY] BAD_FOLDER: {} isn't a resolved absolute path", crate::ai::shape::cut(&f, 200)));
                }
                if !folders.contains(&f) {
                    folders.push(f);
                }
            }
            if folders.is_empty() {
                return Err("[GATEWAY] NO_FOLDERS: name at least one folder for the file operations".into());
            }
            if folders.len() > grant::MAX_FOLDERS {
                return Err(format!("[GATEWAY] TOO_MANY_FOLDERS: at most {} folders", grant::MAX_FOLDERS));
            }
        }
        c.granted += 1;
        c.denials = 0;
        let label = match a.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            // The user's own words. Still not an address: a label typed by
            // mistake from the host field would undo the whole point.
            Some(typed) if !grant::tells_address(typed, &a.server_host) && typed.chars().count() <= 60 => typed.to_string(),
            _ => grant::label(&a.server_name, &a.server_host, c.granted),
        };
        let minutes = grant::minutes(r.ask.minutes, a.minutes, longest_minutes);
        let g = Grant {
            id: tools::new_id(ACCESS_ID_LEN),
            connection: r.connection,
            server: a.server,
            session: a.session,
            label,
            ops,
            folders,
            ftp: a.ftp,
            mode: a.mode,
            until: Until::after(now, minutes),
            generation,
            mask: a.mask,
        };
        r.state = Answered::Approved { access: g.id.clone() };
        self.grants.push(g.clone());
        Ok(g)
    }

    pub fn deny(&mut self, request: &str, note: Option<String>, now: Instant) -> Result<(), String> {
        self.expire(now);
        let Some(r) = self.requests.iter_mut().find(|r| r.id == request) else {
            return Err("[GATEWAY] NO_REQUEST: that request is gone".into());
        };
        if r.state != Answered::Waiting {
            return Err("[GATEWAY] ANSWERED: that request was answered, withdrawn or timed out already".into());
        }
        r.state = Answered::Denied { note };
        if let Some(c) = self.connections.get_mut(&r.connection) {
            c.denials += 1;
            c.asked = Some(now);
        }
        Ok(())
    }

    /// The agent no longer waits for this request's answer.
    pub fn withdraw(&mut self, connection: u64, request: &str) {
        if let Some(r) = self.requests.iter_mut().find(|r| r.id == request && r.connection == connection && r.state == Answered::Waiting) {
            r.state = Answered::Withdrawn;
        }
    }

    /// Marks the requests nobody answered in time. Returns how many.
    pub fn expire(&mut self, now: Instant) -> usize {
        let mut n = 0;
        for r in &mut self.requests {
            if r.state == Answered::Waiting && now.saturating_duration_since(r.made) >= REQUEST_LIFE {
                r.state = Answered::Expired;
                n += 1;
            }
        }
        n
    }

    /// Keeps the newest answered requests of a connection, for
    /// `request_status`, and drops the older ones.
    fn forget_old_answers(&mut self, connection: u64) {
        let answered = self.requests.iter().filter(|r| r.connection == connection && r.state != Answered::Waiting).count();
        let mut excess = answered.saturating_sub(ANSWERED_KEPT);
        self.requests.retain(|r| {
            if excess > 0 && r.connection == connection && r.state != Answered::Waiting {
                excess -= 1;
                return false;
            }
            true
        });
    }

    // --- grants --------------------------------------------------------------

    /// The grant a call named, if it is this connection's and still stands.
    /// One that has ended is taken away here, and the error says why. An id
    /// of another connection gets the same answer as one that never existed.
    pub fn grant_of(&mut self, connection: u64, access: &str, world: World) -> Result<Grant, Gone> {
        let Some(at) = self.grants.iter().position(|g| g.id == access && g.connection == connection) else {
            let told = match self.ended.iter().rev().find(|(id, c, _)| id == access && *c == connection) {
                Some((_, _, why)) => why.told().to_string(),
                None => NOT_YOURS.to_string(),
            };
            return Err(Gone { told, ended: None });
        };
        if let Some(why) = self.grants[at].ended(world) {
            let g = self.grants.remove(at);
            self.remember_ended(&g, why);
            return Err(Gone { told: why.told().to_string(), ended: Some((g, why)) });
        }
        Ok(self.grants[at].clone())
    }

    /// A grant by its id, for the user's side.
    pub fn grant(&self, access: &str) -> Option<&Grant> {
        self.grants.iter().find(|g| g.id == access)
    }

    /// Every waiting request is closed without an answer: the profile was
    /// locked, or agent access turned off. Returns how many.
    pub fn close_requests(&mut self) -> usize {
        let mut n = 0;
        for r in &mut self.requests {
            if r.state == Answered::Waiting {
                r.state = Answered::Closed;
                n += 1;
            }
        }
        n
    }

    /// Ends one grant: the user's Revoke (`connection` is `None`), or the
    /// agent's own release, which reaches only its own.
    pub fn end(&mut self, connection: Option<u64>, access: &str, why: End) -> Option<Grant> {
        let at = self.grants.iter().position(|g| g.id == access && connection.is_none_or(|c| g.connection == c))?;
        let g = self.grants.remove(at);
        self.remember_ended(&g, why);
        Some(g)
    }

    /// Ends every grant `which` picks: a session closed, the profile was
    /// locked, agent access was turned off.
    pub fn end_where(&mut self, which: impl Fn(&Grant) -> bool, why: End) -> Vec<Grant> {
        let (gone, kept): (Vec<Grant>, Vec<Grant>) = std::mem::take(&mut self.grants).into_iter().partition(|g| which(g));
        self.grants = kept;
        for g in &gone {
            self.remember_ended(g, why);
        }
        gone
    }

    /// Ends the grants whose time is up, whose profile is no longer the open
    /// one, or whose session is no longer open. `session_open` says whether a
    /// grant's session is still the one it was made on.
    pub fn sweep(&mut self, now: Now, generation: u64, session_open: impl Fn(&Grant) -> bool) -> Vec<(Grant, End)> {
        let mut gone = Vec::new();
        let mut kept = Vec::new();
        for g in std::mem::take(&mut self.grants) {
            match g.ended(World { now, generation, session_open: session_open(&g) }) {
                Some(why) => gone.push((g, why)),
                None => kept.push(g),
            }
        }
        self.grants = kept;
        for (g, why) in &gone {
            self.remember_ended(g, *why);
        }
        gone
    }

    fn remember_ended(&mut self, g: &Grant, why: End) {
        self.ended.push_back((g.id.clone(), g.connection, why));
        while self.ended.len() > ENDED_KEPT {
            self.ended.pop_front();
        }
    }

    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.grants.iter()
    }

    pub fn grants_of(&self, connection: u64) -> impl Iterator<Item = &Grant> {
        self.grants.iter().filter(move |g| g.connection == connection)
    }

    /// Whether any agent holds a grant on this session.
    #[cfg(test)]
    pub fn on_session(&self, session: &str) -> bool {
        self.grants.iter().any(|g| g.session == session)
    }
}

// --- what an agent is told ---------------------------------------------------

fn mode_told(mode: Mode) -> &'static str {
    match mode {
        Mode::AskEach => "each operation asks the user first",
        Mode::ReadOnlyRuns => "read-only operations run, the others ask the user first",
        Mode::WithinGrant => "operations run without asking, except risky ones, which ask the user first",
    }
}

fn ops_told(g: &Grant) -> String {
    g.ops.iter().map(|o| o.as_str()).collect::<Vec<_>>().join(", ")
}

/// One grant as `list_access` and an approval show it: its folders when it
/// has file operations, and that the server has files only when it is an
/// FTP one. A folder as the server resolved it can hold the server's name
/// (`/var/www/vhosts/shop.example.com`): masked, as file contents are.
pub fn grant_told(g: &Grant, now: Now) -> String {
    let mut out = format!("access: {}\nserver: {}", g.id, g.label);
    if g.ftp {
        out.push_str("\nserver kind: FTP (files only: no commands run there)");
    }
    out.push_str(&format!("\noperations: {}", ops_told(g)));
    if !g.folders.is_empty() {
        out.push_str(&format!("\nfolders: {}", g.folders.iter().map(|f| g.mask.apply(&crate::ai::shape::one_line(f)).into_owned()).collect::<Vec<_>>().join(", ")));
    }
    out.push_str(&format!("\nhow they run: {}\nminutes left: {}", mode_told(g.mode), g.until.minutes_left(now)));
    out
}

/// What `request_access` and `request_status` answer for a request, and
/// whether that is a failure for the agent.
pub fn request_told(r: &Request, grant: Option<&Grant>, now: Now) -> (String, bool) {
    match (&r.state, grant) {
        (Answered::Waiting, _) => (
            format!("The user hasn't answered yet. request: {}\nCheck it with request_status. Don't ask again while it waits; you can tell the user that a request is waiting in Submarine.", r.id),
            false,
        ),
        (Answered::Approved { .. }, Some(g)) => (format!("The user approved.\n{}\nPass the access id as `access` to the tools that work on the server. The user may have given less than you asked for.", grant_told(g, now)), false),
        (Answered::Approved { .. }, None) => ("The user approved this request, and that access has ended since. Ask again with request_access if you still need it.".into(), true),
        (Answered::Denied { note: Some(n) }, _) => (format!("the user denied this: {n}"), true),
        (Answered::Denied { note: None }, _) => ("the user denied this. Don't try another way: ask the user what they want".into(), true),
        (Answered::Expired, _) => ("the user didn't answer in time, and the request is gone. Ask the user whether they want this before asking again".into(), true),
        (Answered::Withdrawn, _) => ("this request was withdrawn".into(), true),
        (Answered::Closed, _) => ("Submarine was locked, or AI agent access was turned off, before the user answered. Ask again once the user has it open".into(), true),
    }
}

/// What `status` answers.
pub fn status_told(open: bool, grants: &[&Grant], waiting: usize, now: Now) -> String {
    let mut out = String::new();
    out.push_str(if open { "Submarine is open and unlocked." } else { "Submarine is locked: no profile is open. Ask the user to unlock it." });
    if waiting > 0 {
        out.push_str(&format!(" {waiting} of your requests wait for the user's answer."));
    }
    if grants.is_empty() {
        out.push_str(" This connection holds no access. Ask for one with request_access, saying what you need and why.");
    } else {
        out.push_str(&format!(" This connection holds {}:", if grants.len() == 1 { "one access".to_string() } else { format!("{} accesses", grants.len()) }));
        for g in grants {
            out.push_str("\n\n");
            out.push_str(&grant_told(g, now));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn ask(purpose: &str) -> Ask {
        Ask { purpose: purpose.into(), ops: vec![Op::Facts, Op::CommandsRead], paths: Vec::new(), server_hint: None, minutes: None }
    }

    fn approval() -> Approval {
        Approval {
            server: "uuid-1".into(),
            session: "s1".into(),
            server_name: "Staging".into(),
            server_host: "10.0.0.5".into(),
            label: None,
            ops: vec![Op::Facts, Op::CommandsRead],
            folders: Vec::new(),
            ftp: false,
            mode: Mode::AskEach,
            minutes: None,
            mask: Default::default(),
        }
    }

    struct T {
        s: State,
        steady: Instant,
        wall: SystemTime,
    }

    impl T {
        fn new() -> T {
            T { s: State::default(), steady: Instant::now(), wall: SystemTime::now() }
        }
        fn now(&self) -> Now {
            Now { steady: self.steady, wall: self.wall }
        }
        fn pass(&mut self, d: Duration) {
            self.steady += d;
            self.wall += d;
        }
        fn world(&self) -> World {
            World { now: self.now(), generation: 1, session_open: true }
        }
        fn connect(&mut self) -> u64 {
            self.s.connect(Peer::default(), 0).unwrap()
        }
        fn granted(&mut self, c: u64) -> Grant {
            let r = self.s.ask(c, ask(&format!("p{}", self.s.requests.len())), self.steady, 0).unwrap();
            self.s.approve(&r, approval(), self.now(), 1, 60).unwrap()
        }
    }

    #[test]
    fn a_request_waits_until_the_user_answers() {
        let mut t = T::new();
        let c = t.connect();
        let id = t.s.ask(c, ask("read the logs"), t.steady, 100).unwrap();
        assert_eq!(id.len(), REQUEST_ID_LEN);
        assert_eq!(t.s.waiting().count(), 1);
        let (told, failed) = request_told(t.s.request_of(c, &id).unwrap(), None, t.now());
        assert!(!failed && told.contains(&id) && told.contains("request_status"));

        let g = t.s.approve(&id, approval(), t.now(), 1, 60).unwrap();
        assert_eq!((g.connection, g.server.as_str(), g.session.as_str(), g.label.as_str()), (c, "uuid-1", "s1", "Staging"));
        assert_eq!(g.id.len(), ACCESS_ID_LEN);
        assert_eq!(g.until.minutes_left(t.now()), 60);
        assert_eq!(t.s.waiting().count(), 0);
        let r = t.s.request_of(c, &id).unwrap().clone();
        assert_eq!(r.state, Answered::Approved { access: g.id.clone() });
        let (told, failed) = request_told(&r, Some(&g), t.now());
        assert!(!failed && told.contains(&g.id) && told.contains("Staging") && told.contains("facts, commands.read"));
        // Nothing in what the agent is told says where the server is.
        assert!(!told.contains("10.0.0.5") && !told.contains("uuid-1") && !told.contains("s1\n"));
        // Answered once.
        assert!(t.s.approve(&id, approval(), t.now(), 1, 60).unwrap_err().contains("ANSWERED"));
        assert!(t.s.deny(&id, None, t.steady).unwrap_err().contains("ANSWERED"));
        assert_eq!(t.s.grants().count(), 1);
    }

    #[test]
    fn what_the_user_chooses_is_what_is_given() {
        let mut t = T::new();
        let c = t.connect();
        // The agent asked for two operations and 8 hours.
        let id = t.s.ask(c, Ask { minutes: Some(480), ..ask("x") }, t.steady, 0).unwrap();
        // The user gives one operation, another way of running, and their
        // longest grant is an hour.
        let a = Approval { ops: vec![Op::CommandsRead], mode: Mode::ReadOnlyRuns, ..approval() };
        let g = t.s.approve(&id, a, t.now(), 1, 60).unwrap();
        assert_eq!(g.ops.iter().copied().collect::<Vec<_>>(), [Op::CommandsRead]);
        assert_eq!(g.mode, Mode::ReadOnlyRuns);
        assert_eq!(g.until.minutes_left(t.now()), 60);
        // The user may give more than was asked, but only what this version offers.
        let id = t.s.ask(c, ask("y"), t.steady, 0).unwrap();
        let a = Approval { ops: vec![Op::CommandsRun, Op::FilesDownload, Op::Facts], minutes: Some(5), ..approval() };
        let g = t.s.approve(&id, a, t.now(), 1, 60).unwrap();
        assert_eq!(g.ops.iter().copied().collect::<Vec<_>>(), [Op::Facts, Op::CommandsRun]);
        assert_eq!(g.until.minutes_left(t.now()), 5);
        assert!(g.folders.is_empty());
        let id = t.s.ask(c, ask("z"), t.steady, 0).unwrap();
        assert!(t.s.approve(&id, Approval { ops: vec![Op::FilesDownload], ..approval() }, t.now(), 1, 60).unwrap_err().contains("NO_OPERATIONS"));
        assert!(t.s.approve(&id, Approval { ops: vec![], ..approval() }, t.now(), 1, 60).unwrap_err().contains("NO_OPERATIONS"));
        // File operations need folders, resolved ones, not too many.
        assert!(t.s.approve(&id, Approval { ops: vec![Op::FilesRead], ..approval() }, t.now(), 1, 60).unwrap_err().contains("NO_FOLDERS"));
        for bad in ["srv/app", "/srv/../etc", "/srv/app/", "~/site", ""] {
            let a = Approval { ops: vec![Op::FilesRead], folders: vec![bad.into()], ..approval() };
            assert!(t.s.approve(&id, a, t.now(), 1, 60).unwrap_err().contains("BAD_FOLDER"), "{bad}");
        }
        let many = (0..=grant::MAX_FOLDERS).map(|i| format!("/srv/{i}")).collect();
        assert!(t.s.approve(&id, Approval { ops: vec![Op::FilesRead], folders: many, ..approval() }, t.now(), 1, 60).unwrap_err().contains("TOO_MANY_FOLDERS"));
        // The failed approvals left the request waiting.
        assert_eq!(t.s.request_of(c, &id).unwrap().state, Answered::Waiting);
        let a = Approval { ops: vec![Op::FilesRead, Op::FilesEdit], folders: vec!["/srv/app".into(), "/srv/app".into(), "/var/log".into()], ..approval() };
        let g = t.s.approve(&id, a, t.now(), 1, 60).unwrap();
        assert_eq!(g.folders, ["/srv/app", "/var/log"]);
        let told = grant_told(&g, t.now());
        assert!(told.contains("folders: /srv/app, /var/log") && !told.contains("FTP"), "{told}");
        // A folder that holds the server's name is told with it hidden.
        let id = t.s.ask(c, ask("vhost"), t.steady, 0).unwrap();
        let a = Approval {
            ops: vec![Op::FilesRead],
            folders: vec!["/var/www/vhosts/shop.example.com/httpdocs".into()],
            server_host: "shop.example.com".into(),
            mask: crate::gateway::mask::Mask::new(["shop.example.com"]),
            ..approval()
        };
        let g = t.s.approve(&id, a, t.now(), 1, 60).unwrap();
        let told = grant_told(&g, t.now());
        assert!(!told.contains("shop.example.com") && told.contains(&format!("folders: /var/www/vhosts/{}/httpdocs", crate::gateway::mask::HIDDEN)), "{told}");
    }

    #[test]
    fn an_ftp_server_gets_no_command_operations() {
        let mut t = T::new();
        let c = t.connect();
        let id = t.s.ask(c, ask("site"), t.steady, 0).unwrap();
        let a = Approval { ops: vec![Op::Facts, Op::CommandsRead, Op::CommandsRun, Op::FilesBrowse], folders: vec!["/htdocs".into()], ftp: true, ..approval() };
        let g = t.s.approve(&id, a, t.now(), 1, 60).unwrap();
        assert_eq!(g.ops.iter().copied().collect::<Vec<_>>(), [Op::Facts, Op::FilesBrowse]);
        assert!(g.ftp);
        assert!(grant_told(&g, t.now()).contains("server kind: FTP (files only: no commands run there)"));
        // Commands alone are nothing to give on an FTP server.
        let id = t.s.ask(c, ask("cmd"), t.steady, 0).unwrap();
        let a = Approval { ops: vec![Op::CommandsRun], ftp: true, ..approval() };
        assert!(t.s.approve(&id, a, t.now(), 1, 60).unwrap_err().contains("NO_OPERATIONS"));
    }

    #[test]
    fn the_label_never_tells_where_the_server_is() {
        let mut t = T::new();
        let c = t.connect();
        let give = |t: &mut T, name: &str, label: Option<&str>| {
            let id = t.s.ask(c, ask(&format!("{name}{label:?}")), t.steady, 0).unwrap();
            let a = Approval { server_name: name.into(), label: label.map(str::to_string), ..approval() };
            t.s.approve(&id, a, t.now(), 1, 60).unwrap().label
        };
        assert_eq!(give(&mut t, "Staging", None), "Staging");
        assert_eq!(give(&mut t, "10.0.0.5", None), "server-2");
        assert_eq!(give(&mut t, "web.example.com", None), "server-3");
        assert_eq!(give(&mut t, "Staging", Some("  the blue one ")), "the blue one");
        // A label the user typed that is the address after all isn't used.
        assert_eq!(give(&mut t, "Staging", Some("10.0.0.5")), "Staging");
        assert_eq!(give(&mut t, "10.0.0.5", Some("root@10.0.0.5")), "server-6");
        assert_eq!(give(&mut t, "Staging", Some("   ")), "Staging");
    }

    #[test]
    fn a_denial_is_told_and_denials_slow_an_agent_down() {
        let mut t = T::new();
        let c = t.connect();
        let id = t.s.ask(c, ask("a"), t.steady, 0).unwrap();
        t.s.deny(&id, Some("not on production".into()), t.steady).unwrap();
        let (told, failed) = request_told(t.s.request_of(c, &id).unwrap(), None, t.now());
        assert!(failed && told == "the user denied this: not on production");
        // One denial changes nothing.
        let id = t.s.ask(c, ask("b"), t.steady, 0).unwrap();
        t.s.deny(&id, None, t.steady).unwrap();
        assert!(request_told(t.s.request_of(c, &id).unwrap(), None, t.now()).0.contains("denied"));
        // Two in a row: not for a minute.
        assert!(t.s.ask(c, ask("c"), t.steady, 0).unwrap_err().contains("denied your last requests"));
        t.pass(Duration::from_secs(59));
        assert!(t.s.ask(c, ask("c"), t.steady, 0).is_err());
        t.pass(Duration::from_secs(1));
        let id = t.s.ask(c, ask("c"), t.steady, 0).unwrap();
        // That one was taken. The next waits its minute too, until a yes.
        assert!(t.s.ask(c, ask("d"), t.steady, 0).is_err());
        t.s.approve(&id, approval(), t.now(), 1, 60).unwrap();
        assert!(t.s.ask(c, ask("d"), t.steady, 0).is_ok());
        // Another connection was never slowed.
        let other = t.connect();
        assert!(t.s.ask(other, ask("a"), t.steady, 0).is_ok());
    }

    #[test]
    fn an_agent_cant_fill_the_panel() {
        let mut t = T::new();
        let c = t.connect();
        let first = t.s.ask(c, ask("same"), t.steady, 0).unwrap();
        // Saying the same thing again is the same request.
        assert_eq!(t.s.ask(c, ask("same"), t.steady, 0).unwrap(), first);
        assert_eq!(t.s.waiting().count(), 1);
        for i in 1..MAX_WAITING {
            t.s.ask(c, ask(&format!("r{i}")), t.steady, 0).unwrap();
        }
        assert!(t.s.ask(c, ask("one more"), t.steady, 0).unwrap_err().contains("waiting for the user already"));
        assert_eq!(t.s.waiting().count(), MAX_WAITING);
        // Unanswered requests go after a while, and make room.
        t.pass(REQUEST_LIFE);
        assert!(t.s.ask(c, ask("later"), t.steady, 0).is_ok());
        assert_eq!(t.s.waiting().count(), 1);
        let (told, failed) = request_told(t.s.request_of(c, &first).unwrap(), None, t.now());
        assert!(failed && told.contains("didn't answer in time"));
        // A timed-out request can't be approved afterwards.
        assert!(t.s.approve(&first, approval(), t.now(), 1, 60).unwrap_err().contains("ANSWERED"));
    }

    #[test]
    fn only_so_many_connections_and_grants() {
        let mut t = T::new();
        let ids: Vec<u64> = (0..MAX_CONNECTIONS).map(|_| t.connect()).collect();
        assert_eq!(t.s.connect(Peer::default(), 0), Err(TOO_MANY_CONNECTIONS));
        t.s.disconnect(ids[0]);
        let c = t.connect();
        assert!(c > ids[MAX_CONNECTIONS - 1], "a connection's number is never used twice");
        for _ in 0..MAX_GRANTS {
            t.granted(c);
        }
        assert!(t.s.ask(c, ask("one more"), t.steady, 0).unwrap_err().contains("release_access"));
    }

    #[test]
    fn an_agent_reaches_only_its_own() {
        let mut t = T::new();
        let (a, b) = (t.connect(), t.connect());
        let ra = t.s.ask(a, ask("a"), t.steady, 0).unwrap();
        assert!(t.s.request_of(b, &ra).is_none());
        let g = t.s.approve(&ra, approval(), t.now(), 1, 60).unwrap();
        assert_eq!(t.s.grant_of(a, &g.id, t.world()).unwrap().id, g.id);
        // The other connection, with the right id: the same answer as for an
        // id that never existed.
        assert_eq!(t.s.grant_of(b, &g.id, t.world()).unwrap_err().told, NOT_YOURS);
        assert_eq!(t.s.grant_of(b, "0123456789abcdef0123456789abcdef", t.world()).unwrap_err().told, NOT_YOURS);
        // It can't give back, or withdraw, what isn't its own.
        assert!(t.s.end(Some(b), &g.id, End::Released).is_none());
        let rb = t.s.ask(b, ask("b"), t.steady, 0).unwrap();
        t.s.withdraw(a, &rb);
        assert_eq!(t.s.request_of(b, &rb).unwrap().state, Answered::Waiting);
        assert_eq!(t.s.grants_of(b).count(), 0);
        assert_eq!(t.s.grants_of(a).count(), 1);
        assert!(t.s.on_session("s1") && !t.s.on_session("s2"));
    }

    #[test]
    fn each_way_a_grant_ends_is_told() {
        let mut t = T::new();
        let c = t.connect();

        // Its time.
        let g = t.granted(c);
        t.pass(Duration::from_secs(60 * 60));
        // The check that finds it ended hands the grant over, once: whoever
        // asked stops what it was running.
        let gone = t.s.grant_of(c, &g.id, t.world()).unwrap_err();
        assert_eq!(gone.told, End::Time.told());
        assert_eq!(gone.ended.map(|(g, why)| (g.id, why)), Some((g.id.clone(), End::Time)));
        // Told again the same way, though the grant is gone.
        let again = t.s.grant_of(c, &g.id, t.world()).unwrap_err();
        assert_eq!(again.told, End::Time.told());
        assert!(again.ended.is_none());
        assert_eq!(t.s.grants().count(), 0);
        assert!(t.s.grant(&g.id).is_none());

        // A lock.
        let g = t.granted(c);
        assert_eq!(t.s.grant_of(c, &g.id, World { generation: 2, ..t.world() }).unwrap_err().told, End::Locked.told());

        // Its session.
        let g = t.granted(c);
        assert_eq!(t.s.grant_of(c, &g.id, World { session_open: false, ..t.world() }).unwrap_err().told, End::Session.told());

        // The user's Revoke.
        let g = t.granted(c);
        assert_eq!(t.s.end(None, &g.id, End::Revoked).unwrap().id, g.id);
        assert_eq!(t.s.grant_of(c, &g.id, t.world()).unwrap_err().told, End::Revoked.told());

        // Its own release.
        let g = t.granted(c);
        assert!(t.s.end(Some(c), &g.id, End::Released).is_some());
        assert_eq!(t.s.grant_of(c, &g.id, t.world()).unwrap_err().told, End::Released.told());

        // Everything on a session, and everything at once.
        let (g1, g2) = (t.granted(c), t.granted(c));
        assert_eq!(t.s.end_where(|g| g.session == "s1", End::Session).len(), 2);
        assert_eq!(t.s.grant_of(c, &g1.id, t.world()).unwrap_err().told, End::Session.told());
        assert_eq!(t.s.grant_of(c, &g2.id, t.world()).unwrap_err().told, End::Session.told());

        // A sweep finds what ended by itself.
        let (g1, _g2) = (t.granted(c), t.granted(c));
        assert!(t.s.sweep(t.now(), 1, |_| true).is_empty());
        let gone = t.s.sweep(t.now(), 1, |g| g.id != g1.id);
        assert_eq!(gone.len(), 1);
        assert_eq!((gone[0].0.id.as_str(), gone[0].1), (g1.id.as_str(), End::Session));
        assert_eq!(t.s.sweep(t.now(), 2, |_| true).len(), 1);

        // The connection.
        let g = t.granted(c);
        let rid = t.s.ask(c, ask("still waiting"), t.steady, 0).unwrap();
        let gone = t.s.disconnect(c);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].id, g.id);
        assert!(t.s.connection(c).is_none());
        assert_eq!(t.s.waiting().count(), 0);
        // What it asked can't be approved into nothing.
        assert!(t.s.approve(&rid, approval(), t.now(), 1, 60).is_err());
    }

    #[test]
    fn an_approved_request_whose_access_ended_says_so() {
        let mut t = T::new();
        let c = t.connect();
        let id = t.s.ask(c, ask("a"), t.steady, 0).unwrap();
        let g = t.s.approve(&id, approval(), t.now(), 1, 60).unwrap();
        t.s.end(None, &g.id, End::Revoked);
        let (told, failed) = request_told(t.s.request_of(c, &id).unwrap(), None, t.now());
        assert!(failed && told.contains("has ended"));
        t.s.withdraw(c, &id);
        assert_eq!(t.s.request_of(c, &id).unwrap().state, Answered::Approved { access: g.id });
    }

    #[test]
    fn a_lock_closes_what_waits() {
        let mut t = T::new();
        let c = t.connect();
        let waiting = t.s.ask(c, ask("a"), t.steady, 0).unwrap();
        let answered = t.s.ask(c, ask("b"), t.steady, 0).unwrap();
        t.s.deny(&answered, None, t.steady).unwrap();
        assert_eq!(t.s.close_requests(), 1);
        assert_eq!(t.s.waiting().count(), 0);
        let (told, failed) = request_told(t.s.request_of(c, &waiting).unwrap(), None, t.now());
        assert!(failed && told.contains("locked"));
        assert_eq!(t.s.request_of(c, &answered).unwrap().state, Answered::Denied { note: None });
        // What was closed can't be approved after an unlock.
        assert!(t.s.approve(&waiting, approval(), t.now(), 2, 60).unwrap_err().contains("ANSWERED"));
    }

    #[test]
    fn status_says_where_things_stand() {
        let mut t = T::new();
        let c = t.connect();
        assert!(status_told(false, &[], 0, t.now()).contains("locked"));
        let empty = status_told(true, &[], 1, t.now());
        assert!(empty.contains("open and unlocked") && empty.contains("1 of your requests") && empty.contains("request_access"));
        let g = t.granted(c);
        let one = status_told(true, &[&g], 0, t.now());
        assert!(one.contains("one access") && one.contains(&g.id) && one.contains("each operation asks the user first") && one.contains("minutes left: 60"));
        let g2 = t.granted(c);
        assert!(status_told(true, &[&g, &g2], 0, t.now()).contains("2 accesses"));
        assert!(!one.contains("10.0.0.5"));
    }

    #[test]
    fn what_a_client_says_about_itself_is_kept_as_its_claim() {
        let mut t = T::new();
        let c = t.connect();
        assert!(t.s.connection(c).unwrap().client.is_none());
        t.s.said(c, Client { name: "claude-code".into(), version: "2.1".into() });
        assert_eq!(t.s.connection(c).unwrap().client.as_ref().unwrap().name, "claude-code");
        t.s.said(99, Client { name: "nobody".into(), version: String::new() });
        assert_eq!(t.s.connections().count(), 1);
    }
}
