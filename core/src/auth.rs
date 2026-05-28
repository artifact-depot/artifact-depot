// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Core auth types shared across crates.
//!
//! The full auth stack (JWT, LDAP, Argon2, backends, middleware) lives in
//! the depot crate. Only lightweight types that downstream crates need to
//! inspect authenticated requests belong here.

use base64::Engine;

use crate::error::DepotError;
use crate::store::kv::{Capability, RoleRecord};

/// The authenticated identity attached to each request via `resolve_identity`.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub String);

/// Decode an HTTP `Authorization: Basic <base64>` header.
///
/// Returns `Some((username, password))` on success, `None` if the header
/// is malformed, not a `Basic` scheme, or doesn't decode to `user:pass`.
pub fn decode_basic_auth(header_value: &str) -> Option<(String, String)> {
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// Check whether any of the given roles grant the required capability on the repo.
pub fn check_grants(
    roles: &[RoleRecord],
    repo: &str,
    required: Capability,
) -> Result<(), DepotError> {
    for role in roles {
        for grant in &role.capabilities {
            if grant.capability == required && (grant.repo == "*" || grant.repo == repo) {
                return Ok(());
            }
        }
    }
    Err(DepotError::Forbidden("access denied".into()))
}

/// Build a 401 Unauthorized response.
///
/// When `suppress_www_authenticate` is false, the response carries a
/// `WWW-Authenticate: Basic` header (needed by CLI/Docker clients). When true,
/// the header is omitted (e.g. XHR requests from the SPA, where a native
/// credentials popup would hijack the SPA's own 401 → redirect flow).
#[cfg(feature = "http")]
pub fn unauthorized_response(suppress_www_authenticate: bool) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    if suppress_www_authenticate {
        StatusCode::UNAUTHORIZED.into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"Artifact Depot\"")],
        )
            .into_response()
    }
}

/// Map a permission-check error to an HTTP response, issuing a Basic
/// challenge to anonymous callers so browsers prompt for credentials.
///
/// For `DepotError::Forbidden` from an anonymous request, returns 401 +
/// `WWW-Authenticate: Basic realm="Artifact Depot"`. For an authenticated
/// caller, or for any other error kind, falls back to the default
/// `IntoResponse` mapping (typically a plain 403 with no challenge).
#[cfg(feature = "http")]
pub fn map_permission_error(err: DepotError, username: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    if matches!(err, DepotError::Forbidden(_)) && username == "anonymous" {
        return unauthorized_response(false);
    }
    err.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_basic_auth_valid() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:secret123");
        let header = format!("Basic {}", encoded);
        let (user, pass) = decode_basic_auth(&header).expect("decodes");
        assert_eq!(user, "alice");
        assert_eq!(pass, "secret123");
    }

    #[test]
    fn test_decode_basic_auth_no_prefix() {
        assert!(decode_basic_auth("NotBasic abc").is_none());
    }

    #[test]
    fn test_decode_basic_auth_invalid_base64() {
        assert!(decode_basic_auth("Basic !!!invalid!!!").is_none());
    }

    #[test]
    fn test_decode_basic_auth_no_colon() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("nocolon");
        let header = format!("Basic {}", encoded);
        assert!(decode_basic_auth(&header).is_none());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_map_permission_error_anonymous_gets_basic_challenge() {
        use axum::http::{header, StatusCode};
        let resp =
            map_permission_error(DepotError::Forbidden("access denied".into()), "anonymous");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www_auth = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("WWW-Authenticate header present");
        assert_eq!(www_auth, "Basic realm=\"Artifact Depot\"");
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_map_permission_error_authenticated_gets_forbidden() {
        use axum::http::{header, StatusCode};
        let resp = map_permission_error(DepotError::Forbidden("access denied".into()), "alice");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_none());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_map_permission_error_non_forbidden_passes_through() {
        use axum::http::StatusCode;
        let resp = map_permission_error(DepotError::NotFound("missing".into()), "anonymous");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
