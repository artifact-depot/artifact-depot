// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use super::{ingest_artifact, now_utc, ArtifactContent, IngestionCtx, IngestionResult, RawRepo};
use crate::error;
use crate::service;
use crate::store::blob::BlobStore;
use crate::store::kv::{
    ArtifactKind, ArtifactRecord, KvStore, PaginatedResult, Pagination, CURRENT_RECORD_VERSION,
};
use crate::update::UpdateSender;

/// Set the Helm metadata stale flag on a single repo (no propagation).
///
/// Inlined here (rather than called via a format-specific module) so the
/// shared `HostedRepo` can trigger a Helm index rebuild without introducing
/// a dependency from `depot-core` onto format-specific code.
async fn set_helm_stale_flag(kv: &dyn KvStore, repo: &str) -> error::Result<()> {
    let now = now_utc();
    let flag = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "text/plain".to_string(),
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        path: String::new(),
        kind: ArtifactKind::HelmMetadata,
        internal: true,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    service::put_artifact_if_absent(kv, repo, "_helm/metadata_stale", &flag).await?;
    Ok(())
}

/// Operations for a hosted repository.
pub struct HostedRepo {
    pub name: String,
    pub kv: Arc<dyn KvStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub updater: UpdateSender,
    pub store: String,
    pub format: String,
}

impl HostedRepo {
    #[tracing::instrument(level = "debug", skip(self, data), fields(repo = %self.name, path))]
    pub async fn put(
        &self,
        path: &str,
        content_type: &str,
        data: &[u8],
    ) -> error::Result<ArtifactRecord> {
        let ctx = IngestionCtx {
            kv: self.kv.as_ref(),
            blobs: self.blobs.as_ref(),
            store: &self.store,
            repo: &self.name,
            format: &self.format,
            repo_type: "hosted",
        };
        let IngestionResult {
            record,
            old_record,
            new_blob,
        } = ingest_artifact(&ctx, path, content_type, data).await?;
        // Per-repo (logical) dir stats track the artifact record either way.
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(&self.name, path, count_delta, bytes_delta)
            .await;
        // Store (physical) stats grow only when a genuinely new blob was stored.
        if new_blob {
            self.updater
                .store_changed(&self.store, 1, record.size as i64)
                .await;
        }

        // If a chart was uploaded to a Helm repo via the generic raw handler
        // (e.g. restore), mark metadata stale so the index is rebuilt.
        if self.format == "helm" && path.ends_with(".tgz") {
            let _ = set_helm_stale_flag(self.kv.as_ref(), &self.name).await;
            let _ = service::propagate_staleness(
                self.kv.as_ref(),
                &self.name,
                crate::store::kv::ArtifactFormat::Helm,
                "_helm/metadata_stale",
            )
            .await;
        }

        Ok(record)
    }

    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.name, path))]
    pub async fn delete(&self, path: &str) -> error::Result<bool> {
        let old_record = service::delete_artifact(self.kv.as_ref(), &self.name, path).await?;
        if let Some(old) = &old_record {
            // Per-repo dir stats only — removing the artifact record orphans the
            // blob; GC reclaims it (and adjusts store stats) once unreferenced.
            self.updater
                .dir_changed(&self.name, path, -1, -(old.size as i64))
                .await;
        }
        // If a chart was deleted from a Helm repo, mark metadata stale.
        if old_record.is_some() && self.format == "helm" && path.ends_with(".tgz") {
            let _ = set_helm_stale_flag(self.kv.as_ref(), &self.name).await;
            let _ = service::propagate_staleness(
                self.kv.as_ref(),
                &self.name,
                crate::store::kv::ArtifactFormat::Helm,
                "_helm/metadata_stale",
            )
            .await;
        }
        Ok(old_record.is_some())
    }
    pub async fn list(
        &self,
        prefix: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        service::list_artifacts(self.kv.as_ref(), &self.name, prefix, pagination).await
    }
}

#[async_trait::async_trait]
impl RawRepo for HostedRepo {
    async fn head(&self, path: &str) -> error::Result<Option<ArtifactRecord>> {
        service::get_artifact(self.kv.as_ref(), &self.name, path).await
    }

    async fn get(&self, path: &str) -> error::Result<Option<ArtifactContent>> {
        let record = service::get_artifact(self.kv.as_ref(), &self.name, path).await?;

        if record.is_some() {
            self.updater.touch(&self.name, path, now_utc());
        }

        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };

        Ok(Some(
            super::open_artifact_blob(self.blobs.as_ref(), record).await?,
        ))
    }

    async fn search(
        &self,
        query: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        service::search_artifacts(self.kv.as_ref(), &self.name, query, pagination).await
    }

    async fn list(
        &self,
        prefix: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        service::list_artifacts(self.kv.as_ref(), &self.name, prefix, pagination).await
    }

    async fn list_children(&self, prefix: &str) -> error::Result<crate::store::kv::ChildrenResult> {
        service::list_children(self.kv.as_ref(), &self.name, prefix).await
    }
}
