use crate::{
    local_process::{ScriptOutput, ScriptRuntime},
    mcp::McpInfo,
    workspace::DirectoryListing,
};
use rmcp::model::JsonObject;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_FILE_BYTES: usize = 128 * 1024;
pub const MAX_UPLOAD_FILES: usize = 10;
pub const MAX_UPLOAD_BYTES: u64 = 100_000_000;
pub const MAX_ACTIONS: usize = 12;
const MAX_REPLY_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub message: String,
    pub actions: Vec<Action>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    List {
        path: String,
    },
    Read {
        path: String,
    },
    Write {
        path: String,
        content: String,
    },
    Mkdir {
        path: String,
    },
    RunScript {
        path: String,
        #[serde(default)]
        args: Vec<String>,
    },
    McpList,
    McpCall {
        name: String,
        #[serde(default)]
        arguments: JsonObject,
    },
}

impl Action {
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::List { path }
            | Self::Read { path }
            | Self::Write { path, .. }
            | Self::Mkdir { path }
            | Self::RunScript { path, .. } => Some(path),
            Self::McpList | Self::McpCall { .. } => None,
        }
    }
}

#[derive(Serialize)]
pub struct Plan {
    pub id: Uuid,
    pub message: String,
    pub actions: Vec<Preview>,
}

#[derive(Clone, Serialize)]
pub struct Preview {
    #[serde(flatten)]
    pub action: Action,
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptPreview>,
    #[serde(rename = "mcpServer", skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct ScriptPreview {
    pub runtime: ScriptRuntime,
    pub source: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionResult {
    List { listing: DirectoryListing },
    Read { name: String, bytes: u64 },
    Write { created: bool },
    Mkdir { created: bool },
    RunScript { output: ScriptOutput },
    McpList { info: McpInfo },
    McpCall { ok: bool, output: serde_json::Value },
    Skipped,
}

pub fn parse_reply(input: &str) -> Result<Reply, String> {
    if input.len() > MAX_REPLY_BYTES {
        return Err("回复超过 1 MiB，请让 Gemini 缩小操作范围。".into());
    }
    let input = input.trim_start_matches('\u{feff}').trim();
    if input.is_empty() {
        return Err("请粘贴 Gemini 的回复。".into());
    }
    let reply: Reply = if input.starts_with('{') {
        serde_json::from_str(input)
    } else if let Some(reply) = parse_chinese_reply(input)? {
        Ok(reply)
    } else {
        let mut lines = input.lines();
        let mut json = None;
        while let Some(line) = lines.next() {
            let fence = line.trim();
            if !fence.starts_with("```") {
                continue;
            }
            let mut block = String::new();
            let mut closed = false;
            for line in lines.by_ref() {
                if line.trim() == "```" {
                    closed = true;
                    break;
                }
                block.push_str(line);
                block.push('\n');
            }
            if fence.eq_ignore_ascii_case("```json")
                || (fence == "```" && block.trim_start().starts_with('{'))
            {
                if !closed || json.replace(block).is_some() {
                    return Err("请提供一个完整的 JSON 代码块。".into());
                }
            }
        }
        match json {
            Some(json) => serde_json::from_str(&json),
            None => {
                return Ok(Reply {
                    message: input.into(),
                    actions: Vec::new(),
                })
            }
        }
    }
    .map_err(|error| format!("无法解析 Gemini 的操作 JSON：{error}"))?;
    if reply.actions.len() > MAX_ACTIONS {
        return Err(format!("每轮最多支持 {MAX_ACTIONS} 项操作。"));
    }
    Ok(reply)
}

fn command_text(line: &str) -> &str {
    let text = line.trim();
    let text = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("* "))
        .unwrap_or(text);
    text.strip_prefix('请')
        .unwrap_or(text)
        .trim()
        .trim_end_matches(['。', '！'])
}

fn strip_command<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let tail = line.strip_prefix(prefix)?;
    if tail.is_empty() || tail.starts_with(|ch: char| ch.is_whitespace() || "：:`\"".contains(ch))
    {
        return Some(
            tail.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '：' || ch == ':'),
        );
    }
    None
}

fn target_and_params(tail: &str) -> Result<(&str, Option<&str>), String> {
    let (target, rest) = if tail.starts_with(['`', '"']) {
        let quote = tail.as_bytes()[0] as char;
        let end = tail[1..]
            .find(quote)
            .ok_or("文件路径或工具名称的引号未闭合。")?
            + 1;
        (&tail[1..end], tail[end + 1..].trim())
    } else if let Some((target, params)) = tail.split_once(" 参数") {
        (target.trim_end(), params)
    } else {
        (tail, "")
    };
    let rest = rest
        .strip_prefix("参数")
        .unwrap_or(rest)
        .trim_start_matches([' ', '：', ':']);
    Ok((target, (!rest.is_empty()).then_some(rest)))
}

fn path_for(tail: &str, allow_root: bool) -> Result<String, String> {
    let (target, params) = target_and_params(tail)?;
    if params.is_some() {
        return Err("此操作只需要文件路径。路径含空格时可用反引号包住。".into());
    }
    if target.is_empty() {
        return if allow_root {
            Ok(".".into())
        } else {
            Err("请填写文件或目录的相对路径。".into())
        };
    }
    Ok(target.into())
}

fn fence_start(line: &str) -> Option<usize> {
    let length = line
        .trim_start()
        .bytes()
        .take_while(|byte| *byte == b'`')
        .count();
    (length >= 3).then_some(length)
}

fn read_block<'a>(lines: &mut impl Iterator<Item = &'a str>, length: usize) -> Option<String> {
    let closing = "`".repeat(length);
    let mut content = String::new();
    for line in lines {
        if line.trim() == closing {
            return Some(content);
        }
        content.push_str(line);
        content.push('\n');
    }
    None
}

fn parse_chinese_reply(input: &str) -> Result<Option<Reply>, String> {
    let mut lines = input.lines().peekable();
    let mut message = Vec::new();
    let mut actions = Vec::new();
    while let Some(line) = lines.next() {
        let text = command_text(line);
        let action = if let Some(tail) = ["列出目录", "查看目录"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            Action::List {
                path: path_for(tail, true)?,
            }
        } else if let Some(tail) = ["读取文件", "读一下文件", "读一下", "读取"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            Action::Read {
                path: path_for(tail, false)?,
            }
        } else if let Some(tail) = ["创建目录", "新建目录"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            Action::Mkdir {
                path: path_for(tail, false)?,
            }
        } else if let Some(tail) = ["写入文件", "编辑文件", "新建文件"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            let path = path_for(tail, false)?;
            while lines.peek().is_some_and(|line| line.trim().is_empty()) {
                lines.next();
            }
            let fence = lines
                .next()
                .and_then(fence_start)
                .ok_or("写入文件的下一行需要一个代码块，包含完整新内容。")?;
            let content = read_block(&mut lines, fence).ok_or("写入文件的代码块未闭合。")?;
            Action::Write { path, content }
        } else if let Some(tail) = ["执行脚本", "运行脚本"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            let (target, params) = target_and_params(tail)?;
            if target.is_empty() {
                return Err("请填写脚本的相对路径。".into());
            }
            let path = target.into();
            let args = params
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| format!("脚本参数应是 JSON 字符串数组：{error}"))?
                .unwrap_or_default();
            Action::RunScript { path, args }
        } else if matches!(text, "列出 MCP 工具" | "列出MCP工具") {
            Action::McpList
        } else if let Some(tail) = ["调用 MCP 工具", "调用MCP工具"]
            .iter()
            .find_map(|prefix| strip_command(text, prefix))
        {
            let (name, params) = target_and_params(tail)?;
            if name.is_empty() {
                return Err("请填写 MCP 工具名称。".into());
            }
            let arguments = params
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| format!("MCP 参数应是 JSON 对象：{error}"))?
                .unwrap_or_default();
            Action::McpCall {
                name: name.into(),
                arguments,
            }
        } else if let Some(fence) = fence_start(line) {
            let mut block = line.to_owned();
            if let Some(content) = read_block(&mut lines, fence) {
                block.push('\n');
                block.push_str(&content);
                block.push_str(&"`".repeat(fence));
            }
            message.push(block);
            continue;
        } else {
            message.push(line.to_owned());
            continue;
        };
        actions.push(action);
    }
    if actions.is_empty() {
        return Ok(None);
    }
    Ok(Some(Reply {
        message: message.join("\n").trim().into(),
        actions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_reply_and_rejects_truncated_or_unsupported_actions() {
        let reply = parse_reply(
            "以下是操作。\r\n```json\r\n{\"message\":\"读取\",\"actions\":[{\"type\":\"read\",\"path\":\"README.md\"}]}\r\n```",
        )
        .unwrap();
        assert_eq!(reply.actions[0].path(), Some("README.md"));
        assert!(parse_reply("```json\n{\"message\":\"no\",\"actions\":[]}").is_err());
        assert!(parse_reply(r#"{"message":"","actions":[{"type":"exec","path":"."}]}"#).is_err());
    }

    #[test]
    fn converts_chinese_requests_without_interpreting_commands_inside_file_content() {
        let reply = parse_reply("先检查项目。\n- 请读取文件 `README.md`。\n编辑文件 doc.json\n```json\n{\"title\":\"示例\"}\n```\n执行脚本 `run test.ts` 参数 [\"hello world\"]\n调用 MCP 工具 echo 参数 {\"text\":\"ok\"}").unwrap();
        assert_eq!(reply.message, "先检查项目。");
        assert!(
            matches!(&reply.actions[1], Action::Write { path, content } if path == "doc.json" && content == "{\"title\":\"示例\"}\n")
        );
        assert!(
            matches!(&reply.actions[2], Action::RunScript { path, args } if path == "run test.ts" && args == &["hello world"])
        );
        assert!(
            matches!(&reply.actions[3], Action::McpCall { name, arguments } if name == "echo" && arguments["text"] == "ok")
        );
        assert!(parse_reply("写入文件 note.txt\n```\nincomplete").is_err());
        assert!(
            parse_reply("这是一段说明。\n```text\n读取文件 secret.txt\n```")
                .unwrap()
                .actions
                .is_empty()
        );
    }
}
