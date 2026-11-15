//! Where agents connect: a named pipe on Windows, a Unix socket on macOS and
//! Linux. There is no network port. Only programs of the same user can open
//! it, and the system says which program did.
//!
//! The name is made from the user and from this program's own file, so two
//! copies of Submarine (an installed one and a portable one, or a test copy)
//! never answer for each other, and the bridge, which is the same file,
//! finds its own app without being told anything.

use sha2::{Digest, Sha256};
use std::path::Path;

/// One process, as the system describes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Proc {
    pub pid: u32,
    /// The full path of its program file. Empty when the system won't say.
    pub program: String,
}

/// Who opened a connection: that process first, then the one that started
/// it, and so on up. For display and for the log, never for a decision: a
/// program of the same user can arrange to look like another.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Peer {
    pub chain: Vec<Proc>,
}

/// How far up the chain of starting processes is followed.
const MAX_CHAIN: usize = 4;

/// Why the bridge couldn't reach the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unreachable {
    /// Nothing listens: Submarine isn't running, or agent access is off.
    Absent,
    /// Every place is taken.
    Busy,
    /// Something answers under Submarine's name that isn't this program.
    NotOurs,
    Other(String),
}

/// The part of the endpoint's name that makes it this user's and this
/// copy's: 32 hex digits.
pub fn id_for(user: &str, program: &Path) -> String {
    let mut key = program.to_string_lossy().into_owned();
    if cfg!(windows) {
        // File names there don't tell upper from lower case.
        key = key.to_lowercase();
    }
    let mut h = Sha256::new();
    h.update(user.as_bytes());
    h.update(b"\n");
    h.update(key.as_bytes());
    let digest: [u8; 32] = h.finalize().into();
    hex::encode(&digest[..16])
}

/// This program's own file, with links and junctions resolved: the app and
/// its bridge get the same answer however each was started.
pub fn own_program() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("[GATEWAY] OWN_PATH: {e}"))?;
    std::fs::canonicalize(&exe).map_err(|e| format!("[GATEWAY] OWN_PATH: {e}"))
}

/// The endpoint of this user and this copy: a pipe name, or a socket path.
pub fn endpoint() -> Result<String, String> {
    let id = id_for(&imp::user()?, &own_program()?);
    imp::named(&id)
}

pub use imp::{connect, ClientStream, Listener};

#[cfg(windows)]
mod imp {
    use super::{Peer, Proc, Unreachable, MAX_CHAIN};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, FILETIME, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

    pub type ClientStream = NamedPipeClient;

    const BUFFER: u32 = 64 * 1024;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_PIPE_BUSY: i32 = 231;

    fn win(what: &str, e: windows::core::Error) -> String {
        format!("[GATEWAY] {what}: {e}")
    }

    /// A handle that is closed when it goes out of scope.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // Safety: the handle is ours and is closed once, here.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    pub fn named(id: &str) -> Result<String, String> {
        Ok(format!(r"\\.\pipe\submarine-agent-{id}"))
    }

    /// The SID of the user this process runs as, in its written form
    /// (`S-1-5-21-…`).
    pub fn user() -> Result<String, String> {
        // Safety: each call gets buffers of the size it is told; the token
        // handle is closed by `Owned`; the string Windows allocates is
        // copied and then freed with LocalFree, as its documentation asks.
        unsafe {
            let mut token = HANDLE(std::ptr::null_mut());
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|e| win("TOKEN", e))?;
            let token = Owned(token);
            let mut len = 0u32;
            // The first call only says how much room the answer needs.
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
            if len == 0 || len > 4096 {
                return Err("[GATEWAY] TOKEN: Windows didn't say who this user is".into());
            }
            // Whole u64s, so the structure read from it is aligned.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast::<c_void>()), len, &mut len).map_err(|e| win("TOKEN", e))?;
            let info = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(info.User.Sid, &mut text).map_err(|e| win("SID", e))?;
            let sid = text.to_string();
            let _ = LocalFree(Some(HLOCAL(text.0.cast::<c_void>())));
            let sid = sid.map_err(|e| format!("[GATEWAY] SID: {e}"))?;
            if !sid.starts_with("S-") || !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err("[GATEWAY] SID: not in the form Windows writes them".into());
            }
            Ok(sid)
        }
    }

    /// A security descriptor that lets one user in and nobody else. Kept for
    /// as long as pipes are made with it.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    // Safety: the descriptor is a block of memory that is only read after it
    // is made, and freed once, in Drop.
    unsafe impl Send for Descriptor {}
    unsafe impl Sync for Descriptor {}

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // Safety: allocated by Windows for us with LocalAlloc.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }

    fn descriptor(sid: &str) -> Result<Descriptor, String> {
        // D: an access list. P: protected, nothing is inherited into it. One
        // entry: allow (A) every access (GA) to this user. Whoever isn't
        // named gets nothing: other users, the network logon, Everyone.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})").encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        // Safety: `sddl` ends with a NUL and outlives the call.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None) }.map_err(|e| win("DESCRIPTOR", e))?;
        Ok(Descriptor(sd))
    }

    /// One instance of the pipe, waiting for a client. `first`: fail if the
    /// name exists already.
    fn instance(name: &str, sd: &Descriptor, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut attrs = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0 .0, bInheritHandle: false.into() };
        let mut options = ServerOptions::new();
        // Clients from other machines are refused whatever the access list
        // says.
        options.first_pipe_instance(first).reject_remote_clients(true).in_buffer_size(BUFFER).out_buffer_size(BUFFER);
        // Safety: `attrs` is a whole SECURITY_ATTRIBUTES that lives through
        // the call, and the descriptor it points at lives in `sd`.
        unsafe { options.create_with_security_attributes_raw(name, (&mut attrs as *mut SECURITY_ATTRIBUTES).cast::<c_void>()) }
    }

    pub struct Listener {
        name: String,
        sd: Descriptor,
        next: NamedPipeServer,
    }

    impl Listener {
        /// Starts listening. Fails when the name exists already: then
        /// another program made it, or another running copy of this one, and
        /// we don't join either.
        pub fn open(name: &str) -> Result<Listener, String> {
            let sd = descriptor(&user()?)?;
            let next = instance(name, &sd, true).map_err(|e| match e.raw_os_error() {
                Some(ERROR_ACCESS_DENIED) => "[GATEWAY] NAME_TAKEN: another program already answers on Submarine's agent pipe, or another copy of Submarine is running".to_string(),
                _ => format!("[GATEWAY] LISTEN: {e}"),
            })?;
            Ok(Listener { name: name.to_string(), sd, next })
        }

        /// The next client, and who it is. The pipe never stops existing in
        /// between: the instance for the client after this one is made
        /// before this one is handed over.
        pub async fn accept(&mut self) -> std::io::Result<(NamedPipeServer, Peer)> {
            self.next.connect().await?;
            let fresh = instance(&self.name, &self.sd, false)?;
            let connected = std::mem::replace(&mut self.next, fresh);
            let mut pid = 0u32;
            // Safety: the handle is the connected pipe's, alive in `connected`.
            let known = unsafe { GetNamedPipeClientProcessId(HANDLE(connected.as_raw_handle()), &mut pid) }.is_ok() && pid != 0;
            let chain = if known { ancestry(pid) } else { Vec::new() };
            Ok((connected, Peer { chain }))
        }
    }

    /// The bridge's side: opens the app's pipe, and checks that what answers
    /// is this same program.
    pub async fn connect(name: &str) -> Result<NamedPipeClient, Unreachable> {
        let mut tries = 0;
        let pipe = loop {
            match ClientOptions::new().open(name) {
                Ok(p) => break p,
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Err(Unreachable::Absent),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && tries < 40 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => return Err(Unreachable::Busy),
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => return Err(Unreachable::NotOurs),
                Err(e) => return Err(Unreachable::Other(e.to_string())),
            }
        };
        let mut pid = 0u32;
        // Safety: the handle is the open pipe's, alive in `pipe`.
        let known = unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }.is_ok() && pid != 0;
        let ours = known && described(pid).is_some_and(|(program, _)| same_as_own(&program));
        if !ours {
            return Err(Unreachable::NotOurs);
        }
        Ok(pipe)
    }

    fn same_as_own(program: &str) -> bool {
        match (std::fs::canonicalize(program), super::own_program()) {
            (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
            _ => false,
        }
    }

    /// Every process's parent, from one snapshot of the process list.
    fn parents() -> HashMap<u32, u32> {
        let mut map = HashMap::new();
        // Safety: the entry has its size set as the API asks, and the
        // snapshot handle is closed by `Owned`.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
            let snapshot = Owned(snapshot);
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut more = Process32FirstW(snapshot.0, &mut e).is_ok();
            while more {
                map.insert(e.th32ProcessID, e.th32ParentProcessID);
                more = Process32NextW(snapshot.0, &mut e).is_ok();
            }
        }
        map
    }

    /// A process's program file and when it started (in Windows' own count),
    /// or `None` when Windows won't open it for us.
    fn described(pid: u32) -> Option<(String, u64)> {
        // Safety: the buffer's length is passed with it; the handle is closed
        // by `Owned`.
        unsafe {
            let h = Owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
            let mut buf = vec![0u16; 32 * 1024];
            let mut len = buf.len() as u32;
            let program = match QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) {
                Ok(()) => String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]),
                Err(_) => String::new(),
            };
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
            Some((program, (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)))
        }
    }

    /// The process and the ones that started it. A parent's number is only a
    /// number: when that process is gone, another may have it. One that
    /// started after its "child" isn't the parent, and the chain ends there.
    fn ancestry(pid: u32) -> Vec<Proc> {
        let parents = parents();
        let mut chain = Vec::new();
        let (mut current, mut child_started) = (pid, None::<u64>);
        while chain.len() < MAX_CHAIN {
            let Some((program, started)) = described(current) else { break };
            if child_started.is_some_and(|c| started > c) {
                break;
            }
            chain.push(Proc { pid: current, program });
            child_started = Some(started);
            match parents.get(&current) {
                Some(&parent) if parent != 0 && parent != current => current = parent,
                _ => break,
            }
        }
        chain
    }
}

#[cfg(unix)]
mod imp {
    use super::{Peer, Proc, Unreachable, MAX_CHAIN};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use tokio::net::{UnixListener, UnixStream};

    pub type ClientStream = UnixStream;

    /// Nothing of the user goes into the name here: the socket's folder is
    /// already one user's alone.
    pub fn user() -> Result<String, String> {
        Ok(String::new())
    }

    /// The socket's path: in the user's runtime folder where the system has
    /// one (Linux), else in the temporary folder (per user on macOS), in a
    /// folder of our own that only this user can enter. Kept short: a socket
    /// path has room for about a hundred bytes.
    pub fn named(id: &str) -> Result<String, String> {
        let short = &id[..id.len().min(16)];
        let dir = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_absolute()) {
            Some(runtime) => runtime.join("submarine"),
            None => {
                // In a temporary folder that may be shared, the folder's name
                // is this user's: made from the home folder's path.
                let home = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
                let tag = super::id_for(&home, Path::new("agent-socket-folder"));
                std::env::temp_dir().join(format!("submarine-{}", &tag[..8]))
            }
        };
        let path = dir.join(format!("{short}.sock"));
        let text = path.to_str().ok_or("[GATEWAY] SOCKET_PATH: not a path that can be written as text")?.to_string();
        if text.len() > 100 {
            return Err("[GATEWAY] SOCKET_PATH: too long for a socket".into());
        }
        Ok(text)
    }

    /// The folder is a real folder that nobody but its owner can enter. We
    /// could make a socket in it (or reach one), so its owner is this user
    /// (or this is root): the owner's id is ours.
    fn our_folder(dir: &Path) -> Result<u32, String> {
        let d = std::fs::symlink_metadata(dir).map_err(|e| format!("[GATEWAY] SOCKET_FOLDER: {e}"))?;
        if !d.file_type().is_dir() || d.mode() & 0o077 != 0 {
            return Err("[GATEWAY] SOCKET_FOLDER: the folder for the agent socket isn't this user's alone".into());
        }
        Ok(d.uid())
    }

    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
        me: u32,
    }

    impl Listener {
        pub fn open(name: &str) -> Result<Listener, String> {
            let path = PathBuf::from(name);
            let dir = path.parent().ok_or("[GATEWAY] SOCKET_PATH: no folder")?;
            match std::fs::create_dir(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("[GATEWAY] SOCKET_FOLDER: {e}")),
            }
            // Ours to tighten if it is ours; if it isn't, this fails or
            // changes nothing, and the check below refuses the folder.
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            let me = our_folder(dir)?;
            // A socket file left by a copy that didn't close cleanly is
            // removed. One that answers means another copy is running.
            if std::fs::symlink_metadata(&path).is_ok() {
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return Err("[GATEWAY] NAME_TAKEN: another copy of Submarine already listens for agents".into());
                }
                let _ = std::fs::remove_file(&path);
            }
            let listener = UnixListener::bind(&path).map_err(|e| format!("[GATEWAY] LISTEN: {e}"))?;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            Ok(Listener { listener, path, me })
        }

        /// The next client of this user, and who it is. A connection from
        /// another user is closed without a word.
        pub async fn accept(&mut self) -> std::io::Result<(UnixStream, Peer)> {
            loop {
                let (stream, _) = self.listener.accept().await?;
                let Ok(cred) = stream.peer_cred() else { continue };
                if cred.uid() != self.me {
                    continue;
                }
                let chain = match cred.pid().and_then(|p| u32::try_from(p).ok()) {
                    Some(pid) => ancestry(pid),
                    None => Vec::new(),
                };
                return Ok((stream, Peer { chain }));
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// The bridge's side: reaches the app's socket, and checks that what
    /// answers runs as this user.
    pub async fn connect(name: &str) -> Result<UnixStream, Unreachable> {
        let path = Path::new(name);
        let Some(dir) = path.parent() else { return Err(Unreachable::Absent) };
        if std::fs::symlink_metadata(dir).is_err() {
            return Err(Unreachable::Absent);
        }
        let Ok(me) = our_folder(dir) else { return Err(Unreachable::NotOurs) };
        let stream = match UnixStream::connect(path).await {
            Ok(s) => s,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Err(Unreachable::Absent),
            Err(e) => return Err(Unreachable::Other(e.to_string())),
        };
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == me => Ok(stream),
            _ => Err(Unreachable::NotOurs),
        }
    }

    /// The process and the ones that started it, from `/proc`. Where there
    /// is no `/proc` (macOS) the chain is the process alone, without its
    /// program file.
    fn ancestry(pid: u32) -> Vec<Proc> {
        let mut chain = Vec::new();
        let (mut current, mut child_started) = (pid, None::<u64>);
        while chain.len() < MAX_CHAIN {
            let Some((parent, started)) = stat_of(current) else {
                if chain.is_empty() {
                    chain.push(Proc { pid: current, program: String::new() });
                }
                break;
            };
            // A number can be given to a new process once its old one is
            // gone: one that started after its "child" isn't the parent.
            if child_started.is_some_and(|c| started > c) {
                break;
            }
            let program = std::fs::read_link(format!("/proc/{current}/exe")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            chain.push(Proc { pid: current, program });
            child_started = Some(started);
            if parent <= 1 || parent == current {
                break;
            }
            current = parent;
        }
        chain
    }

    /// A process's parent and start time, from `/proc/<pid>/stat`. The
    /// program's name in it may hold blanks and brackets, so the fields are
    /// counted from the last `)`.
    fn stat_of(pid: u32) -> Option<(u32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat(&stat)
    }

    pub(super) fn parse_stat(stat: &str) -> Option<(u32, u64)> {
        let rest = &stat[stat.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
        // After the name: state, parent, …; the start time is the 20th.
        let parent = fields.get(1)?.parse().ok()?;
        let started = fields.get(19)?.parse().ok()?;
        Some((parent, started))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_this_users_and_this_copys() {
        let a = id_for("S-1-5-21-1", Path::new("C:/Apps/Submarine/submarine.exe"));
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a, id_for("S-1-5-21-1", Path::new("C:/Apps/Submarine/submarine.exe")));
        assert_ne!(a, id_for("S-1-5-21-2", Path::new("C:/Apps/Submarine/submarine.exe")));
        assert_ne!(a, id_for("S-1-5-21-1", Path::new("D:/portable/submarine.exe")));
        // The user and the path can't be shifted into each other.
        assert_ne!(id_for("ab", Path::new("c")), id_for("a", Path::new("bc")));
        if cfg!(windows) {
            assert_eq!(a, id_for("S-1-5-21-1", Path::new("c:/apps/SUBMARINE/Submarine.EXE")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_stat_line_is_read_past_a_tricky_name() {
        let plain = "1234 (bash) S 1000 1234 1234 34816 5678 4194304 1 2 3 4 5 6 7 8 20 0 1 0 987654 1 2";
        assert_eq!(imp::parse_stat(plain), Some((1000, 987654)));
        let tricky = "1234 (a) b) (c) R 77 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0 0";
        assert_eq!(imp::parse_stat(tricky), Some((77, 55)));
        assert_eq!(imp::parse_stat("1234 (bash) S"), None);
        assert_eq!(imp::parse_stat("nothing here"), None);
    }

    #[cfg(windows)]
    mod windows_only {
        use super::super::*;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        fn test_name(tag: &str) -> String {
            format!(r"\\.\pipe\submarine-agent-test-{}-{tag}", std::process::id())
        }

        #[test]
        fn this_user_has_a_name_windows_can_write() {
            let endpoint = endpoint().unwrap();
            assert!(endpoint.starts_with(r"\\.\pipe\submarine-agent-"));
            assert_eq!(endpoint, super::super::endpoint().unwrap());
        }

        #[tokio::test]
        async fn nothing_listening_is_told_apart_from_something_else() {
            assert_eq!(connect(&test_name("absent")).await.err(), Some(Unreachable::Absent));
        }

        #[tokio::test]
        async fn a_pipe_is_made_once_and_says_who_connected() {
            let name = test_name("one");
            let mut listener = Listener::open(&name).unwrap();
            // The name exists now: a second listener doesn't join it.
            assert!(Listener::open(&name).err().unwrap().contains("NAME_TAKEN"));

            let client_name = name.clone();
            let client = tokio::spawn(async move {
                // The test binary is both ends, so the server is "this program".
                let mut pipe = connect(&client_name).await.unwrap();
                pipe.write_all(b"hello\n").await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut pipe).read_line(&mut line).await.unwrap();
                line
            });
            let (server, peer) = listener.accept().await.unwrap();
            assert_eq!(peer.chain[0].pid, std::process::id());
            let own = own_program().unwrap().to_string_lossy().to_lowercase();
            let seen = std::fs::canonicalize(&peer.chain[0].program).unwrap().to_string_lossy().to_lowercase();
            assert_eq!(seen, own);
            // Whoever started the tests is next, when Windows tells.
            assert!(peer.chain.len() <= 4);
            let mut server = BufReader::new(server);
            let mut line = String::new();
            server.read_line(&mut line).await.unwrap();
            assert_eq!(line, "hello\n");
            server.get_mut().write_all(b"welcome\n").await.unwrap();
            assert_eq!(client.await.unwrap(), "welcome\n");

            // A second client is served while the first is still open.
            let again = tokio::spawn({
                let name = name.clone();
                async move { connect(&name).await.is_ok() }
            });
            let (_second, _) = listener.accept().await.unwrap();
            assert!(again.await.unwrap());
        }
    }
}
