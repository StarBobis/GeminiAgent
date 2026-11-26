use crate::local_process::{command, executable_path, validate_args};
use rmcp::{
    model::{CallToolRequestParams, CallToolResponse, JsonObject, Tool},
    service::RunningService,
    transport::TokioChildProcess,
    RoleClient, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::time::timeout;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_LIMIT: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct McpInfo {
    pub name: String,
    pub version: String,
    pub tools: Vec<Tool>,
}

pub struct McpConnection {
    pub id: Uuid,
    pub info: McpInfo,
    client: RunningService<RoleClient, ()>,
}

impl McpConnection {
    pub async fn connect(config: McpConfig, root: &Path) -> Result<Self, String> {
        validate_args(&config.args)?;
        let executable = executable_path(config.program.trim())?;
        let mut cmd = command(&executable);
        cmd.args(config.args)
            .current_dir(root)
            .stderr(Stdio::null());
        let transport =
            TokioChildProcess::new(cmd).map_err(|error| format!("无法启动 MCP 服务：{error}"))?;
        let client = timeout(CONNECT_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| "MCP 初始化超时。请检查服务是否使用 stdio。")?
            .map_err(|error| format!("MCP 初始化失败：{error}"))?;
        let peer = client.peer_info().ok_or("MCP 服务未返回初始化信息。")?;
        let server = peer
            .server_info
            .as_ref()
            .ok_or("MCP 服务未返回名称和版本。")?;
        let mut connection = Self {
            id: Uuid::new_v4(),
            info: McpInfo {
                name: server.name.clone(),
                version: server.version.clone(),
                tools: Vec::new(),
            },
            client,
        };
        if peer.capabilities.tools.is_some() {
            connection.refresh_tools().await?;
        }
        Ok(connection)
    }

    pub fn require_tool(&self, name: &str) -> Result<(), String> {
        if !self.info.tools.iter().any(|tool| tool.name == name) {
            return Err(format!("当前 MCP 服务未公布工具 {name:?}。"));
        }
        Ok(())
    }

    pub async fn refresh_tools(&mut self) -> Result<McpInfo, String> {
        let tools = timeout(CONNECT_TIMEOUT, self.client.list_all_tools())
            .await
            .map_err(|_| "读取 MCP 工具列表超时。")?
            .map_err(|error| format!("读取 MCP 工具列表失败：{error}"))?;
        if serde_json::to_vec(&tools)
            .map_err(|error| error.to_string())?
            .len()
            > RESULT_LIMIT
        {
            return Err("MCP 工具列表超过 128 KiB。".into());
        }
        self.info.tools = tools;
        Ok(self.info.clone())
    }

    pub async fn call(&self, name: &str, arguments: JsonObject) -> (bool, Value) {
        let request = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        match timeout(TOOL_TIMEOUT, self.client.call_tool_once(request)).await {
            Ok(Ok(CallToolResponse::Complete(result))) => {
                let ok = !result.is_error.unwrap_or(false);
                let value = serde_json::to_value(result);
                match value {
                    Ok(value) if value.to_string().len() <= RESULT_LIMIT => (ok, value),
                    Ok(_) => (
                        ok,
                        json!({"truncated": true, "message": "MCP 结果超过 128 KiB。"}),
                    ),
                    Err(error) => (false, json!({"error": error.to_string()})),
                }
            }
            Ok(Ok(_)) => (false, json!({"error": "MCP 服务要求客户端执行额外步骤。"})),
            Ok(Err(error)) => (false, json!({"error": error.to_string()})),
            Err(_) => (
                false,
                json!({"error": "MCP 调用超时，服务端可能仍在执行。"}),
            ),
        }
    }
}
