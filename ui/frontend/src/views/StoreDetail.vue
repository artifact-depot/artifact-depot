// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, type Store, type UpdateStoreRequest, type BrowseEntry, type StoreCheckResult } from '../api'
import ConfirmDialog from '../components/ConfirmDialog.vue'
import PageHeader from '../components/PageHeader.vue'
import ResponsiveTable from '../components/ResponsiveTable.vue'
import { formatDate, formatBytes } from '../composables/useFormatters'

const route = useRoute()
const router = useRouter()
const storeName = route.params.name as string
const store = ref<Store | null>(null)
const error = ref('')
const activeTab = ref<'browse' | 'manage'>('browse')

// Manage tab state
const saving = ref(false)
const saveError = ref('')
const saveSuccess = ref(false)
const showDeleteDialog = ref(false)
const deleting = ref(false)
const deleteError = ref('')
const checking = ref(false)
const checkResult = ref<StoreCheckResult | null>(null)

// Form fields
const formRoot = ref('')
const formSync = ref(true)
const formIoSize = ref(1048576)
const formDirectIo = ref(false)
const formBucket = ref('')
const formEndpoint = ref('')
const formRegion = ref('')
const formPrefix = ref('')
const formAccessKey = ref('')
const formSecretKey = ref('')
const formMaxRetries = ref(3)
const formConnectTimeout = ref(3)
const formReadTimeout = ref(30)
const formRetryMode = ref('standard')

// Browse tab state
const browsePath = ref('')
const browseEntries = ref<BrowseEntry[]>([])
const browseLoading = ref(false)
const browseError = ref('')

function populateForm(s: Store) {
  formRoot.value = s.root || ''
  formSync.value = s.sync ?? true
  formIoSize.value = s.io_size ?? 1048576
  formDirectIo.value = s.direct_io ?? false
  formBucket.value = s.bucket || ''
  formEndpoint.value = s.endpoint || ''
  formRegion.value = s.region || ''
  formPrefix.value = s.prefix || ''
  formAccessKey.value = s.access_key || ''
  formSecretKey.value = ''
  formMaxRetries.value = s.max_retries ?? 3
  formConnectTimeout.value = s.connect_timeout_secs ?? 3
  formReadTimeout.value = s.read_timeout_secs ?? 0
  formRetryMode.value = s.retry_mode ?? 'standard'
}

async function loadBrowse(path?: string) {
  browseLoading.value = true
  browseError.value = ''
  if (path !== undefined) browsePath.value = path
  try {
    const resp = await api.browseStore(storeName, browsePath.value || undefined)
    browsePath.value = resp.path
    browseEntries.value = resp.entries
  } catch (e: any) {
    browseError.value = e.message
  } finally {
    browseLoading.value = false
  }
}

function navigateUp() {
  // Go up one directory: "ab/cd/ef/" -> "ab/cd/"
  const parts = browsePath.value.replace(/\/$/, '').split('/')
  parts.pop()
  loadBrowse(parts.length > 0 ? parts.join('/') + '/' : '')
}

function navigateInto(name: string) {
  loadBrowse(browsePath.value + name + '/')
}

const breadcrumbs = computed(() => {
  const parts = browsePath.value.replace(/\/$/, '').split('/').filter(Boolean)
  const crumbs: { label: string; path: string }[] = [{ label: '/', path: '' }]
  let acc = ''
  for (const p of parts) {
    acc += p + '/'
    crumbs.push({ label: p, path: acc })
  }
  return crumbs
})

async function save() {
  if (!store.value) return
  saving.value = true
  saveError.value = ''
  saveSuccess.value = false
  try {
    const req: UpdateStoreRequest = {}
    if (store.value.store_type === 'file') {
      req.root = formRoot.value || undefined
      req.sync = formSync.value
      req.io_size = formIoSize.value
      req.direct_io = formDirectIo.value
    } else {
      req.bucket = formBucket.value || undefined
      req.endpoint = formEndpoint.value || undefined
      req.region = formRegion.value || undefined
      req.prefix = formPrefix.value
      req.access_key = formAccessKey.value || undefined
      req.secret_key = formSecretKey.value || undefined
      req.max_retries = formMaxRetries.value
      req.connect_timeout_secs = formConnectTimeout.value
      req.read_timeout_secs = formReadTimeout.value
      req.retry_mode = formRetryMode.value
    }
    const updated = await api.updateStore(storeName, req)
    store.value = updated
    populateForm(updated)
    saveSuccess.value = true
    setTimeout(() => saveSuccess.value = false, 3000)
  } catch (e: any) {
    saveError.value = e.message
  } finally {
    saving.value = false
  }
}

async function doCheck() {
  checking.value = true
  checkResult.value = null
  try {
    checkResult.value = await api.checkStore(storeName)
  } catch (e: any) {
    checkResult.value = { ok: false, error: e.message }
  } finally {
    checking.value = false
  }
}

async function doDelete() {
  deleting.value = true
  deleteError.value = ''
  try {
    await api.deleteStore(storeName)
    router.push({ name: 'stores' })
  } catch (e: any) {
    deleteError.value = e.message
  } finally {
    deleting.value = false
  }
}

onMounted(async () => {
  try {
    store.value = await api.getStore(storeName)
    populateForm(store.value)
    loadBrowse('')
  } catch (e: any) {
    error.value = e.message
  }
})
</script>

<template>
  <section>
    <PageHeader :title="storeName">
      <template #badges>
        <span v-if="store" class="badge" :class="store.store_type === 'file' ? 'badge-green' : 'badge-blue'">{{ store.store_type }}</span>
      </template>
    </PageHeader>

    <p v-if="error" class="error">{{ error }}</p>

    <div v-if="store" class="detail-card">
      <table class="detail-table">
        <tbody>
          <tr>
            <th>Type</th>
            <td>{{ store.store_type }}</td>
            <th>Created</th>
            <td>{{ formatDate(store.created_at) }}</td>
          </tr>
          <tr v-if="store.store_type === 'file'">
            <th>Root</th>
            <td>{{ store.root }}</td>
            <th>Sync</th>
            <td>{{ store.sync ? 'Yes' : 'No' }}</td>
          </tr>
          <tr v-if="store.store_type === 'file'">
            <th>I/O Buffer Size</th>
            <td>{{ store.io_size != null ? formatBytes(store.io_size * 1024) : '1 MB' }}</td>
            <th>O_DIRECT</th>
            <td>{{ store.direct_io ? 'Yes' : 'No' }}</td>
          </tr>
          <tr v-if="store.store_type === 's3'">
            <th>Bucket</th>
            <td>{{ store.bucket }}</td>
            <th>Region</th>
            <td>{{ store.region }}</td>
          </tr>
          <tr v-if="store.store_type === 's3' && store.endpoint">
            <th>Endpoint</th>
            <td>{{ store.endpoint }}</td>
          </tr>
          <tr v-if="store.store_type === 's3'">
            <th>Retry Mode</th>
            <td>{{ store.retry_mode ?? 'standard' }}</td>
            <th>Max Retries</th>
            <td>{{ store.max_retries ?? 3 }}</td>
          </tr>
          <tr v-if="store.store_type === 's3'">
            <th>Connect Timeout</th>
            <td>{{ store.connect_timeout_secs ?? 3 }}s</td>
            <th>Read Timeout</th>
            <td>{{ (store.read_timeout_secs ?? 0) === 0 ? 'disabled' : store.read_timeout_secs + 's' }}</td>
          </tr>
          <tr>
            <th>Blobs</th>
            <td>{{ store.blob_count != null ? store.blob_count.toLocaleString() : '\u2014' }}</td>
            <th>Size</th>
            <td>{{ store.total_bytes != null ? formatBytes(store.total_bytes) : '\u2014' }}</td>
          </tr>
        </tbody>
      </table>
    </div>

    <div v-if="store" class="tab-bar">
      <button :class="{ active: activeTab === 'browse' }" @click="activeTab = 'browse'">Browse</button>
      <button :class="{ active: activeTab === 'manage' }" @click="activeTab = 'manage'">Manage</button>
    </div>

    <!-- Browse tab -->
    <div v-if="store && activeTab === 'browse'" class="browse-section">
      <div class="breadcrumb-bar">
        <span v-for="(crumb, i) in breadcrumbs" :key="crumb.path">
          <span v-if="i > 0" class="breadcrumb-sep">/</span>
          <a v-if="i < breadcrumbs.length - 1" href="#" class="breadcrumb-link" @click.prevent="loadBrowse(crumb.path)">{{ crumb.label }}</a>
          <span v-else class="breadcrumb-current">{{ crumb.label }}</span>
        </span>
      </div>
      <p v-if="browseError" class="error">{{ browseError }}</p>
      <p v-if="browseLoading"><span class="loading-spinner"></span> Loading...</p>
      <template v-else>
        <ResponsiveTable v-if="browseEntries.length > 0 || browsePath !== ''">
          <table class="blob-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Size</th>
              </tr>
            </thead>
            <tbody>
              <tr v-if="browsePath !== ''" class="dir-row" tabindex="0" @click="navigateUp()" @keydown.enter="navigateUp()" style="cursor: pointer">
                <td>..</td>
                <td></td>
              </tr>
              <tr v-for="entry in browseEntries" :key="entry.name"
                  :class="{ 'dir-row': entry.is_dir }"
                  :style="entry.is_dir ? 'cursor: pointer' : ''"
                  :tabindex="entry.is_dir ? 0 : undefined"
                  @click="entry.is_dir && navigateInto(entry.name)"
                  @keydown.enter="entry.is_dir && navigateInto(entry.name)">
                <td>
                  <span v-if="entry.is_dir" class="dir-icon">&#128193;</span>
                  <code v-if="!entry.is_dir" class="hash-cell">{{ entry.name }}</code>
                  <span v-else>{{ entry.name }}/</span>
                </td>
                <td>{{ entry.size != null ? formatBytes(entry.size) : '' }}</td>
              </tr>
            </tbody>
          </table>
        </ResponsiveTable>
        <p v-else>Empty store.</p>
      </template>
    </div>

    <!-- Manage tab -->
    <div v-if="store && activeTab === 'manage'" class="manage-section">
      <form class="manage-form" @submit.prevent="save">
        <h3>Settings</h3>

        <div class="form-group">
          <label>Name</label>
          <input :value="store.name" disabled />
        </div>

        <div class="form-group">
          <label>Type</label>
          <input :value="store.store_type" disabled />
        </div>

        <div class="form-group">
          <label>Created</label>
          <input :value="formatDate(store.created_at)" disabled />
        </div>

        <hr />

        <!-- File-specific fields -->
        <template v-if="store.store_type === 'file'">
          <div class="form-group">
            <label for="root">Root Path</label>
            <input id="root" v-model="formRoot" placeholder="/data/blobs" />
          </div>
          <div class="form-group checkbox-group">
            <label>
              <input type="checkbox" v-model="formSync" />
              Fsync after writes
            </label>
          </div>
          <div class="form-group">
            <label for="io_size">I/O Buffer Size (KiB)</label>
            <input id="io_size" v-model.number="formIoSize" type="number" min="4" max="65536" step="4" />
          </div>
          <div class="form-group checkbox-group">
            <label>
              <input type="checkbox" v-model="formDirectIo" />
              O_DIRECT (bypass page cache)
            </label>
          </div>
        </template>

        <!-- S3-specific fields -->
        <template v-if="store.store_type === 's3'">
          <div class="form-group">
            <label for="bucket">Bucket</label>
            <input id="bucket" v-model="formBucket" placeholder="my-bucket" />
          </div>
          <div class="form-group">
            <label for="endpoint">Endpoint</label>
            <input id="endpoint" v-model="formEndpoint" placeholder="http://minio:9000" />
            <small class="hint">Optional. Leave blank for default AWS S3.</small>
          </div>
          <div class="form-group">
            <label for="region">Region</label>
            <input id="region" v-model="formRegion" placeholder="us-east-1" />
          </div>
          <div class="form-group">
            <label for="prefix">Prefix</label>
            <input id="prefix" v-model="formPrefix" placeholder="depot/" />
            <small class="hint">Optional path prefix within the bucket.</small>
          </div>
          <div class="form-group">
            <label for="access_key">Access Key</label>
            <input id="access_key" v-model="formAccessKey" />
            <small class="hint">Optional. Leave blank to use IAM credentials.</small>
          </div>
          <div class="form-group">
            <label for="secret_key">Secret Key</label>
            <input id="secret_key" v-model="formSecretKey" type="password" placeholder="unchanged" />
            <small class="hint">Optional. Leave blank to use IAM credentials.</small>
          </div>
          <div class="form-group">
            <label for="retry_mode">Retry Mode</label>
            <select id="retry_mode" v-model="formRetryMode">
              <option value="standard">standard</option>
              <option value="adaptive">adaptive</option>
            </select>
            <small class="hint">Adaptive adds client-side rate limiting for high-throughput workloads.</small>
          </div>
          <div class="form-group">
            <label for="max_retries">Max Retries</label>
            <input id="max_retries" v-model.number="formMaxRetries" type="number" min="0" max="20" />
          </div>
          <div class="form-group">
            <label for="connect_timeout_secs">Connect Timeout (seconds)</label>
            <input id="connect_timeout_secs" v-model.number="formConnectTimeout" type="number" min="1" max="7200" />
          </div>
          <div class="form-group">
            <label for="read_timeout_secs">Read Timeout (seconds)</label>
            <input id="read_timeout_secs" v-model.number="formReadTimeout" type="number" min="0" max="7200" />
            <small class="hint">0 = disabled (recommended for S3). Only set if your backend needs an idle timeout.</small>
          </div>
        </template>

        <p v-if="saveError" class="error">{{ saveError }}</p>
        <p v-if="saveSuccess" class="success">Settings saved.</p>

        <div class="form-actions">
          <button type="submit" class="btn btn-primary" :disabled="saving">
            {{ saving ? 'Saving...' : 'Save' }}
          </button>
        </div>
      </form>

      <hr />

      <div class="health-check">
        <h3>Health Check</h3>
        <p>Verify the store is accessible by writing, reading, listing, and deleting a test blob.</p>
        <button class="btn" :disabled="checking" @click="doCheck">
          {{ checking ? 'Checking...' : 'Check Health' }}
        </button>
        <p v-if="checkResult && checkResult.ok" class="success">Store is healthy.</p>
        <p v-if="checkResult && !checkResult.ok" class="error">{{ checkResult.error }}</p>
      </div>

      <hr />

      <div class="danger-zone">
        <h3>Danger Zone</h3>
        <p>Deleting a store permanently removes its configuration. The store must have no repositories or blobs.</p>
        <button class="btn btn-danger" @click="showDeleteDialog = true">Delete Store</button>
      </div>

      <!-- Delete confirmation dialog -->
      <ConfirmDialog
        v-if="showDeleteDialog"
        title="Delete Store"
        message="Are you sure you want to delete"
        :item-name="storeName"
        :error="deleteError"
        :loading="deleting"
        @confirm="doDelete"
        @cancel="showDeleteDialog = false"
      />
    </div>
  </section>
</template>

<style scoped>
/* Browse section */
.browse-section {
  margin-bottom: 1.5rem;
}
.blob-table th,
.blob-table td {
  border-bottom-color: var(--color-border);
}
.blob-table th {
  border-bottom: 2px solid var(--color-border-strong);
}
.hash-cell {
  font-size: 0.85rem;
  word-break: break-all;
}
.breadcrumb-bar {
  font-size: 0.9rem;
  margin-bottom: 0.75rem;
  font-family: monospace;
}
.breadcrumb-link {
  color: var(--color-blue);
  text-decoration: none;
}
.breadcrumb-link:hover {
  text-decoration: underline;
}
.breadcrumb-current {
  font-weight: 600;
}
.breadcrumb-sep {
  margin: 0 0.15rem;
  color: var(--color-text-disabled);
}
.dir-row:hover {
  background: var(--color-bg-hover);
}
.dir-icon {
  margin-right: 0.25rem;
}

/* Manage form */
.manage-section {
  max-width: 640px;
  margin: 0 auto;
}
.manage-form {
  margin-bottom: 1rem;
}
.hint {
  display: block;
  color: var(--color-text-faint);
  font-size: 0.78rem;
  margin-top: 0.2rem;
}
hr {
  border: none;
  border-top: 1px solid var(--color-border);
  margin: 1.5rem 0;
}

/* Danger zone overrides */
.danger-zone {
  margin-top: 1rem;
}
.danger-zone p {
  font-size: 0.9rem;
  color: var(--color-text-muted);
  margin-bottom: 1rem;
}
</style>
