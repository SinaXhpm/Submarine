//! Files shared with an RDP server by the built-in client (rdp_client.rs):
//! one folder or a whole drive, read-only or not, as this device's option
//! for the server says (never synced: in a shared profile nobody else may
//! switch on sharing this computer's files). The server sees it as
//! `\\tsclient\<name>` (RDPDR drive redirection).
//!
//! On Windows IronRDP's own backend serves it: it checks every path the
//! server sends and opens each part relative to the one before without
//! following a link, so nothing outside the shared folder or drive can be
//! reached. Its copy in vendor/ takes a folder as the root, not only a drive
//! (VENDORED.md there).
//!
//! On macOS and Linux IronRDP's backend joins the server's path to the
//! folder as text, so here every path is checked first (`inside`): names
//! only, no `..`, and no link on the way. A read-only share refuses every
//! change there; on Windows the backend does.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The drive's device id on the RDPDR channel (0 is the smart card's).
const DEVICE_ID: u32 = 1;

/// The name the server is told this computer has: it shows the share as
/// "<name> on Submarine".
const COMPUTER_NAME: &str = "Submarine";

/// What to share, as the tab sends it (this device's option for the server).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    pub kind: ShareKind,
    /// The folder; for a drive its root (`D:\` on Windows; elsewhere the
    /// folder it is mounted on).
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    Folder,
    Drive,
}

/// The name the server shows the share by (`\\tsclient\<name>`): the
/// folder's own name, or the drive's letter. Letters, digits, `-` and `_`
/// only, at most 32; "Shared" when nothing is left.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn share_name(share: &Share) -> String {
    let path = share.path.trim();
    let bytes = path.as_bytes();
    let raw = if share.kind == ShareKind::Drive && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && path.len() <= 3 {
        path[..1].to_ascii_uppercase()
    } else {
        let trimmed = path.trim_end_matches(['\\', '/']);
        trimmed.rsplit(['\\', '/']).next().unwrap_or_default().to_string()
    };
    let name: String = raw.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(32).collect();
    if name.is_empty() {
        "Shared".into()
    } else {
        name
    }
}

/// The folder to share, checked once before the session: an absolute path
/// to a folder that's there, its links resolved (what the server may reach
/// is decided here). On Windows a drive is `X:\` of a drive that's there.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn root(share: &Share) -> Result<PathBuf, String> {
    let given = share.path.trim();
    if given.is_empty() {
        return Err("no folder was chosen to share".into());
    }
    #[cfg(windows)]
    if share.kind == ShareKind::Drive {
        let b = given.as_bytes();
        let letter = (b.len() == 2 || (b.len() == 3 && b[2] == b'\\')) && b[0].is_ascii_alphabetic() && b[1] == b':';
        let root = format!("{}:\\", given.get(..1).unwrap_or("").to_ascii_uppercase());
        if !letter || !Path::new(&root).is_dir() {
            return Err(format!("{given} isn't a drive on this computer"));
        }
        return Ok(PathBuf::from(root));
    }
    let path = Path::new(given);
    if !path.is_absolute() {
        return Err(format!("{given} isn't a full path"));
    }
    let real = std::fs::canonicalize(path).map_err(|_| format!("{given} isn't there"))?;
    if !real.is_dir() {
        return Err(format!("{given} isn't a folder"));
    }
    // IronRDP's backend for macOS and Linux takes the folder as text.
    #[cfg(not(windows))]
    if real.to_str().is_none() {
        return Err(format!("{given}: the folder's path isn't text this client can share"));
    }
    // The Windows backend takes `C:\…`, not the `\\?\C:\…` resolving gives, and a
    // folder on one of this computer's drives only.
    #[cfg(windows)]
    let real = {
        let shown = real.to_string_lossy().into_owned();
        let plain = shown.strip_prefix(r"\\?\").unwrap_or(&shown).to_string();
        if plain.starts_with(r"UNC\") || plain.starts_with(r"\\") {
            return Err(format!("{given} is on a network share; only a folder on this computer's own drives can be shared"));
        }
        PathBuf::from(plain)
    };
    Ok(real)
}

/// Where a path the server sent is, inside the shared folder `base`; None
/// when it would lead out of it. The path starts with a backslash (it is
/// joined to the folder as text: without one, `\x` after `/s/Share` would
/// be `/s/Sharex`), then names between backslashes: none empty, `.` or
/// `..`, none with `/` or NUL, and no symbolic link anywhere on the way (a
/// link may point anywhere). With `listing` the last name may end in `*`,
/// which isn't part of the name: the backend lists that folder.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub fn inside(base: &Path, path: &str, listing: bool) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    if path.is_empty() {
        return Some(out);
    }
    let rest = path.strip_prefix('\\')?;
    if rest.is_empty() {
        return Some(out);
    }
    let parts: Vec<&str> = rest.split('\\').collect();
    let count = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        let last = i + 1 == count;
        let name = if last && listing { part.trim_end_matches('*') } else { part };
        if last && listing && name.is_empty() {
            break;
        }
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return None;
        }
        out.push(name);
        if std::fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
        }
    }
    Some(out)
}

/// The RDPDR channel for `share`. Not on a phone.
pub fn channel(share: &Share) -> Result<ironrdp::rdpdr::Rdpdr, String> {
    #[cfg(windows)]
    {
        use ironrdp_rdpdr_native::{RedirectedDrive, WindowsRdpdrBackendFactory};
        // A drive's root or a folder: the vendored backend takes both.
        let drive = RedirectedDrive::new(DEVICE_ID, share_name(share), root(share)?, share.read_only).map_err(|e| e.to_string())?;
        let factory = WindowsRdpdrBackendFactory::new(drive);
        let backend = separators::Separators(Box::new(factory.build()));
        Ok(ironrdp::rdpdr::Rdpdr::new(Box::new(backend), COMPUTER_NAME.to_owned()).with_drives(Some(factory.initial_drives())))
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let backend = separators::Separators(Box::new(confined::Confined::new(root(share)?, share.read_only)));
        Ok(ironrdp::rdpdr::Rdpdr::new(Box::new(backend), COMPUTER_NAME.to_owned()).with_drives(Some(vec![(DEVICE_ID, share_name(share))])))
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = (share, DEVICE_ID, COMPUTER_NAME);
        Err("files are shared from a computer, not from a phone".into())
    }
}

/// The drives this computer has, for the form (`X:\`); none but on Windows.
pub fn drives() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        let used = unsafe { GetLogicalDrives() };
        ('A'..='Z').filter(|c| used & (1 << (*c as u32 - 'A' as u32)) != 0).map(|c| format!("{c}:\\")).collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// xrdp (0.10) sends the path of an open, a new file, a delete and a
/// rename's old name with `/` between names, while its listings and lookups
/// use `\`. IronRDP refuses a path that starts with `/` as a rooted one, so
/// the server could list the share but open nothing in it. No name on
/// Windows, macOS or Linux can hold a `/`, so it is only ever a separator:
/// it becomes `\` before the backend checks the path as always. Everything
/// else is passed on as it is.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod separators {
    use ironrdp::pdu::PduResult;
    use ironrdp::rdpdr::pdu::efs::{
        AnyIoCtlCode, DecodedDeviceControlRequest, DeviceControlRequest, DeviceIoRequest, FileInformationClass, PrinterIoRequest,
        ServerDeviceAnnounceResponse, ServerDriveIoRequest,
    };
    use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
    use ironrdp::rdpdr::{Rdpdr, RdpdrBackend};
    use ironrdp::svc::SvcMessage;

    #[derive(Debug)]
    pub struct Separators(pub Box<dyn RdpdrBackend>);

    ironrdp::core::impl_as_any!(Separators);

    /// `req` with `\` for every `/` in the paths it names.
    pub fn with_backslashes(req: ServerDriveIoRequest) -> ServerDriveIoRequest {
        match req {
            ServerDriveIoRequest::ServerCreateDriveRequest(mut r) => {
                r.path = r.path.replace('/', "\\");
                ServerDriveIoRequest::ServerCreateDriveRequest(r)
            }
            ServerDriveIoRequest::ServerDriveSetInformationRequest(mut r) => {
                if let FileInformationClass::Rename(to) = &mut r.set_buffer {
                    to.file_name = to.file_name.replace('/', "\\");
                }
                ServerDriveIoRequest::ServerDriveSetInformationRequest(r)
            }
            other => other,
        }
    }

    impl RdpdrBackend for Separators {
        fn supports_drive_security(&self) -> bool {
            self.0.supports_drive_security()
        }

        fn reset(&mut self) -> PduResult<()> {
            self.0.reset()
        }

        fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
            self.0.restore_drive(device_id)
        }

        fn add_drive(&mut self, device_id: u32) -> PduResult<()> {
            self.0.add_drive(device_id)
        }

        fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
            self.0.remove_drive(device_id)
        }

        fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
            self.0.handle_server_device_announce_response(pdu)
        }

        fn handle_scard_call(&mut self, req: DeviceControlRequest<ScardIoCtlCode>, call: ScardCall) -> PduResult<Vec<SvcMessage>> {
            self.0.handle_scard_call(req, call)
        }

        fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
            self.0.handle_drive_io_request(with_backslashes(req))
        }

        fn handle_drive_device_control(&mut self, req: DecodedDeviceControlRequest<AnyIoCtlCode>) -> PduResult<Vec<SvcMessage>> {
            self.0.handle_drive_device_control(req)
        }

        fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
            self.0.poll_deferred_messages()
        }

        fn handle_user_logged_on(&mut self, rdpdr: &mut Rdpdr) -> PduResult<Vec<SvcMessage>> {
            self.0.handle_user_logged_on(rdpdr)
        }

        fn handle_printer_io_request(&mut self, req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
            self.0.handle_printer_io_request(req)
        }

        fn reject_printer_write(&mut self, req: DeviceIoRequest) -> PduResult<Vec<SvcMessage>> {
            self.0.reject_printer_write(req)
        }
    }
}

/// IronRDP's backend for macOS and Linux behind a check of every path the
/// server sends (`inside`), and of every change for a read-only share.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod confined {
    use super::inside;
    use ironrdp::pdu::PduResult;
    use ironrdp::rdpdr::pdu::efs::{
        ClientDriveQueryDirectoryResponse, ClientDriveSetInformationResponse, CreateDisposition, CreateOptions, DeviceCreateResponse,
        DeviceIoResponse, DeviceWriteResponse, FileInformationClass, Information, NtStatus, ServerDeviceAnnounceResponse, ServerDriveIoRequest,
    };
    use ironrdp::rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
    use ironrdp::rdpdr::pdu::RdpdrPdu;
    use ironrdp::rdpdr::RdpdrBackend;
    use ironrdp::svc::SvcMessage;
    use ironrdp_rdpdr_native::backend::NixRdpdrBackend;
    use std::path::PathBuf;

    /// The most one read gives (as the Windows backend's limit).
    const MAX_READ: u32 = 1024 * 1024;

    #[derive(Debug)]
    pub struct Confined {
        inner: NixRdpdrBackend,
        base: PathBuf,
        read_only: bool,
    }

    ironrdp::core::impl_as_any!(Confined);

    impl Confined {
        /// `base` is `root`'s answer: resolved, and text.
        pub fn new(base: PathBuf, read_only: bool) -> Self {
            Self { inner: NixRdpdrBackend::new(base.to_string_lossy().into_owned()), base, read_only }
        }

        /// The answer to a request that mustn't reach the backend: a path that
        /// leads out of the folder, something there that is neither a file
        /// nor a folder (opening a FIFO waits for a writer, and would hold up
        /// the whole session), or a change to a read-only share.
        fn refused(&self, req: &ServerDriveIoRequest) -> Option<Vec<SvcMessage>> {
            let answer = |pdu: RdpdrPdu| Some(vec![SvcMessage::from(pdu)]);
            match req {
                ServerDriveIoRequest::ServerCreateDriveRequest(r) => {
                    let changes = r.create_disposition != CreateDisposition::FILE_OPEN || r.create_options.contains(CreateOptions::FILE_DELETE_ON_CLOSE);
                    let place = inside(&self.base, &r.path, false);
                    let odd = place.as_deref().and_then(|p| std::fs::symlink_metadata(p).ok()).is_some_and(|m| !m.is_file() && !m.is_dir());
                    if place.is_none() || odd || (self.read_only && changes) {
                        return answer(RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                            device_io_reply: DeviceIoResponse::new(r.device_io_request.clone(), NtStatus::ACCESS_DENIED),
                            file_id: 0,
                            information: Information::empty(),
                        }));
                    }
                }
                ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) if r.initial_query != 0 && inside(&self.base, &r.path, true).is_none() => {
                    return answer(RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
                        device_io_reply: DeviceIoResponse::new(r.device_io_request.clone(), NtStatus::NO_MORE_FILES),
                        buffer: None,
                    }));
                }
                ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => {
                    let leads_out = matches!(&r.set_buffer, FileInformationClass::Rename(to) if inside(&self.base, &to.file_name, false).is_none());
                    if self.read_only || leads_out {
                        return ClientDriveSetInformationResponse::new(r, NtStatus::ACCESS_DENIED)
                            .ok()
                            .and_then(|resp| answer(RdpdrPdu::ClientDriveSetInformationResponse(resp)));
                    }
                }
                ServerDriveIoRequest::DeviceWriteRequest(r) if self.read_only => {
                    return answer(RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
                        device_io_reply: DeviceIoResponse::new(r.device_io_request.clone(), NtStatus::ACCESS_DENIED),
                        length: 0,
                    }));
                }
                _ => {}
            }
            None
        }
    }

    impl RdpdrBackend for Confined {
        fn reset(&mut self) -> PduResult<()> {
            self.inner.reset()
        }

        fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
            self.inner.restore_drive(device_id)
        }

        fn handle_server_device_announce_response(&mut self, pdu: ServerDeviceAnnounceResponse) -> PduResult<()> {
            self.inner.handle_server_device_announce_response(pdu)
        }

        fn handle_scard_call(
            &mut self,
            req: ironrdp::rdpdr::pdu::efs::DeviceControlRequest<ScardIoCtlCode>,
            call: ScardCall,
        ) -> PduResult<Vec<SvcMessage>> {
            self.inner.handle_scard_call(req, call)
        }

        fn handle_drive_io_request(&mut self, mut req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
            if let Some(answer) = self.refused(&req) {
                return Ok(answer);
            }
            // A read gives a megabyte at most (the server reads on from there):
            // IronRDP's backend makes room for all the server asks, up to 4 GB.
            if let ServerDriveIoRequest::DeviceReadRequest(r) = &mut req {
                r.length = r.length.min(MAX_READ);
            }
            self.inner.handle_drive_io_request(req)
        }

        fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
            self.inner.poll_deferred_messages()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(kind: ShareKind, path: &str) -> Share {
        Share { kind, path: path.into(), read_only: false }
    }

    #[test]
    fn the_server_sees_the_folder_or_drive_by_a_plain_name() {
        assert_eq!(share_name(&share(ShareKind::Folder, "/home/alice/To the server")), "Totheserver");
        assert_eq!(share_name(&share(ShareKind::Folder, r"C:\Users\alice\Shared\")), "Shared");
        assert_eq!(share_name(&share(ShareKind::Folder, r"C:\Users\alice\Pictures_2026")), "Pictures_2026");
        assert_eq!(share_name(&share(ShareKind::Drive, r"d:\")), "D");
        assert_eq!(share_name(&share(ShareKind::Drive, "/")), "Shared", "nothing left");
        assert_eq!(share_name(&share(ShareKind::Folder, "/srv/مشترک")), "Shared");
        assert_eq!(share_name(&share(ShareKind::Folder, &format!("/x/{}", "a".repeat(60)))).len(), 32);
    }

    #[test]
    fn the_options_from_the_tab() {
        let s: Share = serde_json::from_str(r#"{"kind":"folder","path":"/tmp/x","readOnly":true}"#).unwrap();
        assert_eq!(s, Share { kind: ShareKind::Folder, path: "/tmp/x".into(), read_only: true });
        let d: Share = serde_json::from_str(r#"{"kind":"drive","path":"D:\\"}"#).unwrap();
        assert!(d.kind == ShareKind::Drive && !d.read_only, "writable unless said");
        assert!(serde_json::from_str::<Share>(r#"{"kind":"everything","path":"/"}"#).is_err());
    }

    #[test]
    fn only_a_folder_that_is_there_is_shared() {
        let dir = std::env::temp_dir().join(format!("sub-share-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = root(&share(ShareKind::Folder, &dir.to_string_lossy())).unwrap();
        assert_eq!(ok, std::fs::canonicalize(&dir).unwrap());
        assert!(root(&share(ShareKind::Folder, "")).is_err());
        assert!(root(&share(ShareKind::Folder, "relative/folder")).is_err());
        assert!(root(&share(ShareKind::Folder, &dir.join("not-there").to_string_lossy())).is_err());
        std::fs::write(dir.join("a-file"), b"x").unwrap();
        assert!(root(&share(ShareKind::Folder, &dir.join("a-file").to_string_lossy())).is_err(), "a file isn't a folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_from_the_server_stays_in_the_folder() {
        let base = std::env::temp_dir().join(format!("sub-share-inside-{}", std::process::id()));
        std::fs::create_dir_all(base.join("docs")).unwrap();
        // The root, a name, a folder and a name in it, a new file.
        assert_eq!(inside(&base, "", false), Some(base.clone()));
        assert_eq!(inside(&base, r"\", false), Some(base.clone()));
        assert_eq!(inside(&base, r"\docs\a.txt", false), Some(base.join("docs").join("a.txt")));
        assert_eq!(inside(&base, r"\new file (2).txt", false), Some(base.join("new file (2).txt")));
        // Out of the folder, or not a plain name.
        for bad in [r"\..", r"\docs\..\..\etc\passwd", r"\.\docs", r"\docs\\a", r"\a/../../b", "\\a\0b", "docs", r"docs\a", r"\docs\"] {
            assert_eq!(inside(&base, bad, false), None, "{bad:?}");
        }
        // A listing: the last name may end in `*`, which isn't part of it.
        assert_eq!(inside(&base, r"\*", true), Some(base.clone()));
        assert_eq!(inside(&base, r"\docs\*", true), Some(base.join("docs")));
        assert_eq!(inside(&base, r"\..*", true), None, "the folder above, listed");
        assert_eq!(inside(&base, r"\docs\*", false), Some(base.join("docs").join("*")), "a `*` is a name's own elsewhere");
        #[cfg(unix)]
        {
            // A link inside that points out: never followed, wherever it is on the way.
            std::os::unix::fs::symlink("/etc", base.join("out")).unwrap();
            assert_eq!(inside(&base, r"\out", false), None);
            assert_eq!(inside(&base, r"\out\passwd", false), None);
            assert_eq!(inside(&base, r"\out*", true), None);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn odd_files_and_big_reads_in_the_folder_hold_nothing_up() {
        use ironrdp::core::ReadCursor;
        use ironrdp::rdpdr::pdu::efs::{DeviceIoRequest, ServerDriveIoRequest};
        use ironrdp::rdpdr::RdpdrBackend;
        let dir = std::env::temp_dir().join(format!("sub-share-odd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("small.txt"), b"hello").unwrap();
        assert!(std::process::Command::new("mkfifo").arg(dir.join("pipe")).status().unwrap().success());
        let base = std::fs::canonicalize(&dir).unwrap();
        let mut backend = confined::Confined::new(base, false);
        // A request as a server sends it (device 1), and the answer's bytes.
        let mut send = |file_id: u32, major: u32, payload: Vec<u8>| -> Vec<u8> {
            let mut bytes = Vec::new();
            for x in [1u32, file_id, 7, major, 0] {
                bytes.extend(x.to_le_bytes());
            }
            bytes.extend(payload);
            let mut src = ReadCursor::new(&bytes);
            let io = DeviceIoRequest::decode(&mut src).unwrap();
            let req = ServerDriveIoRequest::decode(io, &mut src).unwrap();
            backend.handle_drive_io_request(req).unwrap()[0].encode_unframed_pdu().unwrap()
        };
        // Opened to read (FILE_OPEN).
        let open = |path: &str| {
            let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
            let mut bytes = Vec::new();
            for x in [0x0012_0089u32, 0, 0, 0, 7, 1, 0, (units.len() * 2) as u32] {
                bytes.extend(x.to_le_bytes());
            }
            bytes.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            bytes
        };
        let status = |r: &[u8]| u32::from_le_bytes(r[12..16].try_into().unwrap());
        // A FIFO: refused at once, not opened (that would wait for a writer).
        assert_eq!(status(&send(0, 0, open(r"\pipe"))), 0xC000_0022, "access denied");
        // A read asking for 4 GB gets what there is, without room made for 4 GB.
        let r = send(0, 0, open(r"\small.txt"));
        assert_eq!(status(&r), 0);
        let file_id = u32::from_le_bytes(r[16..20].try_into().unwrap());
        let mut read = u32::MAX.to_le_bytes().to_vec();
        read.extend(0u64.to_le_bytes());
        read.extend([0u8; 20]);
        let r = send(file_id, 3, read);
        assert_eq!(status(&r), 0);
        assert_eq!(&r[20..], b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[test]
    fn xrdp_opens_a_file_by_a_path_with_slashes() {
        use ironrdp::core::ReadCursor;
        use ironrdp::rdpdr::pdu::efs::{DeviceIoRequest, ServerDriveIoRequest};
        // How xrdp 0.10 asks to open `\hello.txt` to read it (from its traffic): device 1,
        // file 0, completion 2, create; read data + synchronize, shared, open, synchronous
        // I/O; and the path "/hello.txt".
        let create = |path: &str| {
            let mut bytes = Vec::new();
            let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
            for x in [1u32, 0, 2, 0, 0, 0x0010_0001, 0, 0, 0, 7, 1, 0x20, (units.len() * 2) as u32] {
                bytes.extend(x.to_le_bytes());
            }
            bytes.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            let mut src = ReadCursor::new(&bytes);
            let io = DeviceIoRequest::decode(&mut src).unwrap();
            match separators::with_backslashes(ServerDriveIoRequest::decode(io, &mut src).unwrap()) {
                ServerDriveIoRequest::ServerCreateDriveRequest(r) => r.path,
                other => panic!("not a create request: {other:?}"),
            }
        };
        assert_eq!(create("/hello.txt"), r"\hello.txt");
        assert_eq!(create("/docs/a b.txt"), r"\docs\a b.txt");
        assert_eq!(create(r"\as\before"), r"\as\before");
        // The backend still checks what comes out: a way out stays a way out.
        let base = std::env::temp_dir();
        assert_eq!(inside(&base, &create("/../etc/passwd"), false), None);
        assert_eq!(inside(&base, &create("//server/share"), false), None);
    }
}
