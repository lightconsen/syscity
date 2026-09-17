# CLAUDE.md - Syscity Project

## API Protocol Convention

- **Prefer the WebSocket RPC protocol (`/ws`)** for every client-facing
  operation. The gateway is WS-native: the built-in UI (web/desktop/mobile)
  and the CLI talk to the gateway exclusively over WS
  (`WsRequest`/`WsResponse`, methods dispatched in `src/gateway/ws/core.rs`).
- **Do not add a new REST endpoint when a WS method can serve the caller.**
  Add a WS method instead (see "Adding a WS method" below).
- REST is reserved for cases that genuinely require HTTP:
  - OpenAI-compatible wire protocol (`/v1/chat/completions`, `/v1/models`) —
    external tools hardcode these paths.
  - OAuth browser redirects (`/api/v1/cloud/login`).
  - Inbound webhooks from external platforms (`/webhooks/*`).
  - File/static download (`/api/v1/artifacts/*`, `/assets/*`).
  - Health/liveness/readiness probes (`/health`, `/ready`, `/live`) and
    Prometheus metrics (`/metrics`) — bare, unversioned paths by convention.
- **Every REST endpoint MUST be authenticated.** No endpoint may be open.
  Register it in the authenticated router (admin tier) so the auth middleware
  applies; keep only what the auth config allows (`shared_token`, device
  pairing, tailscale, trusted proxy).
  - Two exceptions live in the *essential* tier — which carries the
    tailscale/trusted-proxy allow-check, rate limiting and security headers,
    but not the token middleware:
    - `/webhooks/*` — authenticated by per-channel signature verification
      instead of a token. Every handler fails closed when its channel's secret
      is absent or the signature is wrong, so this is authentication by other
      means, not an open route.
    - `/api/v1/artifacts/*` — download path for files the gateway itself
      produced, reachable from loopback/tailnet.
  - `/health`, `/ready`, `/live`, `/metrics` are unauthenticated on purpose
    (probes and scraping) and sit on the same tier.
- **Adding a WS method:**
  1. Implement the handler in `src/gateway/ws/admin_ws.rs` (or the relevant
     `ws/` submodule).
  2. Register the dispatch arm in `src/gateway/ws/core.rs`.
  3. Register the scope in `src/gateway/protocol.rs` `method_scope()`
     (read → `SCOPE_READ`, write → `SCOPE_WRITE`).
  4. Add a `#[tokio::test]` covering the new method.

## Rust Best Practices

### Code Style & Formatting
- Follow the official Rust style guide (`cargo fmt`)
- Maximum line length: 100 characters
- Use `cargo clippy` for linting and fix all warnings
- Enable `#![deny(unsafe_code)]` where possible

### Error Handling
- Use `thiserror` for defining error types
- Use `anyhow` for application-level error handling
- Prefer `Result<T, E>` over panics
- Use `?` operator for error propagation
- Provide descriptive error messages with context

### Naming Conventions
- `PascalCase` for types, traits, enums, structs
- `snake_case` for functions, variables, modules
- `SCREAMING_SNAKE_CASE` for constants, statics
- `PascalCase` for enum variants and type parameters

### Documentation
- Document all public APIs with `///`
- Include examples in doc comments
- Use `cargo doc` to verify documentation builds
- Add module-level documentation with `//!`

### Testing
- Write unit tests in the same file (`#[cfg(test)] mod tests`)
- Use `cargo test` for running tests
- Aim for >80% code coverage
- Use `tokio::test` for async tests
- Use `mockall` for mocking dependencies

### Async Programming
- Use `tokio` as the async runtime
- Prefer `async/await` syntax
- Avoid blocking operations in async contexts
- Use `tokio::sync` primitives for synchronization

### Project Structure
```
syscity/
├── Cargo.toml
├── CLAUDE.md
├── src/
│   ├── main.rs          # Application entry point
│   ├── lib.rs           # Library exports
│   ├── config.rs        # Configuration management
│   ├── cli.rs           # CLI argument parsing
│   ├── error.rs         # Error types
│   ├── core/            # Core business logic
│   │   ├── mod.rs
│   │   ├── models.rs    # Domain models
│   │   └── engine.rs    # Core engine
│   ├── adapters/        # External adapters
│   │   ├── mod.rs
│   │   ├── storage.rs   # Storage implementations
│   │   └── api.rs       # API clients
│   └── utils/           # Utilities
│       ├── mod.rs
│       └── logging.rs   # Logging setup
└── tests/               # Integration tests
    └── integration_tests.rs
```

### Dependencies
- Keep dependencies minimal and justified
- Use workspace dependencies for multi-crate projects
- Pin critical dependencies to specific versions
- Regularly run `cargo audit` for security

### Performance
- Use `cargo bench` for benchmarking
- Profile before optimizing
- Prefer zero-copy where possible
- Use `Arc<str>` over `String` for shared immutable strings
- Leverage iterators and lazy evaluation

### Safety
- Minimize use of `unsafe` code
- Document and justify all `unsafe` blocks
- Use safe abstractions where possible
- Run `miri` for undefined behavior detection

## Development Workflow

1. **Before committing:**
   - Run `./scripts/self-check.sh` (runs all automated checks)
   - Or run individual steps: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`
   - Run `./scripts/static-analysis.sh --full` for cross-module anti-patterns
   - Run `cargo doc` to check docs build
   - Security gate: a versioned pre-commit hook (`.githooks/pre-commit`) runs
     fast staged-content checks (`scripts/staged-checks.sh`: conflict markers,
     oversized files, rustfmt, secret patterns) and then `./scripts/audit.sh`
     (`cargo audit` + `cargo deny`), rejecting the commit on failure. Enable it
     once per clone with `git config core.hooksPath .githooks`; bypass a single
     commit with `git commit --no-verify`.

2. **CI Checks:**
   - Format check: `cargo fmt -- --check`
   - Linting: `cargo clippy -- -D warnings`
   - Tests: `cargo test --all-features`
   - Documentation: `cargo doc --no-deps`
   - Security audit: `cargo audit`
   - Static analysis: `./scripts/static-analysis.sh`

3. **Self-review checklist (review every change against these):**
   - **Error handling**: No `let _ =` or `.ok()` silently drops errors? (static-analysis.sh checks this)
   - **Lock safety**: No `std::sync::Mutex` held across `.await`? (static-analysis.sh flags potential cases)
   - **Task registration**: Are all `tokio::spawn` handles registered in `TaskRegistry`?
   - **Shutdown safety**: Do all long-running loops use `select!` with a shutdown signal?
   - **Audit/event failures**: Are audit log and event send failures logged with `warn!`?

   The automated checks cover patterns 1-2; patterns 3-5 require manual review. Reference the
   full checklist at `.github/PULL_REQUEST_TEMPLATE.md`.
