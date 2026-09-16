//! Multi-tier Rate Limiting for Syscity Gateway
//!
//! Provides sophisticated rate limiting with multiple tiers:
//! - Global: overall API rate limit
//! - Per-user: authenticated user limits
//! - Per-IP: IP-based limits for anonymous requests
//! - Per-endpoint: specific endpoint restrictions

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::gateway::GatewayState;
use crate::security::sliding_window::{LockoutConfig, RateLimitKey, SlidingWindowRateLimiter};
use crate::security::{RateLimitResult, RateLimiter, UserId};

/// Multi-tier rate limit configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiTierRateLimitConfig {
    /// Global rate limit (all requests)
    pub global: TierConfig,
    /// Per-authenticated-user rate limit
    pub per_user: TierConfig,
    /// Per-IP rate limit (for anonymous requests)
    pub per_ip: TierConfig,
    /// Per-endpoint rate limit
    pub per_endpoint: TierConfig,
    /// Shared-secret authentication scope.
    pub shared_secret: TierConfig,
    /// Device-token authentication scope.
    pub device_token: TierConfig,
    /// Webhook/hook authentication scope.
    pub hook_auth: TierConfig,
    /// Control-plane write operations (POST/PUT/DELETE/PATCH).
    pub control_plane_write: TierConfig,
    /// Lockout configuration for repeated failures.
    #[serde(default)]
    pub lockout: LockoutConfig,
    /// Skip rate limiting for loopback addresses.
    #[serde(default)]
    pub loopback_exempt: bool,
}

impl Default for MultiTierRateLimitConfig {
    fn default() -> Self {
        Self {
            global: TierConfig {
                enabled: true,
                capacity: 1000,
                window_secs: 60,
            },
            per_user: TierConfig {
                enabled: true,
                capacity: 100,
                window_secs: 60,
            },
            per_ip: TierConfig {
                enabled: true,
                capacity: 30,
                window_secs: 60,
            },
            per_endpoint: TierConfig {
                enabled: false,
                capacity: 50,
                window_secs: 60,
            },
            shared_secret: TierConfig {
                enabled: true,
                capacity: 200,
                window_secs: 60,
            },
            device_token: TierConfig {
                enabled: true,
                capacity: 60,
                window_secs: 60,
            },
            hook_auth: TierConfig {
                enabled: true,
                capacity: 300,
                window_secs: 60,
            },
            control_plane_write: TierConfig {
                enabled: true,
                capacity: 20,
                window_secs: 60,
            },
            lockout: LockoutConfig::default(),
            loopback_exempt: true,
        }
    }
}

/// Auth scope detected for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScope {
    /// Authenticated via shared secret.
    SharedSecret,
    /// Authenticated via device token.
    DeviceToken,
    /// Authenticated via webhook/hook signature.
    HookAuth,
    /// No auth-specific scope matched.
    None,
}

impl AuthScope {
    /// Return the tier name for this scope.
    pub fn tier_name(self) -> &'static str {
        match self {
            AuthScope::SharedSecret => "shared_secret",
            AuthScope::DeviceToken => "device_token",
            AuthScope::HookAuth => "hook_auth",
            AuthScope::None => "none",
        }
    }
}

/// Request classification used for scope detection.
#[derive(Debug, Clone)]
pub struct RequestScope {
    /// Detected auth scope, if any.
    pub auth_scope: AuthScope,
    /// Whether the request is a control-plane write operation.
    pub is_control_plane_write: bool,
}

impl RequestScope {
    /// Create a new scope classification.
    pub fn new(auth_scope: AuthScope, is_control_plane_write: bool) -> Self {
        Self {
            auth_scope,
            is_control_plane_write,
        }
    }

    /// Detect scope from request metadata.
    ///
    /// `shared_secret_token` should be the configured shared secret, if any.
    pub fn detect(
        method: &axum::http::Method,
        path: &str,
        auth_header: Option<&str>,
        shared_secret_token: Option<&str>,
    ) -> Self {
        let auth_scope = if let Some(auth) = auth_header {
            if let Some(token) = auth.strip_prefix("Bearer ") {
                if shared_secret_token.is_some_and(|s| s == token) {
                    AuthScope::SharedSecret
                } else if path.starts_with("/api/device") {
                    AuthScope::DeviceToken
                } else {
                    AuthScope::None
                }
            } else if auth.starts_with("Device ") || path.starts_with("/api/device") {
                AuthScope::DeviceToken
            } else {
                AuthScope::None
            }
        } else if path.starts_with("/webhooks/") {
            AuthScope::HookAuth
        } else {
            AuthScope::None
        };

        let is_write = matches!(
            *method,
            axum::http::Method::POST
                | axum::http::Method::PUT
                | axum::http::Method::DELETE
                | axum::http::Method::PATCH
        );
        let is_control_plane_write = is_write && path.starts_with("/api/");

        Self::new(auth_scope, is_control_plane_write)
    }
}

/// Single tier configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// Enable this tier
    pub enabled: bool,
    /// Maximum requests per window
    pub capacity: u32,
    /// Window size in seconds
    pub window_secs: u64,
}

/// Multi-tier rate limiter combining token bucket and sliding window
#[derive(Debug, Clone)]
pub struct MultiTierRateLimiter {
    /// Global sliding window limiter
    global: Arc<SlidingWindowRateLimiter>,
    /// Per-user sliding window limiter
    per_user: Arc<SlidingWindowRateLimiter>,
    /// Per-IP sliding window limiter
    per_ip: Arc<SlidingWindowRateLimiter>,
    /// Per-endpoint sliding window limiter
    per_endpoint: Arc<SlidingWindowRateLimiter>,
    /// Shared-secret authentication scope limiter
    shared_secret: Arc<SlidingWindowRateLimiter>,
    /// Device-token authentication scope limiter
    device_token: Arc<SlidingWindowRateLimiter>,
    /// Webhook/hook authentication scope limiter
    hook_auth: Arc<SlidingWindowRateLimiter>,
    /// Control-plane write operation limiter
    control_plane_write: Arc<SlidingWindowRateLimiter>,
    /// Legacy token bucket rate limiter (for backward compat)
    token_bucket: Arc<RateLimiter>,
    /// Configuration
    config: MultiTierRateLimitConfig,
}

impl MultiTierRateLimiter {
    /// Create a new multi-tier rate limiter
    pub fn new(config: MultiTierRateLimitConfig) -> Self {
        let lockout_config = config.lockout;
        Self {
            global: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.global.window_secs),
                config.global.capacity,
                lockout_config,
            )),
            per_user: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.per_user.window_secs),
                config.per_user.capacity,
                lockout_config,
            )),
            per_ip: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.per_ip.window_secs),
                config.per_ip.capacity,
                lockout_config,
            )),
            per_endpoint: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.per_endpoint.window_secs),
                config.per_endpoint.capacity,
                lockout_config,
            )),
            shared_secret: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.shared_secret.window_secs),
                config.shared_secret.capacity,
                lockout_config,
            )),
            device_token: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.device_token.window_secs),
                config.device_token.capacity,
                lockout_config,
            )),
            hook_auth: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.hook_auth.window_secs),
                config.hook_auth.capacity,
                lockout_config,
            )),
            control_plane_write: Arc::new(SlidingWindowRateLimiter::with_lockout(
                Duration::from_secs(config.control_plane_write.window_secs),
                config.control_plane_write.capacity,
                lockout_config,
            )),
            token_bucket: Arc::new(RateLimiter::new(100, 10.0)),
            config,
        }
    }

    /// Return the configured lockout settings.
    pub fn lockout_config(&self) -> LockoutConfig {
        self.config.lockout
    }

    /// Check if loopback requests are exempt from rate limiting.
    pub fn loopback_exempt(&self) -> bool {
        self.config.loopback_exempt
    }

    /// Check all tiers for a request using the legacy scope-free API.
    ///
    /// This is a convenience wrapper around [`Self::check_scoped`] with no
    /// auth-specific scope and no control-plane write classification.
    pub async fn check(
        &self,
        user_id: &UserId,
        ip: Option<std::net::IpAddr>,
        endpoint: &str,
    ) -> MultiTierResult {
        self.check_scoped(user_id, ip, endpoint, &RequestScope::new(AuthScope::None, false))
            .await
    }

    /// Check all tiers for a request, including auth-specific scopes.
    pub async fn check_scoped(
        &self,
        user_id: &UserId,
        ip: Option<std::net::IpAddr>,
        endpoint: &str,
        scope: &RequestScope,
    ) -> MultiTierResult {
        // Check global tier
        if self.config.global.enabled {
            let global_key = RateLimitKey::new("global", endpoint);
            let result = self.global.check_and_record(&global_key);
            if !result.is_allowed() {
                return MultiTierResult::Denied {
                    tier: "global",
                    retry_after_secs: result.retry_after().unwrap_or(60),
                };
            }
        }

        // Check auth-specific scopes before per-user so that shared secrets
        // and device tokens get their own, stricter limits.
        match scope.auth_scope {
            AuthScope::SharedSecret if self.config.shared_secret.enabled => {
                let key = RateLimitKey::new(format!("shared_secret:{}", user_id.0), endpoint);
                let result = self.shared_secret.check_and_record(&key);
                if !result.is_allowed() {
                    return MultiTierResult::Denied {
                        tier: "shared_secret",
                        retry_after_secs: result.retry_after().unwrap_or(60),
                    };
                }
            }
            AuthScope::DeviceToken if self.config.device_token.enabled => {
                let key = RateLimitKey::new(format!("device_token:{}", user_id.0), endpoint);
                let result = self.device_token.check_and_record(&key);
                if !result.is_allowed() {
                    return MultiTierResult::Denied {
                        tier: "device_token",
                        retry_after_secs: result.retry_after().unwrap_or(60),
                    };
                }
            }
            AuthScope::HookAuth if self.config.hook_auth.enabled => {
                let key = RateLimitKey::new(format!("hook_auth:{}", user_id.0), endpoint);
                let result = self.hook_auth.check_and_record(&key);
                if !result.is_allowed() {
                    return MultiTierResult::Denied {
                        tier: "hook_auth",
                        retry_after_secs: result.retry_after().unwrap_or(60),
                    };
                }
            }
            _ => {}
        }

        // Check control-plane write tier.
        if self.config.control_plane_write.enabled && scope.is_control_plane_write {
            let key = RateLimitKey::new(format!("control_plane_write:{}", user_id.0), endpoint);
            let result = self.control_plane_write.check_and_record(&key);
            if !result.is_allowed() {
                return MultiTierResult::Denied {
                    tier: "control_plane_write",
                    retry_after_secs: result.retry_after().unwrap_or(60),
                };
            }
        }

        // Check per-user tier (for authenticated users)
        if self.config.per_user.enabled && !user_id.0.starts_with("ip:") && user_id.0 != "anonymous"
        {
            let user_key = RateLimitKey::new(&user_id.0, endpoint);
            let result = self.per_user.check_and_record(&user_key);
            if !result.is_allowed() {
                return MultiTierResult::Denied {
                    tier: "per_user",
                    retry_after_secs: result.retry_after().unwrap_or(60),
                };
            }
        }

        // Check per-IP tier (for anonymous or as fallback)
        if self.config.per_ip.enabled {
            let ip_str = ip
                .map(|i| i.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let ip_key = RateLimitKey::new(format!("ip:{}", ip_str), endpoint);
            let result = self.per_ip.check_and_record(&ip_key);
            if !result.is_allowed() {
                return MultiTierResult::Denied {
                    tier: "per_ip",
                    retry_after_secs: result.retry_after().unwrap_or(60),
                };
            }
        }

        // Check per-endpoint tier
        if self.config.per_endpoint.enabled {
            let endpoint_key = RateLimitKey::new("endpoint", endpoint);
            let result = self.per_endpoint.check_and_record(&endpoint_key);
            if !result.is_allowed() {
                return MultiTierResult::Denied {
                    tier: "per_endpoint",
                    retry_after_secs: result.retry_after().unwrap_or(60),
                };
            }
        }

        // Legacy token bucket check for backward compatibility
        let legacy = self.token_bucket.check(user_id).await;
        if !legacy.is_allowed() {
            return MultiTierResult::Denied {
                tier: "legacy",
                retry_after_secs: match legacy {
                    RateLimitResult::Denied { retry_after_secs } => retry_after_secs,
                    _ => 60,
                },
            };
        }

        let remaining = match legacy {
            RateLimitResult::Allowed { remaining, .. } => remaining,
            _ => 0,
        };

        MultiTierResult::Allowed { remaining }
    }

    /// Get current state summary
    pub fn stats(&self) -> RateLimitStats {
        RateLimitStats {
            global_windows: self.global.window_count(),
            user_windows: self.per_user.window_count(),
            ip_windows: self.per_ip.window_count(),
            endpoint_windows: self.per_endpoint.window_count(),
            shared_secret_windows: self.shared_secret.window_count(),
            device_token_windows: self.device_token.window_count(),
            hook_auth_windows: self.hook_auth.window_count(),
            control_plane_write_windows: self.control_plane_write.window_count(),
            lockout_states: self.global.lockout_count()
                + self.per_user.lockout_count()
                + self.per_ip.lockout_count()
                + self.per_endpoint.lockout_count()
                + self.shared_secret.lockout_count()
                + self.device_token.lockout_count()
                + self.hook_auth.lockout_count()
                + self.control_plane_write.lockout_count(),
        }
    }

    /// Cleanup old windows (call periodically)
    pub fn cleanup(&self) {
        self.global.cleanup();
        self.per_user.cleanup();
        self.per_ip.cleanup();
        self.per_endpoint.cleanup();
        self.shared_secret.cleanup();
        self.device_token.cleanup();
        self.hook_auth.cleanup();
        self.control_plane_write.cleanup();
    }

    /// Serialize recent attempts across all tiers.
    pub fn serialize_attempts(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut combined = Vec::new();
        for tier in [
            &self.global,
            &self.per_user,
            &self.per_ip,
            &self.per_endpoint,
            &self.shared_secret,
            &self.device_token,
            &self.hook_auth,
            &self.control_plane_write,
        ] {
            combined.extend(tier.attempt_logs());
        }
        serde_json::to_vec(&combined)
    }

    /// Load attempts into all tiers.
    ///
    /// Because the serialized data does not encode the tier, it is loaded into
    /// every tier. The keys are scoped by user/endpoint so duplicate loads are
    /// harmless.
    pub fn load_attempts(&self, data: &[u8]) -> Result<usize, serde_json::Error> {
        let mut total = 0;
        for tier in [
            &self.global,
            &self.per_user,
            &self.per_ip,
            &self.per_endpoint,
            &self.shared_secret,
            &self.device_token,
            &self.hook_auth,
            &self.control_plane_write,
        ] {
            total += tier.load_attempts(data)?;
        }
        Ok(total)
    }
}

impl Default for MultiTierRateLimiter {
    fn default() -> Self {
        Self::new(MultiTierRateLimitConfig::default())
    }
}

/// Result of multi-tier rate limit check
#[derive(Debug, Clone)]
pub enum MultiTierResult {
    /// Request allowed
    Allowed { remaining: u32 },
    /// Request denied by a specific tier
    Denied {
        tier: &'static str,
        retry_after_secs: u64,
    },
}

impl MultiTierResult {
    /// Check if request is allowed
    pub fn is_allowed(&self) -> bool {
        matches!(self, MultiTierResult::Allowed { .. })
    }

    /// Get retry after seconds
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            MultiTierResult::Denied { retry_after_secs, .. } => Some(*retry_after_secs),
            _ => None,
        }
    }
}

/// Rate limit statistics
#[derive(Debug, Clone, Serialize)]
pub struct RateLimitStats {
    pub global_windows: usize,
    pub user_windows: usize,
    pub ip_windows: usize,
    pub endpoint_windows: usize,
    pub shared_secret_windows: usize,
    pub device_token_windows: usize,
    pub hook_auth_windows: usize,
    pub control_plane_write_windows: usize,
    pub lockout_states: usize,
}

/// Extract client IP from request (re-export from middleware for convenience)
fn extract_client_ip(req: &Request) -> Option<std::net::IpAddr> {
    // Check X-Forwarded-For header
    if let Some(forwarded) = req.headers().get("x-forwarded-for") {
        if let Ok(forwarded_str) = forwarded.to_str() {
            if let Some(first_ip) = forwarded_str.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse() {
                    return Some(ip);
                }
            }
        }
    }

    // Check X-Real-IP header
    if let Some(real_ip) = req.headers().get("x-real-ip") {
        if let Ok(real_ip_str) = real_ip.to_str() {
            if let Ok(ip) = real_ip_str.parse() {
                return Some(ip);
            }
        }
    }

    None
}

/// Check whether an IP address is loopback.
fn is_loopback_ip(ip: Option<std::net::IpAddr>) -> bool {
    ip.is_some_and(|ip| ip.is_loopback())
}

/// Enhanced rate limiting middleware with multi-tier support
pub async fn multi_tier_rate_limit_middleware(
    State(state): State<Arc<GatewayState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Check if rate limiting is enabled
    let (rate_limit_enabled, shared_secret) = {
        let config = state.config.read().await;
        (config.security.rate_limit.enabled, config.security.shared_token.clone())
    };

    if !rate_limit_enabled {
        return Ok(next.run(req).await);
    }

    let ip = extract_client_ip(&req);

    // Loopback exemption: skip rate limiting for local development.
    if state.auth.multi_tier_rate_limiter.loopback_exempt() && is_loopback_ip(ip) {
        return Ok(next.run(req).await);
    }

    // Detect request scope (auth method + control-plane writes).
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let scope =
        RequestScope::detect(req.method(), req.uri().path(), auth_header, shared_secret.as_deref());

    // Get user identifier for rate limiting. The identity is the Bearer token's
    // session when present; otherwise the client IP, or a static anonymous id.
    // There is deliberately no session-cookie branch here: no code issues
    // cookies (`gateway::auth::SessionCookieConfig` is a forward-compat shim
    // only), so such a branch would be dead and would pretend cookies are a
    // real credential.
    let user_id = {
        let auth_header = req.headers().get("authorization");
        if let Some(header_value) = auth_header {
            if let Ok(header_str) = header_value.to_str() {
                if let Some(token) = header_str.strip_prefix("Bearer ") {
                    if let Some(session) = state.auth.manager.validate_session(token).await {
                        session.user_id
                    } else {
                        extract_client_ip(&req)
                            .map(|ip| UserId::new(format!("ip:{}", ip)))
                            .unwrap_or_else(|| UserId::new("anonymous"))
                    }
                } else {
                    extract_client_ip(&req)
                        .map(|ip| UserId::new(format!("ip:{}", ip)))
                        .unwrap_or_else(|| UserId::new("anonymous"))
                }
            } else {
                extract_client_ip(&req)
                    .map(|ip| UserId::new(format!("ip:{}", ip)))
                    .unwrap_or_else(|| UserId::new("anonymous"))
            }
        } else {
            extract_client_ip(&req)
                .map(|ip| UserId::new(format!("ip:{}", ip)))
                .unwrap_or_else(|| UserId::new("anonymous"))
        }
    };

    let endpoint = req.uri().path().to_string();

    // Check multi-tier rate limit using the shared instance from GatewayState
    let result = state
        .auth
        .multi_tier_rate_limiter
        .check_scoped(&user_id, ip, &endpoint, &scope)
        .await;

    match result {
        MultiTierResult::Allowed { remaining } => {
            let mut response = next.run(req).await;
            let headers = response.headers_mut();
            headers.insert("X-RateLimit-Limit", HeaderValue::from_static("100"));
            if let Ok(v) = HeaderValue::try_from(remaining.to_string()) {
                headers.insert("X-RateLimit-Remaining", v);
            }
            Ok(response)
        }
        MultiTierResult::Denied { tier, retry_after_secs } => {
            warn!("Rate limit exceeded for user: {} on tier: {}", user_id, tier);
            let body = format!(
                "Rate limit exceeded on tier '{}'. Retry after {} seconds.",
                tier, retry_after_secs
            );
            let mut response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from(body))
                .unwrap_or_else(|_| Response::new(Body::empty()));
            let headers = response.headers_mut();
            if let Ok(v) = HeaderValue::try_from(retry_after_secs.to_string()) {
                headers.insert("Retry-After", v);
            }
            if let Ok(v) = HeaderValue::try_from(tier) {
                headers.insert("X-RateLimit-Tier", v);
            }
            Ok(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_tier_allowed() {
        let limiter = MultiTierRateLimiter::default();
        let user = UserId::new("user1");
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { limiter.check(&user, None, "/api/test").await });
        assert!(result.is_allowed());
    }

    #[test]
    fn test_multi_tier_per_user_limit() {
        let config = MultiTierRateLimitConfig {
            per_user: TierConfig {
                enabled: true,
                capacity: 2,
                window_secs: 60,
            },
            ..Default::default()
        };
        let limiter = MultiTierRateLimiter::new(config);
        let user = UserId::new("user1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        // First 2 requests allowed
        for _ in 0..2 {
            let result = rt.block_on(limiter.check(&user, None, "/api/test"));
            assert!(result.is_allowed());
        }
        // 3rd request denied
        let result = rt.block_on(limiter.check(&user, None, "/api/test"));
        assert!(!result.is_allowed());
    }

    #[test]
    fn test_multi_tier_stats() {
        let limiter = MultiTierRateLimiter::default();
        let stats = limiter.stats();
        assert_eq!(stats.global_windows, 0);
    }

    #[test]
    fn test_multi_tier_result_is_allowed() {
        let allowed = MultiTierResult::Allowed { remaining: 5 };
        assert!(allowed.is_allowed());
        assert_eq!(allowed.retry_after(), None);

        let denied = MultiTierResult::Denied {
            tier: "global",
            retry_after_secs: 30,
        };
        assert!(!denied.is_allowed());
        assert_eq!(denied.retry_after(), Some(30));
    }

    #[test]
    fn test_config_default() {
        let config = MultiTierRateLimitConfig::default();
        assert!(config.global.enabled);
        assert_eq!(config.global.capacity, 1000);
        assert!(config.per_user.enabled);
        assert!(config.per_ip.enabled);
        assert!(!config.per_endpoint.enabled);
        assert!(config.shared_secret.enabled);
        assert!(config.device_token.enabled);
        assert!(config.hook_auth.enabled);
        assert!(config.control_plane_write.enabled);
        assert!(config.loopback_exempt);
        assert!(config.lockout.enabled);
    }

    #[test]
    fn test_request_scope_detection() {
        use axum::http::Method;

        let shared = Some("secret");
        let scope = RequestScope::detect(&Method::GET, "/api/test", Some("Bearer secret"), shared);
        assert_eq!(scope.auth_scope, AuthScope::SharedSecret);
        assert!(!scope.is_control_plane_write);

        let scope = RequestScope::detect(&Method::POST, "/api/test", Some("Bearer secret"), shared);
        assert_eq!(scope.auth_scope, AuthScope::SharedSecret);
        assert!(scope.is_control_plane_write);

        let scope =
            RequestScope::detect(&Method::GET, "/api/device/x", Some("Bearer token"), shared);
        assert_eq!(scope.auth_scope, AuthScope::DeviceToken);

        let scope = RequestScope::detect(&Method::POST, "/webhooks/gh", None, shared);
        assert_eq!(scope.auth_scope, AuthScope::HookAuth);
        assert!(!scope.is_control_plane_write);

        let scope = RequestScope::detect(&Method::GET, "/api/test", Some("Bearer other"), shared);
        assert_eq!(scope.auth_scope, AuthScope::None);
    }

    #[test]
    fn test_control_plane_write_limit() {
        let config = MultiTierRateLimitConfig {
            control_plane_write: TierConfig {
                enabled: true,
                capacity: 2,
                window_secs: 60,
            },
            ..Default::default()
        };
        let limiter = MultiTierRateLimiter::new(config);
        let user = UserId::new("user1");
        let scope = RequestScope::new(AuthScope::None, true);

        let rt = tokio::runtime::Runtime::new().unwrap();
        for _ in 0..2 {
            let result = rt.block_on(limiter.check_scoped(&user, None, "/api/test", &scope));
            assert!(result.is_allowed());
        }
        let result = rt.block_on(limiter.check_scoped(&user, None, "/api/test", &scope));
        assert!(!result.is_allowed());
        // Retry-after is based on the sliding window reset; allow one second of
        // elapsed time between the first request and the denial.
        let retry_after = result.retry_after().unwrap();
        assert!(retry_after > 0 && retry_after <= 60, "unexpected retry_after: {retry_after}");
    }

    #[test]
    fn test_shared_secret_scope_limit() {
        let config = MultiTierRateLimitConfig {
            shared_secret: TierConfig {
                enabled: true,
                capacity: 2,
                window_secs: 60,
            },
            ..Default::default()
        };
        let limiter = MultiTierRateLimiter::new(config);
        let user = UserId::new("shared_secret_user");
        let scope = RequestScope::new(AuthScope::SharedSecret, false);

        let rt = tokio::runtime::Runtime::new().unwrap();
        for _ in 0..2 {
            let result = rt.block_on(limiter.check_scoped(&user, None, "/api/test", &scope));
            assert!(result.is_allowed());
        }
        let result = rt.block_on(limiter.check_scoped(&user, None, "/api/test", &scope));
        assert!(!result.is_allowed());
    }

    #[test]
    fn test_attempt_serialization() {
        let limiter = MultiTierRateLimiter::new(MultiTierRateLimitConfig {
            per_user: TierConfig {
                enabled: true,
                capacity: 5,
                window_secs: 60,
            },
            ..Default::default()
        });
        let user = UserId::new("user1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = limiter.check(&user, None, "/api/test").await;
        });

        let data = limiter.serialize_attempts().unwrap();
        assert!(!data.is_empty());

        let limiter2 = MultiTierRateLimiter::default();
        let loaded = limiter2.load_attempts(&data).unwrap();
        assert!(loaded > 0);
    }

    #[test]
    fn test_multi_tier_ip_limit() {
        let config = MultiTierRateLimitConfig {
            per_ip: TierConfig {
                enabled: true,
                capacity: 2,
                window_secs: 60,
            },
            ..Default::default()
        };
        let limiter = MultiTierRateLimiter::new(config);
        let user = UserId::new("ip:192.168.1.1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        for _ in 0..2 {
            let result =
                rt.block_on(limiter.check(&user, Some("192.168.1.1".parse().unwrap()), "/api"));
            assert!(result.is_allowed());
        }
        let result =
            rt.block_on(limiter.check(&user, Some("192.168.1.1".parse().unwrap()), "/api"));
        assert!(!result.is_allowed());
    }

    #[test]
    fn test_multi_tier_cleanup() {
        let limiter = MultiTierRateLimiter::default();
        let user = UserId::new("user1");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = limiter.check(&user, None, "/api").await;
        });

        assert!(limiter.stats().global_windows > 0);
        limiter.cleanup();
        // After cleanup, windows may be empty depending on timing
    }
}
