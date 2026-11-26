fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "select_workspace",
            "restore_workspace",
            "browse_workspace",
            "stage_reply",
            "resolve_action",
            "connect_mcp",
            "disconnect_mcp",
            "open_gemini",
            "resize_gemini",
            "detach_gemini",
            "inspect_gemini_chat",
            "send_gemini_message",
        ]),
    ))
    .expect("failed to configure Tauri");
}
