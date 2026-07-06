// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { test, expect } from './fixtures'
import { apiHeaders } from './helpers'

test('activity page streams live requests', async ({ authedPage: page }) => {
  await page.locator('nav a', { hasText: 'Activity' }).click()
  await expect(page).toHaveURL(/\/activity/)
  await expect(page.locator('[data-testid="activity-connection"]')).toHaveText('connected')

  // Generate a request through the server; it must appear as a live row.
  const headers = await apiHeaders(page)
  const marker = `/api/v1/repositories?activity-e2e=${Date.now()}`
  const resp = await page.request.get(marker, { headers })
  expect(resp.ok()).toBeTruthy()

  const table = page.locator('[data-testid="activity-table"]')
  await expect(table.locator('td', { hasText: '/api/v1/repositories' }).first()).toBeVisible()

  // Client-side filter narrows the table.
  await page.locator('[data-testid="activity-filter"]').fill('no-such-request-xyz')
  await expect(table.locator('tbody tr')).toHaveCount(0)
  await page.locator('[data-testid="activity-filter"]').fill('repositories')
  await expect(table.locator('tbody tr').first()).toBeVisible()
})

test('pause stops the feed and resume catches up', async ({ authedPage: page }) => {
  await page.goto('/activity')
  await expect(page.locator('[data-testid="activity-connection"]')).toHaveText('connected')

  await page.locator('[data-testid="activity-pause"]').click()
  await expect(page.locator('[data-testid="activity-pause"]')).toHaveText('Resume')

  // A 404 on a unique path is still a completed request with that path.
  const headers = await apiHeaders(page)
  const marker = `paused-marker-${Date.now()}`
  await page.request.get(`/api/v1/repositories/${marker}`, { headers })

  // While paused the marker row must not render...
  const table = page.locator('[data-testid="activity-table"]')
  await expect(table.locator('td', { hasText: marker })).toHaveCount(0)

  // ...and resuming replays the buffered event.
  await page.locator('[data-testid="activity-pause"]').click()
  await expect(table.locator('td', { hasText: marker }).first()).toBeVisible()
})

test('non-admin users see neither the nav entry nor the stream', async ({ authedPage: page, browser }) => {
  // Create a read-only user via the admin session.
  const headers = await apiHeaders(page)
  const username = `viewer-${Date.now()}`
  const resp = await page.request.post('/api/v1/users', {
    headers,
    data: { username, password: 'viewer-password', roles: ['read-only'] },
  })
  expect(resp.ok()).toBeTruthy()

  // Fresh session as the viewer.
  const ctx = await browser.newContext()
  const viewer = await ctx.newPage()
  await viewer.goto('/login')
  await viewer.locator('#username').fill(username)
  await viewer.locator('#password').fill('viewer-password')
  await viewer.locator('.login-btn').click()
  await viewer.waitForURL('**/repositories')

  // Admin-only nav entries are hidden; general ones remain.
  await expect(viewer.locator('nav a', { hasText: 'Repositories' })).toBeVisible()
  for (const hidden of ['Activity', 'Settings', 'Users', 'Roles', 'Backup', 'Stores', 'Tasks']) {
    await expect(viewer.locator('nav a', { hasText: hidden })).toHaveCount(0)
  }

  // Deep-linking to the page still hits the server-side admin gate.
  await viewer.goto('/activity')
  await expect(viewer.locator('.error', { hasText: 'Admin access is required' })).toBeVisible()

  await ctx.close()
})
