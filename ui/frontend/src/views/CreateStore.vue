// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useRouter } from 'vue-router'
import { api, type CreateStoreRequest } from '../api'
import PageHeader from '../components/PageHeader.vue'

const router = useRouter()
const error = ref('')
const submitting = ref(false)

const form = ref<CreateStoreRequest>({
  name: '',
  store_type: 'file',
  root: undefined,
  sync: true,
  io_size: 1024,
  direct_io: false,
  bucket: undefined,
  endpoint: undefined,
  region: 'us-east-1',
  prefix: undefined,
  access_key: undefined,
  secret_key: undefined,
  max_retries: 3,
  connect_timeout_secs: 3,
  read_timeout_secs: 0,
  retry_mode: 'standard',
})

const canSubmit = computed(() => {
  if (!form.value.name) return false
  if (form.value.store_type === 'file' && !form.value.root) return false
  if (form.value.store_type === 's3' && !form.value.bucket) return false
  return !submitting.value
})

async function submit() {
  if (!canSubmit.value) return

  submitting.value = true
  error.value = ''
  try {
    const req: CreateStoreRequest = {
      name: form.value.name,
      store_type: form.value.store_type,
    }
    if (form.value.store_type === 'file') {
      req.root = form.value.root
      req.sync = form.value.sync
      req.io_size = form.value.io_size
      req.direct_io = form.value.direct_io
    } else {
      req.bucket = form.value.bucket
      req.endpoint = form.value.endpoint || undefined
      req.region = form.value.region
      req.prefix = form.value.prefix || undefined
      req.access_key = form.value.access_key || undefined
      req.secret_key = form.value.secret_key || undefined
      req.max_retries = form.value.max_retries
      req.connect_timeout_secs = form.value.connect_timeout_secs
      req.read_timeout_secs = form.value.read_timeout_secs
      req.retry_mode = form.value.retry_mode
    }
    await api.createStore(req)
    router.push({ name: 'stores' })
  } catch (e: any) {
    error.value = e.message
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <section>
    <PageHeader title="Create Store" />

    <p v-if="error" class="error">{{ error }}</p>

    <form class="create-form" @submit.prevent="submit">
      <div class="form-group">
        <label for="name">Name <span class="required">*</span></label>
        <input id="name" v-model="form.name" required placeholder="my-store" />
      </div>

      <div class="form-group">
        <label for="type">Type <span class="required">*</span></label>
        <select id="type" v-model="form.store_type">
          <option value="file">file</option>
          <option value="s3">s3</option>
        </select>
      </div>

      <template v-if="form.store_type === 'file'">
        <div class="form-group">
          <label for="root">Root Path <span class="required">*</span></label>
          <input id="root" v-model="form.root" placeholder="/data/blobs" />
        </div>
        <div class="form-group checkbox-group">
          <label>
            <input type="checkbox" v-model="form.sync" />
            Fsync after writes
          </label>
        </div>
        <div class="form-group">
          <label for="io_size">I/O Buffer Size (KiB)</label>
          <input id="io_size" v-model.number="form.io_size" type="number" min="4" max="65536" step="4" placeholder="1024" />
        </div>
        <div class="form-group checkbox-group">
          <label>
            <input type="checkbox" v-model="form.direct_io" />
            O_DIRECT (bypass page cache)
          </label>
        </div>
      </template>

      <template v-if="form.store_type === 's3'">
        <h4 class="form-section-heading">Connection</h4>
        <div class="form-group">
          <label for="bucket">Bucket <span class="required">*</span></label>
          <input id="bucket" v-model="form.bucket" placeholder="my-bucket" />
        </div>
        <div class="form-group">
          <label for="endpoint">Endpoint</label>
          <input id="endpoint" v-model="form.endpoint" placeholder="http://minio:9000" />
          <small class="hint">Optional. Leave blank for default AWS S3.</small>
        </div>
        <div class="form-group">
          <label for="region">Region</label>
          <input id="region" v-model="form.region" placeholder="us-east-1" />
        </div>
        <div class="form-group">
          <label for="prefix">Prefix</label>
          <input id="prefix" v-model="form.prefix" placeholder="depot/" />
          <small class="hint">Optional path prefix within the bucket.</small>
        </div>

        <h4 class="form-section-heading">Credentials</h4>
        <div class="form-group">
          <label for="access_key">Access Key</label>
          <input id="access_key" v-model="form.access_key" />
          <small class="hint">Optional. Leave blank to use IAM credentials.</small>
        </div>
        <div class="form-group">
          <label for="secret_key">Secret Key</label>
          <input id="secret_key" v-model="form.secret_key" type="password" />
          <small class="hint">Optional. Leave blank to use IAM credentials.</small>
        </div>

        <h4 class="form-section-heading">Resilience</h4>
        <div class="form-group">
          <label for="retry_mode">Retry Mode</label>
          <select id="retry_mode" v-model="form.retry_mode">
            <option value="standard">standard</option>
            <option value="adaptive">adaptive</option>
          </select>
          <small class="hint">Adaptive adds client-side rate limiting for high-throughput workloads.</small>
        </div>
        <div class="form-group">
          <label for="max_retries">Max Retries</label>
          <input id="max_retries" v-model.number="form.max_retries" type="number" min="0" max="20" />
        </div>
        <div class="form-group">
          <label for="connect_timeout_secs">Connect Timeout (seconds)</label>
          <input id="connect_timeout_secs" v-model.number="form.connect_timeout_secs" type="number" min="1" max="7200" />
        </div>
        <div class="form-group">
          <label for="read_timeout_secs">Read Timeout (seconds)</label>
          <input id="read_timeout_secs" v-model.number="form.read_timeout_secs" type="number" min="0" max="7200" />
          <small class="hint">0 = disabled (recommended for S3). Only set if your backend needs an idle timeout.</small>
        </div>
      </template>

      <div class="form-actions">
        <router-link to="/stores" class="btn">Cancel</router-link>
        <button type="submit" class="btn btn-primary" :disabled="!canSubmit">
          {{ submitting ? 'Creating...' : 'Create' }}
        </button>
      </div>
    </form>
  </section>
</template>

<style scoped>
.create-form { max-width: 640px; margin: 0 auto; }
.hint {
  display: block;
  color: var(--color-text-faint);
  font-size: 0.78rem;
  margin-top: 0.2rem;
}
</style>
