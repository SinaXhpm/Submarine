//! The built-in RDP client (#3, second step): an RDP session drawn in a
//! Submarine tab, on Devolutions' IronRDP, here in the backend.
//!
//! Why here and not in the webview (IronRDP also ships a WebAssembly client):
//! the login (NLA / CredSSP) and the check of the server's certificate stay
//! on this side, so the password never reaches the page. The page gets the
//! screen as the rectangles that changed (`frames`) and sends mouse and
//! keyboard back (`Cmd::Input`).
//!
//! The server's certificate is pinned the way an SSH host key is: the
//! caller is shown its SHA-256 fingerprint the first time, and warned when
//! it changes (`verify_cert`). RDP servers use self-signed certificates as a
//! rule, so no CA is asked; the TLS handshake still has to be signed with the
//! certificate's key, so a pinned certificate can't be replayed by someone
//! who doesn't hold that key.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD_NO_PAD as B64, Engine as _};
use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp::cliprdr::CliprdrClient;
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::{self, BitmapConfig, ClientConnector, ClientConnectorState, Credentials, DesktopSize};
use ironrdp::core::{IntoOwned as _, WriteBuf};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::nego::SecurityProtocol;
use ironrdp::pdu::rdp::capability_sets::{client_codecs_capabilities, BitmapCodecs, MajorPlatformType, RailSupportLevel};
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{single_sequence_step_read, split_tokio_framed, FramedWrite as _, TokioFramed};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_rustls::rustls;
use zeroize::Zeroizing;

/// How often the changed parts of the screen go to the page, at most.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Each step of connecting that waits on the server (not on the user): a
/// server that takes the connection and then says nothing doesn't keep the
/// tab, its socket and its SSH route forever.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Below this, a desktop is too small to be of use; above, too big to send.
const MIN_SIDE: u16 = 200;
const MAX_SIDE: u16 = 8192;
/// More changed rectangles than this before a frame goes out: one rectangle
/// around them all is sent instead.
const MAX_RECTS: usize = 32;
/// The most pixels a desktop the server picks may have (a 4K screen fits):
/// its picture is kept here, four bytes a pixel.
const MAX_PIXELS: u32 = 3840 * 2160;

/// A desktop size the server announced, checked before its picture is made:
/// the server picks it (up to 65535 on a side), and a failed allocation would
/// end the whole app, not only this tab.
fn desktop_fits(width: u16, height: u16) -> Result<(), String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || u32::from(width) * u32::from(height) > MAX_PIXELS {
        return Err(format!("The RDP server asked for a {width}×{height} desktop, which this client doesn't draw"));
    }
    Ok(())
}

/// What the page asks of a running session.
pub enum Cmd {
    Input(Vec<InputEvent>),
    Resize(u16, u16),
    /// Text from this computer's clipboard for the server's, then (when the
    /// user pressed a paste key in the tab) that key, once the server has it.
    Paste(Zeroizing<String>, PasteKeys),
    Close,
}

/// The paste key the page held back while the text went to the server's
/// clipboard; the server is sent it once the text is there (see `paste_keys`).
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PasteKeys {
    /// Only put the text on the server's clipboard (the tab's button).
    None,
    /// Ctrl+V.
    Ctrlv,
    /// Shift+Insert.
    Shiftinsert,
    /// Cmd+V on a Mac, which is Win+V to the server: the Windows key is let
    /// go and Ctrl+V sent instead.
    Cmdv,
}

/// How long a held-back paste key waits for the server to take the text.
const PASTE_WAIT: Duration = Duration::from_millis(1500);
/// How often what the shared files' backend finished later is sent on.
const DRIVE_POLL: Duration = Duration::from_millis(100);
/// The most characters a copy on the server brings to this computer.
const MAX_CLIP_CHARS: usize = 1_000_000;
/// The most bytes such a copy can take (4 a character at most, and an end):
/// a longer answer isn't copied out of the channel at all.
const MAX_CLIP_BYTES: usize = MAX_CLIP_CHARS * 4 + 4;

/// The keys that do the paste on the server, as operations on the session's
/// keyboard (`Database`; `held` says what is down there now). The modifier
/// goes with them, so a modifier the user already let go of doesn't turn the
/// paste into typing; one the user still holds stays down (pressed again, it
/// would be let go first, and the next key would come without it).
fn paste_keys(keys: PasteKeys, held: impl Fn(Scancode) -> bool) -> Vec<Operation> {
    let sc = |extended, code| Scancode::from_u8(extended, code);
    let (ctrl, shift, v, insert) = (sc(false, 0x1d), sc(false, 0x2a), sc(false, 0x2f), sc(true, 0x52));
    let press = |modifier, key| {
        if held(modifier) {
            vec![Operation::KeyPressed(key), Operation::KeyReleased(key)]
        } else {
            vec![Operation::KeyPressed(modifier), Operation::KeyPressed(key), Operation::KeyReleased(key), Operation::KeyReleased(modifier)]
        }
    };
    match keys {
        PasteKeys::None => Vec::new(),
        PasteKeys::Ctrlv => press(ctrl, v),
        PasteKeys::Shiftinsert => press(shift, insert),
        PasteKeys::Cmdv => {
            // Ctrl goes down before the Windows keys are let go: a Windows key
            // let go on its own opens the Start menu (GNOME's Activities).
            let ctrl_down = !held(ctrl);
            let mut ops = Vec::new();
            if ctrl_down {
                ops.push(Operation::KeyPressed(ctrl));
            }
            ops.extend([sc(true, 0x5b), sc(true, 0x5c)].into_iter().filter(|win| held(*win)).map(Operation::KeyReleased));
            ops.extend([Operation::KeyPressed(v), Operation::KeyReleased(v)]);
            if ctrl_down {
                ops.push(Operation::KeyReleased(ctrl));
            }
            ops
        }
    }
}

/// What the clipboard channel tells the session loop (`ClipBridge`).
#[derive(Debug)]
enum ClipEvent {
    /// The channel is up: this side says what it has to offer (nothing, until the user pastes).
    FormatListWanted,
    /// The server answered what this side offered.
    Offered(bool),
    /// The user copied something on the server; whether it has text.
    RemoteCopy(Option<ClipboardFormatId>),
    /// The server wants this side's text, in this format.
    Wanted(ClipboardFormatId),
    /// The text the server copied, as asked for (none: it was gone).
    RemoteText(Option<Vec<u8>>),
    /// The server's answer was longer than MAX_CLIP_BYTES (this many bytes).
    RemoteTooLong(usize),
}

/// The clipboard channel's backend: it passes what the server does on to the
/// session loop, which answers through the channel. Text only: no file list
/// is ever offered, so the server never asks for file contents.
#[derive(Debug)]
struct ClipBridge(mpsc::UnboundedSender<ClipEvent>);

ironrdp::core::impl_as_any!(ClipBridge);

impl CliprdrBackend for ClipBridge {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        let _ = self.0.send(ClipEvent::FormatListWanted);
    }

    fn on_format_list_response(&mut self, ok: bool) {
        let _ = self.0.send(ClipEvent::Offered(ok));
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let _ = self.0.send(ClipEvent::RemoteCopy(text_format(available_formats)));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let _ = self.0.send(ClipEvent::Wanted(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let event = if response.is_error() {
            ClipEvent::RemoteText(None)
        } else if response.data().len() > MAX_CLIP_BYTES {
            ClipEvent::RemoteTooLong(response.data().len())
        } else {
            ClipEvent::RemoteText(Some(response.data().to_vec()))
        };
        let _ = self.0.send(event);
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// The text format to ask for from what the server offers: Unicode, else ANSI.
fn text_format(formats: &[ClipboardFormat]) -> Option<ClipboardFormatId> {
    [ClipboardFormatId::CF_UNICODETEXT, ClipboardFormatId::CF_TEXT]
        .into_iter()
        .find(|id| formats.iter().any(|f| f.id == *id))
}

/// The server's text in `format`, up to its first NUL; none when it is too
/// long to bring over.
fn decode_clip_text(format: ClipboardFormatId, data: &[u8]) -> Result<String, String> {
    let text = if format == ClipboardFormatId::CF_UNICODETEXT {
        let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
        String::from_utf16_lossy(&units)
    } else {
        let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
        String::from_utf8_lossy(&data[..end]).into_owned()
    };
    let n = text.chars().count();
    if n > MAX_CLIP_CHARS {
        return Err(format!("The server's copy is too long to bring over ({n} characters; the most is {MAX_CLIP_CHARS})."));
    }
    Ok(text)
}

/// A clipboard step's frames, ready to send; a failure ends nothing but the
/// step (the session goes on without it).
fn clip_step(
    stage: &mut ActiveStage,
    step: impl FnOnce(&mut CliprdrClient) -> ironrdp::pdu::PduResult<ironrdp::cliprdr::CliprdrSvcMessages<ironrdp::cliprdr::Client>>,
) -> Result<Vec<ActiveStageOutput>, String> {
    let Some(channel) = stage.get_svc_processor_mut::<CliprdrClient>() else {
        return Ok(Vec::new());
    };
    let messages = step(channel).map_err(|e| format!("The shared clipboard: {e}"))?;
    let frame = stage.process_svc_processor_messages(messages).map_err(|e| format!("The shared clipboard: {e}"))?;
    Ok(if frame.is_empty() { Vec::new() } else { vec![ActiveStageOutput::ResponseFrame(frame)] })
}

/// What the shared files' backend finished since the last look, ready to send.
fn drive_deferred(stage: &mut ActiveStage) -> Result<Vec<ActiveStageOutput>, String> {
    let Some(channel) = stage.get_svc_processor_mut::<ironrdp::rdpdr::Rdpdr>() else {
        return Ok(Vec::new());
    };
    let messages = channel.poll_deferred_messages().map_err(|e| format!("The shared files: {e}"))?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    let frame = stage
        .process_svc_messages_by_name(&ironrdp::rdpdr::Rdpdr::NAME, messages)
        .map_err(|e| format!("The shared files: {e}"))?;
    Ok(vec![ActiveStageOutput::ResponseFrame(frame)])
}

/// Tells the server what this side's clipboard has: text, or nothing.
fn offer_text(stage: &mut ActiveStage, has_text: bool) -> Result<Vec<ActiveStageOutput>, String> {
    let formats: &[ClipboardFormat] = if has_text { &[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)] } else { &[] };
    clip_step(stage, |c| c.initiate_copy(formats))
}

/// The clipboard channel's next event; never, without the channel.
async fn next_clip_event(events: &mut Option<mpsc::UnboundedReceiver<ClipEvent>>) -> Option<ClipEvent> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_due(due: Option<tokio::time::Instant>) {
    match due {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Mouse and keyboard from the page (TerminalView's RDP sibling, RdpCanvas).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum InputEvent {
    /// A key by its scancode (set 1); bit 0x100 marks an extended (E0) key.
    Key { code: u16, down: bool },
    /// Text the page can't map to a key (a phone's keyboard, IME).
    Text { ch: String, down: bool },
    Move { x: u16, y: u16 },
    /// 0 left, 1 middle, 2 right, 3 back, 4 forward.
    Button { b: u8, down: bool },
    Wheel { vertical: bool, units: i16 },
    /// The page lost the keyboard: nothing may stay pressed on the server.
    ReleaseAll,
}

/// How the refusal of a server without NLA starts; the RDP tab (rdpOptions.ts,
/// `RDP_NO_NLA_PREFIX`) knows it by this and offers the one-click opt-in.
pub const NO_NLA: &str = "This server doesn't use NLA";

/// Whether to go on with the way of logging in the server picked. NLA makes
/// the server prove it knows the password before the password is sent; without
/// it, the password goes inside TLS to whoever answered: a server that was
/// never asked to prove anything, or someone in the middle who answered
/// `PROTOCOL_SSL` to the offer of both. That is for the user to allow, per
/// server, not the default.
fn nla_gate(selected: SecurityProtocol, allow_without_nla: bool) -> Result<(), String> {
    if allow_without_nla || selected.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
        return Ok(());
    }
    Err(format!(
        "{NO_NLA} (network level authentication), so it can't prove that it knows your password. Nothing was sent. If you trust this server and the network to it, allow servers without NLA for it and connect again."
    ))
}

/// Which of the server's desktop effects may be drawn (and sent over the link):
/// the "experience" choices of Remote Desktop Connection. Only a Windows server
/// acts on them; others (xrdp) draw what they draw.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effects {
    /// What this client always asked for: the wallpaper and visual styles,
    /// smooth fonts, no animations and no windows dragged with their contents.
    #[default]
    Standard,
    /// None of them but the picture itself: the least to send on a slow link.
    Minimal,
    /// All of them, for a fast link.
    Full,
}

/// How fast the link is said to be. The server may send less detail on a slower one.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Lan,
    Broadband,
    Slow,
}

/// What the page asks of the picture for one server (the "Speed and quality"
/// choices of the tab, `rdpOptions.ts`). The default is what this client asked
/// for before they existed, byte for byte (`connector_config` is tested for it).
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Graphics {
    /// Bits a pixel: 32, 24 or 16. Less is less to send, and gradients band.
    pub colors: u8,
    pub effects: Effects,
    pub network: Network,
    /// Offer RemoteFX, a picture codec of Windows servers; it is used only if the server has it.
    pub remote_fx: bool,
}

impl Default for Graphics {
    fn default() -> Self {
        Self { colors: 32, effects: Effects::Standard, network: Network::Lan, remote_fx: false }
    }
}

impl Graphics {
    /// The choices as the page sent them, or why they can't be used.
    pub fn checked(self) -> Result<Self, String> {
        if matches!(self.colors, 16 | 24 | 32) {
            Ok(self)
        } else {
            Err(format!("{}-bit colour isn't one of 16, 24 and 32", self.colors))
        }
    }

    /// A one-line account for the tab's log, where the choices are not the standard ones.
    pub fn describe(&self) -> Option<String> {
        if *self == Self::default() {
            return None;
        }
        let effects = match self.effects {
            Effects::Standard => "standard desktop effects",
            Effects::Minimal => "minimal desktop effects",
            Effects::Full => "all desktop effects",
        };
        let network = match self.network {
            Network::Lan => "a fast network",
            Network::Broadband => "broadband",
            Network::Slow => "a slow network",
        };
        let mut parts = vec![format!("{}-bit colour", self.colors), effects.to_string(), network.to_string()];
        if self.remote_fx {
            parts.push("RemoteFX offered".to_string());
        }
        Some(format!("Speed and quality: {}.", parts.join(", ")))
    }

    fn connection_type(&self) -> ConnectionType {
        match self.network {
            Network::Lan => ConnectionType::Lan,
            Network::Broadband => ConnectionType::BroadbandHigh,
            Network::Slow => ConnectionType::BroadbandLow,
        }
    }

    fn performance_flags(&self) -> PerformanceFlags {
        match self.effects {
            Effects::Standard => PerformanceFlags::default(),
            Effects::Minimal => {
                PerformanceFlags::DISABLE_WALLPAPER
                    | PerformanceFlags::DISABLE_FULLWINDOWDRAG
                    | PerformanceFlags::DISABLE_MENUANIMATIONS
                    | PerformanceFlags::DISABLE_THEMING
                    | PerformanceFlags::DISABLE_CURSOR_SHADOW
                    | PerformanceFlags::DISABLE_CURSORSETTINGS
            }
            Effects::Full => PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION,
        }
    }

    /// The picture settings of the connection; none (the connector's own
    /// defaults: 32 bits, no codecs, lossless) where nothing was asked for.
    fn bitmap(&self) -> Option<BitmapConfig> {
        // On a link that isn't fast, the server may lower the picture's detail.
        let lossy = self.network != Network::Lan;
        if self.colors == 32 && !lossy && !self.remote_fx {
            return None;
        }
        let codecs = if self.remote_fx {
            client_codecs_capabilities(&[]).unwrap_or(BitmapCodecs(Vec::new()))
        } else {
            BitmapCodecs(Vec::new())
        };
        Some(BitmapConfig { lossy_compression: lossy, color_depth: u32::from(self.colors), codecs })
    }
}

/// Who to log in as, and the first size of the desktop.
pub struct Login {
    /// The server's name as the user gave it (TLS name and CredSSP target).
    pub host: String,
    pub username: String,
    pub domain: Option<String>,
    pub password: Zeroizing<String>,
    pub width: u16,
    pub height: u16,
    /// This server may be logged in to without NLA (see `nla_gate`).
    pub allow_no_nla: bool,
    pub graphics: Graphics,
    /// Share the clipboard with the server (text, both ways).
    pub clipboard: bool,
    /// Files shared with the server: their channel (rdp_share.rs).
    pub drive: Option<ironrdp::rdpdr::Rdpdr>,
}

/// "DOMAIN\user" or "user@domain" as the login name and domain NLA wants.
pub fn split_domain(user: &str) -> (String, Option<String>) {
    if let Some((domain, name)) = user.split_once('\\') {
        if !domain.is_empty() && !name.is_empty() {
            return (name.to_string(), Some(domain.to_string()));
        }
    }
    (user.to_string(), None)
}

/// "SHA256:<base64>" of a certificate, as SSH writes a host key's.
pub fn cert_fingerprint(der: &[u8]) -> String {
    format!("SHA256:{}", B64.encode(Sha256::digest(der)))
}

/// A size the server and the page can both work with.
pub fn clamp_size(width: u16, height: u16) -> (u16, u16) {
    let mut w = width.clamp(MIN_SIDE, MAX_SIDE);
    let mut h = height.clamp(MIN_SIDE, MAX_SIDE);
    // A tab on a 5K or 8K screen asks for no more than `desktop_fits` takes,
    // keeping its shape: the desktop is drawn scaled up to fill it.
    let pixels = u32::from(w) * u32::from(h);
    if pixels > MAX_PIXELS {
        let k = (f64::from(MAX_PIXELS) / f64::from(pixels)).sqrt();
        w = ((f64::from(w) * k) as u16).max(MIN_SIDE);
        h = ((f64::from(h) * k) as u16).max(MIN_SIDE);
    }
    // RDP wants an even width on some servers (and 4-aligned on old ones).
    (w & !3, h)
}

/// The TLS check: any certificate is taken here (RDP servers sign their own),
/// and pinned afterwards by the caller; the handshake's signatures are still
/// checked against it, so whoever answers holds the certificate's key.
#[derive(Debug)]
struct PinLater(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for PinLater {
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

/// NLA here is NTLM, which needs no network of its own. Kerberos would (a
/// KDC); it isn't offered.
struct NoNetwork;

impl ironrdp_tokio::NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _request: &ironrdp::connector::sspi::generator::NetworkRequest,
    ) -> connector::ConnectorResult<Vec<u8>> {
        Err(connector::general_err!("Kerberos isn't supported: log in with a password"))
    }
}

fn connector_config(login: &Login) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword {
            username: login.username.clone(),
            password: login.password.to_string(),
        },
        domain: login.domain.clone(),
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: login.graphics.connection_type(),
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize { width: login.width, height: login.height },
        monitor_layout: None,
        bitmap: login.graphics.bitmap(),
        client_build: 0,
        client_name: "Submarine".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "android")]
        platform: MajorPlatformType::ANDROID,
        #[cfg(not(any(windows, target_os = "macos", target_os = "android")))]
        platform: MajorPlatformType::UNIX,
        // The page draws the local pointer; the server's isn't sent.
        enable_server_pointer: false,
        pointer_software_rendering: true,
        request_data: None,
        // The login goes in the logon info too (inside TLS: standard RDP
        // security is off), as mstsc does with a saved password: a server
        // without NLA (xrdp, by default) logs in with it instead of showing
        // its own login screen.
        autologon: true,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: None,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
        performance_flags: login.graphics.performance_flags(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
    }
}

/// A frame for the page: the desktop's new size.
fn size_frame(w: u16, h: u16) -> Vec<u8> {
    let mut f = Vec::with_capacity(5);
    f.push(1u8);
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f
}

/// A frame for the page: one rectangle of the desktop, RGBA rows.
fn rect_frame(image: &DecodedImage, r: &InclusiveRectangle) -> Option<Vec<u8>> {
    let (iw, ih) = (image.width(), image.height());
    let left = r.left.min(iw.saturating_sub(1));
    let top = r.top.min(ih.saturating_sub(1));
    let right = r.right.min(iw.saturating_sub(1));
    let bottom = r.bottom.min(ih.saturating_sub(1));
    if right < left || bottom < top {
        return None;
    }
    let (w, h) = (right - left + 1, bottom - top + 1);
    let stride = image.stride();
    let data = image.data();
    let mut f = Vec::with_capacity(9 + usize::from(w) * usize::from(h) * 4);
    f.push(2u8);
    for v in [left, top, w, h] {
        f.extend_from_slice(&v.to_le_bytes());
    }
    for y in top..=bottom {
        let start = usize::from(y) * stride + usize::from(left) * 4;
        let row = data.get(start..start + usize::from(w) * 4)?;
        // Opaque: the alpha byte isn't always set by the decoders.
        f.extend(row.chunks_exact(4).flat_map(|px| [px[0], px[1], px[2], 255]));
    }
    Some(f)
}

/// The changed places as they go out: many of them, or together more than the
/// picture (the same place changing again and again), become one rectangle around
/// them, so a batch is never more than about one picture.
fn tidy(rects: Vec<InclusiveRectangle>, picture_area: u64) -> Vec<InclusiveRectangle> {
    let area: u64 = rects
        .iter()
        .map(|r| (u64::from(r.right.saturating_sub(r.left)) + 1) * (u64::from(r.bottom.saturating_sub(r.top)) + 1))
        .sum();
    if rects.len() > MAX_RECTS || area > picture_area {
        if let Some(first) = rects.first() {
            return vec![rects.iter().fold(first.clone(), |acc, r| union(&acc, r))];
        }
    }
    rects
}

fn union(a: &InclusiveRectangle, b: &InclusiveRectangle) -> InclusiveRectangle {
    InclusiveRectangle {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

fn operations(events: Vec<InputEvent>) -> Vec<Operation> {
    let mut ops = Vec::new();
    for e in events {
        match e {
            InputEvent::Key { code, down } => {
                let sc = Scancode::from_u8(code & 0x100 != 0, (code & 0xff) as u8);
                ops.push(if down { Operation::KeyPressed(sc) } else { Operation::KeyReleased(sc) });
            }
            InputEvent::Text { ch, down } => {
                for c in ch.chars() {
                    ops.push(if down { Operation::UnicodeKeyPressed(c) } else { Operation::UnicodeKeyReleased(c) });
                }
            }
            InputEvent::Move { x, y } => ops.push(Operation::MouseMove(MousePosition { x, y })),
            InputEvent::Button { b, down } => {
                let button = match b {
                    0 => MouseButton::Left,
                    1 => MouseButton::Middle,
                    2 => MouseButton::Right,
                    3 => MouseButton::X1,
                    4 => MouseButton::X2,
                    _ => continue,
                };
                ops.push(if down { Operation::MouseButtonPressed(button) } else { Operation::MouseButtonReleased(button) });
            }
            InputEvent::Wheel { vertical, units } => {
                ops.push(Operation::WheelRotations(WheelRotations { is_vertical: vertical, rotation_units: units }))
            }
            // The caller does it, from the database's own list of what's down.
            InputEvent::ReleaseAll => {}
        }
    }
    ops
}

/// Connects over `stream` (a TCP socket, or a channel through an SSH
/// server), logs in, and runs the session until it ends or `Cmd::Close`.
/// `verify_cert` gets the server's certificate (DER) before any password is
/// sent and says whether to go on. `connected` runs once the desktop is
/// there, told whether the login was NLA (then the server took the password;
/// without NLA it shows its own login screen whatever was sent); `frame`
/// sends one frame to the page and says whether the page is still
/// listening; `remote_copy` gets the text the user copied on the server
/// (with the clipboard shared). Returns why the session ended.
#[allow(clippy::too_many_arguments)]
pub async fn run<S, V, VF>(
    stream: S,
    client_addr: SocketAddr,
    mut login: Login,
    verify_cert: V,
    connected: impl FnOnce(u16, u16, bool),
    notice: impl Fn(&str),
    frame: impl Fn(Vec<u8>) -> bool,
    remote_copy: impl Fn(String),
    flow: Arc<crate::screen_flow::Flow>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    V: FnOnce(Vec<u8>) -> VF,
    VF: std::future::Future<Output = bool>,
{
    let host = login.host.clone();
    let mut framed = TokioFramed::new(stream);
    let drdynvc = DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
    let mut connector = ClientConnector::new(connector_config(&login), client_addr).with_static_channel(drdynvc);
    // The shared clipboard is a channel of its own, only there when it's wanted.
    let mut clip_events = None;
    if login.clipboard {
        let (tx, rx) = mpsc::unbounded_channel();
        connector = connector.with_static_channel(CliprdrClient::new(Box::new(ClipBridge(tx))));
        clip_events = Some(rx);
    }
    // Files shared with the server: a channel of its own too.
    let sharing = login.drive.is_some();
    if let Some(rdpdr) = login.drive.take() {
        connector = connector.with_static_channel(rdpdr);
    }
    let allow_no_nla = login.allow_no_nla;
    drop(login);

    let should_upgrade = tokio::time::timeout(STEP_TIMEOUT, ironrdp_tokio::connect_begin(&mut framed, &mut connector))
        .await
        .map_err(|_| "The RDP server didn't answer within 30 seconds".to_string())?
        .map_err(|e| format!("The RDP server didn't answer as one: {e}"))?;
    // How the server wants the login done is known now, before TLS and before
    // anything secret goes out: NLA, or the user's say-so, or no login at all.
    let nla = match &connector.state {
        ClientConnectorState::EnhancedSecurityUpgrade { selected_protocol } => {
            nla_gate(*selected_protocol, allow_no_nla)?;
            let nla = selected_protocol.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX);
            if !nla {
                notice("This server doesn't use NLA; the password goes inside TLS only (allowed for this server).");
            }
            nla
        }
        _ => return Err("The RDP server picked a way of logging in that this client doesn't offer".into()),
    };
    let (plain, leftover) = framed.into_inner();

    // TLS, then the certificate check, before anything secret is sent.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls_config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinLater(provider)))
        .with_no_client_auth();
    // CredSSP doesn't work over a resumed session.
    tls_config.resumption = rustls::client::Resumption::disabled();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .unwrap_or_else(|_| rustls::pki_types::ServerName::try_from("rdp-server").expect("a valid name"));
    let tls = tokio::time::timeout(STEP_TIMEOUT, tokio_rustls::TlsConnector::from(Arc::new(tls_config)).connect(server_name, plain))
        .await
        .map_err(|_| "TLS with the RDP server didn't finish within 30 seconds".to_string())?
        .map_err(|e| format!("TLS with the RDP server failed: {e}"))?;
    let cert = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or("The RDP server sent no certificate")?;
    let public_key = {
        use x509_cert::der::Decode as _;
        let parsed = x509_cert::Certificate::from_der(&cert).map_err(|e| format!("The RDP server's certificate can't be read: {e}"))?;
        parsed
            .tbs_certificate()
            .subject_public_key_info()
            .subject_public_key
            .as_bytes()
            .ok_or("The RDP server's certificate has no usable key")?
            .to_vec()
    };
    if !verify_cert(cert).await {
        return Err("The RDP server's certificate wasn't accepted".into());
    }
    // The tab was closed, disconnected or opened again meanwhile (its sender
    // is gone): no login for nobody. Queued input and resizes stay queued.
    if cmds.is_closed() {
        return Ok("closed".into());
    }

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new_with_leftover(tls, leftover);
    let result = tokio::time::timeout(
        STEP_TIMEOUT,
        ironrdp_tokio::connect_finalize(upgraded, connector, &mut framed, &mut NoNetwork, host.clone().into(), public_key, None),
    )
    .await
    .map_err(|_| "Logging in to the RDP server didn't finish within 30 seconds".to_string())?
    .map_err(|e| login_error(&e.to_string()))?;

    let size = result.desktop_size;
    desktop_fits(size.width, size.height)?;
    let mut image = DecodedImage::new(PixelFormat::RgbA32, size.width, size.height);
    let activation_factory = result.activation_factory;
    let mut stage: ActiveStage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    if !frame(size_frame(size.width, size.height)) {
        return Ok("closed".into());
    }
    connected(size.width, size.height, nla);

    let (mut reader, mut writer) = split_tokio_framed(framed);
    let mut db = Database::new();
    let mut dirty: Vec<InclusiveRectangle> = Vec::new();
    let mut tick = tokio::time::interval(FRAME_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut drive_tick = tokio::time::interval(DRIVE_POLL);
    drive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The shared clipboard: this side's text (offered to the server only once
    // the user pastes it there), the formats the server's copies were asked
    // for in (one answer comes for each, in order), and a paste key held back
    // until the server has the text.
    let mut local_text: Option<Zeroizing<String>> = None;
    let mut asked_formats: std::collections::VecDeque<ClipboardFormatId> = std::collections::VecDeque::new();
    let mut held_paste: Option<(PasteKeys, tokio::time::Instant)> = None;

    loop {
        let paste_due = held_paste.map(|(_, at)| at);
        let outputs = tokio::select! {
            Some(event) = next_clip_event(&mut clip_events) => match event {
                ClipEvent::FormatListWanted => offer_text(&mut stage, local_text.is_some()).unwrap_or_else(|e| {
                    notice(&e);
                    Vec::new()
                }),
                // The server has the text: the held-back paste key goes now.
                // Refused, it would paste what the server had before: it doesn't.
                ClipEvent::Offered(taken) => match held_paste.take() {
                    Some((keys, _)) if taken => {
                        let ops = paste_keys(keys, |k| db.is_key_pressed(k));
                        let fast = db.apply(ops).into_vec();
                        stage.process_fastpath_input(&mut image, &fast).map_err(|e| format!("RDP: {e}"))?
                    }
                    Some(_) => {
                        notice("The server didn't take this device's text, so nothing was pasted.");
                        Vec::new()
                    }
                    None => Vec::new(),
                },
                ClipEvent::RemoteCopy(Some(format)) => match clip_step(&mut stage, |c| c.initiate_paste(format)) {
                    Ok(outputs) => {
                        asked_formats.push_back(format);
                        outputs
                    }
                    Err(e) => {
                        notice(&e);
                        Vec::new()
                    }
                },
                ClipEvent::RemoteCopy(None) => Vec::new(),
                ClipEvent::RemoteText(None) => {
                    asked_formats.pop_front();
                    Vec::new()
                }
                ClipEvent::RemoteTooLong(bytes) => {
                    asked_formats.pop_front();
                    notice(&format!("The server's copy is too long to bring over ({bytes} bytes; the most is {MAX_CLIP_CHARS} characters)."));
                    Vec::new()
                }
                ClipEvent::Wanted(format) => {
                    let response = match local_text.as_deref() {
                        Some(text) if format == ClipboardFormatId::CF_UNICODETEXT => FormatDataResponse::new_unicode_string(text),
                        Some(text) if format == ClipboardFormatId::CF_TEXT => FormatDataResponse::new_string(text),
                        _ => FormatDataResponse::new_error(),
                    }
                    .into_owned();
                    clip_step(&mut stage, |c| c.submit_format_data(response)).unwrap_or_else(|e| {
                        notice(&e);
                        Vec::new()
                    })
                }
                ClipEvent::RemoteText(Some(data)) => {
                    let format = asked_formats.pop_front().unwrap_or(ClipboardFormatId::CF_UNICODETEXT);
                    match decode_clip_text(format, &data) {
                        Ok(text) if !text.is_empty() => remote_copy(text),
                        Ok(_) => {}
                        Err(e) => notice(&e),
                    }
                    Vec::new()
                }
            },
            // What the shared files' backend finished later: a lock granted,
            // a change in a folder the server watches.
            _ = drive_tick.tick(), if sharing => drive_deferred(&mut stage).unwrap_or_else(|e| {
                notice(&e);
                Vec::new()
            }),
            // The server didn't say it has the text in time: the key goes anyway.
            _ = sleep_until_due(paste_due), if paste_due.is_some() => {
                let keys = held_paste.take().map_or(PasteKeys::None, |(keys, _)| keys);
                let ops = paste_keys(keys, |k| db.is_key_pressed(k));
                let fast = db.apply(ops).into_vec();
                stage.process_fastpath_input(&mut image, &fast).map_err(|e| format!("RDP: {e}"))?
            }
            pdu = reader.read_pdu() => {
                let (action, payload) = pdu.map_err(|e| format!("The connection to the RDP server ended: {e}"))?;
                stage.process(&mut image, action, &payload).map_err(|e| format!("RDP: {e}"))?
            }
            cmd = cmds.recv() => match cmd {
                None | Some(Cmd::Close) => {
                    if let Ok(outs) = stage.graceful_shutdown() {
                        for o in outs {
                            if let ActiveStageOutput::ResponseFrame(b) = o {
                                let _ = writer.write_all(&b).await;
                            }
                        }
                    }
                    return Ok("closed".into());
                }
                Some(Cmd::Input(events)) => {
                    // In order: a release-all (the tab lost the keys) lets go
                    // of what was down before it, not of a key pressed after
                    // it in the same batch.
                    let mut fast = Vec::new();
                    let mut part = Vec::new();
                    for e in events {
                        if matches!(e, InputEvent::ReleaseAll) {
                            fast.extend(db.apply(operations(std::mem::take(&mut part))).into_vec());
                            fast.extend(db.release_all());
                        } else {
                            part.push(e);
                        }
                    }
                    fast.extend(db.apply(operations(part)).into_vec());
                    stage.process_fastpath_input(&mut image, &fast).map_err(|e| format!("RDP: {e}"))?
                }
                Some(Cmd::Paste(text, keys)) => {
                    local_text = Some(text);
                    if keys != PasteKeys::None {
                        held_paste = Some((keys, tokio::time::Instant::now() + PASTE_WAIT));
                    }
                    offer_text(&mut stage, true).unwrap_or_else(|e| {
                        notice(&e);
                        Vec::new()
                    })
                }
                Some(Cmd::Resize(w, h)) => {
                    let (w, h) = clamp_size(w, h);
                    if u32::from(w) != u32::from(image.width()) || u32::from(h) != u32::from(image.height()) {
                        if let Some(Ok(bytes)) = stage.encode_resize(u32::from(w), u32::from(h), None, None) {
                            writer.write_all(&bytes).await.map_err(|e| format!("The connection to the RDP server ended: {e}"))?;
                        }
                    }
                    Vec::new()
                }
            },
            // While the page is far behind on what it was sent, what changes
            // waits here as one rectangle at most (see screen_flow.rs).
            _ = tick.tick(), if !dirty.is_empty() && flow.has_room() => {
                let rects = tidy(std::mem::take(&mut dirty), u64::from(image.width()) * u64::from(image.height()));
                for r in &rects {
                    if let Some(f) = rect_frame(&image, r) {
                        if !frame(f) {
                            return Ok("closed".into());
                        }
                    }
                }
                Vec::new()
            }
            // The page has caught up: what it is owed goes now, not with the
            // next thing the server says.
            _ = flow.room(), if !dirty.is_empty() && !flow.has_room() => Vec::new(),
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(bytes) => {
                    writer.write_all(&bytes).await.map_err(|e| format!("The connection to the RDP server ended: {e}"))?;
                }
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    if dirty.len() >= MAX_RECTS {
                        let all = dirty.iter().fold(rect.clone(), |acc, r| union(&acc, r));
                        dirty.clear();
                        dirty.push(all);
                    } else {
                        dirty.push(rect);
                    }
                }
                ActiveStageOutput::Terminate(reason) => return Ok(format!("{reason}")),
                ActiveStageOutput::DeactivateAll => {
                    // The server starts the desktop over (a new size, mostly):
                    // the deactivation-reactivation sequence, then a new image.
                    let mut activation = activation_factory.create();
                    let mut buf = WriteBuf::new();
                    loop {
                        // A server that starts the desktop over and then says nothing
                        // would hold this loop for good, deaf to Close.
                        let written = tokio::time::timeout(STEP_TIMEOUT, single_sequence_step_read(&mut reader, &mut activation, &mut buf))
                            .await
                            .map_err(|_| "RDP: the server stopped answering while it restarted the desktop".to_string())?
                            .map_err(|e| format!("RDP: {e}"))?;
                        if written.size().is_some() {
                            writer.write_all(buf.filled()).await.map_err(|e| format!("The connection to the RDP server ended: {e}"))?;
                        }
                        if let ConnectionActivationState::Finalized {
                            desktop_size,
                            share_id,
                            enable_server_pointer,
                            pointer_software_rendering,
                            static_channel_chunk_size,
                            ..
                        } = activation.connection_activation_state()
                        {
                            desktop_fits(desktop_size.width, desktop_size.height)?;
                            image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
                            dirty.clear();
                            if !stage.reactivate(
                                activation.io_channel_id(),
                                activation.user_channel_id(),
                                share_id,
                                enable_server_pointer,
                                pointer_software_rendering,
                                static_channel_chunk_size,
                            ) {
                                return Err("RDP: the server started over with settings this client can't use".into());
                            }
                            if !frame(size_frame(desktop_size.width, desktop_size.height)) {
                                return Ok("closed".into());
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// A login failure in words a person can act on.
fn login_error(e: &str) -> String {
    let lower = e.to_lowercase();
    if lower.contains("logon") || lower.contains("credential") || lower.contains("password") || lower.contains("0xc000006d") || lower.contains("authentication") {
        format!("The RDP server didn't accept the user name or password ({e})")
    } else {
        format!("Couldn't log in to the RDP server: {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp::pdu::rdp::capability_sets::CodecProperty;

    #[test]
    fn domain_forms() {
        assert_eq!(split_domain("CORP\\alice"), ("alice".into(), Some("CORP".into())));
        assert_eq!(split_domain("alice"), ("alice".into(), None));
        assert_eq!(split_domain("alice@corp.example"), ("alice@corp.example".into(), None));
        assert_eq!(split_domain("\\alice"), ("\\alice".into(), None));
    }

    #[test]
    fn a_server_without_nla_is_refused_unless_it_was_allowed() {
        let err = nla_gate(SecurityProtocol::SSL, false).unwrap_err();
        assert!(err.starts_with(NO_NLA), "{err}");
        assert!(err.contains("Nothing was sent"), "{err}");
        // Allowed for this server: the old way, TLS and the password inside it.
        assert!(nla_gate(SecurityProtocol::SSL, true).is_ok());
        // A server that does NLA is never in the way, allowed or not.
        for nla in [SecurityProtocol::HYBRID, SecurityProtocol::HYBRID_EX, SecurityProtocol::SSL | SecurityProtocol::HYBRID] {
            assert!(nla_gate(nla, false).is_ok(), "{nla:?}");
            assert!(nla_gate(nla, true).is_ok(), "{nla:?}");
        }
        // Nothing selected is no NLA either.
        assert!(nla_gate(SecurityProtocol::empty(), false).is_err());
    }

    fn login_with(graphics: Graphics) -> Login {
        Login {
            host: "server".into(),
            username: "user".into(),
            domain: None,
            password: Zeroizing::new("secret".into()),
            width: 1280,
            height: 720,
            allow_no_nla: false,
            graphics,
            clipboard: false,
            drive: None,
        }
    }

    #[test]
    fn a_paste_key_brings_its_modifier_and_cmd_v_becomes_ctrl_v() {
        let codes_holding = |keys, held: &[(bool, u8)]| {
            paste_keys(keys, |s: Scancode| held.contains(&s.as_u8()))
                .into_iter()
                .map(|op| match op {
                    Operation::KeyPressed(s) => (true, s.as_u8()),
                    Operation::KeyReleased(s) => (false, s.as_u8()),
                    _ => panic!("only keys"),
                })
                .collect::<Vec<_>>()
        };
        let codes = |keys| codes_holding(keys, &[]);
        assert!(codes(PasteKeys::None).is_empty());
        // Ctrl down, V down, V up, Ctrl up.
        assert_eq!(codes(PasteKeys::Ctrlv), [(true, (false, 0x1d)), (true, (false, 0x2f)), (false, (false, 0x2f)), (false, (false, 0x1d))]);
        assert_eq!(codes(PasteKeys::Shiftinsert), [(true, (false, 0x2a)), (true, (true, 0x52)), (false, (true, 0x52)), (false, (false, 0x2a))]);
        // A modifier the user still holds stays down: only the key goes.
        assert_eq!(codes_holding(PasteKeys::Ctrlv, &[(false, 0x1d)]), [(true, (false, 0x2f)), (false, (false, 0x2f))]);
        assert_eq!(codes_holding(PasteKeys::Shiftinsert, &[(false, 0x2a)]), [(true, (true, 0x52)), (false, (true, 0x52))]);
        // A Mac's Cmd is the server's Windows key: Ctrl goes down first, then
        // the Windows key that is down is let go (never on its own), then V.
        assert_eq!(
            codes_holding(PasteKeys::Cmdv, &[(true, 0x5b)]),
            [(true, (false, 0x1d)), (false, (true, 0x5b)), (true, (false, 0x2f)), (false, (false, 0x2f)), (false, (false, 0x1d))]
        );
        assert_eq!(codes(PasteKeys::Cmdv), codes(PasteKeys::Ctrlv), "no Windows key down: just Ctrl+V");
        // What the page sends.
        for (word, keys) in [("none", PasteKeys::None), ("ctrlv", PasteKeys::Ctrlv), ("shiftinsert", PasteKeys::Shiftinsert), ("cmdv", PasteKeys::Cmdv)] {
            assert_eq!(serde_json::from_str::<PasteKeys>(&format!("\"{word}\"")).unwrap(), keys);
        }
        let ts = include_str!("../../src/util/rdpClipboard.ts");
        for word in ["\"ctrlv\"", "\"shiftinsert\"", "\"cmdv\"", "\"none\""] {
            assert!(ts.contains(word), "src/util/rdpClipboard.ts must name {word}, as rdp_client::PasteKeys does");
        }
    }

    #[test]
    fn the_text_of_a_copy_on_the_server() {
        let unicode = |s: &str| s.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        // UTF-16, up to the NUL the server ends it with.
        let mut data = unicode("héllo ✓ 世界");
        data.extend([0, 0, b'x', 0]);
        assert_eq!(decode_clip_text(ClipboardFormatId::CF_UNICODETEXT, &data).unwrap(), "héllo ✓ 世界");
        // An odd byte at the end is left out, not a panic.
        let mut odd = unicode("ab");
        odd.push(0x41);
        assert_eq!(decode_clip_text(ClipboardFormatId::CF_UNICODETEXT, &odd).unwrap(), "ab");
        assert_eq!(decode_clip_text(ClipboardFormatId::CF_TEXT, b"plain\0rest").unwrap(), "plain");
        assert_eq!(decode_clip_text(ClipboardFormatId::CF_TEXT, b"").unwrap(), "");
        // Too long to bring over: said so, not copied.
        let long = unicode(&"x".repeat(MAX_CLIP_CHARS + 1));
        assert!(decode_clip_text(ClipboardFormatId::CF_UNICODETEXT, &long).unwrap_err().contains("too long"));
        assert!(decode_clip_text(ClipboardFormatId::CF_UNICODETEXT, &unicode(&"x".repeat(MAX_CLIP_CHARS))).is_ok());
    }

    #[test]
    fn what_the_server_does_on_its_clipboard_reaches_the_session_loop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut bridge = ClipBridge(tx);
        // A copy with Unicode text is asked for as Unicode; ANSI only as ANSI; no text: nothing to ask for.
        let f = |id| ClipboardFormat::new(id);
        bridge.on_remote_copy(&[f(ClipboardFormatId::new(49_161)), f(ClipboardFormatId::CF_TEXT), f(ClipboardFormatId::CF_UNICODETEXT)]);
        bridge.on_remote_copy(&[f(ClipboardFormatId::CF_TEXT)]);
        bridge.on_remote_copy(&[f(ClipboardFormatId::new(8))]);
        bridge.on_request_format_list();
        bridge.on_format_list_response(true);
        bridge.on_format_data_request(FormatDataRequest { format: ClipboardFormatId::CF_UNICODETEXT });
        bridge.on_format_data_response(FormatDataResponse::new_unicode_string("hi"));
        bridge.on_format_data_response(FormatDataResponse::new_error());
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(format!("{e:?}"));
        }
        assert_eq!(got.len(), 8, "{got:?}");
        assert!(got[0].contains("RemoteCopy(Some") && got[0].contains("13"), "{}", got[0]);
        assert!(got[1].contains("RemoteCopy(Some") && got[1].contains("(1)"), "{}", got[1]);
        assert_eq!(got[2], "RemoteCopy(None)");
        assert_eq!(got[3], "FormatListWanted");
        assert_eq!(got[4], "Offered(true)");
        assert!(got[5].starts_with("Wanted("), "{}", got[5]);
        assert!(got[6].starts_with("RemoteText(Some("), "{}", got[6]);
        assert_eq!(got[7], "RemoteText(None)");
        // Files are never offered, so this side never stores any.
        assert_eq!(bridge.temporary_directory(), "");
        assert!(!bridge.client_capabilities().contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED));
    }

    // A server nobody changed the speed and quality of must be asked exactly
    // what it was asked before they existed.
    #[test]
    fn untouched_speed_and_quality_ask_what_the_client_always_asked() {
        let c = connector_config(&login_with(Graphics::default()));
        assert!(c.bitmap.is_none(), "no picture settings of its own");
        assert_eq!(c.performance_flags, PerformanceFlags::default());
        assert_eq!(c.connection_type, ConnectionType::Lan);
        assert_eq!(Graphics::default().describe(), None, "nothing to say about the standard ones");
        // What the page keeps for a server that never had them is the default too.
        assert_eq!(serde_json::from_str::<Graphics>("{}").unwrap(), Graphics::default());
        assert!(Graphics::default().checked().is_ok());
    }

    #[test]
    fn fewer_colours_and_a_slower_link_change_the_picture_settings() {
        let b = connector_config(&login_with(Graphics { colors: 16, ..Graphics::default() })).bitmap.expect("16 bits is a setting");
        assert_eq!((b.color_depth, b.lossy_compression, b.codecs.0.len()), (16, false, 0));
        let b = connector_config(&login_with(Graphics { colors: 24, ..Graphics::default() })).bitmap.unwrap();
        assert_eq!(b.color_depth, 24);
        // A link that isn't fast: the server may lower the detail, and is told how fast it is.
        for (network, kind) in [(Network::Broadband, ConnectionType::BroadbandHigh), (Network::Slow, ConnectionType::BroadbandLow)] {
            let c = connector_config(&login_with(Graphics { network, ..Graphics::default() }));
            assert_eq!(c.connection_type, kind);
            let b = c.bitmap.expect("a slower link is a setting");
            assert_eq!((b.color_depth, b.lossy_compression, b.codecs.0.len()), (32, true, 0));
        }
        assert_eq!(connector_config(&login_with(Graphics::default())).connection_type, ConnectionType::Lan);
    }

    #[test]
    fn remotefx_is_offered_only_when_asked_for() {
        let b = connector_config(&login_with(Graphics { remote_fx: true, ..Graphics::default() })).bitmap.expect("RemoteFX is a setting");
        assert_eq!((b.color_depth, b.lossy_compression), (32, false));
        assert!(!b.codecs.0.is_empty() && b.codecs.0.iter().all(|c| matches!(c.property, CodecProperty::RemoteFx(_))));
        // With the other choices it stays one codec (RemoteFX), nothing else is offered.
        let b = connector_config(&login_with(Graphics { colors: 16, network: Network::Slow, remote_fx: true, ..Graphics::default() })).bitmap.unwrap();
        assert_eq!((b.color_depth, b.lossy_compression, b.codecs.0.len()), (16, true, 1));
    }

    #[test]
    fn desktop_effects_map_to_the_servers_flags() {
        let flags = |effects| connector_config(&login_with(Graphics { effects, ..Graphics::default() })).performance_flags;
        assert_eq!(flags(Effects::Standard), PerformanceFlags::default());
        let minimal = flags(Effects::Minimal);
        for f in [
            PerformanceFlags::DISABLE_WALLPAPER,
            PerformanceFlags::DISABLE_THEMING,
            PerformanceFlags::DISABLE_MENUANIMATIONS,
            PerformanceFlags::DISABLE_FULLWINDOWDRAG,
        ] {
            assert!(minimal.contains(f), "{f:?}");
        }
        assert!(!minimal.intersects(PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION));
        let full = flags(Effects::Full);
        assert!(full.contains(PerformanceFlags::ENABLE_FONT_SMOOTHING | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION));
        assert!(!full.intersects(
            PerformanceFlags::DISABLE_WALLPAPER
                | PerformanceFlags::DISABLE_THEMING
                | PerformanceFlags::DISABLE_MENUANIMATIONS
                | PerformanceFlags::DISABLE_FULLWINDOWDRAG
        ));
    }

    #[test]
    fn the_page_sends_camel_case_and_a_depth_that_isnt_offered_is_refused() {
        let g: Graphics = serde_json::from_str(r#"{"colors":16,"effects":"minimal","network":"broadband","remoteFx":true}"#).unwrap();
        assert_eq!(g, Graphics { colors: 16, effects: Effects::Minimal, network: Network::Broadband, remote_fx: true });
        // A part left out is the standard one.
        assert_eq!(serde_json::from_str::<Graphics>(r#"{"colors":16}"#).unwrap(), Graphics { colors: 16, ..Graphics::default() });
        for colors in [0u8, 8, 15, 33, 255] {
            assert!(Graphics { colors, ..Graphics::default() }.checked().is_err(), "{colors}");
        }
        for colors in [16u8, 24, 32] {
            assert!(Graphics { colors, ..Graphics::default() }.checked().is_ok(), "{colors}");
        }
        // A word this client doesn't know is an error, not the standard.
        assert!(serde_json::from_str::<Graphics>(r#"{"effects":"wild"}"#).is_err());
        assert!(serde_json::from_str::<Graphics>(r#"{"network":"dialup"}"#).is_err());
        assert!(serde_json::from_str::<Graphics>(r#"{"colors":"16"}"#).is_err());
    }

    #[test]
    fn the_log_line_says_what_was_chosen() {
        let g = Graphics { colors: 16, effects: Effects::Minimal, network: Network::Slow, remote_fx: true };
        assert_eq!(
            g.describe().as_deref(),
            Some("Speed and quality: 16-bit colour, minimal desktop effects, a slow network, RemoteFX offered.")
        );
        assert_eq!(
            Graphics { effects: Effects::Full, ..Graphics::default() }.describe().as_deref(),
            Some("Speed and quality: 32-bit colour, all desktop effects, a fast network.")
        );
    }

    #[test]
    fn the_tab_and_the_backend_use_the_same_words() {
        let ts = include_str!("../../src/util/rdpOptions.ts");
        for word in ["\"minimal\"", "\"full\"", "\"standard\"", "\"broadband\"", "\"slow\"", "\"lan\"", "remoteFx"] {
            assert!(ts.contains(word), "src/util/rdpOptions.ts must use {word}, as rdp_client::Graphics does");
        }
    }

    #[test]
    fn the_tab_recognises_the_refusal_by_its_start() {
        let ts = include_str!("../../src/util/rdpOptions.ts");
        assert!(
            ts.contains(&format!("\"{NO_NLA}\"")),
            "src/util/rdpOptions.ts: RDP_NO_NLA_PREFIX must be {NO_NLA:?}, the start of the refusal"
        );
    }

    #[test]
    fn changed_places_that_add_up_to_more_than_the_picture_become_one() {
        let rect = |left, top, right, bottom| InclusiveRectangle { left, top, right, bottom };
        let all = rect(0, 0, 99, 99);
        // The whole picture thirty times in one tick: one frame of it.
        let out = tidy(vec![all.clone(); 30], 100 * 100);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].left, out[0].top, out[0].right, out[0].bottom), (0, 0, 99, 99));
        // Two small places that don't add up to the picture stay two frames.
        let out = tidy(vec![rect(0, 0, 9, 9), rect(50, 50, 59, 59)], 100 * 100);
        assert_eq!(out.len(), 2);
        // More places than the limit become one rectangle around them, as before.
        let many: Vec<_> = (0..40u16).map(|i| rect(i, i, i, i)).collect();
        let out = tidy(many, 1_000_000);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].left, out[0].top, out[0].right, out[0].bottom), (0, 0, 39, 39));
        assert!(tidy(Vec::new(), 10).is_empty());
    }

    #[test]
    fn sizes_are_kept_in_range() {
        assert_eq!(clamp_size(1366, 768), (1364, 768));
        assert_eq!(clamp_size(50, 50), (200, 200));
        assert_eq!(clamp_size(3840, 2160), (3840, 2160));
        // Bigger than 4K: scaled down to fit, the shape kept.
        let (w, h) = clamp_size(7680, 4320);
        assert!(u32::from(w) * u32::from(h) <= MAX_PIXELS && w % 4 == 0 && (f64::from(w) / f64::from(h) - 16.0 / 9.0).abs() < 0.01, "{w}x{h}");
        let (w, h) = clamp_size(20000, 20000);
        assert!(w == h - h % 4 && u32::from(w) * u32::from(h) <= MAX_PIXELS, "{w}x{h}");
        // What we ask for is always what we accept.
        for (a, b) in [(1366, 768), (7680, 4320), (20000, 20000), (50, 9000)] {
            let (w, h) = clamp_size(a, b);
            assert!(desktop_fits(w, h).is_ok(), "{a}x{b} -> {w}x{h}");
        }
    }

    // The server picks the desktop's size: anything that can't be drawn is
    // refused before its picture is made (65535 x 65535 would be 17 GB).
    #[test]
    fn a_desktop_the_server_picks_is_checked() {
        assert!(desktop_fits(1920, 1080).is_ok());
        assert!(desktop_fits(3840, 2160).is_ok());
        assert!(desktop_fits(0, 768).is_err());
        assert!(desktop_fits(65535, 65535).is_err());
        assert!(desktop_fits(8192, 8192).is_err());
        assert!(desktop_fits(8193, 100).is_err());
    }

    #[test]
    fn fingerprints_look_like_ssh_ones() {
        let f = cert_fingerprint(b"cert");
        assert!(f.starts_with("SHA256:") && !f.ends_with('='));
    }

    #[test]
    fn a_rectangle_frame_is_opaque_rgba_rows() {
        let mut image = DecodedImage::new(PixelFormat::RgbA32, 4, 2);
        let f = rect_frame(&image, &InclusiveRectangle { left: 1, top: 0, right: 2, bottom: 1 }).unwrap();
        assert_eq!(f[0], 2);
        assert_eq!(u16::from_le_bytes([f[5], f[6]]), 2, "width");
        assert_eq!(u16::from_le_bytes([f[7], f[8]]), 2, "height");
        assert_eq!(f.len(), 9 + 2 * 2 * 4);
        assert!(f[9..].chunks(4).all(|px| px[3] == 255));
        // Out of the image: cut to it, or nothing.
        assert!(rect_frame(&image, &InclusiveRectangle { left: 9, top: 9, right: 9, bottom: 9 }).is_some());
        let _ = &mut image;
    }

    #[test]
    fn input_maps_to_operations() {
        let ops = operations(
            vec![
                InputEvent::Key { code: 0x1e, down: true },
                InputEvent::Key { code: 0x148, down: false },
                InputEvent::Button { b: 2, down: true },
                InputEvent::Button { b: 9, down: true },
                InputEvent::Text { ch: "é".into(), down: true },
            ],
        );
        assert_eq!(ops.len(), 4);
    }
}
