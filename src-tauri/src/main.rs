#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod local_process;
mod mcp;
mod protocol;
mod workspace;

use mcp::{McpConfig, McpConnection, McpInfo};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex;
use workspace::{DirectoryListing, Workspace};

#[derive(Default)]
struct Workbench {
    workspace: Option<Workspace>,
    mcp: Option<McpConnection>,
}

type Session = Mutex<Workbench>;

fn workspace_preference(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_local_data_dir().ok()?.join("workspace.json"))
}

#[tauri::command]
async fn restore_workspace(
    app: tauri::AppHandle,
    session: State<'_, Session>,
) -> Result<Option<DirectoryListing>, String> {
    let mut session = session.lock().await;
    if let Some(workspace) = session.workspace.as_ref() {
        return workspace.list(Path::new(".")).map(Some);
    }
    let Some(root) = workspace_preference(&app)
        .and_then(|path| fs::read(path).ok())
        .and_then(|data| serde_json::from_slice::<PathBuf>(&data).ok())
        .filter(|path| path.is_absolute())
    else {
        return Ok(None);
    };
    let Ok(workspace) = Workspace::open(&root) else {
        return Ok(None);
    };
    let listing = workspace.list(Path::new("."))?;
    session.workspace = Some(workspace);
    Ok(Some(listing))
}

#[tauri::command]
async fn select_workspace(
    app: tauri::AppHandle,
    session: State<'_, Session>,
) -> Result<Option<DirectoryListing>, String> {
    let Some(folder) = app
        .dialog()
        .file()
        .set_title("选择本地工作区")
        .blocking_pick_folder()
    else {
        return Ok(None);
    };
    let root = folder.into_path().map_err(|error| error.to_string())?;
    let workspace = Workspace::open(&root)?;
    let listing = workspace.list(Path::new("."))?;
    let mut session = session.lock().await;
    if let Some(path) = workspace_preference(&app) {
        if let (Some(parent), Ok(data)) = (path.parent(), serde_json::to_vec(&root)) {
            if fs::create_dir_all(parent).is_ok() {
                let _ = fs::write(&path, data);
            }
        }
    }
    *session = Workbench {
        workspace: Some(workspace),
        mcp: None,
    };
    Ok(Some(listing))
}

#[tauri::command]
async fn browse_workspace(
    session: State<'_, Session>,
    path: String,
) -> Result<DirectoryListing, String> {
    let session = session.lock().await;
    let workspace = session.workspace.as_ref().ok_or("请先选择工作区。")?;
    workspace.list(&workspace::relative_path(&path, true)?)
}

#[tauri::command]
async fn stage_reply(session: State<'_, Session>, reply: String) -> Result<protocol::Plan, String> {
    let reply = protocol::parse_reply(&reply)?;
    let mut session = session.lock().await;
    let Workbench { workspace, mcp } = &mut *session;
    workspace
        .as_mut()
        .ok_or("请先选择工作区。")?
        .stage(reply, mcp.as_ref())
}

#[tauri::command]
async fn resolve_action(
    session: State<'_, Session>,
    plan_id: uuid::Uuid,
    index: usize,
    approved: bool,
) -> Result<protocol::ActionResult, String> {
    let mut session = session.lock().await;
    let Workbench { workspace, mcp } = &mut *session;
    workspace
        .as_mut()
        .ok_or("请先选择工作区。")?
        .resolve(plan_id, index, approved, mcp.as_mut())
        .await
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadAttachments {
    plan_id: uuid::Uuid,
    indices: Vec<usize>,
}

#[tauri::command]
async fn send_gemini_message(
    app: tauri::AppHandle,
    session: State<'_, Session>,
    pathname: String,
    text: String,
    attachments: Option<ReadAttachments>,
) -> Result<bool, String> {
    let batch = if let Some(attachments) = attachments {
        let session = session.lock().await;
        let workspace = session.workspace.as_ref().ok_or("请先选择工作区。")?;
        Some(workspace.attachment_batch(attachments.plan_id, &attachments.indices)?)
    } else {
        None
    };
    browser::send_gemini_message(app, pathname, text, batch).await
}

#[tauri::command]
async fn connect_mcp(session: State<'_, Session>, config: McpConfig) -> Result<McpInfo, String> {
    let mut session = session.lock().await;
    let workspace = session.workspace.as_ref().ok_or("请先选择工作区。")?;
    let connection = McpConnection::connect(config, workspace.root()).await?;
    let info = connection.info.clone();
    session.mcp = Some(connection);
    Ok(info)
}

#[tauri::command]
async fn disconnect_mcp(session: State<'_, Session>) -> Result<(), String> {
    session.lock().await.mcp = None;
    Ok(())
}

fn main() {
    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder
        .manage(browser::GeminiHost::default())
        .on_window_event(|window, event| {
            if window.label() == "main" {
                let browser = window.state::<browser::GeminiHost>();
                match event {
                    tauri::WindowEvent::CloseRequested { .. } => browser.detach(),
                    tauri::WindowEvent::Moved(_)
                    | tauri::WindowEvent::Resized(_)
                    | tauri::WindowEvent::ScaleFactorChanged { .. } => browser.sync_position(),
                    _ => {}
                }
            }
        });
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Session::default())
        .invoke_handler(tauri::generate_handler![
            select_workspace,
            restore_workspace,
            browse_workspace,
            stage_reply,
            resolve_action,
            connect_mcp,
            disconnect_mcp,
            browser::open_gemini,
            browser::resize_gemini,
            browser::detach_gemini,
            browser::inspect_gemini_chat,
            send_gemini_message,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start Gemini Agent");
}
