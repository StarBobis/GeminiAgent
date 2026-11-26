# Gemini Agent

Gemini Agent 将 Gemini 网页对话接到本地工作区。浏览器负责 Gemini 登录和对话，桌面应用将中文操作句转换成本地审批项，支持读写文件、执行脚本和调用 MCP 工具。发送给 Gemini 的消息使用当前 Google 账号在网页上的额度。

## 在 Windows 上运行

需要 Bun、Rust MSVC 工具链、Microsoft C++ Build Tools 和 WebView2。Chrome 或 Edge 可在工作台内显示 Gemini 的真实网页，也可以使用系统默认浏览器。依赖安装说明见 [Bun 安装文档](https://bun.com/docs/installation) 和 [Tauri 环境要求](https://v2.tauri.app/start/prerequisites/)。

```powershell
bun install
bun run tauri dev
```

构建 Windows 安装包。

```powershell
bun run tauri build
```

安装包生成在 `src-tauri/target/release/bundle/nsis/`。

## 完成一轮任务

1. 启动应用后直接进入 Gemini。首次使用本地工具时，点击右上角「设置」，在卡片里选择项目目录。应用会记住工作区。
2. 收起设置，在 Gemini 的原始聊天框发送需求，例如「看看当前项目有什么，读取 README 并告诉我怎么启动」。新对话收到第一条消息后，工作台会自动接入。
3. 工作台执行 Gemini 请求的本地文件操作。读取结果通过原生「Upload files」作为附件发回同一个对话，消息里列出原路径和附件名。打开「设置」即可查看本地操作记录、脚本权限与 MCP 服务。

工作台会等待当前草稿发送和当前回复生成完毕。在「设置」中关闭「自动执行和回传」可切换到手动模式，继续使用剪贴板、中文操作句或旧版 JSON。浏览器沿用系统代理设置。在「设置」中点击「弹出 Gemini」可用独立窗口继续当前对话，也可以切换 Chrome 或 Edge 后重新打开。

可以在空白工作区中手动导入以下示例。批准创建目录后，再批准写入。

~~~~text
创建目录 notes
写入文件 notes/hello.txt
```text
你好，Gemini！
```
~~~~

接着导入 `请读取文件 notes/hello.txt`，批准后即可查看原文。

## 执行脚本和 MCP

- `执行脚本 scripts/check.ts 参数 ["hello world"]` 执行已存在的脚本。支持 PowerShell `.ps1` 和 Bun 可运行的 JavaScript、TypeScript 文件。执行完显示退出码、标准输出、标准错误。PowerShell 使用系统现有执行策略，Bun 需要安装在本机。
- 在「MCP 服务」展开连接设置，填入一个可信的本地 stdio 服务的可执行程序和 JSON 参数数组，点击「连接 MCP」。例如程序填 `node.exe`，参数填 `["C:/path/to/server.js", "C:/path/to/workspace"]`。连接后导入 `列出 MCP 工具`，查看名称和输入定义。按定义导入 `调用 MCP 工具 read_text_file 参数 {"path":"notes/hello.txt"}`，实际参数取决于服务。
- 脚本和 MCP 服务以当前账户权限运行，启动目录为工作区。服务可访问的资源由服务配置决定。只运行你信任的脚本和服务。切换工作区会断开当前 MCP 连接。
- 为信任的项目勾选「允许此工作区自动运行脚本和 MCP」后，这些工具也会自动执行和回传。

## 文件权限和协议

- 内置文件操作使用工作区相对路径。根目录可写为 `.`。写入操作的 Markdown 代码块包含完整的新内容。
- Rust 持有选中目录的文件系统句柄，拒绝路径穿越、符号链接、Windows 重解析点和多重硬链接文件。每轮最多 12 项操作，其中最多读取 10 个文件。读取会生成临时附件，每个最大 100 MB，实际类型和额度取决于 Gemini。写入支持最多 128 KiB 的 UTF-8 文本。 [Gemini 文件上传说明](https://support.google.com/gemini/answer/14903178)
- 写入时重新检查文件内容。若文件在预览后发生变化，本轮写入会报告冲突，由 Gemini 重新读取后继续。新文件发布需要工作区所在的文件系统支持硬链接，例如 NTFS。
- 自动模式会把读取的文件作为附件回传，并在消息中附上脚本输出和 MCP 返回等工具结果。手动模式可在批准读取后点击「发送结果与附件」。Gemini 若显示首次上传条款，请在网页内确认。
- 登录由 Chrome 或 Edge 处理。Gemini 使用停靠在应用内的原生浏览器窗口，保留浏览器会话和中文输入法。工作台用 Windows UI Automation 操作当前聊天框。 [Google 登录政策](https://developers.google.com/identity/protocols/oauth2/policies) · [Gemini 网页规则](https://gemini.google.com/robots.txt)

## 项目结构

| 位置 | 职责 |
| --- | --- |
| `src/App.vue` | Vue 工作台、目录浏览、剪贴板操作和审批界面 |
| `src/agent.ts` | TypeScript 提示词和执行结果协议 |
| `src-tauri/src/protocol.rs` | Rust 回复解析和操作类型 |
| `src-tauri/src/workspace.rs` | 工作区授权、写入预览和文件操作 |
| `src-tauri/src/local_process.rs` | PowerShell 和 Bun 的子进程执行 |
| `src-tauri/src/mcp.rs` | 基于 Rust MCP SDK 的 stdio 连接与工具调用 |
| `src-tauri/src/browser.rs` | 浏览器窗口启动 |

用 `bun run build` 检查前端类型并构建，使用 `cargo test --manifest-path src-tauri/Cargo.toml` 运行本地文件与协议测试。

端到端测试需要 Chrome、Windows 桌面会话，以及 PATH 中的 Bun 和 Node。安装 [MCP Filesystem Server](https://www.npmjs.com/package/@modelcontextprotocol/server-filesystem)，把 `GEMINI_AGENT_TEST_MCP_SERVER` 设为该包的 `dist/index.js` 绝对路径，再运行以下命令。

```powershell
cargo test --manifest-path src-tauri/Cargo.toml -- --include-ignored --nocapture
```
