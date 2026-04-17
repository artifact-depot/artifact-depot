// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onUnmounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, type Artifact, type DirInfo, type TaskInfo } from '../api'
import BaseModal from './BaseModal.vue'
import ConfirmDialog from './ConfirmDialog.vue'
import ResponsiveTable from './ResponsiveTable.vue'
import { formatSize, formatDate } from '../composables/useFormatters'

const props = defineProps<{ repoName: string }>()

const route = useRoute()
const router = useRouter()

const dirs = ref<DirInfo[]>([])
const artifacts = ref<Artifact[]>([])
const loading = ref(false)
const prefix = computed(() => (route.query.path as string) || '')
const search = ref('')
const isSearchMode = ref(false)
const selectedArtifact = ref<Artifact | null>(null)
const showDetailModal = ref(false)
const deleteTarget = ref('')
const showDeleteDialog = ref(false)
const deleting = ref(false)
const deleteError = ref('')
const uploading = ref(false)
const uploadError = ref('')
const fileInput = ref<HTMLInputElement | null>(null)
const pageOffset = ref(0)
const pageLimit = ref(100)
const totalArtifacts = ref(0)

interface DisplayItem {
  name: string
  path: string
  isDir: boolean
  size: number
  content_type: string
  updated_at: string
  artifact_count?: number
  total_bytes?: number
}

const breadcrumbs = computed(() => {
  if (!prefix.value) return []
  const parts = prefix.value.split('/').filter(Boolean)
  const crumbs = [{ title: 'Root', prefix: '' }]
  let accumulated = ''
  for (const part of parts) {
    accumulated += part + '/'
    crumbs.push({ title: part, prefix: accumulated })
  }
  return crumbs
})

const displayItems = computed((): DisplayItem[] => {
  if (isSearchMode.value) {
    return artifacts.value.map(a => ({
      name: a.path,
      path: a.path,
      isDir: false,
      size: a.size,
      content_type: a.content_type,
      updated_at: a.updated_at,
    }))
  }

  const dirItems: DisplayItem[] = dirs.value.map(d => ({
    name: d.name,
    path: prefix.value + d.name + '/',
    isDir: true,
    size: d.total_bytes,
    content_type: '',
    updated_at: d.last_modified_at,
    artifact_count: d.artifact_count,
    total_bytes: d.total_bytes,
  }))

  const fileItems: DisplayItem[] = artifacts.value.map(a => ({
    name: a.path,
    path: prefix.value + a.path,
    isDir: false,
    size: a.size,
    content_type: a.content_type,
    updated_at: a.updated_at,
  }))

  return [...dirItems, ...fileItems]
})

function downloadUrl(path: string): string {
  return api.downloadUrl(props.repoName, path)
}

function onRowClick(item: DisplayItem) {
  if (item.isDir) {
    navigateTo(item.path)
  } else {
    const match = artifacts.value.find(a =>
      isSearchMode.value ? a.path === item.path : a.path === item.name
    )
    if (match) {
      selectedArtifact.value = match
      showDetailModal.value = true
    }
  }
}

function closeDetail() {
  selectedArtifact.value = null
  showDetailModal.value = false
}

function deleteFromDetail() {
  if (!selectedArtifact.value) return
  const fullPath = isSearchMode.value
    ? selectedArtifact.value.path
    : prefix.value + selectedArtifact.value.path
  deleteTarget.value = fullPath
  closeDetail()
  showDeleteDialog.value = true
}

function detailFullPath(): string {
  if (!selectedArtifact.value) return ''
  return isSearchMode.value
    ? selectedArtifact.value.path
    : prefix.value + selectedArtifact.value.path
}

function detailFilename(): string {
  const full = detailFullPath()
  const parts = full.split('/')
  return parts[parts.length - 1] || full
}

function navigateTo(newPrefix: string) {
  isSearchMode.value = false
  search.value = ''
  pageOffset.value = 0
  const query = { ...route.query }
  if (newPrefix) {
    query.path = newPrefix
  } else {
    delete query.path
  }
  if (query.path === route.query.path) {
    load()
  } else {
    router.push({ query })
  }
}

function doSearch() {
  if (!search.value) {
    clearSearch()
    return
  }
  isSearchMode.value = true
  pageOffset.value = 0
  load()
}

function clearSearch() {
  search.value = ''
  isSearchMode.value = false
  pageOffset.value = 0
  load()
}

async function load() {
  loading.value = true
  try {
    if (isSearchMode.value && search.value) {
      const resp = await api.listArtifacts(props.repoName, { q: search.value })
      dirs.value = resp.dirs
      artifacts.value = resp.artifacts
      totalArtifacts.value = resp.artifacts.length
    } else {
      const resp = await api.listArtifacts(props.repoName, {
        prefix: prefix.value,
        limit: pageLimit.value,
        offset: pageOffset.value,
      })
      dirs.value = resp.dirs
      artifacts.value = resp.artifacts
      totalArtifacts.value = resp.total ?? resp.artifacts.length
    }
  } catch {
    dirs.value = []
    artifacts.value = []
    totalArtifacts.value = 0
  } finally {
    loading.value = false
  }
}

const currentPage = computed(() => Math.floor(pageOffset.value / pageLimit.value) + 1)
const totalPages = computed(() => Math.ceil(totalArtifacts.value / pageLimit.value) || 1)
const showingFrom = computed(() => totalArtifacts.value === 0 ? 0 : pageOffset.value + 1)
const showingTo = computed(() => Math.min(pageOffset.value + pageLimit.value, totalArtifacts.value))
const hasPrev = computed(() => pageOffset.value > 0)
const hasNext = computed(() => pageOffset.value + pageLimit.value < totalArtifacts.value)

function prevPage() {
  pageOffset.value = Math.max(0, pageOffset.value - pageLimit.value)
  load()
}

function nextPage() {
  pageOffset.value += pageLimit.value
  load()
}

function confirmDelete(path: string, e: Event) {
  e.stopPropagation()
  deleteTarget.value = path
  deleteError.value = ''
  showDeleteDialog.value = true
}

async function doDelete() {
  deleting.value = true
  deleteError.value = ''
  try {
    await api.deleteArtifact(props.repoName, deleteTarget.value)
    showDeleteDialog.value = false
    await load()
  } catch (e: any) {
    deleteError.value = e.message || 'Delete failed'
  } finally {
    deleting.value = false
  }
}

// --- Directory bulk delete ---
const showDirDeleteDialog = ref(false)
const dirDeleteTarget = ref<DisplayItem | null>(null)
const dirDeleteTask = ref<TaskInfo | null>(null)
const dirDeletePhase = ref<'confirm' | 'progress' | 'done'>('confirm')
const dirDeleteError = ref('')
let dirDeletePollTimer: ReturnType<typeof setInterval> | null = null

function confirmDirDelete(item: DisplayItem, e: Event) {
  e.stopPropagation()
  dirDeleteTarget.value = item
  dirDeleteTask.value = null
  dirDeletePhase.value = 'confirm'
  dirDeleteError.value = ''
  showDirDeleteDialog.value = true
}

async function doDirDelete() {
  if (!dirDeleteTarget.value) return
  dirDeletePhase.value = 'progress'
  dirDeleteError.value = ''
  try {
    const task = await api.startBulkDelete(props.repoName, dirDeleteTarget.value.path)
    dirDeleteTask.value = task
    startDirDeletePolling(task.id)
  } catch (e: any) {
    dirDeleteError.value = e.message || 'Failed to start bulk delete'
    dirDeletePhase.value = 'done'
  }
}

function startDirDeletePolling(taskId: string) {
  stopDirDeletePolling()
  dirDeletePollTimer = setInterval(async () => {
    try {
      const task = await api.getTask(taskId)
      dirDeleteTask.value = task
      if (task.status === 'completed' || task.status === 'failed' || task.status === 'cancelled') {
        stopDirDeletePolling()
        dirDeletePhase.value = 'done'
        if (task.status === 'failed') {
          dirDeleteError.value = task.error || 'Bulk delete failed'
        }
      }
    } catch {
      // ignore poll errors
    }
  }, 2000)
}

function stopDirDeletePolling() {
  if (dirDeletePollTimer) {
    clearInterval(dirDeletePollTimer)
    dirDeletePollTimer = null
  }
}

async function cancelDirDelete() {
  if (dirDeleteTask.value) {
    try {
      await api.deleteTask(dirDeleteTask.value.id)
    } catch {
      // ignore
    }
  }
}

function closeDirDelete() {
  stopDirDeletePolling()
  const wasCompleted = dirDeleteTask.value?.status === 'completed'
  showDirDeleteDialog.value = false
  dirDeleteTarget.value = null
  dirDeleteTask.value = null
  if (wasCompleted) {
    load()
  }
}

function dirDeleteProgress(): number {
  if (!dirDeleteTask.value) return 0
  const p = dirDeleteTask.value.progress
  if (p.total_artifacts === 0) return 0
  return Math.min(100, Math.round((p.checked_artifacts / p.total_artifacts) * 100))
}

// --- Directory download ---
const showDirDownloadDialog = ref(false)
const dirDownloadTarget = ref<DisplayItem | null>(null)
const dirDownloadFormat = ref('tar.gz')
const dirDownloading = ref(false)
const dirDownloadError = ref('')

function confirmDirDownload(item: DisplayItem, e: Event) {
  e.stopPropagation()
  dirDownloadTarget.value = item
  dirDownloadFormat.value = 'tar.gz'
  dirDownloadError.value = ''
  dirDownloading.value = false
  showDirDownloadDialog.value = true
}

async function doDirDownload() {
  if (!dirDownloadTarget.value) return
  dirDownloading.value = true
  dirDownloadError.value = ''
  try {
    await api.downloadArchive(
      props.repoName,
      dirDownloadTarget.value.path,
      dirDownloadFormat.value,
    )
    showDirDownloadDialog.value = false
  } catch (e: any) {
    dirDownloadError.value = e.message || 'Download failed'
  } finally {
    dirDownloading.value = false
  }
}

onUnmounted(() => {
  stopDirDeletePolling()
})

function triggerUpload() {
  uploadError.value = ''
  fileInput.value?.click()
}

async function onFileSelected(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  input.value = ''
  uploading.value = true
  uploadError.value = ''
  try {
    await api.uploadArtifact(props.repoName, prefix.value + file.name, file)
    await load()
  } catch (e: any) {
    uploadError.value = e.message || 'Upload failed'
  } finally {
    uploading.value = false
  }
}

watch(
  [() => props.repoName, () => route.query.path],
  (newVals, oldVals) => {
    if (oldVals && newVals[0] !== oldVals[0]) {
      isSearchMode.value = false
      search.value = ''
      pageOffset.value = 0
    }
    load()
  },
  { immediate: true },
)
</script>

<template>
  <div class="artifact-browser">
    <div class="browser-header">
      <h3>Artifacts</h3>
      <div class="header-actions">
        <input ref="fileInput" type="file" hidden @change="onFileSelected" />
        <button class="btn btn-upload" :disabled="uploading" @click="triggerUpload">
          {{ uploading ? 'Uploading...' : 'Upload' }}
        </button>
        <div class="search-box">
          <input
            v-model="search"
            placeholder="Search artifacts..."
            @keyup.enter="doSearch"
          />
          <button v-if="search" class="clear-btn" @click="clearSearch">&times;</button>
        </div>
      </div>
    </div>

    <p v-if="uploadError" class="upload-error">{{ uploadError }}</p>

    <!-- Breadcrumbs -->
    <div v-if="breadcrumbs.length" class="breadcrumbs">
      <span
        v-for="(crumb, i) in breadcrumbs"
        :key="i"
        class="crumb"
        @click="navigateTo(crumb.prefix)"
      >
        {{ crumb.title }}<span v-if="i < breadcrumbs.length - 1" class="separator">/</span>
      </span>
    </div>

    <p v-if="loading"><span class="loading-spinner"></span> Loading...</p>

    <ResponsiveTable v-else-if="displayItems.length > 0">
      <table>
        <colgroup>
          <col style="width: auto;" />
          <col style="width: 10rem;" />
          <col style="width: 12rem;" />
          <col style="width: 14rem;" />
          <col style="width: 5rem;" />
        </colgroup>
        <thead>
          <tr>
            <th>Name</th>
            <th>Size</th>
            <th>Content Type</th>
            <th>Updated</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="item in displayItems"
            :key="item.path"
            class="clickable-row"
            tabindex="0"
            @click="onRowClick(item)"
            @keydown.enter="onRowClick(item)"
          >
            <td>
              <span class="item-icon" :class="item.isDir ? 'icon-folder' : 'icon-file'">
                {{ item.isDir ? '\uD83D\uDCC1' : '\uD83D\uDCC4' }}
              </span>
              {{ item.name }}
            </td>
            <td>
              <template v-if="item.isDir">
                {{ formatSize(item.total_bytes || 0) }}
                <span class="dir-count">({{ item.artifact_count }} items)</span>
              </template>
              <template v-else>{{ formatSize(item.size) }}</template>
            </td>
            <td>{{ item.isDir ? 'Folder' : item.content_type }}</td>
            <td>{{ formatDate(item.updated_at) }}</td>
            <td>
              <template v-if="item.isDir">
                <button class="action-btn" @click="confirmDirDownload(item, $event)" title="Download directory">&#11015;</button>
                <button class="action-btn action-delete" @click="confirmDirDelete(item, $event)" title="Delete directory">&#10005;</button>
              </template>
              <template v-else>
                <a :href="downloadUrl(item.path)" target="_blank" class="action-btn" title="Download">&#11015;</a>
                <button class="action-btn action-delete" @click="confirmDelete(item.path, $event)" title="Delete">&#10005;</button>
              </template>
            </td>
          </tr>
        </tbody>
      </table>
    </ResponsiveTable>

    <div v-if="!loading && !isSearchMode && totalArtifacts > pageLimit" class="pagination-bar">
      <span class="page-info">
        Showing {{ showingFrom }}&ndash;{{ showingTo }} of {{ totalArtifacts }} artifacts
      </span>
      <div class="page-controls">
        <button class="btn btn-page" :disabled="!hasPrev" @click="prevPage">&larr; Prev</button>
        <span class="page-number">Page {{ currentPage }} of {{ totalPages }}</span>
        <button class="btn btn-page" :disabled="!hasNext" @click="nextPage">Next &rarr;</button>
      </div>
    </div>

    <p v-if="!loading && displayItems.length === 0 && totalArtifacts === 0" class="empty-state">No artifacts. Upload via the button above or push via the API.</p>

    <!-- Detail modal -->
    <BaseModal v-if="showDetailModal && selectedArtifact" max-width="550px" content-class="modal-detail" :show-close="true" @close="closeDetail">
      <h3>{{ detailFilename() }}</h3>
      <dl class="detail-list">
        <dt>Full Path</dt>
        <dd>{{ detailFullPath() }}</dd>
        <dt>UUID</dt>
        <dd class="mono">{{ selectedArtifact.id }}</dd>
        <dt>Blob ID</dt>
        <dd class="mono">{{ selectedArtifact.blob_id }}</dd>
        <dt>BLAKE3 Hash</dt>
        <dd class="mono">{{ selectedArtifact.content_hash || 'N/A' }}</dd>
        <dt>ETag</dt>
        <dd class="mono">{{ selectedArtifact.etag || 'N/A' }}</dd>
        <dt>Size</dt>
        <dd>{{ formatSize(selectedArtifact.size) }}</dd>
        <dt>Content Type</dt>
        <dd>{{ selectedArtifact.content_type }}</dd>
        <dt>Created</dt>
        <dd>{{ formatDate(selectedArtifact.created_at) }}</dd>
        <dt>Updated</dt>
        <dd>{{ formatDate(selectedArtifact.updated_at) }}</dd>
        <dt>Last Accessed</dt>
        <dd>{{ formatDate(selectedArtifact.last_accessed_at) }}</dd>
      </dl>
      <template #footer>
        <a :href="downloadUrl(detailFullPath())" target="_blank" class="btn btn-download">Download</a>
        <button class="btn btn-danger" @click="deleteFromDetail">Delete</button>
      </template>
    </BaseModal>

    <!-- Delete dialog -->
    <ConfirmDialog
      v-if="showDeleteDialog"
      title="Delete Artifact"
      message="Are you sure you want to delete"
      :item-name="deleteTarget"
      :error="deleteError"
      :loading="deleting"
      @confirm="doDelete"
      @cancel="showDeleteDialog = false"
    />

    <!-- Directory bulk delete dialog -->
    <BaseModal
      v-if="showDirDeleteDialog && dirDeleteTarget"
      :persistent="dirDeletePhase !== 'confirm'"
      @close="closeDirDelete"
    >
      <!-- Confirm phase -->
      <template v-if="dirDeletePhase === 'confirm'">
        <h3>Delete Directory</h3>
        <p>Are you sure you want to delete <strong>{{ dirDeleteTarget.name }}</strong> and all of its contents?</p>
        <p class="dir-delete-stats">{{ dirDeleteTarget.artifact_count }} items &mdash; {{ formatSize(dirDeleteTarget.total_bytes || 0) }}</p>
      </template>

      <!-- Progress phase -->
      <template v-else-if="dirDeletePhase === 'progress'">
        <h3>Deleting Directory</h3>
        <p>Deleting <strong>{{ dirDeleteTarget.name }}</strong>...</p>
        <div class="dir-delete-progress">
          <div class="progress-bar">
            <div class="progress-fill" :style="{ width: dirDeleteProgress() + '%' }"></div>
          </div>
          <div class="progress-stats">
            <span v-if="dirDeleteTask">{{ dirDeleteTask.progress.phase || 'Starting...' }}</span>
            <span v-if="dirDeleteTask && dirDeleteTask.progress.total_artifacts > 0">
              &mdash; {{ dirDeleteTask.progress.checked_artifacts }} / {{ dirDeleteTask.progress.total_artifacts }} artifacts
            </span>
          </div>
        </div>
      </template>

      <!-- Done phase -->
      <template v-else>
        <h3>{{ dirDeleteTask?.status === 'completed' ? 'Delete Complete' : dirDeleteTask?.status === 'cancelled' ? 'Delete Cancelled' : 'Delete Failed' }}</h3>
        <template v-if="dirDeleteTask?.status === 'completed' && dirDeleteTask?.result && dirDeleteTask.result.type === 'bulk_delete'">
          <p>Deleted {{ dirDeleteTask.result.deleted_artifacts }} artifacts ({{ formatSize(dirDeleteTask.result.deleted_bytes || 0) }})</p>
        </template>
        <template v-else-if="dirDeleteTask?.status === 'cancelled'">
          <p>The bulk delete was cancelled.</p>
        </template>
        <template v-else>
          <p class="error-text">{{ dirDeleteError || 'An error occurred during deletion.' }}</p>
        </template>
      </template>

      <template #footer>
        <template v-if="dirDeletePhase === 'confirm'">
          <button class="btn" @click="closeDirDelete">Cancel</button>
          <button class="btn btn-danger" @click="doDirDelete">Delete</button>
        </template>
        <template v-else-if="dirDeletePhase === 'progress'">
          <button class="btn btn-danger" @click="cancelDirDelete">Cancel</button>
        </template>
        <template v-else>
          <button class="btn" @click="closeDirDelete">Close</button>
        </template>
      </template>
    </BaseModal>

    <!-- Directory download dialog -->
    <BaseModal
      v-if="showDirDownloadDialog && dirDownloadTarget"
      title="Download Directory"
      :persistent="dirDownloading"
      @close="showDirDownloadDialog = false"
    >
      <p>Download <strong>{{ dirDownloadTarget.name }}</strong> as archive</p>
      <p class="dir-delete-stats">{{ dirDownloadTarget.artifact_count }} items &mdash; {{ formatSize(dirDownloadTarget.total_bytes || 0) }}</p>
      <div class="format-select">
        <label><input type="radio" v-model="dirDownloadFormat" value="tar.gz" /> tar.gz</label>
        <label><input type="radio" v-model="dirDownloadFormat" value="tar" /> tar</label>
        <label><input type="radio" v-model="dirDownloadFormat" value="tar.xz" /> tar.xz</label>
        <label><input type="radio" v-model="dirDownloadFormat" value="zip" /> zip</label>
      </div>
      <p v-if="dirDownloadError" class="error-text">{{ dirDownloadError }}</p>
      <template #footer>
        <button class="btn" @click="showDirDownloadDialog = false" :disabled="dirDownloading">Cancel</button>
        <button class="btn btn-download" :disabled="dirDownloading" @click="doDirDownload">
          {{ dirDownloading ? 'Downloading...' : 'Download' }}
        </button>
      </template>
    </BaseModal>
  </div>
</template>

<style scoped>
.artifact-browser {
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg);
  padding: 1rem;
}
.browser-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 0.75rem;
}
.browser-header h3 {
  margin: 0;
}
.header-actions {
  display: flex;
  align-items: center;
  gap: 0.5rem;
}
.btn-upload {
  background: var(--color-primary);
  color: var(--color-primary-text);
  border-color: var(--color-primary);
}
.btn-upload:hover {
  background: var(--color-primary-hover);
}
.btn-upload:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.upload-error {
  color: var(--color-danger);
  font-size: 0.85rem;
  margin: 0 0 0.5rem 0;
}
.search-box {
  position: relative;
}
.search-box input {
  padding: 0.35rem 2rem 0.35rem 0.6rem;
  border: 1px solid var(--color-border-strong);
  border-radius: 4px;
  font-size: 0.9rem;
  width: 250px;
}
.search-box input:focus {
  outline: none;
  border-color: var(--color-primary);
}
.clear-btn {
  position: absolute;
  right: 0.4rem;
  top: 50%;
  transform: translateY(-50%);
  background: none;
  border: none;
  cursor: pointer;
  font-size: 1.1rem;
  color: var(--color-text-disabled);
  padding: 0 0.2rem;
}
.clear-btn:hover {
  color: var(--color-text-strong);
}
.breadcrumbs {
  padding: 0.25rem 0;
  margin-bottom: 0.5rem;
  font-size: 0.85rem;
}
.crumb {
  color: var(--color-blue);
  cursor: pointer;
}
.crumb:hover {
  text-decoration: underline;
}
.separator {
  color: var(--color-text-disabled);
  margin: 0 0.25rem;
}
/* Override global table: this component needs fixed layout and tighter padding */
table {
  table-layout: fixed;
}
th, td {
  padding: 0.5rem 0.75rem;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
th {
  font-size: 0.85rem;
}
.item-icon {
  margin-right: 0.4rem;
}
.dir-count {
  color: var(--color-text-faint);
  font-size: 0.8rem;
  margin-left: 0.3rem;
}
.action-btn {
  background: none;
  border: none;
  cursor: pointer;
  font-size: 0.9rem;
  padding: 0.2rem 0.4rem;
  color: var(--color-text-muted);
  text-decoration: none;
}
.action-btn:hover {
  color: var(--color-text-strong);
}
.action-delete:hover {
  color: var(--color-danger);
}
.btn-download {
  background: var(--color-primary);
  color: var(--color-primary-text);
  border-color: var(--color-primary);
  text-decoration: none;
  display: inline-block;
}
.btn-download:hover {
  background: var(--color-primary-hover);
}
.btn-download:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.detail-list {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 0.4rem 1rem;
  margin: 1rem 0;
  font-size: 0.9rem;
}
.detail-list dt {
  font-weight: 600;
  color: var(--color-text-secondary);
  white-space: nowrap;
}
.detail-list dd {
  margin: 0;
  word-break: break-all;
}
.detail-list .mono {
  font-family: monospace;
  font-size: 0.85rem;
}
.pagination-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0.75rem 0.75rem 0;
  font-size: 0.85rem;
}
.page-info {
  color: var(--color-text-muted);
}
.page-controls {
  display: flex;
  align-items: center;
  gap: 0.5rem;
}
.btn-page {
  padding: 0.3rem 0.75rem;
  font-size: 0.85rem;
}
.btn-page:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}
.page-number {
  color: var(--color-text-secondary);
  min-width: 8rem;
  text-align: center;
}
.dir-delete-stats {
  color: var(--color-text-muted);
  font-size: 0.9rem;
}
.dir-delete-progress {
  margin: 1rem 0;
}
.progress-bar {
  height: 8px;
  background: var(--color-border);
  border-radius: 4px;
  overflow: hidden;
}
.progress-fill {
  height: 100%;
  background: var(--color-blue);
  border-radius: 4px;
  transition: width 0.3s ease;
}
.progress-stats {
  margin-top: 0.5rem;
  font-size: 0.85rem;
  color: var(--color-text-muted);
}
.error-text {
  color: var(--color-danger);
}
.format-select {
  display: flex;
  gap: 1rem;
  margin: 0.75rem 0;
}
.format-select label {
  display: flex;
  align-items: center;
  gap: 0.3rem;
  cursor: pointer;
  font-size: 0.9rem;
}
</style>
