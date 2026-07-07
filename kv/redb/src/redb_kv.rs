// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;

use depot_core::error::{self, DepotError, Retryability};
use depot_core::store::kv::ScanResult;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum number of write operations to coalesce into a single transaction.
/// Under load, the channel naturally accumulates operations while the
/// previous batch commits, so no intentional delay is needed.
const BATCH_CAP: usize = 64;

/// Envelope format tag for the current version.
const ENVELOPE_V1: u8 = 0x01;
/// Size of the envelope header: 1 byte format + 8 bytes version.
const ENVELOPE_HEADER_SIZE: usize = 9;

// ---------------------------------------------------------------------------
// Writer-thread protocol
// ---------------------------------------------------------------------------

enum WriteOp {
    Put {
        table: String,
        key: Vec<u8>,
        value: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<error::Result<()>>,
    },
    PutIfVersion {
        table: String,
        key: Vec<u8>,
        value: Vec<u8>,
        expected_version: Option<u64>,
        reply: tokio::sync::oneshot::Sender<error::Result<bool>>,
    },
    Delete {
        table: String,
        key: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<error::Result<bool>>,
    },
    DeleteReturning {
        table: String,
        key: Vec<u8>,
        reply: tokio::sync::oneshot::Sender<error::Result<Option<Vec<u8>>>>,
    },
    DeleteIfVersion {
        table: String,
        key: Vec<u8>,
        expected_version: u64,
        reply: tokio::sync::oneshot::Sender<error::Result<bool>>,
    },
    DeleteBatch {
        table: String,
        keys: Vec<Vec<u8>>,
        reply: tokio::sync::oneshot::Sender<error::Result<Vec<bool>>>,
    },
    PutBatch {
        table: String,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        reply: tokio::sync::oneshot::Sender<error::Result<()>>,
    },
}

pub struct RedbKvStore {
    /// Holds the Database and writer task handle. Read-path ops take the
    /// read lock; compact takes the write lock and briefly swaps the
    /// Database out to call `redb::Database::compact(&mut self)`.
    inner: tokio::sync::RwLock<Inner>,
    /// Kept outside the RwLock so `Drop` can synchronously take the
    /// sender and let the writer task exit without needing an async
    /// context.
    write_tx: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<WriteOp>>>,
    /// Path to the backing file, retained so compaction can report
    /// before/after file sizes in its telemetry.
    path: PathBuf,
}

struct Inner {
    /// `Some` except transiently during `compact()` (under the write lock)
    /// when the Database is moved into a `spawn_blocking` task.
    db: Option<Arc<Database>>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RedbKvStore {
    fn drop(&mut self) {
        // Drop the sender to close the channel — the writer task will see
        // `None` from recv() and exit after draining any in-flight batch,
        // allowing redb to shut down cleanly without WAL recovery on restart.
        //
        // Uses the std Mutex so Drop doesn't need to be async. Handle is
        // not awaited here (best-effort); callers that need a clean
        // handoff should call `close().await` first.
        if let Ok(mut guard) = self.write_tx.lock() {
            guard.take();
        }
    }
}

impl RedbKvStore {
    pub fn open(path: &Path) -> error::Result<Self> {
        Self::open_with_cache(path, 0)
    }

    pub fn open_with_cache(path: &Path, cache_bytes: usize) -> error::Result<Self> {
        // New files are created in the v3 format; existing v2 files (redb 2.x
        // created v2 by default, and depot never upgraded) are converted in
        // place on open. redb 3.0+ reads ONLY v3, so every deployed store
        // must have booted once through this path before the crate can move
        // past 2.6 — see docs/redb_v3_migration.md.
        let mut builder = Database::builder();
        if cache_bytes > 0 {
            builder.set_cache_size(cache_bytes);
        }
        builder.create_with_file_format_v3(true);
        let mut db = builder
            .create(path)
            .map_err(DepotError::storage_permanent)?;
        match db.upgrade() {
            Ok(true) => {
                tracing::info!(path = %path.display(), "redb file upgraded to the v3 format")
            }
            Ok(false) => {}
            // Savepoints or open read transactions block the upgrade; depot
            // creates neither, and nothing else has the file open yet.
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "redb v3 upgrade failed");
                return Err(DepotError::storage_permanent(e));
            }
        }

        // Pre-create all known tables so reads never hit TableDoesNotExist.
        let txn = db.begin_write().map_err(DepotError::storage_permanent)?;
        for name in depot_core::store::keys::ALL_TABLES {
            let def = TableDefinition::<&[u8], &[u8]>::new(name);
            let _ = txn.open_table(def).map_err(DepotError::storage_permanent)?;
        }
        txn.commit().map_err(DepotError::storage_permanent)?;

        let db = Arc::new(db);
        let writer_db = Arc::clone(&db);

        let (write_tx, write_rx) = tokio::sync::mpsc::channel::<WriteOp>(4096);

        let writer_handle = tokio::spawn(writer_task(writer_db, write_rx));

        Ok(Self {
            inner: tokio::sync::RwLock::new(Inner {
                db: Some(db),
                writer_handle: Some(writer_handle),
            }),
            write_tx: std::sync::Mutex::new(Some(write_tx)),
            path: path.to_path_buf(),
        })
    }

    /// Gracefully shut down the writer task and wait for it to finish.
    /// Must be called from an async context. This ensures the redb database
    /// file lock is released before the store is re-opened.
    pub async fn close(&mut self) {
        if let Ok(mut guard) = self.write_tx.lock() {
            guard.take();
        }
        let mut inner = self.inner.write().await;
        if let Some(h) = inner.writer_handle.take() {
            let _ = h.await;
        }
    }

    /// Send a write operation and await its result.
    ///
    /// Holds the inner read lock across the await so compact (which takes
    /// the write lock) cannot run until all in-flight writes have drained.
    /// Keeps `try_send` (non-blocking) to avoid a deadlock where a full
    /// channel would block under the read lock while compact waits for
    /// the write lock.
    async fn send_write<T: Send + 'static>(
        &self,
        op_fn: impl FnOnce(tokio::sync::oneshot::Sender<error::Result<T>>) -> WriteOp,
    ) -> error::Result<T> {
        let _inner = self.inner.read().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let op = op_fn(tx);
        let sender = self.write_tx.lock().ok().and_then(|guard| guard.clone());
        let sent = sender.map(|s| s.try_send(op).is_ok()).unwrap_or(false);
        if !sent {
            return Err(DepotError::Storage(
                "redb writer channel full or closed".into(),
                Retryability::Permanent,
            ));
        }
        rx.await.map_err(|_| storage_err("writer dropped".into()))?
    }
}

/// Async writer task.  Receives write ops via a tokio channel (no kernel
/// futex on the send path), batches them, and executes the redb transaction
/// on a blocking thread.
///
/// Batching is opportunistic: execute immediately with whatever is available,
/// no intentional delay.  While the transaction commits on the blocking
/// thread, new ops accumulate in the channel and form the next batch
/// naturally.
async fn writer_task(db: Arc<Database>, mut rx: tokio::sync::mpsc::Receiver<WriteOp>) {
    loop {
        // Wait for at least one operation (no busy-spin when idle).
        let first = match rx.recv().await {
            Some(op) => op,
            None => return, // Channel closed — shut down.
        };

        let mut batch = Vec::with_capacity(BATCH_CAP);
        batch.push(first);

        // Drain whatever else is already queued, up to BATCH_CAP.
        while batch.len() < BATCH_CAP {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }

        let db = Arc::clone(&db);
        // Run the redb transaction on a blocking thread so we don't
        // block the tokio runtime during the (synchronous) commit.
        let _ = tokio::task::spawn_blocking(move || execute_batch(&db, batch)).await;
    }
}

/// Execute a batch of write operations in a single redb write transaction.
fn execute_batch(db: &Database, batch: Vec<WriteOp>) {
    // Attempt to run all operations in one transaction.
    let txn = match db.begin_write() {
        Ok(t) => t,
        Err(e) => {
            let err_msg = e.to_string();
            send_errors(batch, &err_msg);
            return;
        }
    };

    // Collect per-operation results while the transaction is open.
    let mut results: Vec<OpResult> = Vec::with_capacity(batch.len());

    // Helper macro to open table by name within the transaction.
    macro_rules! open_table {
        ($table_name:expr, $err_variant:ident) => {
            match txn.open_table(TableDefinition::<&[u8], &[u8]>::new($table_name)) {
                Ok(t) => t,
                Err(e) => {
                    results.push(OpResult::$err_variant(Err(e.to_string())));
                    continue;
                }
            }
        };
    }

    for op in &batch {
        let r = match op {
            WriteOp::Put {
                table, key, value, ..
            } => {
                let mut t = open_table!(table, Unit);
                // Read current version, increment, wrap with envelope.
                let current_version = match t.get(key.as_slice()) {
                    Ok(Some(v)) => read_version(v.value()),
                    Ok(None) => 0,
                    Err(e) => {
                        results.push(OpResult::Unit(Err(e.to_string())));
                        continue;
                    }
                };
                let wrapped = wrap_envelope(current_version + 1, value);
                let r = match t.insert(key.as_slice(), wrapped.as_slice()) {
                    Ok(_) => OpResult::Unit(Ok(())),
                    Err(e) => OpResult::Unit(Err(e.to_string())),
                };
                r
            }
            WriteOp::PutIfVersion {
                table,
                key,
                value,
                expected_version,
                ..
            } => {
                let mut t = open_table!(table, Bool);
                // Read current version, check against expected.
                let current_version = match t.get(key.as_slice()) {
                    Ok(Some(v)) => Some(read_version(v.value())),
                    Ok(None) => None,
                    Err(e) => {
                        results.push(OpResult::Bool(Err(e.to_string())));
                        continue;
                    }
                };

                let ok = match (*expected_version, current_version) {
                    (None, None) => true,
                    (None, Some(_)) => false,
                    (Some(exp), Some(act)) if exp == act => true,
                    (Some(_), _) => false,
                };

                if !ok {
                    OpResult::Bool(Ok(false))
                } else {
                    let new_version = expected_version.unwrap_or(0) + 1;
                    let wrapped = wrap_envelope(new_version, value);
                    let r = match t.insert(key.as_slice(), wrapped.as_slice()) {
                        Ok(_) => OpResult::Bool(Ok(true)),
                        Err(e) => OpResult::Bool(Err(e.to_string())),
                    };
                    r
                }
            }
            WriteOp::Delete { table, key, .. } => {
                let mut t = open_table!(table, Bool);
                let r = match t.remove(key.as_slice()) {
                    Ok(guard) => OpResult::Bool(Ok(guard.is_some())),
                    Err(e) => OpResult::Bool(Err(e.to_string())),
                };
                r
            }
            WriteOp::DeleteIfVersion {
                table,
                key,
                expected_version,
                ..
            } => {
                let mut t = open_table!(table, Bool);
                let current_version = match t.get(key.as_slice()) {
                    Ok(Some(v)) => Some(read_version(v.value())),
                    Ok(None) => None,
                    Err(e) => {
                        results.push(OpResult::Bool(Err(e.to_string())));
                        continue;
                    }
                };

                if current_version != Some(*expected_version) {
                    OpResult::Bool(Ok(false))
                } else {
                    match t.remove(key.as_slice()) {
                        Ok(guard) => OpResult::Bool(Ok(guard.is_some())),
                        Err(e) => OpResult::Bool(Err(e.to_string())),
                    }
                }
            }
            WriteOp::DeleteReturning { table, key, .. } => {
                let mut t = open_table!(table, OptBytes);
                let result = match t.remove(key.as_slice()) {
                    Ok(Some(guard)) => {
                        let raw = guard.value();
                        let (payload, _version) = unwrap_envelope(raw);
                        Ok(Some(payload.to_vec()))
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e.to_string()),
                };
                OpResult::OptBytes(result)
            }
            WriteOp::DeleteBatch { table, keys, .. } => {
                let mut t = open_table!(table, Bools);
                let mut existed = Vec::with_capacity(keys.len());
                let mut err = None;
                for key in keys {
                    match t.remove(key.as_slice()) {
                        Ok(guard) => existed.push(guard.is_some()),
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => OpResult::Bools(Err(e)),
                    None => OpResult::Bools(Ok(existed)),
                }
            }
            WriteOp::PutBatch { table, entries, .. } => {
                let mut t = open_table!(table, Unit);
                let mut err = None;
                for (key, value) in entries {
                    // Read current version, increment, wrap with envelope.
                    let current_version = match t.get(key.as_slice()) {
                        Ok(Some(v)) => read_version(v.value()),
                        Ok(None) => 0,
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    };
                    let wrapped = wrap_envelope(current_version + 1, value);
                    if let Err(e) = t.insert(key.as_slice(), wrapped.as_slice()) {
                        err = Some(e.to_string());
                        break;
                    }
                }
                match err {
                    Some(e) => OpResult::Unit(Err(e)),
                    None => OpResult::Unit(Ok(())),
                }
            }
        };
        results.push(r);
    }

    if let Err(e) = txn.commit() {
        let err_msg = e.to_string();
        send_errors(batch, &err_msg);
        return;
    }

    // Send individual results back.
    for (op, result) in batch.into_iter().zip(results) {
        match (op, result) {
            (WriteOp::Put { reply, .. }, OpResult::Unit(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::PutIfVersion { reply, .. }, OpResult::Bool(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::Delete { reply, .. }, OpResult::Bool(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::DeleteIfVersion { reply, .. }, OpResult::Bool(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::DeleteReturning { reply, .. }, OpResult::OptBytes(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::DeleteBatch { reply, .. }, OpResult::Bools(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            (WriteOp::PutBatch { reply, .. }, OpResult::Unit(r)) => {
                let _ = reply.send(r.map_err(storage_err));
            }
            _ => {
                tracing::error!("mismatched op/result types in write batch");
            }
        }
    }
}

enum OpResult {
    Unit(Result<(), String>),
    Bool(Result<bool, String>),
    Bools(Result<Vec<bool>, String>),
    OptBytes(Result<Option<Vec<u8>>, String>),
}

fn storage_err(msg: String) -> DepotError {
    DepotError::Storage(msg.into(), Retryability::Permanent)
}

/// Built when a read/write path observes `Inner::db == None` — impossible
/// outside of compaction, and compaction holds the write lock that excludes
/// readers/writers. Surfaced as a transient error rather than a panic so the
/// caller can retry if the invariant ever slips.
fn db_missing() -> DepotError {
    DepotError::Storage(
        "redb database unavailable (compaction in progress)".into(),
        Retryability::Transient,
    )
}

fn send_errors(batch: Vec<WriteOp>, err_msg: &str) {
    for op in batch {
        match op {
            WriteOp::Put { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::PutIfVersion { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::Delete { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::DeleteIfVersion { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::DeleteReturning { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::DeleteBatch { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
            WriteOp::PutBatch { reply, .. } => {
                let _ = reply.send(Err(storage_err(err_msg.to_string())));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Flat-key helpers
// ---------------------------------------------------------------------------

/// Build the internal flat key from (pk, sk).
/// Table dimension is now handled by separate redb tables.
fn flat_key(pk: &str, sk: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(pk.len() + 1 + sk.len());
    k.extend_from_slice(pk.as_bytes());
    k.push(0x00);
    k.extend_from_slice(sk.as_bytes());
    k
}

/// Compute the prefix for scanning all sort keys within a partition.
fn partition_prefix(pk: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(pk.len() + 1);
    k.extend_from_slice(pk.as_bytes());
    k.push(0x00);
    k
}

use depot_core::store::keys::prefix_end as prefix_range_end;

/// Extract the sort key from a flat key given the known partition prefix length.
fn extract_sk(flat: &[u8], prefix_len: usize) -> String {
    let bytes = flat.get(prefix_len..).unwrap_or(&[]);
    String::from_utf8(bytes.to_vec()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Envelope helpers
// ---------------------------------------------------------------------------

/// Wrap payload bytes with the envelope header: [format][version LE].
fn wrap_envelope(version: u64, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ENVELOPE_HEADER_SIZE + payload.len());
    buf.push(ENVELOPE_V1);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Unwrap envelope, returning (payload, version).
/// Returns the raw bytes and version 0 if no envelope header is present (shouldn't happen).
fn unwrap_envelope(raw: &[u8]) -> (&[u8], u64) {
    #[allow(clippy::indexing_slicing)] // all slices guarded by raw.len() >= ENVELOPE_HEADER_SIZE
    if raw.len() >= ENVELOPE_HEADER_SIZE && raw[0] == ENVELOPE_V1 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&raw[1..9]);
        let version = u64::from_le_bytes(bytes);
        (&raw[ENVELOPE_HEADER_SIZE..], version)
    } else {
        // No envelope — treat as version 0 (pre-envelope data).
        (raw, 0)
    }
}

/// Read the version from an existing raw value, or 0 if not present.
fn read_version(raw: &[u8]) -> u64 {
    unwrap_envelope(raw).1
}

// ---------------------------------------------------------------------------
// KvStore implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl depot_core::store::kv::KvStore for RedbKvStore {
    async fn get(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<Vec<u8>>> {
        let key = flat_key(&pk, &sk);
        let inner = self.inner.read().await;
        let db = inner.db.as_ref().ok_or_else(db_missing)?;
        let txn = db.begin_read().map_err(DepotError::storage_permanent)?;
        let t = txn
            .open_table(TableDefinition::<&[u8], &[u8]>::new(table))
            .map_err(DepotError::storage_permanent)?;
        let r = t
            .get(key.as_slice())
            .map_err(DepotError::storage_permanent)?
            .map(|v| unwrap_envelope(v.value()).0.to_vec());
        Ok(r)
    }

    async fn get_versioned(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<(Vec<u8>, u64)>> {
        let key = flat_key(&pk, &sk);
        let inner = self.inner.read().await;
        let db = inner.db.as_ref().ok_or_else(db_missing)?;
        let txn = db.begin_read().map_err(DepotError::storage_permanent)?;
        let t = txn
            .open_table(TableDefinition::<&[u8], &[u8]>::new(table))
            .map_err(DepotError::storage_permanent)?;
        let r = t
            .get(key.as_slice())
            .map_err(DepotError::storage_permanent)?
            .map(|v| {
                let (payload, version) = unwrap_envelope(v.value());
                (payload.to_vec(), version)
            });
        Ok(r)
    }

    async fn put(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        value: &[u8],
    ) -> error::Result<()> {
        let table = table.to_string();
        let key = flat_key(&pk, &sk);
        let value = value.to_vec();
        self.send_write(|reply| WriteOp::Put {
            table,
            key,
            value,
            reply,
        })
        .await
    }

    async fn put_if_version(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        value: &[u8],
        expected_version: Option<u64>,
    ) -> error::Result<bool> {
        let table = table.to_string();
        let key = flat_key(&pk, &sk);
        let value = value.to_vec();
        self.send_write(|reply| WriteOp::PutIfVersion {
            table,
            key,
            value,
            expected_version,
            reply,
        })
        .await
    }

    async fn delete(&self, table: &str, pk: Cow<'_, str>, sk: Cow<'_, str>) -> error::Result<bool> {
        let table = table.to_string();
        let key = flat_key(&pk, &sk);
        self.send_write(|reply| WriteOp::Delete { table, key, reply })
            .await
    }

    async fn delete_if_version(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        expected_version: u64,
    ) -> error::Result<bool> {
        let table = table.to_string();
        let key = flat_key(&pk, &sk);
        self.send_write(|reply| WriteOp::DeleteIfVersion {
            table,
            key,
            expected_version,
            reply,
        })
        .await
    }

    async fn delete_returning(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<Vec<u8>>> {
        let table = table.to_string();
        let key = flat_key(&pk, &sk);
        self.send_write(|reply| WriteOp::DeleteReturning { table, key, reply })
            .await
    }

    async fn delete_batch(&self, table: &str, keys: &[(&str, &str)]) -> error::Result<Vec<bool>> {
        if keys.is_empty() {
            return Ok(vec![]);
        }
        let table = table.to_string();
        let flat_keys: Vec<Vec<u8>> = keys.iter().map(|(pk, sk)| flat_key(pk, sk)).collect();
        self.send_write(|reply| WriteOp::DeleteBatch {
            table,
            keys: flat_keys,
            reply,
        })
        .await
    }

    async fn put_batch(&self, table: &str, entries: &[(&str, &str, &[u8])]) -> error::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let table = table.to_string();
        let flat_entries: Vec<(Vec<u8>, Vec<u8>)> = entries
            .iter()
            .map(|(pk, sk, value)| (flat_key(pk, sk), value.to_vec()))
            .collect();
        self.send_write(|reply| WriteOp::PutBatch {
            table,
            entries: flat_entries,
            reply,
        })
        .await
    }

    async fn scan_prefix(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk_prefix: Cow<'_, str>,
        limit: usize,
    ) -> error::Result<ScanResult> {
        let prefix = {
            let mut p = partition_prefix(&pk);
            p.extend_from_slice(sk_prefix.as_bytes());
            p
        };
        let part_prefix_len = partition_prefix(&pk).len();
        let inner = self.inner.read().await;
        let db = inner.db.as_ref().ok_or_else(db_missing)?;
        let txn = db.begin_read().map_err(DepotError::storage_permanent)?;
        let t = txn
            .open_table(TableDefinition::<&[u8], &[u8]>::new(table))
            .map_err(DepotError::storage_permanent)?;
        let mut results = Vec::new();
        let mut hit_limit = false;
        let end = prefix_range_end(&prefix);
        let range = if let Some(ref end_bytes) = end {
            t.range::<&[u8]>(prefix.as_slice()..end_bytes.as_slice())
                .map_err(DepotError::storage_permanent)?
        } else {
            t.range::<&[u8]>(prefix.as_slice()..)
                .map_err(DepotError::storage_permanent)?
        };
        for entry in range {
            if results.len() >= limit {
                hit_limit = true;
                break;
            }
            let (k, v) = entry.map_err(DepotError::storage_permanent)?;
            let flat = k.value();
            if !flat.starts_with(&prefix) {
                break;
            }
            let sk = extract_sk(flat, part_prefix_len);
            let (payload, _version) = unwrap_envelope(v.value());
            results.push((sk, payload.to_vec()));
        }
        Ok(ScanResult {
            items: results,
            done: !hit_limit,
        })
    }

    async fn scan_range(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk_start: Cow<'_, str>,
        sk_end: Option<&str>,
        limit: usize,
    ) -> error::Result<ScanResult> {
        let start = flat_key(&pk, &sk_start);
        let part_prefix_len = partition_prefix(&pk).len();
        let end = match sk_end {
            Some(end_str) => Some(flat_key(&pk, end_str)),
            None => {
                // Open-ended: scan to end of partition.
                prefix_range_end(&partition_prefix(&pk))
            }
        };
        let inner = self.inner.read().await;
        let db = inner.db.as_ref().ok_or_else(db_missing)?;
        let txn = db.begin_read().map_err(DepotError::storage_permanent)?;
        let t = txn
            .open_table(TableDefinition::<&[u8], &[u8]>::new(table))
            .map_err(DepotError::storage_permanent)?;
        let mut results = Vec::new();
        let mut hit_limit = false;
        let range = if let Some(ref end_bytes) = end {
            t.range::<&[u8]>(start.as_slice()..end_bytes.as_slice())
                .map_err(DepotError::storage_permanent)?
        } else {
            t.range::<&[u8]>(start.as_slice()..)
                .map_err(DepotError::storage_permanent)?
        };
        for entry in range {
            if results.len() >= limit {
                hit_limit = true;
                break;
            }
            let (k, v) = entry.map_err(DepotError::storage_permanent)?;
            let sk = extract_sk(k.value(), part_prefix_len);
            let (payload, _version) = unwrap_envelope(v.value());
            results.push((sk, payload.to_vec()));
        }
        Ok(ScanResult {
            items: results,
            done: !hit_limit,
        })
    }

    fn is_single_node(&self) -> bool {
        true
    }

    async fn compact(&self) -> error::Result<bool> {
        // Hold the write lock for the full operation: excludes all reads
        // and writes so no redb ReadTransaction or WriteTransaction is live
        // when `Database::compact` runs. Also guarantees no writer-task
        // batch is in flight across the restart boundary.
        let mut inner = self.inner.write().await;

        // Drop the sender and await the writer task so its `Arc<Database>`
        // clone is released, making the main Arc the sole owner.
        if let Ok(mut guard) = self.write_tx.lock() {
            guard.take();
        }
        if let Some(h) = inner.writer_handle.take() {
            let _ = h.await;
        }

        let size_before = std::fs::metadata(&self.path).map(|m| m.len()).ok();

        // Move the Database out of the Arc so we can pass it by owned
        // value into spawn_blocking (compact is synchronous and can take
        // seconds on large files).
        let db_arc = inner.db.take().ok_or_else(db_missing)?;
        let db = Arc::try_unwrap(db_arc)
            .map_err(|_| storage_err("unexpected extra Arc<Database> owners".into()))?;

        let (compact_res, db) = tokio::task::spawn_blocking(move || {
            let mut db = db;
            let res = db.compact();
            (res, db)
        })
        .await
        .map_err(|e| storage_err(format!("compact task panicked: {e}")))?;

        // Restore the Database and restart the writer task.
        inner.db = Some(Arc::new(db));
        let (write_tx, write_rx) = tokio::sync::mpsc::channel::<WriteOp>(4096);
        let writer_db = Arc::clone(inner.db.as_ref().ok_or_else(db_missing)?);
        let writer_handle = tokio::spawn(writer_task(writer_db, write_rx));
        inner.writer_handle = Some(writer_handle);
        if let Ok(mut guard) = self.write_tx.lock() {
            *guard = Some(write_tx);
        }

        let compacted = compact_res.map_err(|e| {
            DepotError::Storage(
                format!("redb compact failed: {e}").into(),
                Retryability::Transient,
            )
        })?;
        let size_after = std::fs::metadata(&self.path).map(|m| m.len()).ok();
        tracing::info!(
            path = %self.path.display(),
            compacted,
            size_before = size_before.unwrap_or(0),
            size_after = size_after.unwrap_or(0),
            "redb shard compact complete"
        );
        Ok(compacted)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use depot_core::store::kv::KvStore;

    const TBL: &str = "artifacts";

    fn db_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("shard.redb")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compact_shrinks_file_after_bulk_delete() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = db_path(&dir);
        let store = RedbKvStore::open(&path).unwrap();

        // Write enough bytes that the file clearly grows beyond the
        // minimum-page footprint.
        let payload = vec![0xABu8; 1024];
        let entries: Vec<(String, String)> = (0..2000)
            .map(|i| (format!("p{:04}", i % 8), format!("k{:06}", i)))
            .collect();
        let refs: Vec<(&str, &str, &[u8])> = entries
            .iter()
            .map(|(pk, sk)| (pk.as_str(), sk.as_str(), payload.as_slice()))
            .collect();
        store.put_batch(TBL, &refs).await.unwrap();

        let keys: Vec<(&str, &str)> = entries
            .iter()
            .map(|(pk, sk)| (pk.as_str(), sk.as_str()))
            .collect();
        store.delete_batch(TBL, &keys).await.unwrap();

        let size_before = std::fs::metadata(&path).unwrap().len();
        let compacted = store.compact().await.unwrap();
        let size_after = std::fs::metadata(&path).unwrap().len();

        assert!(compacted, "expected compact to report work done");
        assert!(
            size_after < size_before,
            "file did not shrink: before={size_before} after={size_after}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_work_after_compact() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = RedbKvStore::open(&db_path(&dir)).unwrap();

        store
            .put(TBL, "p".into(), "a".into(), b"one")
            .await
            .unwrap();
        let (v1, ver1) = store
            .get_versioned(TBL, "p".into(), "a".into())
            .await
            .unwrap()
            .expect("key a present");
        assert_eq!(v1, b"one");
        assert_eq!(ver1, 1);

        assert!(store.compact().await.is_ok());

        // Writer must be restarted — a new put should succeed.
        store
            .put(TBL, "p".into(), "b".into(), b"two")
            .await
            .unwrap();
        let (v2, ver2) = store
            .get_versioned(TBL, "p".into(), "b".into())
            .await
            .unwrap()
            .expect("key b present");
        assert_eq!(v2, b"two");
        assert_eq!(ver2, 1);

        // Existing key and value survive compaction.
        let (v1b, _) = store
            .get_versioned(TBL, "p".into(), "a".into())
            .await
            .unwrap()
            .expect("key a still present");
        assert_eq!(v1b, b"one");

        // A second put on the same key increments version monotonically.
        store
            .put(TBL, "p".into(), "a".into(), b"one-v2")
            .await
            .unwrap();
        let (_, ver1b) = store
            .get_versioned(TBL, "p".into(), "a".into())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ver1b, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compact_serialises_with_concurrent_ops() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(RedbKvStore::open(&db_path(&dir)).unwrap());

        // Seed some data.
        for i in 0i32..100 {
            store
                .put(TBL, "p".into(), format!("k{i:03}").into(), &i.to_le_bytes())
                .await
                .unwrap();
        }

        // Race a compact with a flurry of reads and writes.
        let s1 = Arc::clone(&store);
        let reader = tokio::spawn(async move {
            for i in 0i32..100 {
                let v = s1
                    .get(TBL, "p".into(), format!("k{i:03}").into())
                    .await
                    .unwrap();
                assert!(v.is_some());
            }
        });
        let s2 = Arc::clone(&store);
        let writer = tokio::spawn(async move {
            for i in 100i32..200 {
                s2.put(TBL, "p".into(), format!("k{i:03}").into(), &i.to_le_bytes())
                    .await
                    .unwrap();
            }
        });
        let s3 = Arc::clone(&store);
        let compactor = tokio::spawn(async move { s3.compact().await });

        reader.await.unwrap();
        writer.await.unwrap();
        compactor.await.unwrap().unwrap();

        // Every written key must read back correctly after the dust settles.
        for i in 0i32..200 {
            let v = store
                .get(TBL, "p".into(), format!("k{i:03}").into())
                .await
                .unwrap()
                .expect("key present");
            assert_eq!(v, i.to_le_bytes());
        }
    }

    /// A store written in the legacy v2 file format (redb 2.x default) must
    /// be converted to v3 in place on open, keeping its contents. This is
    /// the prerequisite for ever moving to redb >= 3, which cannot open v2
    /// files at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_file_upgrades_to_v3_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = db_path(&dir);
        let fixture = redb::TableDefinition::<&[u8], &[u8]>::new("v2-fixture");

        // Build a v2-format file with raw redb (2.x creates v2 by default)
        // holding one row outside the store's own tables.
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut t = txn.open_table(fixture).unwrap();
                t.insert(b"k".as_slice(), b"v".as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        // Opening through the store performs the in-place upgrade; the store
        // must be fully usable afterwards.
        {
            let mut store = RedbKvStore::open(&path).unwrap();
            store
                .put(TBL, "p".into(), "a".into(), b"one")
                .await
                .unwrap();
            let (v, _) = store
                .get_versioned(TBL, "p".into(), "a".into())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(v, b"one");
            store.close().await;
        }

        // The file is now v3 — a fresh handle has no upgrade left to do —
        // and the pre-upgrade row survived the conversion.
        let mut db = Database::create(&path).unwrap();
        assert!(
            !db.upgrade().unwrap(),
            "file should already be in the v3 format after a store open"
        );
        let txn = db.begin_read().unwrap();
        let t = txn.open_table(fixture).unwrap();
        assert_eq!(t.get(b"k".as_slice()).unwrap().unwrap().value(), b"v");
    }

    /// Fresh stores are created directly in the v3 format.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_store_is_v3() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = db_path(&dir);
        {
            let mut store = RedbKvStore::open(&path).unwrap();
            store
                .put(TBL, "p".into(), "a".into(), b"one")
                .await
                .unwrap();
            store.close().await;
        }
        let mut db = Database::create(&path).unwrap();
        assert!(!db.upgrade().unwrap(), "fresh store should be v3 already");
    }
}
