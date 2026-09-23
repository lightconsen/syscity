//! The network-allowlist policy proxy: a small HTTP/CONNECT forward proxy
//! bound to 127.0.0.1 that fenced children are routed through when
//! `[security] fence_network_allow` is non-empty.
//!
//! This is deliberately NOT a MITM proxy: CONNECT tunnels are opaque (the
//! policy sees the hostname, never the content), plain HTTP is forwarded
//! byte-for-byte after a Host-header check, and everything is per-domain —
//! no content inspection, no certificate management. macOS enforces the
//! routing at the Seatbelt layer (the only egress is this proxy); on Linux
//! and Windows the routing is advisory (`http_proxy`/`https_proxy` env that
//! well-behaved tools honor) — see docs/security-config.md for the honest
//! enforcement matrix.
//!
//! Coverage is HTTP(S) only. A tool speaking raw TCP (a database driver,
//! `ssh`) either honors the proxy env or bypasses it; nothing here can see
//! it. Loopback targets are NOT exempt — reaching the gateway itself through
//! a fenced child must be allowlisted like any other host.
//!
//! Denials are logged to the audit trail (`AuditEventType::NetworkPolicy`);
//! allowed connections are not (they would flood it).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::security::runtime_audit::{AuditEventType, AuditLogger};

/// The largest request head (request line + headers) the proxy will read
/// before answering 400.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// How long the proxy waits for the first request bytes before closing the
/// connection — idle direct-TLS clients and port scanners must not hold
/// sockets.
const HEAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A running policy proxy. The allowlist lives behind a `RwLock` so config
/// hot reload can update it while children are already routed through.
pub struct NetworkProxy {
    addr: SocketAddr,
    allow: Arc<RwLock<Vec<String>>>,
    accept_handle: Mutex<Option<JoinHandle<()>>>,
}

impl NetworkProxy {
    /// Bind `127.0.0.1:0` and spawn the accept loop. The actual port is read
    /// back from the bound listener so it can be injected into children.
    pub async fn start(
        allow: Arc<RwLock<Vec<String>>>,
        audit: Option<Arc<dyn AuditLogger>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
        let addr = listener.local_addr()?;
        // Entries normalize at load so the hot-reload handle always holds
        // match-ready hosts; junk entries are dropped with a warning.
        let allow = Arc::new(RwLock::new(
            allow
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter_map(|entry| match normalize_allow_entry(entry) {
                    Some(normalized) => Some(normalized),
                    None => {
                        tracing::warn!("network_proxy: dropping unusable allow entry {entry:?}");
                        None
                    }
                })
                .collect(),
        ));
        let allow_for_loop = Arc::clone(&allow);
        let audit_for_loop = audit.clone();
        let accept_handle = tokio::spawn(async move {
            // An accept error means the listener is gone (stop/shutdown);
            // leave the loop so the handle completes.
            while let Ok((client, _peer)) = listener.accept().await {
                let allow = Arc::clone(&allow_for_loop);
                let audit = audit_for_loop.clone();
                tokio::spawn(async move {
                    serve_connection(client, allow, audit).await;
                });
            }
        });
        Ok(Self {
            addr,
            allow,
            accept_handle: Mutex::new(Some(accept_handle)),
        })
    }

    /// The port children are pointed at.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The shared allowlist handle for hot reload.
    pub fn allow_handle(&self) -> Arc<RwLock<Vec<String>>> {
        Arc::clone(&self.allow)
    }

    /// Abort the accept loop and drop the listener.
    pub fn stop(&self) {
        if let Some(handle) = self
            .accept_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }
}

/// The allowlist matcher: an entry matches when the host is exactly the
/// entry or a subdomain of it (`github.com` covers `api.github.com`, not
/// `notgithub.com`). Case-insensitive; IP-literal entries match exactly.
/// Entries are assumed normalized (`normalize_allow_entry` at load).
fn host_allowed(host: &str, allow: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    allow.iter().any(|entry| {
        let entry = entry.to_ascii_lowercase();
        if parse_ip_entry(&entry) {
            return host == entry;
        }
        host == entry || host.ends_with(&format!(".{entry}"))
    })
}

/// Whether an entry is an IP literal (exact-match semantics only — `1.2.3.4`
/// has no subdomains, and `1.2.3.4` must not match `1.2.3.40`).
fn parse_ip_entry(entry: &str) -> bool {
    entry.parse::<std::net::IpAddr>().is_ok()
}

/// Normalize a configured entry: lowercase, strip any `scheme://` prefix,
/// `:port` suffix and `/path`. Empty results are rejected by the caller.
fn normalize_allow_entry(entry: &str) -> Option<String> {
    let mut host = entry.trim().to_ascii_lowercase();
    if let Some((_, rest)) = host.split_once("://") {
        host = rest.to_string();
    }
    let host = host.split('/').next().unwrap_or(&host);
    let host = match host.rsplit_once(':') {
        // `:port` — but not an IPv6 literal like `[::1]` (we don't support
        // bracketed IPv6 entries; the rsplit would mangle them, so drop it).
        Some((_, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            host.split(':').next().unwrap_or(host).to_string()
        }
        _ => host.to_string(),
    };
    let host = host.trim().to_string();
    if host.is_empty() || host.contains('/') || host.contains(' ') {
        None
    } else {
        Some(host)
    }
}

/// One proxied connection: read the request head, decide, tunnel or refuse.
async fn serve_connection(
    mut client: TcpStream,
    allow: Arc<RwLock<Vec<String>>>,
    audit: Option<Arc<dyn AuditLogger>>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Byte-at-a-time up to the cap. A client that overruns the cap or goes
    // quiet mid-head is closed without a response — it is not speaking the
    // protocol this proxy serves. The initial read timeout keeps idle
    // clients and port scanners from holding sockets.
    loop {
        match tokio::time::timeout(HEAD_READ_TIMEOUT, client.read(&mut byte)).await {
            Ok(Ok(0)) => return,
            Ok(Ok(_)) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
                    break;
                }
                if head.len() > MAX_HEAD_BYTES {
                    return;
                }
            }
            Ok(Err(_)) | Err(_) => return,
        }
    }

    let head_str = String::from_utf8_lossy(&head);
    let Some(request_line) = head_str.lines().next() else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("").to_string();

    if method == "CONNECT" {
        // Authority form: `host:port`.
        let (host, port) = match target.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
                (host.to_string(), port.parse::<u16>().unwrap_or(443))
            }
            _ => (target.clone(), 443),
        };
        if !host_allowed(&host, &allow.read().unwrap_or_else(|e| e.into_inner())) {
            audit_deny(&audit, &host, port).await;
            let _ = client
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        // Connect upstream first: 200 only once the tunnel is real, 502 if
        // the upstream is unreachable.
        let upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(up) => up,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        };
        if client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let mut upstream = upstream;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    } else {
        // Plain HTTP: the host comes from the absolute URI on the request
        // line or the Host header. Forward through a raw pipe (byte-for-byte,
        // preserving streaming/chunked bodies); one request per connection.
        let host = if let Some(rest) = target
            .strip_prefix("http://")
            .or_else(|| target.strip_prefix("https://"))
        {
            rest.split('/').next().unwrap_or("").to_string()
        } else {
            head_str
                .lines()
                .skip(1)
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("host")
                        .then(|| value.trim().to_string())
                })
                .unwrap_or_default()
        };
        let (host, port) = match host.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h.to_string(), p.parse::<u16>().unwrap_or(80))
            }
            _ => (host.clone(), 80),
        };
        if host.is_empty() || !host_allowed(&host, &allow.read().unwrap_or_else(|e| e.into_inner()))
        {
            audit_deny(&audit, &host, port).await;
            let _ = client
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        let upstream = match TcpStream::connect((host.as_str(), port)).await {
            Ok(up) => up,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        };
        let mut upstream = upstream;
        if upstream.write_all(&head).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
}

async fn audit_deny(audit: &Option<Arc<dyn AuditLogger>>, host: &str, port: u16) {
    if let Some(audit) = audit {
        audit
            .log_entry(
                AuditEventType::NetworkPolicy,
                "network_proxy".to_string(),
                format!("{host}:{port}"),
                false,
                format!("connect to {host}:{port} refused by fence_network_allow"),
                None,
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::runtime_audit::{AuditEventType, AuditLogger};
    use std::sync::Arc as StdArc;

    #[derive(Debug, Clone)]
    struct Captured {
        event_type: AuditEventType,
        target: String,
        allowed: bool,
    }

    #[derive(Debug)]
    struct CapturingAudit {
        entries: std::sync::Mutex<Vec<Captured>>,
    }
    impl CapturingAudit {
        fn new() -> StdArc<Self> {
            StdArc::new(Self {
                entries: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn captured(&self) -> Vec<Captured> {
            self.entries.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AuditLogger for CapturingAudit {
        async fn log_entry(
            &self,
            event_type: AuditEventType,
            _actor: String,
            target: String,
            allowed: bool,
            _description: String,
            _details: Option<serde_json::Value>,
        ) {
            self.entries
                .lock()
                .unwrap()
                .push(Captured { event_type, target, allowed });
        }
    }

    #[test]
    fn host_matching_is_exact_or_subdomain() {
        let allow = vec!["github.com".to_string()];
        assert!(host_allowed("github.com", &allow));
        assert!(host_allowed("api.github.com", &allow));
        assert!(host_allowed("GitHub.COM", &allow));
        assert!(!host_allowed("notgithub.com", &allow), "no dot boundary, no match");
        assert!(!host_allowed("github.com.evil.io", &allow));
        assert!(!host_allowed("", &allow));
    }

    #[test]
    fn ip_entries_match_exactly() {
        let allow = vec!["127.0.0.1".to_string()];
        assert!(host_allowed("127.0.0.1", &allow));
        assert!(!host_allowed("127.0.0.10", &allow));
        assert!(!host_allowed("127.0.0.2", &allow));
    }

    #[test]
    fn entries_normalize_at_load() {
        assert_eq!(
            normalize_allow_entry("HTTPS://GitHub.com:443/repos"),
            Some("github.com".to_string())
        );
        assert_eq!(normalize_allow_entry("  example.com  "), Some("example.com".to_string()));
        assert_eq!(normalize_allow_entry(""), None);
    }

    /// A full round trip through the running proxy: an allowed CONNECT to a
    /// local echo listener tunnels bytes both ways.
    #[tokio::test]
    async fn allowed_connect_tunnels_to_the_target() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = echo.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = sock.read(&mut buf).await {
                        let _ = sock.write_all(&buf[..n]).await;
                    }
                });
            }
        });

        let proxy = NetworkProxy::start(Arc::new(RwLock::new(vec!["127.0.0.1".to_string()])), None)
            .await
            .unwrap();

        let mut client = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], proxy.port())))
            .await
            .unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut head = vec![0u8; 256];
        let n = client.read(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head[..n]).contains("200 Connection established"));
        client.write_all(b"ping").await.unwrap();
        let mut back = vec![0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        proxy.stop();
    }

    /// A denied CONNECT gets 403 and an audit entry.
    #[tokio::test]
    async fn denied_connect_is_403_and_audited() {
        let audit = CapturingAudit::new();
        let proxy = NetworkProxy::start(
            Arc::new(RwLock::new(vec!["github.com".to_string()])),
            Some(std::sync::Arc::clone(&audit) as Arc<dyn AuditLogger>),
        )
        .await
        .unwrap();

        let mut client = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], proxy.port())))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT evil.example.net:443 HTTP/1.1\r\nHost: evil.example.net\r\n\r\n")
            .await
            .unwrap();
        let mut head = vec![0u8; 256];
        let n = client.read(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head[..n]).contains("403 Forbidden"));
        let entries = audit.captured();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].event_type, AuditEventType::NetworkPolicy));
        assert!(!entries[0].allowed);
        proxy.stop();
    }

    /// Plain HTTP is forwarded by Host header: an allowed GET reaches a
    /// local listener with the original bytes intact.
    #[tokio::test]
    async fn plain_http_forwarded_when_host_allowed() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = target.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 512];
                    if let Ok(n) = sock.read(&mut buf).await {
                        let _ = sock
                            .write_all(
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", n)
                                    .as_bytes(),
                            )
                            .await;
                        let _ = sock.write_all(&buf[..n]).await;
                    }
                });
            }
        });

        let proxy = NetworkProxy::start(Arc::new(RwLock::new(vec!["127.0.0.1".to_string()])), None)
            .await
            .unwrap();

        let mut client = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], proxy.port())))
            .await
            .unwrap();
        client
            .write_all(
                format!("GET /hello HTTP/1.1\r\nHost: 127.0.0.1:{target_port}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut resp = Vec::new();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.read_to_end(&mut resp))
                .await
                .unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"));
        assert!(text.contains("GET /hello"));
        proxy.stop();
    }
}
