//! VNC servers: a saved node with `protocol = 'vnc'` shows the server's screen
//! in its tab (RemoteScreen.tsx: drawn to fit, mouse, keyboard, touch and
//! zoom on a phone), straight to it or through an SSH server (a direct-tcpip
//! channel, no local port). Plain VNC isn't encrypted: its password check is a
//! DES challenge, and the screen and the keys go as they are. The form and the
//! tab say so; through an SSH server it's private up to that server.
//!
//! The protocol is vnc-rs's (RFC 6143): Tight (without JPEG, which is only
//! sent when asked for), ZRLE, TRLE, CopyRect and Raw; the desktop's size from
//! the server and, where it takes one, from here (ExtendedDesktopSize). Its
//! logins are none and the VNC password; a server that only offers others
//! (RealVNC's own, macOS's) is told apart in the error. This side keeps the
//! picture as RGBA and sends the parts that changed, ~60 times a second at
//! most, as frames the page draws: `[1, w, h]` when the size changes,
//! `[2, x, y, w, h, RGBA rows]` for a part (little-endian u16s after the tag).

use std::collections::HashMap;
use std::future::Future;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use ::vnc::{ClientKeyEvent, ClientMouseEvent, PixelFormat, Rect, VncConnector, VncEncoding, VncError, VncEvent, X11Event};
use zeroize::Zeroizing;

/// The port a VNC server listens on unless told otherwise (display :0).
pub const DEFAULT_PORT: u16 = 5900;
/// The sizes a desktop may be asked for.
const MIN_SIDE: u16 = 200;
const MAX_SIDE: u16 = 8192;
/// More changed rectangles than this before a frame goes out: one rectangle
/// around them all is sent instead.
const MAX_RECTS: usize = 32;
/// Wheel units (the page's, 120 to a notch) for one click of a wheel button.
const WHEEL_NOTCH: i32 = 120;

/// The running sessions, by tab: where the page's input and sizes go.
pub type VncClients = std::sync::Arc<tokio::sync::Mutex<HashMap<String, mpsc::UnboundedSender<Cmd>>>>;

/// What the page asks of a running session.
pub enum Cmd {
    Input(Vec<InputEvent>),
    Resize(u16, u16),
    Close,
}

/// Mouse and keyboard from the page (RemoteScreen.tsx).
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum InputEvent {
    /// A key: `key` is the browser's `KeyboardEvent.key`, `code` its
    /// scancode (which physical key, to release what that key pressed).
    Key { code: u16, key: String, down: bool },
    /// Text the page can't map to a key (a phone's keyboard, IME).
    Text { ch: String, down: bool },
    Move { x: u16, y: u16 },
    /// 0 left, 1 middle, 2 right, 3 back, 4 forward.
    Button { b: u8, down: bool },
    Wheel { vertical: bool, units: i16 },
    /// The page lost the keyboard: nothing may stay pressed on the server.
    ReleaseAll,
}

pub fn clamp_size(width: u16, height: u16) -> (u16, u16) {
    (width.clamp(MIN_SIDE, MAX_SIDE), height.clamp(MIN_SIDE, MAX_SIDE))
}

/// The X11 keysym of a browser key name (`KeyboardEvent.key`), as VNC sends
/// keys: printable characters by themselves (Latin-1 as is, the rest as
/// Unicode keysyms), the others by name.
pub fn keysym(key: &str) -> Option<u32> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return char_keysym(c);
    }
    Some(match key {
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "Enter" => 0xff0d,
        "Pause" => 0xff13,
        "ScrollLock" => 0xff14,
        "Escape" => 0xff1b,
        "Home" => 0xff50,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "End" => 0xff57,
        "PrintScreen" => 0xff61,
        "Insert" => 0xff63,
        "ContextMenu" => 0xff67,
        "NumLock" => 0xff7f,
        "Delete" => 0xffff,
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "CapsLock" => 0xffe5,
        "Meta" | "OS" => 0xffeb,
        "Alt" => 0xffe9,
        "AltGraph" => 0xfe03,
        _ => {
            let n: u32 = key.strip_prefix('F')?.parse().ok().filter(|n| (1..=24).contains(n))?;
            0xffbe + n - 1
        }
    })
}

fn char_keysym(c: char) -> Option<u32> {
    let cp = c as u32;
    match cp {
        0x20..=0x7e | 0xa0..=0xff => Some(cp),
        // Controls have no key of their own here.
        0x00..=0x1f | 0x7f..=0x9f => None,
        _ => Some(0x0100_0000 | cp),
    }
}

/// The right-hand modifiers, by scancode, which VNC tells apart.
fn right_hand(code: u16, sym: u32) -> u32 {
    match (code, sym) {
        (0x36, 0xffe1) => 0xffe2,
        (0x11d, 0xffe3) => 0xffe4,
        (0x138, 0xffe9) => 0xffea,
        (0x15c, 0xffeb) => 0xffec,
        _ => sym,
    }
}

/// The keyboard and mouse as the server last heard them: what's pressed (so
/// it can all be let go), where the pointer is, the wheel's remainder.
#[derive(Default)]
struct Pointer {
    x: u16,
    y: u16,
    buttons: u8,
    /// Keysyms pressed, by the key (scancode, or a text's character) that
    /// pressed them: a key is let go by what it pressed, whatever the page
    /// says it is by then (Shift let go first, an uppercase letter).
    keys: HashMap<u32, u32>,
    wheel: (i32, i32),
}

impl Pointer {
    /// The X11 events for a batch from the page.
    fn events(&mut self, input: Vec<InputEvent>) -> Vec<X11Event> {
        let mut out = Vec::new();
        for ev in input {
            match ev {
                InputEvent::Key { code, key, down } => {
                    let id = if code != 0 { u32::from(code) } else { 0x8000_0000 | keysym(&key).unwrap_or(0) };
                    if down {
                        let Some(sym) = keysym(&key).map(|s| right_hand(code, s)) else { continue };
                        self.keys.insert(id, sym);
                        out.push(X11Event::KeyEvent(ClientKeyEvent { keycode: sym, down: true }));
                    } else if let Some(sym) = self.keys.remove(&id) {
                        out.push(X11Event::KeyEvent(ClientKeyEvent { keycode: sym, down: false }));
                    }
                }
                InputEvent::Text { ch, down } => {
                    for c in ch.chars() {
                        let Some(sym) = char_keysym(c) else { continue };
                        let id = 0x4000_0000 | c as u32;
                        if down {
                            self.keys.insert(id, sym);
                        } else if self.keys.remove(&id).is_none() {
                            continue;
                        }
                        out.push(X11Event::KeyEvent(ClientKeyEvent { keycode: sym, down }));
                    }
                }
                InputEvent::Move { x, y } => {
                    (self.x, self.y) = (x, y);
                    out.push(self.pointer());
                }
                InputEvent::Button { b, down } => {
                    let bit = match b {
                        0 => 1,
                        1 => 2,
                        2 => 4,
                        _ => continue,
                    };
                    if down {
                        self.buttons |= bit;
                    } else {
                        self.buttons &= !bit;
                    }
                    out.push(self.pointer());
                }
                InputEvent::Wheel { vertical, units } => {
                    // Buttons 4/5 (up/down) and 6/7 (left/right), one press
                    // and release a notch; small turns add up.
                    let acc = if vertical { &mut self.wheel.0 } else { &mut self.wheel.1 };
                    *acc += i32::from(units);
                    let mut clicks = Vec::new();
                    while acc.abs() >= WHEEL_NOTCH && clicks.len() < 16 {
                        let up = *acc > 0;
                        *acc -= WHEEL_NOTCH * acc.signum();
                        clicks.push(match (vertical, up) {
                            (true, true) => 8,
                            (true, false) => 16,
                            (false, true) => 32,
                            (false, false) => 64,
                        });
                    }
                    for bit in clicks {
                        out.push(self.pointer_with(self.buttons | bit));
                        out.push(self.pointer());
                    }
                }
                InputEvent::ReleaseAll => out.extend(self.release_all()),
            }
        }
        out
    }

    fn release_all(&mut self) -> Vec<X11Event> {
        let mut out: Vec<X11Event> = self
            .keys
            .drain()
            .map(|(_, sym)| X11Event::KeyEvent(ClientKeyEvent { keycode: sym, down: false }))
            .collect();
        if self.buttons != 0 {
            self.buttons = 0;
            out.push(self.pointer());
        }
        out
    }

    fn pointer(&self) -> X11Event {
        self.pointer_with(self.buttons)
    }

    fn pointer_with(&self, buttons: u8) -> X11Event {
        X11Event::PointerEvent(ClientMouseEvent { position_x: self.x, position_y: self.y, bottons: buttons })
    }
}

/// The server's picture as RGBA, and the parts of it not yet sent.
struct Screen {
    w: u16,
    h: u16,
    rgba: Vec<u8>,
    dirty: Vec<Rect>,
}

impl Screen {
    fn new() -> Self {
        Self { w: 0, h: 0, rgba: Vec::new(), dirty: Vec::new() }
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.rgba = vec![0; usize::from(w) * usize::from(h) * 4];
        self.dirty.clear();
    }

    /// `r` within the picture, or nothing.
    fn clip(&self, r: Rect) -> Option<Rect> {
        let right = (u32::from(r.x) + u32::from(r.width)).min(u32::from(self.w));
        let bottom = (u32::from(r.y) + u32::from(r.height)).min(u32::from(self.h));
        (u32::from(r.x) < right && u32::from(r.y) < bottom)
            .then(|| Rect { x: r.x, y: r.y, width: (right - u32::from(r.x)) as u16, height: (bottom - u32::from(r.y)) as u16 })
    }

    /// RGBA rows for `r` (`r.width` pixels a row) from the server.
    fn put(&mut self, r: Rect, data: &[u8]) {
        let Some(c) = self.clip(r) else { return };
        let src_row = usize::from(r.width) * 4;
        let row = usize::from(c.width) * 4;
        for y in 0..usize::from(c.height) {
            let from = y * src_row;
            let Some(src) = data.get(from..from + row) else { break };
            let at = ((usize::from(c.y) + y) * usize::from(self.w) + usize::from(c.x)) * 4;
            let dst = &mut self.rgba[at..at + row];
            dst.copy_from_slice(src);
            // The server's alpha byte is padding: drawn opaque.
            for px in dst.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        self.dirty.push(c);
    }

    /// A part of the picture copied to another place (CopyRect).
    fn copy(&mut self, dst: Rect, src: Rect) {
        let (Some(d), Some(s)) = (self.clip(dst), self.clip(src)) else { return };
        let (w, h) = (d.width.min(s.width), d.height.min(s.height));
        let stride = usize::from(self.w) * 4;
        let row = usize::from(w) * 4;
        // Through a copy: the two may overlap.
        let mut block = Vec::with_capacity(row * usize::from(h));
        for y in 0..usize::from(h) {
            let at = (usize::from(s.y) + y) * stride + usize::from(s.x) * 4;
            block.extend_from_slice(&self.rgba[at..at + row]);
        }
        for y in 0..usize::from(h) {
            let at = (usize::from(d.y) + y) * stride + usize::from(d.x) * 4;
            self.rgba[at..at + row].copy_from_slice(&block[y * row..(y + 1) * row]);
        }
        self.dirty.push(Rect { x: d.x, y: d.y, width: w, height: h });
    }

    /// The frames for what changed since the last time.
    fn frames(&mut self) -> Vec<Vec<u8>> {
        let mut rects = std::mem::take(&mut self.dirty);
        if rects.len() > MAX_RECTS {
            let (x0, y0) = (rects.iter().map(|r| r.x).min().unwrap_or(0), rects.iter().map(|r| r.y).min().unwrap_or(0));
            let x1 = rects.iter().map(|r| r.x + r.width).max().unwrap_or(0);
            let y1 = rects.iter().map(|r| r.y + r.height).max().unwrap_or(0);
            rects = vec![Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }];
        }
        rects.iter().filter_map(|r| self.rect_frame(*r)).collect()
    }

    fn rect_frame(&self, r: Rect) -> Option<Vec<u8>> {
        let c = self.clip(r)?;
        let mut f = Vec::with_capacity(9 + usize::from(c.width) * usize::from(c.height) * 4);
        f.push(2u8);
        for v in [c.x, c.y, c.width, c.height] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        let row = usize::from(c.width) * 4;
        for y in 0..usize::from(c.height) {
            let at = ((usize::from(c.y) + y) * usize::from(self.w) + usize::from(c.x)) * 4;
            f.extend_from_slice(&self.rgba[at..at + row]);
        }
        Some(f)
    }
}

fn size_frame(w: u16, h: u16) -> Vec<u8> {
    let mut f = Vec::with_capacity(5);
    f.push(1u8);
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f
}

/// What a failed start means, for the tab.
pub fn explain(e: &VncError) -> String {
    match e {
        VncError::WrongPassword => "The VNC server refused the password.".into(),
        VncError::NoPassword => "The VNC server wants a password, and none was given.".into(),
        VncError::General(m) if m.contains("Security type apart from Vnc Auth") => {
            "The VNC server only offers logins Submarine doesn't speak (it speaks none and the VNC password). On macOS, turn on \"VNC viewers may control screen with password\"; on RealVNC, set its encryption to \"Prefer off\" and its login to \"VNC password\".".into()
        }
        // An RFB 3.8 server's own words for a refused login.
        VncError::General(m) => format!("The VNC server said: {m}"),
        VncError::IoError(e) => format!("The connection to the VNC server ended: {e}"),
        other => format!("VNC: {other}"),
    }
}

/// Runs one VNC session until the page closes it or the server ends it: the
/// handshake (with `password`, awaited only if the server asks for one), then
/// the screen to `frame` and the page's input to the server. `connected` hears
/// the desktop's first size. Ok("closed") when the page closed it, Ok(reason)
/// when the server ended it, Err when it failed.
pub async fn run<S, P, C, F>(
    stream: S,
    password: P,
    connected: C,
    mut frame: F,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    wanted: (u16, u16),
) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    P: Future<Output = Option<Zeroizing<String>>> + Send + Sync + 'static,
    C: FnOnce(u16, u16) + Send,
    F: FnMut(Vec<u8>) -> bool + Send,
{
    let password = async move {
        match password.await {
            Some(p) => Ok(p.to_string()),
            None => Err(VncError::NoPassword),
        }
    };
    let state = VncConnector::new(stream)
        .set_auth_method(password)
        .add_encoding(VncEncoding::Tight)
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::Trle)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .add_encoding(VncEncoding::ExtendedDesktopSizePseudo)
        .add_encoding(VncEncoding::LastRectPseudo)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::rgba())
        .build()
        .map_err(|e| explain(&e))?;
    let started = tokio::time::timeout(std::time::Duration::from_secs(150), state.try_start())
        .await
        .map_err(|_| "The VNC server didn't finish the handshake".to_string())?
        .map_err(|e| explain(&e))?;
    let client = started.finish().map_err(|e| explain(&e))?;

    let mut screen = Screen::new();
    let mut pointer = Pointer::default();
    let mut connected = Some(connected);
    let mut asked_size: Option<(u16, u16)> = None;
    let resize = |client: &::vnc::VncClient, (w, h): (u16, u16)| {
        let client = client.clone();
        let (w, h) = clamp_size(w, h);
        tokio::spawn(async move {
            let _ = client.resize_desktop(w, h).await;
        });
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut since_refresh = 0u32;
    let outcome = loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Input(events)) => {
                    for ev in pointer.events(events) {
                        if client.input(ev).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Cmd::Resize(w, h)) => {
                    asked_size = Some((w, h));
                    if screen.w > 0 {
                        resize(&client, (w, h));
                    }
                }
                Some(Cmd::Close) | None => {
                    for ev in pointer.release_all() {
                        let _ = client.input(ev).await;
                    }
                    break Ok("closed".to_string());
                }
            },
            _ = tick.tick() => {
                let mut ended = None;
                loop {
                    match client.poll_event().await {
                        Ok(Some(ev)) => match ev {
                            VncEvent::SetResolution(s) => {
                                screen.resize(s.width, s.height);
                                if !frame(size_frame(s.width, s.height)) {
                                    ended = Some(Ok("closed".to_string()));
                                    break;
                                }
                                if let Some(c) = connected.take() {
                                    c(s.width, s.height);
                                    // The tab's size, once the server has said its own.
                                    let want = asked_size.unwrap_or(wanted);
                                    if want != (s.width, s.height) {
                                        resize(&client, want);
                                    }
                                }
                            }
                            VncEvent::DesktopUpdate(u) => {
                                if let Some(l) = u.layout {
                                    if (l.width, l.height) != (screen.w, screen.h) {
                                        screen.resize(l.width, l.height);
                                        if !frame(size_frame(l.width, l.height)) {
                                            ended = Some(Ok("closed".to_string()));
                                            break;
                                        }
                                    }
                                }
                            }
                            VncEvent::RawImage(r, data) => screen.put(r, &data),
                            VncEvent::Copy(dst, src) => screen.copy(dst, src),
                            VncEvent::Error(e) => {
                                ended = Some(Err(format!("VNC: {e}")));
                                break;
                            }
                            // JPEG isn't asked for; the cursor is in the
                            // picture; the clipboard and the bell aren't used.
                            _ => {}
                        },
                        Ok(None) => break,
                        Err(e) => {
                            ended = Some(match e {
                                VncError::ClientNotRunning => Ok("The VNC server closed the connection.".to_string()),
                                e => Err(explain(&e)),
                            });
                            break;
                        }
                    }
                }
                if let Some(end) = ended {
                    break end;
                }
                let sent = screen.frames();
                let changed = !sent.is_empty();
                for f in sent {
                    if !frame(f) {
                        break;
                    }
                }
                // The next incremental update: right after one came, else a
                // few times a second.
                since_refresh += 1;
                if changed || since_refresh >= 6 {
                    since_refresh = 0;
                    if client.input(X11Event::Refresh).await.is_err() {
                        break Ok("The VNC server closed the connection.".to_string());
                    }
                }
            }
        }
    };
    let _ = client.close().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(evs: &[X11Event]) -> Vec<(u32, bool)> {
        evs.iter()
            .filter_map(|e| match e {
                X11Event::KeyEvent(k) => Some((k.keycode, k.down)),
                _ => None,
            })
            .collect()
    }
    fn pointers(evs: &[X11Event]) -> Vec<(u16, u16, u8)> {
        evs.iter()
            .filter_map(|e| match e {
                X11Event::PointerEvent(p) => Some((p.position_x, p.position_y, p.bottons)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym("a"), Some(0x61));
        assert_eq!(keysym("A"), Some(0x41));
        assert_eq!(keysym(" "), Some(0x20));
        assert_eq!(keysym("é"), Some(0xe9));
        assert_eq!(keysym("€"), Some(0x0100_20ac));
        assert_eq!(keysym("Enter"), Some(0xff0d));
        assert_eq!(keysym("Backspace"), Some(0xff08));
        assert_eq!(keysym("ArrowDown"), Some(0xff54));
        assert_eq!(keysym("F1"), Some(0xffbe));
        assert_eq!(keysym("F12"), Some(0xffc9));
        assert_eq!(keysym("F25"), None);
        assert_eq!(keysym("Fn"), None);
        assert_eq!(keysym("Unidentified"), None);
        assert_eq!(keysym("\u{7}"), None);
    }

    #[test]
    fn a_key_is_let_go_by_what_it_pressed() {
        let mut p = Pointer::default();
        let k = |code, key: &str, down| InputEvent::Key { code, key: key.into(), down };
        // Shift down, "A", Shift up, then the key up as "a": it lets go of "A".
        let evs = p.events(vec![k(0x2a, "Shift", true), k(0x1e, "A", true), k(0x2a, "Shift", false), k(0x1e, "a", false)]);
        assert_eq!(keys(&evs), [(0xffe1, true), (0x41, true), (0xffe1, false), (0x41, false)]);
        // A key up never pressed: nothing.
        assert!(p.events(vec![k(0x1f, "s", false)]).is_empty());
        // The right-hand Control.
        assert_eq!(keys(&p.events(vec![k(0x11d, "Control", true)])), [(0xffe4, true)]);
        // Let go of everything: what's still down.
        assert_eq!(keys(&p.events(vec![InputEvent::ReleaseAll])), [(0xffe4, false)]);
    }

    #[test]
    fn text_from_a_phone_keyboard() {
        let mut p = Pointer::default();
        let evs = p.events(vec![InputEvent::Text { ch: "hé".into(), down: true }, InputEvent::Text { ch: "hé".into(), down: false }]);
        assert_eq!(keys(&evs), [(0x68, true), (0xe9, true), (0x68, false), (0xe9, false)]);
    }

    #[test]
    fn buttons_and_the_wheel() {
        let mut p = Pointer::default();
        let evs = p.events(vec![
            InputEvent::Move { x: 10, y: 20 },
            InputEvent::Button { b: 0, down: true },
            InputEvent::Button { b: 2, down: true },
            InputEvent::Button { b: 0, down: false },
            InputEvent::Button { b: 3, down: true },
        ]);
        assert_eq!(pointers(&evs), [(10, 20, 0), (10, 20, 1), (10, 20, 5), (10, 20, 4)]);
        // A notch up: button 4 pressed and let go, the right button still held.
        assert_eq!(pointers(&p.events(vec![InputEvent::Wheel { vertical: true, units: 120 }])), [(10, 20, 12), (10, 20, 4)]);
        // Small turns add up to a notch down.
        assert!(p.events(vec![InputEvent::Wheel { vertical: true, units: -60 }]).is_empty());
        assert_eq!(pointers(&p.events(vec![InputEvent::Wheel { vertical: true, units: -60 }])), [(10, 20, 20), (10, 20, 4)]);
        // Sideways: buttons 6 and 7.
        assert_eq!(pointers(&p.events(vec![InputEvent::Wheel { vertical: false, units: -240 }])), [(10, 20, 68), (10, 20, 4), (10, 20, 68), (10, 20, 4)]);
        // Let go of everything: the button too.
        assert_eq!(pointers(&p.events(vec![InputEvent::ReleaseAll])), [(10, 20, 0)]);
    }

    #[test]
    fn the_picture_its_parts_and_copies() {
        let mut s = Screen::new();
        s.resize(4, 3);
        // A 2x2 red square at (1,1), its alpha made opaque.
        s.put(Rect { x: 1, y: 1, width: 2, height: 2 }, &[255, 0, 0, 0].repeat(4));
        let f = s.frames();
        assert_eq!(f.len(), 1);
        assert_eq!(&f[0][..9], &[2, 1, 0, 1, 0, 2, 0, 2, 0]);
        assert_eq!(&f[0][9..13], &[255, 0, 0, 255]);
        assert!(s.frames().is_empty(), "sent once");
        // Copied one to the left (overlapping).
        s.copy(Rect { x: 0, y: 1, width: 2, height: 2 }, Rect { x: 1, y: 1, width: 2, height: 2 });
        let px = |x: usize, y: usize| s.rgba[(y * 4 + x) * 4];
        assert_eq!([px(0, 1), px(1, 1), px(2, 1), px(3, 1)], [255, 255, 255, 0]);
        // Parts past the edge are cut; one entirely outside is dropped.
        s.put(Rect { x: 3, y: 2, width: 3, height: 3 }, &[9; 36]);
        s.put(Rect { x: 9, y: 9, width: 1, height: 1 }, &[9; 4]);
        let f = s.frames();
        assert_eq!(f.len(), 2);
        assert_eq!(&f[1][..9], &[2, 3, 0, 2, 0, 1, 0, 1, 0]);
        // Short data stops where it ends.
        s.put(Rect { x: 0, y: 0, width: 4, height: 3 }, &[7; 20]);
        assert_eq!(s.rgba[0], 7);
        assert_eq!(s.rgba[16], 255, "the second row wasn't in the data: left as it was");
        // Many parts: one rectangle around them.
        for i in 0..40u16 {
            s.dirty.push(Rect { x: i % 4, y: i % 3, width: 1, height: 1 });
        }
        let f = s.frames();
        assert_eq!(f.len(), 1);
        assert_eq!(&f[0][..9], &[2, 0, 0, 0, 0, 4, 0, 3, 0]);
    }

    #[test]
    fn sizes() {
        assert_eq!(clamp_size(10, 10), (200, 200));
        assert_eq!(clamp_size(1280, 800), (1280, 800));
        assert_eq!(clamp_size(20000, 9000), (8192, 8192));
        assert_eq!(size_frame(1024, 700), [1, 0, 4, 188, 2]);
    }

    #[test]
    fn errors_for_the_tab() {
        assert!(explain(&VncError::WrongPassword).contains("refused the password"));
        assert!(explain(&VncError::General("Security type apart from Vnc Auth has not been implemented".into())).contains("VNC password"));
        assert_eq!(explain(&VncError::General("Authentication failed".into())), "The VNC server said: Authentication failed");
    }
}
