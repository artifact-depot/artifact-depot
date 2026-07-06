// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
import { ref, computed, watch, onUnmounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { tokenRef, clearToken, isAdmin } from './api'
import { useEventStream } from './composables/useEventStream'
import { useHealthStore } from './stores/healthStore'
import BaseModal from './components/BaseModal.vue'

const route = useRoute()
const routerInstance = useRouter()
const healthStore = useHealthStore()
const health = computed(() => healthStore.health)
const isAuthenticated = computed(() => !!tokenRef.value)
// Hide nav entries whose every endpoint requires admin; the server still
// enforces authorization on each request.
const admin = computed(() => isAdmin())
const isLoginPage = computed(() => route.name === 'login')
const menuOpen = ref(false)
const showLicenseModal = ref(false)
const licenseText = ref<string | null>(null)

async function openLicenseModal() {
  showLicenseModal.value = true
  if (licenseText.value === null) {
    try {
      const resp = await fetch('/THIRD-PARTY-NOTICES')
      licenseText.value = await resp.text()
    } catch {
      licenseText.value = 'Failed to load third-party notices.'
    }
  }
}

const { start, stop } = useEventStream()

// Start/stop the event stream based on auth state.
watch(
  () => tokenRef.value,
  (token) => {
    if (token) start()
    else stop()
  },
  { immediate: true }
)
onUnmounted(() => stop())

routerInstance.afterEach(() => { menuOpen.value = false })

function logout() {
  clearToken()
  routerInstance.push('/login')
}
</script>

<template>
  <header v-if="!isLoginPage">
    <h1><router-link to="/"><img src="./assets/logo.svg" alt="Artifact Depot" class="header-logo" /></router-link></h1>
    <button class="menu-toggle" @click="menuOpen = !menuOpen" aria-label="Toggle menu">&#9776;</button>
    <nav v-if="isAuthenticated" :class="{ 'menu-open': menuOpen }">
      <router-link to="/repositories">Repositories</router-link>
      <router-link v-if="admin" to="/stores">Stores</router-link>
      <router-link v-if="admin" to="/users">Users</router-link>
      <router-link v-if="admin" to="/roles">Roles</router-link>
      <router-link v-if="admin" to="/settings">Settings</router-link>
      <router-link v-if="admin" to="/tasks">Tasks</router-link>
      <router-link v-if="admin" to="/backup">Backup</router-link>
      <router-link v-if="admin" to="/activity">Activity</router-link>
      <router-link to="/api-docs">API</router-link>
    </nav>
    <div class="spacer"></div>
    <a v-if="health" class="version-badge" href="https://github.com/artifact-depot" target="_blank" rel="noopener noreferrer"><img src="./assets/github-mark.svg" alt="GitHub" class="github-icon" />v{{ health.version }}</a>
    <button v-if="health" class="info-btn" @click="openLicenseModal" title="License information">i</button>
    <button v-if="isAuthenticated" class="logout-btn" @click="logout">Logout</button>
  </header>
  <BaseModal v-if="showLicenseModal" max-width="800px" :show-close="true" @close="showLicenseModal = false">
    <template #title>
      <h3>Artifact Depot <span v-if="health">v{{ health.version }}</span></h3>
    </template>
    <div class="license-body">
      <p class="license-summary">Licensed under the Apache License, Version 2.0</p>
      <h4>Third-Party Notices</h4>
      <pre v-if="licenseText" class="license-text">{{ licenseText }}</pre>
      <p v-else class="license-loading">Loading...</p>
    </div>
  </BaseModal>
  <main v-if="!isLoginPage">
    <router-view />
  </main>
  <router-view v-else />
</template>

<style>
/* Base reset, body, and :root are now in assets/variables.css + assets/base.css */
/* .modal-overlay and .modal-close are now in assets/components.css */
header {
  background: var(--color-header-bg);
  color: var(--color-header-text);
  padding: 1rem 2rem;
  display: flex;
  align-items: center;
  gap: 2rem;
}
header h1 {
  margin: 0;
  font-size: 1.25rem;
  line-height: 1;
}
header h1 a {
  color: var(--color-header-text);
  text-decoration: none;
  display: flex;
  align-items: center;
}
.header-logo {
  height: 36px;
  width: auto;
}
nav {
  display: flex;
  gap: 1.25rem;
}
nav a {
  color: var(--color-header-text-muted);
  text-decoration: none;
  font-size: 0.9rem;
  padding: 0.25rem 0;
  border-bottom: 2px solid transparent;
  transition: color 0.2s, border-color 0.2s;
}
nav a:hover {
  color: var(--color-header-text);
}
nav a.router-link-active {
  color: var(--color-header-text);
  border-bottom-color: var(--color-header-text);
}
.spacer {
  flex: 1;
}
.version-badge {
  display: inline-flex;
  align-items: center;
  gap: 0.35rem;
  font-size: 0.8rem;
  color: var(--color-header-text-muted);
  border: 1px solid var(--color-header-border);
  border-radius: 4px;
  padding: 0.2rem 0.6rem;
  text-decoration: none;
  transition: color 0.2s, border-color 0.2s;
}
.github-icon {
  width: 12px;
  height: 12px;
  opacity: 0.7;
}
.version-badge:hover .github-icon {
  opacity: 1;
}
.version-badge:hover {
  color: var(--color-header-text);
  border-color: var(--color-header-text);
}
.info-btn {
  background: transparent;
  color: var(--color-header-text-muted);
  border: 1px solid var(--color-header-border);
  border-radius: 50%;
  width: 1.5rem;
  height: 1.5rem;
  font-size: 0.75rem;
  font-style: italic;
  font-weight: 700;
  font-family: Georgia, 'Times New Roman', serif;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  transition: color 0.2s, border-color 0.2s;
}
.info-btn:hover {
  color: var(--color-header-text);
  border-color: var(--color-header-text);
}
.license-body {
  max-height: 60vh;
  overflow-y: auto;
}
.license-body h4 {
  margin-top: 1.25rem;
  margin-bottom: 0.75rem;
  border-top: 1px solid var(--color-border);
  padding-top: 1rem;
}
.license-summary {
  color: var(--color-text-secondary);
  margin: 0;
}
.license-text {
  font-family: var(--font-mono);
  font-size: 0.75rem;
  line-height: 1.4;
  white-space: pre-wrap;
  word-wrap: break-word;
  background: var(--color-bg-subtle);
  border: 1px solid var(--color-border);
  border-radius: 4px;
  padding: 1rem;
  max-height: 50vh;
  overflow-y: auto;
  margin: 0;
}
.license-loading {
  color: var(--color-text-faint);
  font-style: italic;
}
.logout-btn {
  background: transparent;
  color: var(--color-header-text-muted);
  border: 1px solid var(--color-header-border);
  border-radius: 4px;
  padding: 0.3rem 0.8rem;
  cursor: pointer;
  font-size: 0.85rem;
  transition: color 0.2s, border-color 0.2s;
}
.logout-btn:hover {
  color: var(--color-header-text);
  border-color: var(--color-header-text);
}
main {
  padding: 2rem;
  max-width: 1800px;
  margin: 0 auto;
}
.menu-toggle {
  display: none;
  align-items: center;
  justify-content: center;
  background: transparent;
  border: 1px solid var(--color-header-border);
  border-radius: var(--radius-md);
  color: var(--color-header-text);
  font-size: 1.25rem;
  padding: 0.25rem 0.5rem;
  cursor: pointer;
}

@media (max-width: 900px) {
  header {
    flex-wrap: wrap;
    gap: 0.75rem;
    padding: 0.75rem 1rem;
  }
  nav {
    order: 3;
    width: 100%;
    flex-wrap: wrap;
    gap: 0.5rem 1rem;
  }
}

@media (max-width: 600px) {
  nav {
    display: none;
  }
  nav.menu-open {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .menu-toggle {
    display: flex;
  }
  main {
    padding: 1rem;
  }
}
</style>
