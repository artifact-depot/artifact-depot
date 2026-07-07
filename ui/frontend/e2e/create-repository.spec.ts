// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { test, expect } from './fixtures'
import { deleteRepoViaApi, createRepoViaApi } from './helpers'

test('submit button is disabled when name is empty', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')
  // Name is empty by default
  const submitBtn = page.locator('button[type="submit"], .btn-primary')
  await expect(submitBtn).toBeDisabled()
})

test('submit button is disabled for cache type without upstream URL', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')
  await page.locator('#name').fill('test-cache-validation')
  await page.locator('#type').selectOption('cache')

  // upstream URL is empty - submit should be disabled
  const submitBtn = page.locator('button[type="submit"], .btn-primary')
  await expect(submitBtn).toBeDisabled()

  // Fill upstream URL - submit should become enabled
  await page.locator('#upstream').fill('https://example.com')
  await expect(submitBtn).toBeEnabled()
})

test('cache fields appear only when type is cache', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')

  // Hosted (default) - no cache fields
  await expect(page.locator('#upstream')).not.toBeVisible()
  await expect(page.locator('#ttl')).not.toBeVisible()
  await expect(page.locator('#auth_user')).not.toBeVisible()

  // Switch to cache
  await page.locator('#type').selectOption('cache')
  await expect(page.locator('#upstream')).toBeVisible()
  await expect(page.locator('#ttl')).toBeVisible()
  await expect(page.locator('#auth_user')).toBeVisible()
  await expect(page.locator('#auth_pass')).toBeVisible()

  // Switch back to hosted
  await page.locator('#type').selectOption('hosted')
  await expect(page.locator('#upstream')).not.toBeVisible()
})

test('proxy fields appear only when type is proxy', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')

  // Hosted - no proxy fields
  await expect(page.locator('#write_member')).not.toBeVisible()

  // Switch to proxy
  await page.locator('#type').selectOption('proxy')
  await expect(page.locator('#write_member')).toBeVisible()
  await expect(page.locator('.add-member')).toBeVisible()

  // Switch back
  await page.locator('#type').selectOption('hosted')
  await expect(page.locator('#write_member')).not.toBeVisible()
})

test('docker-specific fields appear for docker format', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')

  // Raw (default) - no docker fields
  await expect(page.locator('#listen')).not.toBeVisible()

  // Switch to docker
  await page.locator('#format').selectOption('docker')
  await expect(page.locator('#listen')).toBeVisible()
  await expect(page.locator('text=Clean up untagged manifests')).toBeVisible()

  // Switch back
  await page.locator('#format').selectOption('raw')
  await expect(page.locator('#listen')).not.toBeVisible()
})

test('raw content disposition field appears for raw format', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')

  // Raw is the default format - content disposition should be visible
  await expect(page.locator('#content_disposition')).toBeVisible()

  // Switch to docker - should disappear
  await page.locator('#format').selectOption('docker')
  await expect(page.locator('#content_disposition')).not.toBeVisible()

  // Switch back to raw
  await page.locator('#format').selectOption('raw')
  await expect(page.locator('#content_disposition')).toBeVisible()
})

test('create hosted raw repository successfully', async ({ authedPage: page, snapshotA11y }) => {
  const repoName = `cr-hosted-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`

  await page.goto('/repositories/new')
  await page.locator('#name').fill(repoName)
  await page.locator('#type').selectOption('hosted')
  await page.locator('#format').selectOption('raw')
  await page.locator('#store').selectOption('default')

  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(/\/repositories$/)
  await expect(page.locator('td', { hasText: repoName })).toBeVisible()

  await snapshotA11y('created-repo-in-list')

  // Clean up
  await deleteRepoViaApi(page, repoName)
})

test('create cache repository with upstream URL', async ({ authedPage: page }) => {
  const repoName = `cr-cache-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`

  await page.goto('/repositories/new')
  await page.locator('#name').fill(repoName)
  await page.locator('#type').selectOption('cache')
  await page.locator('#format').selectOption('raw')
  await page.locator('#store').selectOption('default')
  await page.locator('#upstream').fill('https://example.com/repo')
  await page.locator('#ttl').fill('7200')

  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(/\/repositories$/)
  await expect(page.locator('td', { hasText: repoName })).toBeVisible()

  // Clean up
  await deleteRepoViaApi(page, repoName)
})

test('duplicate name shows error', async ({ authedPage: page }) => {
  // Create a repo first
  const repoName = await createRepoViaApi(page)

  try {
    // Try to create another with the same name via UI
    await page.goto('/repositories/new')
    await page.locator('#name').fill(repoName)
    await page.locator('#type').selectOption('hosted')
    await page.locator('#format').selectOption('raw')
    await page.locator('#store').selectOption('default')

    await page.locator('button[type="submit"]').click()
    // Should stay on form with error
    await expect(page.locator('.error')).toBeVisible()
  } finally {
    await deleteRepoViaApi(page, repoName)
  }
})

test('cancel button returns to repository list', async ({ authedPage: page }) => {
  await page.goto('/repositories/new')
  await page.locator('a.btn', { hasText: 'Cancel' }).click()
  await expect(page).toHaveURL(/\/repositories$/)
})
