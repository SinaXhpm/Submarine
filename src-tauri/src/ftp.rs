//! FTP and FTPS: the protocol client under an FTP server's tab.
//!
//! One `Control` is one control connection, logged in. It works over any byte
//! stream: a TCP connection to the server, or a channel through an SSH server.
//! Data connections go the same way, always passive (EPSV, else PASV), to the
//! address the control connection went to: the address in a PASV answer is
//! ignored, so a server can't send this client to another host (the "FTP
//! bounce": a port on this machine, or inside the network it sits in).
//!
//! FTPS is TLS from the first byte ("implicit", port 990) or after `AUTH TLS`
//! ("explicit"). Either way the data connections are protected too (`PROT P`)
//! and must show the control connection's certificate, or resume its TLS
//! session; a server that refuses `PROT P` is left, not used in the clear.
//! Plain FTP is the user's own choice, saved with the server.
//!
//! Everything the server sends is held to limits: a line, an answer, a
//! listing, and the time each may take.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls;
use zeroize::Zeroizing;

pub const DEFAULT_PORT: u16 = 21;
pub const IMPLICIT_PORT: u16 = 990;

/// How long the server gets to answer a command.
pub const ANSWER_TIME: Duration = Duration::from_secs(30);
/// How long a connection (TCP, or a channel through an SSH server) may take.
pub const CONNECT_TIME: Duration = Duration::from_secs(15);
/// How long the server may take to say a transfer ended, once the data has
/// all gone: it may still be writing the file out.
pub const FINISH_TIME: Duration = Duration::from_secs(120);
/// How long an upload's data connection is read after its end, for what the
/// server still sends before it closes it (`Data::shutdown`).
const DRAIN_TIME: Duration = Duration::from_secs(5);
/// The longest line in an answer, and the most an answer may hold.
const LINE_MAX: usize = 8 * 1024;
const ANSWER_MAX: usize = 256 * 1024;
/// The most a folder listing may take, as shell_sftp's.
pub const LISTING_MAX: usize = 64 * 1024 * 1024;
/// How long a whole listing may take (under the SFTP client's 240 s per request).
const LISTING_TIME: Duration = Duration::from_secs(150);

/// How the connection is protected; saved with the server as its protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// Plain FTP: nothing is encrypted, the password included.
    Plain,
    /// `AUTH TLS` on the usual port before the login.
    Explicit,
    /// TLS from the first byte, usually on port 990.
    Implicit,
}

impl Security {
    /// A server's protocol column: "ftp", "ftps" (explicit) or "ftps-implicit".
    pub fn of_protocol(protocol: Option<&str>) -> Option<Security> {
        match protocol {
            Some("ftp") => Some(Security::Plain),
            Some("ftps") => Some(Security::Explicit),
            Some("ftps-implicit") => Some(Security::Implicit),
            _ => None,
        }
    }

    pub fn default_port(self) -> u16 {
        if self == Security::Implicit {
            IMPLICIT_PORT
        } else {
            DEFAULT_PORT
        }
    }

    pub fn encrypted(self) -> bool {
        self != Security::Plain
    }

    /// For the page.
    pub fn name(self) -> &'static str {
        match self {
            Security::Plain => "plain",
            Security::Explicit => "explicit",
            Security::Implicit => "implicit",
        }
    }
}

/// A byte stream to the server.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

pub type DialFuture = Pin<Box<dyn Future<Output = Result<Stream, String>> + Send>>;
/// Opens a connection to one of the server's ports: straight, or through an
/// SSH server. The host is always the one the user saved.
pub type Dial = Arc<dyn Fn(u16) -> DialFuture + Send + Sync>;

/// What went wrong.
#[derive(Debug)]
pub enum Error {
    /// The control connection is gone, or answered in a way that leaves it
    /// unusable: it is dropped and a new one is made.
    Broken(String),
    /// The server answered no (a 4xx or 5xx, or another code than the one
    /// asked for). The connection is fine.
    Refused(Reply),
    /// Refused here, before anything was sent (a name with a line break), or a
    /// data connection that failed; the control connection is fine.
    Local(String),
}

impl Error {
    pub fn message(&self) -> String {
        match self {
            Error::Broken(m) | Error::Local(m) => m.clone(),
            Error::Refused(r) => r.said(),
        }
    }

    pub fn is_broken(&self) -> bool {
        matches!(self, Error::Broken(_))
    }

    /// The code of a refusal.
    pub fn code(&self) -> Option<u16> {
        match self {
            Error::Refused(r) => Some(r.code),
            _ => None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// One answer: its code and its lines (the code taken off the first and last).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn class(&self) -> u16 {
        self.code / 100
    }

    /// What the server said, on one line, for a message: "550 No such file".
    pub fn said(&self) -> String {
        let text: Vec<&str> = self.lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let mut s = format!("{} {}", self.code, text.join(" "));
        if s.chars().count() > 300 {
            s = s.chars().take(300).collect::<String>() + "…";
        }
        // No control characters from the server reach a message.
        s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
    }

    fn first(&self) -> &str {
        self.lines.first().map(String::as_str).unwrap_or("")
    }
}

/// Finds one whole answer at the start of `buf`: the answer and how many bytes
/// it took, or `None` when more has to come first. A line is three digits and
/// a space (the last), or three digits and `-` (more lines follow, up to one
/// that starts with the same three digits and a space).
fn parse_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, String> {
    let mut lines: Vec<String> = Vec::new();
    let mut at = 0;
    let mut code: Option<u16> = None;
    loop {
        let Some(nl) = buf[at..].iter().position(|b| *b == b'\n') else {
            if buf.len() - at > LINE_MAX {
                return Err("The server sent a line too long to be an answer".into());
            }
            return Ok(None);
        };
        let raw = &buf[at..at + nl];
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.len() > LINE_MAX {
            return Err("The server sent a line too long to be an answer".into());
        }
        let line = String::from_utf8_lossy(raw).into_owned();
        at += nl + 1;
        match code {
            None => {
                let b = line.as_bytes();
                if b.len() < 3 || !b[..3].iter().all(u8::is_ascii_digit) || (b.len() > 3 && b[3] != b' ' && b[3] != b'-') {
                    return Err(format!("The server's answer isn't FTP: {:?}", line.chars().take(80).collect::<String>()));
                }
                let c: u16 = line[..3].parse().map_err(|_| "Bad answer".to_string())?;
                if !(100..600).contains(&c) {
                    return Err(format!("The server answered with code {c}"));
                }
                let more = b.len() > 3 && b[3] == b'-';
                lines.push(line.get(4..).unwrap_or("").to_string());
                if !more {
                    return Ok(Some((Reply { code: c, lines }, at)));
                }
                code = Some(c);
            }
            Some(c) => {
                let end = line.len() >= 4 && line.as_bytes()[3] == b' ' && line[..3].parse::<u16>().ok() == Some(c);
                if end {
                    lines.push(line[4..].to_string());
                    return Ok(Some((Reply { code: c, lines }, at)));
                }
                lines.push(line);
                if lines.len() > 10_000 {
                    return Err("The server's answer has too many lines".into());
                }
            }
        }
    }
}

/// Refuses what can't go in a command: a line break would end it and start
/// another (a name like "x\r\nDELE y"), and NUL is no name's.
pub fn check_arg(s: &str) -> Result<(), Error> {
    if s.contains(['\r', '\n', '\0']) {
        return Err(Error::Local("A name with a line break can't be used over FTP".into()));
    }
    Ok(())
}

/// What the server can do, from FEAT.
#[derive(Clone, Debug, Default)]
pub struct Features {
    /// MLST and MLSD: listings with exact fields and UTC times.
    pub mlst: bool,
    pub utf8: bool,
    pub mfmt: bool,
    pub size: bool,
    pub mdtm: bool,
    pub rest: bool,
    pub epsv: bool,
}

fn parse_feat(r: &Reply) -> Features {
    let mut f = Features::default();
    // The first and last lines are the reply's own text; the features are
    // the lines between, each starting with a space.
    let n = r.lines.len();
    for (i, line) in r.lines.iter().enumerate() {
        if i == 0 || i + 1 == n {
            continue;
        }
        let word = line.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        match word.as_str() {
            "MLST" => f.mlst = true,
            "UTF8" => f.utf8 = true,
            "MFMT" => f.mfmt = true,
            "SIZE" => f.size = true,
            "MDTM" => f.mdtm = true,
            "REST" => f.rest = line.to_ascii_uppercase().contains("STREAM"),
            "EPSV" => f.epsv = true,
            _ => {}
        }
    }
    f
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// The TLS side of one tab's connections: one configuration, so the data
/// connections can resume the control connection's session (vsftpd and
/// others insist on it) and no tab can resume another's.
#[derive(Clone)]
pub struct Tls {
    config: Arc<rustls::ClientConfig>,
    name: rustls::pki_types::ServerName<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl Tls {
    pub fn new(host: &str) -> Result<Tls, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(CheckAfter(Arc::clone(&provider))))
            .with_no_client_auth();
        // The default resumption (sessions kept in memory) stays on: it is what
        // lets a data connection resume the control connection's session.
        let name = rustls::pki_types::ServerName::try_from(host.trim().to_string())
            .map_err(|_| format!("{host:?} can't be checked against a certificate"))?;
        Ok(Tls { config: Arc::new(config), name, provider })
    }

    async fn connect(&self, io: Stream) -> Result<tokio_rustls::client::TlsStream<Stream>, String> {
        let connector = tokio_rustls::TlsConnector::from(Arc::clone(&self.config));
        tokio::time::timeout(ANSWER_TIME, connector.connect(self.name.clone(), io))
            .await
            .map_err(|_| "TLS with the server didn't finish within 30 seconds".to_string())?
            .map_err(|e| format!("TLS with the server failed: {e}"))
    }

    /// Whether a chain is valid for the host by the public certificate
    /// authorities (Mozilla's list), and why not when it isn't.
    pub fn ca_check(&self, chain: &[Vec<u8>]) -> Result<(), String> {
        use rustls::client::danger::ServerCertVerifier as _;
        let (end, rest) = chain.split_first().ok_or("no certificate")?;
        let mut roots = rustls::RootCertStore::empty();
        roots.roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&self.provider))
            .build()
            .map_err(|e| e.to_string())?;
        let end = rustls::pki_types::CertificateDer::from(end.as_slice());
        let rest: Vec<_> = rest.iter().map(|c| rustls::pki_types::CertificateDer::from(c.as_slice())).collect();
        verifier
            .verify_server_cert(&end, &rest, &self.name, &[], rustls::pki_types::UnixTime::now())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Takes any certificate during the handshake, and the caller checks it right
/// after (`Peer`), before the login: against the certificate authorities, or
/// as pinned. The handshake's signatures are checked here all the same, so
/// whoever answers holds the certificate's key. (As rdp_client's PinLater.)
#[derive(Debug)]
struct CheckAfter(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for CheckAfter {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The server's side of a TLS connection, for the check after the handshake.
#[derive(Clone, Debug)]
pub struct Peer {
    /// The certificate chain as sent, the server's own first.
    pub chain: Vec<Vec<u8>>,
    /// "TLS 1.3".
    pub version: String,
}

impl Peer {
    fn of(conn: &rustls::ClientConnection) -> Peer {
        let chain = conn
            .peer_certificates()
            .map(|c| c.iter().map(|d| d.as_ref().to_vec()).collect())
            .unwrap_or_default();
        let version = match conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLS 1.3",
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLS 1.2",
            _ => "TLS",
        };
        Peer { chain, version: version.to_string() }
    }

    pub fn cert(&self) -> Option<&[u8]> {
        self.chain.first().map(Vec::as_slice)
    }
}

/// "SHA256:<base64>" of a certificate, as for RDP servers and SSH host keys.
pub fn fingerprint(der: &[u8]) -> String {
    crate::rdp_client::cert_fingerprint(der)
}

// ---------------------------------------------------------------------------
// The control connection
// ---------------------------------------------------------------------------

/// Logged in, ready for commands.
pub struct Control {
    io: Stream,
    /// Read from the server, not yet taken as an answer.
    buf: Vec<u8>,
    dial: Dial,
    /// Set under FTPS: data connections are protected as the control is.
    tls: Option<Tls>,
    /// The control connection's certificate, for the data connections' check.
    cert: Option<Vec<u8>>,
    pub features: Features,
    /// The folder the login started in (PWD).
    pub home: String,
    /// EPSV was refused once: PASV from then on.
    no_epsv: bool,
    /// `LIST -a` came back empty where `LIST` didn't: the server took `-a`
    /// for a name.
    no_list_a: bool,
    last_used: Instant,
}

/// What `open` found before the login: for the caller to check.
pub struct Opened {
    pub control: Control,
    /// The server's TLS side (FTPS only).
    pub peer: Option<Peer>,
    /// The greeting, for the log.
    pub greeting: String,
}

impl Control {
    /// Connects and, under FTPS, starts TLS; nothing secret has gone yet. The
    /// caller checks `peer` and then calls `login`.
    pub async fn open(dial: Dial, port: u16, security: Security, tls: Option<Tls>) -> Result<Opened, Error> {
        let raw = dial(port).await.map_err(Error::Broken)?;
        let tls = if security.encrypted() {
            Some(tls.ok_or_else(|| Error::Local("TLS isn't set up".into()))?)
        } else {
            None
        };
        let mut peer = None;
        let io: Stream = match (security, &tls) {
            (Security::Implicit, Some(t)) => {
                // What the server sends first is kept: a server that refuses the
                // connection answers in plain FTP even here (vsftpd's "421 too
                // many connections"), and TLS then fails on it.
                let (raw, first) = FirstBytes::wrap(raw);
                match t.connect(raw).await {
                    Ok(s) => {
                        peer = Some(Peer::of(s.get_ref().1));
                        Box::new(s)
                    }
                    Err(e) => {
                        let seen = first.lock().map(|b| b.clone()).unwrap_or_default();
                        return Err(match parse_reply(&seen) {
                            Ok(Some((reply, _))) => Error::Refused(reply),
                            _ => Error::Broken(e),
                        });
                    }
                }
            }
            _ => raw,
        };
        let mut c = Control {
            io,
            buf: Vec::new(),
            dial,
            tls: None,
            cert: None,
            features: Features::default(),
            home: "/".into(),
            no_epsv: false,
            no_list_a: false,
            last_used: Instant::now(),
        };
        // The greeting: 220, maybe after a 120 ("ready in a moment").
        let mut greeting = c.read_reply(ANSWER_TIME).await?;
        if greeting.code == 120 {
            greeting = c.read_reply(Duration::from_secs(60)).await?;
        }
        if greeting.code != 220 {
            return Err(Error::Refused(greeting));
        }
        if security == Security::Explicit {
            let r = c.cmd("AUTH TLS").await?;
            if r.code != 234 {
                return Err(Error::Local(format!(
                    "The server doesn't offer TLS ({}). Nothing was sent in the clear; choose plain FTP for this server only if that is meant.",
                    r.said()
                )));
            }
            // A server sends nothing between its 234 and TLS. Bytes that came
            // anyway would be read as if they came inside TLS, while anyone on
            // the way could have put them there.
            if !c.buf.is_empty() {
                return Err(Error::Broken("The server sent data before TLS began, so the connection was dropped".into()));
            }
            let t = tls.as_ref().expect("explicit has TLS");
            let raw = std::mem::replace(&mut c.io, Box::new(empty::EmptyDuplex));
            let s = t.connect(raw).await.map_err(Error::Broken)?;
            peer = Some(Peer::of(s.get_ref().1));
            c.io = Box::new(s);
        }
        c.cert = peer.as_ref().and_then(|p| p.cert().map(<[u8]>::to_vec));
        c.tls = tls;
        Ok(Opened { greeting: greeting.said(), control: c, peer })
    }

    /// Logs in and sets the connection up: protected data under FTPS, the
    /// server's features, UTF-8 names, binary transfers, the start folder.
    pub async fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        check_arg(user)?;
        if password.contains(['\r', '\n', '\0']) {
            return Err(Error::Local("A password with a line break can't be sent over FTP".into()));
        }
        let r = self.cmd(&format!("USER {user}")).await?;
        let r = match r.code {
            230 => r,
            331 => {
                // Written and dropped at once; the line itself is wiped.
                let line = Zeroizing::new(format!("PASS {password}"));
                self.cmd(&line).await?
            }
            _ => return Err(Error::Refused(r)),
        };
        match r.code {
            230 | 202 => {}
            332 => return Err(Error::Local("The server asks for an account (ACCT) as well, which isn't supported".into())),
            _ => return Err(Error::Refused(r)),
        }
        if self.tls.is_some() {
            self.expect("PBSZ 0", 200).await?;
            let r = self.cmd("PROT P").await?;
            if r.code != 200 {
                return Err(Error::Local(format!(
                    "The server won't protect file transfers with TLS ({}); they aren't sent in the clear",
                    r.said()
                )));
            }
        }
        let r = self.cmd("FEAT").await?;
        if r.code == 211 {
            self.features = parse_feat(&r);
        }
        if self.features.utf8 {
            let _ = self.cmd("OPTS UTF8 ON").await?;
        }
        if self.features.mlst {
            // The fields wanted, where the server lets them be chosen.
            let _ = self.cmd("OPTS MLST type;size;modify;UNIX.mode;").await?;
        }
        self.expect("TYPE I", 200).await?;
        let r = self.cmd("PWD").await?;
        if r.code == 257 {
            if let Some(p) = parse_pwd(r.first()) {
                self.home = p;
            }
        }
        Ok(())
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Reads one whole answer within `limit`.
    async fn read_reply(&mut self, limit: Duration) -> Result<Reply, Error> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match parse_reply(&self.buf).map_err(Error::Broken)? {
                Some((reply, used)) => {
                    self.buf.drain(..used);
                    self.last_used = Instant::now();
                    return Ok(reply);
                }
                None => {
                    if self.buf.len() > ANSWER_MAX {
                        return Err(Error::Broken("The server's answer is too long".into()));
                    }
                    // read_buf only appends what came, so a read cut off by
                    // the deadline (or a select) loses nothing.
                    let n = tokio::time::timeout_at(deadline, self.io.read_buf(&mut self.buf))
                        .await
                        .map_err(|_| Error::Broken("The server didn't answer in time".into()))?
                        .map_err(|e| Error::Broken(format!("The connection to the server failed: {e}")))?;
                    if n == 0 {
                        return Err(Error::Broken("The server closed the connection".into()));
                    }
                }
            }
        }
    }

    async fn send(&mut self, line: &str) -> Result<(), Error> {
        check_arg(line)?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(line.len() + 2));
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        let write = async {
            self.io.write_all(&bytes).await?;
            self.io.flush().await
        };
        tokio::time::timeout(ANSWER_TIME, write)
            .await
            .map_err(|_| Error::Broken("The server stopped taking commands".into()))?
            .map_err(|e| Error::Broken(format!("The connection to the server failed: {e}")))?;
        self.last_used = Instant::now();
        Ok(())
    }

    /// Sends a command and reads its answer, whatever it is. A 421 ("closing")
    /// leaves the connection unusable.
    pub async fn cmd(&mut self, line: &str) -> Result<Reply, Error> {
        self.send(line).await?;
        let r = self.read_reply(ANSWER_TIME).await?;
        if r.code == 421 {
            return Err(Error::Broken(format!("The server closed the connection: {}", r.said())));
        }
        Ok(r)
    }

    /// A command whose answer must be `code`.
    async fn expect(&mut self, line: &str, code: u16) -> Result<Reply, Error> {
        let r = self.cmd(line).await?;
        if r.code == code {
            Ok(r)
        } else {
            Err(Error::Refused(r))
        }
    }

    /// A command that has worked when its answer is 2xx.
    async fn ok(&mut self, line: &str) -> Result<Reply, Error> {
        let r = self.cmd(line).await?;
        if r.class() == 2 {
            Ok(r)
        } else {
            Err(Error::Refused(r))
        }
    }

    pub async fn noop(&mut self) -> Result<(), Error> {
        self.ok("NOOP").await.map(|_| ())
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<(), Error> {
        check_arg(path)?;
        self.ok(&format!("MKD {path}")).await.map(|_| ())
    }

    pub async fn rmdir(&mut self, path: &str) -> Result<(), Error> {
        check_arg(path)?;
        self.ok(&format!("RMD {path}")).await.map(|_| ())
    }

    pub async fn delete(&mut self, path: &str) -> Result<(), Error> {
        check_arg(path)?;
        self.ok(&format!("DELE {path}")).await.map(|_| ())
    }

    pub async fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        check_arg(from)?;
        check_arg(to)?;
        self.expect(&format!("RNFR {from}"), 350).await?;
        self.ok(&format!("RNTO {to}")).await.map(|_| ())
    }

    /// Whether `path` is a folder this login can go into.
    pub async fn is_dir(&mut self, path: &str) -> Result<bool, Error> {
        check_arg(path)?;
        let r = self.cmd(&format!("CWD {path}")).await?;
        Ok(r.class() == 2)
    }

    /// A file's size (SIZE), None when the server has no answer for it.
    pub async fn size(&mut self, path: &str) -> Result<Option<u64>, Error> {
        check_arg(path)?;
        let r = self.cmd(&format!("SIZE {path}")).await?;
        Ok(if r.code == 213 { r.first().trim().parse().ok() } else { None })
    }

    /// A file's time (MDTM, exact and in UTC), None when the server has no
    /// answer for it.
    pub async fn mdtm(&mut self, path: &str) -> Result<Option<u32>, Error> {
        check_arg(path)?;
        let r = self.cmd(&format!("MDTM {path}")).await?;
        Ok(if r.code == 213 { parse_time(r.first().trim()) } else { None })
    }

    /// One path's fields (MLST), where the server has it.
    pub async fn mlst(&mut self, path: &str) -> Result<Option<Entry>, Error> {
        check_arg(path)?;
        let r = self.cmd(&format!("MLST {path}")).await?;
        if r.code != 250 {
            return match r.class() {
                5 => Err(Error::Refused(r)),
                _ => Ok(None),
            };
        }
        // The fields are on a line of their own, which starts with a space.
        Ok(r.lines.iter().skip(1).find_map(|l| parse_mlsx(l.strip_prefix(' ').unwrap_or(l), true)))
    }

    /// Permission bits (SITE CHMOD, which Unix servers offer).
    pub async fn chmod(&mut self, path: &str, mode: u32) -> Result<(), Error> {
        check_arg(path)?;
        self.ok(&format!("SITE CHMOD {:o} {path}", mode & 0o7777)).await.map(|_| ())
    }

    /// A file's time (UTC): MFMT where the server has it, else the
    /// two-argument MDTM that vsftpd and others take (`MDTM <time> <path>`).
    /// A server that knows neither reads that as a name and says no.
    pub async fn set_time(&mut self, path: &str, time: u32) -> Result<(), Error> {
        check_arg(path)?;
        let t = chrono::DateTime::from_timestamp(i64::from(time), 0).ok_or_else(|| Error::Local("Bad time".into()))?;
        let stamp = t.format("%Y%m%d%H%M%S");
        let command = if self.features.mfmt { format!("MFMT {stamp} {path}") } else { format!("MDTM {stamp} {path}") };
        self.ok(&command).await.map(|_| ())
    }

    // ---- data connections ----------------------------------------------------

    /// The server's port for the next data connection: EPSV, else PASV. The
    /// address in a PASV answer isn't used (see the module comment).
    async fn passive_port(&mut self) -> Result<u16, Error> {
        if !self.no_epsv {
            let r = self.cmd("EPSV").await?;
            if r.code == 229 {
                return parse_epsv(&r).ok_or_else(|| Error::Broken(format!("Odd EPSV answer: {}", r.said())));
            }
            if r.class() != 5 {
                return Err(Error::Refused(r));
            }
            self.no_epsv = true;
        }
        let r = self.cmd("PASV").await?;
        if r.code != 227 {
            return Err(Error::Refused(r));
        }
        parse_pasv_port(&r).ok_or_else(|| Error::Broken(format!("Odd PASV answer: {}", r.said())))
    }

    /// Starts a transfer: a data connection, `REST offset` when it isn't 0,
    /// then `command` (RETR, STOR, APPE, LIST, MLSD). Returns the data stream
    /// once the server has said it is coming (1xx); `finish` reads how it ended.
    pub async fn start(&mut self, command: &str, offset: u64) -> Result<Data, Error> {
        check_arg(command)?;
        let port = self.passive_port().await?;
        let raw = (self.dial)(port)
            .await
            .map_err(|e| Error::Local(format!("The data connection to port {port} failed: {e}")))?;
        if offset > 0 {
            self.expect(&format!("REST {offset}"), 350).await?;
        }
        self.send(command).await?;
        let Some(tls) = self.tls.clone() else {
            let r = self.read_reply(ANSWER_TIME).await?;
            if r.class() != 1 {
                return Err(refusal(r));
            }
            return Ok(Data { io: raw, tls_end: false, clean_end: false });
        };
        // Under TLS the server's 1xx and the data connection's handshake come
        // in either order: vsftpd answers 150 only once the handshake is done,
        // others the other way round. Both are waited for together.
        let handshake = tls.connect(raw);
        tokio::pin!(handshake);
        let mut said: Option<Reply> = None;
        let mut data: Option<tokio_rustls::client::TlsStream<Stream>> = None;
        while said.is_none() || data.is_none() {
            tokio::select! {
                r = self.read_reply(ANSWER_TIME), if said.is_none() => {
                    let r = r?;
                    if r.class() != 1 {
                        // Dropping the handshake closes the data connection.
                        return Err(refusal(r));
                    }
                    said = Some(r);
                }
                h = &mut handshake, if data.is_none() => {
                    let s = h.map_err(|e| Error::Local(format!("The data connection's TLS failed: {e}")))?;
                    data = Some(s);
                }
            }
        }
        let data = data.expect("both done");
        let conn = data.get_ref().1;
        let resumed = conn.handshake_kind() == Some(rustls::HandshakeKind::Resumed);
        let same = conn.peer_certificates().and_then(|c| c.first()).map(|c| c.as_ref()) == self.cert.as_deref();
        if !resumed && !same {
            return Err(Error::Broken(
                "The data connection showed another certificate than the control connection, so it was dropped".into(),
            ));
        }
        Ok(Data { io: Box::new(data), tls_end: true, clean_end: false })
    }

    /// How a transfer ended, after its data stream is done with (read to its
    /// end, or written and shut down): the server's 2xx, or what it said.
    pub async fn finish(&mut self) -> Result<Reply, Error> {
        let r = self.read_reply(FINISH_TIME).await?;
        if r.class() == 2 {
            Ok(r)
        } else {
            Err(refusal(r))
        }
    }

    /// After a transfer was given up on (its data stream dropped early): the
    /// server's answer to that, whatever it is (426, 451, 226). A server that
    /// says nothing within a few seconds has left the connection in a state
    /// that can't be trusted, so it is reported as broken.
    pub async fn after_drop(&mut self) -> Result<(), Error> {
        self.read_reply(Duration::from_secs(10)).await.map(|_| ())
    }

    /// The whole of what a command sends over a data connection, up to `max`.
    async fn fetch(&mut self, command: &str, max: usize) -> Result<Vec<u8>, Error> {
        let mut data = self.start(command, 0).await?;
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        // The whole listing within LISTING_TIME, not only each read: a server
        // that sends a byte now and then would hold the tab's file access up.
        let deadline = tokio::time::Instant::now() + LISTING_TIME;
        loop {
            let n = tokio::time::timeout_at(deadline, data.read(&mut chunk))
                .await
                .map_err(|_| Error::Broken("The listing took too long".into()))?
                .map_err(|e| Error::Broken(format!("The listing's connection failed: {e}")))?;
            if n == 0 {
                break;
            }
            if out.len() + n > max {
                drop(data);
                let _ = self.after_drop().await;
                return Err(Error::Local("The folder's listing is too big".into()));
            }
            out.extend_from_slice(&chunk[..n]);
        }
        drop(data);
        self.finish().await?;
        Ok(out)
    }

    /// A folder's entries, without "." and "..". MLSD where the server has
    /// it; else LIST in the folder (a name with spaces, or one starting with
    /// `-`, then never reads as an option), `-a` asked for so names starting
    /// with a dot show.
    pub async fn list(&mut self, dir: &str) -> Result<Vec<Entry>, Error> {
        check_arg(dir)?;
        let now = chrono::Utc::now().timestamp();
        if self.features.mlst {
            let out = self.fetch(&format!("MLSD {dir}"), LISTING_MAX).await?;
            return Ok(listing(&out, |l| parse_mlsx(l, false)));
        }
        self.ok(&format!("CWD {dir}")).await?;
        if !self.no_list_a {
            match self.fetch("LIST -a", LISTING_MAX).await {
                Ok(out) => {
                    let entries = listing(&out, |l| parse_list_line(l, now));
                    if !entries.is_empty() || out.iter().any(|b| !b.is_ascii_whitespace()) {
                        return Ok(entries);
                    }
                    // Nothing at all: maybe a server that took "-a" for a name.
                }
                Err(Error::Refused(r)) if r.class() == 5 => {}
                Err(e) => return Err(e),
            }
            let out = self.fetch("LIST", LISTING_MAX).await?;
            let entries = listing(&out, |l| parse_list_line(l, now));
            if !entries.is_empty() {
                self.no_list_a = true;
            }
            return Ok(entries);
        }
        let out = self.fetch("LIST", LISTING_MAX).await?;
        Ok(listing(&out, |l| parse_list_line(l, now)))
    }
}

/// A 4xx or 5xx as a refusal; a 421 leaves the connection unusable.
fn refusal(r: Reply) -> Error {
    if r.code == 421 {
        Error::Broken(format!("The server closed the connection: {}", r.said()))
    } else {
        Error::Refused(r)
    }
}

/// A transfer's data connection.
pub struct Data {
    io: Stream,
    /// Under TLS: the end of the data should come as TLS's own close.
    tls_end: bool,
    /// The data ended with TLS's close (or, plain, with the connection's).
    clean_end: bool,
}

impl Data {
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.io.read(buf).await {
            Ok(0) => {
                self.clean_end = true;
                Ok(0)
            }
            Ok(n) => Ok(n),
            // Many servers end a download by closing the connection without
            // TLS's close. The end is taken, and `clean_end` says how it came.
            Err(e) if self.tls_end && e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.io.write_all(data).await
    }

    /// The end of an upload: TLS's close, then the connection's, then what
    /// the server still sends read to its end (for a few seconds at most). A
    /// socket closed with bytes unread in it (TLS 1.3's session tickets) is
    /// reset rather than closed, and the reset makes the server throw away
    /// the end of the upload it hasn't read yet: vsftpd's "426 Failure
    /// reading network stream", and a file cut short.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        self.io.flush().await?;
        self.io.shutdown().await?;
        let mut sink = [0u8; 4096];
        let _ = tokio::time::timeout(DRAIN_TIME, async {
            while let Ok(n) = self.io.read(&mut sink).await {
                if n == 0 {
                    break;
                }
            }
        })
        .await;
        Ok(())
    }

    /// Whether a download's data ended as it should: not cut off by someone on
    /// the way (under TLS, a close without TLS's own can be).
    pub fn ended_cleanly(&self) -> bool {
        self.clean_end || !self.tls_end
    }
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// The path in a 257 answer: `"/home/x" is the current directory`, a quote
/// inside written twice.
fn parse_pwd(text: &str) -> Option<String> {
    let start = text.find('"')?;
    let mut out = String::new();
    let mut chars = text[start + 1..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                out.push('"');
            } else {
                return (!out.is_empty()).then_some(out);
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// The port in `229 Entering Extended Passive Mode (|||6446|)`; the delimiter
/// may be any character.
fn parse_epsv(r: &Reply) -> Option<u16> {
    let text = r.first();
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let inner: Vec<char> = text[open + 1..close].chars().collect();
    let d = *inner.first()?;
    if inner.len() < 5 || inner[1] != d || inner[2] != d || *inner.last()? != d {
        return None;
    }
    let port: String = inner[3..inner.len() - 1].iter().collect();
    port.parse::<u16>().ok().filter(|p| *p > 0)
}

/// The port in `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
fn parse_pasv_port(r: &Reply) -> Option<u16> {
    let text = r.first();
    let digits_at = text.find(|c: char| c.is_ascii_digit())?;
    let nums: Vec<u16> = text[digits_at..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(6)
        .map(|s| s.parse::<u16>().ok())
        .collect::<Option<Vec<_>>>()?;
    if nums.len() != 6 || nums.iter().any(|n| *n > 255) {
        return None;
    }
    let port = nums[4] * 256 + nums[5];
    (port > 0).then_some(port)
}

// ---------------------------------------------------------------------------
// Listings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A symbolic link, with its target when the listing tells it.
    Link(Option<String>),
    /// A device, a pipe...: listed, not opened.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: Option<u64>,
    /// UTC seconds.
    pub mtime: Option<u32>,
    /// Permission bits (0o7777), where the server tells them.
    pub mode: Option<u32>,
    /// `mtime` is exact and in UTC (MLSD, MLST, MDTM). A LIST line's time is
    /// to the minute (or the day), in the server's own time zone.
    pub exact: bool,
}

/// Each line of a listing through `parse`, keeping only entries a folder can
/// hold: no "." or "..", no name with a slash (a hostile server's "../x").
fn listing(out: &[u8], parse: impl Fn(&str) -> Option<Entry>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for raw in out.split(|b| *b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        // A name that isn't UTF-8 can't be sent back as it came: skipped.
        let Ok(line) = std::str::from_utf8(raw) else { continue };
        if let Some(e) = parse(line) {
            if e.name != "." && e.name != ".." && crate::sftp_listing::is_plain_name(&e.name) {
                entries.push(e);
            }
        }
        if entries.len() as u64 >= crate::sftp_listing::MAX_ENTRIES {
            break;
        }
    }
    entries
}

/// `YYYYMMDDHHMMSS[.sss]`, UTC, as MLSD's `modify` and MDTM give it.
fn parse_time(s: &str) -> Option<u32> {
    let s = s.split('.').next()?;
    if s.len() != 14 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |a: usize, b: usize| s[a..b].parse::<u32>().ok();
    let date = chrono::NaiveDate::from_ymd_opt(n(0, 4)? as i32, n(4, 6)?, n(6, 8)?)?;
    let t = date.and_hms_opt(n(8, 10)?, n(10, 12)?, n(12, 14)?)?.and_utc().timestamp();
    u32::try_from(t).ok()
}

/// One MLSD line (`type=file;size=12;modify=20261113100500; name`) or MLST's
/// (with a path in place of the name: `whole` keeps it whole).
fn parse_mlsx(line: &str, whole: bool) -> Option<Entry> {
    let (facts, name) = line.split_once(' ')?;
    let name = name.to_string();
    if name.is_empty() {
        return None;
    }
    let mut kind = None;
    let mut size = None;
    let mut mtime = None;
    let mut mode = None;
    for fact in facts.split(';') {
        let Some((k, v)) = fact.split_once('=') else { continue };
        match k.to_ascii_lowercase().as_str() {
            "type" => {
                let lv = v.to_ascii_lowercase();
                kind = Some(match lv.as_str() {
                    "file" => Kind::File,
                    "dir" => Kind::Dir,
                    "cdir" | "pdir" => return None,
                    _ if lv.starts_with("os.unix=symlink") => Kind::Link(None),
                    _ if lv.starts_with("os.unix=slink:") => Kind::Link(v.split_once(':').map(|(_, t)| t.to_string()).filter(|t| !t.is_empty())),
                    _ => Kind::Other,
                });
            }
            "size" | "sizd" => size = v.parse().ok(),
            "modify" => mtime = parse_time(v),
            "unix.mode" => mode = u32::from_str_radix(v.trim_start_matches("0o"), 8).ok().map(|m| m & 0o7777),
            _ => {}
        }
    }
    let kind = kind.unwrap_or(Kind::File);
    let size = if kind == Kind::Dir { Some(0) } else { size };
    Some(Entry { name: if whole { name.rsplit('/').next().unwrap_or(&name).to_string() } else { name }, kind, size, mtime, mode, exact: true })
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// One LIST line: Unix `ls -l` style or the Windows (IIS) style.
pub(crate) fn parse_list_line(line: &str, now: i64) -> Option<Entry> {
    parse_unix_line(line, now).or_else(|| parse_dos_line(line))
}

/// `drwxr-xr-x 2 owner group 4096 Nov 13 10:05 name` (the group may be
/// missing; a device has "major, minor" where the size goes). The name is
/// whatever follows the date, a link's as `name -> target`.
fn parse_unix_line(line: &str, now: i64) -> Option<Entry> {
    let perms = line.split_whitespace().next()?;
    let full = crate::shell_sftp::parse_mode_string(perms)?;
    // The words with where each ends, to cut the name out of the line itself
    // (it may hold runs of spaces).
    let mut words: Vec<(&str, usize)> = Vec::new();
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < bytes.len() && words.len() < 12 {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b' ' {
            i += 1;
        }
        if start < i {
            words.push((&line[start..i], i));
        }
    }
    // The date: a month, a day, then a time (this year) or a year.
    let at = (3..words.len().saturating_sub(2)).find(|&k| {
        MONTHS.contains(&words[k].0.to_ascii_lowercase().as_str())
            && words[k + 1].0.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d))
            && (words[k + 2].0.contains(':') || words[k + 2].0.len() == 4)
            && words[k - 1].0.parse::<u64>().is_ok()
    })?;
    let size: u64 = words[at - 1].0.parse().ok()?;
    let month = MONTHS.iter().position(|m| *m == words[at].0.to_ascii_lowercase())? as u32 + 1;
    let day: u32 = words[at + 1].0.parse().ok()?;
    let when = words[at + 2].0;
    let mtime = if let Some((h, m)) = when.split_once(':') {
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        // No year: this year, or last year for a date that would be ahead.
        let year = chrono::DateTime::from_timestamp(now, 0)?.format("%Y").to_string().parse::<i32>().ok()?;
        let at_year = |y: i32| chrono::NaiveDate::from_ymd_opt(y, month, day).and_then(|d| d.and_hms_opt(h, m, 0)).map(|t| t.and_utc().timestamp());
        let mut t = at_year(year)?;
        if t > now + 2 * 86_400 {
            t = at_year(year - 1)?;
        }
        u32::try_from(t).ok()
    } else {
        let y: i32 = when.parse().ok()?;
        chrono::NaiveDate::from_ymd_opt(y, month, day)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .and_then(|t| u32::try_from(t.and_utc().timestamp()).ok())
    };
    let rest = &line[words[at + 2].1..];
    let name = rest.strip_prefix(' ').unwrap_or(rest);
    if name.is_empty() {
        return None;
    }
    let type_bits = full & 0o170000;
    let (name, kind) = match type_bits {
        0o040000 => (name.to_string(), Kind::Dir),
        0o100000 => (name.to_string(), Kind::File),
        0o120000 => match name.split_once(" -> ") {
            Some((n, target)) => (n.to_string(), Kind::Link(Some(target.to_string()))),
            None => (name.to_string(), Kind::Link(None)),
        },
        _ => (name.to_string(), Kind::Other),
    };
    let size = if kind == Kind::Dir { Some(0) } else { Some(size) };
    Some(Entry { name, kind, size, mtime, mode: Some(full & 0o7777), exact: false })
}

/// `11-13-26  10:05AM       <DIR>          name` or `... 1234 name`.
fn parse_dos_line(line: &str) -> Option<Entry> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(\d{1,2})-(\d{1,2})-(\d{2}|\d{4}) +(\d{1,2}):(\d{2}) ?([AaPp][Mm])? +(<DIR>|\d+) +(.+)$").expect("valid")
    });
    let c = re.captures(line)?;
    let num = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let mut year = num(3)? as i32;
    if year < 100 {
        year += if year < 70 { 2000 } else { 1900 };
    }
    let mut hour = num(4)?;
    if let Some(ampm) = c.get(6) {
        let pm = ampm.as_str().eq_ignore_ascii_case("pm");
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let mtime = chrono::NaiveDate::from_ymd_opt(year, num(1)?, num(2)?)
        .and_then(|d| d.and_hms_opt(hour, num(5)?, 0))
        .and_then(|t| u32::try_from(t.and_utc().timestamp()).ok());
    let name = c.get(8)?.as_str().to_string();
    let dir = c.get(7)?.as_str() == "<DIR>";
    let size = if dir { Some(0) } else { c.get(7)?.as_str().parse().ok() };
    Some(Entry { name, kind: if dir { Kind::Dir } else { Kind::File }, size, mtime, mode: None, exact: false })
}

// ---------------------------------------------------------------------------
// Getting there
// ---------------------------------------------------------------------------

/// A `Dial` straight to the server.
pub fn dial_direct(host: &str) -> Dial {
    let host = host.trim().to_string();
    Arc::new(move |port| {
        let host = host.clone();
        Box::pin(async move {
            let address = crate::telnet::address(&host, port);
            let sock = match tokio::time::timeout(CONNECT_TIME, tokio::net::TcpStream::connect((host.as_str(), port))).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(format!("Couldn't reach {address}: {e}")),
                Err(_) => return Err(format!("{address} didn't answer within 15 seconds")),
            };
            crate::tunnel::apply_tunnel_sockopts(&sock);
            let _ = sock.set_nodelay(true);
            Ok(Box::new(sock) as Stream)
        })
    })
}

/// A `Dial` through the last SSH server of `route`, which it keeps alive.
/// Each connection is a `direct-tcpip` channel, always drained (see
/// `tunnel::drained_stream`), so a transfer that stalls holds up nothing else
/// on that SSH connection.
pub fn dial_through(route: Arc<Vec<russh::client::Handle<crate::ssh_manager::ClientHandler>>>, host: &str) -> Dial {
    let host = host.trim().to_string();
    Arc::new(move |port| {
        let (route, host) = (Arc::clone(&route), host.clone());
        Box::pin(async move {
            let address = crate::telnet::address(&host, port);
            let last = route.last().ok_or("no SSH server to go through")?;
            let channel = tokio::time::timeout(
                CONNECT_TIME,
                last.channel_open_direct_tcpip(host.clone(), u32::from(port), "127.0.0.1".to_string(), 0),
            )
            .await
            .map_err(|_| format!("The SSH server didn't answer within 15 seconds when asked to reach {address}"))?
            .map_err(|e| format!("The SSH server couldn't reach {address}: {e}"))?;
            Ok(Box::new(crate::tunnel::drained_stream(channel)) as Stream)
        })
    })
}

/// The proxy a server row says to go through, SOCKS5 or HTTP (CONNECT), as
/// for an SSH server. The control connection and every data connection go
/// through it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proxy {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyKind {
    Socks5,
    Http,
}

impl Proxy {
    /// The row's proxy (`proxy_type`, `proxy_host`, `proxy_port`), when it
    /// has one. A proxy without a host is a mistake to report, not "no
    /// proxy". The ports when none is saved are the SSH connect's.
    pub fn of_row(kind: Option<&str>, host: Option<&str>, port: Option<i64>) -> Result<Option<Proxy>, String> {
        let kind = match kind.map(str::trim) {
            Some("socks5") => ProxyKind::Socks5,
            Some("http") => ProxyKind::Http,
            _ => return Ok(None),
        };
        let name = kind.name();
        let host = host.map(str::trim).unwrap_or_default();
        if host.is_empty() {
            return Err(format!("The {name} proxy has no host"));
        }
        crate::telnet::check_host(host).map_err(|e| format!("The {name} proxy: {e}"))?;
        let saved = port.and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0);
        let port = saved.unwrap_or(match kind {
            ProxyKind::Socks5 => 1080,
            ProxyKind::Http => 8080,
        });
        Ok(Some(Proxy { kind, host: host.to_string(), port }))
    }

    /// "the SOCKS5 proxy host:port".
    pub fn describe(&self) -> String {
        format!("the {} proxy {}", self.kind.name(), crate::telnet::address(&self.host, self.port))
    }
}

impl ProxyKind {
    fn name(self) -> &'static str {
        match self {
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::Http => "HTTP",
        }
    }
}

/// A `Dial` through `proxy`: a connection to it for each connection to the
/// server, the data connections as well.
pub fn dial_proxy(proxy: Proxy, host: &str) -> Dial {
    let host = host.trim().to_string();
    Arc::new(move |port| {
        let (proxy, host) = (proxy.clone(), host.clone());
        Box::pin(async move {
            let address = crate::telnet::address(&host, port);
            let sock = match tokio::time::timeout(CONNECT_TIME, through(&proxy, &host, port)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(format!("Couldn't reach {address} through {}: {e}", proxy.describe())),
                Err(_) => return Err(format!("Couldn't reach {address} through {}: no answer within 15 seconds", proxy.describe())),
            };
            crate::tunnel::apply_tunnel_sockopts(&sock);
            let _ = sock.set_nodelay(true);
            Ok(Box::new(sock) as Stream)
        })
    })
}

/// A connection to `proxy`, asked to go on to `host:port`. The proxy's name
/// is looked up as any host's is (not with the blocking lookup of
/// `Socks5Stream::connect`).
async fn through(proxy: &Proxy, host: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    let mut tcp = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port)).await.map_err(|e| e.to_string())?;
    match proxy.kind {
        ProxyKind::Socks5 => tokio_socks::tcp::Socks5Stream::connect_with_socket(tcp, (host, port))
            .await
            .map(tokio_socks::tcp::Socks5Stream::into_inner)
            .map_err(|e| e.to_string()),
        ProxyKind::Http => {
            http_connect(&mut tcp, host, port).await?;
            Ok(tcp)
        }
    }
}

/// Asks an HTTP proxy for a tunnel to `host:port` and reads its answer a
/// byte at a time up to the blank line: an FTP server speaks first, and its
/// greeting can come in the same packet as the proxy's "200". A reader with
/// a buffer would take the greeting along and lose it.
async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, host: &str, port: u16) -> Result<(), String> {
    if !crate::http_connect_host_ok(host) {
        return Err(format!("{host:?} can't be asked of an HTTP proxy"));
    }
    let target = crate::telnet::address(host, port);
    io.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut head: Vec<u8> = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while !(head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n")) {
        if head.len() >= 8 * 1024 {
            return Err("the proxy's answer is too long".into());
        }
        if io.read(&mut byte).await.map_err(|e| e.to_string())? == 0 {
            return Err("the proxy closed the connection".into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let first = text.lines().next().unwrap_or_default().trim();
    let code = first.split_whitespace().nth(1).unwrap_or_default();
    if first.starts_with("HTTP/1.") && code.len() == 3 && code.starts_with('2') {
        Ok(())
    } else {
        let said: String = first.chars().filter(|c| !c.is_control()).take(120).collect();
        Err(format!("the proxy refused: {said}"))
    }
}

/// A stream that keeps a copy of the first bytes read from it (up to 1 KiB),
/// for `Control::open` to tell a plain FTP refusal from a broken TLS server.
struct FirstBytes {
    inner: Stream,
    seen: Arc<std::sync::Mutex<Vec<u8>>>,
    /// How much more is kept; nothing is copied once it is 0.
    left: usize,
}

impl FirstBytes {
    fn wrap(inner: Stream) -> (Stream, Arc<std::sync::Mutex<Vec<u8>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        (Box::new(FirstBytes { inner, seen: Arc::clone(&seen), left: 1024 }), seen)
    }
}

impl AsyncRead for FirstBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let (std::task::Poll::Ready(Ok(())), true) = (&polled, self.left > 0) {
            let new = &buf.filled()[before..];
            let n = new.len().min(self.left);
            if let Ok(mut seen) = self.seen.lock() {
                seen.extend_from_slice(&new[..n]);
            }
            self.left -= n;
        }
        polled
    }
}

impl AsyncWrite for FirstBytes {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, data: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A stand-in stream while the real one is being upgraded to TLS.
mod empty {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    pub struct EmptyDuplex;

    impl AsyncRead for EmptyDuplex {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for EmptyDuplex {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::ErrorKind::NotConnected.into()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(s: &str) -> Reply {
        parse_reply(s.as_bytes()).unwrap().unwrap().0
    }

    #[test]
    fn answers_one_line_and_several() {
        let r = reply("220 Welcome\r\n");
        assert_eq!((r.code, r.lines.clone()), (220, vec!["Welcome".to_string()]));
        let raw = "211-Features:\r\n MLST type*;size*;modify*;\r\n UTF8\r\n EPSV\r\n REST STREAM\r\n211 End\r\nrest";
        let (r, used) = parse_reply(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(&raw[used..], "rest");
        let f = parse_feat(&r);
        assert!(f.mlst && f.utf8 && f.epsv && f.rest && !f.mfmt);
        // A line inside that starts with another code isn't the end.
        let r = reply("250-Begin\r\n123 not the end\r\n250 End\r\n");
        assert_eq!(r.lines.len(), 3);
        // Not all there yet.
        assert!(parse_reply(b"250-Begin\r\n250 En").unwrap().is_none());
        // Bare LF line ends.
        assert_eq!(reply("200 ok\n").code, 200);
    }

    #[test]
    fn answers_that_arent_ftp_are_refused() {
        assert!(parse_reply(b"SSH-2.0-OpenSSH_9.6\r\n").is_err());
        assert!(parse_reply(b"HTTP/1.1 400 Bad\r\n").is_err());
        assert!(parse_reply(b"999 what\r\n").is_err());
        let long = format!("220-{}\r\n", "x".repeat(LINE_MAX + 10));
        assert!(parse_reply(long.as_bytes()).is_err());
        // Too long with no line end yet counts too.
        assert!(parse_reply("2".repeat(LINE_MAX + 10).as_bytes()).is_err());
    }

    #[test]
    fn messages_carry_no_control_characters() {
        let r = Reply { code: 550, lines: vec!["No\x1b[31m such\u{7}file".into()] };
        assert!(!r.said().chars().any(char::is_control));
    }

    #[test]
    fn line_breaks_never_reach_a_command() {
        assert!(check_arg("/a\r\nDELE /b").is_err());
        assert!(check_arg("/a\nb").is_err());
        assert!(check_arg("/a\0").is_err());
        assert!(check_arg("/a b/c'd\"e").is_ok());
    }

    #[test]
    fn passive_answers() {
        assert_eq!(parse_epsv(&reply("229 Entering Extended Passive Mode (|||6446|)\r\n")), Some(6446));
        assert_eq!(parse_epsv(&reply("229 ok (!!!21000!)\r\n")), Some(21000));
        assert_eq!(parse_epsv(&reply("229 ok (|||0|)\r\n")), None);
        assert_eq!(parse_epsv(&reply("229 ok (||1.2.3.4|5|)\r\n")), None);
        // The address in PASV is never used: only the port is read.
        assert_eq!(parse_pasv_port(&reply("227 Entering Passive Mode (127,0,0,1,4,1)\r\n")), Some(1025));
        assert_eq!(parse_pasv_port(&reply("227 =10,0,0,5,200,10\r\n")), Some(51210));
        assert_eq!(parse_pasv_port(&reply("227 Entering Passive Mode (1,2,3,4,256,1)\r\n")), None);
        assert_eq!(parse_pasv_port(&reply("227 Entering Passive Mode (1,2,3,4,0,0)\r\n")), None);
    }

    #[test]
    fn pwd_paths() {
        assert_eq!(parse_pwd("\"/home/ftp\" is the current directory").as_deref(), Some("/home/ftp"));
        assert_eq!(parse_pwd("\"/a \"\"b\"\"\" is current").as_deref(), Some("/a \"b\""));
        assert_eq!(parse_pwd("no quotes"), None);
    }

    #[test]
    fn mlsd_lines() {
        let e = parse_mlsx("type=file;size=1024;modify=20261113100500;UNIX.mode=0644; a b.txt", false).unwrap();
        assert_eq!(e, Entry { name: "a b.txt".into(), kind: Kind::File, size: Some(1024), mtime: Some(1794564300), mode: Some(0o644), exact: true });
        let d = parse_mlsx("Type=dir;Modify=20261113100500.123;Perm=flcdmpe; sub", false).unwrap();
        assert_eq!((d.kind, d.size), (Kind::Dir, Some(0)));
        assert!(parse_mlsx("type=cdir;modify=20261113100500; .", false).is_none());
        assert!(parse_mlsx("type=pdir; ..", false).is_none());
        let l = parse_mlsx("type=OS.unix=slink:/etc/x;size=6; link", false).unwrap();
        assert_eq!(l.kind, Kind::Link(Some("/etc/x".into())));
        let l = parse_mlsx("type=OS.unix=symlink;size=6; link", false).unwrap();
        assert_eq!(l.kind, Kind::Link(None));
        // MLST gives a path: the last part is the name.
        let m = parse_mlsx("type=file;size=3; /srv/ftp/x.txt", true).unwrap();
        assert_eq!(m.name, "x.txt");
    }

    #[test]
    fn listings_drop_names_a_folder_cant_hold() {
        let out = b"type=file;size=1; ok\r\ntype=file;size=1; ../evil\r\ntype=file;size=1; a/b\r\ntype=dir; .\r\n";
        let e = listing(out, |l| parse_mlsx(l, false));
        assert_eq!(e.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["ok"]);
        // A line that isn't UTF-8 is skipped, not mangled.
        let out = b"type=file;size=1; caf\xe9\r\ntype=file;size=1; caf\xc3\xa9\r\n";
        let e = listing(out, |l| parse_mlsx(l, false));
        assert_eq!(e.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["café"]);
    }

    #[test]
    fn unix_list_lines() {
        // 2026-11-13 12:00 UTC.
        let now = 1794571200;
        let e = parse_list_line("drwxr-xr-x    2 1000     1000         4096 Nov 13 10:05 my dir", now).unwrap();
        assert_eq!((e.name.as_str(), e.kind.clone(), e.mode), ("my dir", Kind::Dir, Some(0o755)));
        assert_eq!(e.mtime, Some(1794564300));
        let f = parse_list_line("-rw-r--r--   1 ftp ftp   1234 Jan  2  2024 two  spaces", now).unwrap();
        assert_eq!((f.name.as_str(), f.size, f.kind.clone()), ("two  spaces", Some(1234), Kind::File));
        assert_eq!(f.mtime, Some(1704153600));
        // No group column.
        let g = parse_list_line("-rw-r--r-- 1 owner 77 Mar 3 09:00 x", now).unwrap();
        assert_eq!((g.name.as_str(), g.size), ("x", Some(77)));
        let l = parse_list_line("lrwxrwxrwx 1 0 0 7 Nov 13 10:05 www -> /var/www", now).unwrap();
        assert_eq!((l.name.as_str(), l.kind.clone()), ("www", Kind::Link(Some("/var/www".into()))));
        let c = parse_list_line("crw-rw-rw- 1 0 0 1, 3 Nov 13 10:05 null", now).unwrap();
        assert_eq!(c.kind, Kind::Other);
        // A date ahead of now is last year's.
        let dec = parse_list_line("-rw-r--r-- 1 a b 1 Dec 30 10:00 old", now).unwrap();
        assert_eq!(dec.mtime, Some(1767088800));
        assert!(parse_list_line("total 12", now).is_none());
        // A month name in the name doesn't fool it.
        let m = parse_list_line("-rw-r--r-- 1 a b 5 Nov 13 10:05 Nov 13 10:05 x", now).unwrap();
        assert_eq!(m.name, "Nov 13 10:05 x");
    }

    #[test]
    fn windows_list_lines() {
        let d = parse_list_line("11-13-26  10:05AM       <DIR>          My Folder", 0).unwrap();
        assert_eq!((d.name.as_str(), d.kind.clone()), ("My Folder", Kind::Dir));
        assert_eq!(d.mtime, Some(1794564300));
        let f = parse_list_line("11-13-2026  01:05PM              1234 report.pdf", 0).unwrap();
        assert_eq!((f.name.as_str(), f.size), ("report.pdf", Some(1234)));
        // 13:05, three hours after the 10:05 above.
        assert_eq!(f.mtime, Some(1794564300 + 3 * 3600));
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("20261113100500"), Some(1794564300));
        assert_eq!(parse_time("20261113100500.999"), Some(1794564300));
        assert_eq!(parse_time("2026111310050"), None);
        assert_eq!(parse_time("20261340100500"), None);
    }

    /// vsftpd over its per-client limit answers a new connection on its
    /// implicit-TLS port with a plain "421": that is a refusal the tab can act
    /// on (fewer connections), not a broken TLS server.
    #[tokio::test]
    async fn a_plain_refusal_on_the_implicit_port_is_read_as_one() {
        use tokio::io::AsyncWriteExt as _;
        let dial: Dial = Arc::new(|_port| {
            Box::pin(async {
                let (ours, mut theirs) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let _ = theirs.write_all(b"421 There are too many connections from your internet address.\r\n").await;
                    let _ = theirs.shutdown().await;
                    // Read whatever the client sends, then go.
                    let mut sink = vec![0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut theirs, &mut sink).await;
                });
                Ok(Box::new(ours) as Stream)
            })
        });
        let tls = Tls::new("127.0.0.1").unwrap();
        match Control::open(dial, IMPLICIT_PORT, Security::Implicit, Some(tls)).await {
            Err(Error::Refused(r)) => assert_eq!(r.code, 421),
            Err(e) => panic!("expected the 421, got {e}"),
            Ok(_) => panic!("expected the 421, got a connection"),
        }
    }

    #[test]
    fn protocols() {
        assert_eq!(Security::of_protocol(Some("ftps")), Some(Security::Explicit));
        assert_eq!(Security::of_protocol(Some("ftps-implicit")).map(Security::default_port), Some(990));
        assert_eq!(Security::of_protocol(Some("ftp")).map(Security::encrypted), Some(false));
        assert_eq!(Security::of_protocol(Some("ssh")), None);
        assert_eq!(Security::of_protocol(None), None);
    }
    #[test]
    fn a_rows_proxy_is_read_as_the_ssh_connect_reads_it() {
        assert_eq!(Proxy::of_row(None, Some("p"), Some(1)), Ok(None));
        assert_eq!(Proxy::of_row(Some("none"), Some("p"), Some(1)), Ok(None));
        let socks = Proxy::of_row(Some("socks5"), Some(" proxy.lan "), None).unwrap().unwrap();
        assert_eq!(socks, Proxy { kind: ProxyKind::Socks5, host: "proxy.lan".into(), port: 1080 });
        assert_eq!(socks.describe(), "the SOCKS5 proxy proxy.lan:1080");
        let http = Proxy::of_row(Some("http"), Some("::1"), Some(3128)).unwrap().unwrap();
        assert_eq!((http.kind, http.port, http.describe().as_str()), (ProxyKind::Http, 3128, "the HTTP proxy [::1]:3128"));
        assert_eq!(Proxy::of_row(Some("http"), Some("p"), Some(0)).unwrap().unwrap().port, 8080);
        assert_eq!(Proxy::of_row(Some("http"), Some("p"), Some(70000)).unwrap().unwrap().port, 8080);
        assert_eq!(Proxy::of_row(Some("socks5"), Some("  "), Some(1080)), Err("The SOCKS5 proxy has no host".into()));
        assert!(Proxy::of_row(Some("socks5"), None, None).is_err());
        assert!(Proxy::of_row(Some("http"), Some("a b"), None).unwrap_err().starts_with("The HTTP proxy: "));
    }

    /// A pretend HTTP proxy at the other end of a pipe: reads the CONNECT
    /// request, then sends `answer` in one write and hangs up. Gives what
    /// `http_connect` made of it, the request, and what was left to read.
    async fn http_proxy_says(answer: &'static [u8]) -> (Result<(), String>, String, Vec<u8>) {
        let (mut client, mut proxy) = tokio::io::duplex(4096);
        let side = tokio::spawn(async move {
            let mut asked = Vec::new();
            let mut b = [0u8; 1];
            while !asked.ends_with(b"\r\n\r\n") {
                if proxy.read(&mut b).await.unwrap() == 0 {
                    break;
                }
                asked.push(b[0]);
            }
            proxy.write_all(answer).await.unwrap();
            drop(proxy);
            String::from_utf8(asked).unwrap()
        });
        let res = http_connect(&mut client, "ftp.example", 21).await;
        let asked = side.await.unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        (res, asked, rest)
    }

    #[tokio::test]
    async fn the_server_greeting_that_comes_with_the_proxys_answer_is_kept() {
        let (res, asked, rest) = http_proxy_says(b"HTTP/1.1 200 Connection established\r\nProxy-Agent: x\r\n\r\n220 Welcome\r\n").await;
        assert_eq!(res, Ok(()));
        assert_eq!(asked, "CONNECT ftp.example:21 HTTP/1.1\r\nHost: ftp.example:21\r\n\r\n");
        assert_eq!(rest, b"220 Welcome\r\n");
        // A proxy that ends its lines with LF only.
        let (res, _, rest) = http_proxy_says(b"HTTP/1.0 200 OK\n\n220 Hi\r\n").await;
        assert_eq!((res, rest), (Ok(()), b"220 Hi\r\n".to_vec()));
    }

    #[tokio::test]
    async fn an_http_proxy_that_refuses_or_hangs_up_is_told() {
        let (res, _, _) = http_proxy_says(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        assert_eq!(res, Err("the proxy refused: HTTP/1.1 403 Forbidden".into()));
        let (res, _, _) = http_proxy_says(b"HTTP/1.1 200 OK\r\n").await;
        assert_eq!(res, Err("the proxy closed the connection".into()));
        let (res, _, _) = http_proxy_says(b"SSH-2.0-x\r\n\r\n").await;
        assert!(res.unwrap_err().starts_with("the proxy refused: SSH-2.0-x"));
        let (mut client, _proxy) = tokio::io::duplex(64);
        assert!(http_connect(&mut client, "a\r\nb", 21).await.unwrap_err().contains("can't be asked of an HTTP proxy"));
    }
}
