// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { test, expect } from './fixtures'

test('empty repository list shows table or empty state', async ({ authedPage: page, snapshotA11y, uiScreenshot }) => {
  await expect(page.locator('th', { hasText: 'Name' })).toBeVisible()
  await snapshotA11y()
  await uiScreenshot('repository-list-empty')
})

test('create, view, and delete a hosted raw repository', async ({ authedPage: page, snapshotA11y, uiScreenshot }) => {
  const repoName = `test-repo-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`

  // Navigate to create form
  await page.locator('a', { hasText: 'Create Repository' }).click()
  await expect(page).toHaveURL(/\/repositories\/new/)

  // Fill form
  await page.locator('#name').fill(repoName)
  await page.locator('#type').selectOption('hosted')
  await page.locator('#format').selectOption('raw')
  await page.locator('#store').selectOption('default')

  // Submit
  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(/\/repositories$/)

  // Verify repo appears in list
  await expect(page.locator('td', { hasText: repoName })).toBeVisible()
  await snapshotA11y('list-with-repo')
  await uiScreenshot('repository-list')

  // Click to view detail
  await page.locator('td', { hasText: repoName }).click()
  await expect(page).toHaveURL(new RegExp(`/repositories/${repoName}`))
  await expect(page.locator('.detail-card')).toBeVisible()
  await snapshotA11y('detail')

  // Delete the repo via the Manage tab on the detail page
  await page.locator('.tab-bar button', { hasText: 'Manage' }).click()
  await page.locator('.danger-zone button', { hasText: 'Delete Repository' }).click()
  await page.locator('.modal .btn-danger', { hasText: /delete/i }).click()

  // Should redirect back to list with repo gone
  await expect(page).toHaveURL(/\/repositories$/)
  await expect(page.locator('td', { hasText: repoName })).not.toBeVisible()
})

test('stats header shows pie chart and summary cards when repos exist', async ({ authedPage: page, uiScreenshot }) => {
  const repoName = `stats-repo-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`
  const token = await page.evaluate(() => localStorage.getItem('depot_token'))

  // Create a repo via API so the list is non-empty.
  await page.request.post('/api/v1/repositories', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { name: repoName, repo_type: 'hosted', format: 'raw', store: 'default' },
  })
  try {
    await page.reload()
    await expect(page.locator('.stats-header .pie-card')).toBeVisible({ timeout: 10000 })
    await expect(page.locator('.stat-label', { hasText: 'Total Artifacts' })).toBeVisible()
    await expect(page.locator('.stat-label', { hasText: 'Total Size' })).toBeVisible()
    await expect(page.locator('.stat-label', { hasText: 'Repositories' })).toBeVisible()
    await uiScreenshot('repositories-with-stats-header')
  } finally {
    await page.request.delete(`/api/v1/repositories/${repoName}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
  }
})

test('proxy repos are excluded from Repositories page storage totals', async ({ authedPage: page }) => {
  const suffix = `${Date.now()}-${Math.random().toString(36).slice(2, 6)}`
  const memberName = `ddmem-${suffix}`
  const proxyName = `ddgrp-${suffix}`
  const token = await page.evaluate(() => localStorage.getItem('depot_token'))

  await page.request.post('/api/v1/repositories', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { name: memberName, repo_type: 'hosted', format: 'raw', store: 'default' },
  })
  await page.request.post('/api/v1/repositories', {
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    data: { name: proxyName, repo_type: 'proxy', format: 'raw', store: 'default', members: [memberName] },
  })
  try {
    // Upload so the proxy has a non-zero aggregated byte count -- otherwise it
    // would be filtered from the pie chart just because its total_bytes is 0.
    await page.request.put(`/repository/${memberName}/some/file.bin`, {
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/octet-stream' },
      data: 'x'.repeat(100),
    })

    // Wait for both rows to reflect the upload. The proxy's count is
    // aggregated live from members, so once the member shows 1 the proxy does too.
    await expect.poll(async () => {
      const resp = await page.request.get('/api/v1/repositories', {
        headers: { Authorization: `Bearer ${token}` },
      })
      const repos = await resp.json()
      const member = repos.find((r: { name: string }) => r.name === memberName)
      const proxy = repos.find((r: { name: string }) => r.name === proxyName)
      return member?.artifact_count === 1 && proxy?.artifact_count === 1
    }, { timeout: 15000 }).toBeTruthy()

    await page.reload()
    await expect(page.locator('.stats-header .pie-card')).toBeVisible({ timeout: 10000 })

    // Sanity: the member shows up in the table and in the pie legend.
    await expect(page.locator('tbody tr td strong', { hasText: new RegExp(`^${memberName}$`) })).toBeVisible()
    // The legend entries render a beat after the .pie-card container (chart
    // layout runs once the repo data has loaded), so under parallel load this
    // can lag past Playwright's 5s default. Give it the same budget as the
    // card above. This also gates the proxy-absence check below: the legend
    // must be populated before toHaveCount(0) is a meaningful assertion.
    await expect(page.locator('.pie-container .legend-name', { hasText: new RegExp(`^${memberName}$`) })).toBeVisible({ timeout: 10000 })

    // Core assertion: the proxy is visible in the table (it is a repository),
    // but MUST NOT appear as a pie slice -- otherwise its bytes would be
    // double-counted alongside the member's. This also implicitly validates
    // that the Total Artifacts / Total Size cards exclude proxies, since
    // they share the same computed filter in repoStore.ts.
    await expect(page.locator('tbody tr td strong', { hasText: new RegExp(`^${proxyName}$`) })).toBeVisible()
    await expect(page.locator('.pie-container .legend-name', { hasText: new RegExp(`^${proxyName}$`) })).toHaveCount(0)
  } finally {
    await page.request.delete(`/api/v1/repositories/${proxyName}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    await page.request.delete(`/api/v1/repositories/${memberName}`, {
      headers: { Authorization: `Bearer ${token}` },
    })
  }
})

test('create repository link navigates to form', async ({ authedPage: page, uiScreenshot }) => {
  await page.locator('a', { hasText: 'Create Repository' }).click()
  await expect(page).toHaveURL(/\/repositories\/new/)
  await expect(page.locator('h2', { hasText: 'Create Repository' })).toBeVisible()
  await uiScreenshot('repository-create-form')
})

test('clicking repo row navigates to detail', async ({ authedPage: page }) => {
  const repoName = `nav-test-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`

  // Create repo via UI
  await page.locator('a', { hasText: 'Create Repository' }).click()
  await page.locator('#name').fill(repoName)
  await page.locator('#type').selectOption('hosted')
  await page.locator('#format').selectOption('raw')
  await page.locator('#store').selectOption('default')
  await page.locator('button[type="submit"]').click()
  await expect(page).toHaveURL(/\/repositories$/)

  // Click the repo row
  await page.locator('td', { hasText: repoName }).click()
  await expect(page).toHaveURL(new RegExp(`/repositories/${repoName}`))
  await expect(page.locator('.detail-card')).toBeVisible()

  // Clean up
  const token = await page.evaluate(() => localStorage.getItem('depot_token'))
  await page.request.delete(`/api/v1/repositories/${repoName}`, {
    headers: { Authorization: `Bearer ${token}` },
  })
})
