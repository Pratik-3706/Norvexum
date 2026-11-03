// ===========================================================================
// Browser — one shared Chromium session for the agent and the GUI
//
// The agent drives it with browser_interact; the GUI shows it live in its
// browser panel (the browser then runs headless) and forwards the user's
// clicks, scrolling, typing and navigation to the same tab.
//
// The GUI never waits on the session lock (the agent holds it for whole page
// loads): it reads the open tab from TAB and the picture from FRAME, both
// held only for a moment. While the panel is visible the page streams frames
// with CDP screencast, decoded off the UI thread; panel input goes through one
// worker thread in order.
//
// The page is pinned to VIEW_WIDTH×VIEW_HEIGHT CSS pixels at scale factor 1,
// so screenshot pixels and click coordinates are the same thing whatever the
// display scaling. (At 140% scaling they used to differ by 1.4× and clicks
// landed off target.)
// ===========================================================================

use async_trait::async_trait;
use headless_chrome::protocol::cdp::types::Event;
use headless_chrome::protocol::cdp::{Emulation, Input, Page};
use headless_chrome::{Browser, LaunchOptions, Tab};
use once_cell::sync::Lazy;
use serde_json::json;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::{Tool, ToolContext, ToolResult};

/// Page size the agent sees and clicks in.
pub const VIEW_WIDTH: u32 = 1280;
pub const VIEW_HEIGHT: u32 = 800;

struct Session {
    // Kept alive: dropping it closes the browser.
    _browser: Browser,
    tab: Arc<Tab>,
}

static SESSION: Lazy<Mutex<Option<Session>>> = Lazy::new(|| Mutex::new(None));

fn session() -> MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// The open tab, for readers that must not wait on SESSION.
static TAB: Lazy<Mutex<Option<Arc<Tab>>>> = Lazy::new(|| Mutex::new(None));

fn current_tab() -> Option<Arc<Tab>> {
    TAB.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set_current_tab(tab: Option<Arc<Tab>>) {
    *TAB.lock().unwrap_or_else(|p| p.into_inner()) = tab;
}

/// Called when a new frame is ready (the GUI repaints).
static ON_FRAME: once_cell::sync::OnceCell<Box<dyn Fn() + Send + Sync>> =
    once_cell::sync::OnceCell::new();

/// Run `notify` whenever a new picture of the page is ready.
pub fn on_new_frame(notify: impl Fn() + Send + Sync + 'static) {
    let _ = ON_FRAME.set(Box::new(notify));
}

/// Set by the GUI: the browser runs headless and is shown in its own panel.
static IN_APP: AtomicBool = AtomicBool::new(false);
/// The agent is in the middle of a browser action.
static AGENT_BUSY: AtomicBool = AtomicBool::new(false);
/// The GUI's browser panel is on screen (keep its picture fresh).
static LIVE_VIEW: AtomicBool = AtomicBool::new(false);

/// Show the browser inside the app instead of as a separate window. Applies
/// from the next launch.
pub fn show_in_app(on: bool) {
    IN_APP.store(on, Ordering::SeqCst);
}

/// The live view streams screencast frames (false: falls back to polling).
static CASTING: AtomicBool = AtomicBool::new(false);

/// Whether the GUI's browser panel is visible; while it is, the page streams
/// a new frame whenever it changes.
pub fn set_live_view(on: bool) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    LIVE_VIEW.store(on, Ordering::SeqCst);
    if on {
        STARTED.call_once(|| {
            std::thread::spawn(live_view_loop);
        });
    }
}

/// Keeps a screencast running on the open tab while the panel is visible.
fn live_view_loop() {
    let (frames_tx, frames_rx) = mpsc::channel::<(Arc<Tab>, String, u32)>();
    std::thread::spawn(move || screencast_frames(frames_rx));
    // The tab being cast and its listener.
    let mut cast: Option<(
        Arc<Tab>,
        std::sync::Weak<dyn headless_chrome::browser::tab::EventListener<Event> + Send + Sync>,
    )> = None;
    loop {
        let want = LIVE_VIEW.load(Ordering::SeqCst);
        let tab = if want { current_tab() } else { None };
        let same = matches!((&cast, &tab), (Some((a, _)), Some(b)) if Arc::ptr_eq(a, b));
        if !same {
            if let Some((old, listener)) = cast.take() {
                let _ = old.stop_screencast();
                let _ = old.remove_event_listener(&listener);
                CASTING.store(false, Ordering::SeqCst);
            }
            if let Some(tab) = tab.clone() {
                let tx = Mutex::new(frames_tx.clone());
                let owner = tab.clone();
                let listener = move |event: &Event| {
                    if let Event::PageScreencastFrame(frame) = event {
                        let _ = tx.lock().unwrap_or_else(|p| p.into_inner()).send((
                            owner.clone(),
                            frame.params.data.clone(),
                            frame.params.session_id,
                        ));
                    }
                };
                let started = tab.add_event_listener(Arc::new(listener)).and_then(|weak| {
                    tab.start_screencast(
                        Some(Page::StartScreencastFormatOption::Jpeg),
                        Some(70),
                        Some(VIEW_WIDTH),
                        Some(VIEW_HEIGHT),
                        Some(1),
                    )
                    .map(|_| weak)
                });
                match started {
                    Ok(weak) => {
                        CASTING.store(true, Ordering::SeqCst);
                        cast = Some((tab, weak));
                    }
                    Err(e) => tracing::warn!("Screencast unavailable, polling instead: {e}"),
                }
            }
        }
        if want && cast.is_none() {
            // No screencast: poll.
            refresh_frame();
            std::thread::sleep(Duration::from_millis(200));
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Acknowledge screencast frames at once (Chrome sends the next one only
/// after that) and hand them to a decoder thread.
fn screencast_frames(rx: mpsc::Receiver<(Arc<Tab>, String, u32)>) {
    let (decode_tx, decode_rx) = mpsc::channel::<(Arc<Tab>, String)>();
    std::thread::spawn(move || decode_frames(decode_rx));
    while let Ok((tab, data, session_id)) = rx.recv() {
        let _ = tab.ack_screencast(session_id);
        if decode_tx.send((tab, data)).is_err() {
            return;
        }
    }
}

/// Decode and publish frames; when they pile up only the newest is decoded.
fn decode_frames(rx: mpsc::Receiver<(Arc<Tab>, String)>) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut title = (String::new(), String::new()); // (url, title)
    while let Ok(mut next) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            next = newer;
        }
        let (tab, data) = next;
        let Ok(jpeg) = STANDARD.decode(data.as_bytes()) else {
            continue;
        };
        let url = tab.get_url();
        if url != title.0 {
            title = (url.clone(), tab.get_title().unwrap_or_default());
        }
        publish_frame(jpeg, url, title.1.clone());
    }
}

pub fn is_open() -> bool {
    current_tab().is_some()
}

pub fn agent_busy() -> bool {
    AGENT_BUSY.load(Ordering::SeqCst)
}

/// The most recent picture of the page.
pub struct Frame {
    pub jpeg: Vec<u8>,
    /// Decoded pixels (RGBA, `width`×`height`), ready for a texture.
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// Increases with every new frame.
    pub seq: u64,
    pub url: String,
    pub title: String,
}

static FRAME: Lazy<Mutex<Option<Arc<Frame>>>> = Lazy::new(|| Mutex::new(None));
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn latest_frame() -> Option<Arc<Frame>> {
    FRAME.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn store_frame(tab: &Tab, jpeg: Vec<u8>) {
    publish_frame(jpeg, tab.get_url(), tab.get_title().unwrap_or_default());
}

/// Decode `jpeg` (on the calling thread, never the UI's) and make it the
/// current picture.
fn publish_frame(jpeg: Vec<u8>, url: String, title: String) {
    let Ok(img) = image::load_from_memory(&jpeg) else {
        return;
    };
    let rgba = img.to_rgba8();
    let frame = Frame {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        rgba: rgba.into_raw(),
        jpeg,
        seq: FRAME_SEQ.fetch_add(1, Ordering::SeqCst) + 1,
        url,
        title,
    };
    *FRAME.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(frame));
    if let Some(notify) = ON_FRAME.get() {
        notify();
    }
}

fn clear_frame() {
    *FRAME.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

fn capture(tab: &Tab, quality: u32) -> Result<Vec<u8>, String> {
    tab.capture_screenshot(
        Page::CaptureScreenshotFormatOption::Jpeg,
        Some(quality),
        None, // the viewport only
        true,
    )
    .map_err(|e| format!("Failed to capture screenshot: {e}"))
}

/// Re-capture the page for the live view.
pub fn refresh_frame() {
    if let Some(tab) = current_tab() {
        if let Ok(jpeg) = capture(&tab, 60) {
            store_frame(&tab, jpeg);
        }
    }
}

/// Pin the page to VIEW_WIDTH×VIEW_HEIGHT at scale 1 (see the module comment).
fn pin_viewport(tab: &Tab) -> Result<(), String> {
    tab.call_method(Emulation::SetDeviceMetricsOverride {
        width: VIEW_WIDTH,
        height: VIEW_HEIGHT,
        device_scale_factor: 1.0,
        mobile: false,
        scale: None,
        screen_width: None,
        screen_height: None,
        position_x: None,
        position_y: None,
        dont_set_visible_size: None,
        screen_orientation: None,
        viewport: None,
        display_feature: None,
        device_posture: None,
    })
    .map(|_| ())
    .map_err(|e| format!("Failed to set the page size: {e}"))
}

/// Start a browser: the preferred installed one that launches, else the one
/// headless_chrome finds itself (no download: an installed browser is used).
fn launch(timeout: Duration) -> Result<Session, String> {
    let headless = IN_APP.load(Ordering::SeqCst);
    let mut last_error = String::new();
    for path in browser_candidates().into_iter().map(Some).chain([None]) {
        let options = LaunchOptions {
            path: path.clone(),
            headless,
            // Room for the window frame and toolbar around the pinned page.
            window_size: Some(if headless {
                (VIEW_WIDTH, VIEW_HEIGHT)
            } else {
                (VIEW_WIDTH + 16, VIEW_HEIGHT + 140)
            }),
            idle_browser_timeout: Duration::from_secs(3600),
            // Render on the GPU when there is one; Chrome falls back to
            // software rendering by itself when there isn't.
            enable_gpu: true,
            ..Default::default()
        };
        match Browser::new(options) {
            Ok(browser) => {
                if let Some(path) = &path {
                    tracing::info!("Browser: {} (headless: {headless})", path.display());
                }
                let tab = browser
                    .new_tab()
                    .map_err(|e| format!("Failed to open tab: {e}"))?;
                tab.set_default_timeout(timeout);
                pin_viewport(&tab)?;
                BROWSER_PID.store(browser.get_process_id().unwrap_or(0), Ordering::SeqCst);
                return Ok(Session {
                    _browser: browser,
                    tab,
                });
            }
            Err(e) => {
                let which = path
                    .as_ref()
                    .map_or("default".into(), |p| p.display().to_string());
                tracing::warn!("Couldn't launch {which}: {e}");
                last_error = e.to_string();
            }
        }
    }
    Err(format!("Failed to launch browser: {last_error}"))
}

/// The open tab, launching the browser first if needed.
fn ensure_tab(session: &mut Option<Session>, timeout: Duration) -> Result<Arc<Tab>, String> {
    if session.is_none() {
        let launched = launch(timeout)?;
        set_current_tab(Some(launched.tab.clone()));
        *session = Some(launched);
    }
    let tab = session.as_ref().map(|s| s.tab.clone()).expect("just set");
    tab.set_default_timeout(timeout);
    Ok(tab)
}

/// Process id of the running browser (0: none).
static BROWSER_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// End the browser process at once, without the session lock. An action
/// stuck on the page (the agent's or the panel's) then fails and lets go.
fn kill_browser() {
    set_current_tab(None);
    clear_frame();
    let pid = BROWSER_PID.swap(0, Ordering::SeqCst);
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

fn close(session: &mut Option<Session>) {
    kill_browser();
    drop(session.take());
}

/// Close the browser from the panel: never waits behind a running action.
pub fn force_close() {
    std::thread::spawn(|| {
        kill_browser();
        // Drop the session once whoever holds it lets go.
        drop(session().take());
    });
}

fn mouse(
    tab: &Tab,
    kind: Input::DispatchMouseEventTypeOption,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let button = match kind {
        Input::DispatchMouseEventTypeOption::MousePressed
        | Input::DispatchMouseEventTypeOption::MouseReleased => Some(Input::MouseButton::Left),
        _ => None,
    };
    tab.call_method(Input::DispatchMouseEvent {
        Type: kind,
        x,
        y,
        click_count: button.as_ref().map(|_| 1),
        button,
        modifiers: None,
        timestamp: None,
        buttons: None,
        force: None,
        tangential_pressure: None,
        tilt_x: None,
        tilt_y: None,
        twist: None,
        delta_x: None,
        delta_y: None,
        pointer_Type: None,
    })
    .map(|_| ())
    .map_err(|e| format!("Mouse event failed: {e}"))
}

fn click(tab: &Tab, x: f64, y: f64) -> Result<(), String> {
    use Input::DispatchMouseEventTypeOption::*;
    mouse(tab, MouseMoved, x, y)?;
    std::thread::sleep(Duration::from_millis(15));
    mouse(tab, MousePressed, x, y)?;
    std::thread::sleep(Duration::from_millis(15));
    mouse(tab, MouseReleased, x, y)
}

/// Scroll whatever is under (x, y), like a mouse wheel; this also reaches
/// scrollable panels that `window.scrollBy` would miss.
fn wheel(tab: &Tab, x: f64, y: f64, dx: f64, dy: f64) -> Result<(), String> {
    tab.call_method(Input::DispatchMouseEvent {
        Type: Input::DispatchMouseEventTypeOption::MouseWheel,
        x,
        y,
        button: None,
        click_count: None,
        modifiers: None,
        timestamp: None,
        buttons: None,
        force: None,
        tangential_pressure: None,
        tilt_x: None,
        tilt_y: None,
        twist: None,
        delta_x: Some(dx),
        delta_y: Some(dy),
        pointer_Type: None,
    })
    .map(|_| ())
    .map_err(|e| format!("Failed to scroll: {e}"))
}

fn navigate(tab: &Tab, url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let has_scheme = url.contains("://")
        || [
            "about:",
            "data:",
            "file:",
            "chrome:",
            "view-source:",
            "javascript:",
        ]
        .iter()
        .any(|s| lower.starts_with(s));
    let url = if has_scheme {
        url.to_string()
    } else if lower.starts_with("localhost") || lower.starts_with("127.0.0.1") {
        format!("http://{url}")
    } else if url.contains('.') && !url.contains(' ') {
        format!("https://{url}")
    } else {
        format!(
            "https://www.google.com/search?q={}",
            url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
        )
    };
    tab.navigate_to(&url)
        .map_err(|e| format!("Failed to navigate: {e}"))?;
    wait_until_ready(tab, PAGE_READY_WAIT);
    Ok(())
}

/// Longest wait for a page to finish loading before carrying on with it.
const PAGE_READY_WAIT: Duration = Duration::from_secs(15);

/// Wait until the document has loaded, at most `limit`. Not "network idle":
/// pages that keep connections open (chat, ads, live data) never get there,
/// and waiting for it left the agent stuck until the timeout.
fn wait_until_ready(tab: &Tab, limit: Duration) {
    let started = std::time::Instant::now();
    std::thread::sleep(Duration::from_millis(150));
    while started.elapsed() < limit {
        let state = tab
            .evaluate("document.readyState", false)
            .ok()
            .and_then(|r| r.value)
            .and_then(|v| v.as_str().map(str::to_string));
        if state.as_deref() == Some("complete") {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// What the user does in the GUI's browser panel.
#[derive(Debug, Clone)]
pub enum UserAction {
    Navigate(String),
    Back,
    Forward,
    Reload,
    /// Page coordinates (CSS pixels in the pinned viewport).
    Click {
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        dy: f64,
    },
    Type(String),
    Key(String),
    Close,
}

/// Queue a panel action; one worker runs them in order and the panel picks
/// up the new frame when it is ready. Opening a page starts the browser if
/// needed. Never blocks the caller.
pub fn user_action(action: UserAction, timeout: Duration) {
    // Closing must work even while a queued action hangs.
    if matches!(action, UserAction::Close) {
        force_close();
        return;
    }
    static QUEUE: Lazy<Mutex<mpsc::Sender<(UserAction, Duration)>>> = Lazy::new(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || panel_worker(rx));
        Mutex::new(tx)
    });
    let _ = QUEUE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .send((action, timeout));
}

fn panel_worker(rx: mpsc::Receiver<(UserAction, Duration)>) {
    let mut pending = None;
    loop {
        let (mut action, timeout) = match pending.take() {
            Some(next) => next,
            None => match rx.recv() {
                Ok(next) => next,
                Err(_) => return,
            },
        };
        // A burst of wheel events becomes one scroll.
        if let UserAction::Scroll { dy, .. } = &mut action {
            while let Ok(next) = rx.try_recv() {
                match next {
                    (UserAction::Scroll { dy: more, .. }, _) => *dy += more,
                    other => {
                        pending = Some(other);
                        break;
                    }
                }
            }
        }
        run_panel_action(action, timeout);
    }
}

fn run_panel_action(action: UserAction, timeout: Duration) {
    // Starting or closing the browser needs the session; everything else
    // goes straight to the open tab, without waiting for the agent.
    let tab = match &action {
        UserAction::Close => {
            force_close();
            return;
        }
        UserAction::Navigate(_) if current_tab().is_none() => {
            match ensure_tab(&mut session(), timeout) {
                Ok(tab) => tab,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            }
        }
        _ => match current_tab() {
            Some(tab) => tab,
            None => return,
        },
    };
    {
        let result = match &action {
            UserAction::Navigate(url) => navigate(&tab, url),
            UserAction::Back => tab
                .evaluate("history.back()", false)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            UserAction::Forward => tab
                .evaluate("history.forward()", false)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            UserAction::Reload => tab
                .evaluate("location.reload()", false)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            UserAction::Click { x, y } => click(&tab, *x, *y),
            UserAction::Scroll { x, y, dy } => wheel(&tab, *x, *y, 0.0, *dy),
            UserAction::Type(text) => tab.type_str(text).map(|_| ()).map_err(|e| e.to_string()),
            UserAction::Key(key) => tab.press_key(key).map(|_| ()).map_err(|e| e.to_string()),
            UserAction::Close => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!("Browser panel action {action:?} failed: {e}");
        }
        // The screencast shows the result by itself; without it, capture.
        if !CASTING.load(Ordering::SeqCst) {
            let settle = match action {
                UserAction::Navigate(_)
                | UserAction::Back
                | UserAction::Forward
                | UserAction::Reload => 600,
                _ => 120,
            };
            std::thread::sleep(Duration::from_millis(settle));
            if let Ok(jpeg) = capture(&tab, 60) {
                store_frame(&tab, jpeg);
            }
        }
    }
}

/// Marks the agent as using the browser for as long as it lives.
struct AgentBusy;

impl AgentBusy {
    fn start() -> Self {
        AGENT_BUSY.store(true, Ordering::SeqCst);
        AgentBusy
    }
}

impl Drop for AgentBusy {
    fn drop(&mut self) {
        AGENT_BUSY.store(false, Ordering::SeqCst);
    }
}

pub struct BrowserInteractTool;

#[async_trait]
impl Tool for BrowserInteractTool {
    fn name(&self) -> &str {
        "browser_interact"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: Lazy<String> = Lazy::new(|| {
            format!(
                "Drive a persistent Chromium browser (the user can watch it). Actions: navigate, click, \
                 type, key, scroll, screenshot, close. Every action returns a screenshot of the \
                 {VIEW_WIDTH}×{VIEW_HEIGHT} page; x/y for click and scroll are pixels in that screenshot. \
                 To fill a field, click it first, then type. file:/// paths open local HTML files without \
                 a server. Close the browser when a testing or lookup task is done; keep it open for \
                 ongoing tasks the user asked for (music, a video, a dashboard)."
            )
        });
        &DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "click", "type", "key", "scroll", "close", "screenshot"],
                    "description": "The action to perform."
                },
                "url": { "type": "string", "description": "For 'navigate': the URL to open" },
                "x": { "type": "number", "description": "For 'click' (and optionally 'scroll'): X in screenshot pixels" },
                "y": { "type": "number", "description": "For 'click' (and optionally 'scroll'): Y in screenshot pixels" },
                "text": { "type": "string", "description": "For 'type': text to type. For 'key': the key (Enter, Tab, Escape, Backspace, ArrowDown, …)" },
                "scroll_x": { "type": "number", "description": "For 'scroll': pixels to scroll right" },
                "scroll_y": { "type": "number", "description": "For 'scroll': pixels to scroll down (negative = up)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let action = args["action"].as_str().unwrap_or("screenshot").to_string();
        let url = args["url"].as_str().unwrap_or("").to_string();
        let x = args["x"].as_f64();
        let y = args["y"].as_f64();
        let text = args["text"].as_str().unwrap_or("").to_string();
        let scroll_x = args["scroll_x"].as_f64().unwrap_or(0.0);
        let scroll_y = args["scroll_y"].as_f64().unwrap_or(0.0);
        let screenshot_path = ctx
            .settings
            .project_root
            .join(crate::config::NORVEXUM_DIR)
            .join("latest_screenshot.jpg");
        // Settings → Advanced → Browser timeout: how long page loads and waits may take.
        let timeout = Duration::from_secs(ctx.settings.browser_timeout_secs.max(5));

        let action_inner = action.clone();
        let work = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>, String> {
            let _busy = AgentBusy::start();
            let mut guard = session();
            if action_inner == "close" {
                close(&mut guard);
                return Ok(None);
            }
            let tab = ensure_tab(&mut guard, timeout)?;
            let seq_before = latest_frame().map_or(0, |f| f.seq);
            let in_view = |v: Option<f64>, max: u32| v.map(|v| v.clamp(0.0, f64::from(max) - 1.0));
            match action_inner.as_str() {
                "navigate" => {
                    if url.is_empty() {
                        return Err("'url' is required for navigate".into());
                    }
                    navigate(&tab, &url)?;
                    std::thread::sleep(Duration::from_millis(400));
                }
                "click" => {
                    let (Some(cx), Some(cy)) = (in_view(x, VIEW_WIDTH), in_view(y, VIEW_HEIGHT))
                    else {
                        return Err("'x' and 'y' are required for click".into());
                    };
                    click(&tab, cx, cy)?;
                    show_click_marker(&tab, cx, cy);
                    std::thread::sleep(Duration::from_millis(700));
                }
                "type" => {
                    tab.type_str(&text)
                        .map_err(|e| format!("Failed to type: {e}"))?;
                    std::thread::sleep(Duration::from_millis(400));
                }
                "key" => {
                    tab.press_key(&text)
                        .map_err(|e| format!("Failed to press key {text}: {e}"))?;
                    std::thread::sleep(Duration::from_millis(500));
                }
                "scroll" => {
                    let cx = in_view(x, VIEW_WIDTH).unwrap_or(f64::from(VIEW_WIDTH) / 2.0);
                    let cy = in_view(y, VIEW_HEIGHT).unwrap_or(f64::from(VIEW_HEIGHT) / 2.0);
                    wheel(&tab, cx, cy, scroll_x, scroll_y)?;
                    std::thread::sleep(Duration::from_millis(500));
                }
                "screenshot" => {}
                other => return Err(format!("Unknown action: {other}")),
            }
            // While the panel streams the page, a frame from after the
            // action is as good as a capture and costs nothing.
            if action_inner != "screenshot" && CASTING.load(Ordering::SeqCst) {
                if let Some(frame) = latest_frame().filter(|f| f.seq > seq_before) {
                    return Ok(Some(frame.jpeg.clone()));
                }
            }
            let jpeg = capture(&tab, 75)?;
            store_frame(&tab, jpeg.clone());
            Ok(Some(jpeg))
        });
        // A browser that stops answering must not hold the agent forever:
        // past the limit it is killed (the next action starts a fresh one).
        let limit = timeout + PAGE_READY_WAIT + Duration::from_secs(10);
        let result = match tokio::time::timeout(limit, work).await {
            Ok(result) => result,
            Err(_) => {
                kill_browser();
                return ToolResult::err(format!(
                    "The browser stopped responding during '{action}' (no answer in {}s) and was                      restarted. Open the page again to continue.",
                    limit.as_secs()
                ));
            }
        };

        match result {
            Ok(Ok(None)) => ToolResult::ok("✅ Browser closed."),
            Ok(Ok(Some(jpeg))) => {
                let _ =
                    std::fs::create_dir_all(screenshot_path.parent().unwrap_or(&screenshot_path));
                let _ = std::fs::write(&screenshot_path, &jpeg);
                let page = latest_frame()
                    .map(|f| format!(" Page: {} — {}", f.title, f.url))
                    .unwrap_or_default();
                use base64::{Engine as _, engine::general_purpose::STANDARD};
                ToolResult::ok_with_image(
                    format!(
                        "✅ '{action}' done.{page} The {VIEW_WIDTH}×{VIEW_HEIGHT} screenshot is attached."
                    ),
                    STANDARD.encode(&jpeg),
                    "image/jpeg".to_string(),
                )
            }
            Ok(Err(e)) => ToolResult::err(e),
            Err(e) => ToolResult::err(format!("Browser task panicked: {e}")),
        }
    }
}

/// A small dot where the agent clicked, so the screenshot shows the spot.
fn show_click_marker(tab: &Tab, x: f64, y: f64) {
    let script = format!(
        r#"(function() {{
            let dot = document.getElementById('norvexum-click');
            if (!dot) {{
                dot = document.createElement('div');
                dot.id = 'norvexum-click';
                dot.style.cssText = 'position:fixed;width:14px;height:14px;margin:-7px 0 0 -7px;' +
                    'border-radius:50%;background:rgba(122,146,240,.85);border:2px solid #fff;' +
                    'box-shadow:0 0 4px rgba(0,0,0,.5);z-index:2147483647;pointer-events:none';
                (document.body || document.documentElement).appendChild(dot);
            }}
            dot.style.left = '{x}px';
            dot.style.top = '{y}px';
        }})();"#
    );
    let _ = tab.evaluate(&script, false);
}

/// Chromium-family browsers to drive, best first: Chrome, then Brave, then any
/// other (Chromium, Vivaldi, Opera, Yandex), then Edge. A `CHROME=<path>`
/// environment variable goes before all of them. Only existing files are kept.
fn browser_candidates() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |path: PathBuf| {
        if path.is_file() && !found.contains(&path) {
            found.push(path);
        }
    };
    if let Some(path) = std::env::var_os("CHROME") {
        add(PathBuf::from(path));
    }
    for tier in browser_locations() {
        for path in tier {
            add(path);
        }
    }
    found
}

/// Where each preferred browser usually lives, one list per preference tier.
#[cfg(windows)]
fn browser_locations() -> [Vec<PathBuf>; 4] {
    let roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    let under = |relative: &[&str]| -> Vec<PathBuf> {
        relative
            .iter()
            .flat_map(|rel| roots.iter().map(move |root| root.join(rel)))
            .collect()
    };
    // Installers register their exe under "App Paths"; it covers custom folders.
    let registered = |exe: &str| -> Vec<PathBuf> {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        let key = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}");
        [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
            .into_iter()
            .filter_map(|hive| {
                RegKey::predef(hive)
                    .open_subkey(&key)
                    .and_then(|k| k.get_value::<String, _>(""))
                    .ok()
            })
            .map(|p| PathBuf::from(p.trim_matches('"')))
            .collect()
    };
    let chrome = [
        under(&[
            r"Google\Chrome\Application\chrome.exe",
            r"Google\Chrome Beta\Application\chrome.exe",
            r"Google\Chrome SxS\Application\chrome.exe",
        ]),
        // Chromium registers chrome.exe too; keep only Google's.
        registered("chrome.exe")
            .into_iter()
            .filter(|p| p.to_string_lossy().contains("Google"))
            .collect(),
    ]
    .concat();
    let brave = [
        registered("brave.exe"),
        under(&[
            r"BraveSoftware\Brave-Browser\Application\brave.exe",
            r"BraveSoftware\Brave-Browser-Beta\Application\brave.exe",
            r"BraveSoftware\Brave-Browser-Nightly\Application\brave.exe",
        ]),
    ]
    .concat();
    let other = [
        under(&[
            r"Chromium\Application\chrome.exe",
            r"Vivaldi\Application\vivaldi.exe",
            r"Programs\Opera\opera.exe",
            r"Programs\Opera GX\opera.exe",
            r"Yandex\YandexBrowser\Application\browser.exe",
        ]),
        registered("vivaldi.exe"),
        registered("chrome.exe"),
    ]
    .concat();
    let edge = [
        registered("msedge.exe"),
        under(&[r"Microsoft\Edge\Application\msedge.exe"]),
    ]
    .concat();
    [chrome, brave, other, edge]
}

#[cfg(not(windows))]
fn browser_locations() -> [Vec<PathBuf>; 4] {
    let on_path = |names: &[&str]| -> Vec<PathBuf> {
        names.iter().filter_map(|n| which::which(n).ok()).collect()
    };
    let apps = |names: &[&str]| -> Vec<PathBuf> {
        let home = dirs::home_dir().unwrap_or_default();
        names
            .iter()
            .flat_map(|app| {
                let exe = format!("{app}.app/Contents/MacOS/{app}");
                [
                    PathBuf::from("/Applications").join(&exe),
                    home.join("Applications").join(&exe),
                ]
            })
            .collect()
    };
    let chrome = [
        on_path(&[
            "google-chrome-stable",
            "google-chrome",
            "google-chrome-beta",
        ]),
        apps(&[
            "Google Chrome",
            "Google Chrome Beta",
            "Google Chrome Canary",
        ]),
    ]
    .concat();
    let brave = [
        on_path(&["brave-browser", "brave-browser-stable", "brave"]),
        apps(&["Brave Browser"]),
    ]
    .concat();
    let other = [
        on_path(&[
            "chromium",
            "chromium-browser",
            "vivaldi",
            "vivaldi-stable",
            "opera",
            "chrome",
        ]),
        apps(&["Chromium", "Vivaldi"]),
    ]
    .concat();
    let edge = [
        on_path(&["microsoft-edge-stable", "microsoft-edge", "msedge"]),
        apps(&["Microsoft Edge"]),
    ]
    .concat();
    [chrome, brave, other, edge]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Launches a real browser, so it only runs on request:
    /// cargo test --lib clicks_land_where_the_screenshot_says -- --ignored
    #[test]
    #[ignore]
    fn clicks_land_where_the_screenshot_says() {
        show_in_app(true);
        let mut guard = session();
        let tab = ensure_tab(&mut guard, Duration::from_secs(30)).unwrap();
        let page = r#"data:text/html,<body style='margin:0'><button style='position:absolute;left:1000px;top:600px;width:60px;height:40px' onclick='document.title="hit"'>x</button></body>"#;
        navigate(&tab, page).unwrap();
        click(&tab, 1030.0, 620.0).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let shot = image::load_from_memory(&capture(&tab, 75).unwrap()).unwrap();
        assert_eq!((shot.width(), shot.height()), (VIEW_WIDTH, VIEW_HEIGHT));
        assert_eq!(tab.get_title().unwrap(), "hit");
        close(&mut guard);
    }

    /// The agent's tool keeps working while the panel streams the page.
    /// Needs a real browser:
    /// cargo test --lib agent_actions_finish_while_the_panel_streams -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn agent_actions_finish_while_the_panel_streams() {
        show_in_app(true);
        set_live_view(true);
        let root = std::env::temp_dir().join(format!("nvx-browser-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let settings = crate::config::Settings {
            project_root: root.clone(),
            browser_timeout_secs: 20,
            ..crate::config::Settings::default()
        };
        let ctx = ToolContext {
            settings: Arc::new(settings),
            cwd: root,
            client: None,
            temp_memory: Arc::new(tokio::sync::RwLock::new(
                crate::tools::memory::TempMemory::new(),
            )),
            events: None,
            approvals_off: true,
            agent_name: None,
            inbox: None,
        };
        let page = "data:text/html,<body style='height:4000px'><button style='position:absolute;left:600px;top:300px;width:80px;height:40px' onclick='document.title=\"hit\"'>x</button><h1>hello</h1></body>";
        let steps = [
            json!({"action": "navigate", "url": page}),
            json!({"action": "screenshot"}),
            json!({"action": "click", "x": 640, "y": 320}),
            json!({"action": "scroll", "scroll_y": 600}),
            json!({"action": "type", "text": "abc"}),
            json!({"action": "key", "text": "Enter"}),
            json!({"action": "navigate", "url": "about:blank"}),
            json!({"action": "screenshot"}),
        ];
        for args in steps {
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(
                Duration::from_secs(40),
                BrowserInteractTool.execute(args.clone(), &ctx),
            )
            .await
            .unwrap_or_else(|_| panic!("stuck on {args}"));
            eprintln!("{args} -> {} in {:?}", result.success, started.elapsed());
            assert!(result.success, "{args}: {}", result.to_message_content());
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{args} was slow"
            );
        }
        assert!(latest_frame().is_some());
        close(&mut session());
    }

    #[test]
    fn browser_candidates_are_existing_files_without_duplicates() {
        let found = browser_candidates();
        assert!(found.iter().all(|p| p.is_file()));
        let unique: std::collections::HashSet<_> = found.iter().collect();
        assert_eq!(unique.len(), found.len());
        // Edge, when present, is the last resort.
        if let Some(edge) = found
            .iter()
            .position(|p| p.to_string_lossy().to_lowercase().contains("msedge"))
        {
            assert!(
                found[edge + 1..]
                    .iter()
                    .all(|p| p.to_string_lossy().to_lowercase().contains("edge"))
            );
        }
    }
}
