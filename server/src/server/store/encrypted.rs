// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Transparent encryption wrapper for `KvStore`.
//!
//! Wraps an inner `KvStore` and encrypts all values on write using
//! AES-256-GCM, decrypting on read.  A 12-byte random nonce is prepended
//! to each ciphertext so it travels with the value.
//!
//! The 256-bit encryption key is derived from the user-supplied
//! `server_secret`.  When no secret is configured the layer is not
//! used and values are stored unencrypted.

use std::borrow::Cow;

use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use depot_core::error::{self, DepotError};
use depot_core::store::kv::{KvStore, ScanResult};

/// Derive a 256-bit AES key from arbitrary-length input via BLAKE3.
fn derive_key(secret: &[u8]) -> [u8; 32] {
    let ctx = "depot-kv-encryption-key";
    blake3::derive_key(ctx, secret)
}

/// Transparent encrypt/decrypt wrapper around any `KvStore`.
pub struct EncryptedKvStore<T> {
    inner: T,
    cipher: Aes256Gcm,
}

impl<T> EncryptedKvStore<T> {
    /// Create with an explicit secret (from config).
    pub fn with_secret(inner: T, secret: &str) -> Self {
        let key_bytes = derive_key(secret.as_bytes());
        Self {
            inner,
            cipher: Aes256Gcm::new(&Key::<Aes256Gcm>::from(key_bytes)),
        }
    }

    fn encrypt(&self, plaintext: &[u8]) -> error::Result<Vec<u8>> {
        let nonce = Nonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| DepotError::Internal("KV encryption failed".into()))?;
        // nonce (12 bytes) || ciphertext
        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    fn decrypt(&self, blob: &[u8]) -> error::Result<Vec<u8>> {
        if blob.len() < 12 {
            return Err(DepotError::Internal(
                "KV value too short to contain nonce".into(),
            ));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(12);
        let nonce = Nonce::try_from(nonce_bytes)
            .map_err(|_| DepotError::Internal("KV value nonce malformed".into()))?;
        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| DepotError::Internal("KV decryption failed (wrong server_secret?)".into()))
    }
}

#[async_trait::async_trait]
impl<T: KvStore> KvStore for EncryptedKvStore<T> {
    async fn get(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<Vec<u8>>> {
        match self.inner.get(table, pk, sk).await? {
            Some(blob) => Ok(Some(self.decrypt(&blob)?)),
            None => Ok(None),
        }
    }

    async fn get_versioned(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<(Vec<u8>, u64)>> {
        match self.inner.get_versioned(table, pk, sk).await? {
            Some((blob, version)) => Ok(Some((self.decrypt(&blob)?, version))),
            None => Ok(None),
        }
    }

    async fn put(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        value: &[u8],
    ) -> error::Result<()> {
        let encrypted = self.encrypt(value)?;
        self.inner.put(table, pk, sk, &encrypted).await
    }

    async fn put_if_version(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        value: &[u8],
        expected_version: Option<u64>,
    ) -> error::Result<bool> {
        let encrypted = self.encrypt(value)?;
        self.inner
            .put_if_version(table, pk, sk, &encrypted, expected_version)
            .await
    }

    async fn delete(&self, table: &str, pk: Cow<'_, str>, sk: Cow<'_, str>) -> error::Result<bool> {
        self.inner.delete(table, pk, sk).await
    }

    async fn delete_if_version(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
        expected_version: u64,
    ) -> error::Result<bool> {
        self.inner
            .delete_if_version(table, pk, sk, expected_version)
            .await
    }

    async fn delete_returning(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<Vec<u8>>> {
        match self.inner.delete_returning(table, pk, sk).await? {
            Some(encrypted) => Ok(Some(self.decrypt(&encrypted)?)),
            None => Ok(None),
        }
    }

    async fn delete_batch(&self, table: &str, keys: &[(&str, &str)]) -> error::Result<Vec<bool>> {
        self.inner.delete_batch(table, keys).await
    }

    async fn put_batch(&self, table: &str, entries: &[(&str, &str, &[u8])]) -> error::Result<()> {
        let encrypted: Vec<_> = entries
            .iter()
            .map(|(pk, sk, v)| self.encrypt(v).map(|enc| (pk, sk, enc)))
            .collect::<error::Result<Vec<_>>>()?;
        let refs: Vec<(&str, &str, &[u8])> = encrypted
            .iter()
            .map(|(pk, sk, enc)| (**pk, **sk, enc.as_slice()))
            .collect();
        self.inner.put_batch(table, &refs).await
    }

    async fn scan_prefix(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk_prefix: Cow<'_, str>,
        limit: usize,
    ) -> error::Result<ScanResult> {
        let mut result = self.inner.scan_prefix(table, pk, sk_prefix, limit).await?;
        for item in &mut result.items {
            item.1 = self.decrypt(&item.1)?;
        }
        Ok(result)
    }

    async fn scan_range(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk_start: Cow<'_, str>,
        sk_end: Option<&str>,
        limit: usize,
    ) -> error::Result<ScanResult> {
        let mut result = self
            .inner
            .scan_range(table, pk, sk_start, sk_end, limit)
            .await?;
        for item in &mut result.items {
            item.1 = self.decrypt(&item.1)?;
        }
        Ok(result)
    }

    async fn get_consistent(
        &self,
        table: &str,
        pk: Cow<'_, str>,
        sk: Cow<'_, str>,
    ) -> error::Result<Option<Vec<u8>>> {
        match self.inner.get_consistent(table, pk, sk).await? {
            Some(blob) => Ok(Some(self.decrypt(&blob)?)),
            None => Ok(None),
        }
    }

    fn is_single_node(&self) -> bool {
        self.inner.is_single_node()
    }

    async fn compact(&self) -> error::Result<bool> {
        self.inner.compact().await
    }
}

/// Well-known canary key used to verify the encryption key on startup.
const CANARY_TABLE: &str = "meta";
const CANARY_PK: &str = "SINGLE_PK";
const CANARY_SK: &str = "__encryption_canary";
const CANARY_PLAINTEXT: &[u8] = b"depot-encryption-canary-ok";

/// Write the encryption canary. Called once when no canary exists yet.
pub async fn write_canary<T: KvStore>(store: &EncryptedKvStore<T>) -> error::Result<()> {
    store
        .put(
            CANARY_TABLE,
            Cow::Borrowed(CANARY_PK),
            Cow::Borrowed(CANARY_SK),
            CANARY_PLAINTEXT,
        )
        .await
}

/// Verify the encryption canary can be decrypted with the current key.
/// Returns `Ok(true)` if the canary exists and decrypted correctly,
/// `Ok(false)` if no canary exists yet (fresh database), or an error
/// if decryption failed (wrong key).
pub async fn verify_canary<T: KvStore>(store: &EncryptedKvStore<T>) -> error::Result<bool> {
    match store
        .get(
            CANARY_TABLE,
            Cow::Borrowed(CANARY_PK),
            Cow::Borrowed(CANARY_SK),
        )
        .await
    {
        Ok(Some(plaintext)) => {
            if plaintext == CANARY_PLAINTEXT {
                Ok(true)
            } else {
                Err(DepotError::Internal(
                    "encryption canary mismatch — server_secret may be wrong".into(),
                ))
            }
        }
        Ok(None) => Ok(false),
        Err(e) => Err(DepotError::Internal(format!(
            "cannot decrypt KV store — server_secret does not match the key \
             used to encrypt existing data ({})",
            e
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use depot_kv_redb::redb_kv::RedbKvStore;

    fn open_temp_kv() -> RedbKvStore {
        let dir = tempfile::tempdir().unwrap();
        RedbKvStore::open(&dir.path().join("test.redb")).unwrap()
    }

    #[tokio::test]
    async fn round_trip_with_secret() {
        let raw = open_temp_kv();
        let enc = EncryptedKvStore::with_secret(raw, "my-secret");
        let value = b"hello world";

        enc.put("t", Cow::Borrowed("pk"), Cow::Borrowed("sk"), value)
            .await
            .unwrap();
        let got = enc
            .get("t", Cow::Borrowed("pk"), Cow::Borrowed("sk"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, value);
    }

    #[tokio::test]
    async fn wrong_key_fails_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let value = b"secret data";

        // Write with one key.
        {
            let raw = RedbKvStore::open(&path).unwrap();
            let enc = EncryptedKvStore::with_secret(raw, "key-one");
            enc.put("t", Cow::Borrowed("pk"), Cow::Borrowed("sk"), value)
                .await
                .unwrap();
            drop(enc);
            // Yield to let the writer task finish after the channel closes.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Read with a different key — should fail.
        {
            let raw = RedbKvStore::open(&path).unwrap();
            let enc = EncryptedKvStore::with_secret(raw, "key-two");
            let result = enc.get("t", Cow::Borrowed("pk"), Cow::Borrowed("sk")).await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn canary_lifecycle() {
        let raw = open_temp_kv();
        let enc = EncryptedKvStore::with_secret(raw, "test-secret");

        // No canary yet.
        assert!(!verify_canary(&enc).await.unwrap());

        // Write canary.
        write_canary(&enc).await.unwrap();

        // Verify succeeds.
        assert!(verify_canary(&enc).await.unwrap());
    }

    #[tokio::test]
    async fn canary_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");

        // Write canary with one key.
        {
            let raw = RedbKvStore::open(&path).unwrap();
            let enc = EncryptedKvStore::with_secret(raw, "correct-key");
            write_canary(&enc).await.unwrap();
            drop(enc);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Verify with wrong key — should error.
        {
            let raw = RedbKvStore::open(&path).unwrap();
            let enc = EncryptedKvStore::with_secret(raw, "wrong-key");
            assert!(verify_canary(&enc).await.is_err());
        }
    }

    #[tokio::test]
    async fn scan_prefix_decrypts() {
        let raw = open_temp_kv();
        let enc = EncryptedKvStore::with_secret(raw, "scan-test");

        enc.put("t", Cow::Borrowed("pk"), Cow::Borrowed("a/1"), b"one")
            .await
            .unwrap();
        enc.put("t", Cow::Borrowed("pk"), Cow::Borrowed("a/2"), b"two")
            .await
            .unwrap();

        let result = enc
            .scan_prefix("t", Cow::Borrowed("pk"), Cow::Borrowed("a/"), 100)
            .await
            .unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].1, b"one");
        assert_eq!(result.items[1].1, b"two");
    }

    #[tokio::test]
    async fn put_batch_encrypts() {
        let raw = open_temp_kv();
        let enc = EncryptedKvStore::with_secret(raw, "batch-test");

        enc.put_batch(
            "t",
            &[("pk", "sk1", b"val1" as &[u8]), ("pk", "sk2", b"val2")],
        )
        .await
        .unwrap();

        let v1 = enc
            .get("t", Cow::Borrowed("pk"), Cow::Borrowed("sk1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1, b"val1");
    }

    #[tokio::test]
    async fn get_versioned_decrypts() {
        let raw = open_temp_kv();
        let enc = EncryptedKvStore::with_secret(raw, "ver-test");

        enc.put("t", Cow::Borrowed("pk"), Cow::Borrowed("sk"), b"data")
            .await
            .unwrap();
        let (val, version) = enc
            .get_versioned("t", Cow::Borrowed("pk"), Cow::Borrowed("sk"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(val, b"data");
        assert!(version >= 1);
    }
}
