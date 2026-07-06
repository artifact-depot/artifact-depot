// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { api, type Settings, type CorsConfig, type RateLimitConfig } from '../api'
import { useSettingsStore } from '../stores/settingsStore'

const settingsStore = useSettingsStore()
const loading = computed(() => !settingsStore.loaded)
const saving = ref(false)
const error = ref('')
const success = ref('')
const settings = ref<Settings>({
  access_log: true,
  max_artifact_bytes: 32 * 1024 * 1024 * 1024,
  default_docker_repo: null,
  cors: null,
  rate_limit: null,
  gc_interval_secs: 86400,
  gc_min_interval_secs: 3600,
  jwt_expiry_secs: 86400,
  jwt_rotation_interval_secs: 604800,
})

// Proxy values for optional fields so inputs bind cleanly
const dockerRepoEnabled = ref(false)
const dockerRepo = ref('')
const corsEnabled = ref(false)
const cors = ref<CorsConfig>({
  allowed_origins: [],
  allowed_methods: ['GET', 'HEAD', 'POST', 'PUT', 'DELETE', 'PATCH'],
  allowed_headers: ['Authorization', 'Content-Type', 'Accept'],
  max_age_secs: 3600,
})
const corsOriginsText = ref('')
const corsMethodsText = ref('')
const corsHeadersText = ref('')
const rateLimitEnabled = ref(false)
const rateLimit = ref<RateLimitConfig>({ requests_per_second: 100, burst_size: 50 })
const gcIntervalHours = ref(24)
const gcMinIntervalMinutes = ref(60)
const gcStartTime = ref('')

// Logging endpoints
const loggingOtlpEndpoint = ref('')

// Tracing
const tracingEndpoint = ref('')

const maxArtifactGiB = computed({
  get: () => settings.value.max_artifact_bytes / (1024 * 1024 * 1024),
  set: (v: number) => { settings.value.max_artifact_bytes = v * 1024 * 1024 * 1024 },
})

function loadProxies(s: Settings) {
  dockerRepoEnabled.value = s.default_docker_repo !== null
  dockerRepo.value = s.default_docker_repo ?? ''
  corsEnabled.value = s.cors !== null
  if (s.cors) {
    cors.value = { ...s.cors }
    corsOriginsText.value = s.cors.allowed_origins.join(', ')
    corsMethodsText.value = s.cors.allowed_methods.join(', ')
    corsHeadersText.value = s.cors.allowed_headers.join(', ')
  }
  rateLimitEnabled.value = s.rate_limit !== null
  if (s.rate_limit) {
    rateLimit.value = { ...s.rate_limit }
  }
  gcIntervalHours.value = (s.gc_interval_secs ?? 86400) / 3600
  gcMinIntervalMinutes.value = (s.gc_min_interval_secs ?? 3600) / 60
  gcStartTime.value = s.gc_start_time ?? ''
  // Logging endpoints
  loggingOtlpEndpoint.value = s.logging?.otlp_endpoint ?? ''
  tracingEndpoint.value = s.tracing_endpoint ?? ''
}

function buildSettings(): Settings {
  // Overlay the edited fields on the full loaded document so fields this
  // form doesn't render (base_url, task_retention_secs, ...) survive a save
  // instead of being reset server-side.
  return {
    ...settings.value,
    access_log: settings.value.access_log,
    max_artifact_bytes: settings.value.max_artifact_bytes,
    default_docker_repo: dockerRepoEnabled.value && dockerRepo.value ? dockerRepo.value : null,
    cors: corsEnabled.value ? {
      ...cors.value,
      allowed_origins: corsOriginsText.value.split(',').map(s => s.trim()).filter(Boolean),
      allowed_methods: corsMethodsText.value.split(',').map(s => s.trim()).filter(Boolean),
      allowed_headers: corsHeadersText.value.split(',').map(s => s.trim()).filter(Boolean),
    } : null,
    rate_limit: rateLimitEnabled.value ? { ...rateLimit.value } : null,
    gc_interval_secs: gcIntervalHours.value * 3600,
    gc_min_interval_secs: gcMinIntervalMinutes.value * 60,
    gc_start_time: gcStartTime.value || null,
    jwt_expiry_secs: settings.value.jwt_expiry_secs,
    jwt_rotation_interval_secs: settings.value.jwt_rotation_interval_secs,
    logging: {
      otlp_endpoint: loggingOtlpEndpoint.value || null,
    },
    tracing_endpoint: tracingEndpoint.value || null,
  }
}

function resetForm() {
  if (settingsStore.settings) {
    settings.value = { ...settingsStore.settings }
    loadProxies(settings.value)
  }
}

watch(() => settingsStore.settings, (s) => {
  if (s && !saving.value) {
    settings.value = { ...s }
    loadProxies(settings.value)
  }
}, { immediate: true })

async function save() {
  saving.value = true
  error.value = ''
  success.value = ''
  try {
    await api.updateSettings(buildSettings())
    success.value = 'Settings saved.'
    setTimeout(() => { success.value = '' }, 3000)
  } catch (e: any) {
    error.value = e.message || 'Failed to save settings'
  } finally {
    saving.value = false
  }
}

</script>

<template>
  <section>
    <p class="subtitle">Cluster-wide operational settings stored in the shared KV store.</p>

    <div v-if="error" class="error">{{ error }}</div>
    <div v-if="success" class="success">{{ success }}</div>
    <div v-if="loading" class="muted"><span class="loading-spinner"></span> Loading...</div>

    <form v-else @submit.prevent="save">
      <!-- Access Log -->
      <details class="settings-section" open>
        <summary>Logging <span class="badge-live">live</span></summary>
        <div class="settings-section-body">
          <label class="checkbox-group">
            <input type="checkbox" v-model="settings.access_log" />
            <span>Access log</span>
            <span class="hint">Log every HTTP request (method, path, status, duration)</span>
          </label>
        </div>
      </details>

      <!-- Upload Limit -->
      <details class="settings-section" open>
        <summary>Upload Limit <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <label class="field-row">
            <span>Max artifact size (GiB)</span>
            <input type="number" v-model.number="maxArtifactGiB" min="1" step="1" class="input-sm" />
          </label>
        </div>
      </details>

      <!-- Garbage Collection -->
      <details class="settings-section" open>
        <summary>Garbage Collection <span class="badge-live">live</span></summary>
        <div class="settings-section-body">
          <label class="field-row">
            <span>Garbage Collection Interval (hours)</span>
            <input type="number" v-model.number="gcIntervalHours" min="1" step="1" class="input-sm" />
            <span class="hint">How often the automatic garbage collection pass runs</span>
          </label>
          <label class="field-row">
            <span>Min Interval (minutes)</span>
            <input type="number" v-model.number="gcMinIntervalMinutes" min="1" step="1" class="input-sm" />
            <span class="hint">Minimum cooldown between any two garbage collection runs</span>
          </label>
          <label class="field-row">
            <span>Start Time (UTC)</span>
            <input type="time" v-model="gcStartTime" class="input-sm" data-testid="gc-start-time" />
            <span class="hint">Optional fixed daily start time; leave empty to run on the interval</span>
          </label>
        </div>
      </details>

      <!-- Default Docker Repo -->
      <details class="settings-section">
        <summary>Default Docker Repository <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <label class="checkbox-group">
            <input type="checkbox" v-model="dockerRepoEnabled" />
            <span>Enable</span>
            <span class="hint">Serve Docker V2 at /v2/ on the main listener for this repo</span>
          </label>
          <label v-if="dockerRepoEnabled" class="field-row">
            <span>Repository name</span>
            <input type="text" v-model="dockerRepo" placeholder="e.g. docker-hosted" class="input-md" />
          </label>
        </div>
      </details>

      <!-- CORS -->
      <details class="settings-section">
        <summary>CORS <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <label class="checkbox-group">
            <input type="checkbox" v-model="corsEnabled" />
            <span>Enable CORS headers</span>
          </label>
          <template v-if="corsEnabled">
            <label class="field-row">
              <span>Allowed origins</span>
              <input type="text" v-model="corsOriginsText" placeholder="http://localhost:3000, https://app.example.com" class="input-lg" />
            </label>
            <label class="field-row">
              <span>Allowed methods</span>
              <input type="text" v-model="corsMethodsText" placeholder="GET, POST, PUT, DELETE" class="input-lg" />
            </label>
            <label class="field-row">
              <span>Allowed headers</span>
              <input type="text" v-model="corsHeadersText" placeholder="Authorization, Content-Type" class="input-lg" />
            </label>
            <label class="field-row">
              <span>Max age (seconds)</span>
              <input type="number" v-model.number="cors.max_age_secs" min="0" class="input-sm" />
            </label>
          </template>
        </div>
      </details>

      <!-- Rate Limit -->
      <details class="settings-section">
        <summary>Rate Limiting <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <label class="checkbox-group">
            <input type="checkbox" v-model="rateLimitEnabled" />
            <span>Enable rate limiting</span>
            <span class="hint">Per-client-IP request throttling</span>
          </label>
          <template v-if="rateLimitEnabled">
            <label class="field-row">
              <span>Requests per second</span>
              <input type="number" v-model.number="rateLimit.requests_per_second" min="1" class="input-sm" />
            </label>
            <label class="field-row">
              <span>Burst size</span>
              <input type="number" v-model.number="rateLimit.burst_size" min="1" class="input-sm" />
            </label>
          </template>
        </div>
      </details>

      <!-- Logging Endpoints -->
      <details class="settings-section">
        <summary>Logging Endpoints <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <p class="hint">
            Depot emits logs to stdout and, when configured, to an OpenTelemetry collector via OTLP.
            Route the OTLP stream through an OTel collector to deliver logs to Loki, Splunk, S3, or files.
          </p>
          <label class="field-row">
            <span>OTLP Endpoint</span>
            <input type="text" v-model="loggingOtlpEndpoint" placeholder="http://loki:3100/otlp" class="input-lg" />
          </label>
        </div>
      </details>

      <!-- Tracing -->
      <details class="settings-section">
        <summary>Tracing <span class="badge-restart">restart</span></summary>
        <div class="settings-section-body">
          <label class="field-row">
            <span>OTLP Endpoint</span>
            <input type="text" v-model="tracingEndpoint" placeholder="http://jaeger:4317" class="input-lg" />
            <span class="hint">OpenTelemetry collector for traces</span>
          </label>
        </div>
      </details>

      <!-- Authentication -->
      <details class="settings-section" open>
        <summary>Authentication <span class="badge-live">live</span></summary>
        <div class="settings-section-body">
          <label class="field-row">
            <span>JWT token expiry (seconds)</span>
            <input type="number" v-model.number="settings.jwt_expiry_secs" min="60" step="60" class="input-sm" />
          </label>
          <label class="field-row">
            <span>JWT rotation interval (seconds)</span>
            <input type="number" v-model.number="settings.jwt_rotation_interval_secs" min="3600" step="3600" class="input-sm" />
          </label>
        </div>
      </details>

      <div class="actions">
        <button type="submit" class="btn btn-primary" :disabled="saving">
          {{ saving ? 'Saving...' : 'Save Settings' }}
        </button>
        <button type="button" class="btn" @click="resetForm" :disabled="saving">Reset</button>
      </div>
    </form>
  </section>
</template>

<style scoped>
section { max-width: 720px; margin: 0 auto; }

.subtitle {
  color: var(--color-text-muted);
  font-size: 0.9rem;
  margin: -0.5rem 0 1.5rem;
}

.settings-section {
  border: 1px solid var(--color-border);
  border-radius: var(--radius-lg);
  margin-bottom: 1rem;
}

.settings-section summary {
  padding: 0.75rem 1rem;
  font-weight: 600;
  cursor: pointer;
  user-select: none;
  list-style: none;
  display: flex;
  align-items: center;
  gap: 0.5rem;
}

.settings-section summary::-webkit-details-marker {
  display: none;
}

.settings-section summary::before {
  content: '\25B6';
  font-size: 0.7rem;
  transition: transform 0.15s;
  color: var(--color-text-faint);
}

.settings-section[open] summary::before {
  transform: rotate(90deg);
}

.settings-section-body {
  padding: 0 1rem 1rem;
}

.field-row {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  padding: 0.4rem 0;
}
.field-row > span:first-of-type {
  width: 280px;
  flex-shrink: 0;
  font-size: 0.9rem;
}

.hint {
  margin-left: 0.25rem;
}

input[type="text"], input[type="number"] {
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-md);
  font-size: 0.9rem;
}
.input-sm { width: 100px; }
.input-md { width: 220px; }
.input-lg { width: 100%; max-width: 420px; }

.actions {
  display: flex;
  gap: 0.75rem;
  margin-top: 1.5rem;
}

.error {
  background: var(--color-danger-banner-bg);
  color: var(--color-danger);
  padding: 0.75rem 1rem;
  border-radius: var(--radius-md);
  margin-bottom: 1rem;
  border: 1px solid var(--color-danger-banner-border);
}
.success {
  background: var(--color-success-banner-bg);
  color: var(--color-success);
  padding: 0.75rem 1rem;
  border-radius: var(--radius-md);
  margin-bottom: 1rem;
  border: 1px solid var(--color-success-banner-border);
}
.muted { color: var(--color-text-disabled); }

.badge-live, .badge-restart {
  font-size: 0.7rem;
  font-weight: 500;
  padding: 0.1rem 0.4rem;
  border-radius: var(--radius-sm);
  vertical-align: middle;
  margin-left: 0.4rem;
}
.badge-live {
  background: var(--color-success-bg);
  color: var(--color-success);
}
.badge-restart {
  background: var(--color-orange-bg);
  color: var(--color-orange);
}
</style>
