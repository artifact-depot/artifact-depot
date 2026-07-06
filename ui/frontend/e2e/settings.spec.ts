// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { test, expect } from './fixtures'

test('settings page loads and shows all fieldsets', async ({ authedPage: page, snapshotA11y, uiScreenshot }) => {
  await page.locator('nav a', { hasText: 'Settings' }).click()
  await expect(page).toHaveURL(/\/settings/)
  // All section summaries should be visible
  await expect(page.locator('summary', { hasText: 'Upload Limit' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Garbage Collection' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Default Docker Repository' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'CORS' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Rate Limiting' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Logging Endpoints' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Tracing' })).toBeVisible()
  await expect(page.locator('summary', { hasText: 'Authentication' })).toBeVisible()

  await snapshotA11y('settings-page')
  await uiScreenshot('settings-page')
})

test('toggle CORS enables/disables sub-fields', async ({ authedPage: page, uiScreenshot }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  const corsFieldset = page.locator('.settings-section', { hasText: 'CORS' })
  // Open the collapsed section and wait for content to render
  await corsFieldset.locator('summary').click()
  await expect(corsFieldset).toHaveAttribute('open', '')
  const corsCheckbox = corsFieldset.locator('input[type="checkbox"]')
  await expect(corsCheckbox).toBeVisible()

  // Ensure CORS is disabled to start
  if (await corsCheckbox.isChecked()) {
    await corsCheckbox.uncheck()
  }

  // Sub-fields should be hidden
  await expect(corsFieldset.locator('input[type="text"]').first()).not.toBeVisible()

  // Enable CORS
  await corsCheckbox.check()
  // Origins, methods, headers, max age should appear
  await expect(corsFieldset.locator('input[type="text"]').first()).toBeVisible()
  await expect(corsFieldset.locator('input[type="number"]')).toBeVisible()
  await uiScreenshot('settings-cors-expanded')

  // Disable again
  await corsCheckbox.uncheck()
  await expect(corsFieldset.locator('input[type="text"]').first()).not.toBeVisible()
})

test('toggle rate limiting enables/disables sub-fields', async ({ authedPage: page }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  const rlFieldset = page.locator('.settings-section', { hasText: 'Rate Limiting' })
  // Open the collapsed section and wait for content to render
  await rlFieldset.locator('summary').click()
  await expect(rlFieldset).toHaveAttribute('open', '')
  const rlCheckbox = rlFieldset.locator('input[type="checkbox"]')
  await expect(rlCheckbox).toBeVisible()

  // Ensure disabled
  if (await rlCheckbox.isChecked()) {
    await rlCheckbox.uncheck()
  }

  await expect(rlFieldset.locator('input[type="number"]').first()).not.toBeVisible()

  // Enable
  await rlCheckbox.check()
  // Requests per second and burst size
  await expect(rlFieldset.locator('input[type="number"]')).toHaveCount(2)

  // Disable
  await rlCheckbox.uncheck()
  await expect(rlFieldset.locator('input[type="number"]').first()).not.toBeVisible()
})

test('toggle default docker repo enables/disables name field', async ({ authedPage: page }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  const dockerFieldset = page.locator('.settings-section', { hasText: 'Default Docker Repository' })
  // Open the collapsed section and wait for content to render
  await dockerFieldset.locator('summary').click()
  await expect(dockerFieldset).toHaveAttribute('open', '')
  const dockerCheckbox = dockerFieldset.locator('input[type="checkbox"]')
  await expect(dockerCheckbox).toBeVisible()

  // Ensure disabled
  if (await dockerCheckbox.isChecked()) {
    await dockerCheckbox.uncheck()
  }

  await expect(dockerFieldset.locator('input[type="text"]')).not.toBeVisible()

  // Enable
  await dockerCheckbox.check()
  await expect(dockerFieldset.locator('input[type="text"]')).toBeVisible()

  // Disable
  await dockerCheckbox.uncheck()
  await expect(dockerFieldset.locator('input[type="text"]')).not.toBeVisible()
})

test('save settings shows success message', async ({ authedPage: page, uiScreenshot }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  // Click Save Settings
  await page.locator('button[type="submit"]', { hasText: /save/i }).click()
  await expect(page.locator('.success', { hasText: 'Settings saved' })).toBeVisible()
  await uiScreenshot('success-message')
})

test('gc start time saves and persists across reload', async ({ authedPage: page }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  const startTime = page.locator('[data-testid="gc-start-time"]')
  await expect(startTime).toBeVisible()
  await expect(startTime).toHaveValue('') // interval mode by default

  // Set a fixed daily start time and save.
  await startTime.fill('07:00')
  await page.locator('button[type="submit"]', { hasText: /save/i }).click()
  await expect(page.locator('.success', { hasText: 'Settings saved' })).toBeVisible()

  // Round-trips through the settings API.
  await page.reload()
  await expect(page.locator('form')).toBeVisible()
  await expect(startTime).toHaveValue('07:00')

  // Clear it back to interval mode and save.
  await startTime.fill('')
  await page.locator('button[type="submit"]', { hasText: /save/i }).click()
  await expect(page.locator('.success', { hasText: 'Settings saved' })).toBeVisible()
  await page.reload()
  await expect(page.locator('form')).toBeVisible()
  await expect(startTime).toHaveValue('')
})

test('reset button restores original values', async ({ authedPage: page }) => {
  await page.goto('/settings')
  await expect(page.locator('form')).toBeVisible()

  // Record original JWT expiry value
  const jwtInput = page.locator('.settings-section', { hasText: 'Authentication' }).locator('input[type="number"]').first()
  const originalValue = await jwtInput.inputValue()

  // Change it
  await jwtInput.fill('999')
  await expect(jwtInput).toHaveValue('999')

  // Click Reset
  await page.locator('button', { hasText: 'Reset' }).click()

  // Wait for reload and check value restored
  await expect(jwtInput).toHaveValue(originalValue)
})
