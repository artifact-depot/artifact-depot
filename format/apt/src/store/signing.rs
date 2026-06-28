// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use depot_core::error::{self, DepotError};

/// Generate a GPG key pair for signing APT repos.
/// Returns (private_key_armor, public_key_armor).
pub fn generate_gpg_keypair(repo_name: &str) -> error::Result<(String, String)> {
    use pgp::composed::{KeyType, SecretKeyParamsBuilder};
    use pgp::crypto::sym::SymmetricKeyAlgorithm;

    let mut rng = rand::thread_rng();

    let mut key_params = SecretKeyParamsBuilder::default();
    key_params
        .key_type(KeyType::Rsa(2048))
        .can_certify(true)
        .can_sign(true)
        .primary_user_id(format!("Artifact Depot <depot@{repo_name}>"))
        .preferred_symmetric_algorithms(smallvec::smallvec![SymmetricKeyAlgorithm::AES256]);

    let secret_key_params = key_params
        .build()
        .map_err(|e| DepotError::BadRequest(format!("failed to build key params: {e}")))?;

    // pgp 0.19's generate() self-signs the key and returns a SignedSecretKey
    // directly (the separate SecretKey::sign step from 0.18 was removed). With
    // no passphrase set on the builder it defaults to an unprotected key.
    let signed_key = secret_key_params
        .generate(&mut rng)
        .map_err(|e| DepotError::BadRequest(format!("failed to generate key: {e}")))?;

    let private_armor = signed_key
        .to_armored_string(Default::default())
        .map_err(|e| DepotError::BadRequest(format!("failed to armor private key: {e}")))?;

    let public_key: pgp::composed::SignedPublicKey = signed_key.into();
    let public_armor = public_key
        .to_armored_string(Default::default())
        .map_err(|e| DepotError::BadRequest(format!("failed to armor public key: {e}")))?;

    Ok((private_armor, public_armor))
}

/// Derive the armored public key from an armored secret (private) key.
///
/// Used when importing an externally-supplied signing key: the operator provides
/// only the private half, and we need the public half to serve at `public.key`
/// and to let `apt` verify the signature. Doubles as validation — an unparseable
/// or non-secret key is rejected here before anything is stored.
pub fn public_key_from_secret(secret_key_armor: &str) -> error::Result<String> {
    use pgp::composed::{Deserializable, SignedPublicKey, SignedSecretKey};

    let (secret_key, _) = SignedSecretKey::from_string(secret_key_armor)
        .map_err(|e| DepotError::BadRequest(format!("failed to parse signing key: {e}")))?;
    let public_key: SignedPublicKey = secret_key.into();
    public_key
        .to_armored_string(Default::default())
        .map_err(|e| DepotError::BadRequest(format!("failed to armor public key: {e}")))
}

/// Sign a Release file, producing (InRelease clearsigned, Release.gpg detached).
pub(super) fn sign_release(
    signing_key_armor: &str,
    release_text: &str,
) -> error::Result<(String, String)> {
    use pgp::composed::{
        CleartextSignedMessage, Deserializable, DetachedSignature, SignedSecretKey,
    };
    use pgp::types::Password;

    let mut rng = rand::thread_rng();

    let (secret_key, _) = SignedSecretKey::from_string(signing_key_armor)
        .map_err(|e| DepotError::BadRequest(format!("failed to parse signing key: {e}")))?;

    // Create clearsigned InRelease using ClearTextSignedMessage.
    // SignedSecretKey derefs to the underlying signing key (SecretKeyTrait).
    let clearsigned =
        CleartextSignedMessage::sign(&mut rng, release_text, &*secret_key, &Password::empty())
            .map_err(|e| {
                DepotError::BadRequest(format!("failed to create clearsigned message: {e}"))
            })?;

    let in_release = clearsigned
        .to_armored_string(Default::default())
        .map_err(|e| DepotError::BadRequest(format!("failed to armor clearsigned message: {e}")))?;

    // Create detached signature
    // Re-parse for the standalone signature
    let sigs = clearsigned.signatures();
    let detached_armor = if let Some(sig) = sigs.first() {
        // pgp 0.18 armors standalone signatures via DetachedSignature.
        DetachedSignature::new(sig.clone())
            .to_armored_string(Default::default())
            .map_err(|e| {
                DepotError::BadRequest(format!("failed to armor detached signature: {e}"))
            })?
    } else {
        String::new()
    };

    Ok((in_release, detached_armor))
}

/// Produce only the InRelease (clearsigned) string.
pub(super) fn sign_inrelease(signing_key_armor: &str, release_text: &str) -> error::Result<String> {
    sign_release(signing_key_armor, release_text).map(|(inrelease, _)| inrelease)
}

/// Produce only the detached Release.gpg signature.
pub(super) fn sign_release_detached(
    signing_key_armor: &str,
    release_text: &str,
) -> error::Result<String> {
    sign_release(signing_key_armor, release_text).map(|(_, gpg)| gpg)
}

/// Public wrapper for sign_release (used by proxy API and YUM signing).
pub fn sign_release_pub(
    signing_key_armor: &str,
    release_text: &str,
) -> error::Result<(String, String)> {
    sign_release(signing_key_armor, release_text)
}
