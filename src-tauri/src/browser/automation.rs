use super::{ChatPage, ToolRequest};
use crate::workspace::AttachmentBatch;
use std::{
    ffi::c_void,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::BSTR,
    Win32::{
        Foundation::HWND,
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
                COINIT_MULTITHREADED,
            },
            Variant::VARIANT,
        },
        UI::Accessibility::*,
    },
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetWindow, IsWindowVisible, GW_ENABLEDPOPUP,
};

struct ComApartment;

impl ComApartment {
    fn enter() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Conversation {
    pathname: String,
    automation: IUIAutomation,
    document: IUIAutomationElement,
    composer: Composer,
}

struct Composer {
    element: IUIAutomationElement,
    value: IUIAutomationValuePattern,
}

impl Composer {
    fn text(&self) -> windows::core::Result<String> {
        let value = unsafe { self.value.CurrentValue()? }.to_string();
        let visible = normalized_text(&value);
        if visible.is_empty() {
            return Ok(String::new());
        }
        let class = unsafe { self.element.CurrentClassName()? }.to_string();
        let placeholder = unsafe { self.element.CurrentHelpText() }
            .map(|text| text.to_string())
            .unwrap_or_default();
        // Quill exposes its generated placeholder as the control value while it is blank.
        if class.split_whitespace().any(|name| name == "ql-blank") {
            let placeholder = normalized_text(&placeholder);
            let placeholders = [
                placeholder.as_str(),
                "Ask Gemini",
                "Type @ for connectors, skills and more",
                "向 Gemini 发送消息",
                "向 Gemini 提问",
                "询问 Gemini",
            ];
            for placeholder in placeholders.into_iter().filter(|text| !text.is_empty()) {
                if let Some(contents) = visible.strip_prefix(placeholder) {
                    return Ok(contents.to_owned());
                }
            }
        }
        Ok(value)
    }
}

fn normalized_text(text: &str) -> String {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{200b}' || ch == '\u{feff}')
        .replace("\r\n", "\n")
}

fn property(element: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<String> {
    let raw = unsafe { element.GetCurrentPropertyValue(id) }.ok()?;
    BSTR::try_from(&raw).ok().map(|value| value.to_string())
}

fn by_type(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    control: UIA_CONTROLTYPE_ID,
) -> windows::core::Result<IUIAutomationElementArray> {
    unsafe {
        let condition = automation
            .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(control.0))?;
        root.FindAll(TreeScope_Descendants, &condition)
    }
}

fn conversation(
    hwnd: isize,
    pathname_for: impl Fn(&str) -> Option<String>,
) -> windows::core::Result<Option<Conversation>> {
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    let root = unsafe { automation.ElementFromHandle(HWND(hwnd as *mut c_void))? };
    let documents = by_type(&automation, &root, UIA_DocumentControlTypeId)?;
    for index in 0..unsafe { documents.Length()? } {
        let document = unsafe { documents.GetElement(index)? };
        let url = property(&document, UIA_ValueValuePropertyId)
            .filter(|value| !value.is_empty())
            .or_else(|| unsafe {
                let provider: IUIAutomationLegacyIAccessiblePattern = document
                    .GetCurrentPatternAs(UIA_LegacyIAccessiblePatternId)
                    .ok()?;
                provider.CurrentValue().ok().map(|value| value.to_string())
            });
        let Some(pathname) = pathname_for(url.as_deref().unwrap_or_default()) else {
            continue;
        };
        let entries = by_type(&automation, &document, UIA_EditControlTypeId)?;
        for index in 0..unsafe { entries.Length()? } {
            let element = unsafe { entries.GetElement(index)? };
            let name = unsafe { element.CurrentName()? }.to_string().to_lowercase();
            let class = unsafe { element.CurrentClassName()? }.to_string();
            if !name.contains("prompt for gemini")
                && !name.contains("ask gemini")
                && !name.contains("询问 gemini")
                && !name.contains("向 gemini")
                && !class.split_whitespace().any(|part| part == "ql-editor")
            {
                continue;
            }
            let composer: IUIAutomationValuePattern =
                unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId)? };
            if unsafe {
                element.CurrentIsEnabled()?.as_bool() && !composer.CurrentIsReadOnly()?.as_bool()
            } {
                return Ok(Some(Conversation {
                    pathname,
                    automation,
                    document,
                    composer: Composer {
                        element,
                        value: composer,
                    },
                }));
            }
        }
    }
    Ok(None)
}

#[derive(PartialEq, Eq)]
enum ChatButton {
    Send,
    Stop,
}

fn chat_button_kind(label: &str) -> Option<ChatButton> {
    match label.trim().to_lowercase().as_str() {
        "send" | "send message" | "send prompt" | "发送" | "发送消息" | "发送提示"
        | "发送提示词" => Some(ChatButton::Send),
        "stop"
        | "stop response"
        | "stop generating"
        | "stop generating response"
        | "停止"
        | "停止回复"
        | "停止生成"
        | "停止回答" => Some(ChatButton::Stop),
        _ => None,
    }
}

struct ChatControls {
    generating: bool,
    send: Option<IUIAutomationInvokePattern>,
}

struct UploadControls {
    menu: IUIAutomationElement,
    attached: Vec<String>,
}

fn invoke_pattern(element: &IUIAutomationElement) -> Option<IUIAutomationInvokePattern> {
    unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }.ok()
}

fn activate(element: &IUIAutomationElement) -> windows::core::Result<()> {
    unsafe {
        if let Some(invoke) = invoke_pattern(element) {
            return invoke.Invoke();
        }
        let legacy: IUIAutomationLegacyIAccessiblePattern =
            element.GetCurrentPatternAs(UIA_LegacyIAccessiblePatternId)?;
        legacy.DoDefaultAction()
    }
}

fn upload_controls(chat: &Conversation) -> windows::core::Result<Option<UploadControls>> {
    let walker = unsafe { chat.automation.ControlViewWalker()? };
    let mut scope = chat.composer.element.clone();
    for _ in 0..7 {
        scope = unsafe { walker.GetParentElement(&scope)? };
        let buttons = by_type(&chat.automation, &scope, UIA_ButtonControlTypeId)?;
        let mut attached = Vec::new();
        let mut menu = None;
        for index in 0..unsafe { buttons.Length()? } {
            let button = unsafe { buttons.GetElement(index)? };
            let name = unsafe { button.CurrentName()? }.to_string();
            let label = name.trim();
            let lower = label.to_lowercase();
            if matches!(
                lower.as_str(),
                "upload & tools"
                    | "上传和工具"
                    | "上传与工具"
                    | "上傳和工具"
                    | "上傳與工具"
                    | "添加文件和工具"
                    | "新增檔案和工具"
            ) {
                if unsafe {
                    !button.CurrentIsOffscreen()?.as_bool() && button.CurrentIsEnabled()?.as_bool()
                } {
                    menu = Some(button);
                }
            } else if matches!(
                lower.as_str(),
                "close" | "remove" | "移除" | "关闭" | "關閉"
            ) {
                attached.push(String::new());
            } else if let Some((prefix, name)) = label.split_once(' ') {
                if matches!(
                    prefix.to_lowercase().as_str(),
                    "close" | "remove" | "移除" | "关闭" | "關閉"
                ) {
                    attached.push(name.trim().to_owned());
                }
            }
        }
        if let Some(menu) = menu {
            return Ok(Some(UploadControls { menu, attached }));
        }
        if unsafe { chat.automation.CompareElements(&scope, &chat.document)? }.as_bool() {
            break;
        }
    }
    Ok(None)
}

fn upload_menu_item(chat: &Conversation) -> windows::core::Result<Option<IUIAutomationElement>> {
    let items = by_type(&chat.automation, &chat.document, UIA_MenuItemControlTypeId)?;
    for index in 0..unsafe { items.Length()? } {
        let item = unsafe { items.GetElement(index)? };
        let name = unsafe { item.CurrentName()? }.to_string().to_lowercase();
        if ["upload files", "上传文件", "上傳檔案", "上傳文件"]
            .iter()
            .any(|label| name.starts_with(label))
            && unsafe {
                !item.CurrentIsOffscreen()?.as_bool() && item.CurrentIsEnabled()?.as_bool()
            }
        {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

fn popup(hwnd: isize) -> Option<isize> {
    let target = hwnd as *mut c_void;
    let dialog = unsafe { GetWindow(target, GW_ENABLEDPOPUP) };
    (!dialog.is_null() && dialog != target && unsafe { IsWindowVisible(dialog) != 0 })
        .then_some(dialog as isize)
}

fn picker(
    chat: &Conversation,
    hwnd: isize,
) -> Result<Option<(IUIAutomationValuePattern, IUIAutomationElement)>, String> {
    let Some(dialog) = popup(hwnd) else {
        return Ok(None);
    };
    let mut class = [0u16; 32];
    let length = unsafe {
        GetClassNameW(
            dialog as *mut c_void,
            class.as_mut_ptr(),
            class.len() as i32,
        )
    };
    if length <= 0 || String::from_utf16_lossy(&class[..length as usize]) != "#32770" {
        return Ok(None);
    }
    picker_controls(&chat.automation, dialog).map_err(|error| error.to_string())
}

fn picker_controls(
    automation: &IUIAutomation,
    dialog: isize,
) -> windows::core::Result<Option<(IUIAutomationValuePattern, IUIAutomationElement)>> {
    unsafe {
        let root = automation.ElementFromHandle(HWND(dialog as *mut c_void))?;
        let name = automation.CreatePropertyCondition(
            UIA_AutomationIdPropertyId,
            &VARIANT::from(BSTR::from("1148")),
        )?;
        let edit = automation.CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_EditControlTypeId.0),
        )?;
        let edit = automation.CreateAndCondition(&name, &edit)?;
        let accept = automation
            .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(BSTR::from("1")))?;
        let Ok(field) = root.FindFirst(TreeScope_Descendants, &edit) else {
            return Ok(None);
        };
        let Ok(open) = root.FindFirst(TreeScope_Children, &accept) else {
            return Ok(None);
        };
        Ok(Some((field.GetCurrentPatternAs(UIA_ValuePatternId)?, open)))
    }
}

fn choose_files(chat: &Conversation, hwnd: isize, batch: &AttachmentBatch) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some((field, open)) = picker(chat, hwnd)? {
            let paths = batch
                .names()
                .iter()
                .map(|name| format!("\"{}\"", batch.folder().join(name).display()))
                .collect::<Vec<_>>()
                .join(" ");
            let paths = BSTR::from(paths.as_str());
            unsafe {
                field.SetValue(&paths).map_err(|error| error.to_string())?;
                if field.CurrentValue().map_err(|error| error.to_string())? != paths {
                    return Err("文件选择器未接受完整的附件清单。".into());
                }
            }
            activate(&open).map_err(|error| error.to_string())?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(125));
    }
    Err("Gemini 未打开文件选择器。如果页面显示首次上传条款，请在 Gemini 内确认后重试。".into())
}

fn attachments_match(attached: &[String], expected: &[String]) -> bool {
    attached.len() == expected.len()
        && expected.iter().all(|name| {
            let stem = Path::new(name).file_stem().and_then(|stem| stem.to_str());
            attached
                .iter()
                .any(|label| label == name || stem == Some(label.as_str()))
        })
}

fn attach_files(
    chat: &Conversation,
    hwnd: isize,
    batch: &AttachmentBatch,
    controls: UploadControls,
    pathname_for: impl Fn(&str) -> Option<String>,
) -> Result<Conversation, String> {
    let uia_error = |error: windows::core::Error| error.to_string();
    if upload_menu_item(chat).map_err(uia_error)?.is_none() {
        let expandable: windows::core::Result<IUIAutomationExpandCollapsePattern> = unsafe {
            controls
                .menu
                .GetCurrentPatternAs(UIA_ExpandCollapsePatternId)
        };
        if let Ok(expandable) = expandable {
            unsafe { expandable.Expand() }.map_err(uia_error)?;
        } else {
            activate(&controls.menu).map_err(uia_error)?;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let upload = loop {
        if let Some(item) = upload_menu_item(chat).map_err(uia_error)? {
            break item;
        }
        if Instant::now() >= deadline {
            return Err("找不到 Gemini 的 Upload files 菜单项。".into());
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !conversation(hwnd, &pathname_for)
        .map_err(uia_error)?
        .is_some_and(|page| {
            page.pathname == chat.pathname
                && page
                    .composer
                    .text()
                    .is_ok_and(|draft| draft.trim().is_empty())
        })
    {
        return Err("聊天已切换或出现草稿，已停止选择附件。".into());
    }
    activate(&upload).map_err(uia_error)?;
    choose_files(chat, hwnd, batch)?;

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut observed = 0;
    let mut ready_since = None;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(250));
        if let Some(current) = conversation(hwnd, &pathname_for).map_err(uia_error)? {
            if current.pathname != chat.pathname
                || !current
                    .composer
                    .text()
                    .map_err(uia_error)?
                    .trim()
                    .is_empty()
            {
                return Err("上传期间聊天或草稿已变化，请在 Gemini 中检查附件。".into());
            }
            if let Some(controls) = upload_controls(&current).map_err(uia_error)? {
                observed = controls.attached.len();
                let chat_controls = chat_controls(&current).map_err(uia_error)?;
                if attachments_match(&controls.attached, batch.names())
                    && chat_controls.send.is_some()
                    && !chat_controls.generating
                {
                    if ready_since.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_secs(1)
                    {
                        return Ok(current);
                    }
                    continue;
                }
            }
        }
        ready_since = None;
    }
    Err(format!(
        "Gemini 未确认全部附件（识别到 {observed}/{} 个）。请在 Gemini 中检查上传状态。",
        batch.names().len()
    ))
}

fn chat_controls(chat: &Conversation) -> windows::core::Result<ChatControls> {
    let entries = by_type(&chat.automation, &chat.document, UIA_ButtonControlTypeId)?;
    let input = unsafe { chat.composer.element.CurrentBoundingRectangle()? };
    let mut controls = ChatControls {
        generating: false,
        send: None,
    };
    for index in 0..unsafe { entries.Length()? } {
        let element = unsafe { entries.GetElement(index)? };
        let name = unsafe { element.CurrentName()? }.to_string();
        let Some(kind) = chat_button_kind(&name) else {
            continue;
        };
        if unsafe {
            element.CurrentIsOffscreen()?.as_bool() || !element.CurrentIsEnabled()?.as_bool()
        } {
            continue;
        }
        let button = unsafe { element.CurrentBoundingRectangle()? };
        if button.right < input.left - 64
            || button.left > input.right + 160
            || button.top > input.bottom + 120
            || button.bottom < input.top - 16
        {
            continue;
        }
        match kind {
            ChatButton::Stop => controls.generating = true,
            ChatButton::Send => {
                if let Ok(action) = unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) } {
                    controls.send = Some(action);
                }
            }
        }
    }
    Ok(controls)
}

fn read_request(document: &IUIAutomationElement, token: &str) -> Option<ToolRequest> {
    let pattern: IUIAutomationTextPattern =
        unsafe { document.GetCurrentPatternAs(UIA_TextPatternId) }.ok()?;
    let text = unsafe { pattern.DocumentRange().ok()?.GetText(384_000).ok()? };
    super::last_request(&text.to_string(), token)
}

pub(super) fn inspect(hwnd: isize, token: Option<&str>) -> windows::core::Result<Option<ChatPage>> {
    let _apartment = ComApartment::enter()?;
    let Some(chat) = conversation(hwnd, super::gemini_path)? else {
        return Ok(None);
    };
    let draft = chat.composer.text()?;
    let has_draft = !draft.trim().is_empty();
    let request = token.and_then(|token| {
        let composed = super::last_request(&draft, token);
        read_request(&chat.document, token)
            .filter(|request| !composed.is_some_and(|composed| composed.text == request.text))
    });
    Ok(Some(ChatPage {
        pathname: chat.pathname,
        has_draft,
        request,
    }))
}

pub(super) fn send(
    hwnd: isize,
    path: &str,
    text: &str,
    batch: Option<&AttachmentBatch>,
) -> Result<bool, String> {
    let uia_error = |error: windows::core::Error| error.to_string();
    let _apartment = ComApartment::enter().map_err(uia_error)?;
    let Some(chat) = conversation(hwnd, super::gemini_path).map_err(uia_error)? else {
        return Ok(false);
    };
    if chat.pathname != path {
        return Ok(false);
    }
    let before = upload_controls(&chat).map_err(uia_error)?;
    let Some(batch) = batch else {
        if before.is_some_and(|controls| !controls.attached.is_empty()) {
            return Ok(false);
        }
        return send_to_chat(&chat, text, &[]).map_err(uia_error);
    };
    let controls = before.ok_or("找不到 Gemini 聊天框的上传入口。")?;
    if attachments_match(&controls.attached, batch.names()) {
        return send_to_chat(&chat, text, batch.names()).map_err(uia_error);
    }
    if !controls.attached.is_empty()
        || !chat.composer.text().map_err(uia_error)?.trim().is_empty()
        || chat_controls(&chat).map_err(uia_error)?.generating
        || popup(hwnd).is_some()
    {
        return Ok(false);
    }
    let chat = attach_files(&chat, hwnd, batch, controls, super::gemini_path)?;
    if send_to_chat(&chat, text, batch.names()).map_err(uia_error)? {
        return Ok(true);
    }
    Err("附件仍在 Gemini 草稿中。请检查后手动发送。".into())
}

fn send_to_chat(
    chat: &Conversation,
    text: &str,
    attached: &[String],
) -> windows::core::Result<bool> {
    let expected = normalized_text(text);
    let current = normalized_text(&chat.composer.text()?);
    if (!current.is_empty() && current != expected) || chat_controls(chat)?.generating {
        return Ok(false);
    }
    if current.is_empty() {
        unsafe { chat.composer.value.SetValue(&BSTR::from(text))? };
    }
    for _ in 0..8 {
        // Chromium applies accessibility edits asynchronously in the renderer.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let actual = normalized_text(&chat.composer.text()?);
        if !actual.is_empty() && actual != expected {
            return Ok(false);
        }
        if actual == expected {
            let controls = chat_controls(chat)?;
            if !controls.generating {
                if let Some(send) = controls.send {
                    // Do not submit a draft the user edited while we looked up the button.
                    if normalized_text(&chat.composer.text()?) != expected {
                        return Ok(false);
                    }
                    let current = upload_controls(chat)?
                        .map(|controls| controls.attached)
                        .unwrap_or_default();
                    if !attachments_match(&current, attached) {
                        return Ok(false);
                    }
                    unsafe { send.Invoke()? };
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{browser_paths, host, Browser};
    use crate::protocol::parse_reply;
    use crate::workspace::Workspace;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        process::{Child, Command},
        thread,
        time::Duration,
    };

    struct TestChrome(Child);

    impl Drop for TestChrome {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    #[ignore = "requires Chrome and an interactive Windows session"]
    async fn uploads_and_sends_multiple_reads_through_chrome_file_picker() {
        let chrome = browser_paths(Browser::Chrome)
            .into_iter()
            .find(|path| path.is_file())
            .unwrap();
        let test = tempfile::tempdir().unwrap();
        let large = format!("{}first-test-tail", "a".repeat(200_000));
        fs::write(test.path().join("demo.txt"), &large).unwrap();
        fs::write(test.path().join("说明 example.rs"), "second-test-tail").unwrap();
        let token = "0123456789abcdef";
        let page = format!(
            r#"
            <!doctype html><meta charset="utf-8"><title>Gemini Bridge Test</title>
            <style>.ql-blank::before {{ content: attr(data-placeholder); }}</style>
            <main><pre>【工具请求 {token}】
读取文件 demo.txt
读取文件 `说明 example.rs`
【工具结束 {token}】</pre>
            <div id="composer">
            <div id="files"></div>
            <button id="uploads">Upload &amp; tools</button>
            <div id="menu" role="menu" hidden><button id="upload" role="menuitem">Upload files. Documents, data, code files</button></div>
            <input id="picker" type="file" multiple hidden>
            <div id="editor" class="ql-editor ql-blank" data-placeholder="Ask Gemini"
                 role="textbox" aria-label="Enter a prompt for Gemini" aria-multiline="true" tabindex="0" contenteditable="true"><p><br></p></div>
            <button id="send" aria-label="Send message">Send</button>
            </div>
            <p id="sent"></p></main>
            <script>
                const editor = document.getElementById('editor');
                const sent = document.getElementById('sent');
                const picker = document.getElementById('picker');
                editor.focus();
                const menu = document.getElementById('menu');
                const files = document.getElementById('files');
                document.getElementById('uploads').onclick = () => {{ menu.hidden = !menu.hidden }};
                document.getElementById('upload').onclick = () => {{ menu.hidden = true; picker.click() }};
                picker.onchange = () => {{
                    for (const file of picker.files) {{
                        const badge = document.createElement('button');
                        badge.textContent = 'close ' + file.name.replace(/\.[^.]+$/, '');
                        files.append(badge);
                    }}
                }};
                editor.addEventListener('input', () => editor.classList.toggle('ql-blank', !editor.textContent.trim()));
                document.getElementById('send').addEventListener('click', async () => {{
                    const received = await Promise.all(Array.from(picker.files, async (file) => {{
                        const text = await file.text();
                        return file.size + ':' + text.slice(-15);
                    }}));
                    sent.textContent = 'Submitted: ' + editor.textContent + ' ' + received.join(' ');
                    editor.textContent = '';
                    editor.classList.add('ql-blank');
                    files.replaceChildren();
                }});
            </script>
        "#
        );
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/chat.html", server.local_addr().unwrap());
        thread::spawn(move || {
            for mut socket in server.incoming().flatten() {
                let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
                let mut request = [0; 2048];
                if socket.read(&mut request).is_ok_and(|count| count > 0) {
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
                }
            }
        });
        let mut profile = std::ffi::OsString::from("--user-data-dir=");
        profile.push(test.path().join("browser"));
        let _browser = TestChrome(
            Command::new(&chrome)
                .arg(profile)
                .args([
                    "--no-first-run",
                    "--disable-background-networking",
                    "--disable-sync",
                    "--no-proxy-server",
                    "--force-renderer-accessibility=complete",
                ])
                .arg(format!("--app={url}"))
                .spawn()
                .unwrap(),
        );
        let hwnd = (0..60)
            .find_map(|_| {
                thread::sleep(Duration::from_millis(150));
                host::browser_windows(&chrome).into_iter().find(|id| {
                    let mut pid = 0;
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                            *id as *mut c_void,
                            &mut pid,
                        )
                    };
                    if pid != _browser.0.id() {
                        return false;
                    }
                    let mut title = [0u16; 128];
                    let length = unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW(
                            *id as *mut c_void,
                            title.as_mut_ptr(),
                            title.len() as i32,
                        )
                    };
                    length > 0
                        && String::from_utf16_lossy(&title[..length as usize])
                            .contains("Gemini Bridge Test")
                })
            })
            .expect("the isolated Chrome test browser should open");
        let _apartment = ComApartment::enter().unwrap();
        let pathname_for = |input: &str| (input == url || input.is_empty()).then(|| "/test".into());
        let chat = (0..32).find_map(|_| {
            let result = conversation(hwnd, pathname_for);
            match result {
                Ok(Some(chat)) => Some(chat),
                result => {
                    if let Err(error) = result {
                        eprintln!("Waiting for Chrome accessibility: {error}");
                    }
                    thread::sleep(Duration::from_millis(200));
                    None
                }
            }
        });
        let chat = chat.unwrap_or_else(|| {
            let automation: IUIAutomation =
                unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.unwrap();
            let root = unsafe { automation.ElementFromHandle(HWND(hwnd as *mut c_void)) }.unwrap();
            let docs = by_type(&automation, &root, UIA_DocumentControlTypeId).unwrap();
            let count = unsafe { docs.Length() }.unwrap();
            for index in 0..count {
                let document = unsafe { docs.GetElement(index) }.unwrap();
                let edits = by_type(&automation, &document, UIA_EditControlTypeId).unwrap();
                eprintln!(
                    "Fixture document {:?}, URL {:?}, editors {}",
                    unsafe { document.CurrentName() }.map(|name| name.to_string()),
                    property(&document, UIA_ValueValuePropertyId),
                    unsafe { edits.Length() }.unwrap()
                );
            }
            panic!("Chrome should expose the test document and composer ({count} documents)");
        });
        let request = read_request(&chat.document, token).expect("one tool request");
        assert_eq!(
            request.text,
            "读取文件 demo.txt\n读取文件 `说明 example.rs`"
        );
        assert!(chat.composer.text().unwrap().is_empty());
        let mut workspace = Workspace::open(test.path()).unwrap();
        let plan = workspace
            .stage(parse_reply(&request.text).unwrap(), None)
            .unwrap();
        workspace.resolve(plan.id, 0, true, None).await.unwrap();
        workspace.resolve(plan.id, 1, true, None).await.unwrap();
        let batch = workspace.attachment_batch(plan.id, &[0, 1]).unwrap();
        let controls = upload_controls(&chat).unwrap().expect("upload button");
        let chat = attach_files(&chat, hwnd, &batch, controls, pathname_for).unwrap();
        let message = "工具结果：两个附件";
        assert!(
            send_to_chat(&chat, message, batch.names()).unwrap(),
            "the placeholder must allow sending both picked files"
        );
        let read_document = || -> String {
            let pattern: IUIAutomationTextPattern =
                unsafe { chat.document.GetCurrentPatternAs(UIA_TextPatternId) }.unwrap();
            unsafe { pattern.DocumentRange().unwrap().GetText(1000) }
                .unwrap()
                .to_string()
        };
        assert!(
            (0..10).any(|_| {
                thread::sleep(Duration::from_millis(50));
                read_document().contains(&format!(
                    "Submitted: {message} {}:first-test-tail 16:econd-test-tail",
                    large.len()
                ))
            }),
            "the browser must submit both complete files with the short message"
        );

        unsafe { chat.composer.value.SetValue(&BSTR::from("Ask Gemini")) }.unwrap();
        assert!(
            (0..10).any(|_| {
                thread::sleep(Duration::from_millis(50));
                chat.composer.text().unwrap() == "Ask Gemini"
            }),
            "a typed copy of the placeholder is still a draft"
        );
        assert!(!send_to_chat(&chat, "不能覆盖现有草稿", &[]).unwrap());
        assert_eq!(chat.composer.text().unwrap(), "Ask Gemini");
    }
}
