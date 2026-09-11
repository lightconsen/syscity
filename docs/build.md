# Build from Source

## Prerequisites

| Requirement | Version | Notes |
|-------------|---------|-------|
| Rust | 1.75+ (MSRV) | Install via [rustup](https://rustup.rs) |
| Node.js | 22 | Only needed to build the web UI |
| pnpm | latest | `npm i -g pnpm` — used by the web build |

### Platform-specific system dependencies

**Linux (Debian/Ubuntu):**
```bash
sudo apt-get install -y build-essential pkg-config libssl-dev
```
A full `--all-features` build (what release CI runs) additionally needs:
```bash
sudo apt-get install -y protobuf-compiler libasound2-dev libdbus-1-dev
# protobuf-compiler: prost builds; libasound2-dev: cpal audio; libdbus-1-dev: libdbus-sys
```

**macOS:**
```bash
xcode-select --install   # Command Line Tools (clang, headers)
```
Desktop control additionally requires granting **Screen Recording** and
**Accessibility** permissions in *System Settings → Privacy & Security*.

**Windows:** Install the [Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
(MSVC toolchain). Desktop control is macOS-first; core agent / channel
functionality works cross-platform.

## Build

```bash
git clone https://github.com/lightconsen/syscity.git
cd syscity
./scripts/build.sh
```

`build.sh` builds the web frontend (`web/` → `dist/`) then compiles the release
binary — with cloud features included by default. To build only the frontend:
`./scripts/build.sh --front`; to build without cloud: `./scripts/build.sh --nocloud`.

To build just the Rust binary without the web bundle:
```bash
cargo build --release
# binary at ./target/release/syscity
```

## Feature flags

Syscity uses Cargo features to gate optional functionality. The defaults enable
all channels plus browser, vision, and local embeddings.

| Feature | Default | Enables |
|---------|:-------:|---------|
| `telegram` / `discord` / `slack` / `whatsapp` / `qq` / `feishu` / `signal` / `imessage` / `webchat` | ✓ | Individual messaging channels |
| `all-channels` | — | All of the above at once |
| `browser` | ✓ | Browser automation (requires Chrome/Chromium installed) |
| `vision` | ✓ | ONNX-based screen vision (`ort`/`image`/`ndarray`) |
| `local-embeddings` | ✓ | On-device embeddings via `llama-cpp-2` + `hf-hub` |
| `plugins` | ✓ | WASM plugin sandbox (`wasmtime`) |
| `hot-reload` | ✓ | Config/plugin hot-reload via file watching |
| `vector-db` / `pgvector` / `sqlite-vec` | — / — / ✓ | Vector memory backends (`sqlite-vec` is the default persistent backend) |
| `keyring` | — | OS keyring (macOS Keychain / Windows DPAPI / Linux Secret Service) as the primary secret store; default builds use 0600 AES-GCM encrypted files only and never touch the keychain |
| `cloud` | — | Syscity Cloud integration (login + cloud LLM/search/KB/connectors). Compiled into release artifacts and `./scripts/build.sh` (runtime opt-out: `syscity start --nocloud`); default `cargo build` excludes it (§2.7). Build in with `--features cloud` |
| `intel-macos` | — | Release profile for Intel Macs: default features minus `vision` (ONNX Runtime ships no prebuilt `x86_64-apple-darwin` library), plus `cloud`. Build with `--no-default-features --features intel-macos` |
| `mobile` | — | Pruned Android/iOS profile: `webchat` + `plugins` + `embedded-assets` + `sqlite-vec` with bundled SQLite; no channels, embeddings, vision, browser, or keyring. Build with `--no-default-features --features mobile` |

Build with a custom feature set:
```bash
# Minimal headless build, no channels or browser
cargo build --release --no-default-features --features webchat,plugins

# Everything
cargo build --release --features all-channels,vector-db
```

## Cross-compiling to Windows (compile check)

There is no Windows machine in this project, so Windows support is verified
by cross-compiling from Linux/macOS with [zig](https://ziglang.org/) as the
linker/CC — zig ships its own mingw-w64 sysroot, so no MinGW install is
needed:

```bash
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu
```

`.cargo/config.toml` wires that target's linker and the target-suffixed
`CC_`/`AR_` vars to the wrappers in `scripts/cross/` (`zig-cc-…`,
`zig-ar-…`); the wrappers rewrite the Rust triple into zig's spelling and
mirror the target dir via symlink. They need zig 0.16 on `PATH` (or the
tarball install under `~/.local/opt`). The vars are target-suffixed, so host
builds are unaffected. This is a **compile-only** check — the resulting
binary is never run here; the desktop release pipeline builds real Windows
bundles natively on `windows-latest` (MSVC).

## Release targets

Release CI (`.github/workflows/release.yml`) publishes:

- CLI tarballs: Linux x64/arm64, macOS arm64, and macOS Intel via the
  `intel-macos` profile (no `vision`/ONNX — `ort` has no prebuilt
  `x86_64-apple-darwin` library).
- Desktop bundles: Linux x64, macOS arm64 + Intel (also `intel-macos`), and
  Windows x64 (MSVC).
- Each CLI tarball ships a `.sha256`; desktop updater artifacts are
  minisign-signed with per-platform `latest.json` manifests (see
  [self-upgrade.md](self-upgrade.md)).
- Every macOS artifact (CLI binaries, `.app`, `.dmg`) is Developer ID signed
  and notarized (see [release.md](release.md#macos-signing)).

macOS builds pin `MACOSX_DEPLOYMENT_TARGET=10.15` because llama.cpp requires
`std::filesystem`.

## Desktop App

```bash
./scripts/desktop-build.sh
```

Runs as a menu-bar app with the web UI embedded. macOS deployment target: 10.15+.

## Verifying your build

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
./target/release/syscity --version
```

## Troubleshooting

- **`openssl`/`pkg-config` errors on Linux** — install `libssl-dev` and `pkg-config`.
- **`llama-cpp-2` fails to compile** — needs a C compiler; on Linux install
  `build-essential`. To skip it, build with `--no-default-features` and omit
  `local-embeddings`.
- **Browser tools do nothing** — the `browser` feature requires a Chrome/Chromium
  binary on `PATH`.
- **`pnpm: command not found`** — install pnpm (`npm i -g pnpm`) or build the
  Rust binary alone with `cargo build --release`.
