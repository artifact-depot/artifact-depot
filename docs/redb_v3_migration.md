---
title: redb v3 file format migration
nav_order: 8
---

<!--
SPDX-FileCopyrightText: 2026 Artifact Depot Contributors

SPDX-License-Identifier: Apache-2.0
-->

# redb v3 file format migration

Single-node depot stores its KV data in [redb](https://github.com/cberner/redb).
redb 2.x wrote its **v2** file format by default; redb 3.0 and later read
**only v3**. Depot is migrating in two phases so the crate can eventually
move past 2.6 without stranding deployed databases. DynamoDB-backed
deployments are unaffected.

## Phase 1 (this release): automatic in-place upgrade

On startup, depot now:

- creates **new** store files directly in the v3 format, and
- converts existing v2 files to v3 **in place** the first time they are
  opened (`Database::upgrade()`), logging
  `redb file upgraded to the v3 format` once per shard file.

The conversion is transactional metadata work, not a full rewrite — but on a
large store the first boot after upgrading may take noticeably longer than
usual. Plan the same way as any depot upgrade:

1. Take a copy of the redb directory while depot is stopped (it is the real
   metadata backup; the backup API covers configuration only).
2. Start the new build. Watch for the upgrade log line; the service becomes
   ready when the normal startup completes.
3. **There is no downgrade.** A build older than this release can still read
   the upgraded file only if it is redb 2.6-based; anything using an older
   redb cannot. The pre-upgrade copy from step 1 is the rollback path.

## Phase 2 (future release): redb 4

Once every deployment has booted at least once on a Phase 1 build, the redb
dependency can move to 4.x, which cannot open v2 files at all. That bump is
deliberately a separate release — **do not skip Phase 1**: a v2 store opened
by a redb 4 build fails to start (the data file is intact, but the service
will not come up until an intermediate 2.6-based build performs the
conversion).

Tracking: [#56](https://github.com/artifact-depot/artifact-depot/issues/56).
