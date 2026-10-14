use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{Tool, ToolContext, ToolResult};

type BrowserSession = (headless_chrome::Browser, Arc<headless_chrome::Tab>);

static BROWSER_SESSION: Lazy<Mutex<Option<BrowserSession>>> = Lazy::new(|| Mutex::new(None));

pub struct BrowserInteractTool;

#[async_trait]
impl Tool for BrowserInteractTool {
    fn name(&self) -> &str {
        "browser_interact"
    }

    fn description(&self) -> &str {
        "Interact with a persistent Chrome browser. Supports navigate, click (coordinate-based), type, scroll, close, and screenshot actions. Returns a screenshot of the updated page. Note: You can use file:/// absolute paths to test local HTML files without needing a localhost server! \
         IMPORTANT: When you are finished with your browser task (like testing, looking up information, or debugging), you MUST explicitly use the 'close' action to close the browser and free up memory. \
         However, if the user asks you to do a persistent task (like playing a YouTube video, music, or a dashboard), do NOT close the browser when finished."
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
                "url": { "type": "string", "description": "For 'navigate' action: URL to open" },
                "x": { "type": "number", "description": "For 'click' action: X coordinate" },
                "y": { "type": "number", "description": "For 'click' action: Y coordinate" },
                "text": { "type": "string", "description": "For 'type' action: Text to type. For 'key' action: Key to press (e.g. 'Enter', 'Tab')" },
                "scroll_x": { "type": "number", "description": "For 'scroll' action: Pixels to scroll right" },
                "scroll_y": { "type": "number", "description": "For 'scroll' action: Pixels to scroll down" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = args["action"].as_str().unwrap_or("screenshot").to_string();

        let url = args["url"].as_str().unwrap_or("").to_string();
        let x = args["x"].as_f64().unwrap_or(0.0);
        let y = args["y"].as_f64().unwrap_or(0.0);
        let text = args["text"].as_str().unwrap_or("").to_string();
        let scroll_x = args["scroll_x"].as_f64().unwrap_or(0.0);
        let scroll_y = args["scroll_y"].as_f64().unwrap_or(0.0);

        let filename_str = ".norvexum/latest_screenshot.png".to_string();
        let path = Path::new(&filename_str).to_path_buf();

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let action_inner = action.clone();
        let project_dir = _ctx.cwd.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            use headless_chrome::{Browser, LaunchOptions};

            let mut session = BROWSER_SESSION.lock().unwrap();

            if action_inner == "close" {
                if let Some((browser, _)) = session.as_ref() {
                    if let Some(pid) = browser.get_process_id() {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/F", "/PID", &pid.to_string(), "/T"])
                            .output();
                    }
                }
                *session = None;
                return Ok("".to_string());
            }

            if session.is_none() {
                let mut w = 1920;
                let mut h = 1080;
                if let Ok(data) =
                    std::fs::read_to_string(project_dir.join(".norvexum/browser_size.json"))
                {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                        w = val["width"].as_u64().unwrap_or(1920) as u32;
                        h = val["height"].as_u64().unwrap_or(1080) as u32;
                    }
                }

                let browser = Browser::new(LaunchOptions {
                    headless: false,
                    window_size: Some((w, h)),
                    idle_browser_timeout: std::time::Duration::from_secs(3600),
                    ..Default::default()
                })
                .map_err(|e| format!("Failed to launch browser: {}", e))?;

                let tab = browser
                    .new_tab()
                    .map_err(|e| format!("Failed to open tab: {}", e))?;
                *session = Some((browser, tab));
            }

            let tab = session.as_ref().unwrap().1.clone();

            match action_inner.as_str() {
                "navigate" => {
                    if url.is_empty() {
                        return Err("URL is required for navigate action".to_string());
                    }
                    tab.navigate_to(&url)
                        .map_err(|e| format!("Failed to navigate: {}", e))?;
                    tab.wait_until_navigated()
                        .map_err(|e| format!("Failed to wait for navigation: {}", e))?;
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                "click" => {
                    use headless_chrome::protocol::cdp::Input;

                    // First move the mouse to the location
                    tab.call_method(Input::DispatchMouseEvent {
                        Type: Input::DispatchMouseEventTypeOption::MouseMoved,
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
                        delta_x: None,
                        delta_y: None,
                        pointer_Type: None,
                    })
                    .map_err(|e| format!("Failed to dispatch mouse moved: {}", e))?;

                    std::thread::sleep(std::time::Duration::from_millis(50));

                    tab.call_method(Input::DispatchMouseEvent {
                        Type: Input::DispatchMouseEventTypeOption::MousePressed,
                        x,
                        y,
                        button: Some(Input::MouseButton::Left),
                        click_count: Some(1),
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
                    .map_err(|e| format!("Failed to dispatch mouse down: {}", e))?;

                    std::thread::sleep(std::time::Duration::from_millis(50));

                    tab.call_method(Input::DispatchMouseEvent {
                        Type: Input::DispatchMouseEventTypeOption::MouseReleased,
                        x,
                        y,
                        button: Some(Input::MouseButton::Left),
                        click_count: Some(1),
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
                    .map_err(|e| format!("Failed to dispatch mouse up: {}", e))?;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                "type" => {
                    for c in text.chars() {
                        tab.type_str(&c.to_string())
                            .map_err(|e| format!("Failed to type: {}", e))?;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                "key" => {
                    tab.press_key(&text)
                        .map_err(|e| format!("Failed to press key {}: {}", text, e))?;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                "scroll" => {
                    let script = format!("window.scrollBy({}, {})", scroll_x, scroll_y);
                    tab.evaluate(&script, false)
                        .map_err(|e| format!("Failed to scroll: {}", e))?;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                "screenshot" => {
                    // Do nothing, just fall through to screenshot
                }
                _ => {
                    return Err(format!("Unknown action: {}", action_inner));
                }
            }

            // Inject cursor element
            let cursor_script = format!(
                r#"
                (function() {{
                    let cursor = document.getElementById('norvexum-cursor');
                    if (!cursor) {{
                        cursor = document.createElement('div');
                        cursor.id = 'norvexum-cursor';
                        cursor.style.position = 'fixed';
                        cursor.style.width = '20px';
                        cursor.style.height = '20px';
                        cursor.style.borderRadius = '50%';
                        cursor.style.backgroundColor = 'red';
                        cursor.style.border = '2px solid white';
                        cursor.style.boxShadow = '0 0 5px rgba(0,0,0,0.5)';
                        cursor.style.zIndex = '999999';
                        cursor.style.pointerEvents = 'none';
                        cursor.style.transition = 'top 0.1s, left 0.1s';
                        document.body.appendChild(cursor);
                    }}
                    cursor.style.left = '{}px';
                    cursor.style.top = '{}px';
                }})();
                "#,
                x, y
            );
            let _ = tab.evaluate(&cursor_script, false);
            std::thread::sleep(std::time::Duration::from_millis(100)); // wait for cursor to render

            if let Ok(ret) = tab.evaluate(
                "JSON.stringify({width: window.outerWidth, height: window.outerHeight})",
                false,
            ) {
                if let Some(s) = ret.value.and_then(|v| v.as_str().map(|s| s.to_string())) {
                    let _ = std::fs::write(project_dir.join(".norvexum/browser_size.json"), s);
                }
            }

            let img_data = tab
                .capture_screenshot(
                    headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Jpeg,
                    None,
                    None, // Capture viewport only, not full height
                    true,
                )
                .map_err(|e| format!("Failed to capture screenshot: {}", e))?;

            std::fs::write(&path, &img_data)
                .map_err(|e| format!("Failed to save screenshot: {}", e))?;

            use base64::{Engine as _, engine::general_purpose::STANDARD};
            let base64_str = STANDARD.encode(&img_data);

            Ok(base64_str)
        })
        .await;

        match result {
            Ok(Ok(base64_str)) => {
                if action == "close" {
                    ToolResult::ok("✅ Browser session closed.")
                } else {
                    let msg = format!(
                        "✅ Action '{}' completed. The current browser viewport is attached.",
                        action
                    );
                    ToolResult::ok_with_image(msg, base64_str, "image/jpeg".to_string())
                }
            }
            Ok(Err(e)) => ToolResult::err(e),
            Err(e) => ToolResult::err(format!("Task panicked: {}", e)),
        }
    }
}
