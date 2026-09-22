//! Signature and request-freshness verification shared by the platform handlers.

use super::*;

/// How old a signed webhook request may be.
///
/// The signature covers the timestamp, so a captured request stays valid
/// forever unless its age is checked — this is the only replay defense on
/// routes that are unauthenticated by design. Five minutes is what Slack
/// documents (their own retries land within seconds, so this tolerates clock
/// skew without leaving a useful window).
pub(super) const SIGNATURE_MAX_AGE_SECS: u64 = 300;

/// Whether a signed-request timestamp is recent enough to act on.
///
/// Rejects a timestamp that is unparseable, older than
/// [`SIGNATURE_MAX_AGE_SECS`], or far in the future (which a skewed or
/// hand-crafted header can claim).
pub(super) fn timestamp_is_fresh(timestamp: &str) -> bool {
    let Ok(secs) = timestamp.trim().parse::<i64>() else {
        return false;
    };
    let now = chrono::Utc::now().timestamp();
    let age = now.saturating_sub(secs);
    // The future bound is looser than the past one: a few seconds of skew in
    // the other direction is normal.
    age <= SIGNATURE_MAX_AGE_SECS as i64 && age >= -(SIGNATURE_MAX_AGE_SECS as i64)
}

/// Verify HMAC-SHA256 signature
///
/// Used by WhatsApp and generic webhooks
pub(super) fn verify_hmac_sha256(secret: &str, body: &[u8], expected_sig: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            warn!("Failed to create HMAC from secret");
            return false;
        }
    };

    mac.update(body);
    let result = mac.finalize();
    let computed_sig = hex::encode(result.into_bytes());

    // Constant-time comparison to prevent timing attacks
    use subtle::ConstantTimeEq;
    computed_sig
        .as_bytes()
        .ct_eq(expected_sig.as_bytes())
        .into()
}

/// Verify Feishu/Lark signature
///
/// Feishu uses a custom signature algorithm:
/// SHA256(timestamp + nonce + secret + body)
pub(super) fn verify_feishu_signature(
    secret: &str,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
    expected_sig: &str,
) -> bool {
    if !timestamp_is_fresh(timestamp) {
        return false;
    }
    use sha2::{Digest, Sha256};

    // Feishu signature: SHA256(timestamp + nonce + secret + body)
    let body_str = String::from_utf8_lossy(body);
    let sign_string = format!("{}{}{}{}", timestamp, nonce, secret, body_str);

    let mut hasher = Sha256::new();
    hasher.update(sign_string.as_bytes());
    let computed_sig = hex::encode(hasher.finalize());

    // Constant-time comparison to prevent timing attacks
    use subtle::ConstantTimeEq;
    computed_sig
        .as_bytes()
        .ct_eq(expected_sig.as_bytes())
        .into()
}
