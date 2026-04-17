// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, type Repo, type UpdateRepoRequest } from '../api'
import ArtifactBrowser from '../components/ArtifactBrowser.vue'
import ConfirmDialog from '../components/ConfirmDialog.vue'
import PageHeader from '../components/PageHeader.vue'
import { formatDate } from '../composables/useFormatters'

const route = useRoute()
const router = useRouter()
const repoName = route.params.name as string
const repo = ref<Repo | null>(null)
const error = ref('')
const activeTab = ref<'browse' | 'manage'>('browse')

// Manage tab state
const saving = ref(false)
const saveError = ref('')
const saveSuccess = ref(false)
const cleanLoading = ref(false)
const cleanError = ref('')
const showDeleteDialog = ref(false)
const deleting = ref(false)
const deleteError = ref('')
const showCloneDialog = ref(false)
const cloning = ref(false)
const cloneError = ref('')
const cloneName = ref('')

// Form fields for editable settings
const formUpstreamUrl = ref('')
const formCacheTtl = ref<number | undefined>()
const formAuthUsername = ref('')
const formAuthPassword = ref('')
const members = ref<string[]>([])
const allRepos = ref<Repo[]>([])
const newMember = ref('')
const formWriteMember = ref('')
const formListen = ref('')
const formCleanupUntagged = ref(false)
const formAptComponents = ref('')
const aptDistributions = ref<string[]>([])
const snapshotSource = ref('')
const snapshotName = ref('')
const snapshotLoading = ref(false)
const snapshotError = ref('')
const formContentDisposition = ref<'attachment' | 'inline'>('attachment')
const formRepodataDepth = ref<number | undefined>()
const formCleanupMaxAge = ref<number | undefined>()
const formCleanupMaxUnaccessed = ref<number | undefined>()

function populateForm(r: Repo) {
  formUpstreamUrl.value = r.upstream_url || ''
  formCacheTtl.value = r.cache_ttl_secs ?? undefined
  formAuthUsername.value = r.upstream_auth?.username || ''
  formAuthPassword.value = r.upstream_auth?.password || ''
  members.value = [...(r.members || [])]
  formWriteMember.value = r.write_member || ''
  formListen.value = r.listen || ''
  formCleanupUntagged.value = r.cleanup_untagged_manifests || false
  formAptComponents.value = r.apt_components?.join(', ') || ''
  formContentDisposition.value = r.content_disposition || 'attachment'
  formRepodataDepth.value = r.repodata_depth ?? undefined
  formCleanupMaxAge.value = r.cleanup_max_age_days ?? undefined
  formCleanupMaxUnaccessed.value = r.cleanup_max_unaccessed_days ?? undefined
}

const availableRepos = computed(() =>
  allRepos.value.filter(r =>
    repo.value &&
    r.format === repo.value.format &&
    r.repo_type !== 'proxy' &&
    r.name !== repoName &&
    !members.value.includes(r.name)
  )
)

function addMember() {
  if (newMember.value && !members.value.includes(newMember.value)) {
    members.value.push(newMember.value)
    newMember.value = ''
  }
}

function removeMember(i: number) {
  members.value.splice(i, 1)
}

function moveMember(i: number, dir: number) {
  const j = i + dir
  const tmp = members.value[i]
  members.value[i] = members.value[j]
  members.value[j] = tmp
}

const hasCleanupPolicy = computed(() =>
  repo.value && (
    repo.value.cleanup_max_age_days != null ||
    repo.value.cleanup_max_unaccessed_days != null ||
    repo.value.format === 'docker'
  )
)

function typeClass(t: string): string {
  switch (t) {
    case 'hosted': return 'badge-green'
    case 'cache': return 'badge-orange'
    case 'proxy': return 'badge-blue'
    default: return 'badge-grey'
  }
}

async function save() {
  if (!repo.value) return
  saving.value = true
  saveError.value = ''
  saveSuccess.value = false
  try {
    const req: UpdateRepoRequest = {
      cleanup_max_age_days: formCleanupMaxAge.value ? Number(formCleanupMaxAge.value) : null,
      cleanup_max_unaccessed_days: formCleanupMaxUnaccessed.value ? Number(formCleanupMaxUnaccessed.value) : null,
    }
    if (repo.value.repo_type === 'cache') {
      req.upstream_url = formUpstreamUrl.value || undefined
      req.cache_ttl_secs = formCacheTtl.value ? Number(formCacheTtl.value) : undefined
      if (formAuthUsername.value) {
        req.upstream_auth = {
          username: formAuthUsername.value,
          password: formAuthPassword.value || undefined,
        }
      } else {
        req.upstream_auth = null
      }
    }
    if (repo.value.repo_type === 'proxy') {
      req.members = members.value
      req.write_member = formWriteMember.value || null
    }
    if (repo.value.format === 'raw') {
      req.content_disposition = formContentDisposition.value
    }
    if (repo.value.format === 'docker') {
      req.listen = formListen.value || null
      req.cleanup_untagged_manifests = formCleanupUntagged.value || null
    }
    if (repo.value.format === 'apt') {
      req.apt_components = formAptComponents.value
        ? formAptComponents.value.split(',').map(s => s.trim()).filter(Boolean)
        : null
    }
    if (repo.value.format === 'yum') {
      req.repodata_depth = formRepodataDepth.value != null && (formRepodataDepth.value as unknown) !== '' ? Number(formRepodataDepth.value) : null
    }
    const updated = await api.updateRepo(repoName, req)
    repo.value = updated
    populateForm(updated)
    saveSuccess.value = true
    setTimeout(() => saveSuccess.value = false, 3000)
  } catch (e: any) {
    saveError.value = e.message
  } finally {
    saving.value = false
  }
}

async function startClean() {
  cleanLoading.value = true
  cleanError.value = ''
  try {
    await api.startRepoClean(repoName)
    router.push('/tasks')
  } catch (e: any) {
    cleanError.value = e.message
  } finally {
    cleanLoading.value = false
  }
}

async function doDelete() {
  deleting.value = true
  deleteError.value = ''
  try {
    await api.deleteRepo(repoName)
    router.push({ name: 'repositories' })
  } catch (e: any) {
    deleteError.value = e.message
  } finally {
    deleting.value = false
  }
}

function openCloneDialog() {
  cloneName.value = repoName + '-copy'
  cloneError.value = ''
  showCloneDialog.value = true
}

async function doClone() {
  if (!cloneName.value) return
  cloning.value = true
  cloneError.value = ''
  try {
    await api.cloneRepo(repoName, { new_name: cloneName.value })
    showCloneDialog.value = false
    router.push('/tasks')
  } catch (e: any) {
    cloneError.value = e.message
  } finally {
    cloning.value = false
  }
}

async function loadDistributions() {
  if (repo.value?.format !== 'apt') return
  try {
    const resp = await fetch(`/apt/${repoName}/distributions`)
    if (resp.ok) aptDistributions.value = await resp.json()
  } catch {
    // non-critical
  }
}

async function createSnapshot() {
  if (!snapshotSource.value || !snapshotName.value) return
  snapshotLoading.value = true
  snapshotError.value = ''
  try {
    const resp = await fetch(`/apt/${repoName}/snapshots`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: snapshotSource.value, name: snapshotName.value }),
    })
    if (!resp.ok) throw new Error(await resp.text())
    snapshotName.value = ''
    await loadDistributions()
  } catch (e: any) {
    snapshotError.value = e.message
  } finally {
    snapshotLoading.value = false
  }
}

onMounted(async () => {
  try {
    repo.value = await api.getRepo(repoName)
    populateForm(repo.value)
    await loadDistributions()
  } catch (e: any) {
    error.value = e.message
  }
  try {
    allRepos.value = await api.listRepos()
  } catch {
    // non-critical
  }
})
</script>

<template>
  <section>
    <PageHeader :title="repoName">
      <template #badges>
        <span v-if="repo" class="badge" :class="typeClass(repo.repo_type)">{{ repo.repo_type }}</span>
        <span v-if="repo" class="badge badge-outlined">{{ repo.format }}</span>
      </template>
    </PageHeader>

    <p v-if="error" class="error">{{ error }}</p>

    <div v-if="repo" class="detail-card">
      <table class="detail-table">
        <tbody>
          <tr>
            <th>Type</th>
            <td>{{ repo.repo_type }}</td>
            <th>Format</th>
            <td>{{ repo.format }}</td>
          </tr>
          <tr>
            <th v-if="repo.upstream_url">Upstream URL</th>
            <td v-if="repo.upstream_url">{{ repo.upstream_url }}</td>
            <th v-if="repo.cache_ttl_secs">Cache TTL</th>
            <td v-if="repo.cache_ttl_secs">{{ repo.cache_ttl_secs }}s</td>
            <th v-if="!repo.upstream_url && !repo.cache_ttl_secs"></th>
            <td v-if="!repo.upstream_url && !repo.cache_ttl_secs"></td>
            <th v-if="!repo.upstream_url && !repo.cache_ttl_secs"></th>
            <td v-if="!repo.upstream_url && !repo.cache_ttl_secs"></td>
          </tr>
          <tr v-if="repo.members?.length || repo.listen">
            <th v-if="repo.members?.length">Members</th>
            <td v-if="repo.members?.length">{{ repo.members.join(', ') }}</td>
            <th v-if="repo.listen">Listen</th>
            <td v-if="repo.listen">{{ repo.listen }}</td>
          </tr>
          <tr v-if="repo.write_member">
            <th>Write Member</th>
            <td>{{ repo.write_member }}</td>
            <th></th>
            <td></td>
          </tr>
          <tr>
            <th>Store</th>
            <td>{{ repo.store }}</td>
            <th>Created</th>
            <td>{{ formatDate(repo.created_at) }}</td>
          </tr>
        </tbody>
      </table>
    </div>

    <div v-if="repo" class="tab-bar">
      <button :class="{ active: activeTab === 'browse' }" @click="activeTab = 'browse'">Browse</button>
      <button :class="{ active: activeTab === 'manage' }" @click="activeTab = 'manage'">Manage</button>
    </div>

    <artifact-browser v-if="repo && activeTab === 'browse'" :repo-name="repoName" />

    <div v-if="repo && activeTab === 'manage'" class="manage-section">
      <form class="manage-form" @submit.prevent="save">
        <h3>Settings</h3>

        <div class="form-group">
          <label>Name</label>
          <input :value="repo.name" disabled />
        </div>

        <div class="form-group">
          <label>Type</label>
          <input :value="repo.repo_type" disabled />
        </div>

        <div class="form-group">
          <label>Format</label>
          <input :value="repo.format" disabled />
        </div>

        <div class="form-group">
          <label>Store</label>
          <input :value="repo.store" disabled />
        </div>

        <div class="form-group">
          <label>Created</label>
          <input :value="formatDate(repo.created_at)" disabled />
        </div>

        <hr />

        <!-- Cache-specific fields -->
        <template v-if="repo.repo_type === 'cache'">
          <div class="form-group">
            <label for="upstream_url">Upstream URL</label>
            <input id="upstream_url" v-model="formUpstreamUrl" placeholder="https://registry-1.docker.io" />
          </div>

          <div class="form-group">
            <label for="cache_ttl">Cache TTL (seconds)</label>
            <input id="cache_ttl" v-model.number="formCacheTtl" type="number" placeholder="3600" />
          </div>

          <div class="form-group">
            <label for="auth_user">Upstream Username</label>
            <input id="auth_user" v-model="formAuthUsername" placeholder="username" />
          </div>

          <div class="form-group">
            <label for="auth_pass">Upstream Password</label>
            <input id="auth_pass" v-model="formAuthPassword" type="password" placeholder="password" />
          </div>
        </template>

        <!-- Proxy-specific fields -->
        <div v-if="repo.repo_type === 'proxy'" class="form-group">
          <label>Members</label>
          <ul v-if="members.length" class="member-list">
            <li v-for="(m, i) in members" :key="m">
              <span class="member-name">{{ m }}</span>
              <button type="button" class="reorder-btn" :disabled="i === 0" @click="moveMember(i, -1)">&uarr;</button>
              <button type="button" class="reorder-btn" :disabled="i === members.length - 1" @click="moveMember(i, 1)">&darr;</button>
              <button type="button" class="remove-btn" @click="removeMember(i)">&times;</button>
            </li>
          </ul>
          <div class="add-member">
            <select v-model="newMember">
              <option value="" disabled>Add a member…</option>
              <option v-for="r in availableRepos" :key="r.name" :value="r.name">{{ r.name }} ({{ r.repo_type }})</option>
            </select>
            <button type="button" class="btn" @click="addMember" :disabled="!newMember">Add</button>
          </div>
        </div>

        <div v-if="repo.repo_type === 'proxy'" class="form-group">
          <label for="write_member">Write Member</label>
          <select id="write_member" v-model="formWriteMember">
            <option value="">None (defaults to first hosted member)</option>
            <option v-for="m in members" :key="m" :value="m">{{ m }}</option>
          </select>
          <p class="hint">Member repository that receives PUT/DELETE writes</p>
        </div>

        <!-- Raw-specific fields -->
        <template v-if="repo.format === 'raw'">
          <div class="form-group">
            <label for="content_disposition">Content Disposition</label>
            <select id="content_disposition" v-model="formContentDisposition">
              <option value="attachment">attachment</option>
              <option value="inline">inline</option>
            </select>
            <p class="hint">Controls whether browsers download or display artifacts inline</p>
          </div>
        </template>

        <!-- Docker-specific fields -->
        <template v-if="repo.format === 'docker'">
          <div class="form-group">
            <label for="listen">Dedicated Listen Address</label>
            <input id="listen" v-model="formListen" placeholder="0.0.0.0:5000" />
          </div>

          <div class="form-group checkbox-group">
            <label>
              <input type="checkbox" v-model="formCleanupUntagged" />
              Clean up untagged manifests
            </label>
          </div>
        </template>

        <!-- Apt-specific fields -->
        <template v-if="repo.format === 'apt'">
          <div class="form-group">
            <label for="apt_comp">APT Default Components</label>
            <input id="apt_comp" v-model="formAptComponents" placeholder="main, contrib" />
            <p class="hint">Comma-separated list of default components</p>
          </div>

          <div class="form-group" v-if="aptDistributions.length">
            <label>Distributions</label>
            <ul class="dist-list">
              <li v-for="d in aptDistributions" :key="d">{{ d }}</li>
            </ul>
          </div>

          <div class="form-group">
            <label>Clone Distribution</label>
            <div style="display:flex;gap:0.5rem;align-items:center">
              <select v-model="snapshotSource">
                <option value="" disabled>Source</option>
                <option v-for="d in aptDistributions" :key="d" :value="d">{{ d }}</option>
              </select>
              <span>to</span>
              <input v-model="snapshotName" placeholder="snapshot name" style="flex:1" />
              <button type="button" class="btn btn-primary" :disabled="snapshotLoading || !snapshotSource || !snapshotName" @click="createSnapshot">
                {{ snapshotLoading ? 'Cloning...' : 'Clone' }}
              </button>
            </div>
            <p v-if="snapshotError" class="error">{{ snapshotError }}</p>
          </div>
        </template>

        <!-- Yum-specific fields -->
        <template v-if="repo.format === 'yum' && repo.repo_type === 'hosted'">
          <div class="form-group">
            <label for="repodata_depth">Repodata Depth</label>
            <input id="repodata_depth" v-model.number="formRepodataDepth" type="number" min="0" max="5" />
            <p class="hint">Directory depth for repodata grouping. Changing this rebuilds all metadata.</p>
          </div>
        </template>

        <!-- Cleanup fields (all types) -->
        <div class="form-group">
          <label for="max_age">Cleanup: Max Age (days)</label>
          <input id="max_age" v-model.number="formCleanupMaxAge" type="number" min="1" placeholder="No limit" />
        </div>

        <div class="form-group">
          <label for="max_unaccessed">Cleanup: Max Unaccessed (days)</label>
          <input id="max_unaccessed" v-model.number="formCleanupMaxUnaccessed" type="number" min="1" placeholder="No limit" />
        </div>

        <p v-if="saveError" class="error">{{ saveError }}</p>
        <p v-if="saveSuccess" class="success">Settings saved.</p>

        <div class="form-actions">
          <button type="submit" class="btn btn-primary" :disabled="saving">
            {{ saving ? 'Saving...' : 'Save' }}
          </button>
          <button
            v-if="hasCleanupPolicy"
            type="button"
            class="btn btn-clean"
            :disabled="cleanLoading"
            @click="startClean"
          >
            {{ cleanLoading ? 'Starting...' : 'Clean' }}
          </button>
        </div>
        <p v-if="cleanError" class="error">{{ cleanError }}</p>
      </form>

      <hr />

      <div class="clone-section">
        <h3>Clone Repository</h3>
        <p>Create a new repository with the same configuration and all the same artifacts.</p>
        <button class="btn btn-primary" @click="openCloneDialog">Clone Repository</button>
      </div>

      <!-- Clone confirmation dialog -->
      <ConfirmDialog
        v-if="showCloneDialog"
        title="Clone Repository"
        :error="cloneError"
        :loading="cloning"
        action-label="Clone"
        action-loading-label="Cloning…"
        action-variant="primary"
        @confirm="doClone"
        @cancel="showCloneDialog = false"
      >
        <p>Create a copy of <strong>{{ repoName }}</strong> with a new name.</p>
        <template #extra>
          <div class="form-group">
            <label for="clone_name">New Repository Name</label>
            <input id="clone_name" v-model="cloneName" placeholder="New name" @keyup.enter="doClone" />
          </div>
        </template>
      </ConfirmDialog>

      <hr />

      <div class="danger-zone">
        <h3>Danger Zone</h3>
        <p>Deleting a repository permanently removes all its artifacts. This cannot be undone.</p>
        <button class="btn btn-danger" @click="showDeleteDialog = true">Delete Repository</button>
      </div>

      <!-- Delete confirmation dialog -->
      <ConfirmDialog
        v-if="showDeleteDialog"
        title="Delete Repository"
        message="Are you sure you want to delete"
        :item-name="repoName"
        :error="deleteError"
        :loading="deleting"
        @confirm="doDelete"
        @cancel="showDeleteDialog = false"
      >
        <p>This cannot be undone.</p>
      </ConfirmDialog>
    </div>
  </section>
</template>

<style scoped>
/* Manage form */
.manage-section {
  max-width: 640px;
  margin: 0 auto;
}
.manage-form {
  margin-bottom: 1rem;
}
hr {
  border: none;
  border-top: 1px solid var(--color-border);
  margin: 1.5rem 0;
}

/* Clone section */
.clone-section h3 {
  color: var(--color-primary);
}
.clone-section p {
  font-size: 0.9rem;
  color: var(--color-text-muted);
  margin-bottom: 1rem;
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

/* Member list */
.member-list {
  list-style: none;
  padding: 0;
  margin: 0 0 0.5rem;
}
.member-list li {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.35rem 0.5rem;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  margin-bottom: 0.25rem;
  background: var(--color-bg);
}
.member-name {
  flex: 1;
}
.reorder-btn {
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
  padding: 0.1rem 0.4rem;
  font-size: 0.85rem;
  color: var(--color-text-secondary);
}
.reorder-btn:disabled {
  opacity: 0.3;
  cursor: not-allowed;
}
.remove-btn {
  background: none;
  border: 1px solid var(--color-danger);
  border-radius: var(--radius-sm);
  cursor: pointer;
  padding: 0.1rem 0.4rem;
  font-size: 0.85rem;
  color: var(--color-danger);
}
.add-member {
  display: flex;
  gap: 0.5rem;
}
.add-member select {
  flex: 1;
  padding: 0.4rem 0.6rem;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-md);
  font-size: 0.9rem;
  font-family: inherit;
}
</style>
