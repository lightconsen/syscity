//! SSRF Navigation Guard
//!
//! Prevents navigation to private IP addresses and other restricted hosts.
//!
//! Checks:
//! - URL scheme must be http/https/about
//! - IP literals are checked against private ranges
//! - Hostnames are resolved and checked against private ranges (DNS rebinding
//!   mitigation — resolves at navigation time to catch hostnames that point to
//!   internal addresses)
//! - Hostnames are checked against blocklist/allowlist

use std::net::IpAddr;

use tokio::net::lookup_host;
use tracing::debug;

/// Navigation policy for URL validation
#[derive(Debug, Clone)]
pub struct NavigationPolicy {
    /// Allow navigation to private IPs
    pub allow_private: bool,
    /// Allowed hostnames (empty = all public allowed)
    pub allowed_hostnames: Vec<String>,
    /// Blocked hostnames
    pub blocked_hostnames: Vec<String>,
}

impl Default for NavigationPolicy {
    fn default() -> Self {
        Self {
            allow_private: false,
            allowed_hostnames: Vec::new(),
            blocked_hostnames: vec!["localhost".to_string()],
        }
    }
}

impl NavigationPolicy {
    /// Create a permissive policy (allows private IPs)
    pub fn permissive() -> Self {
        Self {
            allow_private: true,
            ..Default::default()
        }
    }

    /// Create a restrictive policy (default)
    pub fn restrictive() -> Self {
        Self::default()
    }
}

/// Check whether a hostname matches a pattern.
/// Supports exact match and `*.` prefix wildcard (e.g. `*.example.com`
/// matches `sub.example.com` but NOT `anexample.com`).
fn hostname_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern
        .strip_prefix("/*.")
        .or_else(|| pattern.strip_prefix("*."))
    {
        // suffix is "example.com" for pattern "*.example.com".
        // Use subdomain-boundary matching (dot prefix) to avoid false
        // positives like `anexample.com` matching `*.example.com`.
        host.eq_ignore_ascii_case(suffix) || host.ends_with(&format!(".{}", suffix))
    } else {
        host.eq_ignore_ascii_case(pattern)
    }
}

/// Validate that a URL is allowed by the navigation policy.
///
/// For hostnames (non-IP literals), DNS is resolved so that hostnames pointing
/// to private IPs are also caught (basic DNS rebinding mitigation).
pub async fn assert_navigation_allowed(url: &str, policy: &NavigationPolicy) -> crate::Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| crate::error::SyscityError::Validation(format!("Invalid URL: {}", e)))?;

    let scheme = parsed.scheme();
    if !matches!(scheme, "http" | "https" | "about") {
        return Err(crate::error::SyscityError::Validation(format!(
            "URL scheme '{}' is not allowed",
            scheme
        )));
    }

    let host_raw = parsed.host_str().unwrap_or("");
    // Strip brackets from IPv6 literals (url::Url returns "[::1]" for IPv6)
    let host = if host_raw.starts_with('[') && host_raw.ends_with(']') {
        &host_raw[1..host_raw.len() - 1]
    } else {
        host_raw
    };

    // Check blocked hostnames (supports wildcard: *.example.com)
    if policy
        .blocked_hostnames
        .iter()
        .any(|h| hostname_matches(h, host))
    {
        return Err(crate::error::SyscityError::Validation(format!(
            "Hostname '{}' is blocked",
            host
        )));
    }

    // Check IP literal or resolve hostname and check resolved IPs.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !policy.allow_private && is_private_ip(ip) {
            return Err(crate::error::SyscityError::Validation(format!(
                "Navigation to private IP '{}' is not allowed",
                ip
            )));
        }
    } else if !policy.allow_private {
        // Resolve hostname and verify no resolved address is private
        // (basic DNS rebinding mitigation). Non-fatal if resolution fails:
        // the hostname-based blocklist/allowlist checks above already applied.
        if let Ok(addrs) = lookup_host(format!("{}:80", host)).await {
            for addr in addrs {
                if is_private_ip(addr.ip()) {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Hostname '{}' resolves to private IP '{}'",
                        host,
                        addr.ip()
                    )));
                }
            }
        } else {
            debug!("Could not resolve hostname '{}' for SSRF check", host);
        }
    }

    // Check allowlist (if specified, only allowlisted hosts are permitted;
    // supports wildcard: *.example.com)
    if !policy.allowed_hostnames.is_empty()
        && !policy
            .allowed_hostnames
            .iter()
            .any(|h| hostname_matches(h, host))
    {
        return Err(crate::error::SyscityError::Validation(format!(
            "Hostname '{}' is not in the allowed list",
            host
        )));
    }

    Ok(())
}

/// Check if an IP address is in a private range.
///
/// `pub(crate)` so other outbound-fetch paths (e.g. skill install from a URL)
/// share one definition of "not a public address" instead of growing a second,
/// subtly different one.
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 0.0.0.0/8
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // fc00::/7 (unique local addresses)
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_navigation_guard_blocks_private_ip() {
        let policy = NavigationPolicy::restrictive();

        assert!(assert_navigation_allowed("http://127.0.0.1/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("http://10.0.0.1/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("http://192.168.1.1/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("http://172.16.0.1/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("http://[::1]/", &policy)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_navigation_guard_blocks_localhost() {
        let policy = NavigationPolicy::restrictive();

        // localhost is blocked by hostname blocklist (sync check).
        assert!(assert_navigation_allowed("http://localhost/", &policy)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_navigation_guard_allows_public() {
        let policy = NavigationPolicy::restrictive();

        assert!(assert_navigation_allowed("https://example.com/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("https://google.com/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("https://github.com/", &policy)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_navigation_guard_blocks_invalid_scheme() {
        let policy = NavigationPolicy::restrictive();

        assert!(assert_navigation_allowed("file:///etc/passwd", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("ftp://example.com/", &policy)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_navigation_guard_permissive() {
        let policy = NavigationPolicy::permissive();

        assert!(assert_navigation_allowed("http://127.0.0.1/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("http://192.168.1.1/", &policy)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_navigation_guard_allowlist() {
        let policy = NavigationPolicy {
            allow_private: false,
            allowed_hostnames: vec!["example.com".to_string()],
            blocked_hostnames: Vec::new(),
        };

        assert!(assert_navigation_allowed("https://example.com/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("https://google.com/", &policy)
            .await
            .is_err());
    }

    #[test]
    fn test_hostname_matches() {
        assert!(hostname_matches("localhost", "localhost"));
        assert!(!hostname_matches("localhost", "example.com"));
        // Wildcard: *.example.com
        assert!(hostname_matches("*.example.com", "sub.example.com"));
        assert!(hostname_matches("*.example.com", "deep.sub.example.com"));
        assert!(hostname_matches("*.example.com", "example.com"));
        assert!(!hostname_matches("*.example.com", "other.com"));
        // Wildcard: /*. form
        assert!(hostname_matches("/*.example.com", "sub.example.com"));
        assert!(!hostname_matches("/*.example.com", "other.com"));
        // Regression: wildcard must use subdomain boundary, not suffix match
        assert!(
            !hostname_matches("*.example.com", "anexample.com"),
            "should NOT match anexample.com against *.example.com"
        );
        assert!(
            !hostname_matches("*.example.com", "notexample.com"),
            "should NOT match notexample.com against *.example.com"
        );
    }

    #[tokio::test]
    async fn test_navigation_guard_blocks_wildcard_hostnames() {
        let policy = NavigationPolicy {
            allow_private: false,
            allowed_hostnames: Vec::new(),
            blocked_hostnames: vec!["*.malicious.com".to_string()],
        };
        assert!(assert_navigation_allowed("http://sub.malicious.com/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("http://evil.malicious.com/", &policy)
            .await
            .is_err());
        assert!(assert_navigation_allowed("https://example.com/", &policy)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_navigation_guard_allowlist_wildcard() {
        let policy = NavigationPolicy {
            allow_private: false,
            allowed_hostnames: vec!["*.example.com".to_string()],
            blocked_hostnames: Vec::new(),
        };
        assert!(assert_navigation_allowed("https://sub.example.com/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("https://example.com/", &policy)
            .await
            .is_ok());
        assert!(assert_navigation_allowed("https://other.com/", &policy)
            .await
            .is_err());
    }

    #[test]
    fn test_is_private_ip() {
        use std::net::Ipv4Addr;

        assert!(is_private_ip(Ipv4Addr::new(127, 0, 0, 1).into()));
        assert!(is_private_ip(Ipv4Addr::new(10, 0, 0, 1).into()));
        assert!(is_private_ip(Ipv4Addr::new(192, 168, 1, 1).into()));
        assert!(is_private_ip(Ipv4Addr::new(172, 16, 0, 1).into()));
        assert!(!is_private_ip(Ipv4Addr::new(8, 8, 8, 8).into()));
        assert!(!is_private_ip(Ipv4Addr::new(1, 1, 1, 1).into()));
    }
}
