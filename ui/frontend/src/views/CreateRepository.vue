// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useRouter } from 'vue-router'
import { api, type CreateRepoRequest } from '../api'
import { useBlobStoreStore } from '../stores/blobStoreStore'
import { useRepoStore } from '../stores/repoStore'
import PageHeader from '../components/PageHeader.vue'

const router = useRouter()
const blobStoreStore = useBlobStoreStore()
const repoStore = useRepoStore()
const error = ref('')
const submitting = ref(false)
const stores = computed(() => blobStoreStore.stores)

const form = ref<CreateRepoRequest>({
  name: '',
  repo_type: 'hosted',
  format: 'raw',
  store: '',
  upstream_url: undefined,
  cache_ttl_secs: undefined,
  members: undefined,
  write_member: undefined,
  listen: undefined,
  cleanup_max_age_days: undefined,
  cleanup_max_unaccessed_days: undefined,
  cleanup_untagged_manifests: undefined,
  apt_components: undefined,
  upstream_auth: undefined,
  content_disposition: undefined,
  repodata_depth: undefined,
})

const members = ref<string[]>([])
const repos = computed(() => repoStore.repos)
const newMember = ref('')
const aptComponentsInput = ref('')
const authUsername = ref('')
const authPassword = ref('')

const availableRepos = computed(() =>
  repos.value.filter(r =>
    r.format === form.value.format &&
    r.repo_type !== 'proxy' &&
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

const canSubmit = computed(() => {
  if (!form.value.name) return false
  if (form.value.repo_type === 'cache' && !form.value.upstream_url) return false
  return !submitting.value
})

async function submit() {
  if (!canSubmit.value) return

  submitting.value = true
  error.value = ''
  try {
    const req: CreateRepoRequest = {
      name: form.value.name,
      repo_type: form.value.repo_type,
      format: form.value.format,
      store: form.value.store,
    }
    if (form.value.repo_type === 'cache') {
      req.upstream_url = form.value.upstream_url
      req.cache_ttl_secs = form.value.cache_ttl_secs
      if (authUsername.value) {
        req.upstream_auth = {
          username: authUsername.value,
          password: authPassword.value || undefined,
        }
      }
    }
    if (form.value.repo_type === 'proxy') {
      if (members.value.length) req.members = members.value
      if (form.value.write_member) req.write_member = form.value.write_member
    }
    if (form.value.listen) {
      req.listen = form.value.listen
    }
    if (form.value.cleanup_max_age_days) {
      req.cleanup_max_age_days = form.value.cleanup_max_age_days
    }
    if (form.value.cleanup_max_unaccessed_days) {
      req.cleanup_max_unaccessed_days = form.value.cleanup_max_unaccessed_days
    }
    if (form.value.format === 'docker' && form.value.cleanup_untagged_manifests) {
      req.cleanup_untagged_manifests = form.value.cleanup_untagged_manifests
    }
    if (form.value.format === 'raw' && form.value.content_disposition) {
      req.content_disposition = form.value.content_disposition
    }
    if (form.value.format === 'apt') {
      const comps = aptComponentsInput.value.split(',').map(s => s.trim()).filter(Boolean)
      if (comps.length) req.apt_components = comps
    }
    if (form.value.format === 'yum' && form.value.repodata_depth != null) {
      req.repodata_depth = form.value.repodata_depth
    }
    await api.createRepo(req)
    router.push({ name: 'repositories' })
  } catch (e: any) {
    error.value = e.message
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <section>
    <PageHeader title="Create Repository" />

    <p v-if="error" class="error">{{ error }}</p>

    <form class="create-form" @submit.prevent="submit">
      <div class="form-group">
        <label for="name">Name <span class="required">*</span></label>
        <input id="name" v-model="form.name" required placeholder="my-repo" />
      </div>

      <div class="form-group">
        <label for="type">Type <span class="required">*</span></label>
        <select id="type" v-model="form.repo_type">
          <option value="hosted">hosted</option>
          <option value="cache">cache</option>
          <option value="proxy">proxy</option>
        </select>
      </div>

      <div class="form-group">
        <label for="format">Format <span class="required">*</span></label>
        <select id="format" v-model="form.format">
          <option value="raw">raw</option>
          <option value="docker">docker</option>
          <option value="cargo">cargo</option>
          <option value="pypi">pypi</option>
          <option value="apt">apt</option>
          <option value="golang">golang</option>
          <option value="helm">helm</option>
          <option value="yum">yum</option>
          <option value="npm">npm</option>
        </select>
      </div>

      <div class="form-group">
        <label for="store">Store</label>
        <select id="store" v-model="form.store">
          <option v-for="s in stores" :key="s.name" :value="s.name">{{ s.name }} ({{ s.store_type }})</option>
        </select>
      </div>

      <!-- Cache-specific fields -->
      <div v-if="form.repo_type === 'cache'" class="form-group">
        <label for="upstream">Upstream URL <span class="required">*</span></label>
        <input id="upstream" v-model="form.upstream_url" placeholder="https://registry-1.docker.io" />
      </div>

      <div v-if="form.repo_type === 'cache'" class="form-group">
        <label for="ttl">Cache TTL (seconds)</label>
        <input id="ttl" v-model.number="form.cache_ttl_secs" type="number" placeholder="3600" />
      </div>

      <div v-if="form.repo_type === 'cache'" class="form-group">
        <label for="auth_user">Upstream Username</label>
        <input id="auth_user" v-model="authUsername" placeholder="username" />
      </div>

      <div v-if="form.repo_type === 'cache'" class="form-group">
        <label for="auth_pass">Upstream Password</label>
        <input id="auth_pass" v-model="authPassword" type="password" placeholder="password" />
      </div>

      <!-- Proxy-specific fields -->
      <div v-if="form.repo_type === 'proxy'" class="form-group">
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

      <div v-if="form.repo_type === 'proxy'" class="form-group">
        <label for="write_member">Write Member</label>
        <select id="write_member" v-model="form.write_member">
          <option value="">None (defaults to first hosted member)</option>
          <option v-for="m in members" :key="m" :value="m">{{ m }}</option>
        </select>
      </div>

      <!-- Raw-specific fields -->
      <div v-if="form.format === 'raw'" class="form-group">
        <label for="content_disposition">Content Disposition</label>
        <select id="content_disposition" v-model="form.content_disposition">
          <option :value="undefined">attachment (default)</option>
          <option value="attachment">attachment</option>
          <option value="inline">inline</option>
        </select>
        <p class="hint">Controls whether browsers download or display artifacts inline</p>
      </div>

      <!-- Docker-specific fields -->
      <div v-if="form.format === 'docker'" class="form-group">
        <label for="listen">Dedicated Listen Address</label>
        <input id="listen" v-model="form.listen" placeholder="0.0.0.0:5000" />
      </div>

      <div v-if="form.format === 'docker'" class="form-group checkbox-group">
        <label>
          <input type="checkbox" v-model="form.cleanup_untagged_manifests" />
          Clean up untagged manifests
        </label>
      </div>

      <!-- Yum-specific fields -->
      <div v-if="form.format === 'yum' && form.repo_type === 'hosted'" class="form-group">
        <label for="repodata_depth">Repodata Depth</label>
        <input id="repodata_depth" v-model.number="form.repodata_depth" type="number" min="0" max="5" placeholder="0 (default)" />
        <p class="hint">Number of directory levels for repodata grouping (0 = single repodata at root)</p>
      </div>

      <!-- Apt-specific fields -->
      <div v-if="form.format === 'apt'" class="form-group">
        <label for="apt_comp">APT Components</label>
        <input id="apt_comp" v-model="aptComponentsInput" placeholder="main, contrib" />
        <p class="hint">Comma-separated list of components</p>
      </div>

      <!-- Cleanup fields (all types) -->
      <div class="form-group">
        <label for="max_age">Cleanup: Max Age (days)</label>
        <input id="max_age" v-model.number="form.cleanup_max_age_days" type="number" placeholder="No limit" />
      </div>

      <div class="form-group">
        <label for="max_unaccessed">Cleanup: Max Unaccessed (days)</label>
        <input id="max_unaccessed" v-model.number="form.cleanup_max_unaccessed_days" type="number" placeholder="No limit" />
      </div>

      <div class="form-actions">
        <router-link to="/repositories" class="btn">Cancel</router-link>
        <button type="submit" class="btn btn-primary" :disabled="!canSubmit">
          {{ submitting ? 'Creating...' : 'Create' }}
        </button>
      </div>
    </form>
  </section>
</template>

<style scoped>
.create-form {
  max-width: 640px;
  margin: 0 auto;
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
