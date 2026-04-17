// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, type CapabilityGrant } from '../api'
import PageHeader from '../components/PageHeader.vue'

const route = useRoute()
const router = useRouter()
const error = ref('')
const submitting = ref(false)
const isCreate = computed(() => route.name === 'role-create')

const name = ref('')
const description = ref('')
const capabilities = ref<CapabilityGrant[]>([])

function addCapability() {
  capabilities.value.push({ capability: 'read', repo: '*' })
}

function removeCapability(index: number) {
  capabilities.value.splice(index, 1)
}

async function loadRole() {
  if (isCreate.value) return
  try {
    const r = await api.getRole(route.params.name as string)
    name.value = r.name
    description.value = r.description
    capabilities.value = [...r.capabilities]
  } catch (e: any) {
    error.value = e.message
  }
}

const canSubmit = computed(() => {
  if (isCreate.value) return !!name.value && !submitting.value
  return !submitting.value
})

async function submit() {
  if (!canSubmit.value) return
  submitting.value = true
  error.value = ''
  try {
    if (isCreate.value) {
      await api.createRole({
        name: name.value,
        description: description.value,
        capabilities: capabilities.value,
      })
    } else {
      await api.updateRole(route.params.name as string, {
        description: description.value,
        capabilities: capabilities.value,
      })
    }
    router.push({ name: 'roles' })
  } catch (e: any) {
    error.value = e.message
  } finally {
    submitting.value = false
  }
}

onMounted(loadRole)
</script>

<template>
  <section>
    <PageHeader :title="isCreate ? 'Create Role' : `Edit Role: ${name}`" />

    <p v-if="error" class="error">{{ error }}</p>

    <form class="create-form" @submit.prevent="submit">
      <div class="form-group" v-if="isCreate">
        <label for="name">Name <span class="required">*</span></label>
        <input id="name" v-model="name" required placeholder="role-name" />
      </div>

      <div class="form-group">
        <label for="description">Description</label>
        <input id="description" v-model="description" placeholder="Optional description" />
      </div>

      <div class="form-group">
        <label>Capabilities</label>
        <div v-for="(cap, i) in capabilities" :key="i" class="cap-row">
          <select v-model="cap.capability">
            <option value="read">read</option>
            <option value="write">write</option>
            <option value="delete">delete</option>
          </select>
          <span class="cap-on">on</span>
          <input v-model="cap.repo" placeholder="repo name or *" class="cap-repo" />
          <button type="button" class="remove-btn" @click="removeCapability(i)">x</button>
        </div>
        <button type="button" class="add-btn" @click="addCapability">+ Add capability</button>
      </div>

      <div class="form-actions">
        <router-link to="/roles" class="btn">Cancel</router-link>
        <button type="submit" class="btn btn-primary" :disabled="!canSubmit">
          {{ submitting ? 'Saving...' : (isCreate ? 'Create' : 'Save') }}
        </button>
      </div>
    </form>
  </section>
</template>

<style scoped>
.create-form { max-width: 640px; margin: 0 auto; }
.form-group > input { width: 100%; }
.cap-row { display: flex; align-items: center; gap: 0.5rem; margin-bottom: 0.5rem; }
.cap-row select { width: auto; flex: 0 0 auto; }
.cap-on { font-size: 0.85rem; color: var(--color-text-muted); }
.cap-repo { flex: 1; min-width: 0; }
.remove-btn {
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-md);
  cursor: pointer;
  font-size: 0.85rem;
  padding: 0.25rem 0.5rem;
  color: var(--color-danger);
}
.remove-btn:hover {
  background: var(--color-danger-banner-bg);
  border-color: var(--color-danger);
}
.add-btn {
  background: none;
  border: 1px dashed var(--color-border-strong);
  border-radius: var(--radius-md);
  cursor: pointer;
  padding: 0.3rem 0.8rem;
  font-size: 0.85rem;
  color: var(--color-text-muted);
}
.add-btn:hover {
  border-color: var(--color-text-disabled);
  color: var(--color-text-strong);
}
</style>
