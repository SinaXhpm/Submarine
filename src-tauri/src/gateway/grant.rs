//! What the user can give an agent, and when it ends. A grant is one agent
//! connection's permission to do some kinds of operation on one server for a
//! while. It lives in memory only: nothing here is ever written to disk.
//!
//! Everything in this file is plain logic, with the clocks passed in, so each
//! rule can be tested by itself.

use crate::ai::gate::Level;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

/// A grant's time when the agent names none.
pub const DEFAULT_MINUTES: u32 = 60;
/// The longest a grant can last, whatever is asked or set.
pub const MAX_MINUTES: u32 = 8 * 60;
/// What a masked address becomes, and the default label's stem.
pub const LABEL_STEM: &str = "server";
/// The most folders one grant's file operations can name.
pub const MAX_FOLDERS: usize = 8;

/// One kind of operation a grant can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = "facts")]
    Facts,
    #[serde(rename = "commands.read")]
    CommandsRead,
    #[serde(rename = "commands.run")]
    CommandsRun,
    #[serde(rename = "files.browse")]
    FilesBrowse,
    #[serde(rename = "files.read")]
    FilesRead,
    #[serde(rename = "files.write")]
    FilesWrite,
    #[serde(rename = "files.edit")]
    FilesEdit,
    #[serde(rename = "files.delete")]
    FilesDelete,
    #[serde(rename = "files.download")]
    FilesDownload,
    #[serde(rename = "files.upload")]
    FilesUpload,
}

impl Op {
    pub const ALL: [Op; 10] = [
        Op::Facts,
        Op::CommandsRead,
        Op::CommandsRun,
        Op::FilesBrowse,
        Op::FilesRead,
        Op::FilesWrite,
        Op::FilesEdit,
        Op::FilesDelete,
        Op::FilesDownload,
        Op::FilesUpload,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Facts => "facts",
            Op::CommandsRead => "commands.read",
            Op::CommandsRun => "commands.run",
            Op::FilesBrowse => "files.browse",
            Op::FilesRead => "files.read",
            Op::FilesWrite => "files.write",
            Op::FilesEdit => "files.edit",
            Op::FilesDelete => "files.delete",
            Op::FilesDownload => "files.download",
            Op::FilesUpload => "files.upload",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|o| o.as_str() == s)
    }

    /// It works on files, so its grant needs folders.
    pub fn files(self) -> bool {
        !matches!(self, Op::Facts | Op::CommandsRead | Op::CommandsRun)
    }

    /// It can change or remove something on the server. On the request card
    /// such an operation starts unchecked, asked for or not.
    #[cfg(test)]
    pub fn changes(self) -> bool {
        matches!(self, Op::CommandsRun | Op::FilesWrite | Op::FilesEdit | Op::FilesDelete | Op::FilesUpload)
    }
}

/// How the operations of a grant run. The user picks it for each grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every operation waits for the user.
    AskEach,
    /// What is proven read-only runs; the rest waits for the user.
    ReadOnlyRuns,
    /// What the grant covers runs, except what always asks.
    WithinGrant,
}

impl Mode {
    /// The assistant's permission level with the same rules. Full access is
    /// never one of them: no grant can reach it.
    pub fn level(self) -> Level {
        match self {
            Mode::AskEach => Level::AskAll,
            Mode::ReadOnlyRuns => Level::Ask,
            Mode::WithinGrant => Level::Auto,
        }
    }
}

/// Both clocks at one moment. A grant's time is measured on both, because
/// each can be wrong alone: the steady one may stand still while the computer
/// sleeps, and the wall clock can be set back.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub steady: Instant,
    pub wall: SystemTime,
}

impl Now {
    pub fn read() -> Now {
        Now { steady: Instant::now(), wall: SystemTime::now() }
    }
}

/// When a grant's time is up: at the first of the two clocks to get there.
#[derive(Clone, Copy, Debug)]
pub struct Until {
    steady: Instant,
    wall: SystemTime,
}

impl Until {
    pub fn after(now: Now, minutes: u32) -> Until {
        let d = Duration::from_secs(u64::from(minutes.clamp(1, MAX_MINUTES)) * 60);
        Until { steady: now.steady + d, wall: now.wall + d }
    }

    pub fn passed(&self, now: Now) -> bool {
        now.steady >= self.steady || now.wall >= self.wall
    }

    /// Whole minutes left, rounded up; 0 once it has passed.
    pub fn minutes_left(&self, now: Now) -> u32 {
        if self.passed(now) {
            return 0;
        }
        let steady = self.steady.saturating_duration_since(now.steady);
        let wall = self.wall.duration_since(now.wall).unwrap_or(Duration::ZERO);
        let secs = steady.min(wall).as_secs();
        u32::try_from(secs.div_ceil(60)).unwrap_or(u32::MAX).max(1)
    }
}

/// The minutes a grant gets: what the user set on the card if anything, else
/// what the agent asked for, else the default; never more than the user's
/// longest, and never more than `MAX_MINUTES`.
pub fn minutes(asked: Option<u32>, chosen: Option<u32>, longest: u32) -> u32 {
    let longest = longest.clamp(1, MAX_MINUTES);
    chosen.or(asked).unwrap_or(DEFAULT_MINUTES).clamp(1, longest)
}

/// Why a grant ended. Told to the agent, shown to the user and logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// Its time was up.
    Time,
    /// The profile was locked or switched.
    Locked,
    /// The server's session closed, or it isn't the same one any more.
    Session,
    /// The agent disconnected.
    Disconnected,
    /// The user took it back.
    Revoked,
    /// The agent gave it back.
    Released,
    /// Agent access was turned off, or the app is closing.
    Off,
}

impl End {
    /// What a call gets that names a grant which ended this way.
    pub fn told(self) -> &'static str {
        match self {
            End::Time => "this access has ended: its time was up. Ask again with request_access if you still need it",
            End::Locked => "this access has ended: Submarine was locked. Ask the user to unlock it, then ask again with request_access",
            End::Session => "this access has ended: the server's session was closed. Ask again with request_access",
            End::Disconnected => "this access has ended: its connection closed. Ask again with request_access",
            End::Revoked => "this access has ended: the user took it back. Don't ask again unless the user tells you to",
            End::Released => "this access has ended: it was given back with release_access",
            End::Off => "this access has ended: AI agent access was turned off in Submarine",
        }
    }
}

/// One grant.
#[derive(Clone, Debug)]
pub struct Grant {
    /// Random, 128 bits. It means something only on its own connection.
    pub id: String,
    pub connection: u64,
    /// The server's uuid in the profile.
    pub server: String,
    /// The open session the operations go through.
    pub session: String,
    /// The name the agent sees for the server.
    pub label: String,
    pub ops: BTreeSet<Op>,
    /// The folders file operations may touch: absolute, resolved on the
    /// server when the grant was made.
    pub folders: Vec<String>,
    /// The server is an FTP one: files only, nothing runs commands there.
    pub ftp: bool,
    pub mode: Mode,
    pub until: Until,
    /// The profile generation the grant was made in.
    pub generation: u64,
    /// The server's address in the forms Submarine knows: hidden in all an
    /// agent is told about this grant, its folders and its errors included.
    pub mask: super::mask::Mask,
}

/// What the rest of the app knows at the moment a grant is checked.
#[derive(Clone, Copy, Debug)]
pub struct World {
    pub now: Now,
    /// The open profile's generation; it changes on every lock and switch.
    pub generation: u64,
    /// The grant's session is open and still the one it was made on.
    pub session_open: bool,
}

impl Grant {
    /// `None` while the grant stands. Checked at every call, and again after
    /// every wait for the user.
    pub fn ended(&self, world: World) -> Option<End> {
        if world.generation != self.generation {
            return Some(End::Locked);
        }
        if !world.session_open {
            return Some(End::Session);
        }
        if self.until.passed(world.now) {
            return Some(End::Time);
        }
        None
    }

    pub fn covers(&self, op: Op) -> bool {
        self.ops.contains(&op)
    }

    /// A path a file operation named, after the server resolved it, lies in
    /// one of the granted folders.
    pub fn holds(&self, resolved: &str) -> bool {
        inside(&self.folders, resolved)
    }
}

/// What a call gets when its grant lacks the operation.
pub fn not_covered(op: Op) -> String {
    format!("this access doesn't cover {} ({}); ask for it with request_access", plain(op), op.as_str())
}

/// An operation in a few plain words, for sentences and for the card.
pub fn plain(op: Op) -> &'static str {
    match op {
        Op::Facts => "reading the server's basic facts",
        Op::CommandsRead => "running read-only commands",
        Op::CommandsRun => "running commands that can change things",
        Op::FilesBrowse => "listing folders",
        Op::FilesRead => "reading files",
        Op::FilesWrite => "creating files",
        Op::FilesEdit => "changing files",
        Op::FilesDelete => "deleting files",
        Op::FilesDownload => "copying files to this computer",
        Op::FilesUpload => "copying files to the server",
    }
}

/// `resolved` is one of `folders` or lies under one. Both sides are absolute
/// paths as the server's realpath gives them: no `.`, `..` or empty part. A
/// path that isn't in that form is inside nothing.
pub fn inside(folders: &[String], resolved: &str) -> bool {
    if !canonical(resolved) {
        return false;
    }
    folders.iter().any(|f| {
        if !canonical(f) {
            return false;
        }
        f == "/" || resolved == f || resolved.strip_prefix(f.as_str()).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// An absolute path with no `.`, `..` or empty part and no slash at its end
/// (`/` alone is one).
pub fn canonical(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    match path.strip_prefix('/') {
        Some(rest) => !path.contains('\0') && rest.split('/').all(|part| !part.is_empty() && part != "." && part != ".."),
        None => false,
    }
}

/// The name an agent sees for a server: the name the user gave it in
/// Submarine, unless that name gives away where the server is. Then it is
/// `server-1`, `server-2`, …: `number` counts this connection's grants. The
/// user can change it on the card.
pub fn label(name: &str, host: &str, number: usize) -> String {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 || tells_address(name, host) {
        return format!("{LABEL_STEM}-{number}");
    }
    name.to_string()
}

/// The name holds the server's host as saved, an IP address, or something
/// written like a host name (`db1.example.com`, `root@web`).
pub fn tells_address(name: &str, host: &str) -> bool {
    let lower = name.to_lowercase();
    let host = host.trim().to_lowercase();
    if !host.is_empty() && lower.contains(&host) {
        return true;
    }
    if lower.contains('@') {
        return true;
    }
    // A word is what an address could be written with; anything else parts
    // two words.
    lower.split(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | ':' | '-' | '_' | '%'))).any(|word| {
        let word = word.trim_matches(|c: char| matches!(c, '.' | ':' | '-' | '_' | '%'));
        if word.is_empty() {
            return false;
        }
        // An IPv6 address may name its interface after a `%`.
        let bare = word.split_once('%').map_or(word, |(address, _)| address);
        if bare.parse::<IpAddr>().is_ok() {
            return true;
        }
        // An address with a port.
        if let Some((a, port)) = word.rsplit_once(':') {
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && (a.parse::<IpAddr>().is_ok() || host_like(a)) {
                return true;
            }
        }
        host_like(word)
    })
}

/// Written like a host name with a domain: labels of letters, digits and
/// hyphens with dots between them, the last one letters only. A version
/// number (`v2.1`) or a file name with digits at the end isn't one; a word
/// like `notes.txt` is, and a name holding it gets the plain label, which
/// costs nothing.
fn host_like(word: &str) -> bool {
    let labels: Vec<&str> = word.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let ok = |l: &&str| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-');
    if !labels.iter().all(ok) {
        return false;
    }
    let last = labels[labels.len() - 1];
    last.len() >= 2 && last.bytes().all(|b| b.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(steady: Instant, wall: SystemTime) -> Now {
        Now { steady, wall }
    }

    fn grant(now: Now) -> Grant {
        Grant {
            id: "0123456789abcdef0123456789abcdef".into(),
            connection: 1,
            server: "uuid-1".into(),
            session: "s1".into(),
            label: "staging".into(),
            ops: BTreeSet::from([Op::Facts, Op::CommandsRead, Op::FilesRead]),
            folders: vec!["/srv/app".into(), "/var/log".into()],
            ftp: false,
            mode: Mode::AskEach,
            until: Until::after(now, 60),
            generation: 7,
            mask: Default::default(),
        }
    }

    #[test]
    fn every_operation_has_one_name_both_ways() {
        let mut seen = BTreeSet::new();
        for op in Op::ALL {
            assert_eq!(Op::parse(op.as_str()), Some(op));
            assert_eq!(serde_json::to_value(op).unwrap(), serde_json::json!(op.as_str()));
            assert_eq!(serde_json::from_value::<Op>(serde_json::json!(op.as_str())).unwrap(), op);
            assert!(seen.insert(op.as_str()));
            assert!(!plain(op).is_empty());
        }
        assert_eq!(Op::parse("files.chmod"), None);
        assert_eq!(Op::parse("Facts"), None);
        assert_eq!(Op::parse(""), None);
    }

    #[test]
    fn what_changes_things_is_known() {
        let changing: Vec<&str> = Op::ALL.into_iter().filter(|o| o.changes()).map(Op::as_str).collect();
        assert_eq!(changing, ["commands.run", "files.write", "files.edit", "files.delete", "files.upload"]);
        let files: Vec<&str> = Op::ALL.into_iter().filter(|o| o.files()).map(Op::as_str).collect();
        assert_eq!(files.len(), 7);
        assert!(files.iter().all(|f| f.starts_with("files.")));
    }

    #[test]
    fn no_way_of_running_reaches_full_access() {
        assert_eq!(Mode::AskEach.level(), Level::AskAll);
        assert_eq!(Mode::ReadOnlyRuns.level(), Level::Ask);
        assert_eq!(Mode::WithinGrant.level(), Level::Auto);
        for m in [Mode::AskEach, Mode::ReadOnlyRuns, Mode::WithinGrant] {
            assert!(!matches!(m.level(), Level::Full | Level::Suggest));
        }
    }

    #[test]
    fn a_grants_time_ends_on_whichever_clock_gets_there_first() {
        let (s0, w0) = (Instant::now(), SystemTime::now());
        let until = Until::after(at(s0, w0), 60);
        let min = |m: u64| Duration::from_secs(m * 60);
        assert!(!until.passed(at(s0, w0)));
        assert!(!until.passed(at(s0 + min(59), w0 + min(59))));
        assert!(until.passed(at(s0 + min(60), w0 + min(60))));
        // The computer slept for a day: the steady clock stood still, the
        // wall clock didn't.
        assert!(until.passed(at(s0 + min(5), w0 + min(24 * 60))));
        // The wall clock was set back a year: the steady clock still counts.
        assert!(until.passed(at(s0 + min(61), w0 - min(365 * 24 * 60))));
        assert!(!until.passed(at(s0 + min(30), w0 - min(365 * 24 * 60))));

        assert_eq!(until.minutes_left(at(s0, w0)), 60);
        assert_eq!(until.minutes_left(at(s0 + Duration::from_secs(1), w0 + Duration::from_secs(1))), 60);
        assert_eq!(until.minutes_left(at(s0 + min(59), w0 + min(59))), 1);
        assert_eq!(until.minutes_left(at(s0 + min(10), w0 + min(50))), 10);
        assert_eq!(until.minutes_left(at(s0 + min(60), w0)), 0);
    }

    #[test]
    fn the_minutes_are_the_users_to_set_within_the_longest() {
        assert_eq!(minutes(None, None, 60), 60);
        assert_eq!(minutes(Some(15), None, 60), 15);
        assert_eq!(minutes(Some(600), None, 60), 60);
        assert_eq!(minutes(Some(600), None, 4000), MAX_MINUTES);
        assert_eq!(minutes(Some(15), Some(30), 60), 30);
        assert_eq!(minutes(Some(15), Some(0), 60), 1);
        assert_eq!(minutes(None, None, 15), 15);
        assert_eq!(minutes(None, None, 0), 1);
        // However it is asked, a grant never outlasts the cap.
        let now = Now::read();
        assert_eq!(Until::after(now, u32::MAX).minutes_left(now), MAX_MINUTES);
        assert_eq!(Until::after(now, 0).minutes_left(now), 1);
    }

    #[test]
    fn a_grant_ends_with_its_profile_its_session_or_its_time() {
        let now = Now::read();
        let g = grant(now);
        let world = World { now, generation: 7, session_open: true };
        assert_eq!(g.ended(world), None);
        assert_eq!(g.ended(World { generation: 8, ..world }), Some(End::Locked));
        assert_eq!(g.ended(World { session_open: false, ..world }), Some(End::Session));
        let later = at(now.steady + Duration::from_secs(3600), now.wall + Duration::from_secs(3600));
        assert_eq!(g.ended(World { now: later, ..world }), Some(End::Time));
        // A lock is the reason given when several hold.
        assert_eq!(g.ended(World { now: later, generation: 9, session_open: false }), Some(End::Locked));
        for e in [End::Time, End::Locked, End::Session, End::Disconnected, End::Revoked, End::Released, End::Off] {
            assert!(e.told().starts_with("this access has ended"));
        }
    }

    #[test]
    fn a_grant_covers_only_what_it_holds() {
        let g = grant(Now::read());
        assert!(g.covers(Op::CommandsRead) && g.covers(Op::Facts) && g.covers(Op::FilesRead));
        for op in [Op::CommandsRun, Op::FilesWrite, Op::FilesEdit, Op::FilesDelete, Op::FilesBrowse, Op::FilesUpload, Op::FilesDownload] {
            assert!(!g.covers(op));
            assert!(not_covered(op).contains(op.as_str()));
        }
    }

    #[test]
    fn a_path_is_inside_a_folder_or_it_isnt() {
        let g = grant(Now::read());
        for yes in ["/srv/app", "/srv/app/config.yml", "/srv/app/a/b/c", "/var/log/syslog", "/srv/app/..hidden", "/srv/app/a..b"] {
            assert!(g.holds(yes), "{yes}");
        }
        for no in [
            "/srv",
            "/srv/application",
            "/srv/app2/x",
            "/srv/app.bak",
            "/var/logs",
            "/",
            "/etc/passwd",
            // Not as realpath gives it: inside nothing.
            "/srv/app/",
            "/srv/app/../../etc/passwd",
            "/srv/app/./x",
            "/srv//app/x",
            "srv/app/x",
            "",
            "~/x",
            "/srv/app/x\0y",
        ] {
            assert!(!g.holds(no), "{no:?}");
        }
        // The root covers everything; a folder that isn't in form covers nothing.
        assert!(inside(&["/".into()], "/etc/passwd"));
        assert!(inside(&["/".into()], "/"));
        assert!(!inside(&["/srv/app/".into()], "/srv/app/x"));
        assert!(!inside(&["/srv/../etc".into()], "/etc/passwd"));
        assert!(!inside(&["srv".into()], "/srv/x"));
        assert!(!inside(&[], "/srv/app"));
    }

    #[test]
    fn a_name_that_tells_where_the_server_is_isnt_shown() {
        // A host that is in none of the names below.
        const ELSEWHERE: &str = "203.0.113.9";
        for (name, host) in [
            ("10.0.0.5", "10.0.0.5"),
            ("prod 10.0.0.5", "example.org"),
            ("web (192.168.1.20:2222)", ELSEWHERE),
            ("db1.example.com", ELSEWHERE),
            ("Staging db1.example.com!", ELSEWHERE),
            ("root@web", ELSEWHERE),
            ("2001:db8::1", ELSEWHERE),
            ("[2001:db8::1]:22", ELSEWHERE),
            ("fe80::1%eth0", ELSEWHERE),
            ("my box", "box"),
            ("STAGING", "staging"),
            ("edge.internal:8022", ELSEWHERE),
            ("", ELSEWHERE),
        ] {
            assert_eq!(label(name, host, 3), "server-3", "{name:?}");
        }
        for (name, host) in [
            ("Staging", "10.0.0.5"),
            ("web 2", "web2.example.com"),
            ("Build box v2.1", ELSEWHERE),
            ("db (primary)", ELSEWHERE),
            ("نمونه", ELSEWHERE),
            ("backup-2026", ELSEWHERE),
            ("Mr. Smith's box", ELSEWHERE),
            ("rack 12:30", ELSEWHERE),
        ] {
            assert_eq!(label(name, host, 3), name, "{name:?}");
        }
        assert_eq!(label(&"x".repeat(61), "h", 1), "server-1");
        assert_eq!(label("  Staging  ", "h", 1), "Staging");
    }
}
