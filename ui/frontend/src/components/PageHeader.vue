// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

<script setup lang="ts">
defineProps<{
  title?: string
}>()
</script>

<template>
  <div class="page-header">
    <h2 v-if="title">{{ title }}</h2>
    <slot name="badges" />
    <div v-if="$slots.actions" class="spacer"></div>
    <slot name="actions" />
  </div>
</template>

<style scoped>
.spacer {
  flex: 1;
}
</style>
