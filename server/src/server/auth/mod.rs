// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

pub mod backend;
#[cfg(feature = "ldap")]
pub mod ldap;

use depot_core::auth::{decode_basic_auth, unauthorized_response, AuthenticatedUser};

use std::sync::LazyLock;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use metrics::counter;
use serde::{Deserialize, Serialize};

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;
use depot_core::store::kv::KvStore;

fn argon2_instance() -> Argon2<'static> {
    #[cfg(test)]
    {
        let params = argon2::Params::new(8, 1, 1, None).expect("valid argon2 params");
        Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
    }
    #[cfg(not(test))]
    {
        Argon2::default()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
}

/// Global JWT signing state. `current` is the active signing key; `previous`
/// (if present) is accepted during validation so tokens issued before the last
/// rotation remain valid until they expire.
#[derive(Clone, Serialize, Deserialize)]
pub struct JwtSecretState {
    pub current: Vec<u8>,
    pub previous: Option<Vec<u8>>,
    pub rotated_at: i64,
}

/// Derive the effective HMAC signing key from a global secret and a per-user
/// token secret. Both inputs are mixed via BLAKE3 keyed-hash so that changing
/// either one invalidates the derived key (and therefore all JWTs signed with it).
///
/// `global_secret` must be exactly 32 bytes (the size generated at startup).
pub fn derive_signing_key(global_secret: &[u8; 32], user_token_secret: &[u8]) -> [u8; 32] {
    blake3::keyed_hash(global_secret, user_token_secret).into()
}

/// Create a JWT for `username` using the current global secret combined with
/// the user's per-user token secret.
pub fn create_user_token(
    username: &str,
    global_secret: &[u8],
    user_token_secret: &[u8],
    expiry_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let gs: &[u8; 32] = global_secret
        .try_into()
        .map_err(|_| jsonwebtoken::errors::ErrorKind::InvalidKeyFormat)?;
    let key = derive_signing_key(gs, user_token_secret);
    create_token(username, &key, expiry_secs)
}

/// Decode the claims payload of a JWT **without** verifying its signature.
/// Used to extract the `sub` (username) before we can look up the per-user
/// secret needed for full verification.
pub fn decode_claims_unverified(token: &str) -> Option<Claims> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Generate a random 32-byte per-user token secret.
pub fn generate_token_secret() -> Vec<u8> {
    let mut buf = vec![0u8; 32];
    rand::Rng::fill(&mut rand::rng(), buf.as_mut_slice());
    buf
}

/// Pre-computed Argon2 hash used for timing-safe rejection of unknown users.
/// The actual value doesn't matter — we just need verify_password() to run
/// its full Argon2 computation so the response time is indistinguishable
/// from a real user with a wrong password.
#[allow(clippy::panic)]
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let salt = SaltString::generate(&mut OsRng);
    argon2_instance()
        .hash_password("__dummy__".as_bytes(), &salt)
        .map(|h| h.to_string())
        .unwrap_or_else(|e| panic!("failed to hash dummy password: {e}"))
});

pub fn dummy_hash() -> &'static str {
    &DUMMY_HASH
}

pub async fn hash_password(password: String) -> depot_core::error::Result<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        let hash = argon2_instance()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| {
                depot_core::error::DepotError::Internal(format!("password hash failed: {e}"))
            })?;
        Ok(hash.to_string())
    })
    .await?
}

pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let parsed = match PasswordHash::new(&hash) {
            Ok(h) => h,
            Err(_) => return false,
        };
        argon2_instance()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

pub fn generate_random_password() -> String {
    use rand::seq::IndexedRandom;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..16)
        .map(|_| *CHARSET.choose(&mut rng).unwrap_or(&b'a') as char)
        .collect()
}

pub fn create_token(
    username: &str,
    secret: &[u8],
    expiry_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as usize;
    let claims = Claims {
        sub: username.to_string(),
        iat: now,
        exp: now + expiry_secs as usize,
    };
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret),
    )
}

/// Convenience: derive the signing key from a global + user secret and validate.
/// Returns `true` if the token is valid. Silently returns `false` if the global
/// secret is not exactly 32 bytes (should never happen in practice).
fn try_validate_token(token: &str, global_secret: &[u8], user_token_secret: &[u8]) -> bool {
    let Ok(gs) = <&[u8; 32]>::try_from(global_secret) else {
        return false;
    };
    let key = derive_signing_key(gs, user_token_secret);
    validate_token(token, &key).is_ok()
}

pub fn validate_token(token: &str, secret: &[u8]) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::new(Algorithm::HS256),
    )?;
    Ok(data.claims)
}

/// Unified identity middleware: resolves Bearer JWT, Basic Auth, or anonymous.
///
/// - Valid credentials → inserts `AuthenticatedUser(username)`
/// - No credentials → inserts `AuthenticatedUser("anonymous")`
/// - Invalid credentials → returns 401
pub async fn resolve_identity(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // Suppress the WWW-Authenticate header for XHR requests from the SPA so
    // the browser does not show a native Basic-auth popup on 401.
    let is_xhr = req
        .headers()
        .get("X-Requested-With")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("xmlhttprequest"));

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let username = match auth_header.as_deref() {
        Some(h) if h.starts_with("Bearer ") => {
            let token = h.strip_prefix("Bearer ").unwrap_or("");

            // Always suppress WWW-Authenticate for Bearer token failures.
            // A client that sent a Bearer token already knows about token-based
            // auth and should never be challenged with Basic — doing so would
            // trigger the browser's native credentials popup when a stale JWT
            // is presented (e.g. after a server restart regenerates the signing
            // key), hijacking the SPA's normal 401 → redirect-to-login flow.
            let suppress = true;

            // Step 1: decode claims without verifying signature to get username.
            let claims = match decode_claims_unverified(token) {
                Some(c) => c,
                None => {
                    tracing::warn!("audit: malformed JWT token");
                    counter!("auth_attempts_total", "method" => "jwt", "result" => "rejected")
                        .increment(1);
                    return unauthorized_response(suppress);
                }
            };

            // Step 2: look up the user to get their per-user token_secret.
            let user = match state.auth.backend.lookup_user(&claims.sub).await {
                Ok(Some(u)) => u,
                _ => {
                    tracing::warn!(username = %claims.sub, "audit: JWT for unknown/deleted user");
                    counter!("auth_attempts_total", "method" => "jwt", "result" => "rejected")
                        .increment(1);
                    return unauthorized_response(suppress);
                }
            };

            // Step 3: verify signature with current global secret, fall back to previous.
            let jwt_state = state.auth.jwt_secret.load();
            let valid = try_validate_token(token, &jwt_state.current, &user.token_secret)
                || jwt_state
                    .previous
                    .as_ref()
                    .is_some_and(|prev| try_validate_token(token, prev, &user.token_secret));

            if valid {
                counter!("auth_attempts_total", "method" => "jwt", "result" => "ok").increment(1);
                claims.sub
            } else {
                tracing::warn!("audit: invalid JWT token");
                counter!("auth_attempts_total", "method" => "jwt", "result" => "rejected")
                    .increment(1);
                return unauthorized_response(suppress);
            }
        }
        Some(h) if h.starts_with("Basic ") => {
            let (user, pass) = match decode_basic_auth(h) {
                Some(c) => c,
                None => {
                    counter!("auth_attempts_total", "method" => "basic", "result" => "rejected")
                        .increment(1);
                    return unauthorized_response(is_xhr);
                }
            };
            match state.auth.backend.authenticate(&user, &pass).await {
                Ok(Some(_)) => {
                    counter!("auth_attempts_total", "method" => "basic", "result" => "ok")
                        .increment(1);
                    user
                }
                _ => {
                    tracing::warn!(username = %user, "audit: basic auth failed");
                    counter!("auth_attempts_total", "method" => "basic", "result" => "rejected")
                        .increment(1);
                    return unauthorized_response(is_xhr);
                }
            }
        }
        Some(_) => {
            tracing::warn!("audit: unknown auth scheme");
            counter!("auth_attempts_total", "method" => "unknown", "result" => "rejected")
                .increment(1);
            return unauthorized_response(is_xhr);
        }
        None => {
            counter!("auth_attempts_total", "method" => "anonymous", "result" => "ok").increment(1);
            "anonymous".to_string()
        }
    };

    tracing::Span::current().record("enduser.id", &username);
    req.extensions_mut().insert(AuthenticatedUser(username));
    next.run(req).await
}

/// Middleware that rejects anonymous (unauthenticated) requests.
/// Must be applied after `resolve_identity`.
pub async fn require_authenticated(req: Request, next: Next) -> Response {
    match req.extensions().get::<AuthenticatedUser>() {
        Some(user) if user.0 != "anonymous" => next.run(req).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Background task that periodically rotates the global JWT signing secret.
/// The previous secret is preserved so tokens issued before rotation remain
/// valid until they expire (configured via `jwt_expiry_secs`).
pub async fn run_jwt_rotation(
    kv: Arc<dyn KvStore>,
    jwt_secret: Arc<ArcSwap<JwtSecretState>>,
    settings: Arc<crate::server::config::settings::SettingsHandle>,
    cancel: CancellationToken,
) {
    // Check once per hour whether rotation is due.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
    interval.tick().await; // skip immediate tick

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let rotation_interval = settings.load().jwt_rotation_interval_secs as i64;
                let current = jwt_secret.load();
                let now = chrono::Utc::now().timestamp();
                if now - current.rotated_at < rotation_interval {
                    continue;
                }
                let new_state = JwtSecretState {
                    previous: Some(current.current.clone()),
                    current: generate_token_secret(),
                    rotated_at: now,
                };
                let serialized = match rmp_serde::to_vec(&new_state) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize JWT secret state");
                        continue;
                    }
                };
                if let Err(e) = depot_core::service::put_meta(kv.as_ref(), "jwt_secret", &serialized).await {
                    tracing::error!(error = %e, "failed to persist rotated JWT secret");
                    continue;
                }
                jwt_secret.store(Arc::new(new_state));
                tracing::info!("rotated global JWT signing secret");
            }
            _ = cancel.cancelled() => {
                tracing::info!("JWT rotation worker shutting down");
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hash_verify_password_roundtrip() {
        let hash = hash_password("mysecret".to_string()).await.unwrap();
        assert!(verify_password("mysecret".to_string(), hash).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_verify_password_wrong() {
        let hash = hash_password("correct".to_string()).await.unwrap();
        assert!(!verify_password("wrong".to_string(), hash).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_verify_password_invalid_hash() {
        assert!(!verify_password("anything".to_string(), "not-a-valid-hash".to_string()).await);
    }

    #[test]
    fn test_generate_random_password_length() {
        let pwd = generate_random_password();
        assert_eq!(pwd.len(), 16);
    }

    #[test]
    fn test_generate_random_password_alphanumeric() {
        let pwd = generate_random_password();
        assert!(pwd.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn test_generate_random_password_unique() {
        let a = generate_random_password();
        let b = generate_random_password();
        assert_ne!(a, b);
    }

    #[test]
    fn test_decode_basic_auth_valid() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("alice:secret123");
        let header = format!("Basic {}", encoded);
        let (user, pass) = decode_basic_auth(&header).unwrap();
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

    #[test]
    fn test_create_validate_token_roundtrip() {
        let secret = b"test-secret-key";
        let token = create_token("bob", secret, 86400).unwrap();
        let claims = validate_token(&token, secret).unwrap();
        assert_eq!(claims.sub, "bob");
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn test_validate_token_wrong_secret() {
        let token = create_token("bob", b"secret1", 86400).unwrap();
        assert!(validate_token(&token, b"secret2").is_err());
    }

    #[test]
    fn test_validate_token_garbage() {
        assert!(validate_token("not.a.token", b"secret").is_err());
    }

    #[test]
    fn test_derive_signing_key_deterministic() {
        let global = [1u8; 32];
        let user = b"user-secret";
        let k1 = derive_signing_key(&global, user);
        let k2 = derive_signing_key(&global, user);
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_derive_signing_key_changes_with_user_secret() {
        let global = [1u8; 32];
        let k1 = derive_signing_key(&global, b"secret-a");
        let k2 = derive_signing_key(&global, b"secret-b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_derive_signing_key_changes_with_global_secret() {
        let user = b"user-secret";
        let k1 = derive_signing_key(&[1u8; 32], user);
        let k2 = derive_signing_key(&[2u8; 32], user);
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_create_user_token_roundtrip() {
        let global = [42u8; 32];
        let user_secret = b"per-user";
        let token = create_user_token("alice", &global, user_secret, 86400).unwrap();
        let key = derive_signing_key(&global, user_secret);
        let claims = validate_token(&token, &key).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn test_create_user_token_rejected_after_user_secret_change() {
        let global = [42u8; 32];
        let token = create_user_token("alice", &global, b"old-secret", 86400).unwrap();
        let new_key = derive_signing_key(&global, b"new-secret");
        assert!(validate_token(&token, &new_key).is_err());
    }

    #[test]
    fn test_create_user_token_accepted_with_previous_global() {
        let old_global = [1u8; 32];
        let new_global = [2u8; 32];
        let user_secret = b"user";
        // Token signed with old global.
        let token = create_user_token("bob", &old_global, user_secret, 86400).unwrap();
        // Validate with old global key (simulating "previous" fallback).
        let prev_key = derive_signing_key(&old_global, user_secret);
        assert!(validate_token(&token, &prev_key).is_ok());
        // Reject with new global key.
        let curr_key = derive_signing_key(&new_global, user_secret);
        assert!(validate_token(&token, &curr_key).is_err());
    }

    #[test]
    fn test_decode_claims_unverified() {
        let token = create_token("carol", b"any-secret-here!", 86400).unwrap();
        let claims = decode_claims_unverified(&token).unwrap();
        assert_eq!(claims.sub, "carol");
    }

    #[test]
    fn test_decode_claims_unverified_garbage() {
        assert!(decode_claims_unverified("not-a-jwt").is_none());
        assert!(decode_claims_unverified("a.b.c").is_none());
    }

    #[test]
    fn test_generate_token_secret_length_and_uniqueness() {
        let a = generate_token_secret();
        let b = generate_token_secret();
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn test_jwt_secret_state_serde_roundtrip() {
        let state = JwtSecretState {
            current: vec![1u8; 32],
            previous: Some(vec![2u8; 32]),
            rotated_at: 1234567890,
        };
        let packed = rmp_serde::to_vec(&state).unwrap();
        let decoded: JwtSecretState = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(decoded.current, state.current);
        assert_eq!(decoded.previous, state.previous);
        assert_eq!(decoded.rotated_at, state.rotated_at);
    }
}
