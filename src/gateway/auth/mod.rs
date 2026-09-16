//! Gateway Authentication Module
//!
//! Authenticates via Bearer token, shared token, device pairing and Tailscale
//! (`security::AuthManager` and per-mode handlers in `ws/`). Session *cookies*
//! are a forward-compat shim only: no code ever issues a `Set-Cookie`, so
//! nothing populates them — the types and parsers below exist so a future
//! OAuth-callback session (or an external reverse proxy) can hand a token over
//! without inventing the plumbing after the fact.

use axum::extract::Request;
use axum::http::header;
use serde::{Deserialize, Serialize};

pub mod ws_origin;

/// Session cookie configuration.
///
/// **Not in active use.** No production code sets a `Set-Cookie` header, so
/// cookies are never present when these parsers run — they are kept, and kept
/// honest, as the forward-compat surface for a session handed over by an
/// OAuth callback or a reverse proxy. If that day never comes, this is
/// deletable along with `extract_session_cookie*` and their one call site in
/// the rate limiter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCookieConfig {
    /// Cookie name
    pub name: String,
    /// Cookie domain
    pub domain: Option<String>,
    /// Cookie path
    pub path: String,
    /// Secure flag (HTTPS only)
    pub secure: bool,
    /// HttpOnly flag
    pub http_only: bool,
    /// SameSite policy
    pub same_site: String,
    /// Max age in seconds
    pub max_age_secs: i64,
}

impl Default for SessionCookieConfig {
    fn default() -> Self {
        Self {
            name: "syscity_session".to_string(),
            domain: None,
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: "lax".to_string(),
            max_age_secs: 86400 * 7, // 7 days
        }
    }
}

/// Extract a session token from the `Cookie` request header.
///
/// See the module docs and [`SessionCookieConfig`]: nothing populates this
/// today — the return value is almost always `None` because no code sets the
/// cookie. The parser is kept functional regardless, so a future OAuth
/// callback or reverse proxy can feed it without new plumbing.
pub fn extract_session_cookie(req: &Request, cookie_name: &str) -> Option<String> {
    extract_session_cookie_from_headers(req.headers(), cookie_name)
}

/// Extract session token from a HeaderMap directly (for use before upgrade)
pub fn extract_session_cookie_from_headers(
    headers: &axum::http::HeaderMap,
    cookie_name: &str,
) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?;
    let cookie_str = cookie_header.to_str().ok()?;
    for cookie in cookie_str.split(';') {
        let (name, value) = cookie.trim().split_once('=')?;
        if name == cookie_name {
            return Some(value.to_string());
        }
    }
    None
}

/// CORS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Enable CORS
    pub enabled: bool,
    /// Allowed origins (use ["*"] for any)
    pub allowed_origins: Vec<String>,
    /// Allowed methods
    pub allowed_methods: Vec<String>,
    /// Allowed headers
    pub allowed_headers: Vec<String>,
    /// Allow credentials (cookies)
    pub allow_credentials: bool,
    /// Max age for preflight cache
    pub max_age_secs: u32,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec!["*".to_string()],
            allowed_methods: vec![
                "GET".to_string(),
                "POST".to_string(),
                "PUT".to_string(),
                "DELETE".to_string(),
                "OPTIONS".to_string(),
            ],
            allowed_headers: vec![
                "Content-Type".to_string(),
                "Authorization".to_string(),
                "X-Requested-With".to_string(),
            ],
            allow_credentials: true,
            max_age_secs: 3600,
        }
    }
}

/// CSP (Content Security Policy) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CspConfig {
    /// Enable CSP
    pub enabled: bool,
    /// Default CSP policy string
    pub policy: String,
    /// Nonce-enabled script-src (for inline scripts)
    pub use_nonce: bool,
}

impl Default for CspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                     img-src 'self' data:; font-src 'self'; connect-src 'self' ws: wss:; \
                     frame-ancestors 'none'; base-uri 'self'; form-action 'self';"
                .to_string(),
            use_nonce: true,
        }
    }
}

/// Generate a random CSP nonce
pub fn generate_csp_nonce() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    // `gen::<u8>()` — on Windows an extra `FromIterator` impl (encode_unicode)
    // makes the element type ambiguous.
    let bytes: Vec<u8> = (0..16).map(|_| rng.gen::<u8>()).collect();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &bytes)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::*;

    #[test]
    fn test_generate_csp_nonce() {
        let nonce1 = generate_csp_nonce();
        let nonce2 = generate_csp_nonce();
        assert!(!nonce1.is_empty());
        assert_ne!(nonce1, nonce2);
    }

    #[test]
    fn test_session_cookie_config_default() {
        let config = SessionCookieConfig::default();
        assert_eq!(config.name, "syscity_session");
        assert_eq!(config.path, "/");
        assert!(config.secure);
        assert!(config.http_only);
        assert_eq!(config.same_site, "lax");
        assert_eq!(config.max_age_secs, 86400 * 7);
        assert!(config.domain.is_none());
    }

    #[test]
    fn test_session_cookie_config_serde() {
        let config = SessionCookieConfig {
            name: "custom".to_string(),
            domain: Some("example.com".to_string()),
            path: "/app".to_string(),
            secure: false,
            http_only: false,
            same_site: "strict".to_string(),
            max_age_secs: 3600,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: SessionCookieConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "custom");
        assert_eq!(restored.domain, Some("example.com".to_string()));
        assert_eq!(restored.path, "/app");
        assert!(!restored.secure);
        assert_eq!(restored.same_site, "strict");
    }

    #[test]
    fn test_extract_session_cookie_single() {
        let req = Request::builder()
            .header(header::COOKIE, "syscity_session=abc123")
            .body(Body::empty())
            .unwrap();
        let token = extract_session_cookie(&req, "syscity_session");
        assert_eq!(token, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_session_cookie_multiple() {
        let req = Request::builder()
            .header(header::COOKIE, "other=value; syscity_session=xyz; foo=bar")
            .body(Body::empty())
            .unwrap();
        let token = extract_session_cookie(&req, "syscity_session");
        assert_eq!(token, Some("xyz".to_string()));
    }

    #[test]
    fn test_extract_session_cookie_missing() {
        let req = Request::builder()
            .header(header::COOKIE, "other=value")
            .body(Body::empty())
            .unwrap();
        let token = extract_session_cookie(&req, "syscity_session");
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_session_cookie_no_header() {
        let req = Request::builder().body(Body::empty()).unwrap();
        let token = extract_session_cookie(&req, "syscity_session");
        assert_eq!(token, None);
    }

    #[test]
    fn test_extract_session_cookie_no_header_noop() {
        // The session-cookie path is kept only for rate-limit identification;
        // no code creates cookies anymore, so extraction always yields None.
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(extract_session_cookie(&req, "syscity_session"), None);
    }

    #[test]
    fn test_cors_config_default() {
        let config = CorsConfig::default();
        assert!(config.enabled);
        assert_eq!(config.allowed_origins, vec!["*"]);
        assert!(config.allow_credentials);
        assert_eq!(config.max_age_secs, 3600);
    }

    #[test]
    fn test_csp_config_default() {
        let config = CspConfig::default();
        assert!(config.enabled);
        assert!(!config.policy.is_empty());
        assert!(config.use_nonce);
    }

    #[test]
    fn test_csp_nonce_url_safe() {
        let nonce = generate_csp_nonce();
        assert!(!nonce.contains('+'));
        assert!(!nonce.contains('/'));
        assert!(!nonce.contains('='));
    }
}
