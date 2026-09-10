//! Device Pairing for WebSocket-native protocol
//!
//! Flow:
//! 1. Client connects via WS and sends `connect` with `device` identity
//! 2. If device not paired: server generates a short pairing code
//! 3. Server broadcasts `device.pair.requested` event to admin clients
//! 4. Admin runs `syscity device approve <code>`
//! 5. Server issues a device token to the client
//! 6. Client reconnects using the device token in `auth.token`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::auth_store::{self, AuthStore, StoredDevice};

/// A pending device pairing request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingDeviceRequest {
    /// Short pairing code (e.g., "A3F7K2X9").
    pub code: String,
    /// Device unique ID.
    pub device_id: String,
    /// Optional display name.
    pub display_name: Option<String>,
    /// Ed25519 public key (base64) if provided.
    pub public_key: Option<String>,
    /// When the request was created.
    pub created_at: SystemTime,
    /// When the request expires.
    pub expires_at: SystemTime,
}

impl PendingDeviceRequest {
    pub fn is_valid(&self) -> bool {
        SystemTime::now() < self.expires_at
    }
}

/// An authorized device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedDevice {
    pub device_id: String,
    pub display_name: Option<String>,
    pub public_key: Option<String>,
    /// Device token used for re-authentication.
    ///
    /// Populated with the plaintext token only for a device approved in this
    /// process; devices restored from persistence leave this empty (the
    /// plaintext is unrecoverable by design) and authenticate via
    /// [`DevicePairingStore::validate_token`] against `token_hash`.
    pub token: String,
    /// SHA-256 digest of `token` — the only form written to disk and the
    /// value matched during validation. Never serialized over the wire.
    #[serde(skip)]
    pub token_hash: String,
    pub authorized_at: SystemTime,
    pub approved_by: Option<String>,
}

/// Result of a device access check.
#[derive(Debug, Clone)]
pub enum DeviceAccessResult {
    /// Device is authorized; here is the token.
    Authorized { token: String },
    /// New pairing request created.
    PairingRequired { code: String },
    /// Already has a pending request.
    AlreadyPending { code: String },
    /// Rate limited.
    RateLimited,
}

/// Thread-safe store for device pairing.
#[derive(Debug, Clone)]
pub struct DevicePairingStore {
    pending: Arc<RwLock<HashMap<String, PendingDeviceRequest>>>,
    /// device_id -> code reverse index.
    pending_index: Arc<RwLock<HashMap<String, String>>>,
    authorized: Arc<RwLock<HashMap<String, AuthorizedDevice>>>,
    default_ttl: Duration,
    /// Max total pending requests allowed (across all devices).
    max_pending: usize,
    /// Optional SQLite persistence for authorized devices. `None` keeps the
    /// store purely in-memory (unchanged behavior for tests / non-sqlite
    /// deployments).
    store: Option<Arc<AuthStore>>,
}

impl Default for DevicePairingStore {
    fn default() -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            pending_index: Arc::new(RwLock::new(HashMap::new())),
            authorized: Arc::new(RwLock::new(HashMap::new())),
            default_ttl: Duration::from_secs(3600),
            max_pending: 100,
            store: None,
        }
    }
}

impl DevicePairingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom configuration.
    pub fn with_config(default_ttl: Duration, max_pending: usize) -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            pending_index: Arc::new(RwLock::new(HashMap::new())),
            authorized: Arc::new(RwLock::new(HashMap::new())),
            default_ttl,
            max_pending,
            store: None,
        }
    }

    /// Attach a persistent store and restore previously authorized devices.
    ///
    /// Only hashed tokens are read back; restored devices expose an empty
    /// `token` field but still authenticate through [`Self::validate_token`]
    /// against their stored digest. Revoked devices leave no row behind, so
    /// they are rejected after a restart.
    pub async fn with_persistence(mut self, store: Arc<AuthStore>) -> crate::Result<Self> {
        store.init_schema().await?;
        let loaded = store.load_devices().await?;
        let restored = loaded.len();
        {
            let mut auth = self.authorized.write().await;
            for device in loaded {
                auth.insert(device.device_id.clone(), device.into_authorized());
            }
        }
        self.store = Some(store);
        info!("Restored {} authorized device(s) from persistence", restored);
        Ok(self)
    }

    /// Request device pairing access.
    pub async fn request_access(
        &self,
        device_id: &str,
        display_name: Option<&str>,
        public_key: Option<&str>,
    ) -> DeviceAccessResult {
        // Check if already authorized
        {
            let auth = self.authorized.read().await;
            if let Some(dev) = auth.get(device_id) {
                return DeviceAccessResult::Authorized { token: dev.token.clone() };
            }
        }

        // Check existing pending
        {
            let index = self.pending_index.read().await;
            if let Some(code) = index.get(device_id) {
                let pending = self.pending.read().await;
                if let Some(req) = pending.get(code) {
                    if req.is_valid() {
                        return DeviceAccessResult::AlreadyPending { code: code.clone() };
                    }
                }
            }
        }

        // Enforce max pending limit
        {
            let pending = self.pending.read().await;
            if pending.len() >= self.max_pending {
                warn!(
                    "Device pairing max pending limit reached ({}), rejecting request for {}",
                    self.max_pending, device_id
                );
                return DeviceAccessResult::RateLimited;
            }
        }

        // Generate short code
        let code = Self::generate_code();
        let now = SystemTime::now();
        let req = PendingDeviceRequest {
            code: code.clone(),
            device_id: device_id.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            public_key: public_key.map(|s| s.to_string()),
            created_at: now,
            expires_at: now + self.default_ttl,
        };

        {
            let mut pending = self.pending.write().await;
            pending.insert(code.clone(), req);
        }
        {
            let mut index = self.pending_index.write().await;
            index.insert(device_id.to_string(), code.clone());
        }

        info!("Device pairing request created: device_id={} code={}", device_id, code);

        DeviceAccessResult::PairingRequired { code }
    }

    /// Approve a pending device by code. Returns the device token on success.
    pub async fn approve(&self, code: &str, approved_by: Option<&str>) -> Option<String> {
        let req = {
            let mut pending = self.pending.write().await;
            pending.remove(code)?
        };

        if !req.is_valid() {
            warn!("Attempted to approve expired device pairing code: {}", code);
            // Clean up index
            let mut index = self.pending_index.write().await;
            index.remove(&req.device_id);
            return None;
        }

        let token = format!("dt_{}", uuid::Uuid::new_v4());
        let dev = AuthorizedDevice {
            device_id: req.device_id.clone(),
            display_name: req.display_name.clone(),
            public_key: req.public_key.clone(),
            token: token.clone(),
            token_hash: auth_store::hash_device_token(&token),
            authorized_at: SystemTime::now(),
            approved_by: approved_by.map(|s| s.to_string()),
        };

        {
            let mut auth = self.authorized.write().await;
            auth.insert(req.device_id.clone(), dev.clone());
        }
        {
            let mut index = self.pending_index.write().await;
            index.remove(&req.device_id);
        }

        // Write through to the store; only the token digest is persisted.
        if let Some(ref store) = self.store {
            if let Err(e) = store
                .upsert_device(&StoredDevice::from_authorized(&dev))
                .await
            {
                warn!("Failed to persist authorized device {}: {}", dev.device_id, e);
            }
        }

        info!(
            "Device approved: device_id={} code={} by={:?}",
            req.device_id, code, approved_by
        );

        Some(token)
    }

    /// Reject/deny a pending request by code.
    pub async fn reject(&self, code: &str) -> Option<PendingDeviceRequest> {
        let req = {
            let mut pending = self.pending.write().await;
            pending.remove(code)?
        };
        {
            let mut index = self.pending_index.write().await;
            index.remove(&req.device_id);
        }
        info!("Device pairing rejected: code={} device_id={}", code, req.device_id);
        Some(req)
    }

    /// Revoke an authorized device.
    ///
    /// Removes the in-memory entry and deletes the persisted row, so a
    /// revoked device stays revoked across a restart.
    pub async fn revoke(&self, device_id: &str) -> bool {
        let removed = {
            let mut auth = self.authorized.write().await;
            auth.remove(device_id).is_some()
        };
        if removed {
            if let Some(ref store) = self.store {
                if let Err(e) = store.delete_device(device_id).await {
                    warn!("Failed to delete revoked device {} from store: {}", device_id, e);
                }
            }
        }
        removed
    }

    /// Check if a device token is valid.
    ///
    /// The presented plaintext token is hashed and compared (in constant
    /// time) against each device's stored digest, so validation works for
    /// devices restored from persistence where the plaintext is unavailable.
    pub async fn validate_token(&self, token: &str) -> Option<String> {
        let presented = auth_store::hash_device_token(token);
        let auth = self.authorized.read().await;
        for (device_id, dev) in auth.iter() {
            if auth_store::token_hash_eq(&dev.token_hash, &presented) {
                return Some(device_id.clone());
            }
        }
        None
    }

    /// Get device info by ID.
    pub async fn get_device(&self, device_id: &str) -> Option<AuthorizedDevice> {
        let auth = self.authorized.read().await;
        auth.get(device_id).cloned()
    }

    /// List all authorized devices.
    pub async fn list_authorized(&self) -> Vec<AuthorizedDevice> {
        let auth = self.authorized.read().await;
        auth.values().cloned().collect()
    }

    /// List pending requests.
    pub async fn list_pending(&self) -> Vec<PendingDeviceRequest> {
        let pending = self.pending.read().await;
        pending.values().cloned().collect()
    }

    fn generate_code() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
            .collect()
    }

    /// Generate a pairing URI suitable for QR encoding.
    /// Format: syscity://pair/{code}
    pub fn pairing_uri(code: &str) -> String {
        format!("syscity://pair/{}", code)
    }

    /// Generate an SVG string of a QR code encoding the given text.
    pub fn generate_qr_svg(data: &str) -> Result<String, String> {
        use qrcode::QrCode;
        let code = QrCode::new(data.as_bytes()).map_err(|e| e.to_string())?;
        let svg = code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(3, 3)
            .build();
        Ok(svg)
    }

    /// Encode a pairing code into a base64url-safe setup token.
    ///
    /// The token can be embedded in URLs without further escaping.
    pub fn encode_setup_code(code: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(code.as_bytes())
    }

    /// Decode a base64url setup token back to the original pairing code.
    pub fn decode_setup_code(encoded: &str) -> Option<String> {
        if encoded.is_empty() {
            return None;
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()?;
        String::from_utf8(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_device_pairing_flow() {
        let store = DevicePairingStore::new();

        // Request access
        let result = store.request_access("dev_1", Some("My Laptop"), None).await;
        let code = match result {
            DeviceAccessResult::PairingRequired { code } => code,
            _ => panic!("Expected PairingRequired"),
        };
        assert_eq!(code.len(), 8);

        // Same device returns same pending code
        let result2 = store.request_access("dev_1", None, None).await;
        assert!(
            matches!(result2, DeviceAccessResult::AlreadyPending { code: ref c } if c == &code)
        );

        // Approve
        let token = store.approve(&code, Some("admin")).await;
        assert!(token.is_some());
        let token = token.unwrap();
        assert!(token.starts_with("dt_"));

        // Now authorized
        let result3 = store.request_access("dev_1", None, None).await;
        assert!(matches!(result3, DeviceAccessResult::Authorized { token: ref t } if t == &token));

        // Validate token
        let validated = store.validate_token(&token).await;
        assert_eq!(validated, Some("dev_1".to_string()));

        // Revoke
        assert!(store.revoke("dev_1").await);
        assert!(matches!(
            store.request_access("dev_1", None, None).await,
            DeviceAccessResult::PairingRequired { .. }
        ));
    }

    #[tokio::test]
    async fn test_max_pending_limit() {
        let store = DevicePairingStore::with_config(Duration::from_secs(3600), 3);

        // Should work: 3 requests
        for i in 0..3 {
            let result = store
                .request_access(&format!("dev_{}", i), None, None)
                .await;
            assert!(matches!(result, DeviceAccessResult::PairingRequired { .. }));
        }

        // 4th should be rate limited
        let result = store.request_access("dev_overflow", None, None).await;
        assert!(matches!(result, DeviceAccessResult::RateLimited));
    }

    #[test]
    fn test_qr_svg_generation() {
        let svg = DevicePairingStore::generate_qr_svg("syscity://pair/ABC12345").unwrap();
        assert!(svg.contains("<svg"), "SVG should contain svg tag");
        assert!(svg.contains("</svg>"));
        assert!(svg.contains("xmlns"), "SVG should contain xmlns");
    }

    #[test]
    fn test_pairing_uri_format() {
        let uri = DevicePairingStore::pairing_uri("ABC12345");
        assert_eq!(uri, "syscity://pair/ABC12345");
    }

    #[test]
    fn test_code_unambiguous_chars() {
        let code = DevicePairingStore::generate_code();
        assert_eq!(code.len(), 8);
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
        assert!(!code.contains('l'));
    }

    #[test]
    fn test_setup_code_roundtrip() {
        let code = "A3F7K2X9";
        let encoded = DevicePairingStore::encode_setup_code(code);
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('='));

        let decoded = DevicePairingStore::decode_setup_code(&encoded).unwrap();
        assert_eq!(decoded, code);
    }

    #[test]
    fn test_decode_setup_code_rejects_invalid_input() {
        assert!(DevicePairingStore::decode_setup_code("!!!").is_none());
        assert!(DevicePairingStore::decode_setup_code("").is_none());
    }
}
