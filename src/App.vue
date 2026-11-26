<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { invoke, isTauri } from '@tauri-apps/api/core'
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager'
import {
  ArrowLeft, ArrowUpRight, Check, ChevronRight, Clipboard, ClipboardPaste, Code2, File,
  Cable, FilePlus2, Folder, FolderOpen, FolderPlus, ListTree, RefreshCw, ScanText, Settings, Sparkles, Terminal, X,
} from '@lucide/vue'
import { actionNames, bridgePrompt, bridgeResult, failed, resultPrompt, taskPrompt } from './agent'
import type { Action, ActionResult, Browser, Listing, McpConfig, McpInfo, Plan } from './agent'
import McpSettings from './components/McpSettings.vue'
import WritePreview from './components/WritePreview.vue'

const desktop = isTauri()
const browser = ref<Browser>('chrome')
const listing = ref<Listing>()
const task = ref('')
const reply = ref('')
const plan = ref<Plan>()
const mcp = ref<McpInfo>()
const geminiEmbedded = ref(false)
const geminiOpening = ref(desktop)
const geminiStatus = ref('')
const geminiViewport = ref<HTMLElement>()
const geminiVisible = computed(() => geminiEmbedded.value || geminiOpening.value)
const settingsOpen = ref(false)
const settingsButton = ref<HTMLButtonElement>()
const results = ref<(ActionResult | null)[]>([])
const actionErrors = ref<Record<number, string>>({})
const busy = ref(false)
const notice = ref<{ text: string; error: boolean }>()
const automatic = ref(true)
const trustedRoot = ref(localStorage.getItem('gemini-trusted-workspace') ?? '')
const trustedTools = computed(() => !!listing.value && trustedRoot.value === listing.value.root)
const bridgeStatus = ref('等待 Gemini 对话')
const automaticPlan = ref('')
const uploadFailed = ref<{ pathname: string; token: string }>()

interface PendingMessage {
  text: string
  attachments?: { planId: string; indices: number[] }
}

interface ChatPage {
  pathname: string
  hasDraft: boolean
  request: { sequence: number; text: string } | null
}

function newBridge() {
  const token = [...crypto.getRandomValues(new Uint32Array(2))]
    .map((part) => part.toString(16).padStart(8, '0')).join('')
  return { token, pathname: '', prepared: false, seen: new Set<string>(), outgoing: undefined as PendingMessage | undefined }
}

let bridge = newBridge()
let bridgePolling = false
let bridgeTimer = 0

const workspaceName = computed(() => listing.value?.root.split(/[\\/]/).filter(Boolean).at(-1) ?? '')
const pending = computed(() => results.value.filter((result) => result === null).length)
const processed = computed(() => results.value.some((result) => result !== null) || Object.keys(actionErrors.value).length > 0)
const attachments = computed(() => {
  const indices = results.value.flatMap((result, index) => result?.type === 'read' ? [index] : [])
  return plan.value && indices.length ? { planId: plan.value.id, indices } : undefined
})
const actionIcons = { list: ListTree, read: ScanText, write: FilePlus2, mkdir: FolderPlus, run_script: Terminal, mcp_list: Cable, mcp_call: Cable }

function actionTarget(action: Action) {
  if ('path' in action) return action.path
  return action.type === 'mcp_call' ? `${action.mcpServer} / ${action.name}` : action.mcpServer
}

function resultLabel(result?: ActionResult | null) {
  if (!result) return ''
  if (result.type === 'skipped') return '已跳过'
  return failed(result) ? '执行失败' : '已执行'
}

function resultFailed(result?: ActionResult | null) {
  return result ? failed(result) : false
}

async function run(action: () => Promise<void>) {
  if (busy.value) return
  busy.value = true
  notice.value = undefined
  try {
    await action()
  } catch (error) {
    notice.value = { text: String(error), error: true }
  } finally {
    busy.value = false
  }
}

function inform(text: string) {
  notice.value = { text, error: false }
}

function clearPlan() {
  plan.value = undefined
  results.value = []
  actionErrors.value = {}
  automaticPlan.value = ''
  uploadFailed.value = undefined
}

watch([geminiEmbedded, automatic, trustedTools, () => listing.value?.root, () => mcp.value?.name], () => {
  bridge = newBridge()
  bridgeStatus.value = '等待 Gemini 对话'
})

function setTrustedTools(enabled: boolean) {
  trustedRoot.value = enabled ? listing.value?.root ?? '' : ''
  localStorage.setItem('gemini-trusted-workspace', trustedRoot.value)
}

async function chooseWorkspace() {
  await run(async () => {
    const selected = await invoke<Listing | null>('select_workspace')
    if (selected) {
      listing.value = selected
      mcp.value = undefined
      reply.value = ''
      clearPlan()
    }
  })
}

async function closeSettings() {
  settingsOpen.value = false
  await nextTick()
  settingsButton.value?.focus()
}

function browse(path: string) {
  return run(async () => {
    listing.value = await invoke<Listing>('browse_workspace', { path })
  })
}

function entryPath(name: string) {
  const current = listing.value?.path ?? '.'
  return current === '.' ? name : `${current}/${name}`
}

function addReference(name: string) {
  const path = entryPath(name)
  task.value = [task.value.trimEnd(), `相关文件：${path}`].filter(Boolean).join('\n')
  inform(`已把 ${path} 的路径加入任务。`)
}

function geminiBounds() {
  const rect = geminiViewport.value?.getBoundingClientRect()
  if (!rect || rect.width <= 0 || rect.height <= 0) return undefined
  const pixels = (value: number) => Math.round(value * window.devicePixelRatio)
  return { x: pixels(rect.left), y: pixels(rect.top), width: pixels(rect.width), height: pixels(rect.height) }
}

let resizeObserver: ResizeObserver | undefined
let resizeFrame = 0

function syncGeminiBounds() {
  cancelAnimationFrame(resizeFrame)
  resizeFrame = requestAnimationFrame(async () => {
    const bounds = geminiBounds()
    if (!geminiEmbedded.value || !bounds) return
    try {
      const alive = await invoke<boolean>('resize_gemini', { bounds })
      if (!alive && geminiEmbedded.value) {
        geminiEmbedded.value = false
        geminiStatus.value = 'Gemini 窗口已关闭。'
      }
    } catch (error) {
      notice.value = { text: String(error), error: true }
    }
  })
}

onMounted(() => {
  resizeObserver = new ResizeObserver(syncGeminiBounds)
  if (geminiViewport.value) resizeObserver.observe(geminiViewport.value)
  window.addEventListener('resize', syncGeminiBounds)
  bridgeTimer = window.setInterval(() => {
    if (geminiEmbedded.value) syncGeminiBounds()
    void syncChat()
  }, 1300)
  if (desktop) {
    void run(async () => {
      listing.value = await invoke<Listing | null>('restore_workspace') ?? undefined
    }).then(openGemini)
  }
})

onUnmounted(() => {
  resizeObserver?.disconnect()
  window.removeEventListener('resize', syncGeminiBounds)
  cancelAnimationFrame(resizeFrame)
  window.clearInterval(bridgeTimer)
})

function openGemini() {
  return run(async () => {
    if (geminiEmbedded.value) {
      await invoke('detach_gemini')
      geminiEmbedded.value = false
      geminiStatus.value = '当前 Gemini 对话已移到独立窗口。'
      return
    }
    const preferred = browser.value
    geminiOpening.value = true
    geminiStatus.value = ''
    try {
      await nextTick()
      const result = await invoke<{ embedded: boolean; message: string }>('open_gemini', {
        browser: preferred,
        bounds: geminiBounds() ?? null,
      })
      geminiEmbedded.value = result.embedded
      geminiStatus.value = result.message
      if (result.embedded) syncGeminiBounds()
    } catch (error) {
      geminiStatus.value = String(error)
    } finally {
      geminiOpening.value = false
    }
  })
}

function connectMcp(config: McpConfig) {
  return run(async () => {
    mcp.value = await invoke<McpInfo>('connect_mcp', { config })
    inform(`已连接 ${mcp.value.name}，发现 ${mcp.value.tools.length} 个工具。`)
  })
}

function disconnectMcp() {
  return run(async () => {
    await invoke('disconnect_mcp')
    mcp.value = undefined
    inform('MCP 服务已断开。')
  })
}

function copyTask() {
  return run(async () => {
    const instructions = task.value.trim() || '请按这些工具说明继续处理我上一条任务。'
    await writeText(taskPrompt(instructions, mcp.value))
    inform('已复制。请在 Gemini 中粘贴并发送。')
  })
}

function usePlan(staged: Plan, automated = false) {
  plan.value = staged
  results.value = staged.actions.map(() => null)
  actionErrors.value = {}
  automaticPlan.value = automated ? staged.id : ''
  uploadFailed.value = undefined
}

async function sendToChat(pathname: string, message: PendingMessage) {
  return invoke<boolean>('send_gemini_message', { pathname, ...message })
}

async function executeStep(staged: Plan, index: number, approved: boolean) {
  delete actionErrors.value[index]
  try {
    const result = await invoke<ActionResult>('resolve_action', { planId: staged.id, index, approved })
    results.value[index] = result
    if (result.type === 'mcp_list') mcp.value = result.info
    return result
  } catch (error) {
    actionErrors.value[index] = String(error)
  }
}

async function runRequestedTools(current: ReturnType<typeof newBridge>, request: string) {
  try {
    const staged = await invoke<Plan>('stage_reply', { reply: request })
    if (current !== bridge) return
    usePlan(staged, true)
    const authorization = trustedTools.value
    for (const [index, action] of staged.actions.entries()) {
      if (current !== bridge) return
      const needsTrust = action.type === 'run_script' || action.type === 'mcp_call' || action.type === 'mcp_list'
      bridgeStatus.value = `正在自动执行 ${index + 1}/${staged.actions.length}`
      await executeStep(staged, index, !needsTrust || authorization)
      if (needsTrust && !authorization) {
        actionErrors.value[index] = '此工作区的脚本与 MCP 自动执行尚未启用。'
      }
    }
    if (current === bridge) {
      const summary = staged.actions.length
        ? resultPrompt(staged, results.value, actionErrors.value)
        : '没有解析到可执行操作。请按约定重新给出工具请求，或直接回答。'
      current.outgoing = { text: bridgeResult(current.token, summary), attachments: attachments.value }
      if (listing.value && staged.actions.some(({ type }) => type === 'write' || type === 'mkdir' || type === 'run_script')) {
        listing.value = await invoke<Listing>('browse_workspace', { path: listing.value.path })
      }
    }
  } catch (error) {
    if (current === bridge && !current.outgoing) {
      current.outgoing = { text: bridgeResult(current.token, `工具请求无法执行：${String(error)}。请调整请求后重试。`) }
    }
  }
}

async function syncChat() {
  if (!desktop || !geminiEmbedded.value || !listing.value || !automatic.value || busy.value || bridgePolling) return
  bridgePolling = true
  const current = bridge
  try {
    const page = await invoke<ChatPage | null>('inspect_gemini_chat', {
      token: current.prepared ? current.token : null,
    })
    if (current !== bridge || busy.value) return
    if (!page) {
      bridgeStatus.value = '等待 Gemini 聊天框'
      return
    }
    const enteringConversation = current.pathname === '/app' && page.pathname.startsWith('/app/')
    if (current.pathname && current.pathname !== page.pathname && !enteringConversation) {
      bridge = newBridge()
      bridgeStatus.value = '正在接入新对话'
      return
    }
    current.pathname = page.pathname
    if (current.outgoing) {
      const message = current.outgoing
      if (message.attachments) {
        bridgeStatus.value = `正在向 Gemini 上传 ${message.attachments.indices.length} 个附件`
        busy.value = true
      }
      try {
        const sent = await sendToChat(current.pathname, message)
        if (current !== bridge) return
        if (sent) current.outgoing = undefined
        bridgeStatus.value = sent ? '工具结果已发回 Gemini' : '结果已就绪，等待对话空闲'
      } catch (error) {
        if (current === bridge && message.attachments) {
          current.outgoing = undefined
          uploadFailed.value = { pathname: current.pathname, token: current.token }
          bridgeStatus.value = '附件回传失败'
          notice.value = { text: String(error), error: true }
        } else {
          throw error
        }
      } finally {
        if (message.attachments) busy.value = false
      }
      return
    }
    if (!current.prepared) {
      if (page.pathname === '/app' || page.pathname === '/app/') {
        bridgeStatus.value = '已就绪，可在 Gemini 中聊天'
        return
      }
      const prepared = await sendToChat(page.pathname, { text: bridgePrompt(current.token, trustedTools.value, mcp.value) })
      if (current !== bridge) return
      current.prepared = prepared
      bridgeStatus.value = prepared ? '已接入本地工具，等待 Gemini' : '等待对话空闲后接入'
      return
    }
    const request = page.request
    if (!request) return
    const key = `${request.sequence}:${request.text}`
    if (current.seen.has(key)) return
    current.seen.add(key)
    await run(() => runRequestedTools(current, request.text))
  } catch (error) {
    if (current === bridge) {
      bridgeStatus.value = 'Gemini 接入暂时不可用，正在重试'
      notice.value = { text: String(error), error: true }
    }
  } finally {
    bridgePolling = false
  }
}

function stageReply(clipboard = false) {
  return run(async () => {
    if (clipboard) reply.value = await readText()
    const staged = await invoke<Plan>('stage_reply', { reply: reply.value })
    usePlan(staged)
    inform(staged.actions.length ? `已解析 ${staged.actions.length} 项操作。` : '已导入 Gemini 回复。')
  })
}

function resolve(index: number, approved: boolean) {
  const currentPlan = plan.value
  if (!currentPlan) return
  return run(async () => {
    const result = await executeStep(currentPlan, index, approved)
    if ((result?.type === 'write' || result?.type === 'mkdir' || result?.type === 'run_script') && listing.value) {
      listing.value = await invoke<Listing>('browse_workspace', { path: listing.value.path })
    }
  })
}

function copyResults() {
  const currentPlan = plan.value
  if (!currentPlan) return
  return run(async () => {
    await writeText(resultPrompt(currentPlan, results.value, actionErrors.value))
    inform('结果已复制。请检查内容，再粘贴到同一个 Gemini 对话。')
  })
}

function sendAttachments() {
  const currentPlan = plan.value
  const batch = attachments.value
  if (!currentPlan || !batch || !geminiEmbedded.value) return
  if (uploadFailed.value && currentPlan.id === automaticPlan.value) {
    if (uploadFailed.value.pathname !== bridge.pathname || uploadFailed.value.token !== bridge.token) {
      notice.value = { text: '原工具请求已失效，请在需要文件的 Gemini 对话中重新请求读取。', error: true }
      return
    }
    bridge.outgoing = { text: bridgeResult(bridge.token, resultPrompt(currentPlan, results.value, actionErrors.value)), attachments: batch }
    uploadFailed.value = undefined
    notice.value = undefined
    return
  }
  return run(async () => {
    const page = await invoke<ChatPage | null>('inspect_gemini_chat', { token: null })
    if (!page) throw new Error('请打开要接收附件的 Gemini 对话。')
    const sent = await sendToChat(page.pathname, { text: resultPrompt(currentPlan, results.value, actionErrors.value), attachments: batch })
    if (!sent) throw new Error('Gemini 目前有草稿、附件或正在回复，请在对话空闲后重试。')
    inform('文件已作为附件发送给 Gemini。')
  })
}
</script>

<template>
  <div class="app-shell" @keydown.esc="settingsOpen && closeSettings()">
    <header class="topbar">
      <button ref="settingsButton" class="button settings-toggle" :aria-expanded="settingsOpen"
        aria-controls="settings-card" @click="settingsOpen = !settingsOpen">
        <Settings :size="16" /> 设置
      </button>
    </header>

    <div class="content-frame" :class="{ 'settings-open': settingsOpen }">
      <main ref="geminiViewport" class="gemini-viewport" aria-label="Gemini 浏览器">
        <div v-if="!geminiEmbedded" class="gemini-placeholder">
          <p v-if="geminiOpening" role="status">正在打开 Gemini…</p>
          <template v-else-if="desktop">
            <p v-if="geminiStatus" role="status">{{ geminiStatus }}</p>
            <button class="button secondary" :disabled="busy" @click="openGemini">打开 Gemini</button>
          </template>
          <p v-else>请从 Gemini Agent 桌面应用打开 Gemini。</p>
        </div>
      </main>

      <aside v-show="settingsOpen" id="settings-card" class="settings-card" aria-labelledby="settings-title">
        <div class="settings-heading">
          <h1 id="settings-title">设置</h1>
          <button class="icon-button" title="关闭设置" aria-label="关闭设置" @click="closeSettings">
            <X :size="17" />
          </button>
        </div>
        <div class="settings-content">
          <section class="settings-section" aria-labelledby="browser-title">
            <h2 id="browser-title">浏览器</h2>
            <div class="browser-controls">
              <select v-model="browser" aria-label="Gemini 浏览器" :disabled="geminiVisible">
                <option value="chrome">Chrome</option>
                <option value="edge">Edge</option>
                <option value="default">默认浏览器</option>
              </select>
              <button class="button secondary" :disabled="!desktop || busy || geminiOpening" @click="openGemini">
                {{ geminiEmbedded ? '弹出 Gemini' : geminiOpening ? '正在打开…' : '打开 Gemini' }}
                <ArrowUpRight :size="15" />
              </button>
            </div>
          </section>

          <section class="settings-section" aria-labelledby="workspace-title">
            <div class="workspace-heading">
              <h2 id="workspace-title">工作区</h2>
              <button v-if="listing" class="icon-button" title="刷新目录" aria-label="刷新目录"
                :disabled="busy" @click="browse(listing.path)"><RefreshCw :size="15" /></button>
            </div>
            <button class="workspace-button" :disabled="!desktop || busy" @click="chooseWorkspace">
              <FolderOpen :size="18" />
              <span><strong>{{ workspaceName || '选择文件夹' }}</strong>
                <small v-if="listing" :title="listing.root">{{ listing.root }}</small>
                <small v-else>设置本地操作范围</small>
              </span>
              <ChevronRight :size="15" />
            </button>
            <template v-if="listing">
              <div class="directory-bar">
                <button v-if="listing.path !== '.'" class="icon-button" title="返回上一级" aria-label="返回上一级"
                  :disabled="busy" @click="browse(listing.path.split('/').slice(0, -1).join('/') || '.')">
                  <ArrowLeft :size="15" />
                </button>
                <code :title="listing.path">{{ listing.path === '.' ? '/' : listing.path }}</code>
              </div>
              <ul v-if="listing.entries.length" class="entries">
                <li v-for="entry in listing.entries" :key="entry.name">
                  <button :title="entry.kind === 'file' ? '将路径加入任务' : entry.name"
                    :disabled="busy || entry.kind === 'other'"
                    @click="entry.kind === 'directory' ? browse(entryPath(entry.name)) : addReference(entry.name)">
                    <Folder v-if="entry.kind === 'directory'" :size="15" class="folder-icon" />
                    <File v-else :size="15" />
                    <span>{{ entry.name }}</span>
                  </button>
                </li>
              </ul>
              <p v-else class="small muted">此目录为空。</p>
              <p v-if="listing.truncated" class="small muted">此目录只显示前 200 项。</p>
            </template>
            <p v-else class="small muted">选择项目目录后，可在 Gemini 对话中使用本地文件工具。</p>
          </section>

          <section v-if="listing" class="settings-section" aria-labelledby="tools-title">
            <h2 id="tools-title">本地工具</h2>
            <div class="bridge-status">
              <label><input v-model="automatic" type="checkbox" /> 自动执行和回传</label>
              <label v-if="automatic">
                <input type="checkbox" :checked="trustedTools" :disabled="busy" @change="setTrustedTools(($event.target as HTMLInputElement).checked)" />
                允许此工作区自动运行脚本和 MCP
              </label>
              <span v-if="automatic" role="status">{{ geminiEmbedded ? bridgeStatus : '等待 Gemini 窗口接入' }}</span>
            </div>
          </section>
          <p v-if="!desktop" class="notice error">请使用 Tauri 桌面窗口连接本地文件和剪贴板。</p>
          <div v-if="!geminiVisible || !automatic" class="steps">
            <section class="card" aria-labelledby="task-title">
              <div class="card-heading">
                <span class="step-icon"><Sparkles :size="18" /></span>
                <div><h2 id="task-title">发送任务</h2><p>复制提示词，粘贴到 Gemini 对话。</p></div>
              </div>
              <textarea v-model="task" aria-label="任务内容" placeholder="例如：读取 README.md，补充项目的启动步骤。" />
              <div class="card-actions">
                <button class="button primary" :disabled="!desktop || !listing || busy" @click="copyTask">
                  <Clipboard :size="15" /> {{ task.trim() ? '复制提示词' : '复制工具说明' }}
                </button>
              </div>
            </section>
            <section class="card" aria-labelledby="reply-title">
              <div class="card-heading">
                <span class="step-icon"><Code2 :size="19" /></span>
                <div><h2 id="reply-title">导入回复</h2><p>复制 Gemini 的回复，导入本地操作。</p></div>
              </div>
              <textarea v-model="reply" class="code-input" aria-label="Gemini 回复" placeholder="例如：读取文件 README.md" spellcheck="false" />
              <div class="card-actions">
                <button class="button secondary" :disabled="!desktop || !listing || busy" @click="stageReply(true)">
                  <ClipboardPaste :size="15" /> 从剪贴板导入
                </button>
                <button class="button quiet" :disabled="!desktop || !listing || !reply.trim() || busy" @click="stageReply()">
                  解析回复
                </button>
              </div>
            </section>
          </div>
          <McpSettings :ready="desktop && !!listing" :busy="busy" :info="mcp" @connect="connectMcp" @disconnect="disconnectMcp" />
          <p v-if="notice" class="notice" :class="{ error: notice.error }" :role="notice.error ? 'alert' : 'status'">
            {{ notice.text }}
          </p>
          <section class="review" aria-labelledby="review-title">
            <div class="review-heading">
              <div><h2 id="review-title">本地操作</h2><p v-if="plan?.actions.length">{{ plan.id === automaticPlan ? '自动执行' : `${pending} 项待审批` }} · {{ plan.actions.length }} 项操作</p></div>
              <button v-if="attachments && (!automatic || uploadFailed)" class="button secondary"
                :disabled="!geminiEmbedded || busy" @click="sendAttachments">
                {{ uploadFailed ? '重试上传' : '发送结果与附件' }}
              </button>
              <button v-else class="button secondary" :disabled="!processed || busy" @click="copyResults">
                <Clipboard :size="15" /> 复制结果
              </button>
            </div>
            <div v-if="!plan" class="empty">
              <span class="step-icon"><ListTree :size="19" /></span>
              <p>{{ geminiEmbedded && automatic ? 'Gemini 使用本地工具时，操作记录会显示在这里。' : '导入 Gemini 回复后，在这里查看和执行操作。' }}</p>
            </div>
            <template v-else>
              <p v-if="plan.message" class="model-message">{{ plan.message }}</p>
              <p v-if="!plan.actions.length" class="small muted">此回复没有请求本地操作。</p>
              <article v-for="(action, index) in plan.actions" :key="`${plan.id}-${index}`" class="operation">
                <div class="operation-heading">
                  <span class="operation-icon"><component :is="actionIcons[action.type]" :size="17" /></span>
                  <div class="operation-name"><h3>{{ actionNames[action.type] }}</h3><code>{{ actionTarget(action) }}</code></div>
                  <div v-if="!results[index] && plan.id !== automaticPlan" class="operation-buttons">
                    <button class="button quiet" :disabled="busy" @click="resolve(index, false)"><X :size="14" />跳过</button>
                    <button class="button primary" :disabled="busy" @click="resolve(index, true)"><Check :size="15" />批准执行</button>
                  </div>
                  <span v-else class="state" :class="{ skipped: results[index]?.type === 'skipped', failed: resultFailed(results[index]) }">
                    {{ resultLabel(results[index]) || (actionErrors[index] ? '执行失败' : '待执行') }}
                  </span>
                </div>
                <WritePreview v-if="action.type === 'write'" :before="action.before" :content="action.content" />
                <details v-if="action.type === 'run_script'" class="preview" open>
                  <summary>脚本源码与参数 <span>{{ action.script.runtime === 'bun' ? 'Bun' : 'PowerShell' }}</span></summary>
                  <p class="small muted">脚本以你的账户权限运行，工作区是启动目录。</p>
                  <pre class="file-output">{{ action.script.source || '（空脚本）' }}</pre>
                  <pre class="file-output">参数 {{ JSON.stringify(action.args) }}</pre>
                </details>
                <details v-if="action.type === 'mcp_call'" class="preview" open>
                  <summary>传递给 MCP 服务的参数</summary>
                  <pre class="file-output">{{ JSON.stringify(action.arguments, null, 2) }}</pre>
                </details>
                <p v-if="actionErrors[index]" class="notice error" role="alert">{{ actionErrors[index] }}</p>
                <template v-if="results[index]">
                  <p v-if="results[index]?.type === 'read'" class="small muted">
                    附件 {{ results[index]?.name }}（{{ results[index]?.bytes.toLocaleString() }} 字节）
                  </p>
                  <details v-else-if="results[index]?.type === 'list'" class="preview" open>
                    <summary>目录内容</summary><pre class="file-output">{{ results[index]?.listing.entries.map((item) => item.name + (item.kind === 'directory' ? '/' : '')).join('\n') || '（空目录）' }}</pre>
                    <p v-if="results[index]?.listing.truncated" class="small muted">此目录只显示前 200 项。</p>
                  </details>
                  <details v-else-if="results[index]?.type === 'run_script'" class="preview" open>
                    <summary>脚本输出 <span>{{ results[index]?.output.timedOut ? '已超时' : `退出码 ${results[index]?.output.exitCode ?? '未知'}` }}</span></summary>
                    <pre v-if="results[index]?.output.stdout" class="file-output">{{ results[index]?.output.stdout }}</pre>
                    <pre v-if="results[index]?.output.stderr" class="file-output removed">{{ results[index]?.output.stderr }}</pre>
                    <p v-if="results[index]?.output.error" class="small muted">{{ results[index]?.output.error }}</p>
                    <p v-if="results[index]?.output.truncated" class="small muted">输出已截断。</p>
                  </details>
                  <details v-else-if="results[index]?.type === 'mcp_list'" class="preview" open>
                    <summary>MCP 工具定义</summary>
                    <pre class="file-output">{{ JSON.stringify(results[index]?.info.tools, null, 2) }}</pre>
                  </details>
                  <details v-else-if="results[index]?.type === 'mcp_call'" class="preview" open>
                    <summary>MCP 工具返回</summary>
                    <pre class="file-output">{{ JSON.stringify(results[index]?.output, null, 2) }}</pre>
                  </details>
                </template>
              </article>
            </template>
          </section>
        </div>
      </aside>
    </div>
  </div>
</template>
