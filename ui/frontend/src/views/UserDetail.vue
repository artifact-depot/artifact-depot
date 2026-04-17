// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api, type Role, getToken, setToken } from '../api'
import BaseModal from '../components/BaseModal.vue'
import PageHeader from '../components/PageHeader.vue'

const route = useRoute()
const router = useRouter()
const error = ref('')
const submitting = ref(false)
const isCreate = computed(() => route.name === 'user-create')
const isAnonymous = computed(() => !isCreate.value && username.value === 'anonymous')

// Extract the logged-in username from the JWT so we can tell whether the
// admin is changing *their own* password or another user's.
function loggedInUsername(): string | null {
  const token = getToken()
  if (!token) return null
  try {
    const payload = JSON.parse(atob(token.split('.')[1]))
    return payload.sub ?? null
  } catch { return null }
}
const isSelf = computed(() => username.value === loggedInUsername())
const requireCurrentPassword = computed(() => isSelf.value)

const username = ref('')
const password = ref('')
const selectedRoles = ref<string[]>([])
const availableRoles = ref<Role[]>([])
const mustChangePassword = ref(false)

// Change password dialog state
const showPasswordDialog = ref(false)
const passwordError = ref('')
const passwordSubmitting = ref(false)
const currentPassword = ref('')
const newPassword = ref('')
const confirmNewPassword = ref('')

async function loadRoles() {
  try {
    availableRoles.value = await api.listRoles()
  } catch {
    // non-critical
  }
}

async function loadUser() {
  if (isCreate.value) return
  try {
    const u = await api.getUser(route.params.username as string)
    username.value = u.username
    selectedRoles.value = [...u.roles]
    mustChangePassword.value = u.must_change_password
  } catch (e: any) {
    error.value = e.message
  }
}

function toggleRole(name: string) {
  const idx = selectedRoles.value.indexOf(name)
  if (idx >= 0) {
    selectedRoles.value.splice(idx, 1)
  } else {
    selectedRoles.value.push(name)
  }
}

const canSubmit = computed(() => {
  if (isCreate.value) return !!username.value && !!password.value && !submitting.value
  return !submitting.value
})

async function submit() {
  if (!canSubmit.value) return
  submitting.value = true
  error.value = ''
  try {
    if (isCreate.value) {
      await api.createUser({ username: username.value, password: password.value, roles: selectedRoles.value, must_change_password: mustChangePassword.value })
    } else {
      await api.updateUser(route.params.username as string, { roles: selectedRoles.value, must_change_password: mustChangePassword.value })
    }
    router.push({ name: 'users' })
  } catch (e: any) {
    error.value = e.message
  } finally {
    submitting.value = false
  }
}

function openPasswordDialog() {
  currentPassword.value = ''
  newPassword.value = ''
  confirmNewPassword.value = ''
  passwordError.value = ''
  showPasswordDialog.value = true
}

async function submitPasswordChange() {
  passwordError.value = ''
  if (!newPassword.value) {
    passwordError.value = 'New password is required'
    return
  }
  if (newPassword.value !== confirmNewPassword.value) {
    passwordError.value = 'New passwords do not match'
    return
  }
  passwordSubmitting.value = true
  try {
    const payload: Record<string, string> = {
      username: username.value,
      new_password: newPassword.value,
    }
    if (requireCurrentPassword.value) {
      payload.current_password = currentPassword.value
    }
    const resp = await fetch('/api/v1/auth/change-password', {
      method: 'POST',
      headers: (() => {
        const h: Record<string, string> = { 'Content-Type': 'application/json', 'X-Requested-With': 'XMLHttpRequest' }
        const t = getToken()
        if (t) h['Authorization'] = `Bearer ${t}`
        return h
      })(),
      body: JSON.stringify(payload),
    })
    if (!resp.ok) {
      const text = await resp.text()
      passwordError.value = text || `HTTP ${resp.status}`
      return
    }
    // If the server returned a fresh token (self-change), swap it in so
    // the user stays logged in despite the token_secret rotation.
    try {
      const body = await resp.json()
      if (body?.token) setToken(body.token)
    } catch {
      // 204 No Content (admin changing another user) — no body to parse.
    }
    showPasswordDialog.value = false
  } catch (e: any) {
    passwordError.value = e.message
  } finally {
    passwordSubmitting.value = false
  }
}

onMounted(async () => {
  await loadRoles()
  await loadUser()
})
</script>

<template>
  <section>
    <PageHeader :title="isCreate ? 'Create User' : `Edit User: ${username}`" />

    <p v-if="error" class="error">{{ error }}</p>

    <form class="create-form" @submit.prevent="submit">
      <div class="form-group" v-if="isCreate">
        <label for="username">Username <span class="required">*</span></label>
        <input id="username" v-model="username" required placeholder="username" />
      </div>

      <div class="form-group" v-if="isCreate">
        <label for="password">Password <span class="required">*</span></label>
        <input id="password" v-model="password" type="password" placeholder="password" />
      </div>

      <div v-if="!isCreate && !isAnonymous" class="form-group">
        <button type="button" class="btn btn-secondary" @click="openPasswordDialog">Change Password</button>
      </div>

      <div v-if="!isAnonymous" class="form-group checkbox-group">
        <label>
          <input type="checkbox" v-model="mustChangePassword" />
          Must change password on next login
        </label>
      </div>

      <div class="form-group">
        <label>Roles</label>
        <div class="role-list">
          <label v-for="r in availableRoles" :key="r.name" class="checkbox-group">
            <input type="checkbox" :checked="selectedRoles.includes(r.name)" @change="toggleRole(r.name)" />
            {{ r.name }}
          </label>
          <p v-if="!availableRoles.length" class="hint">No roles available</p>
        </div>
      </div>

      <div class="form-actions">
        <router-link to="/users" class="btn">Cancel</router-link>
        <button type="submit" class="btn btn-primary" :disabled="!canSubmit">
          {{ submitting ? 'Saving...' : (isCreate ? 'Create' : 'Save') }}
        </button>
      </div>
    </form>

    <!-- Change password dialog -->
    <BaseModal v-if="showPasswordDialog" title="Change Password" @close="showPasswordDialog = false">
      <p v-if="passwordError" class="error">{{ passwordError }}</p>
      <form @submit.prevent="submitPasswordChange">
        <div v-if="requireCurrentPassword" class="form-group">
          <label for="cp-current">Current Password <span class="required">*</span></label>
          <input id="cp-current" v-model="currentPassword" type="password" required />
        </div>
        <div class="form-group">
          <label for="cp-new">New Password <span class="required">*</span></label>
          <input id="cp-new" v-model="newPassword" type="password" required />
        </div>
        <div class="form-group">
          <label for="cp-confirm">Confirm New Password <span class="required">*</span></label>
          <input id="cp-confirm" v-model="confirmNewPassword" type="password" required />
        </div>
        <div class="modal-actions">
          <button type="button" class="btn" @click="showPasswordDialog = false">Cancel</button>
          <button type="submit" class="btn btn-primary" :disabled="passwordSubmitting">
            {{ passwordSubmitting ? 'Changing...' : 'Change Password' }}
          </button>
        </div>
      </form>
    </BaseModal>
  </section>
</template>

<style scoped>
.create-form { max-width: 640px; margin: 0 auto; }
.role-list { display: flex; flex-direction: column; gap: 0.25rem; }
</style>
