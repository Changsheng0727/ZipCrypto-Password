<script setup>
import { computed, onBeforeUnmount, ref } from 'vue'
import { Archive, CheckCircle2, ChevronRight, Cpu, FolderOpen, KeyRound, Play, RotateCcw, Square, Terminal, XCircle } from 'lucide-vue-next'

const archivePath = ref('')
const outputDirectory = ref('')
const workerCount = ref(Math.min(16, navigator.hardwareConcurrency || 8))
const knownHeader = ref('504b0304140000000800')
const job = ref(null)
const busy = ref(false)
const errorMessage = ref('')
let pollingTimer

const isDone = computed(() => ['completed', 'failed', 'cancelled'].includes(job.value?.status))
const statusLabel = computed(() => ({ queued: '排队中', running: '处理中', completed: '已完成', failed: '失败', cancelled: '已取消' }[job.value?.status] || '等待输入'))

function chooseArchive(event) {
  const file = event.target.files?.[0]
  if (file) {
    archivePath.value = file.path || file.name
    if (!file.path) errorMessage.value = '浏览器不会暴露完整路径，请把 ZIP 的绝对路径粘贴到输入框后再开始。'
  }
}

async function startJob() {
  errorMessage.value = ''
  if (!archivePath.value.trim()) return (errorMessage.value = '请先填写本机 ZIP 路径。')
  busy.value = true
  job.value = null
  const response = await fetch('/api/jobs', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ archivePath: archivePath.value, outputDirectory: outputDirectory.value || null, workerCount: workerCount.value, knownHeader: knownHeader.value }) })
  if (!response.ok) { errorMessage.value = (await response.json()).error || '任务创建失败'; busy.value = false; return }
  const created = await response.json()
  await pollJob(created.id)
}

async function pollJob(id) {
  const tick = async () => {
    const response = await fetch(`/api/jobs/${id}`)
    if (response.ok) job.value = await response.json()
    if (!isDone.value) pollingTimer = setTimeout(tick, 700)
    else busy.value = false
  }
  await tick()
}

async function cancelJob() { if (job.value) await fetch(`/api/jobs/${job.value.id}/cancel`, { method: 'POST' }) }
function reset() { clearTimeout(pollingTimer); job.value = null; busy.value = false; errorMessage.value = '' }
onBeforeUnmount(() => clearTimeout(pollingTimer))
</script>

<template>
  <main class="shell">
    <header class="topbar">
      <div class="brand"><div class="brand-mark"><Archive :size="18" /></div><div><strong>Local Archive Lab</strong><span>ZIP 恢复工作台</span></div></div>
      <div class="privacy"><ShieldCheck :size="16" /> 仅本机处理 · 不上传文件</div>
    </header>

    <section class="intro"><div><p class="eyebrow">ZIPCRYPTO / KNOWN-PLAINTEXT</p><h1>找回项目文件，<em>不必猜密码。</em></h1><p class="lede">用已知明文攻击恢复 ZipCrypto 内部密钥，再以高并发解密整包。所有步骤都在你的电脑上运行。</p></div><div class="status-chip"><span class="pulse"></span><span>{{ statusLabel }}</span></div></section>

    <section class="workspace">
      <div class="panel form-panel">
        <div class="panel-heading"><div><span class="step">01</span><h2>选择归档</h2></div><Archive :size="20" /></div>
        <label class="field-label" for="archive">原始 ZIP 路径</label>
        <div class="path-row"><input id="archive" v-model="archivePath" placeholder="D:\\projects\\项目文件.zip" /><label class="icon-button" title="选择文件"><FolderOpen :size="18" /><input type="file" accept=".zip" @change="chooseArchive" /></label></div>
        <p class="hint">后端会直接读取这个路径，浏览器不会加载大文件。浏览器安全限制下，选择文件后仍需粘贴绝对路径。</p>
        <label class="field-label" for="output">输出目录 <span>可选</span></label><input id="output" v-model="outputDirectory" placeholder="留空则输出到归档旁的 解密结果" />
      </div>
      <div class="panel form-panel">
        <div class="panel-heading"><div><span class="step">02</span><h2>配置恢复</h2></div><Cpu :size="20" /></div>
        <label class="field-label" for="workers">并发线程</label><div class="range-row"><input id="workers" v-model.number="workerCount" type="range" min="1" max="16" /><output>{{ workerCount }} <small>threads</small></output></div>
        <label class="field-label" for="header">已知 ZIP 头（十六进制）</label><input id="header" v-model="knownHeader" spellcheck="false" /><p class="hint">默认锚点：PK 本地文件头 + 版本、标志、压缩方法。</p>
        <div class="method"><KeyRound :size="16" /><span>自动尝试 A 题条目，成功后复用内部密钥。</span></div>
      </div>
    </section>

    <section class="action-row"><button class="primary" :disabled="busy" @click="startJob"><Play :size="17" /> {{ busy ? '正在恢复…' : '开始恢复' }}</button><button v-if="busy && !isDone" class="quiet danger" @click="cancelJob"><Square :size="15" /> 取消任务</button><button v-else class="quiet" @click="reset"><RotateCcw :size="16" /> 清空</button></section>
    <p v-if="errorMessage" class="error"><XCircle :size="17" />{{ errorMessage }}</p>

    <section v-if="job" class="panel progress-panel"><div class="progress-heading"><div><span class="step">03</span><h2>恢复记录</h2></div><span class="percent">{{ job.progress }}%</span></div><div class="progress-track"><span :style="{ width: `${job.progress}%` }"></span></div><div class="phase"><strong>{{ job.phase }}</strong><span>{{ job.message }}</span></div><div v-if="job.keys" class="key-line"><KeyRound :size="15" /><code>{{ job.keys }}</code></div><div class="logs"><div v-for="entry in job.logs" :key="entry.at + entry.message" :class="['log', entry.level]"><Terminal :size="14" /><span>{{ entry.message }}</span></div></div><div v-if="job.outputDirectory" class="output"><FolderOpen :size="16" /><span>结果目录：{{ job.outputDirectory }}</span></div></section>

    <footer><span><CheckCircle2 :size="14" /> 支持 ZipCrypto / Store / Deflate</span><span>bkcrack engine · v1</span></footer>
  </main>
</template>
