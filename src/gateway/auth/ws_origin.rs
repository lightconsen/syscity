//! Origin and Host validation for WebSocket upgrades.
//!
//! A loopback bind is not a boundary for a browser. Any page the user visits
//! can open `ws://127.0.0.1:<port>/ws` — WebSocket handshakes are plain GETs,
//! so neither CORS nor the same-origin policy stops them, and the server has to
//! check `Origin` itself. DNS rebinding gets around the address restriction the
//! same way, which is what the `Host` check is for.
//!
//! Non-browser clients (the CLI and the TUI) send neither header, so an absent
//! `Origin` is allowed: there is no ambient authority to steal when the caller
//! has to present its own credential.

use axum::http::HeaderMap;

/// Origins always accepted, whatever the config says.
///
/// The gateway's own origins are added by [`local_origins`]; these are the
/// desktop shell's, which loads the bundled UI from a Tauri scheme rather than
/// over HTTP.
const TAURI_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// Origins the gateway serves itself from, for the given port.
pub fn local_origins(port: u16) -> Vec<String> {
    ["http", "https"]
        .iter()
        .flat_map(|scheme| {
            ["localhost", "127.0.0.1", "[::1]"]
                .iter()
                .map(move |host| format!("{scheme}://{host}:{port}"))
        })
        .collect()
}

/// Whether a browser Origin may open a WebSocket to this gateway.
pub fn origin_allowed(origin: &str, port: u16, extra: &[String]) -> bool {
    if extra.iter().any(|o| o == origin) {
        return true;
    }
    if TAURI_ORIGINS.contains(&origin) {
        return true;
    }
    local_origins(port).iter().any(|o| o == origin)
}

/// Whether `origin` is the same origin as the `Host` the request was sent to.
///
/// A browser derives both headers from the URL it is loading, so a page cannot
/// make these disagree — which is what makes this pairing trustworthy, and why
/// the UI keeps working when the gateway is reached by a name the built-in
/// allowlist cannot enumerate (a LAN address, a tunnel hostname).
///
/// It is not sufficient on its own for a loopback bind: under DNS rebinding
/// both headers name the attacker's domain and agree with each other, which is
/// exactly what [`host_allowed`] is there to catch.
pub fn origin_matches_host(origin: &str, host_header: &str) -> bool {
    let Some((_scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    authority
        .trim_end_matches('/')
        .eq_ignore_ascii_case(host_header.trim())
}

/// Whether the `Host` header is acceptable for the configured bind address.
///
/// Only meaningful for a loopback bind: there, any host other than a loopback
/// name means the request was addressed through a name that resolves to
/// 127.0.0.1 — the shape of a rebinding attack.
pub fn host_allowed(host_header: &str, bound_loopback: bool) -> bool {
    if !bound_loopback {
        return true;
    }
    let Some((name, _port)) = split_host(host_header) else {
        return false;
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Split `host:port`, handling a bracketed IPv6 literal.
fn split_host(host_header: &str) -> Option<(&str, Option<&str>)> {
    let host = host_header.trim();
    if host.is_empty() {
        return None;
    }
    if let Some(rest) = host.strip_prefix('[') {
        let (name, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':');
        return Some((name, port));
    }
    match host.split_once(':') {
        Some((name, port)) => Some((name, Some(port))),
        None => Some((host, None)),
    }
}

/// Is the configured bind address loopback-only?
pub fn is_loopback_bind(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// Validate an upgrade request's `Origin` (when present) and `Host`.
///
/// Returns a short reason when the request must be refused.
pub fn check_upgrade(
    headers: &HeaderMap,
    bound_host: &str,
    port: u16,
    extra_origins: &[String],
) -> Result<(), String> {
    let loopback = is_loopback_bind(bound_host);

    // Only a *present* Host is evidence of anything: rebinding works by naming
    // the attacker's domain, so a request with no Host at all (an HTTP/2
    // client, a test harness) is not that attack and is left alone.
    if let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        if !host_allowed(host, loopback) {
            return Err(format!("unexpected Host: {host}"));
        }
    }

    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !origin_matches_host(origin, host) && !origin_allowed(origin, port, extra_origins) {
            return Err(format!("origin not allowed: {origin}"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
                v.parse().expect("header value"),
            );
        }
        map
    }

    #[test]
    fn the_gateways_own_origins_are_allowed() {
        assert!(origin_allowed("http://localhost:18080", 18080, &[]));
        assert!(origin_allowed("http://127.0.0.1:18080", 18080, &[]));
        assert!(origin_allowed("http://[::1]:18080", 18080, &[]));
    }

    #[test]
    fn the_desktop_shell_is_allowed() {
        assert!(origin_allowed("tauri://localhost", 18080, &[]));
        assert!(origin_allowed("http://tauri.localhost", 18080, &[]));
    }

    /// The point of the check: a page the user visits must not be able to drive
    /// the local runtime.
    #[test]
    fn a_foreign_page_is_refused() {
        assert!(!origin_allowed("https://evil.example", 18080, &[]));
        assert!(!origin_allowed("http://localhost:5173", 18080, &[]));
        // Same host, different port, is a different origin.
        assert!(!origin_allowed("http://localhost:9999", 18080, &[]));
    }

    #[test]
    fn configured_origins_are_honoured() {
        let extra = vec!["http://localhost:5173".to_string()];
        assert!(origin_allowed("http://localhost:5173", 18080, &extra));
        assert!(!origin_allowed("http://localhost:5174", 18080, &extra));
    }

    #[test]
    fn host_is_checked_only_on_a_loopback_bind() {
        assert!(host_allowed("localhost:18080", true));
        assert!(host_allowed("127.0.0.1:18080", true));
        assert!(host_allowed("[::1]:18080", true));
        assert!(!host_allowed("evil.example:18080", true), "rebinding shape");
        assert!(host_allowed("evil.example:18080", false), "public bind is not our business");
    }

    #[test]
    fn a_browser_request_from_a_foreign_origin_is_refused() {
        let h = headers(&[
            ("host", "127.0.0.1:18080"),
            ("origin", "https://evil.example"),
        ]);
        let err = check_upgrade(&h, "127.0.0.1", 18080, &[]).expect_err("must refuse");
        assert!(err.contains("origin not allowed"), "{err}");
    }

    #[test]
    fn a_rebinding_style_host_is_refused() {
        let h = headers(&[("host", "attacker.example:18080")]);
        let err = check_upgrade(&h, "127.0.0.1", 18080, &[]).expect_err("must refuse");
        assert!(err.contains("Host"), "{err}");
    }

    /// The CLI and the TUI send no Origin — they carry their own credential, so
    /// there is no ambient authority to abuse.
    #[test]
    fn a_non_browser_client_is_allowed() {
        let h = headers(&[("host", "127.0.0.1:18080")]);
        assert!(check_upgrade(&h, "127.0.0.1", 18080, &[]).is_ok());

        // No Host either: an HTTP/2 client puts it in `:authority`, and a
        // missing Host is not the shape of a rebinding attack.
        assert!(check_upgrade(&headers(&[]), "127.0.0.1", 18080, &[]).is_ok());
    }

    #[test]
    fn the_web_ui_on_its_own_origin_is_allowed() {
        let h = headers(&[
            ("host", "localhost:18080"),
            ("origin", "http://localhost:18080"),
        ]);
        assert!(check_upgrade(&h, "127.0.0.1", 18080, &[]).is_ok());
    }

    /// The same-origin shortcut: the UI must keep working when the gateway is
    /// reached by an address the allowlist cannot enumerate (LAN, tunnel).
    #[test]
    fn a_same_origin_request_is_allowed_whatever_the_host() {
        let h = headers(&[
            ("host", "192.168.1.5:18080"),
            ("origin", "http://192.168.1.5:18080"),
        ]);
        assert!(check_upgrade(&h, "0.0.0.0", 18080, &[]).is_ok());
        assert!(origin_matches_host("http://192.168.1.5:18080", "192.168.1.5:18080"));
        assert!(!origin_matches_host("http://evil.example", "192.168.1.5:18080"));
    }

    /// Rebinding agrees with itself (both headers name the attacker's domain),
    /// which is why the Host check has to run first.
    #[test]
    fn rebinding_is_caught_by_the_host_check_not_the_origin_one() {
        let h = headers(&[
            ("host", "evil.example:18080"),
            ("origin", "http://evil.example"),
        ]);
        assert!(
            origin_matches_host("http://evil.example", "evil.example:18080") == false,
            "the port difference alone would not save us"
        );
        let err = check_upgrade(&h, "127.0.0.1", 18080, &[]).expect_err("must refuse");
        assert!(err.contains("Host"), "{err}");
    }

    #[test]
    fn loopback_bind_detection() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("::1"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.5"));
    }
}
