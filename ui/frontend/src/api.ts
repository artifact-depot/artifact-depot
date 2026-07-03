// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

export interface Repo {
  name: string
  repo_type: 'hosted' | 'cache' | 'proxy'
  format: 'raw' | 'docker' | 'cargo' | 'pypi' | 'apt' | 'golang' | 'helm' | 'yum' | 'npm'
  store: string
  upstream_url?: string
  cache_ttl_secs?: number
  members?: string[]
  write_member?: string
  listen?: string
  cleanup_max_age_days?: number
  cleanup_max_unaccessed_days?: number
  cleanup_untagged_manifests?: boolean
  apt_components?: string[]
  upstream_auth?: { username: string; password?: string }
  content_disposition?: 'attachment' | 'inline'
  repodata_depth?: number
  created_at: string
  artifact_count: number
  total_bytes: number
}

export interface CreateRepoRequest {
  name: string
  repo_type: 'hosted' | 'cache' | 'proxy'
  format: 'raw' | 'docker' | 'cargo' | 'pypi' | 'apt' | 'golang' | 'helm' | 'yum' | 'npm'
  store?: string
  upstream_url?: string
  cache_ttl_secs?: number
  members?: string[]
  write_member?: string
  listen?: string
  cleanup_max_age_days?: number
  cleanup_max_unaccessed_days?: number
  cleanup_untagged_manifests?: boolean
  apt_components?: string[]
  upstream_auth?: { username: string; password?: string }
  content_disposition?: 'attachment' | 'inline'
  repodata_depth?: number
}

export interface CloneRepoRequest {
  new_name: string
}

export interface UpdateRepoRequest {
  upstream_url?: string
  cache_ttl_secs?: number
  upstream_auth?: { username: string; password?: string } | null
  members?: string[]
  write_member?: string | null
  listen?: string | null
  cleanup_untagged_manifests?: boolean | null
  apt_components?: string[] | null
  cleanup_max_age_days?: number | null
  cleanup_max_unaccessed_days?: number | null
  content_disposition?: 'attachment' | 'inline' | null
  repodata_depth?: number | null
}

export interface Store {
  name: string
  store_type: 'file' | 's3'
  root?: string
  sync?: boolean
  io_size?: number
  direct_io?: boolean
  bucket?: string
  endpoint?: string
  region?: string
  prefix?: string
  access_key?: string
  max_retries?: number
  connect_timeout_secs?: number
  read_timeout_secs?: number
  retry_mode?: string
  created_at: string
  blob_count?: number
  total_bytes?: number
}

export interface CreateStoreRequest {
  name: string
  store_type: 'file' | 's3'
  root?: string
  sync?: boolean
  io_size?: number
  direct_io?: boolean
  bucket?: string
  endpoint?: string
  region?: string
  prefix?: string
  access_key?: string
  secret_key?: string
  max_retries?: number
  connect_timeout_secs?: number
  read_timeout_secs?: number
  retry_mode?: string
}

export interface UpdateStoreRequest {
  root?: string
  sync?: boolean
  io_size?: number
  direct_io?: boolean
  bucket?: string
  endpoint?: string
  region?: string
  prefix?: string
  access_key?: string
  secret_key?: string
  max_retries?: number
  connect_timeout_secs?: number
  read_timeout_secs?: number
  retry_mode?: string
}

export interface BrowseEntry {
  name: string
  is_dir: boolean
  size?: number
}

export interface BrowseResponse {
  path: string
  entries: BrowseEntry[]
}

export interface StoreCheckResult {
  ok: boolean
  error?: string
}

export interface Artifact {
  id: string
  path: string
  size: number
  content_type: string
  blob_id: string
  content_hash: string | null
  etag: string | null
  created_at: string
  updated_at: string
  last_accessed_at: string
}

export interface DirInfo {
  name: string
  artifact_count: number
  total_bytes: number
  last_modified_at: string
  last_accessed_at: string
}

export interface BrowseResponse {
  dirs: DirInfo[]
  artifacts: Artifact[]
  total?: number
  offset?: number
  limit?: number
}

export interface HealthStatus {
  status: string
  version: string
}

export interface LoginResponse {
  token: string
  must_change_password: boolean
}

export interface User {
  username: string
  roles: string[]
  must_change_password: boolean
  created_at: string
  updated_at: string
}

export interface CreateUserRequest {
  username: string
  password: string
  roles: string[]
  must_change_password?: boolean
}

export interface UpdateUserRequest {
  password?: string
  roles?: string[]
  must_change_password?: boolean
}

export interface CapabilityGrant {
  capability: 'read' | 'write' | 'delete'
  repo: string
}

export interface Role {
  name: string
  description: string
  capabilities: CapabilityGrant[]
  created_at: string
  updated_at: string
}

export interface CreateRoleRequest {
  name: string
  description: string
  capabilities: CapabilityGrant[]
}

export interface UpdateRoleRequest {
  description?: string
  capabilities?: CapabilityGrant[]
}

// --- Settings types ---

export interface CorsConfig {
  allowed_origins: string[]
  allowed_methods: string[]
  allowed_headers: string[]
  max_age_secs: number
}

export interface RateLimitConfig {
  requests_per_second: number
  burst_size: number
}

export interface LoggingConfig {
  otlp_endpoint?: string | null
}

export interface Settings {
  access_log: boolean
  max_artifact_bytes: number
  default_docker_repo: string | null
  cors: CorsConfig | null
  rate_limit: RateLimitConfig | null
  gc_interval_secs: number | null
  gc_min_interval_secs: number | null
  jwt_expiry_secs: number
  jwt_rotation_interval_secs: number
  logging?: LoggingConfig | null
  tracing_endpoint?: string | null
}

export interface SchedulerStatus {
  gc_last_started_at: string | null
  gc_interval_secs: number
  gc_min_interval_secs: number
}

// --- Task types ---

export interface TaskProgress {
  total_repos: number
  completed_repos: number
  total_blobs: number
  checked_blobs: number
  failed_blobs: number
  bytes_read: number
  total_bytes: number
  total_artifacts: number
  checked_artifacts: number
  phase?: string
}

export interface CheckFinding {
  repo: string | null
  path: string
  expected_hash: string
  actual_hash: string | null
  error: string | null
  fixed?: boolean
}

export interface CheckResult {
  total_blobs: number
  passed: number
  mismatched: number
  missing: number
  errors: number
  findings: CheckFinding[]
  bytes_read: number
}

export interface CheckTaskResult {
  type: 'check'
  total_blobs: number
  passed: number
  mismatched: number
  missing: number
  errors: number
  findings: CheckFinding[]
  bytes_read: number
  dry_run: boolean
  fixed: number
}

export interface GcTaskResult {
  type: 'blob_gc'
  scanned_artifacts: number
  expired_artifacts: number
  docker_deleted: number
  scanned_blobs: number
  deleted_dedup_refs: number
  orphaned_blobs_deleted: number
  orphaned_blob_bytes_deleted: number
  stale_temps_deleted: number
  elapsed_ns: number
  dry_run: boolean
}

export interface RepoCleanTaskResult {
  type: 'repo_clean'
  repo: string
  scanned_artifacts: number
  expired_artifacts: number
  docker_deleted: number
  elapsed_ns: number
}

export interface RebuildDirEntriesTaskResult {
  type: 'rebuild_dir_entries'
  repos: number
  entries_updated: number
  elapsed_ns: number
}

export interface BulkDeleteTaskResult {
  type: 'bulk_delete'
  repo: string
  prefix: string
  deleted_artifacts: number
  deleted_bytes: number
  elapsed_ns: number
}

export interface RepoCloneTaskResult {
  type: 'repo_clone'
  source: string
  target: string
  copied_artifacts: number
  copied_entries: number
  elapsed_ns: number
}

export type TaskResult = CheckTaskResult | GcTaskResult | RepoCleanTaskResult | RebuildDirEntriesTaskResult | BulkDeleteTaskResult | RepoCloneTaskResult

export interface TaskLogEntry {
  timestamp: string
  message: string
}

export interface TaskInfo {
  id: string
  kind: { type: string; repo?: string; prefix?: string; source?: string; target?: string }
  status: 'pending' | 'running' | 'completed' | 'failed' | 'cancelled'
  created_at: string
  started_at: string | null
  completed_at: string | null
  progress: TaskProgress
  result: TaskResult | null
  error: string | null
  summary: string
  log: TaskLogEntry[]
}

// --- Backup & Restore types ---

export interface RestoreSummary {
  stores: number
  repositories: number
  roles: number
  users: number
  settings: boolean
  ldap_config?: boolean
}

import { ref } from 'vue'

const TOKEN_KEY = 'depot_token'
export const tokenRef = ref(localStorage.getItem(TOKEN_KEY))

export function getToken(): string | null {
  return tokenRef.value
}

export function setToken(token: string) {
  localStorage.setItem(TOKEN_KEY, token)
  tokenRef.value = token
}

export function clearToken() {
  localStorage.removeItem(TOKEN_KEY)
  tokenRef.value = null
}

/** Role names embedded in the current JWT (added to the token at login so the
 *  UI can gate admin-only affordances without a lookup). Reads `tokenRef`, so
 *  it stays reactive inside computeds. Authorization is always re-checked
 *  server-side; this only controls what the UI offers. */
export function currentRoles(): string[] {
  const t = tokenRef.value
  if (!t) return []
  try {
    const payload = t.split('.')[1]
    const json = JSON.parse(atob(payload.replace(/-/g, '+').replace(/_/g, '/')))
    return Array.isArray(json.roles) ? json.roles : []
  } catch {
    return []
  }
}

export function isAdmin(): boolean {
  return currentRoles().includes('admin')
}

function authHeaders(): Record<string, string> {
  const h: Record<string, string> = { 'X-Requested-With': 'XMLHttpRequest' }
  const token = getToken()
  if (token) {
    h['Authorization'] = `Bearer ${token}`
  }
  return h
}

async function handleResponse<T>(resp: Response): Promise<T> {
  if (resp.status === 401 || resp.status === 403) {
    clearToken()
    window.location.href = '/login'
    throw new Error(resp.status === 401 ? 'Unauthorized' : 'Forbidden')
  }
  if (!resp.ok) {
    const text = await resp.text()
    throw new Error(text || `HTTP ${resp.status}`)
  }
  return resp.json()
}

async function handleVoidResponse(resp: Response): Promise<void> {
  if (resp.status === 401 || resp.status === 403) {
    clearToken()
    window.location.href = '/login'
    throw new Error(resp.status === 401 ? 'Unauthorized' : 'Forbidden')
  }
  if (!resp.ok) {
    const text = await resp.text()
    throw new Error(text || `HTTP ${resp.status}`)
  }
}

export const api = {
  async login(username: string, password: string): Promise<LoginResponse> {
    const resp = await fetch('/api/v1/auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Requested-With': 'XMLHttpRequest' },
      body: JSON.stringify({ username, password }),
    })
    if (!resp.ok) {
      throw new Error('Invalid username or password')
    }
    return resp.json()
  },

  async health(): Promise<HealthStatus> {
    const resp = await fetch('/api/v1/health')
    return handleResponse(resp)
  },

  async listRepos(): Promise<Repo[]> {
    const resp = await fetch('/api/v1/repositories', {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async getRepo(name: string): Promise<Repo> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(name)}`, {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async createRepo(req: CreateRepoRequest): Promise<Repo> {
    const resp = await fetch('/api/v1/repositories', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async deleteRepo(name: string): Promise<void> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(name)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  async updateRepo(name: string, req: UpdateRepoRequest): Promise<Repo> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(name)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async cloneRepo(name: string, req: CloneRepoRequest): Promise<TaskInfo> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(name)}/clone`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async listArtifacts(repo: string, params?: { prefix?: string; q?: string; limit?: number; offset?: number }): Promise<BrowseResponse> {
    const query = new URLSearchParams()
    if (params?.prefix) query.set('prefix', params.prefix)
    if (params?.q) query.set('q', params.q)
    if (params?.limit !== undefined) query.set('limit', String(params.limit))
    if (params?.offset !== undefined) query.set('offset', String(params.offset))
    const qs = query.toString()
    const resp = await fetch(
      `/api/v1/repositories/${encodeURIComponent(repo)}/artifacts${qs ? '?' + qs : ''}`,
      { headers: authHeaders() },
    )
    return handleResponse(resp)
  },

  /** Fetch a Docker manifest by tag or digest, for the tag detail panel.
   *  Uses the repo-scoped /v2 endpoint with the SPA's Bearer token (accepted
   *  like any docker client). `image` is a path (keep its slashes). Returns the
   *  parsed manifest plus the resolved manifest digest from the response. */
  async getDockerManifest(
    repo: string,
    image: string,
    reference: string,
  ): Promise<{ data: any; digest: string }> {
    const resp = await fetch(
      `/repository/${encodeURIComponent(repo)}/v2/${image}/manifests/${encodeURIComponent(reference)}`,
      {
        headers: {
          ...authHeaders(),
          Accept: [
            'application/vnd.docker.distribution.manifest.v2+json',
            'application/vnd.docker.distribution.manifest.list.v2+json',
            'application/vnd.oci.image.manifest.v1+json',
            'application/vnd.oci.image.index.v1+json',
          ].join(', '),
        },
      },
    )
    if (!resp.ok) throw new Error(`manifest fetch failed (${resp.status})`)
    const digest = resp.headers.get('Docker-Content-Digest') || ''
    const data = await resp.json()
    return { data, digest }
  },

  /** All Docker image names in a repo (one cheap call), so the browser can
   *  label a folder as a namespace vs an image without probing each row. */
  async getDockerCatalog(repo: string): Promise<string[]> {
    const resp = await fetch(`/repository/${encodeURIComponent(repo)}/v2/_catalog`, {
      headers: authHeaders(),
    })
    if (!resp.ok) return []
    const data = await resp.json()
    return Array.isArray(data?.repositories) ? data.repositories : []
  },

  async deleteArtifact(repo: string, path: string): Promise<void> {
    const resp = await fetch(`/repository/${encodeURIComponent(repo)}/${path}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  async startBulkDelete(repo: string, prefix: string): Promise<TaskInfo> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(repo)}/bulk-delete`, {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ prefix }),
    })
    return handleResponse(resp)
  },

  async uploadArtifact(repo: string, path: string, file: File): Promise<Artifact> {
    const resp = await fetch(`/repository/${encodeURIComponent(repo)}/${path}`, {
      method: 'PUT',
      headers: {
        'Content-Type': file.type || 'application/octet-stream',
        ...authHeaders(),
      },
      body: file,
    })
    return handleResponse(resp)
  },

  downloadUrl(repo: string, path: string): string {
    return `/repository/${encodeURIComponent(repo)}/${path}`
  },

  async downloadArchive(repo: string, prefix: string, format: string): Promise<void> {
    const params = new URLSearchParams({ prefix, format })
    const resp = await fetch(
      `/api/v1/repositories/${encodeURIComponent(repo)}/download-archive?${params}`,
      { headers: authHeaders() },
    )
    if (resp.status === 401 || resp.status === 403) {
      clearToken()
      window.location.href = '/login'
      throw new Error(resp.status === 401 ? 'Unauthorized' : 'Forbidden')
    }
    if (!resp.ok) {
      const text = await resp.text()
      throw new Error(text || `HTTP ${resp.status}`)
    }
    const blob = await resp.blob()
    const disposition = resp.headers.get('Content-Disposition')
    let filename = `archive.${format}`
    if (disposition) {
      const match = disposition.match(/filename="(.+?)"/)
      if (match) filename = match[1]
    }
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    URL.revokeObjectURL(url)
  },

  // --- Users ---

  async listUsers(): Promise<User[]> {
    const resp = await fetch('/api/v1/users', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async getUser(username: string): Promise<User> {
    const resp = await fetch(`/api/v1/users/${encodeURIComponent(username)}`, {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async createUser(req: CreateUserRequest): Promise<User> {
    const resp = await fetch('/api/v1/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async updateUser(username: string, req: UpdateUserRequest): Promise<User> {
    const resp = await fetch(`/api/v1/users/${encodeURIComponent(username)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async deleteUser(username: string): Promise<void> {
    const resp = await fetch(`/api/v1/users/${encodeURIComponent(username)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  // --- Roles ---

  async listRoles(): Promise<Role[]> {
    const resp = await fetch('/api/v1/roles', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async getRole(name: string): Promise<Role> {
    const resp = await fetch(`/api/v1/roles/${encodeURIComponent(name)}`, {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async createRole(req: CreateRoleRequest): Promise<Role> {
    const resp = await fetch('/api/v1/roles', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async updateRole(name: string, req: UpdateRoleRequest): Promise<Role> {
    const resp = await fetch(`/api/v1/roles/${encodeURIComponent(name)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async deleteRole(name: string): Promise<void> {
    const resp = await fetch(`/api/v1/roles/${encodeURIComponent(name)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  // --- Stores ---

  async listStores(): Promise<Store[]> {
    const resp = await fetch('/api/v1/stores', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async getStore(name: string): Promise<Store> {
    const resp = await fetch(`/api/v1/stores/${encodeURIComponent(name)}`, {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async createStore(req: CreateStoreRequest): Promise<Store> {
    const resp = await fetch('/api/v1/stores', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async updateStore(name: string, req: UpdateStoreRequest): Promise<Store> {
    const resp = await fetch(`/api/v1/stores/${encodeURIComponent(name)}`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(req),
    })
    return handleResponse(resp)
  },

  async browseStore(name: string, path?: string): Promise<BrowseResponse> {
    const query = new URLSearchParams()
    if (path) query.set('path', path)
    const qs = query.toString()
    const resp = await fetch(
      `/api/v1/stores/${encodeURIComponent(name)}/browse${qs ? '?' + qs : ''}`,
      { headers: authHeaders() },
    )
    return handleResponse(resp)
  },

  async checkStore(name: string): Promise<StoreCheckResult> {
    const resp = await fetch(`/api/v1/stores/${encodeURIComponent(name)}/check`, {
      method: 'POST',
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async deleteStore(name: string): Promise<void> {
    const resp = await fetch(`/api/v1/stores/${encodeURIComponent(name)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  // --- Settings ---

  async getSettings(): Promise<Settings> {
    const resp = await fetch('/api/v1/settings', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async updateSettings(settings: Settings): Promise<Settings> {
    const resp = await fetch('/api/v1/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(settings),
    })
    return handleResponse(resp)
  },

  // --- Tasks ---

  async listTasks(): Promise<TaskInfo[]> {
    const resp = await fetch('/api/v1/tasks', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async getSchedulerStatus(): Promise<SchedulerStatus> {
    const resp = await fetch('/api/v1/tasks/scheduler', { headers: authHeaders() })
    return handleResponse(resp)
  },

  async startCheck(dryRun: boolean = true, verifyBlobHashes: boolean = true): Promise<TaskInfo> {
    const resp = await fetch('/api/v1/tasks/check', {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ dry_run: dryRun, verify_blob_hashes: verifyBlobHashes }),
    })
    return handleResponse(resp)
  },

  async startGc(dryRun: boolean = false): Promise<TaskInfo> {
    const resp = await fetch('/api/v1/tasks/gc', {
      method: 'POST',
      headers: { ...authHeaders(), 'Content-Type': 'application/json' },
      body: JSON.stringify({ dry_run: dryRun }),
    })
    return handleResponse(resp)
  },

  async startRebuildDirEntries(): Promise<TaskInfo> {
    const resp = await fetch('/api/v1/tasks/rebuild-dir-entries', {
      method: 'POST',
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async startRepoClean(name: string): Promise<TaskInfo> {
    const resp = await fetch(`/api/v1/repositories/${encodeURIComponent(name)}/clean`, {
      method: 'POST',
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async getTask(id: string): Promise<TaskInfo> {
    const resp = await fetch(`/api/v1/tasks/${encodeURIComponent(id)}`, {
      headers: authHeaders(),
    })
    return handleResponse(resp)
  },

  async deleteTask(id: string): Promise<void> {
    const resp = await fetch(`/api/v1/tasks/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: authHeaders(),
    })
    return handleVoidResponse(resp)
  },

  // --- Backup & Restore ---

  async downloadBackup(): Promise<void> {
    const resp = await fetch('/api/v1/backup', { headers: authHeaders() })
    if (resp.status === 401 || resp.status === 403) {
      clearToken()
      window.location.href = '/login'
      throw new Error(resp.status === 401 ? 'Unauthorized' : 'Forbidden')
    }
    if (!resp.ok) {
      const text = await resp.text()
      throw new Error(text || `HTTP ${resp.status}`)
    }
    const blob = await resp.blob()
    const disposition = resp.headers.get('Content-Disposition')
    let filename = 'depot-backup.json'
    if (disposition) {
      const match = disposition.match(/filename="(.+?)"/)
      if (match) filename = match[1]
    }
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = filename
    document.body.appendChild(a)
    a.click()
    document.body.removeChild(a)
    URL.revokeObjectURL(url)
  },

  async restoreBackup(data: unknown): Promise<RestoreSummary> {
    const resp = await fetch('/api/v1/restore', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', ...authHeaders() },
      body: JSON.stringify(data),
    })
    return handleResponse(resp)
  },
}
