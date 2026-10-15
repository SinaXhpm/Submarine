//! RDP servers (issue #3), first step: Submarine opens the system's RDP app.
//!
//! An RDP server is a saved node with `protocol = 'rdp'`. Opened directly, the
//! RDP app connects to its host and port. Opened "through an SSH server" (the
//! node's `jump_host_id`), Submarine logs in to that SSH server the way it
//! reaches a jump host, listens on a loopback port, and forwards every
//! connection the RDP app makes there to the RDP server over a `direct-tcpip`
//! channel: what `ssh -L` does, for this one tab.
//!
//! The RDP app asks for the password itself; nothing secret is handed to it.
//! A built-in client (IronRDP) comes later and replaces the launcher.

use crate::ssh_manager::ClientHandler;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// The port an RDP server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 3389;

/// More connections than this at once through one tab's forward is not an
/// RDP app (which opens one or two); the rest are closed.
const MAX_FORWARDED_CONNS: usize = 16;

/// One RDP tab's state on this side: the SSH route and its listener (when it
/// goes through an SSH server) and the `.rdp` file handed to the RDP app.
pub struct RdpSession {
    /// The route's SSH connections, the one the forward rides last. Only
    /// held: dropping the session drops them, which closes them.
    _route: Arc<Vec<russh::client::Handle<ClientHandler>>>,
    /// The forwarding listener's task.
    accept: Option<tokio::task::JoinHandle<()>>,
    /// What the RDP app connects to: `host:port` or the forward's address.
    pub address: String,
    pub username: String,
    /// The options it was opened with; "Open again" may bring newer ones.
    pub options: RdpOptions,
    /// The file the RDP app was given, deleted with the session.
    pub file: Option<PathBuf>,
    /// RDP windows open from this tab that it can follow: "closed" is said
    /// once the last of them is.
    pub windows: Arc<std::sync::atomic::AtomicUsize>,
}

impl RdpSession {
    pub fn direct(address: String, username: String, options: RdpOptions) -> Self {
        Self { _route: Arc::new(Vec::new()), accept: None, address, username, options, file: None, windows: Arc::default() }
    }
}

impl Drop for RdpSession {
    fn drop(&mut self) {
        if let Some(task) = self.accept.take() {
            task.abort();
        }
        if let Some(file) = self.file.take() {
            let _ = std::fs::remove_file(file);
        }
    }
}

pub type RdpSessions = Arc<Mutex<HashMap<String, RdpSession>>>;

/// A host name or address an RDP app may be given: letters, digits, `.`, `-`,
/// `_`, or an IPv6 address. Everything else (spaces, control characters, a
/// `:` that would read as a port, a leading `-` that would read as an
/// option) is refused: the value goes into a `.rdp` file line and onto a
/// command line.
pub fn check_host(host: &str) -> Result<(), String> {
    let host = host.trim();
    let ipv6 = host.parse::<std::net::Ipv6Addr>().is_ok();
    let plain = !host.is_empty()
        && !host.starts_with('-')
        && host.len() <= 253
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ipv6 || plain {
        Ok(())
    } else {
        Err(format!("{host:?} isn't a host name or address an RDP app can be given"))
    }
}

/// A user name for the `.rdp` file: any text without control characters
/// (a line break would start a new setting), at most 256 characters. It may
/// name a domain: `DOMAIN\user` or `user@domain`.
pub fn check_user(user: &str) -> Result<(), String> {
    if user.chars().count() > 256 || user.chars().any(char::is_control) {
        Err("the user name has a line break or another control character".into())
    } else {
        Ok(())
    }
}

/// `host:port` as an RDP app reads it, an IPv6 address in brackets.
pub fn address(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// How the RDP window looks and what it shares with the server. It's this
/// device's choice for a server, kept in its own settings and never synced:
/// a window size belongs to a screen, and in a shared profile someone else
/// mustn't be able to switch on sharing this computer's files. The tab
/// passes it in.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RdpOptions {
    pub full_screen: bool,
    /// The window's size when not full screen.
    pub width: u32,
    pub height: u32,
    pub clipboard: bool,
    /// This computer's drives (with Remmina: the home folder).
    pub drives: bool,
}

impl Default for RdpOptions {
    /// What Remote Desktop Connection does on its own, files kept private.
    fn default() -> Self {
        Self { full_screen: true, width: 1600, height: 900, clipboard: true, drives: false }
    }
}

impl RdpOptions {
    /// A window size RDP servers take: 640×480 up to 8K.
    pub fn checked(mut self) -> Self {
        self.width = self.width.clamp(640, 7680);
        self.height = self.height.clamp(480, 4320);
        self
    }
}

/// The `.rdp` file for `address` (already checked): where to connect, who
/// logs in, and the options. No password is written.
pub fn rdp_file(address: &str, user: &str, options: &RdpOptions) -> String {
    let mut out = format!("full address:s:{address}\r\n");
    let user = user.trim();
    if !user.is_empty() {
        out.push_str(&format!("username:s:{user}\r\n"));
    }
    if options.full_screen {
        out.push_str("screen mode id:i:2\r\n");
    } else {
        out.push_str(&format!("screen mode id:i:1\r\ndesktopwidth:i:{}\r\ndesktopheight:i:{}\r\n", options.width, options.height));
    }
    out.push_str(&format!("redirectclipboard:i:{}\r\n", u8::from(options.clipboard)));
    out.push_str(if options.drives { "drivestoredirect:s:*\r\n" } else { "drivestoredirect:s:\r\n" });
    out
}

/// The same as a Remmina profile, which (unlike its `.rdp` import) carries
/// full screen and a shared folder. `home` is the folder shared for
/// `drives`. Values are checked already; a line break can't reach here.
pub fn remmina_file(address: &str, user: &str, options: &RdpOptions, home: Option<&Path>) -> String {
    let clean = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let mut out = format!("[remmina]\nname={address}\nprotocol=RDP\nserver={address}\n");
    let user = user.trim();
    if !user.is_empty() {
        out.push_str(&format!("username={user}\n"));
    }
    // Remmina 1.4: viewmode 4 is full screen, 1 a window; resolution_mode 2
    // takes the window's (there: the screen's) size, 1 the one given.
    if options.full_screen {
        out.push_str("viewmode=4\nresolution_mode=2\n");
    } else {
        out.push_str(&format!("viewmode=1\nresolution_mode=1\nresolution_width={}\nresolution_height={}\n", options.width, options.height));
    }
    out.push_str(&format!("disableclipboard={}\n", u8::from(!options.clipboard)));
    let share = match (options.drives, home) {
        (true, Some(home)) => clean(&home.to_string_lossy()),
        _ => String::new(),
    };
    out.push_str(&format!("sharefolder={share}\n"));
    out
}

/// FreeRDP's command line for the same.
pub fn freerdp_args(address: &str, user: &str, options: &RdpOptions) -> Vec<String> {
    let mut args = vec![format!("/v:{address}")];
    if !user.trim().is_empty() {
        args.push(format!("/u:{}", user.trim()));
    }
    args.push(if options.full_screen { "+f".into() } else { format!("/size:{}x{}", options.width, options.height) });
    args.push(if options.clipboard { "/clipboard".into() } else { "-clipboard".into() });
    if options.drives {
        args.push("+drives".into());
    }
    args
}

/// The loopback address a node's forward listens on. RDP apps remember
/// certificates and saved logins by host name, without the port: had every
/// tunnelled server been `127.0.0.1`, a login saved for one would be offered
/// to the next. Each node gets its own `127.x.y.z`, the same every time.
/// macOS answers only `127.0.0.1` unless more are configured, so it stays
/// there.
pub fn loopback_for(node_key: &str) -> IpAddr {
    if cfg!(target_os = "macos") {
        return IpAddr::V4(Ipv4Addr::LOCALHOST);
    }
    // FNV-1a: stable across builds and platforms (std's hasher isn't).
    let mut hash: u32 = 0x811c_9dc5;
    for b in node_key.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let [_, x, y, z] = hash.to_be_bytes();
    // Not 127.0.0.0, 127.0.0.1 (shared with everything else) or .255.
    let z = z.clamp(2, 254);
    IpAddr::V4(Ipv4Addr::new(127, x, y, z))
}

/// Listens on `ip` (a free port) and forwards each connection to
/// `host:port` through the last of `hops`. Returns the address it listens on
/// and the accepting task. `closed` runs once when the SSH route is gone.
pub async fn forward_through(
    hops: Vec<russh::client::Handle<ClientHandler>>,
    ip: IpAddr,
    host: String,
    port: u16,
    closed: impl FnOnce() + Send + 'static,
) -> Result<(SocketAddr, RdpSessionParts), String> {
    if hops.is_empty() {
        return Err("no SSH server to go through".into());
    }
    let listener = match tokio::net::TcpListener::bind((ip, 0)).await {
        Ok(l) => l,
        // A loopback address other than 127.0.0.1 that this system won't
        // bind: fall back to the usual one.
        Err(_) if ip != IpAddr::V4(Ipv4Addr::LOCALHOST) => tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| format!("couldn't listen on a local port: {e}"))?,
        Err(e) => return Err(format!("couldn't listen on a local port: {e}")),
    };
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    let hops = Arc::new(hops);
    let route = Arc::clone(&hops);
    let accept = tokio::spawn(async move {
        let limit = Arc::new(tokio::sync::Semaphore::new(MAX_FORWARDED_CONNS));
        let mut watch = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = watch.tick() => {
                    if route.iter().any(|h| h.is_closed()) {
                        closed();
                        return;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((sock, peer)) = accepted else { continue };
                    let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else { continue };
                    let route = Arc::clone(&route);
                    let host = host.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        crate::tunnel::apply_tunnel_sockopts(&sock);
                        let Some(last) = route.last() else { return };
                        let opened = last
                            .channel_open_direct_tcpip(host.clone(), u32::from(port), peer.ip().to_string(), u32::from(peer.port()))
                            .await;
                        match opened {
                            Ok(channel) => {
                                let mut sock = sock;
                                let mut stream = crate::tunnel::drained_stream(channel);
                                crate::tunnel::pump_channel(&mut sock, &mut stream).await;
                            }
                            Err(e) => println!("[RDP] couldn't open a channel to {host}:{port}: {e}"),
                        }
                    });
                }
            }
        }
    });
    Ok((local, RdpSessionParts { hops, accept }))
}

/// What `forward_through` hands to the session.
pub struct RdpSessionParts {
    hops: Arc<Vec<russh::client::Handle<ClientHandler>>>,
    accept: tokio::task::JoinHandle<()>,
}

impl RdpSession {
    pub fn forwarded(parts: RdpSessionParts, address: String, username: String, options: RdpOptions) -> Self {
        Self { _route: parts.hops, accept: Some(parts.accept), address, username, options, file: None, windows: Arc::default() }
    }
}

fn files_dir() -> PathBuf {
    std::env::temp_dir().join("submarine-rdp")
}

/// Writes the session's `.rdp` (or `.remmina`) file into this user's temp
/// folder (readable by this user only on Unix) and returns its path. The
/// name carries this process's id: another copy of Submarine (a portable
/// one) numbers its tabs the same way.
pub fn write_file(session_id: &str, ext: &str, contents: &str) -> Result<PathBuf, String> {
    let dir = files_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make the folder for the .rdp file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let safe: String = session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let path = dir.join(format!("{}-{safe}.{ext}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut f = options.open(&path).map_err(|e| format!("couldn't write the .rdp file: {e}"))?;
    let bytes = if ext == "rdp" { file_bytes(contents) } else { contents.as_bytes().to_vec() };
    f.write_all(&bytes).map_err(|e| format!("couldn't write the .rdp file: {e}"))?;
    Ok(path)
}

/// Removes `.rdp` files older than a day: what a copy of Submarine that
/// ended without closing its tabs left behind. A live tab's file is newer,
/// and its RDP app read it when it started anyway.
pub fn clear_old_files() {
    let Ok(entries) = std::fs::read_dir(files_dir()) else { return };
    let day = std::time::Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > day);
        if old && path.extension().is_some_and(|e| e == "rdp" || e == "remmina") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The file's bytes. On Windows UTF-16LE with a byte-order mark, as Remote
/// Desktop Connection writes them itself (it reads a user name like `Jörg`
/// wrongly from UTF-8); UTF-8 elsewhere.
fn file_bytes(contents: &str) -> Vec<u8> {
    if cfg!(target_os = "windows") {
        let mut out = vec![0xFF, 0xFE];
        for unit in contents.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    } else {
        contents.as_bytes().to_vec()
    }
}

/// The RDP apps Submarine can start.
#[derive(Debug, PartialEq)]
pub enum App {
    /// Windows' Remote Desktop Connection.
    Mstsc(PathBuf),
    /// macOS: whatever opens `.rdp` files (Windows App).
    MacOpen,
    Remmina(PathBuf),
    FreeRdp(PathBuf),
}

/// The RDP app this system has. Linux and the other Unixes: Remmina first
/// (it asks for the password in a window), else FreeRDP's SDL client (a
/// window too), else its X11 client. `find` looks a program up on PATH.
pub fn pick_app(find: impl Fn(&str) -> Option<PathBuf>) -> Result<App, String> {
    if cfg!(target_os = "windows") {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        return Ok(App::Mstsc(root.join("System32").join("mstsc.exe")));
    }
    if cfg!(target_os = "macos") {
        return Ok(App::MacOpen);
    }
    if cfg!(target_os = "android") {
        return Err("RDP isn't available on Android yet: it comes with the built-in RDP client".into());
    }
    if let Some(program) = find("remmina") {
        return Ok(App::Remmina(program));
    }
    ["sdl-freerdp3", "sdl-freerdp", "xfreerdp3", "xfreerdp"]
        .into_iter()
        .find_map(&find)
        .map(App::FreeRdp)
        .ok_or_else(|| "no RDP app was found: install Remmina (with its RDP plugin) or FreeRDP".into())
}

/// An RDP app, ready to start.
#[derive(Debug, PartialEq)]
pub struct Launcher {
    /// What the tab calls it.
    pub name: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Whether the process lives as long as the RDP window does. A program
    /// that hands the file to an app already running (`open`, Remmina) exits
    /// at once, so its end says nothing.
    pub tracked: bool,
    /// The file written for it, removed with the tab.
    pub file: Option<PathBuf>,
}

/// Writes what `app` needs for `address` (checked), as `user`, with
/// `options` (a `.rdp` file, a Remmina profile, or nothing for FreeRDP),
/// and says how to start it.
pub fn prepare(app: App, session_id: &str, address: &str, user: &str, options: &RdpOptions) -> Result<Launcher, String> {
    let path_arg = |p: &PathBuf| p.to_string_lossy().into_owned();
    Ok(match app {
        App::Mstsc(program) => {
            let file = write_file(session_id, "rdp", &rdp_file(address, user, options))?;
            Launcher { name: "Remote Desktop Connection", program, args: vec![path_arg(&file)], tracked: true, file: Some(file) }
        }
        App::MacOpen => {
            let file = write_file(session_id, "rdp", &rdp_file(address, user, options))?;
            Launcher { name: "the app that opens .rdp files", program: PathBuf::from("/usr/bin/open"), args: vec![path_arg(&file)], tracked: false, file: Some(file) }
        }
        App::Remmina(program) => {
            let home = crate::ssh_config::home_dir().map(PathBuf::from);
            let file = write_file(session_id, "remmina", &remmina_file(address, user, options, home.as_deref()))?;
            Launcher { name: "Remmina", program, args: vec!["-c".into(), path_arg(&file)], tracked: false, file: Some(file) }
        }
        App::FreeRdp(program) => Launcher { name: "FreeRDP", program, args: freerdp_args(address, user, options), tracked: true, file: None },
    })
}

/// What an RDP app wrote before it failed, for the tab: its error lines (its
/// last lines when none is one), at most `n`, each without FreeRDP's
/// `[time] [pid:thread] [LEVEL][module] - [function]: ` prefix.
pub fn last_words(stderr: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let errors: Vec<&str> = lines.iter().copied().filter(|l| l.contains("[ERROR]")).collect();
    let pick = if errors.is_empty() { &lines } else { &errors };
    pick[pick.len().saturating_sub(n)..]
        .iter()
        .map(|l| {
            let said = match l.find("]: ") {
                Some(i) if l.starts_with('[') => &l[i + 3..],
                _ => l,
            };
            said.chars().take(300).collect()
        })
        .collect()
}

/// A program on PATH, as a shell would find it.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_an_rdp_app_can_be_given() {
        for ok in ["server", "rdp.example.com", "10.0.0.5", "my_host-2", "::1", "fe80::1", "2001:db8::7"] {
            assert!(check_host(ok).is_ok(), "{ok}");
        }
        for bad in ["", " ", "-cmd", "host name", "host\r\nalternate shell:s:x", "host:3389", "a;b", "[::1]", "h\u{0}"] {
            assert!(check_host(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn user_names_never_add_a_line_to_the_file() {
        for ok in ["", "alice", r"CORP\alice", "alice@corp.example", "Ünïcode name"] {
            assert!(check_user(ok).is_ok(), "{ok}");
        }
        for bad in ["alice\r\nalternate shell:s:cmd", "a\tb", "a\u{7f}"] {
            assert!(check_user(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_file_names_the_address_user_and_options_only() {
        let full = RdpOptions::default();
        assert_eq!(
            rdp_file("127.0.0.5:51234", r"CORP\alice", &full),
            concat!(
                "full address:s:127.0.0.5:51234\r\n",
                "username:s:CORP\\alice\r\n",
                "screen mode id:i:2\r\n",
                "redirectclipboard:i:1\r\n",
                "drivestoredirect:s:\r\n",
            )
        );
        let window = RdpOptions { full_screen: false, width: 1280, height: 720, clipboard: false, drives: true };
        assert_eq!(
            rdp_file("[::1]:3389", "  ", &window),
            concat!(
                "full address:s:[::1]:3389\r\n",
                "screen mode id:i:1\r\n",
                "desktopwidth:i:1280\r\n",
                "desktopheight:i:720\r\n",
                "redirectclipboard:i:0\r\n",
                "drivestoredirect:s:*\r\n",
            )
        );
        assert_eq!(address("fe80::1", 3390), "[fe80::1]:3390");
        assert_eq!(address("server", 3389), "server:3389");
    }

    #[test]
    fn options_from_the_tab_are_kept_in_bounds() {
        let o: RdpOptions = serde_json::from_str(r#"{"fullScreen":false,"width":99999,"height":10}"#).unwrap();
        assert_eq!((o.clone().checked().width, o.checked().height), (7680, 480));
        let partial: RdpOptions = serde_json::from_str(r#"{"drives":true}"#).unwrap();
        assert_eq!(partial, RdpOptions { drives: true, ..RdpOptions::default() }, "missing fields take the defaults");
        assert!(!RdpOptions::default().drives, "files are never shared unless asked");
    }

    #[test]
    fn remmina_and_freerdp_get_the_same_options() {
        let window = RdpOptions { full_screen: false, width: 1024, height: 700, clipboard: false, drives: true };
        assert_eq!(
            remmina_file("127.0.0.5:5000", "alice", &window, Some(Path::new("/home/alice"))),
            concat!(
                "[remmina]\n",
                "name=127.0.0.5:5000\n",
                "protocol=RDP\n",
                "server=127.0.0.5:5000\n",
                "username=alice\n",
                "viewmode=1\n",
                "resolution_mode=1\n",
                "resolution_width=1024\n",
                "resolution_height=700\n",
                "disableclipboard=1\n",
                "sharefolder=/home/alice\n",
            )
        );
        let full = RdpOptions::default();
        assert_eq!(
            remmina_file("h:1", "", &full, Some(Path::new("/home/alice"))),
            concat!(
                "[remmina]\n",
                "name=h:1\n",
                "protocol=RDP\n",
                "server=h:1\n",
                "viewmode=4\n",
                "resolution_mode=2\n",
                "disableclipboard=0\n",
                "sharefolder=\n",
            ),
            "no folder shared unless asked"
        );
        assert_eq!(freerdp_args("h:1", r"CORP\alice", &full), vec!["/v:h:1", r"/u:CORP\alice", "+f", "/clipboard"]);
        assert_eq!(freerdp_args("h:1", " ", &window), vec!["/v:h:1", "/size:1024x700", "-clipboard", "+drives"], "no /u: without a user");
    }

    #[test]
    fn each_node_gets_its_own_loopback_address() {
        let a = loopback_for("8a3c-uuid-one");
        let b = loopback_for("8a3c-uuid-two");
        assert_eq!(a, loopback_for("8a3c-uuid-one"), "the same every time");
        if cfg!(target_os = "macos") {
            assert_eq!(a, IpAddr::V4(Ipv4Addr::LOCALHOST));
            return;
        }
        assert_ne!(a, b);
        for ip in [a, b, loopback_for(""), loopback_for("x")] {
            let IpAddr::V4(v4) = ip else { panic!("IPv4") };
            assert!(v4.is_loopback(), "{v4}");
            assert!(v4 != Ipv4Addr::LOCALHOST && v4.octets()[3] != 255 && v4.octets()[3] != 0, "{v4}");
        }
    }

    #[test]
    fn the_rdp_app_for_this_system() {
        let none = |_: &str| None;
        if cfg!(target_os = "windows") {
            assert!(matches!(pick_app(none), Ok(App::Mstsc(p)) if p.ends_with("mstsc.exe")));
            return;
        }
        if cfg!(target_os = "macos") {
            assert_eq!(pick_app(none), Ok(App::MacOpen));
            return;
        }
        assert!(pick_app(none).is_err(), "none installed");
        let only = |want: &'static str| move |p: &str| (p == want).then(|| PathBuf::from(format!("/usr/bin/{p}")));
        assert_eq!(pick_app(only("remmina")), Ok(App::Remmina("/usr/bin/remmina".into())));
        assert_eq!(pick_app(only("xfreerdp3")), Ok(App::FreeRdp("/usr/bin/xfreerdp3".into())));
        // Remmina first when both are there; FreeRDP's SDL client before its X11 one.
        let both = |p: &str| matches!(p, "remmina" | "xfreerdp3").then(|| PathBuf::from(p));
        assert_eq!(pick_app(both), Ok(App::Remmina("remmina".into())));
        let freerdps = |p: &str| matches!(p, "sdl-freerdp3" | "xfreerdp3").then(|| PathBuf::from(p));
        assert_eq!(pick_app(freerdps), Ok(App::FreeRdp("sdl-freerdp3".into())));
        // FreeRDP needs no file; Remmina gets a profile, tracked or not.
        let f = prepare(App::FreeRdp("x".into()), "session-rdp-prep", "h:1", "u", &RdpOptions::default()).unwrap();
        assert_eq!((f.name, f.tracked, f.file), ("FreeRDP", true, None));
        let r = prepare(App::Remmina("remmina".into()), "session-rdp-prep", "h:1", "u", &RdpOptions::default()).unwrap();
        let file = r.file.clone().unwrap();
        assert_eq!((r.name, r.tracked, r.args[0].as_str()), ("Remmina", false, "-c"));
        assert!(file.to_string_lossy().ends_with(".remmina") && std::fs::read_to_string(&file).unwrap().starts_with("[remmina]\n"));
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn an_rdp_apps_last_words_are_its_errors() {
        let freerdp = "[23:15:10:158] [316197:0004d325] [WARN][com.freerdp.client.common.cmdline] - [freerdp_client_warn_experimental]: Developers hang out in https://matrix.to/\n\
            [23:15:16:907] [316197:0004d325] [ERROR][com.freerdp.utils.signal] - [term_handler]: Caught signal 'Terminated' [15]\n\
            [23:15:16:907] [316197:0004d325] [ERROR][com.freerdp.core] - [freerdp_abort_connect_context]: ERRCONNECT_CONNECT_CANCELLED [0x0002000B]\n\
            fuse: writing device: Bad file descriptor\n";
        assert_eq!(
            last_words(freerdp, 3),
            vec!["Caught signal 'Terminated' [15]".to_string(), "ERRCONNECT_CONNECT_CANCELLED [0x0002000B]".to_string()]
        );
        assert_eq!(last_words("one\n\ntwo\nthree\n", 2), vec!["two".to_string(), "three".to_string()], "no errors: the last lines");
        assert!(last_words("", 3).is_empty());
        assert_eq!(last_words(&"x".repeat(1000), 1)[0].len(), 300);
    }

    #[tokio::test]
    async fn the_file_is_private_and_removed_with_the_session() {
        let path = write_file("session-rdp-test::x", "rdp", "full address:s:h:1\r\nusername:s:Jörg\r\n").unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), format!("{}-session-rdp-test__x.rdp", std::process::id()));
        let bytes = std::fs::read(&path).unwrap();
        let text = if cfg!(target_os = "windows") {
            assert_eq!(&bytes[..2], &[0xFF, 0xFE], "UTF-16LE with a BOM");
            let units: Vec<u16> = bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16(&units).unwrap()
        } else {
            String::from_utf8(bytes).unwrap()
        };
        assert_eq!(text, "full address:s:h:1\r\nusername:s:Jörg\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let mut s = RdpSession::direct("h:1".into(), String::new(), RdpOptions::default());
        s.file = Some(path.clone());
        drop(s);
        assert!(!path.exists());
    }
}
