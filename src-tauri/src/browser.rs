use serde::{Deserialize, Serialize};
use std::{path::PathBuf, process::Command};
#[cfg(windows)]
use tauri::Manager;

#[cfg(windows)]
mod automation;
#[cfg(windows)]
mod host;
#[cfg(windows)]
pub use host::GeminiHost;

const GEMINI_URL: &str = "https://gemini.google.com/app";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Browser {
    Default,
    Chrome,
    Edge,
}

#[derive(Clone, Copy, Deserialize)]
pub struct Bounds {
    #[cfg_attr(not(windows), allow(dead_code))]
    x: i32,
    #[cfg_attr(not(windows), allow(dead_code))]
    y: i32,
    width: i32,
    height: i32,
}

impl Bounds {
    fn validate(self) -> Result<Self, String> {
        if self.width <= 0 || self.height <= 0 {
            return Err("Gemini 面板没有可用的显示空间。".into());
        }
        Ok(self)
    }
}

#[derive(Serialize)]
pub struct BrowserWindow {
    embedded: bool,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatPage {
    pathname: String,
    has_draft: bool,
    request: Option<ToolRequest>,
}

#[derive(Serialize)]
struct ToolRequest {
    sequence: usize,
    text: String,
}

#[cfg(any(windows, test))]
fn gemini_path(input: &str) -> Option<String> {
    let url = tauri::Url::parse(input).ok()?;
    let path = url.path();
    (url.scheme() == "https"
        && url.host_str() == Some("gemini.google.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (path == "/app" || path.starts_with("/app/")))
    .then(|| path.to_owned())
}

#[cfg(any(windows, test))]
fn last_request(text: &str, token: &str) -> Option<ToolRequest> {
    let start = format!("【工具请求 {token}】");
    let end = format!("【工具结束 {token}】");
    let mut result = None;
    let mut pending: Option<Vec<&str>> = None;
    let mut sequence = 0;
    for line in text.lines() {
        if line.trim() == start {
            pending = Some(Vec::new());
        } else if line.trim() == end {
            if let Some(lines) = pending.take() {
                let text = lines.join("\n").trim().to_owned();
                if !text.is_empty() {
                    sequence += 1;
                    result = Some(ToolRequest { sequence, text });
                }
            }
        } else if let Some(lines) = pending.as_mut() {
            lines.push(line);
        }
    }
    result
}

fn browser_paths(browser: Browser) -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let executable = match browser {
            Browser::Chrome => r"Google\Chrome\Application\chrome.exe",
            Browser::Edge => r"Microsoft\Edge\Application\msedge.exe",
            Browser::Default => return vec![],
        };
        return ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|base| PathBuf::from(base).join(executable))
            .collect();
    }
    #[cfg(target_os = "macos")]
    let paths: &[&str] = match browser {
        Browser::Chrome => &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"],
        Browser::Edge => &["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"],
        Browser::Default => &[],
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let paths: &[&str] = match browser {
        Browser::Chrome => &["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable"],
        Browser::Edge => &["/usr/bin/microsoft-edge", "/usr/bin/microsoft-edge-stable"],
        Browser::Default => &[],
    };
    #[cfg(unix)]
    paths.iter().map(PathBuf::from).collect()
}

#[tauri::command]
pub async fn open_gemini(
    app: tauri::AppHandle,
    browser: Browser,
    bounds: Option<Bounds>,
) -> Result<BrowserWindow, String> {
    let bounds = bounds.map(Bounds::validate).transpose()?;
    #[cfg(windows)]
    if let Some(bounds) = bounds {
        let host = app.state::<GeminiHost>();
        if host.resize(bounds).unwrap_or(false) {
            return Ok(BrowserWindow {
                embedded: true,
                message: "Gemini 已在工作台内打开。".into(),
            });
        }
    }
    #[cfg(not(windows))]
    let _ = bounds;
    for executable in browser_paths(browser)
        .into_iter()
        .filter(|path| path.is_file())
    {
        #[cfg(windows)]
        if let Some(bounds) = bounds {
            let host = app.state::<GeminiHost>();
            if let Some(id) = host.previous(&executable) {
                let hwnd = workbench_handle(&app)?;
                if host.attach(id, hwnd, bounds).is_ok() {
                    return Ok(BrowserWindow {
                        embedded: true,
                        message: "已将当前 Gemini 对话移回工作台。".into(),
                    });
                }
            }
        }
        #[cfg(windows)]
        let known_windows = bounds.map(|_| host::browser_windows(&executable));
        if let Ok(mut child) = Command::new(&executable)
            .arg(format!("--app={GEMINI_URL}"))
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            #[cfg(windows)]
            if let (Some(bounds), Some(known_windows)) = (bounds, known_windows) {
                let hwnd = workbench_handle(&app)?;
                let attachment = tauri::async_runtime::spawn_blocking(move || {
                    let id = host::wait_for_browser(executable, known_windows)
                        .ok_or("未找到新打开的 Gemini 浏览器窗口。".to_string())?;
                    app.state::<GeminiHost>()
                        .attach(id, hwnd, bounds)
                        .map_err(|error| format!("无法嵌入 Gemini：{error}"))
                })
                .await
                .map_err(|error| error.to_string())?;
                return Ok(match attachment {
                    Ok(()) => BrowserWindow {
                        embedded: true,
                        message: "Gemini 已在工作台内打开。".into(),
                    },
                    Err(reason) => BrowserWindow {
                        embedded: false,
                        message: format!("{reason} 浏览器窗口可独立使用。"),
                    },
                });
            }
            let message = match browser {
                Browser::Chrome => "已启动 Chrome 的 Gemini 窗口。",
                Browser::Edge => "已启动 Edge 的 Gemini 窗口。",
                Browser::Default => unreachable!(),
            };
            return Ok(BrowserWindow {
                embedded: false,
                message: message.into(),
            });
        }
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(GEMINI_URL, None::<&str>)
        .map_err(|error| error.to_string())?;
    let message = match browser {
        Browser::Default => "已在默认浏览器中打开 Gemini。",
        _ => "所选浏览器无法启动，已在默认浏览器中打开 Gemini。",
    };
    Ok(BrowserWindow {
        embedded: false,
        message: message.into(),
    })
}

#[cfg(windows)]
fn workbench_handle(app: &tauri::AppHandle) -> Result<isize, String> {
    let window = app
        .get_webview_window("main")
        .ok_or("无法找到工作台窗口。")?;
    Ok(window.hwnd().map_err(|error| error.to_string())?.0 as isize)
}

#[tauri::command]
pub fn resize_gemini(app: tauri::AppHandle, bounds: Bounds) -> Result<bool, String> {
    let bounds = bounds.validate()?;
    #[cfg(windows)]
    return app
        .state::<GeminiHost>()
        .resize(bounds)
        .map_err(|error| error.to_string());
    #[cfg(not(windows))]
    {
        let _ = (app, bounds);
        Ok(false)
    }
}

#[tauri::command]
pub fn detach_gemini(app: tauri::AppHandle) {
    #[cfg(windows)]
    app.state::<GeminiHost>().detach();
    #[cfg(not(windows))]
    let _ = app;
}

#[tauri::command]
pub async fn inspect_gemini_chat(
    app: tauri::AppHandle,
    token: Option<String>,
) -> Result<Option<ChatPage>, String> {
    if token.as_ref().is_some_and(|token| {
        token.len() != 16 || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("当前对话的工具标识无效。".into());
    }
    #[cfg(windows)]
    if let Some(hwnd) = app.state::<GeminiHost>().active_window() {
        return tauri::async_runtime::spawn_blocking(move || {
            automation::inspect(hwnd, token.as_deref()).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    let _ = app;
    Ok(None)
}

pub async fn send_gemini_message(
    app: tauri::AppHandle,
    pathname: String,
    text: String,
    attachments: Option<crate::workspace::AttachmentBatch>,
) -> Result<bool, String> {
    if text.len() > 1024 * 1024 {
        return Err("待填入的内容超过 1 MiB。".into());
    }
    #[cfg(windows)]
    if let Some(hwnd) = app.state::<GeminiHost>().active_window() {
        return tauri::async_runtime::spawn_blocking(move || {
            automation::send(hwnd, &pathname, &text, attachments.as_ref())
        })
        .await
        .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    let _ = (app, pathname, attachments);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_accepts_the_gemini_conversation_on_the_official_origin() {
        assert_eq!(
            gemini_path("https://gemini.google.com/app/123"),
            Some("/app/123".into())
        );
        for url in [
            "https://gemini.google.com.other.example/app",
            "https://gemini.google.com:8000/app",
            "https://gemini.google.com/?next=/app",
            "http://gemini.google.com/app",
        ] {
            assert!(gemini_path(url).is_none(), "{url}");
        }
    }

    #[test]
    fn requires_a_complete_request_with_the_active_conversation_token() {
        let token = "0123456789abcdef";
        let text = format!(
            "示例中使用「【工具请求 {token}】」和「【工具结束 {token}】」。\n\
             【工具请求 other】\n读取文件 other.txt\n【工具结束 other】\n\
             【工具请求 {token}】\r\n列出目录 .\r\n【工具结束 {token}】\r\n\
             【工具请求 {token}】\n尚未完成"
        );
        let result = last_request(&text, token).unwrap();
        assert_eq!(result.sequence, 1);
        assert_eq!(result.text, "列出目录 .");
    }
}
