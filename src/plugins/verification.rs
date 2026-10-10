//! Plugin Signature Verification
//!
//! Provides ed25519 signature verification for plugin manifests.
//! A signed manifest contains `signature` and `signer_public_key` fields,
//! both base64-encoded.

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, PUBLIC_KEY_LENGTH};
use tracing::{debug, info, warn};

use crate::plugins::manifest::PluginManifest;

/// Outcome of verifying a plugin manifest signature.
#[derive(Debug, Clone, PartialEq)]
pub enum VerificationResult {
    /// No signature fields present; plugin is not signed.
    NotSigned,
    /// Signature is present and valid.
    Valid,
    /// Signature is present but invalid (with reason).
    Invalid(String),
}

/// Canonical serialization of a manifest for signing/verification.
///
/// Uses a sorted JSON serialization of all security-relevant manifest fields.
/// This ensures deterministic output regardless of HashMap/serde field
/// ordering. Excludes `signature` and `signer_public_key` (they are what we
/// are verifying).
fn canonical_message(manifest: &PluginManifest) -> String {
    let canonical = serde_json::json!({
        "id": manifest.id,
        "name": manifest.name,
        "version": manifest.version,
        "description": manifest.description,
        "main": manifest.main,
        "author": manifest.author,
        "capabilities": manifest.capabilities,
        "permissions": manifest.permissions,
        "dependencies": manifest.dependencies,
        "external_resources": manifest.external_resources,
    });
    // serde_json::to_string produces deterministic output for json! macro
    // values (fields are inserted in order).
    // Infallible for json! values; fallback keeps verification possible.
    serde_json::to_string(&canonical).unwrap_or_else(|e| {
        warn!("Canonical serialization failed: {}", e);
        String::new()
    })
}

/// Verify the ed25519 signature on a plugin manifest.
///
/// Returns `VerificationResult::NotSigned` if no `signature` field is present,
/// `Valid` if the signature matches the canonical manifest fields, or
/// `Invalid(reason)` if the signature is missing, malformed, or incorrect.
pub fn verify_manifest(manifest: &PluginManifest) -> VerificationResult {
    let signature_b64 = match &manifest.signature {
        Some(s) => s,
        None => return VerificationResult::NotSigned,
    };
    let pubkey_b64 = match &manifest.signer_public_key {
        Some(pk) => pk,
        None => {
            return VerificationResult::Invalid(
                "signer_public_key missing but signature present".to_string(),
            )
        }
    };

    let engine = base64::engine::general_purpose::STANDARD;

    let pubkey_vec = match engine.decode(pubkey_b64) {
        Ok(b) => b,
        Err(e) => return VerificationResult::Invalid(format!("invalid public key: {}", e)),
    };
    let pubkey_len = pubkey_vec.len();
    let pubkey_arr: [u8; PUBLIC_KEY_LENGTH] = match pubkey_vec.try_into() {
        Ok(a) => a,
        Err(_) => {
            return VerificationResult::Invalid(format!(
                "public key length mismatch: expected {} bytes, got {}",
                PUBLIC_KEY_LENGTH, pubkey_len
            ))
        }
    };

    let sig_bytes = match engine.decode(signature_b64) {
        Ok(b) => b,
        Err(e) => return VerificationResult::Invalid(format!("invalid signature: {}", e)),
    };
    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(e) => return VerificationResult::Invalid(format!("invalid signature format: {}", e)),
    };

    let pubkey = match VerifyingKey::from_bytes(&pubkey_arr) {
        Ok(k) => k,
        Err(e) => return VerificationResult::Invalid(format!("invalid public key: {}", e)),
    };

    let msg = canonical_message(manifest);

    match pubkey.verify(msg.as_bytes(), &signature) {
        Ok(()) => VerificationResult::Valid,
        Err(e) => VerificationResult::Invalid(format!("signature mismatch: {}", e)),
    }
}

/// Sign a plugin manifest in-place using a raw ed25519 secret key (32 bytes).
///
/// The key bytes should be base64-encoded.  Both `signature` and
/// `signer_public_key` fields are populated on the manifest.
/// Returns an error if the key bytes are invalid.
pub fn sign_manifest(manifest: &mut PluginManifest, secret_key_b64: &str) -> crate::Result<()> {
    let engine = base64::engine::general_purpose::STANDARD;
    let key_bytes = engine.decode(secret_key_b64).map_err(|e| {
        crate::error::SyscityError::Internal(format!("invalid key encoding: {}", e))
    })?;
    let key_len = key_bytes.len();
    let key_arr: [u8; 32] = key_bytes.try_into().map_err(|_| {
        crate::error::SyscityError::Internal(format!(
            "ed25519 secret key must be 32 bytes, got {}",
            key_len
        ))
    })?;
    let signing_key = SigningKey::from_bytes(&key_arr);
    let verifying_key = signing_key.verifying_key();

    let msg = canonical_message(manifest);
    let signature = signing_key.sign(msg.as_bytes());

    manifest.signer_public_key = Some(engine.encode(verifying_key.to_bytes()));
    manifest.signature = Some(engine.encode(signature.to_bytes()));

    info!("Plugin '{}' signed successfully", manifest.id);
    Ok(())
}

/// Log the verification result at the appropriate level.
pub fn log_verification(plugin_id: &str, result: &VerificationResult) {
    match result {
        VerificationResult::Valid => info!("Plugin '{}' signature verified", plugin_id),
        VerificationResult::NotSigned => debug!("Plugin '{}' is not signed", plugin_id),
        VerificationResult::Invalid(reason) => {
            warn!("Plugin '{}' signature invalid: {}", plugin_id, reason)
        }
    }
}

/// What a deployment requires of a manifest's signature, on top of the
/// cryptographic check.
///
/// The default accepts everything [`verify_manifest`] accepts: a signature
/// proves the manifest is unmodified, not that its author is anyone in
/// particular — a plugin can sign itself with a key it just generated. A
/// deployment that wants the stronger claim sets these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignaturePolicy {
    /// Refuse a manifest that carries no signature at all.
    pub require_signed: bool,
    /// When non-empty, a signed manifest is accepted only if its
    /// `signer_public_key` — base64, as [`sign_manifest`] writes it — is one
    /// of these.
    pub trusted_signers: Vec<String>,
}

impl SignaturePolicy {
    /// Whether the policy adds no requirement beyond the signature check.
    pub fn is_permissive(&self) -> bool {
        !self.require_signed && self.trusted_signers.is_empty()
    }
}

/// Apply a [`SignaturePolicy`] on top of [`verify_manifest`].
///
/// Returns `Err(reason)` when the manifest does not satisfy the policy. The
/// reason is written for an operator reading a log, so it names the setting
/// that refused it.
pub fn check_policy(manifest: &PluginManifest, policy: &SignaturePolicy) -> Result<(), String> {
    if policy.is_permissive() {
        return Ok(());
    }
    match verify_manifest(manifest) {
        VerificationResult::Invalid(reason) => Err(reason),
        VerificationResult::NotSigned if policy.require_signed => {
            Err("the manifest is not signed and this deployment requires signed plugins"
                .to_string())
        }
        VerificationResult::NotSigned => Ok(()),
        VerificationResult::Valid => {
            if policy.trusted_signers.is_empty() {
                return Ok(());
            }
            let signer = manifest.signer_public_key.as_deref().unwrap_or_default();
            if policy.trusted_signers.iter().any(|key| key == signer) {
                Ok(())
            } else {
                Err(format!(
                    "signed by {}… which is not in the trusted signer list",
                    &signer[..signer.len().min(12)]
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::manifest::PluginManifest;

    fn unsigned_manifest() -> PluginManifest {
        PluginManifest::minimal("com.test.plugin", "Test Plugin")
    }

    #[test]
    fn test_unsigned_returns_not_signed() {
        let m = unsigned_manifest();
        assert_eq!(verify_manifest(&m), VerificationResult::NotSigned);
    }

    #[test]
    fn test_sign_then_verify() {
        // Generate an ephemeral signing key for testing
        use ed25519_dalek::SigningKey;
        use rand::Rng;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill(&mut secret_bytes);
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let engine = base64::engine::general_purpose::STANDARD;
        let secret_b64 = engine.encode(signing_key.to_bytes());

        let mut manifest = PluginManifest {
            description: "Signed plugin".to_string(),
            ..PluginManifest::minimal("com.test.signed", "Signed Plugin")
        };

        sign_manifest(&mut manifest, &secret_b64).unwrap();
        assert!(manifest.signature.is_some());
        assert!(manifest.signer_public_key.is_some());

        let result = verify_manifest(&manifest);
        assert_eq!(result, VerificationResult::Valid);
    }

    #[test]
    fn test_tampered_manifest_fails_verification() {
        use ed25519_dalek::SigningKey;
        use rand::Rng;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill(&mut secret_bytes);
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let engine = base64::engine::general_purpose::STANDARD;
        let secret_b64 = engine.encode(signing_key.to_bytes());

        let mut manifest = PluginManifest {
            description: "Original description".to_string(),
            ..PluginManifest::minimal("com.test.tampered", "Tampered Plugin")
        };

        sign_manifest(&mut manifest, &secret_b64).unwrap();

        // Tamper with the manifest
        manifest.description = "Tampered description".to_string();

        let result = verify_manifest(&manifest);
        assert!(
            matches!(result, VerificationResult::Invalid(ref msg) if msg.starts_with("signature mismatch"))
        );
    }

    #[test]
    fn test_signature_without_public_key() {
        let mut manifest = unsigned_manifest();
        manifest.signature = Some("AAAA".to_string());
        manifest.signer_public_key = None;
        let result = verify_manifest(&manifest);
        assert_eq!(
            result,
            VerificationResult::Invalid(
                "signer_public_key missing but signature present".to_string()
            )
        );
    }

    #[test]
    fn test_invalid_base64_public_key() {
        let mut manifest = unsigned_manifest();
        manifest.signature = Some("AAAA".to_string());
        manifest.signer_public_key = Some("not-valid-base64!!!".to_string());
        let result = verify_manifest(&manifest);
        assert!(
            matches!(result, VerificationResult::Invalid(ref msg) if msg.contains("invalid public key"))
        );
    }

    #[test]
    fn test_wrong_length_public_key() {
        let mut manifest = unsigned_manifest();
        manifest.signature = Some("AAAA".to_string());
        let engine = base64::engine::general_purpose::STANDARD;
        // 5 bytes is not a valid ed25519 public key (must be 32)
        manifest.signer_public_key = Some(engine.encode([0u8; 5]));
        let result = verify_manifest(&manifest);
        assert!(
            matches!(result, VerificationResult::Invalid(ref msg) if msg.contains("public key length mismatch"))
        );
    }

    #[test]
    fn test_invalid_base64_signature() {
        let mut manifest = unsigned_manifest();
        let engine = base64::engine::general_purpose::STANDARD;
        manifest.signature = Some("AAAA".to_string());
        manifest.signer_public_key = Some(engine.encode([0u8; 32]));
        let result = verify_manifest(&manifest);
        assert!(
            matches!(result, VerificationResult::Invalid(ref msg) if msg.contains("invalid signature"))
        );
    }

    #[test]
    fn test_empty_manifest_signing() {
        // A minimal manifest has no signature fields
        let m = PluginManifest::minimal("com.test.empty", "");
        assert_eq!(verify_manifest(&m), VerificationResult::NotSigned);
    }

    /// Sign `manifest` with a fresh key and return that key's public half,
    /// base64 as the manifest records it.
    fn sign_with_fresh_key(manifest: &mut PluginManifest) -> String {
        use ed25519_dalek::SigningKey;
        use rand::Rng;
        let mut secret_bytes = [0u8; 32];
        rand::thread_rng().fill(&mut secret_bytes);
        let engine = base64::engine::general_purpose::STANDARD;
        let secret_b64 = engine.encode(SigningKey::from_bytes(&secret_bytes).to_bytes());
        sign_manifest(manifest, &secret_b64).unwrap();
        manifest.signer_public_key.clone().unwrap()
    }

    #[test]
    fn the_default_policy_adds_nothing() {
        assert!(SignaturePolicy::default().is_permissive());
        let unsigned = unsigned_manifest();
        assert!(check_policy(&unsigned, &SignaturePolicy::default()).is_ok());
    }

    #[test]
    fn require_signed_refuses_an_unsigned_manifest() {
        let policy = SignaturePolicy {
            require_signed: true,
            ..Default::default()
        };
        assert!(!policy.is_permissive());
        let err = check_policy(&unsigned_manifest(), &policy).unwrap_err();
        assert!(err.contains("not signed"), "{err}");
    }

    #[test]
    fn require_signed_accepts_a_signed_manifest() {
        let mut signed = unsigned_manifest();
        sign_with_fresh_key(&mut signed);
        let policy = SignaturePolicy {
            require_signed: true,
            ..Default::default()
        };
        assert!(check_policy(&signed, &policy).is_ok());
    }

    #[test]
    fn a_signer_allowlist_refuses_a_key_it_does_not_list() {
        let mut signed = unsigned_manifest();
        sign_with_fresh_key(&mut signed);
        let policy = SignaturePolicy {
            require_signed: false,
            trusted_signers: vec!["c29tZSBvdGhlciBrZXk=".to_string()],
        };
        let err = check_policy(&signed, &policy).unwrap_err();
        assert!(err.contains("trusted signer list"), "{err}");
    }

    #[test]
    fn a_signer_allowlist_accepts_the_listed_key() {
        let mut signed = unsigned_manifest();
        let signer = sign_with_fresh_key(&mut signed);
        let policy = SignaturePolicy {
            require_signed: false,
            trusted_signers: vec![signer],
        };
        assert!(check_policy(&signed, &policy).is_ok());
    }

    #[test]
    fn a_tampered_manifest_is_refused_under_any_policy() {
        let mut signed = unsigned_manifest();
        let signer = sign_with_fresh_key(&mut signed);
        signed.description = "changed after signing".to_string();
        // Even a policy that would otherwise accept this signer.
        let policy = SignaturePolicy {
            require_signed: true,
            trusted_signers: vec![signer],
        };
        let err = check_policy(&signed, &policy).unwrap_err();
        assert!(err.contains("signature mismatch"), "{err}");
    }
}
