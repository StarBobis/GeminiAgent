use serde::Serialize;
use std::{
    env, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 32 * 1024;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptRuntime {
    PowerShell,
    Bun,
}

impl ScriptRuntime {
    pub fn for_path(path: &Path) -> Result<Self, String> {
        let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        match extension.to_ascii_lowercase().as_str() {
            "ps1" => Ok(Self::PowerShell),
            "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" => Ok(Self::Bun),
            _ => Err("脚本须为 PowerShell、JavaScript 或 TypeScript 文件。".into()),
        }
    }

    fn executable(self) -> Result<PathBuf, String> {
        match self {
            Self::PowerShell => {
                if let Ok(path) = executable_path("pwsh") {
                    return Ok(path);
                }
                #[cfg(windows)]
                if let Some(system) = env::var_os("SystemRoot") {
                    let path = PathBuf::from(system)
                        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
                    if path.is_file() {
                        return Ok(path);
                    }
                }
                Err("找不到 PowerShell。请安装 pwsh 或 Windows PowerShell。".into())
            }
            Self::Bun => {
                if let Ok(path) = executable_path("bun") {
                    return Ok(path);
                }
                let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
                if let Some(home) = home {
                    let path = PathBuf::from(home)
                        .join(".bun")
                        .join("bin")
                        .join(executable_name("bun"));
                    if path.is_file() {
                        return Ok(path);
                    }
                }
                Err("找不到 Bun。请安装 Bun 或将其加入 PATH。".into())
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub error: Option<String>,
}

fn executable_name(program: &str) -> PathBuf {
    let path = PathBuf::from(program);
    #[cfg(windows)]
    if path.extension().is_none() {
        return path.with_extension("exe");
    }
    path
}

pub fn executable_path(program: &str) -> Result<PathBuf, String> {
    let original = Path::new(program);
    let path = executable_name(program);
    #[cfg(windows)]
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return Err("Windows MCP 服务须使用 .exe 程序，例如 bun.exe 或 node.exe。".into());
    }
    if original.is_absolute() && path.is_file() {
        return Ok(path);
    }
    if !program.is_empty()
        && !program.contains(['/', '\\', ':'])
        && original.components().count() == 1
    {
        if let Some(search_path) = env::var_os("PATH") {
            if let Some(found) = env::split_paths(&search_path)
                .filter(|folder| folder.is_absolute())
                .map(|folder| folder.join(&path))
                .find(|path| path.is_file())
            {
                return Ok(found);
            }
        }
    }
    Err(format!("找不到可执行文件：{program}"))
}

pub fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
}

pub fn validate_args(args: &[String]) -> Result<(), String> {
    if args.len() > 32
        || args
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
    {
        return Err("参数最多 32 个，每个参数最多 4096 字节，且须为有效文本。".into());
    }
    Ok(())
}

async fn capture(mut reader: impl AsyncRead + Unpin, output: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0; 4096];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        let capacity = OUTPUT_LIMIT + 1 - output.len();
        output.extend_from_slice(&chunk[..count.min(capacity)]);
    }
}

pub async fn run_script(
    runtime: ScriptRuntime,
    root: &Path,
    script: &Path,
    args: &[String],
) -> Result<ScriptOutput, String> {
    let mut command = command(&runtime.executable()?);
    match runtime {
        ScriptRuntime::PowerShell => {
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        }
        ScriptRuntime::Bun => command.args(["--no-env-file", "--no-install", "run"]),
    };
    let mut child = command
        .arg(script)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动脚本：{error}"))?;
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let result = timeout(SCRIPT_TIMEOUT, async {
        tokio::try_join!(
            child.wait(),
            capture(out, &mut stdout),
            capture(err, &mut stderr)
        )
    })
    .await;
    let (exit_code, timed_out, error) = match result {
        Ok(Ok((status, (), ()))) => (status.code(), false, None),
        Ok(Err(error)) => (None, false, Some(error.to_string())),
        Err(_) => (None, true, None),
    };
    if timed_out || error.is_some() {
        let _ = child.kill().await;
    }
    let truncated = stdout.len() > OUTPUT_LIMIT || stderr.len() > OUTPUT_LIMIT;
    stdout.truncate(OUTPUT_LIMIT);
    stderr.truncate(OUTPUT_LIMIT);
    Ok(ScriptOutput {
        exit_code,
        timed_out,
        error,
        truncated,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}
