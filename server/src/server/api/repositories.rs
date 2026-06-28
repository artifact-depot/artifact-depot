// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::server::api::conversions::{
    build_repo_kind_and_format, validate_yum_proxy_member_depth,
};
use crate::server::auth::backend::check_grants;
use crate::server::infra::task::{RepoCleanResult, TaskKind, TaskResult};
use crate::server::worker::blob_reaper;
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;
use depot_core::service;
use depot_core::store::kv::{
    ArtifactFormat, Capability, ContentDisposition, FormatConfig, RepoConfig, RepoKind, RepoType,
    UpstreamAuth, CURRENT_RECORD_VERSION,
};

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateRepoRequest {
    pub name: String,
    pub repo_type: RepoType,
    pub format: ArtifactFormat,
    pub store: String,
    pub upstream_url: Option<String>,
    pub cache_ttl_secs: Option<u64>,
    pub members: Option<Vec<String>>,
    pub write_member: Option<String>,
    pub listen: Option<String>,
    pub cleanup_max_unaccessed_days: Option<u64>,
    pub cleanup_max_age_days: Option<u64>,
    pub cleanup_untagged_manifests: Option<bool>,
    pub upstream_auth: Option<UpstreamAuth>,
    pub content_disposition: Option<ContentDisposition>,
    pub repodata_depth: Option<u32>,
    /// APT only: armored PGP secret key to use for repository signing. When
    /// omitted, hosted apt repos auto-generate a fresh signing key on creation.
    #[serde(default)]
    pub signing_key: Option<String>,
}

impl CreateRepoRequest {
    /// Build a [`FormatConfig`] from the flat request fields.
    pub(crate) fn format_config(&self) -> FormatConfig {
        match self.format {
            ArtifactFormat::Raw => FormatConfig::Raw {
                content_disposition: self.content_disposition,
            },
            ArtifactFormat::Docker => FormatConfig::Docker {
                listen: self.listen.clone(),
                cleanup_untagged_manifests: self.cleanup_untagged_manifests,
            },
            ArtifactFormat::PyPI => FormatConfig::PyPI {},
            ArtifactFormat::Apt => FormatConfig::Apt {
                apt_components: None,
            },
            ArtifactFormat::Golang => FormatConfig::Golang {},
            ArtifactFormat::Helm => FormatConfig::Helm {},
            ArtifactFormat::Cargo => FormatConfig::Cargo {},
            ArtifactFormat::Yum => FormatConfig::Yum {
                repodata_depth: self.repodata_depth,
            },
            ArtifactFormat::Npm => FormatConfig::Npm {},
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepoResponse {
    pub name: String,
    pub repo_type: RepoType,
    pub format: ArtifactFormat,
    pub store: String,
    pub upstream_url: Option<String>,
    pub cache_ttl_secs: Option<u64>,
    pub members: Option<Vec<String>>,
    pub write_member: Option<String>,
    pub listen: Option<String>,
    pub cleanup_max_unaccessed_days: Option<u64>,
    pub cleanup_max_age_days: Option<u64>,
    pub cleanup_untagged_manifests: Option<bool>,
    pub upstream_auth: Option<UpstreamAuth>,
    pub content_disposition: Option<ContentDisposition>,
    pub repodata_depth: Option<u32>,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub artifact_count: u64,
    pub total_bytes: u64,
}

impl From<RepoConfig> for RepoResponse {
    fn from(c: RepoConfig) -> Self {
        // Decompose format_config into flat optional fields for the API.
        let format = c.format_config.artifact_format();
        let (listen, cleanup_untagged_manifests, content_disposition, repodata_depth) =
            match c.format_config {
                FormatConfig::Docker {
                    listen,
                    cleanup_untagged_manifests,
                } => (listen, cleanup_untagged_manifests, None, None),
                FormatConfig::Raw {
                    content_disposition,
                } => (None, None, content_disposition, None),
                FormatConfig::Yum { repodata_depth } => (None, None, None, repodata_depth),
                _ => (None, None, None, None),
            };

        let (repo_type, upstream_url, cache_ttl_secs, members, upstream_auth, write_member) =
            match c.kind {
                RepoKind::Hosted => (RepoType::Hosted, None, None, None, None, None),
                RepoKind::Cache {
                    upstream_url,
                    cache_ttl_secs,
                    upstream_auth,
                } => (
                    RepoType::Cache,
                    Some(upstream_url),
                    Some(cache_ttl_secs),
                    None,
                    upstream_auth.map(|a| UpstreamAuth {
                        username: a.username,
                        password: a.password.as_ref().map(|_| "********".to_string()),
                    }),
                    None,
                ),
                RepoKind::Proxy {
                    members,
                    write_member,
                } => (
                    RepoType::Proxy,
                    None,
                    None,
                    Some(members),
                    None,
                    write_member,
                ),
            };

        Self {
            name: c.name,
            repo_type,
            format,
            store: c.store,
            upstream_url,
            cache_ttl_secs,
            members,
            write_member,
            listen,
            cleanup_max_unaccessed_days: c.cleanup_max_unaccessed_days,
            cleanup_max_age_days: c.cleanup_max_age_days,
            cleanup_untagged_manifests,
            upstream_auth,
            content_disposition,
            repodata_depth,
            created_at: c.created_at,
            artifact_count: 0,
            total_bytes: 0,
        }
    }
}

impl RepoResponse {
    pub fn with_stats(mut self, stats: depot_core::store::kv::RepoStats) -> Self {
        self.artifact_count = stats.artifact_count;
        self.total_bytes = stats.total_bytes;
        self
    }
}

/// List all repositories.
#[utoipa::path(
    get,
    path = "/api/v1/repositories",
    responses(
        (status = 200, description = "List of repositories", body = Vec<RepoResponse>)
    ),
    tag = "repositories"
)]
pub async fn list_repos(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    let roles = match state.auth.backend.resolve_user_roles(&user.0).await {
        Ok(Some((_, roles))) => roles,
        Ok(None) => return Json(Vec::<RepoResponse>::new()).into_response(),
        Err(e) => return e.into_response(),
    };

    match service::list_repos(state.repo.kv.as_ref()).await {
        Ok(repos) => {
            let kv = state.repo.kv.as_ref();
            let authorized: Vec<_> = repos
                .into_iter()
                .filter(|r| check_grants(&roles, &r.name, Capability::Read).is_ok())
                .collect();
            let stats_futs = authorized.iter().map(|r| service::get_repo_stats(kv, r));
            let all_stats = futures::future::join_all(stats_futs).await;
            let resp: Vec<_> = authorized
                .into_iter()
                .zip(all_stats)
                .map(|(r, stats)| RepoResponse::from(r).with_stats(stats.unwrap_or_default()))
                .collect();
            Json(resp).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Get a repository by name.
#[utoipa::path(
    get,
    path = "/api/v1/repositories/{name}",
    params(("name" = String, Path, description = "Repository name")),
    responses(
        (status = 200, description = "Repository details", body = RepoResponse),
        (status = 404, description = "Repository not found")
    ),
    tag = "repositories"
)]
pub async fn get_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &name, Capability::Read)
        .await
    {
        return e.into_response();
    }
    let name_owned = name.clone();
    match service::get_repo(state.repo.kv.as_ref(), &name_owned).await {
        Ok(Some(repo)) => {
            let stats = service::get_repo_stats(state.repo.kv.as_ref(), &repo)
                .await
                .unwrap_or_default();
            Json(RepoResponse::from(repo).with_stats(stats)).into_response()
        }
        Ok(None) => {
            DepotError::NotFound(format!("repository '{}' not found", name)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Create a new repository.
#[utoipa::path(
    post,
    path = "/api/v1/repositories",
    request_body = CreateRepoRequest,
    responses(
        (status = 201, description = "Repository created", body = RepoResponse),
        (status = 409, description = "Repository already exists"),
        (status = 400, description = "Invalid request")
    ),
    tag = "repositories"
)]
pub async fn create_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<CreateRepoRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    if req.name.is_empty() {
        return DepotError::BadRequest("name is required".into()).into_response();
    }

    let (kind, format_config) = match build_repo_kind_and_format(&req) {
        Ok(pair) => pair,
        Err(e) => return e.into_response(),
    };

    let is_apt = format_config.artifact_format() == ArtifactFormat::Apt;
    // Validate an explicitly-supplied signing key before persisting anything, so
    // a malformed key fails the request cleanly rather than leaving an unsigned
    // repo behind.
    if let Some(ref key) = req.signing_key {
        if !is_apt {
            return DepotError::BadRequest("signing_key is only valid for apt repositories".into())
                .into_response();
        }
        if let Err(e) = depot_format_apt::store::public_key_from_secret(key) {
            return e.into_response();
        }
    }

    // For Yum proxy repos, validate that all members have the same repodata_depth.
    if format_config.artifact_format() == ArtifactFormat::Yum {
        if let RepoKind::Proxy { ref members, .. } = kind {
            if let Err(e) = validate_yum_proxy_member_depth(state.repo.kv.as_ref(), members).await {
                return e.into_response();
            }
        }
    }

    // Validate that the named store exists.
    let store_name = req.store.clone();
    let store_exists = service::get_store(state.repo.kv.as_ref(), &store_name).await;
    match store_exists {
        Ok(None) => {
            return DepotError::BadRequest(format!("store '{}' not found", req.store))
                .into_response();
        }
        Err(e) => return e.into_response(),
        _ => {}
    }

    let now = crate::server::repo::now_utc();

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: req.name,
        kind,
        format_config,
        store: req.store,
        created_at: now,
        cleanup_max_unaccessed_days: req.cleanup_max_unaccessed_days,
        cleanup_max_age_days: req.cleanup_max_age_days,
        deleting: false,
    };

    if service::get_repo(state.repo.kv.as_ref(), &config.name)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return DepotError::Conflict("repository already exists".into()).into_response();
    }
    match service::put_repo(state.repo.kv.as_ref(), &config).await {
        Ok(()) => {
            // Provision the apt signing key: import the supplied key, or
            // auto-generate one for hosted repos. Best-effort — the repo is
            // already created, and a missing key can be added later via the
            // signing-key API, so a failure is logged rather than fatal.
            if is_apt {
                let fs = state.format_state();
                let result = match req.signing_key.as_deref() {
                    Some(key) => depot_format_apt::api::import_repo_signing_key(&fs, &config, key)
                        .await
                        .map(Some),
                    None => depot_format_apt::api::ensure_repo_signing_key(&fs, &config).await,
                };
                if let Err(e) = result {
                    tracing::warn!(
                        repo = %config.name,
                        error = %e,
                        "failed to provision apt signing key on repo creation"
                    );
                }
            }
            let resp = RepoResponse::from(config);
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Repos,
                crate::server::infra::event_bus::ModelEvent::RepoCreated { repo: resp.clone() },
            );
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ImportSigningKeyRequest {
    /// Armored PGP secret (private) key to use for repository signing.
    pub signing_key: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SigningKeyResponse {
    /// Armored PGP public key that clients install (via `signed-by`) to verify
    /// the repository's `InRelease` / `Release.gpg` signatures.
    pub public_key: String,
}

/// Generate a fresh signing key for an apt repository, replacing any existing
/// one and re-signing already-published metadata.
#[utoipa::path(
    post,
    path = "/api/v1/repositories/{name}/signing-key",
    params(("name" = String, Path, description = "Repository name")),
    responses(
        (status = 200, description = "New signing key generated", body = SigningKeyResponse),
        (status = 400, description = "Not an apt repository"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Repository not found"),
    ),
    tag = "repositories"
)]
pub async fn rotate_signing_key(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    let config = match load_apt_repo(&state, &name).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let fs = state.format_state();
    match depot_format_apt::api::rotate_repo_signing_key(&fs, &config).await {
        Ok(public_key) => (StatusCode::OK, Json(SigningKeyResponse { public_key })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Import an externally-supplied signing key for an apt repository, replacing any
/// existing one and re-signing already-published metadata.
#[utoipa::path(
    put,
    path = "/api/v1/repositories/{name}/signing-key",
    params(("name" = String, Path, description = "Repository name")),
    request_body = ImportSigningKeyRequest,
    responses(
        (status = 200, description = "Signing key imported", body = SigningKeyResponse),
        (status = 400, description = "Not an apt repository, or invalid key"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Repository not found"),
    ),
    tag = "repositories"
)]
pub async fn import_signing_key(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
    Json(req): Json<ImportSigningKeyRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    if req.signing_key.trim().is_empty() {
        return DepotError::BadRequest("signing_key is required".into()).into_response();
    }
    let config = match load_apt_repo(&state, &name).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let fs = state.format_state();
    match depot_format_apt::api::import_repo_signing_key(&fs, &config, &req.signing_key).await {
        Ok(public_key) => (StatusCode::OK, Json(SigningKeyResponse { public_key })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Load a repository, returning a ready-to-send error response if it does not
/// exist or is not an apt repository (signing keys are apt-only).
async fn load_apt_repo(
    state: &AppState,
    name: &str,
) -> Result<RepoConfig, axum::response::Response> {
    let config = match service::get_repo(state.repo.kv.as_ref(), name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(
                DepotError::NotFound(format!("repository '{name}' not found")).into_response(),
            )
        }
        Err(e) => return Err(e.into_response()),
    };
    if config.format() != ArtifactFormat::Apt {
        return Err(DepotError::BadRequest(
            "signing keys are only supported for apt repositories".into(),
        )
        .into_response());
    }
    Ok(config)
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateRepoRequest {
    #[serde(default)]
    pub upstream_url: Option<String>,
    #[serde(default)]
    pub cache_ttl_secs: Option<u64>,
    #[serde(default)]
    pub upstream_auth: Option<UpstreamAuth>,
    #[serde(default)]
    pub members: Option<Vec<String>>,
    #[serde(default)]
    pub write_member: Option<String>,
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub cleanup_untagged_manifests: Option<bool>,
    #[serde(default)]
    pub cleanup_max_age_days: Option<u64>,
    #[serde(default)]
    pub cleanup_max_unaccessed_days: Option<u64>,
    #[serde(default)]
    pub content_disposition: Option<ContentDisposition>,
    #[serde(default)]
    pub repodata_depth: Option<u32>,
}

/// Update a repository's mutable settings.
#[utoipa::path(
    put,
    path = "/api/v1/repositories/{name}",
    params(("name" = String, Path, description = "Repository name")),
    request_body = UpdateRepoRequest,
    responses(
        (status = 200, description = "Repository updated", body = RepoResponse),
        (status = 404, description = "Repository not found"),
        (status = 400, description = "Invalid request"),
        (status = 403, description = "Admin required"),
    ),
    tag = "repositories"
)]
pub async fn update_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
    Json(req): Json<UpdateRepoRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    if let Some(depth) = req.repodata_depth {
        if depth > 5 {
            return DepotError::BadRequest("repodata_depth must be 0-5".into()).into_response();
        }
    }
    if req.cleanup_max_age_days == Some(0) {
        return DepotError::BadRequest("cleanup_max_age_days must be >= 1".into()).into_response();
    }
    if req.cleanup_max_unaccessed_days == Some(0) {
        return DepotError::BadRequest("cleanup_max_unaccessed_days must be >= 1".into())
            .into_response();
    }

    let mut config = match service::get_repo(state.repo.kv.as_ref(), &name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", name))
                .into_response();
        }
        Err(e) => return e.into_response(),
    };

    // Update RepoKind mutable fields.
    config.kind = match config.kind {
        RepoKind::Hosted => RepoKind::Hosted,
        RepoKind::Cache {
            upstream_url,
            cache_ttl_secs,
            upstream_auth,
        } => {
            // Preserve existing password if the sentinel "********" is sent back.
            let new_auth = match req.upstream_auth {
                Some(mut auth) => {
                    if auth.password.as_deref() == Some("********") {
                        auth.password = upstream_auth.as_ref().and_then(|a| a.password.clone());
                    }
                    Some(auth)
                }
                None => None,
            };
            RepoKind::Cache {
                upstream_url: req.upstream_url.unwrap_or(upstream_url),
                cache_ttl_secs: req.cache_ttl_secs.unwrap_or(cache_ttl_secs),
                upstream_auth: new_auth,
            }
        }
        RepoKind::Proxy {
            members,
            write_member,
        } => RepoKind::Proxy {
            members: req.members.unwrap_or(members),
            write_member: if req.write_member.is_some() {
                req.write_member
            } else {
                write_member
            },
        },
    };

    // For Yum proxy repos, validate uniform member depth when members change.
    if config.format() == ArtifactFormat::Yum {
        if let RepoKind::Proxy { ref members, .. } = config.kind {
            if let Err(e) = validate_yum_proxy_member_depth(state.repo.kv.as_ref(), members).await {
                return e.into_response();
            }
        }
    }

    // Update FormatConfig mutable fields.
    let old_repodata_depth = config.format_config.repodata_depth();
    config.format_config = match config.format_config {
        FormatConfig::Docker {
            listen,
            cleanup_untagged_manifests,
        } => FormatConfig::Docker {
            listen: if req.listen.is_some() {
                req.listen
            } else {
                listen
            },
            cleanup_untagged_manifests: if req.cleanup_untagged_manifests.is_some() {
                req.cleanup_untagged_manifests
            } else {
                cleanup_untagged_manifests
            },
        },
        FormatConfig::Raw {
            content_disposition,
        } => FormatConfig::Raw {
            content_disposition: if req.content_disposition.is_some() {
                req.content_disposition
            } else {
                content_disposition
            },
        },
        FormatConfig::Apt { .. } => FormatConfig::Apt {
            apt_components: None,
        },
        FormatConfig::Yum { repodata_depth } => FormatConfig::Yum {
            repodata_depth: if req.repodata_depth.is_some() {
                req.repodata_depth
            } else {
                repodata_depth
            },
        },
        other => other,
    };

    // Update shared cleanup fields.
    config.cleanup_max_age_days = req.cleanup_max_age_days;
    config.cleanup_max_unaccessed_days = req.cleanup_max_unaccessed_days;

    // Detect repodata_depth change and trigger rebuild.
    let depth_changed = config.format_config.repodata_depth() != old_repodata_depth;

    match service::put_repo(state.repo.kv.as_ref(), &config).await {
        Ok(()) => {
            if depth_changed {
                // Spawn background task to delete old repodata and trigger rebuild.
                let task_fs = state.format_state();
                let repo_name = config.name.clone();
                let repo_config = config.clone();
                tokio::spawn(async move {
                    if let Err(e) = depot_format_yum::api::rebuild_repodata_for_depth_change(
                        &task_fs,
                        &repo_config,
                    )
                    .await
                    {
                        tracing::error!(
                            repo = repo_name,
                            error = %e,
                            "failed to rebuild yum repodata after depth change"
                        );
                    }
                });
            }
            let resp = RepoResponse::from(config);
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Repos,
                crate::server::infra::event_bus::ModelEvent::RepoUpdated { repo: resp.clone() },
            );
            Json(resp).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Delete a repository.
#[utoipa::path(
    delete,
    path = "/api/v1/repositories/{name}",
    params(("name" = String, Path, description = "Repository name")),
    responses(
        (status = 204, description = "Repository deleted"),
        (status = 404, description = "Repository not found")
    ),
    tag = "repositories"
)]
pub async fn delete_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    let name_owned = name.clone();
    match service::delete_repo(state.repo.kv.as_ref(), &name_owned).await {
        Ok(true) => {
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Repos,
                crate::server::infra::event_bus::ModelEvent::RepoDeleted { name: name_owned },
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            DepotError::NotFound(format!("repository '{}' not found", name)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CloneRepoRequest {
    pub new_name: String,
}

/// Clone a repository, creating a new repo with the same configuration and artifacts.
/// Returns a task that can be polled for progress.
#[utoipa::path(
    post,
    path = "/api/v1/repositories/{name}/clone",
    params(("name" = String, Path, description = "Source repository name")),
    request_body = CloneRepoRequest,
    responses(
        (status = 201, description = "Clone task created", body = crate::server::infra::task::TaskInfo),
        (status = 400, description = "Invalid request"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Source repository not found"),
        (status = 409, description = "Clone already running or target name exists"),
    ),
    tag = "repositories"
)]
pub async fn clone_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
    Json(req): Json<CloneRepoRequest>,
) -> impl IntoResponse {
    use std::sync::Arc;

    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    if req.new_name.is_empty() {
        return DepotError::BadRequest("new_name is required".into()).into_response();
    }
    if req.new_name == name {
        return DepotError::BadRequest("new_name must differ from the source name".into())
            .into_response();
    }

    let source = match service::get_repo(state.repo.kv.as_ref(), &name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", name))
                .into_response();
        }
        Err(e) => return e.into_response(),
    };

    // Check target name not already taken before spawning task.
    if service::get_repo(state.repo.kv.as_ref(), &req.new_name)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return DepotError::Conflict("target repository name already exists".into())
            .into_response();
    }

    if state
        .bg
        .tasks
        .has_running_repo_clone(&name, &req.new_name)
        .await
    {
        return DepotError::Conflict(format!(
            "clone already running for '{}' -> '{}'",
            name, req.new_name
        ))
        .into_response();
    }

    // Get source artifact count for progress estimation.
    let repo_stats = service::get_repo_stats(state.repo.kv.as_ref(), &source)
        .await
        .unwrap_or_default();

    let kind = TaskKind::RepoClone {
        source: name.clone(),
        target: req.new_name.clone(),
    };
    let (task_id, progress_tx, cancel) = state.bg.tasks.create(kind).await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = Arc::clone(&state.repo.kv);
    let new_name = req.new_name.clone();
    let source_name = name.clone();

    tokio::spawn(async move {
        task_manager
            .append_log(
                task_id,
                format!("Cloning '{}' to '{}'", source_name, new_name),
            )
            .await;

        let start = std::time::Instant::now();
        match service::clone_repo(
            kv.as_ref(),
            &source,
            &new_name,
            repo_stats.artifact_count,
            Some(&cancel),
            Some(&progress_tx),
        )
        .await
        {
            Ok(Some(stats)) => {
                let elapsed = start.elapsed();
                let result = crate::server::infra::task::RepoCloneResult {
                    source: source_name.clone(),
                    target: new_name.clone(),
                    copied_artifacts: stats.copied_artifacts,
                    copied_entries: stats.copied_entries,
                    elapsed_ns: elapsed.as_nanos() as u64,
                };
                let summary = format!(
                    "Cloned '{}' to '{}': {} artifacts, {} dir entries",
                    source_name, new_name, stats.copied_artifacts, stats.copied_entries,
                );
                task_manager.update_summary(task_id, summary).await;
                task_manager
                    .mark_completed(task_id, TaskResult::RepoClone(result))
                    .await;
            }
            Ok(None) => {
                task_manager
                    .mark_failed(
                        task_id,
                        format!("target repository '{}' already exists", new_name),
                    )
                    .await;
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    task_manager.mark_cancelled(task_id).await;
                } else {
                    task_manager.mark_failed(task_id, format!("{e}")).await;
                }
            }
        }
    });

    let info = state.bg.tasks.get(task_id).await;
    (StatusCode::CREATED, Json(info)).into_response()
}

/// Start a cleanup task for a specific repository.
#[utoipa::path(
    post,
    path = "/api/v1/repositories/{name}/clean",
    params(("name" = String, Path, description = "Repository name")),
    responses(
        (status = 201, description = "Cleanup task created", body = crate::server::infra::task::TaskInfo),
        (status = 400, description = "No cleanup policy configured"),
        (status = 403, description = "Admin required"),
        (status = 404, description = "Repository not found"),
        (status = 409, description = "Cleanup already running for this repo"),
    ),
    tag = "repositories"
)]
pub async fn clean_repo(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    let repo = match service::get_repo(state.repo.kv.as_ref(), &name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", name))
                .into_response();
        }
        Err(e) => return e.into_response(),
    };

    let has_policy = repo.cleanup_max_age_days.is_some()
        || repo.cleanup_max_unaccessed_days.is_some()
        || repo.format_config.artifact_format() == ArtifactFormat::Docker;
    if !has_policy {
        return DepotError::BadRequest("repository has no cleanup policy configured".to_string())
            .into_response();
    }

    if state.bg.tasks.has_running_repo_clean(&name).await {
        return DepotError::Conflict(format!("A cleanup task is already running for '{}'", name))
            .into_response();
    }

    let (task_id, progress_tx, cancel) = state
        .bg
        .tasks
        .create(TaskKind::RepoClean { repo: name.clone() })
        .await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = state.repo.kv.clone();
    let stores = state.repo.stores.clone();
    let updater = state.repo.updater.clone();
    let repo_name = name.clone();

    tokio::spawn(async move {
        task_manager
            .append_log(
                task_id,
                format!("Starting cleanup for repository '{}'", repo_name),
            )
            .await;

        let start = std::time::Instant::now();
        match blob_reaper::clean_repo_artifacts(
            kv.clone(),
            &stores,
            &repo,
            Some(&cancel),
            Some(&progress_tx),
            &updater,
        )
        .await
        {
            Ok(stats) => {
                let elapsed = start.elapsed();
                let result = RepoCleanResult {
                    repo: repo_name.clone(),
                    scanned_artifacts: stats.scanned_artifacts,
                    expired_artifacts: stats.expired_artifacts,
                    docker_deleted: stats.docker_deleted,
                    elapsed_ns: elapsed.as_nanos() as u64,
                };
                task_manager
                    .update_summary(
                        task_id,
                        format!(
                            "Clean '{}': {} artifacts scanned, {} expired, {} Docker refs cleaned",
                            repo_name,
                            stats.scanned_artifacts,
                            stats.expired_artifacts,
                            stats.docker_deleted,
                        ),
                    )
                    .await;
                task_manager
                    .mark_completed(task_id, TaskResult::RepoClean(result))
                    .await;
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    task_manager.mark_cancelled(task_id).await;
                } else {
                    task_manager.mark_failed(task_id, format!("{e}")).await;
                }
            }
        }
    });

    let info = state.bg.tasks.get(task_id).await;
    (StatusCode::CREATED, Json(info)).into_response()
}
