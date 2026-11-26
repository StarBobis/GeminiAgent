<script setup lang="ts">
import { computed } from 'vue'
import { diffLines } from 'diff'

const props = defineProps<{ before: string | null; content: string }>()
const diff = computed(() => {
  const changes = diffLines(props.before ?? '', props.content, { timeout: 100, maxEditLength: 1500 })
  if (!changes) return null
  return changes.filter((change) => change.added || change.removed)
})
</script>

<template>
  <details class="preview" open>
    <summary>写入预览 <span>{{ before === null ? '新建文件' : '覆盖现有文件' }}</span></summary>
    <pre v-if="diff?.length" class="diff" aria-label="变更内容"><span
      v-for="(change, index) in diff" :key="index"
      :class="change.added ? 'added' : 'removed'"
    >{{ change.added ? '+' : '−' }} {{ change.value }}</span></pre>
    <p v-else-if="diff" class="muted small">{{ before === null ? '文件内容为空。' : '文件内容相同。' }}</p>
    <details class="full-preview" :open="!diff">
      <summary>查看完整内容</summary>
      <div class="file-versions">
        <label v-if="before !== null">写入前<textarea readonly :value="before" spellcheck="false" /></label>
        <label>写入后<textarea readonly :value="content" spellcheck="false" /></label>
      </div>
    </details>
  </details>
</template>
