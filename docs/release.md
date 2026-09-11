# Release Process

How to cut a Syscity release: version bump → changelog → tag → automated build.
The Release workflow (`.github/workflows/release.yml`) triggers on `v*` tags
and publishes CLI tarballs, desktop bundles, and updater manifests. Everything
for macOS is Developer ID signed and notarized; desktop updater payloads are
additionally minisign-signed (see [macOS signing](#macos-signing)).

> **Rule of thumb: write the changelog entry BEFORE tagging.** The tag is the
> point of no return — everything the release says and ships is frozen there.

## Prerequisites (one-time)

**Updater (minisign):**

| Secret | Purpose |
|--------|---------|
| `TAURI_SIGNING_PRIVATE_KEY` | minisign private key that signs desktop updater bundles |

The matching public key lives in `desktop/tauri.conf.json` (`plugins.updater.pubkey`).
The private key's only other copy should be a password manager — **losing it
means existing installs can never auto-update again** (verify fails), and
leaking it lets anyone sign malicious updates.

**Apple (Developer ID):** see [macOS signing](#macos-signing) for what each one
does and how to rotate it.

| Secret | Purpose |
|--------|---------|
| `APPLE_CERTIFICATE` | base64 of a single-line `Certificates.p12` (Developer ID Application + key) |
| `APPLE_CERTIFICATE_PASSWORD` | password for that `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_API_KEY` | App Store Connect **Key ID** (not a path) |
| `APPLE_API_ISSUER` | App Store Connect issuer UUID |
| `APPLE_API_KEY_P8_BASE64` | base64 of `AuthKey_<keyid>.p8` (single line); CI decodes it to a file |

## Steps

### 1. Write the changelog entry

Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (directly under
`## [Unreleased]`). This section becomes the GitHub Release body verbatim —
write it for users, not for the git log. If the section is missing, the
release falls back to auto-generated notes (a bare compare link).

### 2. Bump the version in all four manifests

```bash
perl -pi -e 's/^version = "OLD"$/version = "NEW"/' Cargo.toml desktop/Cargo.toml
perl -pi -e 's/"version": "OLD"/"version": "NEW"/' web/package.json
cargo check --lib   # regenerates Cargo.lock
```

### 3. (Recommended) Run the eval release gate

`Actions → Eval Release Gate → Run workflow` (suite `release_gate`). A red
run means do not tag. See [eval nightly/gate setup](#eval-gates) below.

### 4. Commit, tag, push

```bash
git commit -am "🔧 chore(release): bump version to X.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
export https_proxy=http://127.0.0.1:1087 http_proxy=http://127.0.0.1:1087  # if GitHub is flaky
git push origin main --follow-tags
```

### 5. Watch the build

```bash
gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status
```

Expect ~40 minutes (multi-platform matrix). On success the release has
**26 assets** — verify with `gh release view vX.Y.Z --json assets`.

## What the workflow produces

| Group | Assets |
|-------|--------|
| CLI | `syscity-{linux,macos}-{amd64,arm64}.tar.gz` + `.sha256` |
| Desktop installers | `syscity-desktop-macos-{arm64,amd64}.dmg`, `-windows-amd64.{msi,exe}`, `-linux-amd64.{AppImage,deb}` |
| Signatures | `.sig` for every desktop bundle (minisign) |

On macOS the `.tar.gz` binaries and the `.dmg`/`.app` are also **Developer ID
signed and notarized** (see [macOS signing](#macos-signing)).
| Updater manifests | `syscity-desktop-{darwin,linux,windows}-{aarch64,x86_64}.json` — the per-platform update feeds the desktop apps poll |

The desktop updater endpoint pattern is
`releases/latest/download/syscity-desktop-{{target}}-{{arch}}.json`
(configured in `desktop/tauri.conf.json`); each JSON carries the version, the
payload URL, and the minisign signature of the payload.

## macOS signing

Gatekeeper rejects unsigned downloads ("cannot verify the developer" / "is
damaged"), so every macOS artifact is Developer ID signed and notarized.

- **CLI** (`syscity-macos-{arm64,amd64}.tar.gz`): the `build` job imports the
  `.p12` into a throwaway keychain, runs `codesign --options runtime --timestamp
  --entitlements scripts/entitlements/cli.entitlements`, then notarizes with
  `notarytool`. The tarball ships the **signed** binary.
- **Desktop** (`.dmg`, `.app`): the `build-desktop` job only supplies the
  `APPLE_*` env vars — Tauri creates its own keychain, signs the bundle with the
  hardened runtime, notarizes and **staples** it.
- **Entitlements**: hardened runtime blocks JIT, and both builds ship the
  `plugins` feature (wasmtime/Cranelift), so both entitlement files carry
  `com.apple.security.cs.allow-jit`. The desktop app adds
  `com.apple.security.device.audio-input`.

Rotating the Developer ID certificate: create a new Developer ID Application
cert in the Apple developer portal, export it with its key as a `.p12`, and
update `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` /
`APPLE_SIGNING_IDENTITY`. The current cert expires **2031-09-12**.

## Safety rails in the workflow

The release fails loudly instead of shipping broken:

- **Artifact completeness guard** — the publish job refuses to create a
  release unless all 8 artifact prefixes (4 CLI + 4 desktop) and all 4 updater
  JSONs are present
- **`if-no-files-found: error`** on desktop artifact upload — a collect step
  that finds no bundles fails the job instead of silently shipping a
  CLI-only release
- **Signing is mandatory** — desktop builds fail if the minisign key is
  missing or malformed
- **Notarization is verified, not assumed** — the desktop job runs
  `codesign --verify --deep --strict`, `stapler validate` and `spctl` against
  the built `.app`/`.dmg`; the CLI job fails unless notarytool reports
  `Accepted`. Apple's notary service is a network dependency here.

## Retagging (fixing a botched release)

When a release run fails and the fix lands on main:

```bash
git push origin :refs/tags/vX.Y.Z   # delete remote tag
git tag -d vX.Y.Z                   # delete local tag
git tag -a vX.Y.Z -m "vX.Y.Z"       # re-tag the fix commit
git push origin vX.Y.Z
```

`action-gh-release` merges new assets into the existing release for that tag,
so re-running after a fix **adds** missing assets rather than duplicating.

## Platform gotchas (learned the hard way)

- **macOS deployment target**: `tauri.conf.json → bundle.macOS.minimumSystemVersion`
  (currently `10.15`, required by llama.cpp's `std::filesystem`) *overrides* the
  `MACOSX_DEPLOYMENT_TARGET` env var — set it there, not in the workflow.
- **Updater pubkey format**: tauri wants the base64-wrapped `.pub` file content
  (starts with `dW50cnVzdGVk...`), not the bare `RWS...` minisign string.
- **Workspace layout**: `desktop/` is a workspace member, so bundles land in the
  **workspace-root** `target/<triple>/release/bundle/`, not `desktop/target/`.
- **Tauri v2** does not emit `latest.json`; updater manifests are generated by
  the workflow from tag version + `.sig` content + download URL.
- **CLI binaries cannot be stapled**: `stapler` only handles bundles
  (`.app`/`.dmg`/`.pkg`). A notarized bare Mach-O is validated by Gatekeeper
  **online** on first run, so the very first launch on an offline machine still
  warns. This is the normal trade-off for a `tar.gz` CLI; switching to a `.pkg`
  would allow stapling.
- **`APPLE_API_KEY` is the Key ID, not a path** (Tauri's convention) —
  `APPLE_API_KEY_PATH` is the path to `AuthKey_<keyid>.p8`. electron-builder
  uses the same name for the opposite thing.
- **Notarization is a network call** to Apple on every release run, so a tag
  release needs the runner to reach `appstoreconnect.apple.com`.
- **Windows updater payload** is the NSIS `.exe`, not the `.msi`.

## Eval gates

Two optional-but-recommended quality signals around releases:

- **Nightly** (`eval-nightly.yml`): runs `ci_smoke` against a live LLM daily;
  failures open/update an `eval-nightly` issue, recovery auto-closes it.
  Non-blocking by design.
- **Release gate** (`eval-gate.yml`): manual dispatch running `release_gate`
  (85% pass threshold, 5 trials). Blocking by convention — check it before
  tagging.

Both need `secrets.EVAL_API_KEY` (+ optionally `TAVILY_API_KEY` for search
tasks; DuckDuckGo scraping is blocked on GitHub runner IPs) and the
`vars.EVAL_PROVIDER` / `EVAL_MODEL` / `EVAL_JUDGE_MODEL` / `EVAL_BASE_URL`
variables.
