# Syscity Security Documentation

This document outlines the security model, known vulnerabilities, and security practices for Syscity.

## Security Model

### Defense in Depth

Syscity implements multiple layers of security:

1. **Input Validation**: All user inputs are validated through JSON schema
2. **Sandboxing**: Shell commands run with restricted permissions
3. **Allowlists**: Explicit allowlists for paths and commands
4. **Rate Limiting**: Per-user request throttling
5. **Authentication**: Pairing codes for new users

## Known Security Issues

### Vulnerabilities (from cargo audit)

#### 1. SQLx Binary Protocol Issue (RUSTSEC-2024-0363) — RESOLVED
- **Crate**: `sqlx` v0.7.4 → v0.8.6
- **Severity**: High
- **Issue**: Binary Protocol Misinterpretation caused by Truncating or Overflowing Casts
- **Status**: **Fixed** — upgraded to 0.8.6 on 2026-06-04
- **Impact**: Affects SQLite protocol handling

#### 2. Wasmtime Sandbox Escapes (Multiple RUSTSEC advisories) — RESOLVED
- **Crate**: `wasmtime` v15.0 → v45.0.0, `wasmtime-wasi` v15.0 → v45.0.0
- **Severity**: High/Critical
- **Issues**: ~15 vulnerabilities including sandbox escapes, data leakage, panics, heap OOB reads
- **Status**: **Fixed** — upgraded to 45.0.0 on 2026-06-04
- **Impact**: Affects WASM plugin execution

### Previously Reported (Now Resolved)

#### RSA Timing Sidechannel (RUSTSEC-2023-0071) — NOT EXPLOITABLE
- **Crate**: `rsa` v0.9.10
- **Status**: **Present in build dependencies only** — `rsa` is a transitive dependency of `sqlx-mysql`, pulled in via `sqlx-macros` during compile-time macro expansion.
- **Impact**: Syscity only uses SQLite (`sqlx-sqlite`). MySQL support is never enabled at runtime, and the `rsa` crate is never loaded or executed.
- **Mitigation**: Tracked in `.cargo/audit.toml` ignore list with documented reason; no runtime exposure.

#### rustls-pemfile v1.0.4 (RUSTSEC-2025-0134)
- **Status**: **No longer in dependency tree** — removed after upstream crate updates.

### Transitive Vulnerabilities (Blocked by Upstream)

The following known issues exist in transitive dependencies and cannot be fixed until upstream crates release updates. Not all of them are vulnerabilities: an unsoundness is a correctness defect in a dependency that a caller could trip, and it is listed here because Dependabot reports it as one.

#### rustls-webpki Certificate Parsing Vulnerabilities
- **Crate**: `rustls-webpki` v0.102.8 (via `serenity` → `tokio-tungstenite` 0.21.0 → `rustls` 0.22.4)
- **Advisories**: RUSTSEC-2026-0049, RUSTSEC-2026-0098, RUSTSEC-2026-0099, RUSTSEC-2026-0104
- **Issues**: CRL parsing panic, name constraint bypass, wildcard certificate acceptance
- **Status**: **Blocked upstream** — `serenity` 0.12.5 locks `tokio-tungstenite` 0.21.0 which requires `rustls` 0.22.4
- **Mitigation**: Tracked in `deny.toml` ignore list with documented reason; monitor serenity releases

#### glib `VariantStrIter` Unsoundness
- **Crate**: `glib` v0.18.5 (via `gtk` 0.18.2 → `tauri` 2.11.2 → `syscity-desktop`); Linux targets only
- **Advisories**: RUSTSEC-2024-0429 (GHSA-wrw7-89jp-8q8g) — `informational = "unsound"`, not a vulnerability
- **Issues**: The `Iterator` and `DoubleEndedIterator` impls for `glib::VariantStrIter` (`next`, `nth`, `last`, `next_back`, `nth_back`) passed an immutable reference to a pointer the C function wrote through. Under optimization the write is discarded, so the iterator can dereference NULL and crash.
- **Status**: **Blocked upstream** — the fix is `glib >= 0.20.0`, which arrives with `gtk` 0.19.0 (`gtk` 0.18.2 requires `glib ^0.18`). The newest Tauri, 2.11.5, still declares `gtk ^0.18`, so no version in range can carry the fix. This is one upstream pin away, not an open-ended wait: **when Tauri moves to `gtk ^0.19`, the advisory clears on its own**.
- **Mitigation**: Not tracked in `deny.toml`. It does not need to be: `cargo audit` and `cargo deny check` deny the `vulnerability` category and only warn on `unsound`, so the CI gate passes on its own — an ignore entry would be noise here, and would hide the fix once it lands. Dependabot lists it because it does not make that distinction. Nothing in Syscity's own code calls `VariantStrIter`; exposure would come from the GTK stack iterating GVariant strings.

### Unmaintained Dependencies

The following dependencies are unmaintained but don't have known security vulnerabilities:

1. **paste** (RUSTSEC-2024-0436) — Used by sqlx-core, will be resolved when sqlx removes it
2. **proc-macro-error** (RUSTSEC-2024-0370) — Used by teloxide, will be resolved when teloxide updates
3. **daemonize** (RUSTSEC-2025-0069) — Used for Unix daemonization, no safe upgrade available

These are transitive dependencies and will be updated when upstream crates release updates.

## Security Features

### Tool Security

#### Path Traversal Protection

Syscity's `SecurityValidator` detects and blocks path traversal attempts:

```rust
// Blocked patterns:
- "../"              - Directory traversal
- "..\\"             - Windows traversal
- "~/.."             - Home directory escape
- "/.."              - Root escape
- "%2e%2e%2f"        - URL-encoded traversal
- "%252e%252e%252f"  - Double URL-encoded
- "//"               - Double slash
```

#### Command Injection Protection

The following characters are blocked in shell commands:
- `;` - Command separator
- `&` - Background process
- `|` - Pipe
- `$` - Variable substitution
- `` ` `` - Command substitution
- `$(` - Command substitution
- `${` - Variable expansion

### Configuration Security

#### Environment Variables

Sensitive configuration should use environment variables:

```yaml
provider:
  api_key: "${OPENAI_API_KEY}"  # Never hardcode secrets
```

#### Sandbox Configuration

Default sandbox settings:

```yaml
security:
  sandbox:
    enabled: true
    allowed_commands: ["ls", "cat", "grep", "curl"]
    forbidden_paths: ["/etc/passwd", "~/.ssh/*"]
    timeout_seconds: 30
```

### Network Security

#### TLS Configuration

- All HTTP clients use `rustls-tls` (native Rust TLS, not OpenSSL)
- Certificate validation is enabled by default
- No option to disable TLS verification in production

#### Domain Restrictions

Web tools support domain allowlisting/blocklisting:

```yaml
tools:
  web:
    blocked_domains: ["localhost", "127.0.0.1", "10.*", "192.168.*"]
```

## Security Best Practices

### Deployment

1. **Run as non-root user**
   ```bash
   useradd -r -s /bin/false syscity
   ```

2. **Use read-only filesystem where possible**
   ```docker
   --read-only --tmpfs /tmp
   ```

3. **Limit network access**
   - Only expose necessary ports
   - Use internal networks for database connections

4. **Enable rate limiting**
   ```yaml
   security:
     rate_limits:
       requests_per_minute: 30
   ```

### Secret Management

1. Never commit secrets to version control
2. Use environment variables or secret management systems
3. Rotate API keys regularly
4. Use different keys for different environments

### Monitoring

Monitor for:
- Unusual API request patterns
- Path traversal attempts in logs
- Rate limit violations
- Failed authentication attempts

## Security Checklist

Before deploying Syscity:

- [ ] Changed default API keys
- [ ] Configured allowlists appropriately
- [ ] Enabled sandbox mode
- [ ] Set up rate limiting
- [ ] Configured log rotation
- [ ] Running as non-root user
- [ ] Firewall rules configured
- [ ] Regular backups scheduled
- [ ] Monitoring alerts configured

## Reporting Security Issues

If you discover a security vulnerability:

1. **DO NOT** open a public issue
2. Email security concerns to: security@example.com
3. Include:
   - Description of the vulnerability
   - Steps to reproduce
   - Possible impact
   - Suggested fix (if any)

We will respond within 48 hours and work on a fix.

## Security Update Policy

- Critical vulnerabilities: Fix within 7 days
- High severity: Fix within 30 days
- Medium/Low severity: Fix in next scheduled release
- Dependency updates: Monthly review

## Audit History

| Date | Auditor | Scope | Results |
|------|---------|-------|---------|
| 2024-03 | cargo-audit | Dependencies | 2 vulnerabilities, 3 unmaintained |
| 2026-06-04 | cargo-audit + cargo-deny | Dependencies | wasmtime 15→45, sqlx 0.7→0.8.6, rustls-pemfile removed, rsa removed from tree |

## References

- [RustSec Advisory Database](https://rustsec.org/)
- [OWASP Top 10](https://owasp.org/www-project-top-ten/)
- [CIS Docker Benchmark](https://www.cisecurity.org/benchmark/docker)
