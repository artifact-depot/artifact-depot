// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { gzipSync } from 'zlib'
import { test, expect } from './fixtures'

// Build a minimal but valid Helm chart .tgz: a gzipped tar containing
// `{name}/Chart.yaml`. The server parses Chart.yaml on upload, so random bytes
// would be rejected — we need a real (if tiny) archive.
function tarHeader(name: string, size: number): Buffer {
  const h = Buffer.alloc(512)
  h.write(name, 0, 'utf8') // name (max 100 bytes)
  h.write('0000644', 100, 'ascii') // mode
  h.write('0000000', 108, 'ascii') // uid
  h.write('0000000', 116, 'ascii') // gid
  h.write(size.toString(8).padStart(11, '0'), 124, 'ascii') // size (octal, 11 + NUL)
  h.write('00000000000', 136, 'ascii') // mtime
  h.write('        ', 148, 'ascii') // checksum field = spaces while summing
  h.write('0', 156, 'ascii') // typeflag: normal file
  h.write('ustar\0', 257, 'ascii') // magic
  h.write('00', 263, 'ascii') // version
  let sum = 0
  for (let i = 0; i < 512; i++) sum += h[i]
  h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'ascii')
  return h
}

function makeChartTgz(name: string, version: string): Buffer {
  const chartYaml =
    `apiVersion: v2\nname: ${name}\nversion: ${version}\n` +
    `appVersion: "9.9"\ndescription: e2e test chart\ntype: application\n`
  const content = Buffer.from(chartYaml, 'utf8')
  const header = tarHeader(`${name}/Chart.yaml`, content.length)
  const pad = Buffer.alloc((512 - (content.length % 512)) % 512)
  const endBlocks = Buffer.alloc(1024) // two zero blocks terminate the archive
  return gzipSync(Buffer.concat([header, content, pad, endBlocks]))
}

test('helm charts-first browse: group by name, drill into versions, sha256 detail', async ({
  authedPage: page,
}) => {
  const repo = `helm-ui-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`
  const token = await page.evaluate(() => localStorage.getItem('depot_token'))
  const headers = { Authorization: `Bearer ${token}` }

  const createResp = await page.request.post('/api/v1/repositories', {
    headers,
    data: { name: repo, repo_type: 'hosted', format: 'helm', store: 'default' },
  })
  expect(createResp.ok()).toBeTruthy()

  // A dashed-name chart with two versions (the case filename-splitting breaks),
  // plus a second chart, so the default view has more than one row to group.
  const charts: [string, string][] = [
    ['myriad-uui', '1.4.2'],
    ['myriad-uui', '1.6.0-dev.5'],
    ['nginx', '1.0.0'],
  ]
  for (const [name, version] of charts) {
    const resp = await page.request.put(`/repository/${repo}/${name}-${version}.tgz`, {
      headers: { ...headers, 'Content-Type': 'application/gzip' },
      data: makeChartTgz(name, version),
    })
    expect(resp.ok(), `upload ${name}-${version}`).toBeTruthy()
  }

  // Charts-first: the default view shows chart NAMES (grouped), not 3 raw files.
  // (Cells render "📁 <name>", so match by cell role with a substring name.)
  await page.goto(`/repositories/${repo}`)
  const table = page.locator('.artifact-browser table')
  await expect(table).toBeVisible({ timeout: 10000 })
  await expect(page.getByRole('cell', { name: 'myriad-uui' })).toBeVisible()
  await expect(page.getByRole('cell', { name: 'nginx' })).toBeVisible()
  // The dashed name is grouped as ONE chart showing its version count.
  await expect(page.getByRole('cell', { name: '2 versions' })).toBeVisible()

  // Drill into the dashed-name chart → both versions listed.
  await page.getByRole('cell', { name: 'myriad-uui' }).click()
  await expect(page.getByRole('cell', { name: '1.4.2' })).toBeVisible()
  await expect(page.getByRole('cell', { name: '1.6.0-dev.5' })).toBeVisible()

  // Open a version → detail shows the SHA-256 digest (not BLAKE3).
  await page.getByRole('cell', { name: '1.4.2' }).click()
  await expect(page.getByText('Digest (SHA-256)')).toBeVisible()
  const digest = page.locator('.detail-list dd.mono').first()
  await expect(digest).toContainText('sha256:')
})
