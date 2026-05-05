// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, head, patch, post},
    Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::server::api::{
    artifacts, auth as api_auth, backup, nexus_compat, repositories, roles,
    settings as api_settings, stores, system, tasks, users,
};
use crate::server::infra::event_stream;
use crate::server::infra::state::AppState;
use depot_format_docker::api as docker;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Artifact Depot",
        version = "1.0.0",
        description = "Scale-out artifact repository manager"
    ),
    paths(
        // System
        system::health,
        // Auth
        api_auth::login,
        api_auth::change_password,
        api_auth::revoke_all_tokens,
        // Repositories
        repositories::list_repos,
        repositories::get_repo,
        repositories::create_repo,
        repositories::delete_repo,
        repositories::update_repo,
        repositories::clean_repo,
        repositories::clone_repo,
        // Artifacts
        artifacts::head_artifact,
        artifacts::get_artifact,
        artifacts::put_artifact,
        artifacts::delete_artifact,
        artifacts::list_artifacts,
        artifacts::start_bulk_delete,
        artifacts::download_archive,
        // Users
        users::list_users,
        users::get_user,
        users::create_user,
        users::update_user,
        users::delete_user,
        users::revoke_user_tokens,
        // Roles
        roles::list_roles,
        roles::get_role,
        roles::create_role,
        roles::update_role,
        roles::delete_role,
        // Stores
        stores::list_stores,
        stores::get_store,
        stores::create_store,
        stores::update_store,
        stores::delete_store,
        stores::browse_store_blobs,
        stores::check_store,
        // Settings
        api_settings::get_settings,
        api_settings::put_settings,
        // Backup & Restore
        backup::get_backup,
        backup::post_restore,
        // Tasks
        tasks::start_check,
        tasks::start_gc,
        tasks::get_scheduler_status,
        tasks::start_rebuild_dir_entries,
        tasks::list_tasks,
        tasks::get_task,
        tasks::delete_task,
        // Streaming
        event_stream::event_stream,
    ),
    components(schemas(
        system::HealthResponse,
        // Auth
        api_auth::LoginRequest,
        api_auth::LoginResponse,
        api_auth::ChangePasswordRequest,
        // Repositories
        repositories::CreateRepoRequest,
        repositories::UpdateRepoRequest,
        repositories::CloneRepoRequest,
        repositories::RepoResponse,
        // Artifacts
        artifacts::ArtifactResponse,
        artifacts::DirResponse,
        artifacts::BrowseResponse,
        artifacts::BulkDeleteRequest,
        // Users
        users::UserResponse,
        users::CreateUserRequest,
        users::UpdateUserRequest,
        // Roles
        roles::RoleResponse,
        roles::CreateRoleRequest,
        roles::UpdateRoleRequest,
        // Stores
        stores::CreateStoreRequest,
        stores::StoreResponse,
        stores::UpdateStoreRequest,
        stores::BrowseEntry,
        stores::BrowseResponse,
        stores::StoreCheckResponse,
        // Settings
        crate::server::config::settings::Settings,
        crate::server::config::settings::LoggingSettingsConfig,
        crate::server::config::CorsConfig,
        crate::server::config::RateLimitConfig,
        // Backup
        backup::RestoreSummary,
        // Shared enums
        depot_core::store::kv::RepoType,
        depot_core::store::kv::ArtifactFormat,
        depot_core::store::kv::Capability,
        depot_core::store::kv::CapabilityGrant,
        // Errors
        depot_core::error::ErrorBody,
        depot_core::error::ErrorResponse,
        // Tasks
        crate::server::infra::task::TaskInfo,
        crate::server::infra::task::TaskKind,
        crate::server::infra::task::TaskStatus,
        crate::server::infra::task::TaskProgress,
        crate::server::infra::task::TaskResult,
        crate::server::infra::task::CheckResult,
        crate::server::infra::task::CheckFinding,
        crate::server::infra::task::GcResult,
        crate::server::infra::task::RepoCleanResult,
        crate::server::infra::task::RebuildDirEntriesResult,
        crate::server::infra::task::BulkDeleteResult,
        crate::server::infra::task::TaskLogEntry,
    )),
    tags(
        (name = "system", description = "System operations"),
        (name = "auth", description = "Authentication"),
        (name = "repositories", description = "Repository management"),
        (name = "artifacts", description = "Artifact upload, download, and search"),
        (name = "users", description = "User management"),
        (name = "roles", description = "Role management"),
        (name = "stores", description = "Blob store management"),
        (name = "settings", description = "Cluster-wide settings"),
        (name = "backup", description = "Backup and restore"),
        (name = "tasks", description = "Background task management"),
        (name = "streaming", description = "Real-time SSE event streams"),
    )
)]
struct ApiDoc;

/// Return the generated OpenAPI specification (used by tests and Swagger UI).
pub fn openapi_spec() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// Maximum body size for JSON API endpoints (2 MiB).
const JSON_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Maximum body size for Docker manifest uploads (32 MiB).
const MANIFEST_BODY_LIMIT: usize = 32 * 1024 * 1024;

pub fn build_router(state: AppState, metrics_handle: Option<PrometheusHandle>) -> Router {
    // --- Public API routes (no auth needed for functionality) — all JSON, 2 MiB ---
    let public_routes = Router::new()
        .route("/api/v1/health", get(system::health))
        .route("/api/v1/auth/login", post(api_auth::login))
        .route(
            "/api/v1/auth/change-password",
            post(api_auth::change_password),
        )
        .route(
            "/v2/token",
            get(docker::v2_token).post(docker::v2_token_post),
        )
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT));

    // --- API JSON routes (authorization checked per handler) — 2 MiB ---
    let api_json_routes = Router::new()
        .route(
            "/api/v1/repositories",
            get(repositories::list_repos).post(repositories::create_repo),
        )
        .route(
            "/api/v1/repositories/{name}",
            get(repositories::get_repo)
                .put(repositories::update_repo)
                .delete(repositories::delete_repo),
        )
        .route(
            "/api/v1/repositories/{name}/clean",
            post(repositories::clean_repo),
        )
        .route(
            "/api/v1/repositories/{name}/clone",
            post(repositories::clone_repo),
        )
        .route(
            "/api/v1/repositories/{repo}/artifacts",
            get(artifacts::list_artifacts),
        )
        .route(
            "/api/v1/repositories/{repo}/bulk-delete",
            post(artifacts::start_bulk_delete),
        )
        .route(
            "/api/v1/repositories/{repo}/download-archive",
            get(artifacts::download_archive),
        )
        // Token revocation
        .route("/api/v1/auth/revoke-all", post(api_auth::revoke_all_tokens))
        // User management
        .route(
            "/api/v1/users",
            get(users::list_users).post(users::create_user),
        )
        .route(
            "/api/v1/users/{username}",
            get(users::get_user)
                .put(users::update_user)
                .delete(users::delete_user),
        )
        .route(
            "/api/v1/users/{username}/revoke-tokens",
            post(users::revoke_user_tokens),
        )
        // Role management
        .route(
            "/api/v1/roles",
            get(roles::list_roles).post(roles::create_role),
        )
        .route(
            "/api/v1/roles/{name}",
            get(roles::get_role)
                .put(roles::update_role)
                .delete(roles::delete_role),
        )
        // Store management
        .route(
            "/api/v1/stores",
            get(stores::list_stores).post(stores::create_store),
        )
        .route(
            "/api/v1/stores/{name}",
            get(stores::get_store)
                .put(stores::update_store)
                .delete(stores::delete_store),
        )
        .route(
            "/api/v1/stores/{name}/browse",
            get(stores::browse_store_blobs),
        )
        .route("/api/v1/stores/{name}/check", post(stores::check_store))
        // Settings
        .route(
            "/api/v1/settings",
            get(api_settings::get_settings).put(api_settings::put_settings),
        );
    #[cfg(feature = "ldap")]
    let api_json_routes = api_json_routes.route(
        "/api/v1/settings/ldap",
        get(api_settings::get_ldap_config)
            .put(api_settings::put_ldap_config)
            .delete(api_settings::delete_ldap_config),
    );
    let api_json_routes = api_json_routes
        // Backup & Restore
        .route("/api/v1/backup", get(backup::get_backup))
        .route("/api/v1/restore", post(backup::post_restore))
        // Task management
        .route("/api/v1/tasks", get(tasks::list_tasks))
        .route("/api/v1/tasks/check", post(tasks::start_check))
        .route("/api/v1/tasks/gc", post(tasks::start_gc))
        .route("/api/v1/tasks/scheduler", get(tasks::get_scheduler_status))
        .route(
            "/api/v1/tasks/rebuild-dir-entries",
            post(tasks::start_rebuild_dir_entries),
        )
        .route(
            "/api/v1/tasks/{id}",
            get(tasks::get_task).delete(tasks::delete_task),
        )
        // Model event stream (real-time data model for the UI)
        .route("/api/v1/events/stream", get(event_stream::event_stream))
        // Nexus-compatible REST API
        .route(
            "/service/rest/v1/search/assets/download",
            get(nexus_compat::search_assets_download),
        )
        .route(
            "/service/rest/v1/search/assets",
            get(nexus_compat::search_assets),
        )
        .route(
            "/service/rest/v1/search",
            get(nexus_compat::search_components),
        )
        .route(
            "/service/rest/v1/repositories",
            get(nexus_compat::list_repositories),
        )
        .route("/service/rest/v1/status", get(nexus_compat::status))
        .route("/service/rest/v1/assets", get(nexus_compat::search_assets))
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT));

    // --- Unified artifact/format route — dynamic limit from settings ---
    //
    // The single `/repository/{*rest}` catch-all matches every shape of artifact
    // URL including the bare `/repository/{repo}` (e.g. PyPI twine upload) and
    // the deepest `/repository/{repo}/{a}/{b}/...` paths. Each handler splits
    // `rest` on the first `/` into `(repo_name, path)` and dispatches to the
    // matching format crate based on the repository's configured format.
    let artifact_routes = Router::new()
        .route(
            "/repository/{*rest}",
            head(artifacts::head_artifact)
                .get(artifacts::get_artifact)
                .put(artifacts::put_artifact)
                .post(artifacts::any_format_request)
                .patch(artifacts::any_format_request)
                .delete(artifacts::delete_artifact),
        )
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_body_limit,
        ));

    // Docker Registry V2 API — split by body size category.
    // Each endpoint registers both single-segment (/v2/{name}/...) and
    // two-segment (/v2/{repo}/{image}/...) patterns pointing to the same
    // handler; the DockerPath extractor resolves the path format.

    // Read-only routes — axum default limit (2 MiB) is fine for no-body requests.
    let docker_read_routes = Router::new()
        .route("/v2/", get(docker::v2_check))
        .route("/v2/_catalog", get(docker::catalog))
        .route("/v2/{repo}/{image}/tags/list", get(docker::list_tags))
        .route("/v2/{name}/tags/list", get(docker::list_tags))
        .route(
            "/v2/{repo}/{image}/blobs/{digest}",
            head(docker::head_blob).get(docker::get_blob),
        )
        .route(
            "/v2/{name}/blobs/{digest}",
            head(docker::head_blob).get(docker::get_blob),
        )
        .route(
            "/v2/{repo}/{image}/referrers/{digest}",
            get(docker::get_referrers),
        )
        .route("/v2/{name}/referrers/{digest}", get(docker::get_referrers));

    // Manifest routes — 32 MiB (PUT sends JSON manifests; HEAD/GET/DELETE have no body).
    let docker_manifest_routes = Router::new()
        .route(
            "/v2/{repo}/{image}/manifests/{reference}",
            head(docker::head_manifest)
                .get(docker::get_manifest)
                .put(docker::put_manifest)
                .delete(docker::delete_manifest),
        )
        .route(
            "/v2/{name}/manifests/{reference}",
            head(docker::head_manifest)
                .get(docker::get_manifest)
                .put(docker::put_manifest)
                .delete(docker::delete_manifest),
        )
        .layer(DefaultBodyLimit::max(MANIFEST_BODY_LIMIT));

    // Blob upload routes — dynamic artifact limit from settings.
    let docker_upload_routes = Router::new()
        .route(
            "/v2/{repo}/{image}/blobs/uploads/",
            post(docker::start_upload),
        )
        .route("/v2/{name}/blobs/uploads/", post(docker::start_upload))
        .route(
            "/v2/{repo}/{image}/blobs/uploads/{uuid}",
            patch(docker::patch_upload).put(docker::complete_upload),
        )
        .route(
            "/v2/{name}/blobs/uploads/{uuid}",
            patch(docker::patch_upload).put(docker::complete_upload),
        )
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_body_limit,
        ));

    let docker_routes = Router::new()
        .merge(docker_read_routes)
        .merge(docker_manifest_routes)
        .merge(docker_upload_routes);

    // Nexus-compatible component list + upload (dynamic body limit for large RPMs;
    // irrelevant for GET which has no request body).
    let nexus_upload_routes = Router::new()
        .route(
            "/service/rest/v1/components",
            get(nexus_compat::search_components).post(nexus_compat::upload_component),
        )
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_body_limit,
        ));

    let swagger_routes = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi_spec()))
        .layer(middleware::from_fn(
            crate::server::auth::require_authenticated,
        ));

    let router = Router::new()
        .merge(swagger_routes)
        .merge(public_routes)
        .merge(api_json_routes)
        .merge(artifact_routes)
        .merge(docker_routes)
        .merge(nexus_upload_routes)
        .fallback_service(depot_ui::ui_routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::audit::request_event_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::auth::resolve_identity,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_cors,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_rate_limit,
        ));

    let router = if metrics_handle.is_some() {
        router.layer(middleware::from_fn(
            crate::server::infra::metrics::metrics_middleware,
        ))
    } else {
        router
    };

    router
        .layer(middleware::from_fn(
            crate::server::infra::middleware::request_span_middleware,
        ))
        .layer(middleware::from_fn(
            crate::server::infra::middleware::request_id_middleware,
        ))
        .with_state(state)
}

/// Build a minimal router serving only the `/metrics` endpoint.
/// Intended for a dedicated internal-only listener.
pub fn build_metrics_router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(crate::server::infra::metrics::metrics_handler),
        )
        .with_state(handle)
}

/// Build a dedicated Docker-port router for a specific repo.
pub fn build_docker_port_router(state: AppState, repo_name: String) -> Router {
    // docker_port_routes returns a Router<FormatState>. Apply the dynamic body
    // limit (which wants AppState) then finalize the FormatState so the router
    // can be merged with AppState-stateful public routes.
    let docker_routes: Router = docker::docker_port_routes(repo_name)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_body_limit,
        ))
        .with_state(state.format_state());

    // Token endpoint is public (handles its own auth).
    let public_docker_routes: Router<AppState> = Router::new().route(
        "/v2/token",
        get(docker::v2_token).post(docker::v2_token_post),
    );
    let public_docker_routes: Router = public_docker_routes.with_state(state.clone());

    docker_routes
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::audit::request_event_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::auth::resolve_identity,
        ))
        .merge(public_docker_routes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_cors,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::server::infra::middleware::dynamic_rate_limit,
        ))
        .layer(middleware::from_fn(
            crate::server::infra::middleware::request_span_middleware,
        ))
        .layer(middleware::from_fn(
            crate::server::infra::middleware::request_id_middleware,
        ))
}
