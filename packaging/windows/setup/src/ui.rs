//! The window: borderless, the page (ui/) in WebView2. The page talks through
//! window.ipc.postMessage; work on other threads reaches the page and the
//! handler through a Remote.
use std::ffi::c_void;
use std::time::Duration;

use serde_json::Value;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{IconExtWindows, WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Icon, Theme, Window, WindowBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use wry::{NewWindowResponse, WebContext, WebViewBuilder, WebViewBuilderExtWindows};

use crate::{log, native, webview2};

const PAGE: &str = include_str!(concat!(env!("OUT_DIR"), "/page.html"));

pub enum UserEvent<T> {
    Ipc(String),
    Script(String),
    App(T),
    Exit(i32),
}

/// Reaches the window from any thread.
pub struct Remote<T: 'static>(EventLoopProxy<UserEvent<T>>);

impl<T> Clone for Remote<T> {
    fn clone(&self) -> Self {
        Remote(self.0.clone())
    }
}

impl<T: 'static> Remote<T> {
    /// An event for the page (window.__setupEvent).
    pub fn page(&self, event: Value) {
        let _ = self.0.send_event(UserEvent::Script(format!("window.__setupEvent({event})")));
    }
    /// A message for the handler, on the window's thread.
    pub fn app(&self, msg: T) {
        let _ = self.0.send_event(UserEvent::App(msg));
    }
    pub fn exit(&self, code: i32) {
        let _ = self.0.send_event(UserEvent::Exit(code));
    }
}

/// What the handler is told.
pub enum In<T> {
    /// A message from the page: {"cmd": ..., ...}
    Page(Value),
    /// The window's close button, Alt+F4.
    CloseRequested,
    App(T),
}

pub struct Ctx<'a, T: 'static> {
    pub window: &'a Window,
    pub remote: Remote<T>,
    exit: &'a mut Option<i32>,
}

impl<T: 'static> Ctx<'_, T> {
    pub fn hwnd(&self) -> HWND {
        HWND(self.window.hwnd() as *mut c_void)
    }
    pub fn exit(&mut self, code: i32) {
        *self.exit = Some(code);
    }
    pub fn page(&self, event: Value) {
        self.remote.page(event);
    }
}

pub struct Config {
    pub title: String,
    /// window.__SETUP__ for the page.
    pub init: Value,
}

fn style(window: &Window) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };
    let hwnd = HWND(window.hwnd() as *mut c_void);
    unsafe {
        // Have Windows measure the frame again now that it is borderless: until
        // then the client area can keep a native title bar's room above the page.
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        // Windows 11: rounded corners and a thin border in the app's line colour.
        // Windows 10 ignores both.
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as *const c_void, 4);
        let border: u32 = 0x0030_2a2a;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const c_void, 4);
    }
}

fn place(window: &Window) {
    #[cfg(feature = "lab")]
    if let Some((x, y)) = std::env::var("SUBMARINE_SETUP_POS").ok().and_then(|v| {
        let (x, y) = v.split_once(',')?;
        Some((x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?))
    }) {
        window.set_outer_position(PhysicalPosition::new(x, y));
        return;
    }
    if let Some(m) = window.current_monitor() {
        let (ms, mp, ws) = (m.size(), m.position(), window.outer_size());
        let x = mp.x + (ms.width as i32 - ws.width as i32) / 2;
        let y = mp.y + (ms.height as i32 - ws.height as i32) / 2;
        window.set_outer_position(PhysicalPosition::new(x, y));
    }
}

fn show(window: &Window, webview: &wry::WebView) {
    // Lab builds placed by the tests: shown without taking the focus, behind
    // other windows, so they don't get in the way of someone working.
    #[cfg(feature = "lab")]
    if std::env::var_os("SUBMARINE_SETUP_POS").is_some() {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, ShowWindow, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
        };
        let hwnd = HWND(window.hwnd() as *mut c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
        return;
    }
    window.set_visible(true);
    window.set_focus();
    let _ = webview.focus();
}

/// Shows the window and runs until the handler exits; returns its exit code.
pub fn run<T: 'static>(cfg: Config, mut handler: impl FnMut(&mut Ctx<T>, In<T>)) -> Result<i32, String> {
    webview2::clear_stale_data_dirs();
    let mut event_loop = EventLoopBuilder::<UserEvent<T>>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let icon = Icon::from_resource(1, None).ok();
    let window = WindowBuilder::new()
        .with_title(&cfg.title)
        .with_inner_size(LogicalSize::new(560.0, 400.0))
        .with_resizable(false)
        .with_maximizable(false)
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .with_visible(false)
        .with_window_icon(icon.clone())
        .with_taskbar_icon(icon)
        .with_window_classname(native::window_class())
        .with_theme(Some(Theme::Dark))
        .build(&event_loop)
        .map_err(|e| e.to_string())?;
    style(&window);
    place(&window);

    let data_dir = webview2::data_dir();
    let mut context = WebContext::new(Some(data_dir.clone()));
    let ipc = proxy.clone();
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(PAGE)
        .with_initialization_script(format!("window.__SETUP__ = {};", cfg.init))
        .with_ipc_handler(move |req| {
            let _ = ipc.send_event(UserEvent::Ipc(req.into_body()));
        })
        // The page never goes anywhere: no links, no other pages, no windows.
        .with_navigation_handler(|url| url.starts_with("about:") || url.starts_with("data:"))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_drag_drop_handler(|_| true)
        .with_devtools(cfg!(feature = "lab"))
        .with_hotkeys_zoom(false)
        .with_background_color((10, 10, 12, 255))
        .with_browser_accelerator_keys(false)
        .with_default_context_menus(false)
        .with_theme(wry::Theme::Dark);
    // Lab builds only: a DevTools port for the tests.
    #[cfg(feature = "lab")]
    let builder = match std::env::var("SUBMARINE_SETUP_CDP") {
        Ok(port) => builder.with_additional_browser_args(format!(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --remote-debugging-port={port}"
        )),
        Err(_) => builder,
    };
    let webview = builder.build(&window).map_err(|e| e.to_string())?;

    let remote = Remote(proxy);
    let mut exit: Option<i32> = None;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::Wait;
        {
            let mut ctx = Ctx { window: &window, remote: remote.clone(), exit: &mut exit };
            match event {
                Event::UserEvent(UserEvent::Ipc(body)) => {
                    let msg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    match msg["cmd"].as_str() {
                        Some("ready") => show(&window, &webview),
                        Some("drag") => {
                            let _ = window.drag_window();
                        }
                        Some("minimize") => window.set_minimized(true),
                        Some(_) => handler(&mut ctx, In::Page(msg)),
                        None => {}
                    }
                }
                Event::UserEvent(UserEvent::Script(js)) => {
                    let _ = webview.evaluate_script(&js);
                }
                Event::UserEvent(UserEvent::App(msg)) => handler(&mut ctx, In::App(msg)),
                Event::UserEvent(UserEvent::Exit(code)) => ctx.exit(code),
                Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => handler(&mut ctx, In::CloseRequested),
                _ => {}
            }
        }
        if exit.is_some() {
            *flow = ControlFlow::Exit;
        }
    });
    window.set_visible(false);
    drop(webview);
    drop(context);
    drop(window);
    // WebView2's processes take a moment to let go of the folder (the window is
    // already gone; what is left is cleared by the next run otherwise).
    for _ in 0..80 {
        match std::fs::remove_dir_all(&data_dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => std::thread::sleep(Duration::from_millis(100)),
            _ => break,
        }
    }
    if data_dir.exists() {
        log!("couldn't remove {}", data_dir.display());
    }
    Ok(exit.unwrap_or(1))
}
