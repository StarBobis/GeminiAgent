use crate::{
    local_process::{run_script, validate_args, ScriptRuntime},
    mcp::McpConnection,
    protocol::{
        Action, ActionResult, Plan, Preview, Reply, ScriptPreview, MAX_FILE_BYTES,
        MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES,
    },
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, File, Metadata, OpenOptions},
};
use serde::Serialize;
use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tempfile::{NamedTempFile, TempDir};
use uuid::Uuid;

const MAX_LIST_ENTRIES: usize = 200;

pub struct Workspace {
    root: PathBuf,
    dir: Dir,
    plan: Option<PendingPlan>,
}

struct PendingPlan {
    id: Uuid,
    mcp_id: Option<Uuid>,
    actions: Vec<PendingAction>,
    attachments: Option<Arc<TempDir>>,
}

struct PendingAction {
    path: Option<PathBuf>,
    preview: Preview,
    resolved: bool,
    attachment: Option<String>,
}

pub struct AttachmentBatch {
    folder: Arc<TempDir>,
    names: Vec<String>,
}

impl AttachmentBatch {
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn folder(&self) -> &Path {
        self.folder.path()
    }
}

#[derive(Serialize)]
pub struct DirectoryListing {
    root: String,
    path: String,
    entries: Vec<Entry>,
    truncated: bool,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    kind: &'static str,
}

pub fn relative_path(path: &str, allow_root: bool) -> Result<PathBuf, String> {
    if allow_root && path == "." {
        return Ok(PathBuf::from("."));
    }
    let mut result = PathBuf::new();
    for part in path.split(['/', '\\']) {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_ascii_uppercase();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].into_iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if part.is_empty()
            || part.ends_with(['.', ' '])
            || part
                .chars()
                .any(|ch| ch.is_control() || "<>:\"|?*".contains(ch))
            || reserved
        {
            return Err(format!("请使用工作区内的相对路径：{path:?}"));
        }
        result.push(part);
    }
    Ok(result)
}

fn path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn invalid_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "目标必须是普通 UTF-8 文本文件，大小上限为 128 KiB。",
    )
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.is_symlink()
}

fn has_single_link(file: &File, _metadata: &Metadata) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::{mem::MaybeUninit, os::windows::io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
        let mut info = MaybeUninit::uninit();
        // The handle is live, and the Win32 function initializes info on success.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.assume_init() }.nNumberOfLinks == 1)
    }
    #[cfg(not(windows))]
    {
        use cap_fs_ext::MetadataExt;
        let _ = file;
        Ok(_metadata.nlink() == 1)
    }
}

fn open_regular_file(dir: &Dir, name: &OsStr, lock_writes: bool) -> io::Result<(File, Metadata)> {
    let entry = dir.symlink_metadata(name)?;
    if !entry.is_file() || is_link(&entry) {
        return Err(io::Error::other("目标必须是普通文件，且不能是链接。"));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    if lock_writes {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }
    #[cfg(not(windows))]
    let _ = lock_writes;
    let file = dir.open_with(name, &options)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_link(&metadata) || !has_single_link(&file, &metadata)? {
        return Err(io::Error::other("目标必须是普通文件，且不能是链接。"));
    }
    Ok((file, metadata))
}

fn open_text_file(dir: &Dir, name: &OsStr, lock_writes: bool) -> io::Result<(File, Metadata)> {
    let (file, metadata) = open_regular_file(dir, name, lock_writes)?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(invalid_file());
    }
    Ok((file, metadata))
}

fn read_text(file: &mut File) -> io::Result<String> {
    let mut content = String::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_string(&mut content)?;
    if content.len() > MAX_FILE_BYTES || content.contains('\0') {
        return Err(invalid_file());
    }
    Ok(content)
}

fn read_file(dir: &Dir, name: &OsStr) -> io::Result<(String, Metadata)> {
    let (mut file, metadata) = open_text_file(dir, name, false)?;
    Ok((read_text(&mut file)?, metadata))
}

fn optional_file(dir: &Dir, name: &OsStr) -> io::Result<Option<(String, Metadata)>> {
    match read_file(dir, name) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_directory(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
    let next = dir.open_dir_nofollow(name)?;
    if is_link(&next.dir_metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "无法访问链接目录。",
        ));
    }
    Ok(next)
}

fn split_file(path: &Path) -> (&Path, &OsStr) {
    (path.parent().unwrap(), path.file_name().unwrap())
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self, String> {
        let dir = Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|error| format!("无法打开工作区：{error}"))?;
        Ok(Self {
            root: root.into(),
            dir,
            plan: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn directory(&self, path: &Path) -> io::Result<Dir> {
        let mut dir = self.dir.try_clone()?;
        for part in path.components() {
            match part {
                Component::Normal(name) => dir = open_directory(&dir, name)?,
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "路径超出工作区。",
                    ))
                }
            }
        }
        Ok(dir)
    }

    pub fn list(&self, path: &Path) -> Result<DirectoryListing, String> {
        self.list_entries(path)
            .map_err(|error| format!("{}：{error}", path_label(path)))
    }

    fn list_entries(&self, path: &Path) -> io::Result<DirectoryListing> {
        let dir = self.directory(path)?;
        let mut entries = Vec::new();
        let mut items = dir.entries()?;
        for item in (&mut items).take(MAX_LIST_ENTRIES) {
            let item = item?;
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            let metadata = item.metadata()?;
            let kind = if relative_path(&name, false).is_err() || is_link(&metadata) {
                "other"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(Entry { name, kind });
        }
        let truncated = items.next().is_some();
        entries.sort_by_cached_key(|entry| (entry.kind, entry.name.to_lowercase()));
        Ok(DirectoryListing {
            root: self.root.to_string_lossy().into_owned(),
            path: path_label(path),
            entries,
            truncated,
        })
    }

    fn snapshot(&self, path: &Path) -> io::Result<Option<String>> {
        let (parent, name) = split_file(path);
        let dir = match self.directory(parent) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(optional_file(&dir, name)?.map(|(text, _)| text))
    }

    pub fn stage(&mut self, reply: Reply, mcp: Option<&McpConnection>) -> Result<Plan, String> {
        if reply
            .actions
            .iter()
            .filter(|action| matches!(action, Action::Read { .. }))
            .count()
            > MAX_UPLOAD_FILES
        {
            return Err(format!(
                "一轮最多读取 {MAX_UPLOAD_FILES} 个文件，请分批请求。"
            ));
        }
        let mut actions = Vec::with_capacity(reply.actions.len());
        for action in reply.actions {
            let path = action
                .path()
                .map(|path| relative_path(path, matches!(action, Action::List { .. })))
                .transpose()?;
            let mut preview = Preview {
                action,
                before: None,
                script: None,
                mcp_server: None,
            };
            match &preview.action {
                Action::Write {
                    path: label,
                    content,
                } => {
                    if content.len() > MAX_FILE_BYTES || content.contains('\0') {
                        return Err(format!("{label}：{}", invalid_file()));
                    }
                    preview.before = self
                        .snapshot(path.as_deref().unwrap())
                        .map_err(|error| format!("{label}：{error}"))?;
                }
                Action::RunScript { path: label, args } => {
                    validate_args(args)?;
                    let path = path.as_deref().unwrap();
                    let runtime = ScriptRuntime::for_path(path)?;
                    let source = self
                        .snapshot(path)
                        .map_err(|error| format!("{label}：{error}"))?
                        .ok_or_else(|| {
                            format!("找不到脚本 {label:?}，请先写入脚本，再导入执行请求。")
                        })?;
                    preview.script = Some(ScriptPreview { runtime, source });
                }
                Action::McpList | Action::McpCall { .. } => {
                    let connection = mcp.ok_or("请先连接 MCP 服务。")?;
                    if let Action::McpCall { name, .. } = &preview.action {
                        connection.require_tool(name)?;
                    }
                    preview.mcp_server = Some(connection.info.name.clone());
                }
                _ => {}
            }
            actions.push(PendingAction {
                path,
                preview,
                resolved: false,
                attachment: None,
            });
        }
        let id = Uuid::new_v4();
        let plan = Plan {
            id,
            message: reply.message,
            actions: actions.iter().map(|item| item.preview.clone()).collect(),
        };
        self.plan = Some(PendingPlan {
            id,
            mcp_id: mcp.map(|connection| connection.id),
            actions,
            attachments: None,
        });
        Ok(plan)
    }

    pub async fn resolve(
        &mut self,
        id: Uuid,
        index: usize,
        approved: bool,
        mcp: Option<&mut McpConnection>,
    ) -> Result<ActionResult, String> {
        let plan = self
            .plan
            .as_ref()
            .filter(|plan| plan.id == id)
            .ok_or("审批计划已失效。")?;
        let pending = plan
            .actions
            .get(index)
            .filter(|action| !action.resolved)
            .ok_or("计划中无此待审批操作。")?;
        let result = if approved {
            if matches!(pending.preview.action, Action::Read { .. }) {
                let path = pending.path.as_deref().unwrap();
                let folder = match &plan.attachments {
                    Some(folder) => Arc::clone(folder),
                    None => Arc::new(
                        tempfile::Builder::new()
                            .prefix("gemini-agent-")
                            .tempdir()
                            .map_err(|error| error.to_string())?,
                    ),
                };
                let result = self
                    .prepare_attachment(path, id, index, &folder)
                    .map_err(|error| format!("{}：{error}", path_label(path)))?;
                self.plan.as_mut().unwrap().attachments = Some(folder);
                result
            } else {
                self.execute(pending, mcp, plan.mcp_id).await?
            }
        } else {
            ActionResult::Skipped
        };
        let action = &mut self.plan.as_mut().unwrap().actions[index];
        action.resolved = true;
        if let ActionResult::Read { name, .. } = &result {
            action.attachment = Some(name.clone());
        }
        Ok(result)
    }

    pub fn attachment_batch(&self, id: Uuid, indices: &[usize]) -> Result<AttachmentBatch, String> {
        let plan = self
            .plan
            .as_ref()
            .filter(|plan| plan.id == id)
            .ok_or("附件对应的操作计划已失效。")?;
        let folder = plan
            .attachments
            .clone()
            .ok_or("此计划没有已准备的文件附件。")?;
        let mut names = Vec::new();
        for index in indices {
            let name = plan
                .actions
                .get(*index)
                .and_then(|action| action.attachment.as_ref())
                .ok_or("只能上传此计划已读取的文件。")?;
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        if names.is_empty() || names.len() > MAX_UPLOAD_FILES {
            return Err(format!("请选择 1 至 {MAX_UPLOAD_FILES} 个已读取的文件。"));
        }
        Ok(AttachmentBatch { folder, names })
    }

    fn prepare_attachment(
        &self,
        path: &Path,
        id: Uuid,
        index: usize,
        folder: &TempDir,
    ) -> io::Result<ActionResult> {
        let (parent, name) = split_file(path);
        let dir = self.directory(parent)?;
        let (file, metadata) = open_regular_file(&dir, name, true)?;
        let too_large = || io::Error::other("文件超过 Gemini 附件大小上限 100 MB。");
        if metadata.len() > MAX_UPLOAD_BYTES {
            return Err(too_large());
        }
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .filter(|ext| ext.len() <= 16)
            .unwrap_or("txt");
        let mut attachment = NamedTempFile::new_in(folder.path())?;
        let bytes = io::copy(&mut file.take(MAX_UPLOAD_BYTES + 1), &mut attachment)?;
        if bytes > MAX_UPLOAD_BYTES {
            return Err(too_large());
        }
        // Gemini truncates long labels before exposing the attachment to accessibility clients.
        let name = format!("{:02}-{}.{}", index + 1, &id.simple().to_string()[..5], ext);
        attachment.persist_noclobber(folder.path().join(&name))?;
        Ok(ActionResult::Read { name, bytes })
    }

    async fn execute(
        &self,
        pending: &PendingAction,
        mcp: Option<&mut McpConnection>,
        mcp_id: Option<Uuid>,
    ) -> Result<ActionResult, String> {
        match &pending.preview.action {
            Action::RunScript { args, .. } => {
                let path = pending.path.as_deref().unwrap();
                let preview = pending.preview.script.as_ref().unwrap();
                let (parent, name) = split_file(path);
                let dir = self.directory(parent).map_err(|error| error.to_string())?;
                let (mut script, _) = open_text_file(&dir, name, true)
                    .map_err(|error| format!("{}：{error}", path_label(path)))?;
                if read_text(&mut script).map_err(|error| error.to_string())? != preview.source {
                    return Err("脚本在预览后发生了变化，请重新解析回复再审批。".into());
                }
                let output =
                    run_script(preview.runtime, &self.root, &self.root.join(path), args).await?;
                drop(script);
                Ok(ActionResult::RunScript { output })
            }
            Action::McpList | Action::McpCall { .. } => {
                let connection = mcp
                    .filter(|connection| Some(connection.id) == mcp_id)
                    .ok_or("MCP 连接已变更，请重新解析回复再审批。")?;
                if let Action::McpCall { name, arguments } = &pending.preview.action {
                    connection.require_tool(name)?;
                    let (ok, output) = connection.call(name, arguments.clone()).await;
                    Ok(ActionResult::McpCall { ok, output })
                } else {
                    Ok(ActionResult::McpList {
                        info: connection.refresh_tools().await?,
                    })
                }
            }
            action => {
                let path = pending.path.as_deref().unwrap();
                self.execute_file(action, path, pending.preview.before.as_deref())
                    .map_err(|error| format!("{}：{error}", path_label(path)))
            }
        }
    }

    fn execute_file(
        &self,
        action: &Action,
        path: &Path,
        before: Option<&str>,
    ) -> io::Result<ActionResult> {
        if matches!(action, Action::List { .. }) {
            return Ok(ActionResult::List {
                listing: self.list_entries(path)?,
            });
        }
        let (parent, name) = split_file(path);
        let dir = self.directory(parent)?;
        match action {
            Action::Write { content, .. } => {
                let created = write_file(&dir, name, before, content)?;
                Ok(ActionResult::Write { created })
            }
            Action::Mkdir { .. } => {
                let created = match dir.create_dir(name) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        open_directory(&dir, name)?;
                        false
                    }
                    Err(error) => return Err(error),
                };
                Ok(ActionResult::Mkdir { created })
            }
            _ => unreachable!(),
        }
    }
}

fn ensure_unchanged(dir: &Dir, name: &OsStr, before: Option<&str>) -> io::Result<Option<Metadata>> {
    let current = optional_file(dir, name)?;
    if current.as_ref().map(|(text, _)| text.as_str()) != before {
        return Err(io::Error::other(
            "文件在预览后发生了变化，请重新解析回复再审批。",
        ));
    }
    Ok(current.map(|(_, metadata)| metadata))
}

fn write_file(dir: &Dir, name: &OsStr, before: Option<&str>, content: &str) -> io::Result<bool> {
    let temporary = format!(".gemini-agent-{}.tmp", Uuid::new_v4());
    let mut file = dir.open_with(&temporary, OpenOptions::new().create_new(true).write(true))?;
    let prepared = (|| {
        file.write_all(content.as_bytes())?;
        if let Some(metadata) = ensure_unchanged(dir, name, before)? {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()
    })();
    drop(file);
    let result = prepared.and_then(|()| {
        if before.is_none() {
            dir.hard_link(&temporary, dir, name)
        } else {
            ensure_unchanged(dir, name, before)?;
            dir.rename(&temporary, dir, name)
        }
    });
    // Windows may need write access to unlink a temporary file with copied permissions.
    #[cfg(windows)]
    if result.is_err() && before.is_some() {
        if let Ok(metadata) = dir.symlink_metadata(&temporary) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = dir.set_permissions(&temporary, permissions);
        }
    }
    let _ = dir.remove_file(&temporary);
    result.map(|()| before.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_reply;
    use std::fs;

    #[test]
    fn rejects_paths_outside_the_workspace_and_windows_aliases() {
        for path in [
            "/notes",
            "../secret",
            "a/../b",
            "C:note",
            "a//b",
            "report.txt:stream",
            "CON.txt",
            "folder. /a",
        ] {
            assert!(relative_path(path, true).is_err(), "{path}");
        }
        assert!(relative_path(".", false).is_err());
        assert_eq!(
            relative_path("src/main.ts", false).unwrap(),
            Path::new("src").join("main.ts")
        );
    }

    #[tokio::test]
    async fn stages_complete_files_for_one_upload() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        let large = vec![b'x'; MAX_FILE_BYTES + 19];
        let binary = b"\0\xff test bytes";
        fs::write(root.path().join("example.txt"), &large).unwrap();
        fs::write(root.path().join("src/example.txt"), binary).unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace
            .stage(
                parse_reply("读取文件 example.txt\n读取文件 src/example.txt").unwrap(),
                None,
            )
            .unwrap();
        workspace.resolve(plan.id, 0, true, None).await.unwrap();
        workspace.resolve(plan.id, 1, true, None).await.unwrap();
        let batch = workspace.attachment_batch(plan.id, &[0, 1]).unwrap();
        let [first, second] = batch.names() else {
            panic!("expected two files");
        };
        assert_ne!(first, second);
        assert_eq!(fs::read(batch.folder().join(first)).unwrap(), large);
        assert_eq!(fs::read(batch.folder().join(second)).unwrap(), binary);
    }

    #[tokio::test]
    async fn runs_local_actions_only_after_approval() {
        let root = tempfile::tempdir().unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace
            .stage(
                parse_reply(
                    r#"{"message":"","actions":[
            {"type":"mkdir","path":"notes"},
            {"type":"write","path":"notes/hello.txt","content":"你好"},
            {"type":"read","path":"notes/hello.txt"},
            {"type":"write","path":"skipped.txt","content":"ignored"}
        ]}"#,
                )
                .unwrap(),
                None,
            )
            .unwrap();
        assert!(!root.path().join("notes").exists());
        assert!(workspace.resolve(plan.id, 1, true, None).await.is_err());
        assert!(matches!(
            workspace.resolve(plan.id, 0, true, None).await.unwrap(),
            ActionResult::Mkdir { created: true }
        ));
        assert!(matches!(
            workspace.resolve(plan.id, 1, true, None).await.unwrap(),
            ActionResult::Write { created: true }
        ));
        assert!(workspace.attachment_batch(plan.id, &[2]).is_err());
        let ActionResult::Read { name, bytes } =
            workspace.resolve(plan.id, 2, true, None).await.unwrap()
        else {
            panic!("expected file attachment");
        };
        let batch = workspace.attachment_batch(plan.id, &[2]).unwrap();
        assert_eq!(bytes, "你好".len() as u64);
        assert_eq!(
            fs::read_to_string(batch.folder().join(name)).unwrap(),
            "你好"
        );
        assert!(matches!(
            workspace.resolve(plan.id, 3, false, None).await.unwrap(),
            ActionResult::Skipped
        ));
        assert!(workspace.resolve(plan.id, 3, true, None).await.is_err());
        assert!(!root.path().join("skipped.txt").exists());
    }

    #[tokio::test]
    async fn rejects_stale_write_preview_and_accepts_reviewed_replacement() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("note.txt");
        let raw =
            r#"{"message":"","actions":[{"type":"write","path":"note.txt","content":"proposed"}]}"#;
        fs::write(&target, "original").unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace.stage(parse_reply(raw).unwrap(), None).unwrap();
        fs::write(&target, "changed independently").unwrap();
        assert!(workspace.resolve(plan.id, 0, true, None).await.is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "changed independently"
        );
        let next = workspace.stage(parse_reply(raw).unwrap(), None).unwrap();
        assert_eq!(
            next.actions[0].before.as_deref(),
            Some("changed independently")
        );
        assert!(workspace.resolve(plan.id, 0, true, None).await.is_err());
        assert!(matches!(
            workspace.resolve(next.id, 0, true, None).await.unwrap(),
            ActionResult::Write { created: false }
        ));
        assert_eq!(fs::read_to_string(target).unwrap(), "proposed");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn refuses_to_read_a_hard_link_to_another_directory() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "outside the workspace").unwrap();
        fs::hard_link(secret, root.path().join("linked.txt")).unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace
            .stage(
                parse_reply(
                    r#"{"message":"","actions":[
            {"type":"read","path":"linked.txt"}
        ]}"#,
                )
                .unwrap(),
                None,
            )
            .unwrap();
        assert!(workspace.resolve(plan.id, 0, true, None).await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_script_that_changed_after_review() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("sample.ts");
        fs::write(&target, "console.log('expected')").unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace
            .stage(
                parse_reply(
                    r#"{"message":"","actions":[
            {"type":"run_script","path":"sample.ts"}
        ]}"#,
                )
                .unwrap(),
                None,
            )
            .unwrap();
        fs::write(target, "throw new Error('changed')").unwrap();
        let result = workspace.resolve(plan.id, 0, true, None).await;
        assert!(matches!(result, Err(error) if error.contains("脚本在预览后发生了变化")));
    }

    #[tokio::test]
    #[ignore = "requires the Bun executable"]
    async fn executes_an_approved_typescript_script_with_arguments() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("fixture.txt"), "before").unwrap();
        fs::write(
            root.path().join("sample script.ts"),
            r#"
import { writeFileSync } from 'node:fs';
const value = process.argv[2];
writeFileSync('fixture.txt', value);
console.log(value);
console.error('captured stderr');
"#,
        )
        .unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let plan = workspace
            .stage(
                parse_reply(r#"执行脚本 `sample script.ts` 参数 ["hello world"]"#).unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("fixture.txt")).unwrap(),
            "before"
        );
        let ActionResult::RunScript { output } =
            workspace.resolve(plan.id, 0, true, None).await.unwrap()
        else {
            panic!("expected a script result");
        };
        assert_eq!(output.exit_code, Some(0), "{}", output.stderr);
        assert_eq!(output.stdout.trim(), "hello world");
        assert_eq!(output.stderr.trim(), "captured stderr");
        assert_eq!(
            fs::read_to_string(root.path().join("fixture.txt")).unwrap(),
            "hello world"
        );
        println!("Bun stdout: {}", output.stdout.trim());
        println!("Bun stderr: {}", output.stderr.trim());
    }

    #[tokio::test]
    #[ignore = "requires Node and GEMINI_AGENT_TEST_MCP_SERVER pointing to the MCP filesystem server JS entry"]
    async fn lists_and_calls_tools_on_a_real_stdio_mcp_server() {
        use crate::mcp::McpConfig;
        let entry = std::env::var("GEMINI_AGENT_TEST_MCP_SERVER").unwrap();
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("fixture.txt");
        fs::write(&target, "MCP round trip").unwrap();
        let mut workspace = Workspace::open(root.path()).unwrap();
        let mut mcp = McpConnection::connect(
            McpConfig {
                program: "node".into(),
                args: vec![entry, root.path().to_string_lossy().into_owned()],
            },
            root.path(),
        )
        .await
        .unwrap();
        let arguments = serde_json::json!({ "path": target });
        let reply = format!("列出 MCP 工具\n调用 MCP 工具 read_text_file 参数 {arguments}");
        let plan = workspace
            .stage(parse_reply(&reply).unwrap(), Some(&mcp))
            .unwrap();
        let ActionResult::McpList { info } = workspace
            .resolve(plan.id, 0, true, Some(&mut mcp))
            .await
            .unwrap()
        else {
            panic!("expected a tool list");
        };
        assert!(info.tools.iter().any(|tool| tool.name == "read_text_file"));
        let ActionResult::McpCall { ok, output } = workspace
            .resolve(plan.id, 1, true, Some(&mut mcp))
            .await
            .unwrap()
        else {
            panic!("expected a tool result");
        };
        assert!(ok, "{output}");
        assert_eq!(output["content"][0]["text"], "MCP round trip");
        println!("MCP server: {} · {} tools", info.name, info.tools.len());
        println!("MCP read_text_file: {}", output["content"][0]["text"]);
    }
}
