// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { ref } from 'vue'
import { getToken, clearToken } from '../api'
import { useRepoStore } from '../stores/repoStore'
import { useBlobStoreStore } from '../stores/blobStoreStore'
import { useTaskStore } from '../stores/taskStore'
import { useUserStore } from '../stores/userStore'
import { useRoleStore } from '../stores/roleStore'
import { useSettingsStore } from '../stores/settingsStore'
import { useHealthStore } from '../stores/healthStore'

export type ConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'disconnected'

export function useEventStream() {
  const state = ref<ConnectionState>('disconnected')
  let controller: AbortController | null = null
  let closed = false
  let reconnectDelay = 1000

  async function connect() {
    if (closed) return
    const token = getToken()
    if (!token) {
      state.value = 'disconnected'
      return
    }

    state.value = state.value === 'disconnected' ? 'connecting' : 'reconnecting'
    controller = new AbortController()

    try {
      // Per-connection unique URL. Firefox otherwise coalesces concurrent
      // requests to the identical SSE URL across windows/tabs and aborts the
      // second one (NS_BINDING_ABORTED), so a second window never receives its
      // snapshot and parks on "Loading…". A throwaway query param makes each
      // connection a distinct resource. The server ignores unknown query params.
      // (Date.now()+random, not crypto.randomUUID, since the latter is only
      // available in secure contexts and the UI is often served over plain HTTP.)
      const cacheBust = `${Date.now()}-${Math.random().toString(36).slice(2)}`
      const response = await fetch(`/api/v1/events/stream?c=${cacheBust}`, {
        headers: {
          'Authorization': `Bearer ${token}`,
          'Accept': 'text/event-stream',
        },
        signal: controller.signal,
      })

      if (response.status === 401 || response.status === 403) {
        clearToken()
        window.location.href = '/login'
        return
      }

      if (!response.ok || !response.body) {
        throw new Error(`SSE connection failed: ${response.status}`)
      }

      state.value = 'connected'
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
          const lines = part.split('\n')
          let eventType = 'message'
          let dataStr = ''
          for (const line of lines) {
            if (line.startsWith('event:')) {
              eventType = line.slice(6).trim()
            } else if (line.startsWith('data:')) {
              dataStr += line.slice(5).trim()
            }
          }
          if (!dataStr) continue
          try {
            const payload = JSON.parse(dataStr)
            dispatch(eventType, payload)
          } catch {
            // ignore parse errors
          }
        }
      }
    } catch (err: any) {
      if (err.name === 'AbortError') return
    }

    // Reconnect unless closed
    if (!closed) {
      state.value = 'reconnecting'
      setTimeout(connect, reconnectDelay)
      reconnectDelay = Math.min(reconnectDelay * 1.5, 30000)
    }
  }

  function dispatch(eventType: string, payload: any) {
    if (eventType === 'snapshot' || eventType === 'resync') {
      useRepoStore().applySnapshot(payload.repos ?? [])
      useBlobStoreStore().applySnapshot(payload.stores ?? [])
      useTaskStore().applySnapshot(payload.tasks ?? [])
      if (payload.users) useUserStore().applySnapshot(payload.users)
      if (payload.roles) useRoleStore().applySnapshot(payload.roles)
      if (payload.settings) useSettingsStore().applySnapshot(payload.settings)
      if (payload.health) useHealthStore().applySnapshot(payload.health)
      return
    }

    // The payload is the full Envelope: { seq, timestamp, topic, event }
    // The event data is nested in payload.event
    const eventData = payload.event ?? payload

    // Route domain events to the appropriate store
    if (eventType.startsWith('repo_')) {
      useRepoStore().applyEvent(eventType, eventData)
    } else if (eventType.startsWith('store_')) {
      useBlobStoreStore().applyEvent(eventType, eventData)
    } else if (eventType.startsWith('task_')) {
      useTaskStore().applyEvent(eventType, eventData)
    } else if (eventType.startsWith('user_')) {
      useUserStore().applyEvent(eventType, eventData)
    } else if (eventType.startsWith('role_')) {
      useRoleStore().applyEvent(eventType, eventData)
    } else if (eventType === 'settings_updated') {
      useSettingsStore().applyEvent(eventType, eventData)
    }
  }

  function start() {
    // Stop any existing connection first to prevent duplicates.
    if (controller) {
      controller.abort()
      controller = null
    }
    closed = false
    reconnectDelay = 1000
    connect()
  }

  function stop() {
    closed = true
    controller?.abort()
    controller = null
    state.value = 'disconnected'
  }

  return { state, start, stop }
}
