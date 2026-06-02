// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Direct dispatcher for Docker V2 paths accessed via `/repository/{repo}/v2/...`.
//!
//! Parses the V2 sub-path and calls the appropriate `do_*` handler directly,
//! avoiding the need for URI rewriting, request reconstruction, or `Router::oneshot`.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};

use depot_core::format_state::FormatState;

use super::blobs::{do_get_blob, do_head_blob};
use super::manifests::{do_delete_manifest, do_get_manifest, do_head_manifest, do_put_manifest};
use super::tags::{do_list_tags, paginate_catalog};
use super::types::{DockerPaginationParams, UploadInitParams};
use super::uploads::{do_complete_upload, do_patch_upload, do_start_upload};

use depot_core::store::kv::Capability;

/// Maximum manifest body size (32 MiB), matching the Docker manifest route limit.
const MANIFEST_BODY_LIMIT: usize = 32 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
/// Handle a Docker V2 request arriving via `/repository/{repo}/v2/...`.
///
/// Returns `Some(response)` if the path was recognized as a Docker V2 path
/// and the repo is Docker format. Returns `None` to fall through to the
/// raw artifact handler.
pub async fn try_handle_docker_v2_path(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    path: &str,
    method: &Method,
    query: Option<&str>,
    uri_authority: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Option<Response> {
    // Only handle paths starting with "v2/" or exactly "v2".
    let v2_rest = if path == "v2" {
        ""
    } else {
        path.strip_prefix("v2/")?
    };

    // Split into segments for pattern matching.
    let segments: Vec<&str> = if v2_rest.is_empty() {
        Vec::new()
    } else {
        v2_rest.split('/').collect()
    };

    Some(
        dispatch(
            state,
            username,
            repo_name,
            &segments,
            method,
            query,
            uri_authority,
            headers,
            body,
        )
        .await,
    )
}

#[allow(clippy::too_many_arguments)]
/// Dispatch a parsed V2 path to the appropriate handler.
async fn dispatch(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    segments: &[&str],
    method: &Method,
    query: Option<&str>,
    uri_authority: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    match segments {
        // GET /v2/ — version check
        [] | [""] => {
            if *method != Method::GET {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            do_v2_check(username, headers, uri_authority)
        }

        // GET /v2/token — token exchange (clients may request this under
        // /repository/{repo}/v2/token instead of the top-level /v2/token)
        ["token"] => {
            if *method != Method::GET {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            do_token_exchange(state, headers).await
        }

        // GET /v2/_catalog — scoped to this repo
        ["_catalog"] => {
            if *method != Method::GET {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            do_repo_catalog(state, username, repo_name, query, headers, uri_authority).await
        }

        // /v2/{name}/tags/list
        [name, "tags", "list"] => {
            if *method != Method::GET {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            let pagination = parse_pagination(query);
            do_list_tags(
                state,
                username,
                repo_name,
                Some(name),
                &pagination,
                headers,
                uri_authority,
            )
            .await
        }

        // /v2/{ns}/{img}/tags/list
        [ns, img, "tags", "list"] => {
            if *method != Method::GET {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            let image = format!("{ns}/{img}");
            let pagination = parse_pagination(query);
            do_list_tags(
                state,
                username,
                repo_name,
                Some(&image),
                &pagination,
                headers,
                uri_authority,
            )
            .await
        }

        // /v2/{name}/blobs/{digest} (but not /blobs/uploads)
        [name, "blobs", digest] if !digest.is_empty() && *digest != "uploads" => match *method {
            Method::HEAD => {
                do_head_blob(
                    state,
                    username,
                    repo_name,
                    Some(name),
                    digest,
                    headers,
                    uri_authority,
                )
                .await
            }
            Method::GET => {
                do_get_blob(
                    state,
                    username,
                    repo_name,
                    Some(name),
                    digest,
                    headers,
                    uri_authority,
                )
                .await
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        },

        // /v2/{ns}/{img}/blobs/{digest} (but not /blobs/uploads)
        [ns, img, "blobs", digest] if !digest.is_empty() && *digest != "uploads" => {
            let image = format!("{ns}/{img}");
            match *method {
                Method::HEAD => {
                    do_head_blob(
                        state,
                        username,
                        repo_name,
                        Some(&image),
                        digest,
                        headers,
                        uri_authority,
                    )
                    .await
                }
                Method::GET => {
                    do_get_blob(
                        state,
                        username,
                        repo_name,
                        Some(&image),
                        digest,
                        headers,
                        uri_authority,
                    )
                    .await
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }

        // /v2/{name}/manifests/{reference}
        [name, "manifests", reference] => {
            dispatch_manifest(
                state,
                username,
                repo_name,
                Some(name),
                reference,
                method,
                headers,
                uri_authority,
                body,
            )
            .await
        }

        // /v2/{ns}/{img}/manifests/{reference}
        [ns, img, "manifests", reference] => {
            let image = format!("{ns}/{img}");
            dispatch_manifest(
                state,
                username,
                repo_name,
                Some(&image),
                reference,
                method,
                headers,
                uri_authority,
                body,
            )
            .await
        }

        // /v2/{name}/blobs/uploads/ (trailing empty segment from trailing slash)
        [name, "blobs", "uploads", ""] | [name, "blobs", "uploads"] => {
            if *method != Method::POST {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            let params = parse_upload_init(query);
            do_start_upload(
                state,
                username,
                repo_name,
                Some(name),
                &params,
                body,
                headers,
                uri_authority,
            )
            .await
        }

        // /v2/{ns}/{img}/blobs/uploads/
        [ns, img, "blobs", "uploads", ""] | [ns, img, "blobs", "uploads"] => {
            if *method != Method::POST {
                return StatusCode::METHOD_NOT_ALLOWED.into_response();
            }
            let image = format!("{ns}/{img}");
            let params = parse_upload_init(query);
            do_start_upload(
                state,
                username,
                repo_name,
                Some(&image),
                &params,
                body,
                headers,
                uri_authority,
            )
            .await
        }

        // /v2/{name}/blobs/uploads/{uuid}
        [name, "blobs", "uploads", uuid] => {
            dispatch_upload(
                state,
                username,
                repo_name,
                Some(name),
                uuid,
                method,
                query,
                headers,
                uri_authority,
                body,
            )
            .await
        }

        // /v2/{ns}/{img}/blobs/uploads/{uuid}
        [ns, img, "blobs", "uploads", uuid] => {
            let image = format!("{ns}/{img}");
            dispatch_upload(
                state,
                username,
                repo_name,
                Some(&image),
                uuid,
                method,
                query,
                headers,
                uri_authority,
                body,
            )
            .await
        }

        _ => not_found(),
    }
}

#[allow(clippy::too_many_arguments)]
/// Dispatch manifest operations (HEAD/GET/PUT/DELETE).
async fn dispatch_manifest(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    reference: &str,
    method: &Method,
    headers: &HeaderMap,
    uri_authority: Option<&str>,
    body: Body,
) -> Response {
    match *method {
        Method::HEAD => {
            do_head_manifest(
                state,
                username,
                repo_name,
                image,
                reference,
                headers,
                uri_authority,
            )
            .await
        }
        Method::GET => {
            do_get_manifest(
                state,
                username,
                repo_name,
                image,
                reference,
                headers,
                uri_authority,
            )
            .await
        }
        Method::PUT => {
            let data = match axum::body::to_bytes(body, MANIFEST_BODY_LIMIT).await {
                Ok(b) => b,
                Err(_) => {
                    return super::helpers::docker_error(
                        "SIZE_LIMIT",
                        "manifest too large",
                        StatusCode::PAYLOAD_TOO_LARGE,
                    )
                }
            };
            do_put_manifest(
                state,
                username,
                repo_name,
                image,
                reference,
                headers,
                &data,
                uri_authority,
            )
            .await
        }
        Method::DELETE => {
            do_delete_manifest(
                state,
                username,
                repo_name,
                image,
                reference,
                headers,
                uri_authority,
            )
            .await
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

#[allow(clippy::too_many_arguments)]
/// Dispatch blob upload chunk/complete operations (PATCH/PUT).
async fn dispatch_upload(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    uuid: &str,
    method: &Method,
    query: Option<&str>,
    headers: &HeaderMap,
    uri_authority: Option<&str>,
    body: Body,
) -> Response {
    match *method {
        Method::PATCH => {
            do_patch_upload(
                state,
                username,
                repo_name,
                image,
                uuid,
                body,
                headers,
                uri_authority,
            )
            .await
        }
        Method::PUT => {
            let digest = match query_param(query, "digest") {
                Some(d) => d,
                None => {
                    return super::helpers::docker_error(
                        "DIGEST_INVALID",
                        "digest query parameter required",
                        StatusCode::BAD_REQUEST,
                    )
                }
            };
            do_complete_upload(
                state,
                username,
                repo_name,
                image,
                uuid,
                &digest,
                body,
                headers,
                uri_authority,
            )
            .await
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// Inline V2 check — produces the same response as `handlers::v2_check`
/// without needing axum extractors.
fn do_v2_check(username: &str, req_headers: &HeaderMap, uri_authority: Option<&str>) -> Response {
    if username == "anonymous" {
        return super::helpers::docker_unauthorized_response(req_headers, uri_authority);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Distribution-Api-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    (StatusCode::OK, headers, "{}").into_response()
}

/// Inline token exchange — same logic as `handlers::v2_token`.
async fn do_token_exchange(state: &FormatState, req_headers: &HeaderMap) -> Response {
    let auth_header = req_headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let (username, password) = match auth_header.and_then(depot_core::auth::decode_basic_auth) {
        Some(creds) => creds,
        None => {
            // Issue an anonymous token.
            if let Ok(Some(user)) =
                depot_core::service::get_user(state.kv.as_ref(), "anonymous").await
            {
                if let Ok(token) = state.auth.create_user_token(&user).await {
                    let issued_at = chrono::Utc::now().to_rfc3339();
                    return axum::Json(serde_json::json!({
                        "token": token,
                        "expires_in": 86400,
                        "issued_at": issued_at,
                    }))
                    .into_response();
                }
            }
            return depot_core::auth::unauthorized_response(false);
        }
    };

    match state.auth.authenticate(&username, &password).await {
        Ok(Some(user)) => match state.auth.create_user_token(&user).await {
            Ok(token) => {
                let issued_at = chrono::Utc::now().to_rfc3339();
                axum::Json(serde_json::json!({
                    "token": token,
                    "expires_in": 86400,
                    "issued_at": issued_at,
                }))
                .into_response()
            }
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        _ => depot_core::auth::unauthorized_response(false),
    }
}

fn not_found() -> Response {
    super::helpers::docker_error("NAME_UNKNOWN", "not found", StatusCode::NOT_FOUND)
}

/// Handle `GET /repository/{repo}/v2/_catalog` — Docker Registry V2 catalog
/// scoped to a single depot repo. Returns the set of image names whose tag
/// or manifest records live within `repo_name`, paginated with `n`/`last`.
async fn do_repo_catalog(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = super::helpers::check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Read,
        headers,
        uri_authority,
    )
    .await
    {
        return r;
    }
    if let Err(r) = super::helpers::validate_docker_repo(state, repo_name).await {
        return r;
    }

    let images = match crate::store::list_image_names(state.kv.as_ref(), repo_name).await {
        Ok(i) => i,
        Err(e) => {
            return super::helpers::docker_error(
                "UNKNOWN",
                &e.to_string(),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
    };
    let pagination = parse_pagination(query);
    let link_path = format!("/repository/{}/v2/_catalog", repo_name);
    paginate_catalog(images, &pagination, &link_path)
}

/// Extract a single query parameter value by key, percent-decoding the value.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(percent_decode(v))
        } else {
            None
        }
    })
}

/// Decode `%XX` sequences in a query parameter value.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().and_then(hex_val);
            let lo = bytes.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(char::from(h << 4 | l));
            }
        } else {
            out.push(char::from(b));
        }
    }
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_pagination(query: Option<&str>) -> DockerPaginationParams {
    DockerPaginationParams {
        n: query_param(query, "n").and_then(|v| v.parse().ok()),
        last: query_param(query, "last"),
    }
}

fn parse_upload_init(query: Option<&str>) -> UploadInitParams {
    UploadInitParams {
        mount: query_param(query, "mount"),
        from: query_param(query, "from"),
        digest: query_param(query, "digest"),
    }
}
