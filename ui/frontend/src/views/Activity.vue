// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { getToken } from '../api'
import PageHeader from '../components/PageHeader.vue'
import ResponsiveTable from '../components/ResponsiveTable.vue'
import { formatBytes, formatTime } from '../composables/useFormatters'

interface RequestEvent {
  seq: number
  timestamp: string
  request_id: string
  username: string
  ip: string
  method: string
  path: string
  status: number
  action: string
  elapsed_ns: number
  bytes_recv: number
  bytes_sent: number
}

const MAX_ROWS = 500

const events = ref<RequestEvent[]>([])
const paused = ref(false)
const filter = ref('')
const connection = ref<'connecting' | 'connected' | 'reconnecting' | 'forbidden'>('connecting')

let controller: AbortController | null = null
let closed = false
let reconnectDelay = 1000
const seen = new Set<number>()
// Events that arrive while paused are buffered so resume doesn't lose them.
let pausedBuffer: RequestEvent[] = []

function append(event: RequestEvent) {
  if (seen.has(event.seq)) return
  seen.add(event.seq)
  if (paused.value) {
    pausedBuffer.push(event)
    return
  }
  events.value.unshift(event)
  if (events.value.length > MAX_ROWS) {
    for (const dropped of events.value.splice(MAX_ROWS)) seen.delete(dropped.seq)
  }
}

function togglePause() {
  paused.value = !paused.value
  if (!paused.value) {
    const buffered = pausedBuffer
    pausedBuffer = []
    for (const e of buffered) {
      seen.delete(e.seq)
      append(e)
    }
  }
}

function clear() {
  events.value = []
  pausedBuffer = []
  seen.clear()
}

const filtered = computed(() => {
  const q = filter.value.trim().toLowerCase()
  if (!q) return events.value
  return events.value.filter(
    (e) =>
      e.username.toLowerCase().includes(q) ||
      e.ip.includes(q) ||
      e.path.toLowerCase().includes(q) ||
      e.method.toLowerCase() === q ||
      String(e.status) === q,
  )
})

function statusClass(status: number): string {
  if (status < 300) return 'status-ok'
  if (status < 400) return 'status-redirect'
  if (status < 500) return 'status-client-error'
  return 'status-server-error'
}

/// Real classifications are compact labels (docker.pull_manifest); the
/// unclassified fallback is "METHOD /path", which just duplicates the
/// Method and Path columns — render it as a dash.
function actionLabel(action: string): string {
  return action.includes('/') ? '\u2014' : action
}

function formatDuration(ns: number): string {
  const ms = ns / 1e6
  if (ms < 1) return '<1ms'
  if (ms < 1000) return `${Math.round(ms)}ms`
  return `${(ms / 1000).toFixed(2)}s`
}

async function connect() {
  if (closed) return
  const token = getToken()
  if (!token) return

  controller = new AbortController()
  try {
    // Cache-busting query param: see useEventStream for the Firefox story.
    const cacheBust = `${Date.now()}-${Math.random().toString(36).slice(2)}`
    const response = await fetch(`/api/v1/requests/stream?c=${cacheBust}`, {
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: 'text/event-stream',
      },
      signal: controller.signal,
    })

    if (response.status === 403) {
      connection.value = 'forbidden'
      return
    }
    if (!response.ok || !response.body) {
      throw new Error(`stream connection failed: ${response.status}`)
    }

    connection.value = 'connected'
    reconnectDelay = 1000

    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''

    while (true) {
      const { done, value } = await reader.read()
      if (done) break

      buffer += decoder.decode(value, { stream: true })
      const parts = buffer.split('\n\n')
      buffer = parts.pop() || ''

      for (const part of parts) {
        let eventType = 'message'
        let dataStr = ''
        for (const line of part.split('\n')) {
          if (line.startsWith('event:')) eventType = line.slice(6).trim()
          else if (line.startsWith('data:')) dataStr += line.slice(5).trim()
        }
        if (!dataStr) continue
        try {
          const payload = JSON.parse(dataStr)
          if (eventType === 'snapshot') {
            for (const e of payload as RequestEvent[]) append(e)
          } else if (eventType === 'request') {
            append(payload as RequestEvent)
          }
        } catch {
          // ignore parse errors
        }
      }
    }
  } catch (err: any) {
    if (err.name === 'AbortError') return
  }

  if (!closed && connection.value !== 'forbidden') {
    connection.value = 'reconnecting'
    setTimeout(connect, reconnectDelay)
    reconnectDelay = Math.min(reconnectDelay * 1.5, 30000)
  }
}

onMounted(connect)
onUnmounted(() => {
  closed = true
  controller?.abort()
})
</script>

<template>
  <section>
    <PageHeader>
      <template #actions>
        <span class="conn-state" :class="connection" data-testid="activity-connection">{{ connection }}</span>
        <button class="btn" data-testid="activity-pause" @click="togglePause">
          {{ paused ? 'Resume' : 'Pause' }}
        </button>
        <button class="btn" data-testid="activity-clear" @click="clear">Clear</button>
      </template>
    </PageHeader>

    <p v-if="connection === 'forbidden'" class="error">Admin access is required to view live requests.</p>

    <template v-else>
      <div class="activity-controls">
        <input
          v-model="filter"
          type="text"
          placeholder="Filter by user, IP, path, method, or status"
          class="activity-filter"
          data-testid="activity-filter"
        />
        <span class="hint">{{ filtered.length }} of {{ events.length }} requests</span>
      </div>

      <p v-if="events.length === 0" class="hint">Waiting for requests…</p>

      <ResponsiveTable v-else>
        <table data-testid="activity-table">
          <thead>
            <tr>
              <th>Time</th>
              <th>User</th>
              <th>IP</th>
              <th>Method</th>
              <th>Path</th>
              <th>Status</th>
              <th>Action</th>
              <th>Elapsed</th>
              <th>Xfer</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="e in filtered" :key="e.seq">
              <td class="nowrap">{{ formatTime(e.timestamp) }}</td>
              <td>{{ e.username }}</td>
              <td class="mono">{{ e.ip }}</td>
              <td>{{ e.method }}</td>
              <td class="mono path-col" :title="e.path">{{ e.path }}</td>
              <td><span class="status-badge" :class="statusClass(e.status)">{{ e.status }}</span></td>
              <td>{{ actionLabel(e.action) }}</td>
              <td class="nowrap">{{ formatDuration(e.elapsed_ns) }}</td>
              <td class="nowrap">{{ formatBytes(e.bytes_recv + e.bytes_sent) }}</td>
            </tr>
          </tbody>
        </table>
      </ResponsiveTable>
    </template>
  </section>
</template>

<style scoped>
.activity-controls {
  display: flex;
  align-items: center;
  gap: 1rem;
  margin-bottom: 0.75rem;
}
.activity-filter {
  flex: 0 1 24rem;
}
.conn-state {
  font-size: 0.8rem;
  opacity: 0.7;
  text-transform: capitalize;
  margin-right: 0.5rem;
}
.conn-state.connected {
  color: var(--color-success, #2e7d32);
}
.conn-state.reconnecting {
  color: var(--color-warning, #ed6c02);
}
.path-col {
  max-width: 48rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.status-badge.status-ok {
  color: var(--color-success, #2e7d32);
}
.status-badge.status-client-error {
  color: var(--color-warning, #ed6c02);
}
.status-badge.status-server-error {
  color: var(--color-error, #d32f2f);
}
</style>
