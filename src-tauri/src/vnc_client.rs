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
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use ::vnc::{ClientKeyEvent, ClientMouseEvent, PixelFormat, Rect, VncConnector, VncEncoding, VncError, VncEvent, X11Event};
use zeroize::Zeroizing;

/// The port a VNC server listens on unless told otherwise (display :0).
pub const DEFAULT_PORT: u16 = 5900;
/// How often the session looks for what the server sent (and asks for more).
const TICK_MS: u64 = 16;
/// The most updates a second can be asked for: no limit below the tick's own pace.
const MAX_FPS: u8 = 60;
/// The sizes a desktop may be asked for.
const MIN_SIDE: u16 = 200;
const MAX_SIDE: u16 = 8192;
/// More changed rectangles than this before a frame goes out: one rectangle
/// around them all is sent instead.
const MAX_RECTS: usize = 32;
/// Wheel units (the page's, 120 to a notch) for one click of a wheel button.
const WHEEL_NOTCH: i32 = 120;

/// The running sessions, by tab: where the page's input and sizes go.
/// Each tab's session, with the attempt that started it (see
/// `forget_screen_client`).
pub type VncClients = std::sync::Arc<tokio::sync::Mutex<HashMap<String, (u64, mpsc::UnboundedSender<Cmd>)>>>;

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

/// Which way the server is asked to pack the screen. Automatic suits most
/// links; Raw is the least work for both ends and the most to send, for a fast
/// network; Tight packs the most, but only in full colour (see
/// `Options::encodings`).
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    #[default]
    Auto,
    Tight,
    Zrle,
    Raw,
}

/// What the page asks of the picture for one server (the "Speed and quality"
/// choices of the tab, `vncOptions.ts`). The default is what this client asked
/// for before they existed.
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Options {
    pub encoding: Encoding,
    /// Bits a pixel on the wire: 32 (full colour), 16 or 8.
    pub colors: u8,
    /// The most updates asked for in a second (`MAX_FPS`: no limit).
    pub max_fps: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self { encoding: Encoding::Auto, colors: 32, max_fps: MAX_FPS }
    }
}

impl Options {
    /// The choices as the page sent them, or why they can't be used.
    pub fn checked(self) -> Result<Self, String> {
        if !matches!(self.colors, 32 | 16 | 8) {
            return Err(format!("{}-bit colour isn't one of 32, 16 and 8", self.colors));
        }
        if !(1..=MAX_FPS).contains(&self.max_fps) {
            return Err(format!("{} updates a second isn't between 1 and {MAX_FPS}", self.max_fps));
        }
        Ok(self)
    }

    /// A one-line account for the tab's log, where the choices are not the standard ones.
    pub fn describe(&self) -> Option<String> {
        if *self == Self::default() {
            return None;
        }
        let encoding = match self.encoding {
            Encoding::Auto => "automatic encoding",
            Encoding::Tight => "Tight encoding",
            Encoding::Zrle => "ZRLE encoding",
            Encoding::Raw => "Raw encoding",
        };
        let mut parts = vec![encoding.to_string(), format!("{}-bit colour", self.colors)];
        if self.max_fps < MAX_FPS {
            parts.push(format!("at most {} updates a second", self.max_fps));
        }
        Some(format!("Speed and quality: {}.", parts.join(", ")))
    }

    /// The encodings to offer, in the order of preference: servers use the first
    /// they have. What is chosen goes first; the rest follow as they always did.
    /// Tight is left out below full colour, which vnc-rs can't read it in.
    fn encodings(&self) -> Vec<VncEncoding> {
        let full = self.colors == 32;
        let first = match self.encoding {
            Encoding::Auto => None,
            Encoding::Tight => full.then_some(VncEncoding::Tight),
            Encoding::Zrle => Some(VncEncoding::Zrle),
            Encoding::Raw => Some(VncEncoding::Raw),
        };
        let mut out: Vec<VncEncoding> = first.into_iter().collect();
        for e in [VncEncoding::Tight, VncEncoding::Zrle, VncEncoding::Trle, VncEncoding::CopyRect, VncEncoding::Raw] {
            if (full || e != VncEncoding::Tight) && !out.contains(&e) {
                out.push(e);
            }
        }
        out.extend([VncEncoding::DesktopSizePseudo, VncEncoding::ExtendedDesktopSizePseudo, VncEncoding::LastRectPseudo]);
        out
    }

    /// The pixels as the server is asked to send them: full colour as RGBA,
    /// 16 bits as RGB565, 8 bits as BGR233 (the formats viewers have always used).
    fn pixel_format(&self) -> Result<PixelFormat, VncError> {
        // bits, depth, big-endian, true colour, the three maxima, the three shifts, padding
        let format = |bits: u8, depth: u8, max: [u16; 3], shift: [u8; 3]| {
            let mut b = [0u8; 16];
            b[0] = bits;
            b[1] = depth;
            b[3] = 1;
            for i in 0..3 {
                b[4 + 2 * i..6 + 2 * i].copy_from_slice(&max[i].to_be_bytes());
                b[10 + i] = shift[i];
            }
            PixelFormat::try_from(b)
        };
        match self.colors {
            16 => format(16, 16, [31, 63, 31], [11, 5, 0]),
            8 => format(8, 8, [7, 7, 3], [0, 3, 6]),
            _ => Ok(PixelFormat::rgba()),
        }
    }

    /// The time to leave between two asks for an update (none: ask as often as the tick comes).
    fn gap(&self) -> Duration {
        if self.max_fps >= MAX_FPS {
            return Duration::ZERO;
        }
        Duration::from_millis(1000 / u64::from(self.max_fps.max(1)))
    }
}

/// How the server's pixels are laid out (the format that was asked for), and the
/// way from them to RGBA.
struct Wire {
    /// Bytes a pixel on the wire.
    bytes: usize,
    big_endian: bool,
    max: [u32; 3],
    shift: [u32; 3],
    /// For red, green and blue: a value of the channel (0..=max) as 0..=255.
    scale: [Vec<u8>; 3],
    /// Already RGBA byte for byte (full colour as asked for): copied as it is.
    rgba32: bool,
}

impl Wire {
    fn rgba32() -> Self {
        Self { bytes: 4, big_endian: false, max: [255; 3], shift: [0, 8, 16], scale: Default::default(), rgba32: true }
    }

    /// The way for a format (true colour only, 8, 16 or 32 bits), else none.
    fn new(pf: &PixelFormat) -> Option<Self> {
        let bytes = usize::from(pf.bits_per_pixel / 8);
        let max = [u32::from(pf.red_max), u32::from(pf.green_max), u32::from(pf.blue_max)];
        let shift = [u32::from(pf.red_shift), u32::from(pf.green_shift), u32::from(pf.blue_shift)];
        // A channel is at most a byte here (the tables stay small), and fits in the pixel.
        if !matches!(bytes, 1 | 2 | 4) || pf.true_color_flag != 1 || max.iter().any(|m| *m == 0 || *m > 255 || m & (m + 1) != 0) || shift.iter().any(|s| *s >= 32) {
            return None;
        }
        let big_endian = pf.big_endian_flag != 0;
        if bytes == 4 && !big_endian && max == [255; 3] && shift == [0, 8, 16] {
            return Some(Self::rgba32());
        }
        let scale = max.map(|m| (0..=m).map(|v| ((v * 255 + m / 2) / m) as u8).collect());
        Some(Self { bytes, big_endian, max, shift, scale, rgba32: false })
    }

    /// `src` (whole pixels of the wire) as RGBA into `dst`, opaque: a pixel is
    /// converted only where both have room for it.
    fn to_rgba(&self, src: &[u8], dst: &mut [u8]) {
        if self.rgba32 {
            let n = src.len().min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
            // The server's alpha byte is padding: drawn opaque.
            for px in dst[..n].chunks_exact_mut(4) {
                px[3] = 255;
            }
            return;
        }
        for (px, out) in src.chunks_exact(self.bytes).zip(dst.chunks_exact_mut(4)) {
            let v = match (self.bytes, self.big_endian) {
                (1, _) => u32::from(px[0]),
                (2, false) => u32::from(u16::from_le_bytes([px[0], px[1]])),
                (2, true) => u32::from(u16::from_be_bytes([px[0], px[1]])),
                (_, false) => u32::from_le_bytes([px[0], px[1], px[2], px[3]]),
                (_, true) => u32::from_be_bytes([px[0], px[1], px[2], px[3]]),
            };
            for c in 0..3 {
                // The mask keeps the index inside the table (`max` is a run of ones).
                out[c] = self.scale[c][((v >> self.shift[c]) & self.max[c]) as usize];
            }
            out[3] = 255;
        }
    }
}

/// When to ask the server for the next update: right after one came, else a few
/// times a second; never closer than `gap` to the last ask (the page's "updates a
/// second" choice, kept in time and not in ticks: the tick is only as even as the
/// system's timer, which on Windows is 16 ms at best and often 31); and never
/// while the page is behind (`held`), the ask being made once it has caught up.
struct Pacer {
    gap: Duration,
    last_ask: Option<Instant>,
    idle: u32,
    wanted: bool,
}

impl Pacer {
    fn new(gap: Duration) -> Self {
        Self { gap, last_ask: None, idle: 0, wanted: false }
    }

    /// One tick of the session, at `now`; whether to ask now.
    fn tick(&mut self, now: Instant, changed: bool, held: bool) -> bool {
        self.idle = self.idle.saturating_add(1);
        if changed || self.idle >= 6 {
            self.wanted = true;
        }
        // A tick comes a little early or late: an ask within half a tick of the gap is on time.
        let too_soon = self.last_ask.is_some_and(|t| now.saturating_duration_since(t) + Duration::from_millis(TICK_MS / 2) < self.gap);
        if held || !self.wanted || too_soon {
            return false;
        }
        self.wanted = false;
        self.idle = 0;
        self.last_ask = Some(now);
        true
    }
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
    /// The pixels the server sends, as they are laid out.
    wire: Wire,
}

impl Screen {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_wire(Wire::rgba32())
    }

    fn with_wire(wire: Wire) -> Self {
        Self { w: 0, h: 0, rgba: Vec::new(), dirty: Vec::new(), wire }
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

    /// Rows of the server's pixels for `r` (`r.width` pixels a row, as the wire
    /// lays them out), drawn into the picture as RGBA.
    fn put(&mut self, r: Rect, data: &[u8]) {
        let Some(c) = self.clip(r) else { return };
        let src_row = usize::from(r.width) * self.wire.bytes;
        let src_take = usize::from(c.width) * self.wire.bytes;
        let row = usize::from(c.width) * 4;
        for y in 0..usize::from(c.height) {
            let from = y * src_row;
            let Some(src) = data.get(from..from + src_take) else { break };
            let at = ((usize::from(c.y) + y) * usize::from(self.w) + usize::from(c.x)) * 4;
            self.wire.to_rgba(src, &mut self.rgba[at..at + row]);
        }
        self.mark(c);
    }

    /// Remember that `r` changed. Past `MAX_RECTS` places the list becomes one
    /// rectangle around them all: while the page isn't taking frames, a server's
    /// small updates must not make it grow for good.
    fn mark(&mut self, r: Rect) {
        self.dirty.push(r);
        if self.dirty.len() > MAX_RECTS {
            self.dirty = vec![bounds(&self.dirty)];
        }
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
        self.mark(Rect { x: d.x, y: d.y, width: w, height: h });
    }

    /// The frames for what changed since the last time.
    fn frames(&mut self) -> Vec<Vec<u8>> {
        let mut rects = std::mem::take(&mut self.dirty);
        // Many parts, or parts that together are more than the picture (the same
        // place changed again and again): one rectangle around them, so a batch
        // is never more than about one picture.
        let area: u64 = rects.iter().map(|r| u64::from(r.width) * u64::from(r.height)).sum();
        if rects.len() > MAX_RECTS || area > u64::from(self.w) * u64::from(self.h) {
            rects = vec![bounds(&rects)];
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

/// The smallest rectangle that holds all of `rects` (which are inside the picture).
fn bounds(rects: &[Rect]) -> Rect {
    let (x0, y0) = (rects.iter().map(|r| r.x).min().unwrap_or(0), rects.iter().map(|r| r.y).min().unwrap_or(0));
    let x1 = rects.iter().map(|r| u32::from(r.x) + u32::from(r.width)).max().unwrap_or(0);
    let y1 = rects.iter().map(|r| u32::from(r.y) + u32::from(r.height)).max().unwrap_or(0);
    Rect { x: x0, y: y0, width: (x1 - u32::from(x0)) as u16, height: (y1 - u32::from(y0)) as u16 }
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
#[allow(clippy::too_many_arguments)]
pub async fn run<S, P, C, F>(
    stream: S,
    password: P,
    connected: C,
    mut frame: F,
    flow: std::sync::Arc<crate::screen_flow::Flow>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    wanted: (u16, u16),
    options: Options,
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
    let format = options.pixel_format().map_err(|e| explain(&e))?;
    let wire = Wire::new(&format).ok_or("VNC: that colour depth can't be drawn")?;
    let mut connector = VncConnector::new(stream).set_auth_method(password);
    for encoding in options.encodings() {
        connector = connector.add_encoding(encoding);
    }
    let state = connector.allow_shared(true).set_pixel_format(format).build().map_err(|e| explain(&e))?;
    let started = tokio::time::timeout(std::time::Duration::from_secs(150), state.try_start())
        .await
        .map_err(|_| "The VNC server didn't finish the handshake".to_string())?
        .map_err(|e| explain(&e))?;
    let client = started.finish().map_err(|e| explain(&e))?;

    let mut screen = Screen::with_wire(wire);
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
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pacer = Pacer::new(options.gap());
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
                // While the page is far behind on what it was sent, what
                // changes stays in the picture here (see screen_flow.rs) and no
                // new update is asked for: the server, which only sends on a
                // request, waits too.
                let held = !flow.has_room();
                let sent = if held { Vec::new() } else { screen.frames() };
                let changed = !sent.is_empty();
                for f in sent {
                    if !frame(f) {
                        break;
                    }
                }
                // The next incremental update: right after one came, else a
                // few times a second, and not oftener than the page chose.
                if pacer.tick(Instant::now(), changed, held) {
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
    use std::sync::{Arc, Mutex};

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
    fn what_changes_while_the_page_is_behind_stays_one_rectangle() {
        let mut s = Screen::new();
        s.resize(1000, 1000);
        // 50,000 small updates in two corners and nothing sent: the list of places never grows.
        for i in 0..50_000u32 {
            let at = if i % 2 == 0 { 0 } else { 998 };
            s.put(Rect { x: at, y: at, width: 2, height: 2 }, &[1; 16]);
            assert!(s.dirty.len() <= MAX_RECTS, "{} places after {} updates", s.dirty.len(), i + 1);
        }
        let f = s.frames();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].len(), 9 + 1000 * 1000 * 4, "one frame around both corners: the whole picture");
        // Copies count the same way.
        for _ in 0..1000 {
            s.copy(Rect { x: 10, y: 10, width: 1, height: 1 }, Rect { x: 20, y: 20, width: 1, height: 1 });
            s.copy(Rect { x: 500, y: 500, width: 1, height: 1 }, Rect { x: 20, y: 20, width: 1, height: 1 });
        }
        assert!(s.dirty.len() <= MAX_RECTS);
    }

    #[test]
    fn parts_that_overlap_cost_one_picture_at_most() {
        let mut s = Screen::new();
        s.resize(100, 100);
        // The same full-screen part thirty times in one tick (under the count that already collapsed).
        for _ in 0..30 {
            s.put(Rect { x: 0, y: 0, width: 100, height: 100 }, &vec![1; 40_000]);
        }
        let f = s.frames();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].len(), 9 + 40_000);
        // Parts that don't add up to more than the picture are sent as they are.
        s.put(Rect { x: 0, y: 0, width: 10, height: 10 }, &vec![1; 400]);
        s.put(Rect { x: 50, y: 50, width: 10, height: 10 }, &vec![1; 400]);
        assert_eq!(s.frames().len(), 2);
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

    fn with(encoding: Encoding, colors: u8, max_fps: u8) -> Options {
        Options { encoding, colors, max_fps }
    }

    // A server nobody changed the speed and quality of must be asked exactly
    // what it was asked before they existed.
    #[test]
    fn untouched_options_ask_what_the_client_always_asked() {
        let o = Options::default();
        use VncEncoding::*;
        assert_eq!(o.encodings(), [Tight, Zrle, Trle, CopyRect, Raw, DesktopSizePseudo, ExtendedDesktopSizePseudo, LastRectPseudo]);
        assert_eq!(Vec::<u8>::from(o.pixel_format().unwrap()), Vec::<u8>::from(PixelFormat::rgba()));
        assert_eq!(o.gap(), Duration::ZERO, "no limit: asked as often as the tick comes");
        assert_eq!(o.describe(), None, "nothing to say about the standard ones");
        assert_eq!(serde_json::from_str::<Options>("{}").unwrap(), o);
        assert!(o.checked().is_ok());
    }

    #[test]
    fn the_chosen_encoding_goes_first_and_tight_needs_full_colour() {
        use VncEncoding::*;
        let order = |encoding, colors| with(encoding, colors, MAX_FPS).encodings();
        assert_eq!(&order(Encoding::Raw, 32)[..2], &[Raw, Tight]);
        assert_eq!(&order(Encoding::Zrle, 32)[..2], &[Zrle, Tight]);
        assert_eq!(&order(Encoding::Tight, 32)[..2], &[Tight, Zrle]);
        // Tight asked for below full colour is left out: automatic, with ZRLE first.
        assert_eq!(order(Encoding::Tight, 16), order(Encoding::Auto, 16));
        assert_eq!(order(Encoding::Auto, 8)[0], Zrle);
        for encoding in [Encoding::Auto, Encoding::Tight, Encoding::Zrle, Encoding::Raw] {
            for colors in [32, 16, 8] {
                let list = order(encoding, colors);
                // Raw (which the protocol requires), CopyRect and the size messages are always there, each once.
                for e in [Raw, CopyRect, DesktopSizePseudo, ExtendedDesktopSizePseudo, LastRectPseudo, Zrle, Trle] {
                    assert_eq!(list.iter().filter(|x| **x == e).count(), 1, "{e:?} in {encoding:?} at {colors} bits");
                }
                assert_eq!(list.contains(&Tight), colors == 32, "{encoding:?} at {colors} bits");
                assert_eq!(&list[list.len() - 3..], &[DesktopSizePseudo, ExtendedDesktopSizePseudo, LastRectPseudo]);
            }
        }
    }

    #[test]
    fn every_colour_depth_asks_for_a_format_the_session_can_read() {
        for colors in [32u8, 16, 8] {
            let pf = with(Encoding::Auto, colors, MAX_FPS).pixel_format().unwrap_or_else(|e| panic!("{colors} bits: {e}"));
            assert_eq!(pf.bits_per_pixel, colors);
            assert!(Wire::new(&pf).is_some(), "{colors} bits");
        }
        let b = Vec::<u8>::from(with(Encoding::Auto, 16, MAX_FPS).pixel_format().unwrap());
        assert_eq!(b, [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0], "RGB565, little-endian");
        let b = Vec::<u8>::from(with(Encoding::Auto, 8, MAX_FPS).pixel_format().unwrap());
        assert_eq!(b, [8, 8, 0, 1, 0, 7, 0, 7, 0, 3, 0, 3, 6, 0, 0, 0], "BGR233");
    }

    #[test]
    fn updates_a_second_become_the_time_between_asks() {
        let gap = |fps| with(Encoding::Auto, 32, fps).gap().as_millis();
        assert_eq!([gap(60), gap(30), gap(15), gap(10), gap(5), gap(1)], [0, 33, 66, 100, 200, 1000]);
        // Out of range is the nearest end, never a division by zero.
        assert_eq!(gap(0), 1000);
        assert_eq!(gap(200), 0);
    }

    #[test]
    fn choices_the_client_doesnt_offer_are_refused() {
        for colors in [0u8, 1, 15, 24, 64, 255] {
            assert!(with(Encoding::Auto, colors, 60).checked().is_err(), "{colors}");
        }
        for fps in [0u8, 61, 255] {
            assert!(with(Encoding::Auto, 32, fps).checked().is_err(), "{fps}");
        }
        for (colors, fps) in [(32, 60), (16, 30), (8, 15), (32, 1), (16, 5)] {
            assert!(with(Encoding::Auto, colors, fps).checked().is_ok(), "{colors} {fps}");
        }
        // What the page sends: camel case, a part left out is the standard one, an unknown word is an error.
        let o: Options = serde_json::from_str(r#"{"encoding":"zrle","colors":16,"maxFps":15}"#).unwrap();
        assert_eq!(o, with(Encoding::Zrle, 16, 15));
        assert_eq!(serde_json::from_str::<Options>(r#"{"colors":8}"#).unwrap(), with(Encoding::Auto, 8, 60));
        assert!(serde_json::from_str::<Options>(r#"{"encoding":"hextile"}"#).is_err());
        assert!(serde_json::from_str::<Options>(r#"{"colors":"16"}"#).is_err());
    }

    #[test]
    fn the_log_line_says_what_was_chosen() {
        assert_eq!(
            with(Encoding::Zrle, 16, 15).describe().as_deref(),
            Some("Speed and quality: ZRLE encoding, 16-bit colour, at most 15 updates a second.")
        );
        assert_eq!(with(Encoding::Raw, 32, 60).describe().as_deref(), Some("Speed and quality: Raw encoding, 32-bit colour."));
    }

    #[test]
    fn the_tab_and_the_backend_use_the_same_words() {
        let ts = include_str!("../../src/util/vncOptions.ts");
        for word in ["\"auto\"", "\"tight\"", "\"zrle\"", "\"raw\"", "maxFps"] {
            assert!(ts.contains(word), "src/util/vncOptions.ts must use {word}, as vnc_client::Options does");
        }
    }

    #[test]
    fn sixteen_bit_pixels_become_rgba() {
        let w = Wire::new(&with(Encoding::Auto, 16, MAX_FPS).pixel_format().unwrap()).unwrap();
        // RGB565, little-endian: red 0xF800, green 0x07E0, blue 0x001F, white, black.
        let src = [0x00u8, 0xF8, 0xE0, 0x07, 0x1F, 0x00, 0xFF, 0xFF, 0x00, 0x00];
        let mut dst = [9u8; 20];
        w.to_rgba(&src, &mut dst);
        assert_eq!(dst, [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255]);
        // A middle value is scaled to the whole range: 16 of 31 is 132 of 255.
        let mut one = [0u8; 4];
        w.to_rgba(&[0x00, 0x80], &mut one);
        assert_eq!(one, [132, 0, 0, 255]);
        // Half a pixel at the end is not a pixel; a short place to put them stops there.
        let mut dst = [1u8; 8];
        w.to_rgba(&src[..3], &mut dst);
        assert_eq!(dst, [255, 0, 0, 255, 1, 1, 1, 1]);
        let mut dst = [1u8; 4];
        w.to_rgba(&src, &mut dst);
        assert_eq!(dst, [255, 0, 0, 255]);
    }

    #[test]
    fn eight_bit_pixels_become_rgba() {
        let w = Wire::new(&with(Encoding::Auto, 8, MAX_FPS).pixel_format().unwrap()).unwrap();
        // BGR233: red in bits 0-2, green in 3-5, blue in 6-7.
        let mut dst = [0u8; 16];
        w.to_rgba(&[0b0000_0111, 0b0011_1000, 0b1100_0000, 0xFF], &mut dst);
        assert_eq!(dst, [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]);
        // 4 of 7 is 146 of 255; 1 of 3 is 85.
        let mut one = [0u8; 4];
        w.to_rgba(&[0b0100_0100], &mut one);
        assert_eq!(one, [146, 0, 85, 255]);
    }

    #[test]
    fn other_layouts_of_the_server_are_read_by_their_own_masks() {
        // Big-endian RGB565.
        let be = PixelFormat::try_from([16, 16, 1, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0]).unwrap();
        let w = Wire::new(&be).unwrap();
        let mut dst = [0u8; 8];
        w.to_rgba(&[0xF8, 0x00, 0x00, 0x1F], &mut dst);
        assert_eq!(dst, [255, 0, 0, 255, 0, 0, 255, 255]);
        // 32 bits, blue first (the other full-colour order): converted, not copied.
        let bgra = Wire::new(&PixelFormat::bgra()).unwrap();
        let mut dst = [0u8; 4];
        bgra.to_rgba(&[1, 2, 3, 0], &mut dst);
        assert_eq!(dst, [3, 2, 1, 255]);
        // A palette format (not true colour) and an odd width are not taken.
        let mut palette = [8, 8, 0, 0, 0, 7, 0, 7, 0, 3, 0, 3, 6, 0, 0, 0];
        assert!(PixelFormat::try_from(palette).is_ok_and(|pf| Wire::new(&pf).is_none()));
        palette[0] = 24;
        assert!(PixelFormat::try_from(palette).is_err());
    }

    #[test]
    fn a_picture_in_sixteen_bits_is_drawn_from_its_own_rows() {
        let mut s = Screen::with_wire(Wire::new(&with(Encoding::Auto, 16, MAX_FPS).pixel_format().unwrap()).unwrap());
        s.resize(3, 2);
        // A part 2 wide at x=2: one column is inside, the other past the edge. Each row of the data
        // is two pixels (four bytes) wide, as the server sent it; only the first is drawn.
        let red = [0x00, 0xF8];
        let blue = [0x1F, 0x00];
        s.put(Rect { x: 2, y: 0, width: 2, height: 2 }, &[red, blue, blue, red].concat());
        assert_eq!(&s.rgba[8..12], &[255, 0, 0, 255], "row 0, x=2");
        assert_eq!(&s.rgba[20..24], &[0, 0, 255, 255], "row 1, x=2: from the second row of the data");
        assert_eq!(&s.rgba[0..8], &[0; 8], "the rest of the picture is as it was");
        // Data that ends early stops where it ends.
        s.put(Rect { x: 0, y: 0, width: 3, height: 2 }, &[red, red, red].concat());
        assert_eq!(&s.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&s.rgba[12..16], &[0; 4], "the second row wasn't in the data");
        let f = s.frames();
        assert!(!f.is_empty());
    }

    /// A clock for the pacer's tests: `at(k)` is `k` milliseconds after the start.
    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn at(&self, ms: u64) -> Instant {
            self.0 + Duration::from_millis(ms)
        }
    }

    #[test]
    fn with_no_limit_it_asks_as_often_as_before() {
        let c = Clock::new();
        let mut p = Pacer::new(Duration::ZERO);
        let mut t = 0;
        let mut tick = |p: &mut Pacer, changed, held| {
            t += TICK_MS;
            p.tick(c.at(t), changed, held)
        };
        // Nothing came: every sixth tick.
        let asks: Vec<bool> = (0..12).map(|_| tick(&mut p, false, false)).collect();
        assert_eq!(asks.iter().filter(|a| **a).count(), 2);
        assert!(asks[5] && asks[11]);
        // Something came on a tick: asked on it, and so on while it keeps coming.
        assert!((0..10).all(|_| tick(&mut p, true, false)));
        // The page is behind: nothing is asked, however long, and the ask is made as soon as it has caught up.
        assert!((0..30).all(|_| !tick(&mut p, false, true)));
        assert!(tick(&mut p, false, false));
        assert!(!tick(&mut p, false, false));
    }

    /// How many asks a busy picture (an update on every tick) gets in `seconds`, with ticks `tick_ms` apart.
    fn asks_in(fps: u8, tick_ms: u64, seconds: u64) -> usize {
        let c = Clock::new();
        let mut p = Pacer::new(with(Encoding::Auto, 32, fps).gap());
        (1..=seconds * 1000 / tick_ms).filter(|k| p.tick(c.at(k * tick_ms), true, false)).count()
    }

    #[test]
    fn a_limit_spaces_the_asks_by_time_whatever_the_tick() {
        // The tick is 16 ms when the timer is even and 31 when it is the Windows one: the rate is about the same.
        for (fps, seconds) in [(30u8, 10u64), (15, 10), (5, 20)] {
            let nominal = usize::from(fps) * seconds as usize;
            for tick_ms in [16u64, 31] {
                let n = asks_in(fps, tick_ms, seconds);
                assert!(n * 100 >= nominal * 85 && n * 100 <= nominal * 112, "{fps} a second, {tick_ms} ms ticks: {n} asks in {seconds} s, about {nominal} wanted");
            }
        }
        // No limit: one ask on every tick that has an update.
        assert_eq!(asks_in(60, 16, 2), 125);
    }

    #[test]
    fn what_was_wanted_between_two_asks_is_asked_when_the_gap_is_over() {
        let c = Clock::new();
        let mut p = Pacer::new(Duration::from_millis(200));
        assert!(p.tick(c.at(16), true, false), "the first ask is at once");
        assert!(!p.tick(c.at(32), true, false));
        // The update that came at 32 ms is not forgotten, though nothing comes after it.
        for k in 3..12 {
            assert!(!p.tick(c.at(k * 16), false, false), "{} ms", k * 16);
        }
        assert!(p.tick(c.at(16 + 192), false, false), "ask at the gap, less half a tick");
        assert!(!p.tick(c.at(16 + 192 + 16), false, false));
        // Quiet, with a limit: still a few times a second, no oftener than the limit.
        let mut p = Pacer::new(Duration::from_millis(1000));
        let asks = (1..=1000u64).filter(|k| p.tick(c.at(k * 16), false, false)).count();
        assert!((15..=17).contains(&asks), "{asks} asks in 16 s of quiet at 1 a second");
    }

    /// What the scripted server learned about its client.
    #[derive(Default, Debug)]
    struct Heard {
        pixel_format: Option<[u8; 16]>,
        encodings: Vec<i32>,
        asks: u32,
    }

    /// A pixel as the server was asked to send it.
    fn wire_pixel(pf: &[u8; 16], (r, g, b): (u8, u8, u8)) -> Vec<u8> {
        let max = |at: usize| u32::from(u16::from_be_bytes([pf[at], pf[at + 1]]));
        let part = |value: u8, at: usize, shift: u8| (u32::from(value) * max(at) / 255) << shift;
        let v = part(r, 4, pf[10]) | part(g, 6, pf[11]) | part(b, 8, pf[12]);
        let all = if pf[2] != 0 { v.to_be_bytes() } else { v.to_le_bytes() };
        let n = usize::from(pf[0] / 8);
        if pf[2] != 0 { all[4 - n..].to_vec() } else { all[..n].to_vec() }
    }

    /// A server with no password and a 4×2 desktop. It writes down the pixel
    /// format and the encodings the client asks for, and answers the first ask
    /// for an update with a row of `pixels` in the format that was asked for.
    async fn scripted_server(mut s: tokio::io::DuplexStream, pixels: [(u8, u8, u8); 2], heard: Arc<Mutex<Heard>>) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(b"RFB 003.008\n").await?;
        s.read_exact(&mut [0u8; 12]).await?;
        s.write_all(&[1, 1]).await?; // one way to log in: none
        s.read_exact(&mut [0u8; 1]).await?;
        s.write_all(&0u32.to_be_bytes()).await?; // accepted
        s.read_exact(&mut [0u8; 1]).await?; // ClientInit
        let mut init = Vec::new();
        init.extend_from_slice(&4u16.to_be_bytes());
        init.extend_from_slice(&2u16.to_be_bytes());
        init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        init.extend_from_slice(&5u32.to_be_bytes());
        init.extend_from_slice(b"fake!");
        s.write_all(&init).await?;
        loop {
            let mut t = [0u8; 1];
            s.read_exact(&mut t).await?;
            match t[0] {
                0 => {
                    let mut m = [0u8; 19];
                    s.read_exact(&mut m).await?;
                    let mut pf = [0u8; 16];
                    pf.copy_from_slice(&m[3..]);
                    heard.lock().unwrap().pixel_format = Some(pf);
                }
                2 => {
                    let mut h = [0u8; 3];
                    s.read_exact(&mut h).await?;
                    for _ in 0..u16::from_be_bytes([h[1], h[2]]) {
                        let mut e = [0u8; 4];
                        s.read_exact(&mut e).await?;
                        heard.lock().unwrap().encodings.push(i32::from_be_bytes(e));
                    }
                }
                3 => {
                    s.read_exact(&mut [0u8; 9]).await?;
                    let (first, pf) = {
                        let mut h = heard.lock().unwrap();
                        h.asks += 1;
                        (h.asks == 1, h.pixel_format)
                    };
                    if let (true, Some(pf)) = (first, pf) {
                        // One raw rectangle: the first row, two pixels.
                        let mut u = vec![0u8, 0, 0, 1];
                        for v in [0u16, 0, 2, 1] {
                            u.extend_from_slice(&v.to_be_bytes());
                        }
                        u.extend_from_slice(&0i32.to_be_bytes());
                        for p in pixels {
                            u.extend_from_slice(&wire_pixel(&pf, p));
                        }
                        s.write_all(&u).await?;
                    }
                }
                4 => {
                    s.read_exact(&mut [0u8; 7]).await?;
                }
                5 => {
                    s.read_exact(&mut [0u8; 5]).await?;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Runs the real session against the scripted server until the first
    /// picture is drawn; what the server heard, and the frames the page got.
    async fn session_against_scripted_server(options: Options, pixels: [(u8, u8, u8); 2]) -> (Heard, Vec<Vec<u8>>) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let heard = Arc::new(Mutex::new(Heard::default()));
        let srv = tokio::spawn(scripted_server(server, pixels, Arc::clone(&heard)));
        let frames = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = Arc::clone(&frames);
        let run = tokio::spawn(run(
            client,
            std::future::ready(None::<Zeroizing<String>>),
            |_, _| {},
            move |f| {
                sink.lock().unwrap().push(f);
                true
            },
            Arc::new(crate::screen_flow::Flow::default()),
            rx,
            (4, 2),
            options,
        ));
        for _ in 0..500 {
            if frames.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = tx.send(Cmd::Close);
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), run).await.expect("closed in time").expect("no panic");
        assert_eq!(out, Ok("closed".to_string()));
        srv.abort();
        let heard = std::mem::take(&mut *heard.lock().unwrap());
        let frames = frames.lock().unwrap().clone();
        (heard, frames)
    }

    // The whole session, with the real client library: what is asked of the
    // server on the wire, and what is drawn from what it sends back.
    #[tokio::test]
    async fn the_session_asks_for_the_chosen_format_and_draws_what_comes_back() {
        let pixels = [(255, 0, 0), (0, 0, 255)];
        let expected = [255, 0, 0, 255, 0, 0, 255, 255];
        for (options, bits, first_encoding) in [
            (Options::default(), 32u8, 7),
            (with(Encoding::Raw, 32, 60), 32, 0),
            (with(Encoding::Zrle, 16, 60), 16, 16),
            (with(Encoding::Auto, 8, 30), 8, 16),
            (with(Encoding::Tight, 16, 60), 16, 16),
        ] {
            let (heard, frames) = session_against_scripted_server(options, pixels).await;
            let pf = heard.pixel_format.unwrap_or_else(|| panic!("{options:?}: no pixel format was asked for"));
            assert_eq!(pf[0], bits, "{options:?}: bits a pixel");
            assert_eq!(heard.encodings[0], first_encoding, "{options:?}: {:?}", heard.encodings);
            assert!(heard.encodings.contains(&0), "Raw is always offered: {:?}", heard.encodings);
            assert_eq!(heard.encodings.contains(&7), bits == 32, "Tight only in full colour: {:?}", heard.encodings);
            assert_eq!(frames[0], [1, 4, 0, 2, 0], "the desktop's size comes first");
            let picture = &frames[1];
            assert_eq!(&picture[..9], &[2, 0, 0, 0, 0, 2, 0, 1, 0], "{options:?}");
            assert_eq!(&picture[9..], &expected, "{options:?}: the two pixels as drawn");
        }
    }
}
