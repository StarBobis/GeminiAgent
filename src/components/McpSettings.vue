<script setup lang="ts">
import { ref } from 'vue'
import { Cable } from '@lucide/vue'
import type { McpConfig, McpInfo } from '../agent'

const props = defineProps<{ ready: boolean; busy: boolean; info?: McpInfo }>()
const emit = defineEmits<{ connect: [config: McpConfig]; disconnect: [] }>()
const program = ref('')
const argumentsText = ref('[]')
const error = ref('')

function connect() {
  error.value = ''
  try {
    const args: unknown = JSON.parse(argumentsText.value)
    if (!Array.isArray(args) || !args.every((arg) => typeof arg === 'string')) {
      throw new Error('expected string array')
    }
    emit('connect', { program: program.value.trim(), args })
  } catch {
    error.value = '请输入 JSON 字符串数组，例如 ["server.js", "."]。'
  }
}
</script>

<template>
  <details class="mcp-settings card">
    <summary>
      <span class="mcp-label"><Cable :size="17" /> MCP 服务</span>
      <span v-if="props.info" class="state">{{ props.info.name }} · {{ props.info.tools.length }} 个工具</span>
      <span v-else class="muted small">本地 stdio</span>
    </summary>
    <form class="mcp-form" @submit.prevent="connect">
      <p class="small muted">连接会启动该程序。服务以你的账户权限运行，请选择你信任的程序。</p>
      <div class="mcp-inputs">
        <label>可执行程序<input v-model="program" :disabled="busy" placeholder="例如 node.exe" required /></label>
        <label>参数（JSON 数组）<input v-model="argumentsText" :disabled="busy" spellcheck="false" /></label>
      </div>
      <p v-if="error" class="notice error" role="alert">{{ error }}</p>
      <div class="card-actions">
        <button class="button primary" :disabled="!ready || busy || !program.trim()">{{ info ? '重新连接' : '连接 MCP' }}</button>
        <button v-if="info" type="button" class="button secondary" :disabled="busy" @click="emit('disconnect')">
          断开连接
        </button>
      </div>
      <p v-if="info?.tools.length" class="mcp-tools"><code v-for="tool in info.tools" :key="tool.name">{{ tool.name }}</code></p>
    </form>
  </details>
</template>
