// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { api, type TaskInfo, type SchedulerStatus } from '../api'
import { useTaskStore } from '../stores/taskStore'
import BaseModal from '../components/BaseModal.vue'
import ResponsiveTable from '../components/ResponsiveTable.vue'
import { formatBytes, formatTime } from '../composables/useFormatters'

const taskStore = useTaskStore()
const tasks = computed(() => taskStore.tasks)
const scheduler = ref<SchedulerStatus | null>(null)
const loading = ref(false)
const error = ref('')
const expandedTask = ref<string | null>(null)
const showCheckModal = ref(false)
const checkDryRun = ref(true)
const checkVerifyHashes = ref(true)
const showGcModal = ref(false)
const gcDryRun = ref(false)
const now = ref(Date.now())
let nowTimer: ReturnType<typeof setInterval> | null = null

// --- Singleton task state ---

const checkRunning = computed(() =>
  tasks.value.some(t => t.kind.type === 'check' && (t.status === 'running' || t.status === 'pending'))
)
const gcRunning = computed(() =>
  tasks.value.some(t => t.kind.type === 'blob_gc' && (t.status === 'running' || t.status === 'pending'))
)
const rebuildRunning = computed(() =>
  tasks.value.some(t => t.kind.type === 'rebuild_dir_entries' && (t.status === 'running' || t.status === 'pending'))
)

const activeGcTask = computed(() =>
  tasks.value.find(t => t.kind.type === 'blob_gc' && (t.status === 'running' || t.status === 'pending')) ?? null
)
const activeCheckTask = computed(() =>
  tasks.value.find(t => t.kind.type === 'check' && (t.status === 'running' || t.status === 'pending')) ?? null
)
const activeRebuildTask = computed(() =>
  tasks.value.find(t => t.kind.type === 'rebuild_dir_entries' && (t.status === 'running' || t.status === 'pending')) ?? null
)

function latestCompletedTask(type: string): TaskInfo | null {
  return tasks.value
    .filter(t => t.kind.type === type && t.status !== 'running' && t.status !== 'pending')
    .sort((a, b) => (b.completed_at || b.created_at || '').localeCompare(a.completed_at || a.created_at || ''))[0] ?? null
}

const lastGcTask = computed(() => latestCompletedTask('blob_gc'))
const lastCheckTask = computed(() => latestCompletedTask('check'))
const lastRebuildTask = computed(() => latestCompletedTask('rebuild_dir_entries'))

// --- GC scheduling ---

const gcBlockedReason = computed<string | null>(() => {
  if (gcRunning.value) return 'GC is already running'
  if (checkRunning.value) return 'Integrity check is running'
  if (scheduler.value?.gc_last_started_at) {
    const last = new Date(scheduler.value.gc_last_started_at).getTime()
    const minMs = scheduler.value.gc_min_interval_secs * 1000
    const remaining = Math.max(0, (last + minMs) - now.value)
    if (remaining > 0) {
      return `Min interval (${formatCountdown(remaining)} remaining)`
    }
  }
  return null
})

const checkBlockedReason = computed<string | null>(() => {
  if (checkRunning.value) return 'Check is already running'
  if (gcRunning.value) return 'GC is running'
  return null
})

const rebuildBlockedReason = computed<string | null>(() => {
  if (rebuildRunning.value) return 'Already running'
  return null
})

const gcNextRunTime = computed<Date | null>(() => {
  if (!scheduler.value?.gc_last_started_at) return null
  const last = new Date(scheduler.value.gc_last_started_at).getTime()
  return new Date(last + scheduler.value.gc_interval_secs * 1000)
})

const gcCountdown = computed(() => {
  if (!gcNextRunTime.value) return null
  const diff = gcNextRunTime.value.getTime() - now.value
  if (diff <= 0) return 'due now'
  return formatCountdown(diff)
})

// --- Transient tasks ---

const TRANSIENT_TTL_MS = 24 * 60 * 60 * 1000 // 24 hours
const TRANSIENT_LIMIT = 10

const transientTasks = computed(() => {
  const cutoff = now.value - TRANSIENT_TTL_MS
  return tasks.value
    .filter(t => ['repo_clean', 'bulk_delete', 'repo_clone'].includes(t.kind.type))
    .filter(t => {
      // Keep running/pending tasks regardless of age; expire finished tasks older than 24h.
      if (t.status === 'running' || t.status === 'pending') return true
      const timestamp = t.completed_at || t.created_at
      return new Date(timestamp).getTime() >= cutoff
    })
    .sort((a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime())
    .slice(0, TRANSIENT_LIMIT)
})

// --- Actions ---

async function fetchScheduler() {
  try {
    scheduler.value = await api.getSchedulerStatus()
  } catch {
    // non-critical
  }
}

function openCheckModal() {
  checkDryRun.value = true
  checkVerifyHashes.value = true
  showCheckModal.value = true
}

async function startCheck() {
  showCheckModal.value = false
  loading.value = true
  error.value = ''
  try {
    await api.startCheck(checkDryRun.value, checkVerifyHashes.value)
    await fetchScheduler()
  } catch (e: any) {
    error.value = e.message
  } finally {
    loading.value = false
  }
}

function openGcModal() {
  gcDryRun.value = false
  showGcModal.value = true
}

async function startGc() {
  showGcModal.value = false
  loading.value = true
  error.value = ''
  try {
    await api.startGc(gcDryRun.value)
    await fetchScheduler()
  } catch (e: any) {
    error.value = e.message
  } finally {
    loading.value = false
  }
}

async function startRebuildDirEntries() {
  loading.value = true
  error.value = ''
  try {
    await api.startRebuildDirEntries()
    await fetchScheduler()
  } catch (e: any) {
    error.value = e.message
  } finally {
    loading.value = false
  }
}

function toggleExpand(id: string) {
  expandedTask.value = expandedTask.value === id ? null : id
}

// --- Formatting helpers ---

function statusClass(status: string): string {
  switch (status) {
    case 'completed': return 'status-completed'
    case 'failed': return 'status-failed'
    case 'cancelled': return 'status-cancelled'
    case 'running': return 'status-running'
    default: return 'status-pending'
  }
}

function formatCountdown(ms: number): string {
  const totalSecs = Math.ceil(ms / 1000)
  const h = Math.floor(totalSecs / 3600)
  const m = Math.floor((totalSecs % 3600) / 60)
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m`
  return `${totalSecs}s`
}

function formatDuration(ns: number): string {
  if (ns < 1000) return `${ns}ns`
  if (ns < 1_000_000) return `${Math.floor(ns / 1000)}us`
  if (ns < 1_000_000_000) return `${Math.floor(ns / 1_000_000)}ms`
  return `${Math.floor(ns / 1_000_000_000)}s`
}



function progressPercent(task: TaskInfo): number {
  if (task.kind.type === 'check') {
    const blobWeight = task.progress.total_bytes
    const artifactWeight = task.progress.total_artifacts
    const totalWeight = blobWeight + artifactWeight
    if (totalWeight === 0) return 0
    const blobProgress = Math.min(task.progress.bytes_read, blobWeight)
    const artifactProgress = Math.min(task.progress.checked_artifacts, artifactWeight)
    return Math.min(100, Math.round(((blobProgress + artifactProgress) / totalWeight) * 100))
  } else if (task.kind.type === 'bulk_delete' || task.kind.type === 'rebuild_dir_entries' || task.kind.type === 'repo_clone') {
    if (task.progress.total_artifacts > 0)
      return Math.min(100, Math.round((task.progress.checked_artifacts / task.progress.total_artifacts) * 100))
    if (task.progress.total_repos > 0)
      return Math.round((task.progress.completed_repos / task.progress.total_repos) * 100)
    return 0
  } else {
    if (task.progress.total_blobs > 0)
      return Math.min(100, Math.round((task.progress.checked_blobs / task.progress.total_blobs) * 100))
    if (task.progress.total_repos > 0)
      return Math.round((task.progress.completed_repos / task.progress.total_repos) * 100)
  }
  return 0
}

function kindLabel(kind: { type: string; repo?: string; prefix?: string; source?: string; target?: string }): string {
  switch (kind.type) {
    case 'check': return 'check'
    case 'blob_gc': return 'garbage collection'
    case 'repo_clean': return `clean: ${kind.repo}`
    case 'rebuild_dir_entries': return 'rebuild dir entries'
    case 'bulk_delete': return `bulk delete: ${kind.repo}/${kind.prefix}`
    case 'repo_clone': return `clone: ${kind.source} -> ${kind.target}`
    default: return kind.type
  }
}

onMounted(() => {
  fetchScheduler()
  nowTimer = setInterval(() => { now.value = Date.now() }, 1000)
})

onUnmounted(() => {
  if (nowTimer) clearInterval(nowTimer)
})
</script>

<template>
  <div class="tasks-page">
    <div v-if="error" class="error-banner">{{ error }}</div>

    <!-- Check start modal -->
    <BaseModal v-if="showCheckModal" title="Start Integrity Check" max-width="440px" content-class="modal-dialog" @close="showCheckModal = false">
      <p class="modal-desc">
        Verify blob integrity and repository metadata consistency.
      </p>
      <label class="checkbox-row">
        <input type="checkbox" v-model="checkDryRun" />
        <span>Dry run <span class="hint">(report only, do not fix)</span></span>
      </label>
      <label class="checkbox-row">
        <input type="checkbox" v-model="checkVerifyHashes" />
        <span>Verify blob hashes <span class="hint">(read and hash every blob)</span></span>
      </label>
      <p v-if="!checkDryRun" class="fix-warning">
        Fix mode will remove dangling references and repair malformed paths.
      </p>
      <template #footer>
        <button class="btn btn-secondary" @click="showCheckModal = false">Cancel</button>
        <button class="btn btn-primary" @click="startCheck">Start</button>
      </template>
    </BaseModal>

    <!-- GC start modal -->
    <BaseModal v-if="showGcModal" title="Start Garbage Collection" max-width="440px" content-class="modal-dialog" @close="showGcModal = false">
      <p class="modal-desc">
        Expire old artifacts, clean orphaned blob records, and delete unreferenced blob files.
      </p>
      <label class="checkbox-row">
        <input type="checkbox" v-model="gcDryRun" />
        <span>Dry run <span class="hint">(report only, do not delete)</span></span>
      </label>
      <template #footer>
        <button class="btn btn-secondary" @click="showGcModal = false">Cancel</button>
        <button class="btn btn-primary" @click="startGc">Start</button>
      </template>
    </BaseModal>

    <!-- ============================================================ -->
    <!-- Singleton Tasks (always visible)                              -->
    <!-- ============================================================ -->

    <div class="singleton-section">
      <!-- GC Row -->
      <div class="singleton-card">
        <div class="singleton-header">
          <div class="singleton-title">
            <span class="status-dot" :class="activeGcTask ? statusClass(activeGcTask.status) : 'status-idle'"></span>
            <span class="singleton-name">Garbage Collection</span>
            <span v-if="activeGcTask" class="kind-badge">{{ activeGcTask.status }}</span>
          </div>
          <div class="singleton-actions">
            <button
              class="btn btn-primary btn-small"
              :disabled="!!gcBlockedReason || loading"
              :title="gcBlockedReason || 'Start a GC pass'"
              @click="openGcModal"
            >
              Start Now
            </button>
          </div>
        </div>
        <div v-if="gcBlockedReason && !gcRunning" class="blocked-hint">{{ gcBlockedReason }}</div>

        <!-- Active GC progress (clickable to expand detail) -->
        <div v-if="activeGcTask && activeGcTask.status === 'running'" class="progress-section clickable" role="button" tabindex="0" :aria-expanded="expandedTask === activeGcTask.id" @click="toggleExpand(activeGcTask.id)" @keydown.enter="toggleExpand(activeGcTask.id)">
          <div class="progress-bar">
            <div class="progress-fill" :style="{ width: progressPercent(activeGcTask) + '%' }"></div>
          </div>
          <div class="progress-stats">
            <span v-if="activeGcTask.progress.phase">{{ activeGcTask.progress.phase }} &mdash; </span>
            <template v-if="activeGcTask.progress.total_blobs > 0">
              {{ activeGcTask.progress.checked_blobs.toLocaleString() }} / {{ activeGcTask.progress.total_blobs.toLocaleString() }} artifacts
            </template>
            <template v-else-if="activeGcTask.progress.total_repos > 0">
              {{ activeGcTask.progress.completed_repos }} / {{ activeGcTask.progress.total_repos }} repos
            </template>
          </div>
        </div>
        <div v-if="activeGcTask && expandedTask === activeGcTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ activeGcTask.id }}</div>
            <div><strong>Status:</strong> {{ activeGcTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(activeGcTask.started_at) }}</div>
          </div>
          <div v-if="activeGcTask.summary" class="task-summary-detail">{{ activeGcTask.summary }}</div>
          <div v-if="activeGcTask.log.length > 0" class="log-section">
            <h4>Log</h4>
            <div class="log-entries">
              <div v-for="(entry, i) in activeGcTask.log" :key="i" class="log-entry">
                <span class="log-time">{{ formatTime(entry.timestamp) }}</span>
                <span class="log-msg">{{ entry.message }}</span>
              </div>
            </div>
          </div>
        </div>

        <!-- Schedule info (when idle) -->
        <div v-if="!activeGcTask && gcNextRunTime" class="schedule-info">
          <template v-if="gcCountdown === 'due now'">
            Next run due now
          </template>
          <template v-else>
            Next run in {{ gcCountdown }} ({{ gcNextRunTime.toLocaleString() }})
          </template>
        </div>
        <div v-if="!activeGcTask && !gcNextRunTime" class="schedule-info">
          No previous run recorded
        </div>

        <!-- Last result (expandable) -->
        <div v-if="lastGcTask" class="last-result" role="button" tabindex="0" :aria-expanded="expandedTask === lastGcTask.id" @click="toggleExpand(lastGcTask.id)" @keydown.enter="toggleExpand(lastGcTask.id)">
          <span class="status-dot" :class="statusClass(lastGcTask.status)"></span>
          <span class="last-label">Last run:</span>
          <span class="task-summary">{{ lastGcTask.summary || lastGcTask.status }}</span>
          <span class="task-time">{{ formatTime(lastGcTask.completed_at || lastGcTask.created_at) }}</span>
        </div>
        <div v-if="lastGcTask && expandedTask === lastGcTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ lastGcTask.id }}</div>
            <div><strong>Status:</strong> {{ lastGcTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(lastGcTask.started_at) }}</div>
            <div><strong>Completed:</strong> {{ formatTime(lastGcTask.completed_at) }}</div>
          </div>
          <div v-if="lastGcTask.result && lastGcTask.result.type === 'blob_gc'" class="gc-result">
            <div class="result-badges">
              <span v-if="lastGcTask.result.dry_run" class="badge badge-info">dry run</span>
              <span class="badge badge-info">{{ lastGcTask.result.scanned_artifacts }} artifacts scanned</span>
              <span class="badge badge-info">{{ lastGcTask.result.scanned_blobs }} blobs scanned</span>
              <span v-if="lastGcTask.result.expired_artifacts > 0" class="badge badge-warn">{{ lastGcTask.result.expired_artifacts }} expired</span>
              <span v-if="lastGcTask.result.deleted_dedup_refs > 0" class="badge badge-warn">{{ lastGcTask.result.deleted_dedup_refs }} dedup refs deleted</span>
              <span v-if="lastGcTask.result.orphaned_blobs_deleted > 0" class="badge badge-warn">{{ lastGcTask.result.orphaned_blobs_deleted }} orphan blobs deleted</span>
              <span v-if="lastGcTask.result.orphaned_blob_bytes_deleted > 0" class="badge badge-info">{{ formatBytes(lastGcTask.result.orphaned_blob_bytes_deleted) }} freed</span>
              <span v-if="lastGcTask.result.docker_deleted > 0" class="badge badge-info">{{ lastGcTask.result.docker_deleted }} Docker refs cleaned</span>
              <span class="badge badge-info">{{ formatDuration(lastGcTask.result.elapsed_ns) }}</span>
            </div>
          </div>
          <div v-if="lastGcTask.error" class="task-error"><strong>Error:</strong> {{ lastGcTask.error }}</div>
        </div>
      </div>

      <!-- Check Row -->
      <div class="singleton-card">
        <div class="singleton-header">
          <div class="singleton-title">
            <span class="status-dot" :class="activeCheckTask ? statusClass(activeCheckTask.status) : 'status-idle'"></span>
            <span class="singleton-name">Integrity Check</span>
            <span v-if="activeCheckTask" class="kind-badge">{{ activeCheckTask.status }}</span>
          </div>
          <div class="singleton-actions">
            <button
              class="btn btn-primary btn-small"
              :disabled="!!checkBlockedReason || loading"
              :title="checkBlockedReason || 'Start an integrity check'"
              @click="openCheckModal"
            >
              Start Now
            </button>
          </div>
        </div>
        <div v-if="checkBlockedReason && !checkRunning" class="blocked-hint">{{ checkBlockedReason }}</div>

        <div v-if="activeCheckTask && activeCheckTask.status === 'running'" class="progress-section clickable" role="button" tabindex="0" :aria-expanded="expandedTask === activeCheckTask.id" @click="toggleExpand(activeCheckTask.id)" @keydown.enter="toggleExpand(activeCheckTask.id)">
          <div class="progress-bar">
            <div class="progress-fill" :style="{ width: progressPercent(activeCheckTask) + '%' }"></div>
          </div>
          <div class="progress-stats">
            <span v-if="activeCheckTask.progress.phase">{{ activeCheckTask.progress.phase }} &mdash; </span>
            <template v-if="activeCheckTask.progress.phase === 'Checking repos'">
              {{ activeCheckTask.progress.checked_artifacts.toLocaleString() }} / {{ activeCheckTask.progress.total_artifacts.toLocaleString() }} artifacts
            </template>
            <template v-else>
              {{ formatBytes(activeCheckTask.progress.bytes_read) }} / {{ formatBytes(activeCheckTask.progress.total_bytes) }}
              ({{ activeCheckTask.progress.checked_blobs.toLocaleString() }} blobs)
            </template>
            <span v-if="activeCheckTask.progress.failed_blobs > 0" class="fail-count">
              {{ activeCheckTask.progress.failed_blobs }} failed
            </span>
          </div>
        </div>
        <div v-if="activeCheckTask && expandedTask === activeCheckTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ activeCheckTask.id }}</div>
            <div><strong>Status:</strong> {{ activeCheckTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(activeCheckTask.started_at) }}</div>
          </div>
          <div v-if="activeCheckTask.summary" class="task-summary-detail">{{ activeCheckTask.summary }}</div>
          <div v-if="activeCheckTask.log.length > 0" class="log-section">
            <h4>Log</h4>
            <div class="log-entries">
              <div v-for="(entry, i) in activeCheckTask.log" :key="i" class="log-entry">
                <span class="log-time">{{ formatTime(entry.timestamp) }}</span>
                <span class="log-msg">{{ entry.message }}</span>
              </div>
            </div>
          </div>
        </div>

        <div v-if="lastCheckTask" class="last-result" role="button" tabindex="0" :aria-expanded="expandedTask === lastCheckTask.id" @click="toggleExpand(lastCheckTask.id)" @keydown.enter="toggleExpand(lastCheckTask.id)">
          <span class="status-dot" :class="statusClass(lastCheckTask.status)"></span>
          <span class="last-label">Last run:</span>
          <span class="task-summary">{{ lastCheckTask.summary || lastCheckTask.status }}</span>
          <span class="task-time">{{ formatTime(lastCheckTask.completed_at || lastCheckTask.created_at) }}</span>
        </div>
        <div v-if="lastCheckTask && expandedTask === lastCheckTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ lastCheckTask.id }}</div>
            <div><strong>Status:</strong> {{ lastCheckTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(lastCheckTask.started_at) }}</div>
            <div><strong>Completed:</strong> {{ formatTime(lastCheckTask.completed_at) }}</div>
          </div>
          <div v-if="lastCheckTask.result && lastCheckTask.result.type === 'check'" class="check-result">
            <div class="result-badges">
              <span class="badge" :class="lastCheckTask.result.dry_run ? 'badge-info' : 'badge-warn'">{{ lastCheckTask.result.dry_run ? 'dry run' : 'fix mode' }}</span>
              <span class="badge badge-pass">{{ lastCheckTask.result.passed }} passed</span>
              <span v-if="lastCheckTask.result.mismatched > 0" class="badge badge-fail">{{ lastCheckTask.result.mismatched }} mismatched</span>
              <span v-if="lastCheckTask.result.missing > 0" class="badge badge-fail">{{ lastCheckTask.result.missing }} missing</span>
              <span v-if="lastCheckTask.result.errors > 0" class="badge badge-warn">{{ lastCheckTask.result.errors }} errors</span>
              <span v-if="lastCheckTask.result.fixed > 0" class="badge badge-fixed">{{ lastCheckTask.result.fixed }} fixed</span>
              <span class="badge badge-info">{{ formatBytes(lastCheckTask.result.bytes_read) }} read</span>
            </div>
            <div v-if="lastCheckTask.result.findings.length > 0" class="findings-section">
              <h5>Findings</h5>
              <ResponsiveTable>
                <table class="findings-table">
                  <thead>
                    <tr><th>Repo</th><th>Path</th><th>Expected</th><th>Actual</th><th>Error</th><th>Status</th></tr>
                  </thead>
                  <tbody>
                    <tr v-for="(finding, i) in lastCheckTask.result.findings" :key="i">
                      <td>{{ finding.repo || '-' }}</td>
                      <td class="mono">{{ finding.path }}</td>
                      <td class="mono">{{ finding.expected_hash ? finding.expected_hash.substring(0, 16) + '...' : '-' }}</td>
                      <td class="mono">{{ finding.actual_hash ? finding.actual_hash.substring(0, 16) + '...' : '-' }}</td>
                      <td>{{ finding.error || '-' }}</td>
                      <td><span v-if="finding.fixed" class="badge badge-fixed">fixed</span></td>
                    </tr>
                  </tbody>
                </table>
              </ResponsiveTable>
            </div>
          </div>
          <div v-if="lastCheckTask.error" class="task-error"><strong>Error:</strong> {{ lastCheckTask.error }}</div>
        </div>
      </div>

      <!-- Rebuild Dir Entries Row -->
      <div class="singleton-card">
        <div class="singleton-header">
          <div class="singleton-title">
            <span class="status-dot" :class="activeRebuildTask ? statusClass(activeRebuildTask.status) : 'status-idle'"></span>
            <span class="singleton-name">Rebuild Directory Entries</span>
            <span v-if="activeRebuildTask" class="kind-badge">{{ activeRebuildTask.status }}</span>
          </div>
          <div class="singleton-actions">
            <button
              class="btn btn-primary btn-small"
              :disabled="!!rebuildBlockedReason || loading"
              :title="rebuildBlockedReason || 'Rebuild directory entry counts'"
              @click="startRebuildDirEntries"
            >
              Start Now
            </button>
          </div>
        </div>
        <div v-if="rebuildBlockedReason && !rebuildRunning" class="blocked-hint">{{ rebuildBlockedReason }}</div>

        <div v-if="activeRebuildTask && activeRebuildTask.status === 'running'" class="progress-section clickable" role="button" tabindex="0" :aria-expanded="expandedTask === activeRebuildTask.id" @click="toggleExpand(activeRebuildTask.id)" @keydown.enter="toggleExpand(activeRebuildTask.id)">
          <div class="progress-bar">
            <div class="progress-fill" :style="{ width: progressPercent(activeRebuildTask) + '%' }"></div>
          </div>
          <div class="progress-stats">
            <span v-if="activeRebuildTask.progress.phase">{{ activeRebuildTask.progress.phase }} &mdash; </span>
            <template v-if="activeRebuildTask.progress.total_artifacts > 0">
              {{ activeRebuildTask.progress.checked_artifacts.toLocaleString() }} / {{ activeRebuildTask.progress.total_artifacts.toLocaleString() }} artifacts
            </template>
            <template v-else>
              {{ activeRebuildTask.progress.completed_repos }} / {{ activeRebuildTask.progress.total_repos }} repos
            </template>
          </div>
        </div>
        <div v-if="activeRebuildTask && expandedTask === activeRebuildTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ activeRebuildTask.id }}</div>
            <div><strong>Status:</strong> {{ activeRebuildTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(activeRebuildTask.started_at) }}</div>
          </div>
          <div v-if="activeRebuildTask.summary" class="task-summary-detail">{{ activeRebuildTask.summary }}</div>
          <div v-if="activeRebuildTask.log.length > 0" class="log-section">
            <h4>Log</h4>
            <div class="log-entries">
              <div v-for="(entry, i) in activeRebuildTask.log" :key="i" class="log-entry">
                <span class="log-time">{{ formatTime(entry.timestamp) }}</span>
                <span class="log-msg">{{ entry.message }}</span>
              </div>
            </div>
          </div>
        </div>

        <div v-if="lastRebuildTask" class="last-result" role="button" tabindex="0" :aria-expanded="expandedTask === lastRebuildTask.id" @click="toggleExpand(lastRebuildTask.id)" @keydown.enter="toggleExpand(lastRebuildTask.id)">
          <span class="status-dot" :class="statusClass(lastRebuildTask.status)"></span>
          <span class="last-label">Last run:</span>
          <span class="task-summary">{{ lastRebuildTask.summary || lastRebuildTask.status }}</span>
          <span class="task-time">{{ formatTime(lastRebuildTask.completed_at || lastRebuildTask.created_at) }}</span>
        </div>
        <div v-if="lastRebuildTask && expandedTask === lastRebuildTask.id" class="task-detail">
          <div class="detail-grid">
            <div><strong>ID:</strong> {{ lastRebuildTask.id }}</div>
            <div><strong>Status:</strong> {{ lastRebuildTask.status }}</div>
            <div><strong>Started:</strong> {{ formatTime(lastRebuildTask.started_at) }}</div>
            <div><strong>Completed:</strong> {{ formatTime(lastRebuildTask.completed_at) }}</div>
          </div>
          <div v-if="lastRebuildTask.result && lastRebuildTask.result.type === 'rebuild_dir_entries'" class="gc-result">
            <div class="result-badges">
              <span class="badge badge-info">{{ lastRebuildTask.result.repos }} repos</span>
              <span class="badge badge-info">{{ lastRebuildTask.result.entries_updated }} entries updated</span>
              <span class="badge badge-info">{{ formatDuration(lastRebuildTask.result.elapsed_ns) }}</span>
            </div>
          </div>
          <div v-if="lastRebuildTask.error" class="task-error"><strong>Error:</strong> {{ lastRebuildTask.error }}</div>
        </div>
      </div>
    </div>

    <!-- ============================================================ -->
    <!-- Background Tasks (transient, disappear after completion)      -->
    <!-- ============================================================ -->

    <div v-if="transientTasks.length > 0" class="transient-section">
      <h3>Background Tasks</h3>
      <div class="task-list">
        <div v-for="task in transientTasks" :key="task.id" class="task-card">
          <div class="task-header" role="button" tabindex="0" :aria-expanded="expandedTask === task.id" @click="toggleExpand(task.id)" @keydown.enter="toggleExpand(task.id)">
            <span class="status-dot" :class="statusClass(task.status)"></span>
            <span class="kind-badge">{{ kindLabel(task.kind) }}</span>
            <span class="task-summary">{{ task.summary || task.status }}</span>
            <span class="task-time">{{ formatTime(task.created_at) }}</span>
          </div>

          <div v-if="task.status === 'running'" class="progress-section">
            <div class="progress-bar">
              <div class="progress-fill" :style="{ width: progressPercent(task) + '%' }"></div>
            </div>
            <div class="progress-stats">
              <span v-if="task.progress.phase">{{ task.progress.phase }} &mdash; </span>
              <template v-if="task.kind.type === 'repo_clone'">
                {{ task.progress.checked_artifacts.toLocaleString() }} / {{ task.progress.total_artifacts.toLocaleString() }} artifacts copied
              </template>
              <template v-else-if="task.kind.type === 'bulk_delete'">
                {{ task.progress.checked_artifacts.toLocaleString() }} / {{ task.progress.total_artifacts.toLocaleString() }} artifacts deleted
              </template>
              <template v-else>
                {{ task.progress.checked_blobs.toLocaleString() }} / {{ task.progress.total_blobs.toLocaleString() }} artifacts
              </template>
            </div>
          </div>

          <div v-if="expandedTask === task.id" class="task-detail">
            <div class="detail-grid">
              <div><strong>ID:</strong> {{ task.id }}</div>
              <div><strong>Status:</strong> {{ task.status }}</div>
              <div><strong>Created:</strong> {{ formatTime(task.created_at) }}</div>
              <div><strong>Started:</strong> {{ formatTime(task.started_at) }}</div>
              <div><strong>Completed:</strong> {{ formatTime(task.completed_at) }}</div>
            </div>
            <div v-if="task.result && task.result.type === 'repo_clean'" class="gc-result">
              <div class="result-badges">
                <span class="badge badge-info">{{ task.result.scanned_artifacts }} artifacts scanned</span>
                <span v-if="task.result.expired_artifacts > 0" class="badge badge-warn">{{ task.result.expired_artifacts }} expired</span>
                <span v-if="task.result.docker_deleted > 0" class="badge badge-info">{{ task.result.docker_deleted }} Docker refs cleaned</span>
                <span class="badge badge-info">{{ formatDuration(task.result.elapsed_ns) }}</span>
              </div>
            </div>
            <div v-if="task.result && task.result.type === 'repo_clone'" class="gc-result">
              <div class="result-badges">
                <span class="badge badge-pass">{{ task.result.copied_artifacts }} artifacts copied</span>
                <span class="badge badge-info">{{ task.result.copied_entries }} dir entries copied</span>
                <span class="badge badge-info">{{ formatDuration(task.result.elapsed_ns) }}</span>
              </div>
            </div>
            <div v-if="task.result && task.result.type === 'bulk_delete'" class="gc-result">
              <div class="result-badges">
                <span class="badge badge-warn">{{ task.result.deleted_artifacts }} artifacts deleted</span>
                <span class="badge badge-info">{{ formatBytes(task.result.deleted_bytes) }} freed</span>
                <span class="badge badge-info">{{ formatDuration(task.result.elapsed_ns) }}</span>
              </div>
            </div>
            <div v-if="task.error" class="task-error"><strong>Error:</strong> {{ task.error }}</div>
            <div v-if="task.log.length > 0" class="log-section">
              <h4>Log</h4>
              <div class="log-entries">
                <div v-for="(entry, i) in task.log" :key="i" class="log-entry">
                  <span class="log-time">{{ formatTime(entry.timestamp) }}</span>
                  <span class="log-msg">{{ entry.message }}</span>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tasks-page {
  max-width: 1000px;
  margin: 0 auto;
}
.tasks-page h2 {
  margin-bottom: 1.5rem;
}

/* --- Singleton section --- */
.singleton-section {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
  margin-bottom: 2rem;
}
.singleton-card {
  border: 1px solid var(--color-border);
  border-radius: 8px;
  overflow: hidden;
}
.singleton-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 0.75rem 1rem;
}
.singleton-title {
  display: flex;
  align-items: center;
  gap: 0.6rem;
}
.singleton-name {
  font-weight: 600;
  font-size: 0.95rem;
}
.singleton-actions {
  display: flex;
  gap: 0.5rem;
}
.blocked-hint {
  padding: 0 1rem 0.5rem;
  font-size: 0.8rem;
  color: var(--color-text-faint);
}
.schedule-info {
  padding: 0 1rem 0.75rem;
  font-size: 0.85rem;
  color: var(--color-text-muted);
}
.last-result {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  padding: 0.5rem 1rem;
  border-top: 1px solid var(--color-border);
  cursor: pointer;
  font-size: 0.85rem;
  transition: background 0.15s;
}
.last-result:hover {
  background: var(--color-bg-subtle);
}
.last-label {
  color: var(--color-text-faint);
  white-space: nowrap;
}
.status-idle {
  background: #adb5bd;
}

/* --- Transient section --- */
.transient-section h3 {
  margin: 0 0 1rem;
  font-size: 1.05rem;
  color: var(--color-text-secondary);
}
.task-list {
  display: flex;
  flex-direction: column;
  gap: 0.75rem;
}
.task-card {
  border: 1px solid var(--color-border);
  border-radius: 8px;
  overflow: hidden;
}
.task-header {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  padding: 0.75rem 1rem;
  cursor: pointer;
  transition: background 0.15s;
}
.task-header:hover {
  background: var(--color-bg-subtle);
}

/* --- Shared --- */
.status-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  flex-shrink: 0;
}
.status-completed { background: #28a745; }
.status-failed { background: #dc3545; }
.status-cancelled { background: #6c757d; }
.status-running { background: #007bff; animation: pulse 1.5s infinite; }
.status-pending { background: #ffc107; }
@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.4; }
}
.kind-badge {
  background: var(--color-border);
  color: #495057;
  padding: 0.15rem 0.5rem;
  border-radius: 4px;
  font-size: 0.8rem;
  font-weight: 600;
  text-transform: uppercase;
}
.task-summary {
  flex: 1;
  color: var(--color-text-strong);
  font-size: 0.9rem;
}
.task-time {
  color: var(--color-text-faint);
  font-size: 0.8rem;
  white-space: nowrap;
}
.progress-section {
  padding: 0 1rem 0.75rem;
}
.progress-section.clickable {
  cursor: pointer;
}
.progress-section.clickable:hover {
  background: var(--color-bg-subtle);
}
.task-summary-detail {
  color: var(--color-text-secondary);
  font-size: 0.85rem;
  margin-bottom: 0.75rem;
}
.progress-bar {
  height: 6px;
  background: var(--color-border);
  border-radius: 3px;
  overflow: hidden;
  margin-bottom: 0.4rem;
}
.progress-fill {
  height: 100%;
  background: #007bff;
  transition: width 0.3s;
}
.progress-stats {
  font-size: 0.8rem;
  color: var(--color-text-muted);
}
.fail-count {
  color: #dc3545;
  font-weight: 600;
}
.task-detail {
  padding: 1rem;
  border-top: 1px solid var(--color-border);
  background: var(--color-bg-subtle);
  border-radius: 0 0 var(--radius-lg) var(--radius-lg);
}
.detail-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 0.5rem;
  margin-bottom: 1rem;
  font-size: 0.85rem;
}
.check-result h4,
.gc-result h4,
.log-section h4 {
  margin: 0 0 0.5rem;
  font-size: 0.95rem;
}
.result-badges {
  display: flex;
  gap: 0.5rem;
  flex-wrap: wrap;
  margin-bottom: 1rem;
}
.badge-pass { background: #d4edda; color: #155724; }
.badge-fail { background: #f8d7da; color: #721c24; }
.badge-warn { background: #fff3cd; color: #856404; }
.badge-info { background: #d1ecf1; color: #0c5460; }
.badge-fixed { background: #cce5ff; color: #004085; }
.findings-section h5 {
  margin: 0 0 0.5rem;
  font-size: 0.9rem;
}
.findings-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.8rem;
}
.findings-table th,
.findings-table td {
  text-align: left;
  padding: 0.4rem 0.6rem;
  border-bottom: 1px solid var(--color-border);
}
.findings-table th {
  background: var(--color-bg-disabled);
  font-weight: 600;
}
.mono {
  font-family: 'SF Mono', 'Fira Code', monospace;
  font-size: 0.75rem;
}
.task-error {
  background: #f8d7da;
  color: #721c24;
  padding: 0.5rem 0.75rem;
  border-radius: 4px;
  margin-bottom: 0.75rem;
  font-size: 0.85rem;
}
.log-section {
  margin-top: 0.75rem;
}
.log-entries {
  max-height: 300px;
  overflow-y: auto;
  font-size: 0.8rem;
  font-family: 'SF Mono', 'Fira Code', monospace;
}
.log-entry {
  display: flex;
  gap: 0.75rem;
  padding: 0.2rem 0;
}
.log-time {
  color: var(--color-text-faint);
  white-space: nowrap;
  flex-shrink: 0;
}
.log-msg {
  color: var(--color-text-strong);
}

.modal-desc {
  color: var(--color-text-muted);
  font-size: 0.9rem;
  margin-bottom: 1rem;
}
.checkbox-row {
  display: flex;
  align-items: baseline;
  gap: 0.5rem;
  cursor: pointer;
  font-size: 0.9rem;
  margin-bottom: 0.75rem;
}
.checkbox-row input[type="checkbox"] {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  position: relative;
  top: 2px;
}
.fix-warning {
  background: var(--color-orange-bg);
  color: #856404;
  padding: 0.5rem 0.75rem;
  border-radius: 6px;
  font-size: 0.85rem;
  margin-bottom: 0.75rem;
}
</style>
