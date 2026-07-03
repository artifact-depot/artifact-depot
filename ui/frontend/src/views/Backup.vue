// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref } from 'vue'
import { api, type RestoreSummary } from '../api'
import ConfirmDialog from '../components/ConfirmDialog.vue'

const downloading = ref(false)
const restoring = ref(false)
const error = ref('')
const success = ref('')
const restoreResult = ref<RestoreSummary | null>(null)
const selectedFile = ref<File | null>(null)
const showConfirm = ref(false)

async function downloadBackup() {
  downloading.value = true
  error.value = ''
  try {
    await api.downloadBackup()
  } catch (e: any) {
    error.value = e.message || 'Failed to download backup'
  } finally {
    downloading.value = false
  }
}

function onFileSelected(event: Event) {
  const input = event.target as HTMLInputElement
  selectedFile.value = input.files?.[0] ?? null
  restoreResult.value = null
  error.value = ''
  success.value = ''
}

function confirmRestore() {
  if (!selectedFile.value) return
  showConfirm.value = true
}

async function doRestore() {
  showConfirm.value = false
  if (!selectedFile.value) return
  restoring.value = true
  error.value = ''
  success.value = ''
  restoreResult.value = null
  try {
    const text = await selectedFile.value.text()
    const data = JSON.parse(text)
    restoreResult.value = await api.restoreBackup(data)
    success.value = 'Restore completed successfully.'
  } catch (e: any) {
    error.value = e.message || 'Failed to restore backup'
  } finally {
    restoring.value = false
  }
}
</script>

<template>
  <section>
    <p class="subtitle">Export or import the full system configuration including repositories, stores, users, roles, and settings.</p>

    <div v-if="error" class="error">{{ error }}</div>
    <div v-if="success" class="success">{{ success }}</div>

    <!-- Backup -->
    <div class="card">
      <h3>Backup</h3>
      <p class="hint">Download a JSON file containing the complete system configuration.</p>
      <button class="btn btn-primary" :disabled="downloading" @click="downloadBackup">
        {{ downloading ? 'Downloading...' : 'Download Backup' }}
      </button>
    </div>

    <!-- Restore -->
    <div class="card">
      <h3>Restore</h3>
      <p class="hint">Upload a previously exported backup file to restore system configuration. Existing entities will be overwritten.</p>
      <div class="restore-row">
        <input type="file" accept=".json" @change="onFileSelected" />
        <button class="btn btn-primary" :disabled="restoring || !selectedFile" @click="confirmRestore">
          {{ restoring ? 'Restoring...' : 'Restore' }}
        </button>
      </div>
    </div>

    <!-- Restore result -->
    <div v-if="restoreResult" class="card">
      <h3>Restore Summary</h3>
      <dl class="summary">
        <dt>Stores</dt><dd>{{ restoreResult.stores }}</dd>
        <dt>Repositories</dt><dd>{{ restoreResult.repositories }}</dd>
        <dt>Roles</dt><dd>{{ restoreResult.roles }}</dd>
        <dt>Users</dt><dd>{{ restoreResult.users }}</dd>
        <dt>Settings</dt><dd>{{ restoreResult.settings ? 'Restored' : 'Skipped' }}</dd>
        <dt v-if="restoreResult.ldap_config !== undefined">LDAP Config</dt>
        <dd v-if="restoreResult.ldap_config !== undefined">{{ restoreResult.ldap_config ? 'Restored' : 'Skipped' }}</dd>
      </dl>
    </div>

    <!-- Confirmation dialog -->
    <ConfirmDialog
      v-if="showConfirm"
      title="Confirm Restore"
      action-label="Restore"
      action-loading-label="Restoring…"
      action-variant="primary"
      @confirm="doRestore"
      @cancel="showConfirm = false"
    >
      <p>This will overwrite existing configuration with the contents of <strong>{{ selectedFile?.name }}</strong>. Are you sure?</p>
    </ConfirmDialog>
  </section>
</template>

<style scoped>
section { max-width: 720px; margin: 0 auto; }

.subtitle {
  color: var(--color-text-muted);
  font-size: 0.9rem;
  margin: -0.5rem 0 1.5rem;
}

fieldset {
  border: 1px solid var(--color-border);
  border-radius: 6px;
  padding: 1rem 1.25rem;
  margin-bottom: 1rem;
}
legend {
  font-weight: 600;
  font-size: 0.95rem;
  padding: 0 0.4rem;
}

.restore-row {
  display: flex;
  align-items: center;
  gap: 1rem;
}

.summary {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 0.25rem 1rem;
  margin: 0;
}
.summary dt {
  font-weight: 600;
  font-size: 0.9rem;
}
.summary dd {
  margin: 0;
  font-size: 0.9rem;
}
</style>
