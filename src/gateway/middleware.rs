//! Gateway Middleware - Access Control
//!
//! Provides middleware for restricting access to admin APIs:
//! - Localhost-only (127.0.0.1, ::1)
//! - Tailscale whois identity verification
//! - Trusted proxy IP header resolution
//! - API key authentication (optional)
//! - Rate limiting

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use tracing::{debug, warn};

use crate::gateway::rate_limit::RequestScope;
use crate::gateway::GatewayState;
use crate::security::UserId;

/// Query parameters whose *values* must never reach a log line.
///
/// The WS upgrade accepts `?token=` (and `?ticket=`, once a ticket flow
/// exists), so a logged request line is a logged credential — and these
/// middlewares log on every access decision, including the refusals, which is
/// exactly when someone reads the log. A credential in a log outlives the
/// request it belonged to.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "token",
    "ticket",
    "access_token",
    "api_key",
    "apikey",
    "secret",
    "password",
];

/// A request URI with sensitive query values masked, safe to log.
///
/// The path is kept whole: it is what the middlewares are deciding about. Query
/// parameters are kept in place so a log still says *that* a token was supplied
/// — the presence is useful, the value is not.
pub fn redact_uri(uri: &axum::http::Uri) -> String {
    let Some(query) = uri.query() else {
        return uri.path().to_string();
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if is_sensitive_param(name) => format!("{name}=***"),
            // A bare `token` (no `=`) carries no value to leak.
            _ => pair.to_string(),
        })
        .collect();
    format!("{}?{}", uri.path(), masked.join("&"))
}

fn is_sensitive_param(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_QUERY_PARAMS.contains(&name.as_str())
}

/// Allowed network origins for admin APIs
#[derive(Debug, Clone, Default)]
pub enum AllowedOrigin {
    /// Only localhost
    #[default]
    Localhost,
    /// Tailscale network (100.64.0.0/10 CGNAT range)
    Tailscale,
    /// Any private network (RFC 1918)
    Private,
    /// Specific IP addresses
    IpList(Vec<IpAddr>),
    /// Any origin (disable restriction)
    Any,
}

/// Check if an IP address is allowed based on origin policy
#[cfg_attr(not(test), allow(dead_code))]
fn is_ip_allowed(addr: IpAddr, allowed: &AllowedOrigin) -> bool {
    match allowed {
        AllowedOrigin::Any => true,
        AllowedOrigin::Localhost => is_localhost(addr),
        AllowedOrigin::Tailscale => is_tailscale(addr),
        AllowedOrigin::Private => is_private_ip(addr),
        AllowedOrigin::IpList(allowed_ips) => allowed_ips.contains(&addr),
    }
}

/// Check if IP is localhost
fn is_localhost(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// Check if IP is in Tailscale's CGNAT range (100.64.0.0/10)
fn is_tailscale(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            // 100.64.0.0/10 = 100.64.0.0 - 100.127.255.255
            octets[0] == 100 && (octets[1] & 0xC0) == 0x40
        }
        IpAddr::V6(_) => false, // Tailscale uses IPv4 CGNAT
    }
}

/// Check if IP is in a private network (RFC 1918)
fn is_private_ip(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

/// Extract client IP from request, respecting trusted proxies.
///
/// - If `trusted_proxies` contains the direct connection IP, trusts
///   `X-Forwarded-For`.
/// - If the direct connection IP is *not* a trusted proxy, ignores forwarded
///   headers.
/// - Falls back to `ConnectInfo<SocketAddr>` extension (TCP peer address).
/// - Last resort: `X-Real-IP` header (no proxy verification — best-effort).
pub fn extract_client_ip_with_trusted(req: &Request, trusted_proxies: &[IpAddr]) -> Option<IpAddr> {
    // Try to get the direct connection IP from ConnectInfo extension
    let direct_ip: Option<IpAddr> = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    // Only trust X-Forwarded-For if the direct connection is from a known proxy,
    // OR if no ConnectInfo is available (backward compat: trust headers)
    let should_trust_headers = direct_ip
        .as_ref()
        .is_none_or(|ip| trusted_proxies.contains(ip) || is_localhost(*ip));

    if should_trust_headers {
        if let Some(forwarded) = req.headers().get("x-forwarded-for") {
            if let Ok(forwarded_str) = forwarded.to_str() {
                if let Some(first_ip) = forwarded_str.split(',').next() {
                    if let Ok(ip) = first_ip.trim().parse() {
                        debug!("Client IP from X-Forwarded-For: {}", ip);
                        return Some(ip);
                    }
                }
            }
        }

        // X-Real-IP fallback
        if let Some(real_ip) = req.headers().get("x-real-ip") {
            if let Ok(real_ip_str) = real_ip.to_str() {
                if let Ok(ip) = real_ip_str.parse() {
                    debug!("Client IP from X-Real-IP: {}", ip);
                    return Some(ip);
                }
            }
        }
    }

    // Fall back to direct connection IP
    if let Some(ip) = direct_ip {
        debug!("Client IP from ConnectInfo: {}", ip);
        return Some(ip);
    }

    None
}

/// Legacy client IP extraction (no proxy verification).
/// Used where trusted_proxies not available (e.g. rate limiter).
fn extract_client_ip(req: &Request) -> Option<IpAddr> {
    extract_client_ip_with_trusted(req, &[])
}

/// Middleware: Restrict to localhost only
pub async fn localhost_only_middleware(req: Request, next: Next) -> Result<Response, StatusCode> {
    // Extract client IP
    let client_ip = extract_client_ip(&req);

    match client_ip {
        Some(ip) if is_localhost(ip) => {
            debug!("Localhost access granted for: {}", redact_uri(req.uri()));
            Ok(next.run(req).await)
        }
        Some(ip) => {
            warn!(
                "Non-localhost access attempt to admin API from: {} - {}",
                ip,
                redact_uri(req.uri())
            );
            Err(StatusCode::FORBIDDEN)
        }
        None => {
            // If we can't determine the IP, check if it's from a Unix socket
            // or allow based on connection type
            debug!("Cannot determine client IP, allowing (may be Unix socket)");
            Ok(next.run(req).await)
        }
    }
}

/// Middleware: Tailscale whois identity verification.
///
/// - Localhost connections are always allowed.
/// - Tailscale IPs (100.64.0.0/10) are verified via
///   `TailscaleAuthenticator::is_authorized()`.
/// - Configured `allowed_tailnets` restricts which tailnets are permitted.
/// - Uses `trusted_proxies` from config for X-Forwarded-For header resolution.
pub async fn tailscale_auth_middleware(
    State(state): State<Arc<GatewayState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // If trusted proxy auth already succeeded, the request is authenticated.
    if req
        .extensions()
        .get::<crate::security::trusted_proxy::TrustedProxyUser>()
        .is_some()
    {
        return Ok(next.run(req).await);
    }

    let (trusted_proxies, allowed_tailnets) = {
        let config = state.config.read().await;
        (
            config.security.trusted_proxies.clone(),
            config.security.allowed_tailnets.clone(),
        )
    };

    let client_ip = extract_client_ip_with_trusted(&req, &trusted_proxies);

    match client_ip {
        Some(ip) if is_localhost(ip) => {
            debug!("Localhost access granted for: {}", redact_uri(req.uri()));
            Ok(next.run(req).await)
        }
        Some(ip) if is_tailscale(ip) => {
            // Tailscale IP: verify via whois
            if let Some(ref auth) = state.auth.tailscale_authenticator {
                if auth.is_authorized(&ip.to_string(), &allowed_tailnets).await {
                    debug!("Tailscale whois verified: {} - {}", ip, redact_uri(req.uri()));
                    Ok(next.run(req).await)
                } else {
                    warn!("Tailscale whois rejected: {} - {}", ip, redact_uri(req.uri()));
                    Err(StatusCode::FORBIDDEN)
                }
            } else {
                // No authenticator configured — fall back to IP-range check
                debug!("Tailscale authenticator not configured, allowing by IP range: {}", ip);
                Ok(next.run(req).await)
            }
        }
        Some(ip) => {
            warn!(
                "Non-Tailscale, non-localhost access attempt from: {} - {}",
                ip,
                redact_uri(req.uri())
            );
            Err(StatusCode::FORBIDDEN)
        }
        None => {
            debug!("Cannot determine client IP, allowing (may be Unix socket)");
            Ok(next.run(req).await)
        }
    }
}

/// Middleware: Trusted proxy authentication.
///
/// Validates that the direct connection comes from a configured trusted proxy,
/// requires the configured identity headers, extracts the user, and enforces
/// the allowlist. Successful authentications attach a `TrustedProxyUser`
/// extension to the request for downstream handlers.
///
/// If trusted proxy auth is disabled, this middleware is a no-op.
pub async fn trusted_proxy_auth_middleware(
    State(state): State<Arc<GatewayState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    use crate::security::runtime_audit::AuditEventType;
    use crate::security::trusted_proxy::TrustedProxyError;

    let authenticator = match state.auth.trusted_proxy_authenticator.as_ref() {
        Some(auth) => auth.clone(),
        None => return Ok(next.run(req).await),
    };

    let direct_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    match authenticator.authenticate(&req, direct_ip) {
        Ok(user) => {
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    &user.user_id,
                    &user.proxy_ip.to_string(),
                    true,
                    "Trusted proxy authentication accepted",
                    Some(serde_json::json!({
                        "header": user.header_name,
                        "proxy_ip": user.proxy_ip.to_string(),
                        "path": req.uri().path(),
                    })),
                )
                .await;
            req.extensions_mut().insert(user);
            Ok(next.run(req).await)
        }
        Err(TrustedProxyError::UntrustedProxy { proxy_ip }) => {
            let target = proxy_ip.to_string();
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    "unknown",
                    &target,
                    false,
                    "Trusted proxy authentication rejected: untrusted proxy",
                    Some(serde_json::json!({
                        "proxy_ip": target,
                        "path": req.uri().path(),
                    })),
                )
                .await;
            Err(StatusCode::FORBIDDEN)
        }
        Err(TrustedProxyError::MissingHeader { header }) => {
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    "unknown",
                    &direct_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    false,
                    format!("Trusted proxy authentication rejected: missing header {}", header),
                    Some(serde_json::json!({
                        "header": header,
                        "path": req.uri().path(),
                    })),
                )
                .await;
            Err(StatusCode::BAD_REQUEST)
        }
        Err(TrustedProxyError::NoUserExtracted) => {
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    "unknown",
                    &direct_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    false,
                    "Trusted proxy authentication rejected: no user extracted",
                    Some(serde_json::json!({ "path": req.uri().path() })),
                )
                .await;
            Err(StatusCode::BAD_REQUEST)
        }
        Err(TrustedProxyError::Disabled) => {
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    "unknown",
                    &direct_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    false,
                    "Trusted proxy authentication rejected: authenticator is disabled",
                    Some(serde_json::json!({ "path": req.uri().path() })),
                )
                .await;
            Err(StatusCode::FORBIDDEN)
        }
        Err(TrustedProxyError::UserNotAllowed { user_id }) => {
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TrustedProxyLogin,
                    &user_id,
                    &direct_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    false,
                    "Trusted proxy authentication rejected: user not allowed",
                    Some(serde_json::json!({
                        "user_id": user_id,
                        "path": req.uri().path(),
                    })),
                )
                .await;
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// Middleware: Restrict to Tailscale network (IP-range only, no whois).
/// Kept for backward compatibility with non-whois setups.
pub async fn tailscale_only_middleware(req: Request, next: Next) -> Result<Response, StatusCode> {
    let client_ip = extract_client_ip(&req);

    match client_ip {
        Some(ip) if is_tailscale(ip) || is_localhost(ip) => {
            debug!("Tailscale/localhost access granted for: {}", redact_uri(req.uri()));
            Ok(next.run(req).await)
        }
        Some(ip) => {
            warn!(
                "Non-Tailscale access attempt to admin API from: {} - {}",
                ip,
                redact_uri(req.uri())
            );
            Err(StatusCode::FORBIDDEN)
        }
        None => {
            debug!("Cannot determine client IP, allowing");
            Ok(next.run(req).await)
        }
    }
}

/// Middleware: Restrict to private networks
pub async fn private_only_middleware(req: Request, next: Next) -> Result<Response, StatusCode> {
    let client_ip = extract_client_ip(&req);

    match client_ip {
        Some(ip) if is_private_ip(ip) => {
            debug!("Private network access granted for: {}", redact_uri(req.uri()));
            Ok(next.run(req).await)
        }
        Some(ip) => {
            warn!(
                "Public network access attempt to admin API from: {} - {}",
                ip,
                redact_uri(req.uri())
            );
            Err(StatusCode::FORBIDDEN)
        }
        None => {
            debug!("Cannot determine client IP, allowing");
            Ok(next.run(req).await)
        }
    }
}

/// Whether `presented` matches the configured shared token, compared in
/// constant time. `None` (no shared token configured) never matches.
fn shared_token_matches(shared_token: Option<&str>, presented: &str) -> bool {
    use subtle::ConstantTimeEq;
    match shared_token {
        Some(shared) => presented.as_bytes().ct_eq(shared.as_bytes()).into(),
        None => false,
    }
}

/// What [`auth_middleware`] validated, for handlers that need the caller's
/// entitlement rather than just "authenticated".
///
/// Only the WS-ticket exchange reads it today: a ticket hands the caller's own
/// scopes to a WebSocket connection, so the handler has to know what they are.
/// A route that sees no `RestAuthContext` is one where auth was not required —
/// `security.auth_required = false` — and its caller is an anonymous local
/// client.
#[derive(Debug, Clone)]
pub struct RestAuthContext {
    /// Who the credential belonged to.
    pub user_id: String,
    /// What it was entitled to.
    pub scopes: Vec<String>,
}

/// Middleware: Authentication check
///
/// Validates Bearer token from Authorization header.
/// If security.auth_required is false, allows all requests.
pub async fn auth_middleware(
    State(state): State<Arc<GatewayState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    use crate::security::runtime_audit::AuditEventType;

    let path = req.uri().path().to_string();
    let client_ip = extract_client_ip(&req);
    let actor = client_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check if auth is required
    let auth_required = {
        let config = state.config.read().await;
        config.security.auth_required
    };

    if !auth_required {
        debug!("Auth not required, allowing request");
        return Ok(next.run(req).await);
    }

    // Extract Authorization header
    let auth_header = req.headers().get("authorization");

    match auth_header {
        Some(header_value) => {
            if let Ok(header_str) = header_value.to_str() {
                if let Some(token) = header_str.strip_prefix("Bearer ") {
                    // Validate session; AuthManager emits the TokenValidation event.
                    if let Some(session) = state.auth.manager.validate_session(token).await {
                        debug!("Valid auth token, allowing request");
                        // Same default the WS handshake applies to a session
                        // with no scopes of its own, so the two paths cannot
                        // disagree about what a session token is worth.
                        let scopes = if session.scopes.is_empty() {
                            crate::gateway::protocol::DEFAULT_SCOPES
                                .iter()
                                .map(|s| s.to_string())
                                .collect()
                        } else {
                            session.scopes.clone()
                        };
                        req.extensions_mut().insert(RestAuthContext {
                            user_id: session.user_id.to_string(),
                            scopes,
                        });
                        return Ok(next.run(req).await);
                    }
                    // The configured shared token (auth_mode=token) is also a
                    // valid Bearer credential — matching the WS handshake's
                    // resolve_token_auth, which already accepts it. This is
                    // the only credential mobile builds have (per-install
                    // token); desktop defaults (auth_required=false) never
                    // reach this path.
                    let shared_token = {
                        let config = state.config.read().await;
                        config.security.shared_token.clone()
                    };
                    if shared_token_matches(shared_token.as_deref(), token) {
                        debug!("Valid shared token, allowing request");
                        let scopes = {
                            let config = state.config.read().await;
                            config.security.shared_token_scopes.clone()
                        };
                        req.extensions_mut().insert(RestAuthContext {
                            user_id: "shared".to_string(),
                            scopes,
                        });
                        return Ok(next.run(req).await);
                    }
                    warn!("Invalid or expired auth token");
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            warn!("Invalid Authorization header format");
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TokenValidation,
                    &actor,
                    &path,
                    false,
                    "Bearer token missing or malformed",
                    Some(serde_json::json!({
                        "reason": "malformed_header",
                    })),
                )
                .await;
            Err(StatusCode::UNAUTHORIZED)
        }
        None => {
            warn!("Missing Authorization header");
            state
                .auth
                .audit_log
                .log(
                    AuditEventType::TokenValidation,
                    &actor,
                    &path,
                    false,
                    "Authorization header missing",
                    Some(serde_json::json!({
                        "reason": "missing_header",
                    })),
                )
                .await;
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Middleware: Rate limiting
///
/// Supports both legacy token bucket and multi-tier sliding window rate
/// limiting. Multi-tier mode checks global, per-user, per-IP, per-endpoint,
/// auth-specific scopes, control-plane writes, and lockout state. Adds
/// X-RateLimit-* headers to responses.
pub async fn rate_limit_middleware(
    State(state): State<Arc<GatewayState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Check if rate limiting is enabled
    let (rate_limit_enabled, use_multi_tier, shared_secret) = {
        let config = state.config.read().await;
        (
            config.security.rate_limit.enabled,
            config.security.rate_limit.multi_tier,
            config.security.shared_token.clone(),
        )
    };

    if !rate_limit_enabled {
        return Ok(next.run(req).await);
    }

    let ip = extract_client_ip(&req);

    // Loopback exemption for multi-tier rate limiter.
    if use_multi_tier && state.auth.multi_tier_rate_limiter.loopback_exempt() && is_localhost_ip(ip)
    {
        return Ok(next.run(req).await);
    }

    // Detect request scope for multi-tier rate limiting.
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let scope =
        RequestScope::detect(req.method(), req.uri().path(), auth_header, shared_secret.as_deref());

    // Get user identifier (from auth token if available, else IP)
    let user_id = {
        let auth_header = req.headers().get("authorization");
        if let Some(header_value) = auth_header {
            if let Ok(header_str) = header_value.to_str() {
                if let Some(token) = header_str.strip_prefix("Bearer ") {
                    if let Some(session) = state.auth.manager.validate_session(token).await {
                        session.user_id
                    } else {
                        extract_client_ip(&req)
                            .map(|ip| UserId::new(ip.to_string()))
                            .unwrap_or_else(|| UserId::new("anonymous"))
                    }
                } else {
                    extract_client_ip(&req)
                        .map(|ip| UserId::new(ip.to_string()))
                        .unwrap_or_else(|| UserId::new("anonymous"))
                }
            } else {
                extract_client_ip(&req)
                    .map(|ip| UserId::new(ip.to_string()))
                    .unwrap_or_else(|| UserId::new("anonymous"))
            }
        } else {
            extract_client_ip(&req)
                .map(|ip| UserId::new(ip.to_string()))
                .unwrap_or_else(|| UserId::new("anonymous"))
        }
    };

    if use_multi_tier {
        // Multi-tier sliding window rate limiting
        let endpoint = req.uri().path().to_string();

        let result = state
            .auth
            .multi_tier_rate_limiter
            .check_scoped(&user_id, ip, &endpoint, &scope)
            .await;

        match result {
            crate::gateway::rate_limit::MultiTierResult::Allowed { remaining } => {
                let mut response = next.run(req).await;
                let headers = response.headers_mut();
                headers.insert("X-RateLimit-Limit", HeaderValue::from_static("100"));
                headers.insert(
                    "X-RateLimit-Remaining",
                    remaining
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                Ok(response)
            }
            crate::gateway::rate_limit::MultiTierResult::Denied { tier, retry_after_secs } => {
                warn!("Rate limit exceeded for user: {} on tier: {}", user_id, tier);
                let mut response = Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::from(format!(
                        "Rate limit exceeded on tier '{}'. Retry after {} seconds.",
                        tier, retry_after_secs
                    )))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                response.headers_mut().insert(
                    "Retry-After",
                    retry_after_secs
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                response.headers_mut().insert(
                    "X-RateLimit-Tier",
                    tier.parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                Ok(response)
            }
        }
    } else {
        // Legacy token bucket rate limiting
        let result = state.auth.rate_limiter.check(&user_id).await;

        match result {
            crate::security::RateLimitResult::Allowed { remaining, reset_after_secs } => {
                let mut response = next.run(req).await;

                let headers = response.headers_mut();
                headers.insert(
                    "X-RateLimit-Limit",
                    state
                        .auth
                        .rate_limiter
                        .get_state(&user_id)
                        .await
                        .map(|s| s.capacity)
                        .unwrap_or(100)
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                headers.insert(
                    "X-RateLimit-Remaining",
                    remaining
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                headers.insert(
                    "X-RateLimit-Reset",
                    reset_after_secs
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );

                Ok(response)
            }
            crate::security::RateLimitResult::Denied { retry_after_secs } => {
                warn!("Rate limit exceeded for user: {}", user_id);
                let mut response = Response::builder()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(Body::from(format!(
                        "Rate limit exceeded. Retry after {} seconds.",
                        retry_after_secs
                    )))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                response.headers_mut().insert(
                    "Retry-After",
                    retry_after_secs
                        .to_string()
                        .parse()
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );

                Ok(response)
            }
        }
    }
}

/// Check whether an IP address is localhost.
fn is_localhost_ip(ip: Option<IpAddr>) -> bool {
    ip.is_some_and(|ip| ip.is_loopback())
}

/// Generate a random CSP nonce (16 bytes hex = 32 chars)
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// CSP policy configuration per route type
#[derive(Debug, Clone)]
pub enum CspPolicy {
    /// Strict default policy
    Strict,
    /// API-specific policy (no inline restrictions)
    Api,
    /// Web terminal / admin UI policy with nonce support
    Admin { nonce: String },
}

impl CspPolicy {
    /// Build the CSP string
    pub fn to_header_value(&self) -> String {
        match self {
            CspPolicy::Strict => "default-src 'self'; script-src 'self'; style-src 'self' \
                                  'unsafe-inline'; img-src 'self' data:; font-src 'self'; \
                                  connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri \
                                  'self'; form-action 'self';"
                .to_string(),
            CspPolicy::Api => "default-src 'none'; frame-ancestors 'none';".to_string(),
            CspPolicy::Admin { nonce } => format!(
                "default-src 'self'; script-src 'self' 'nonce-{nonce}'; style-src 'self' \
                 'nonce-{nonce}' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; \
                 connect-src 'self' ws: wss:; frame-ancestors 'none'; base-uri 'self'; \
                 form-action 'self';",
                nonce = nonce
            ),
        }
    }
}

/// Middleware: Security headers
///
/// Adds comprehensive security headers to all responses, including:
/// - Content-Security-Policy (with nonce for admin routes)
/// - X-Content-Type-Options
/// - X-Frame-Options
/// - Referrer-Policy
/// - Permissions-Policy
/// - Strict-Transport-Security
pub async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path();

    // Determine CSP policy based on route
    let csp_policy = if path.starts_with("/api/") {
        CspPolicy::Api
    } else if path.starts_with("/admin") || path.starts_with("/terminal") {
        CspPolicy::Admin { nonce: generate_nonce() }
    } else {
        CspPolicy::Strict
    };

    let mut response = next.run(req).await;
    let headers = response.headers_mut();

    // Content Security Policy with route-aware strategy
    headers.insert(
        "Content-Security-Policy",
        HeaderValue::try_from(csp_policy.to_header_value())
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'")),
    );

    headers.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    headers.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    headers.insert("Referrer-Policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
    headers.insert(
        "Permissions-Policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        "Strict-Transport-Security",
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );

    response
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// A credential in a URL must not survive into a log line.
    #[test]
    fn redact_uri_masks_credentials_and_keeps_the_rest() {
        let uri = "/ws?token=super-secret&since=42"
            .parse::<axum::http::Uri>()
            .unwrap();
        assert_eq!(redact_uri(&uri), "/ws?token=***&since=42");

        // Whatever else a client may call it, and whatever case they use.
        for param in [
            "ticket",
            "access_token",
            "api_key",
            "APIKEY",
            "secret",
            "password",
        ] {
            let uri = format!("/x?{param}=leak-me")
                .parse::<axum::http::Uri>()
                .unwrap();
            assert_eq!(redact_uri(&uri), format!("/x?{param}=***"));
        }

        // Nothing sensitive to hide, nothing hidden.
        let uri = "/api/v1/artifacts/a.md?to=pptx"
            .parse::<axum::http::Uri>()
            .unwrap();
        assert_eq!(redact_uri(&uri), "/api/v1/artifacts/a.md?to=pptx");

        // No query at all.
        let uri = "/health".parse::<axum::http::Uri>().unwrap();
        assert_eq!(redact_uri(&uri), "/health");

        // A bare flag carries no value; a value-less name is left alone.
        let uri = "/ws?token".parse::<axum::http::Uri>().unwrap();
        assert_eq!(redact_uri(&uri), "/ws?token");
    }

    #[test]
    fn test_shared_token_matches() {
        assert!(shared_token_matches(Some("secret-token"), "secret-token"));
        assert!(!shared_token_matches(Some("secret-token"), "wrong"));
        assert!(!shared_token_matches(Some("secret-token"), "secret-token-longer"));
        assert!(!shared_token_matches(Some("secret-token"), "short"));
        // No configured token never matches — even an empty presented one.
        assert!(!shared_token_matches(None, "secret-token"));
        assert!(!shared_token_matches(None, ""));
    }

    #[test]
    fn test_is_localhost() {
        assert!(is_localhost(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_localhost(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 53))));
        assert!(!is_localhost(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn test_is_localhost_ipv6() {
        assert!(is_localhost(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))));
        assert!(!is_localhost(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 2))));
    }

    #[test]
    fn test_is_tailscale() {
        // 100.64.0.0/10 range
        assert!(is_tailscale(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale(IpAddr::V4(Ipv4Addr::new(100, 100, 50, 25))));
        assert!(is_tailscale(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 255))));

        // Outside range
        assert!(!is_tailscale(IpAddr::V4(Ipv4Addr::new(100, 63, 255, 255))));
        assert!(!is_tailscale(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        assert!(!is_tailscale(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn test_is_tailscale_ipv6() {
        // Tailscale only supports IPv4 CGNAT
        assert!(!is_tailscale(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))));
    }

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_private_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn test_is_private_ip_ipv6() {
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))));
        assert!(!is_private_ip(IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0, 0, 0, 0, 0, 0x8888))));
    }

    #[test]
    fn test_allowed_origin_default() {
        assert!(matches!(AllowedOrigin::default(), AllowedOrigin::Localhost));
    }

    #[test]
    fn test_is_ip_allowed_any() {
        assert!(is_ip_allowed(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), &AllowedOrigin::Any));
    }

    #[test]
    fn test_is_ip_allowed_localhost() {
        assert!(is_ip_allowed(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            &AllowedOrigin::Localhost
        ));
        assert!(!is_ip_allowed(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            &AllowedOrigin::Localhost
        ));
    }

    #[test]
    fn test_is_ip_allowed_tailscale() {
        assert!(is_ip_allowed(
            IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1)),
            &AllowedOrigin::Tailscale
        ));
        assert!(!is_ip_allowed(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            &AllowedOrigin::Tailscale
        ));
    }

    #[test]
    fn test_is_ip_allowed_private() {
        assert!(is_ip_allowed(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), &AllowedOrigin::Private));
        assert!(!is_ip_allowed(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), &AllowedOrigin::Private));
    }

    #[test]
    fn test_is_ip_allowed_ip_list() {
        let allowed = AllowedOrigin::IpList(vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        ]);
        assert!(is_ip_allowed(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), &allowed));
        assert!(!is_ip_allowed(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), &allowed));
    }

    #[test]
    fn test_extract_client_ip_x_forwarded_for() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("203.0.113.195, 70.41.3.18"));
        let ip = extract_client_ip(&req);
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 195))));
    }

    #[test]
    fn test_extract_client_ip_x_real_ip() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert("x-real-ip", HeaderValue::from_static("192.168.1.1"));
        let ip = extract_client_ip(&req);
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn test_extract_client_ip_x_forwarded_for_priority() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
        req.headers_mut()
            .insert("x-real-ip", HeaderValue::from_static("192.168.1.1"));
        let ip = extract_client_ip(&req);
        // X-Forwarded-For takes priority
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn test_extract_client_ip_no_headers() {
        let req = Request::new(Body::empty());
        let ip = extract_client_ip(&req);
        assert_eq!(ip, None);
    }

    #[test]
    fn test_extract_client_ip_invalid() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        let ip = extract_client_ip(&req);
        assert_eq!(ip, None);
    }

    #[test]
    fn test_csp_policy_strict() {
        let policy = CspPolicy::Strict;
        let header = policy.to_header_value();
        assert!(header.contains("default-src 'self'"));
        assert!(header.contains("script-src 'self'"));
        assert!(header.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn test_csp_policy_api() {
        let policy = CspPolicy::Api;
        let header = policy.to_header_value();
        assert!(header.contains("default-src 'none'"));
        assert!(header.contains("frame-ancestors 'none'"));
    }

    #[test]
    fn test_csp_policy_admin() {
        let policy = CspPolicy::Admin { nonce: "abc123".to_string() };
        let header = policy.to_header_value();
        assert!(header.contains("script-src 'self' 'nonce-abc123'"));
        assert!(header.contains("style-src 'self' 'nonce-abc123'"));
        assert!(header.contains("img-src 'self' data: blob:"));
    }

    #[test]
    fn test_generate_nonce() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        assert_eq!(nonce1.len(), 32); // 16 bytes hex = 32 chars
        assert_eq!(nonce2.len(), 32);
        assert_ne!(nonce1, nonce2); // Very unlikely to collide
                                    // Should be valid hex
        assert!(hex::decode(&nonce1).is_ok());
    }
}
