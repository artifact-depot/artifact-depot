// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onUnmounted, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, isAdmin, type Artifact, type DirInfo, type TaskInfo } from '../api'
import { useSettingsStore } from '../stores/settingsStore'
import BaseModal from './BaseModal.vue'
import ConfirmDialog from './ConfirmDialog.vue'
import ResponsiveTable from './ResponsiveTable.vue'
import { formatSize, formatDate } from '../composables/useFormatters'

const props = defineProps<{ repoName: string; format?: string }>()

const route = useRoute()
const router = useRouter()

const dirs = ref<DirInfo[]>([])
const artifacts = ref<Artifact[]>([])
const loading = ref(false)
const prefix = computed(() => (route.query.path as string) || '')

// Docker repos browse tags-first: bookkeeping dirs (_manifests/_blobs) are
// hidden in the default view, and at an image level the _tags/ contents are
// shown as tag rows. Expert view reveals the raw storage tree instead.
const isDocker = computed(() => props.format === 'docker')
// Expert view (raw storage + delete) is admin-only; gate the toggle on the
// roles embedded in the JWT. Non-admins only ever see the default browse.
const canExpert = computed(() => isDocker.value && isAdmin())
const expert = ref(false)
const BOOKKEEPING = ['_manifests', '_blobs', '_tags']
// Set when the current prefix is a Docker image (its listing contains _tags/);
// holds the tag rows fetched from _tags/.
const atImage = ref(false)
const dockerTagItems = ref<DisplayItem[]>([])
// Docker browse is tags-first in BOTH modes (so Copy pull / Download stay put).
// Expert is additive: it reveals the raw bookkeeping dirs and the Delete action.
const dockerDefault = computed(
  () => isDocker.value && !isSearchMode.value,
)
function toggleExpert() {
  expert.value = !expert.value
  load()
}

// Repo image catalog (one cached call per repo) — lets us label a folder as a
// namespace vs an image cheaply, without probing every row.
const dockerImageSet = ref<Set<string>>(new Set())
let catalogFetchedFor = ''
async function ensureCatalog() {
  if (!isDocker.value || catalogFetchedFor === props.repoName) return
  try {
    dockerImageSet.value = new Set(await api.getDockerCatalog(props.repoName))
    catalogFetchedFor = props.repoName
  } catch {
    /* leave empty — folders just fall back to "namespace" */
  }
}
function dockerType(item: DisplayItem): string {
  if (item.kind === 'tag') return 'tag'
  if (!item.isDir) return item.content_type || 'file'
  if (BOOKKEEPING.includes(item.name)) return 'storage'
  return dockerImageSet.value.has(item.path.replace(/\/+$/, '')) ? 'image' : 'namespace'
}

// `docker pull` is available through the default docker group (host-root
// routing), so a tag's pull command is just <host>/<image>:<tag> — no per-repo
// port. Only offer it when a default group is configured (else it wouldn't
// resolve).
const settingsStore = useSettingsStore()
const hasDefaultDockerGroup = computed(() => !!settingsStore.settings?.default_docker_repo)
const copiedTag = ref('')
function pullCommand(tag: string): string {
  const image = prefix.value.replace(/\/+$/, '')
  return `docker pull ${window.location.host}/${image}:${tag}`
}
async function copyPull(item: DisplayItem, e: Event) {
  e.stopPropagation()
  try {
    await navigator.clipboard.writeText(pullCommand(item.name))
    copiedTag.value = item.name
    setTimeout(() => { if (copiedTag.value === item.name) copiedTag.value = '' }, 1500)
  } catch {
    /* clipboard unavailable (insecure context) */
  }
}

// Download a tag's manifest JSON (a tag has no blob of its own; the manifest is
// the single downloadable document). Fetched with auth, then saved client-side.
async function downloadManifest(item: DisplayItem, e: Event) {
  e.stopPropagation()
  const image = prefix.value.replace(/\/+$/, '')
  try {
    const { data } = await api.getDockerManifest(props.repoName, image, item.name)
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `${item.name}.manifest.json`
    document.body.appendChild(a)
    a.click()
    a.remove()
    URL.revokeObjectURL(url)
  } catch {
    /* manifest unavailable (e.g. retention-deleted tag) */
  }
}

// Delete visibility: admin-only everywhere; Docker tags additionally require
// Expert; Docker namespace/image folders in the default view have no delete.
function canDelete(item: DisplayItem): boolean {
  if (!isAdmin()) return false
  if (item.kind === 'tag') return expert.value
  if (item.isDir) return !isDocker.value || expert.value
  return true
}
function deleteItem(item: DisplayItem, e: Event) {
  if (item.isDir) confirmDirDelete(item, e)
  else confirmDelete(item.path, e)
}

// --- Docker tag detail (manifest) ---
// Fetched on drill-in via the /v2 endpoint (one request); layer/platform sizes
// come straight from the manifest JSON, so there are no extra blob reads.
const showManifestModal = ref(false)
const manifestLoading = ref(false)
const manifestError = ref('')
const manifestTag = ref('')
const manifestDigest = ref('')
const manifestMediaType = ref('')
const manifestIsList = ref(false)
const manifestPlatforms = ref<{ platform: string; digest: string; size: number }[]>([])
const manifestConfig = ref<{ digest: string; size: number } | null>(null)
const manifestLayers = ref<{ digest: string; size: number }[]>([])
const manifestTotal = ref(0)
// Tag timestamps carried from the browse row (no extra fetch — already loaded).
const manifestUpdated = ref('')
const manifestAccessed = ref('')

async function openTagDetail(item: DisplayItem) {
  const image = prefix.value.replace(/\/+$/, '')
  manifestTag.value = item.name
  manifestUpdated.value = item.updated_at || ''
  manifestAccessed.value = item.last_accessed_at || ''
  manifestError.value = ''
  manifestDigest.value = ''
  manifestMediaType.value = ''
  manifestIsList.value = false
  manifestPlatforms.value = []
  manifestConfig.value = null
  manifestLayers.value = []
  manifestTotal.value = 0
  showManifestModal.value = true
  manifestLoading.value = true
  try {
    const { data, digest } = await api.getDockerManifest(props.repoName, image, item.name)
    manifestDigest.value = digest
    manifestMediaType.value = data.mediaType || ''
    if (Array.isArray(data.manifests)) {
      manifestIsList.value = true
      manifestPlatforms.value = data.manifests.map((m: any) => ({
        platform: m.platform
          ? `${m.platform.os}/${m.platform.architecture}${m.platform.variant ? '/' + m.platform.variant : ''}`
          : '(unknown)',
        digest: m.digest,
        size: m.size || 0,
      }))
    } else {
      manifestConfig.value = data.config ? { digest: data.config.digest, size: data.config.size || 0 } : null
      manifestLayers.value = (data.layers || []).map((l: any) => ({ digest: l.digest, size: l.size || 0 }))
      manifestTotal.value = (manifestConfig.value?.size || 0) +
        manifestLayers.value.reduce((s, l) => s + l.size, 0)
    }
  } catch (e: any) {
    manifestError.value = e.message || 'Failed to load manifest'
  } finally {
    manifestLoading.value = false
  }
}

// --- Lazy tag-size fill for the browse list ---
// A tag is a zero-byte pointer; its real image size lives in the manifest
// (config + layers). Resolving it is one cheap manifest read, but doing that
// for every tag up front fans out to hundreds of requests on images with many
// tags. So each tag row resolves its size only when it scrolls into view, with
// a small concurrency cap — the browse list never blocks.
type TagSize = number | 'err'
const tagSizes = ref<Record<string, TagSize>>({})
const tagSizePending = new Set<string>()
const tagSizeQueue: string[] = []
let tagSizeInFlight = 0
const TAG_SIZE_MAX_CONCURRENT = 6

function resetTagSizes() {
  tagSizes.value = {}
  tagSizePending.clear()
  tagSizeQueue.length = 0
  tagSizeInFlight = 0
}

function tagSizeLabel(tag: string): string | null {
  const v = tagSizes.value[tag]
  return typeof v === 'number' ? formatSize(v) : null
}

function enqueueTagSize(tag: string) {
  if (tag in tagSizes.value || tagSizePending.has(tag) || tagSizeQueue.includes(tag)) return
  tagSizeQueue.push(tag)
  pumpTagSizeQueue()
}

function pumpTagSizeQueue() {
  while (tagSizeInFlight < TAG_SIZE_MAX_CONCURRENT && tagSizeQueue.length) {
    const tag = tagSizeQueue.shift() as string
    tagSizeInFlight++
    tagSizePending.add(tag)
    computeTagSize(tag).finally(() => {
      tagSizeInFlight--
      tagSizePending.delete(tag)
      pumpTagSizeQueue()
    })
  }
}

function sumManifestBytes(m: any): number {
  return (m?.config?.size || 0) + (m?.layers || []).reduce((s: number, l: any) => s + (l.size || 0), 0)
}

async function computeTagSize(tag: string) {
  const image = prefix.value.replace(/\/+$/, '')
  try {
    const { data } = await api.getDockerManifest(props.repoName, image, tag)
    let total = 0
    if (Array.isArray(data.manifests)) {
      // Multi-arch: sum config+layers across each per-arch child manifest.
      const children = await Promise.all(
        data.manifests.map((m: any) =>
          api.getDockerManifest(props.repoName, image, m.digest).then(r => r.data).catch(() => null)),
      )
      total = children.reduce((s: number, c: any) => s + sumManifestBytes(c), 0)
    } else {
      total = sumManifestBytes(data)
    }
    tagSizes.value = { ...tagSizes.value, [tag]: total }
  } catch {
    tagSizes.value = { ...tagSizes.value, [tag]: 'err' }
  }
}

// One shared observer; tag rows register via v-tagsize and resolve their size
// the first time they enter the viewport.
const tagSizeObserver =
  typeof IntersectionObserver !== 'undefined'
    ? new IntersectionObserver(
        (entries) => {
          for (const e of entries) {
            if (e.isIntersecting) {
              const tag = (e.target as HTMLElement).dataset.tagsize
              if (tag) enqueueTagSize(tag)
            }
          }
        },
        { rootMargin: '150px' },
      )
    : null
onUnmounted(() => tagSizeObserver?.disconnect())

const vTagsize = {
  mounted(el: HTMLElement, binding: { value: string }) {
    el.dataset.tagsize = binding.value
    tagSizeObserver?.observe(el)
  },
  unmounted(el: HTMLElement) {
    tagSizeObserver?.unobserve(el)
  },
}

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
const pageSizeOptions = [100, 500, 1000]
const pageInputValue = ref('1')

interface DisplayItem {
  name: string
  path: string
  isDir: boolean
  size: number
  content_type: string
  updated_at: string
  last_accessed_at?: string
  created_at?: string
  artifact_count?: number
  total_bytes?: number
  kind?: 'tag'
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
      last_accessed_at: a.last_accessed_at,
      created_at: a.created_at,
    }))
  }

  // Docker browse: tags-first at an image; namespaces/images otherwise.
  // Expert is additive — it reveals the raw bookkeeping dirs (_manifests/_blobs)
  // and lets you drill into them; the friendly tag list (with Copy pull /
  // Download) stays visible in both modes.
  if (dockerDefault.value) {
    const toDir = (d: DirInfo): DisplayItem => ({
      name: d.name,
      path: prefix.value + d.name + '/',
      isDir: true,
      size: d.total_bytes,
      content_type: '',
      updated_at: d.last_modified_at,
      last_accessed_at: d.last_accessed_at,
      artifact_count: d.artifact_count,
      total_bytes: d.total_bytes,
    })
    // Expert drill-in to a bookkeeping dir → show the raw records.
    const inBookkeeping = BOOKKEEPING.some(b => prefix.value.includes(b + '/'))
    if (expert.value && inBookkeeping) {
      const dItems = dirs.value.map(toDir)
      const fItems: DisplayItem[] = artifacts.value.map(a => ({
        name: a.path,
        path: prefix.value + a.path,
        isDir: false,
        size: a.size,
        content_type: a.content_type,
        updated_at: a.updated_at,
        last_accessed_at: a.last_accessed_at,
        created_at: a.created_at,
      }))
      return [...dItems, ...fItems]
    }
    if (atImage.value) {
      if (!expert.value) return dockerTagItems.value
      // Expert at an image: tags (with actions) + the raw storage dirs.
      const storage = dirs.value
        .filter(d => d.name === '_manifests' || d.name === '_blobs')
        .map(toDir)
      return [...dockerTagItems.value, ...storage]
    }
    return dirs.value
      .filter(d => expert.value || !BOOKKEEPING.includes(d.name))
      .map(toDir)
  }

  const dirItems: DisplayItem[] = dirs.value.map(d => ({
    name: d.name,
    path: prefix.value + d.name + '/',
    isDir: true,
    size: d.total_bytes,
    content_type: '',
    updated_at: d.last_modified_at,
    last_accessed_at: d.last_accessed_at,
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
    last_accessed_at: a.last_accessed_at,
    created_at: a.created_at,
  }))

  return [...dirItems, ...fileItems]
})

// Whether any visible row renders an action (Copy pull / Download / Delete).
// When nothing in view has one (e.g. a page of Docker namespaces/images with no
// delete rights), the actions column is dropped so it doesn't waste width.
const showActionsColumn = computed(() =>
  displayItems.value.some(item => {
    const copyPull = item.kind === 'tag' && hasDefaultDockerGroup.value
    // Docker image folders aren't downloadable (you pull images, not tar the
    // raw storage) — only tags are. Non-docker folders download normally.
    const download =
      item.kind === 'tag' || (item.isDir && !isDocker.value) || !item.isDir
    return copyPull || download || canDelete(item)
  }),
)

function downloadUrl(path: string): string {
  return api.downloadUrl(props.repoName, path)
}

function onRowClick(item: DisplayItem) {
  if (item.kind === 'tag') {
    openTagDetail(item)
    return
  }
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
    await ensureCatalog()
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

    // Docker default view: if this level is an image (has a _tags/ dir), pull
    // its tags so they can be shown in place of the bookkeeping dirs.
    atImage.value = false
    dockerTagItems.value = []
    resetTagSizes()
    if (dockerDefault.value && dirs.value.some(d => d.name === '_tags')) {
      atImage.value = true
      const tagsPrefix = prefix.value + '_tags/'
      const t = await api.listArtifacts(props.repoName, { prefix: tagsPrefix, limit: 1000 })
      dockerTagItems.value = t.artifacts.map(a => ({
        name: a.path,
        path: tagsPrefix + a.path,
        isDir: false,
        size: a.size,
        content_type: 'docker tag',
        updated_at: a.updated_at,
        last_accessed_at: a.last_accessed_at,
        created_at: a.created_at,
        kind: 'tag' as const,
      }))
    }
  } catch {
    dirs.value = []
    artifacts.value = []
    totalArtifacts.value = 0
    atImage.value = false
    dockerTagItems.value = []
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
const showPagination = computed(() =>
  !isSearchMode.value && totalArtifacts.value > pageSizeOptions[0]
)

function goToPage(n: number) {
  const clamped = Math.min(Math.max(1, n), totalPages.value)
  pageOffset.value = (clamped - 1) * pageLimit.value
  load()
}

function prevPage() {
  goToPage(currentPage.value - 1)
}

function nextPage() {
  goToPage(currentPage.value + 1)
}

function onPageSizeChange() {
  pageOffset.value = 0
  load()
}

watch(currentPage, (p) => { pageInputValue.value = String(p) }, { immediate: true })

function commitPageInput() {
  const n = parseInt(pageInputValue.value, 10)
  if (Number.isFinite(n) && n >= 1 && n <= totalPages.value && n !== currentPage.value) {
    goToPage(n)
  } else {
    pageInputValue.value = String(currentPage.value)
  }
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
        <button
          v-if="canExpert"
          class="btn btn-expert"
          :class="{ 'btn-expert-on': expert }"
          :title="expert ? 'Showing raw storage (manifests, blobs)' : 'Show raw storage: manifests, blobs, and delete'"
          @click="toggleExpert"
        >
          Expert: {{ expert ? 'on' : 'off' }}
        </button>
        <button v-if="!isDocker" class="btn btn-upload" :disabled="uploading" @click="triggerUpload">
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
          <!-- Name is a FIXED width (fits a long Supermicro download name;
               longer ellipsizes) so every folder lays out identically. Size,
               Type and the actions column shrink to their content (width:1% +
               nowrap), and the two un-sized date columns float — absorbing the
               leftover width and filling the row (right-aligned, see CSS). The
               actions column holds all actions as a flex row: hidden ones
               collapse to nothing, shown ones expand, so it's narrow when Expert
               is off and widens only for the actions Expert reveals. -->
          <col style="width: 36rem;" />
          <col style="width: 1%;" />
          <col style="width: 1%;" />
          <col />
          <col />
          <col v-if="showActionsColumn" style="width: 1%;" />
        </colgroup>
        <thead>
          <tr>
            <th>Name</th>
            <th>Size</th>
            <th>{{ isDocker ? 'Type' : 'Content Type' }}</th>
            <th>Created/Modified</th>
            <th>Last Accessed</th>
            <th v-if="showActionsColumn"></th>
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
                <span v-if="isDocker" class="size-dash" title="Docker layers are content-addressed and shared across images at the repo level, so they aren't counted per image — open an image to see its size">&mdash;</span>
                <template v-else>
                  {{ formatSize(item.total_bytes || 0) }}
                  <span class="dir-count">({{ item.artifact_count }} items)</span>
                </template>
              </template>
              <template v-else-if="item.kind === 'tag'">
                <span v-if="tagSizeLabel(item.name)">{{ tagSizeLabel(item.name) }}</span>
                <span v-else v-tagsize="item.name" class="size-dash"
                      :title="tagSizes[item.name] === 'err' ? 'Could not read this tag\'s manifest' : 'Resolving image size…'">&mdash;</span>
              </template>
              <template v-else>{{ formatSize(item.size) }}</template>
            </td>
            <td>
              <span v-if="isDocker" class="type-label">{{ dockerType(item) }}</span>
              <template v-else>{{ item.isDir ? 'Folder' : item.content_type }}</template>
            </td>
            <td>{{ formatDate(item.updated_at) }}</td>
            <td>{{ item.last_accessed_at ? formatDate(item.last_accessed_at) : '—' }}</td>
            <td v-if="showActionsColumn">
              <!-- Flex row: only the buttons that apply are rendered, and each
                   takes just its own width — an absent Copy pull / Download /
                   Delete leaves no reserved gap (so a Delete-only row sits tight
                   against the date, not pushed right by phantom slots). -->
              <div class="row-actions">
                <button
                  v-if="isDocker && item.kind === 'tag' && hasDefaultDockerGroup"
                  class="act-link"
                  :title="pullCommand(item.name)"
                  @click="copyPull(item, $event)"
                >{{ copiedTag === item.name ? 'Copied ✓' : 'Copy pull' }}</button>
                <button v-if="item.kind === 'tag'" class="act-link" title="Download manifest" @click="downloadManifest(item, $event)">Download</button>
                <button v-else-if="item.isDir && !isDocker" class="act-link" title="Download directory" @click="confirmDirDownload(item, $event)">Download</button>
                <a v-else-if="!item.isDir" :href="downloadUrl(item.path)" target="_blank" class="act-link" title="Download" @click.stop>Download</a>
                <button v-if="canDelete(item)" class="act-link act-delete" :title="item.isDir ? 'Delete directory' : 'Delete'" @click="deleteItem(item, $event)">Delete</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </ResponsiveTable>

    <div v-if="!loading && showPagination" class="pagination-bar">
      <span class="page-info">
        Showing {{ showingFrom }}&ndash;{{ showingTo }} of {{ totalArtifacts }} artifacts
      </span>
      <div class="page-controls">
        <button class="btn btn-page" :disabled="!hasPrev" @click="prevPage">&larr; Prev</button>
        <span class="page-number">
          Page
          <input
            type="text"
            inputmode="numeric"
            class="page-input-inline"
            v-model="pageInputValue"
            @keydown.enter.prevent="commitPageInput"
            @blur="commitPageInput"
            @focus="($event.target as HTMLInputElement).select()"
          />
          of {{ totalPages }}
        </span>
        <button class="btn btn-page" :disabled="!hasNext" @click="nextPage">Next &rarr;</button>
        <label class="page-size">
          Per page
          <select v-model.number="pageLimit" @change="onPageSizeChange">
            <option v-for="n in pageSizeOptions" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
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

    <!-- Docker tag detail (manifest) -->
    <BaseModal v-if="showManifestModal" max-width="640px" :show-close="true" @close="showManifestModal = false">
      <h3 class="mono">{{ manifestTag }}</h3>
      <p v-if="manifestLoading"><span class="loading-spinner"></span> Loading manifest...</p>
      <p v-else-if="manifestError" class="error-text">{{ manifestError }}</p>
      <template v-else>
        <dl class="detail-list">
          <dt>Digest</dt>
          <dd class="mono">{{ manifestDigest || 'N/A' }}</dd>
          <dt>Type</dt>
          <dd>{{ manifestIsList ? 'Manifest list (multi-arch)' : 'Image manifest' }}</dd>
          <dt v-if="!manifestIsList">Total size</dt>
          <dd v-if="!manifestIsList">{{ formatSize(manifestTotal) }}</dd>
          <dt v-if="manifestMediaType">Media type</dt>
          <dd v-if="manifestMediaType" class="mono">{{ manifestMediaType }}</dd>
          <dt>Created/Modified</dt>
          <dd>{{ manifestUpdated ? formatDate(manifestUpdated) : '—' }}</dd>
          <dt>Last Accessed</dt>
          <dd>{{ manifestAccessed ? formatDate(manifestAccessed) : '—' }}</dd>
        </dl>

        <!-- Multi-arch: per-platform child manifests -->
        <template v-if="manifestIsList">
          <h4 class="manifest-section">Platforms ({{ manifestPlatforms.length }})</h4>
          <table class="manifest-table">
            <tbody>
              <tr v-for="p in manifestPlatforms" :key="p.digest">
                <td class="plat">{{ p.platform }}</td>
                <td class="mono digest-cell">{{ p.digest }}</td>
                <td class="size-cell">{{ formatSize(p.size) }}</td>
              </tr>
            </tbody>
          </table>
        </template>

        <!-- Single image: config + layers -->
        <template v-else>
          <h4 class="manifest-section">Config + {{ manifestLayers.length }} layer(s)</h4>
          <table class="manifest-table">
            <tbody>
              <tr v-if="manifestConfig">
                <td class="mono digest-cell">{{ manifestConfig.digest }}</td>
                <td class="size-cell">config</td>
              </tr>
              <tr v-for="(l, i) in manifestLayers" :key="i">
                <td class="mono digest-cell">{{ l.digest }}</td>
                <td class="size-cell">{{ formatSize(l.size) }}</td>
              </tr>
            </tbody>
          </table>
        </template>
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
.btn-expert {
  border-color: var(--color-border-strong);
  color: var(--color-text-secondary);
}
.btn-expert-on {
  background: var(--color-primary);
  color: var(--color-primary-text);
  border-color: var(--color-primary);
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
/* Auto layout so each column sizes to its content (see the colgroup): Name,
   Size, Type and the actions column shrink to fit, and the two date columns
   absorb the leftover width. When Expert turns on, the actions column is just
   the (content-sized) Delete link added on the right — the rest is unchanged. */
table {
  table-layout: auto;
  width: 100%;
}
.type-label {
  text-transform: capitalize;
  color: var(--color-text-secondary);
}
.act-link {
  background: none;
  border: none;
  cursor: pointer;
  font: inherit;
  font-size: 0.82rem;
  padding: 0.15rem 0.4rem;
  margin-right: 0.25rem;
  color: var(--color-blue);
  text-decoration: none;
}
.act-link:hover {
  text-decoration: underline;
}
.act-delete {
  color: var(--color-danger);
}
/* Pack the actions with flex so only rendered buttons take space — an absent
   Copy pull / Download / Delete collapses instead of leaving a reserved gap.
   (Trades cross-row column alignment for no phantom whitespace.) */
.row-actions {
  display: flex;
  align-items: center;
  gap: 0.75rem;
}
.row-actions .act-link {
  margin: 0;
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
/* Name is a fixed 36rem (see colgroup); this keeps a longer name clipped to
   that width with an ellipsis rather than stretching the column. */
td:first-child, th:first-child {
  max-width: 36rem;
}
/* Right-align the two date columns (Created/Modified, Last Accessed) so, as
   they absorb the leftover width, each value stays against the next column
   rather than leaving a gap before it. */
th:nth-child(4), td:nth-child(4),
th:nth-child(5), td:nth-child(5) {
  text-align: right;
}
.item-icon {
  margin-right: 0.4rem;
}
.dir-count {
  color: var(--color-text-faint);
  font-size: 0.8rem;
  margin-left: 0.3rem;
}
.size-dash {
  color: var(--color-text-faint);
  cursor: help;
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
.pull-btn {
  border: 1px solid var(--color-border-strong);
  border-radius: 4px;
  font-size: 0.8rem;
  color: var(--color-text-secondary);
  white-space: nowrap;
}
.pull-btn:hover {
  border-color: var(--color-primary);
  color: var(--color-primary);
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
.manifest-section {
  margin: 1rem 0 0.4rem;
  font-size: 0.85rem;
  color: var(--color-text-secondary);
}
.manifest-table {
  width: 100%;
  table-layout: auto;
  border-collapse: collapse;
  font-size: 0.82rem;
}
.manifest-table td {
  padding: 0.35rem 0.5rem;
  border-bottom: 1px solid var(--color-border);
  white-space: nowrap;
}
.manifest-table .plat {
  font-weight: 600;
}
.manifest-table .digest-cell {
  width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 0;
}
.manifest-table .size-cell {
  text-align: right;
  color: var(--color-text-muted);
  font-variant-numeric: tabular-nums;
}
.pagination-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0.75rem 0.75rem 0;
  font-size: 0.85rem;
  flex-wrap: wrap;
  gap: 0.5rem;
}
.page-info {
  color: var(--color-text-muted);
}
.page-controls {
  display: flex;
  align-items: center;
  gap: 0.35rem;
  flex-wrap: wrap;
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
  text-align: center;
  display: inline-flex;
  align-items: baseline;
  gap: 0.35rem;
}
.page-input-inline {
  width: 3em;
  padding: 0.2rem 0.3rem;
  font-size: 0.85rem;
  text-align: center;
  background: var(--color-input-bg, transparent);
  border: 1px solid var(--color-border);
  border-radius: 3px;
  color: inherit;
  font-family: inherit;
}
.page-input-inline:focus {
  outline: none;
  border-color: var(--color-blue);
}
.page-size {
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  margin-left: 0.5rem;
  color: var(--color-text-muted);
}
.page-size select {
  padding: 0.25rem 0.4rem;
  font-size: 0.85rem;
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
