#!/usr/bin/env bash
#
# One-shot provisioning + verification for the OSWorld × syscity benchmark.
#
# This is the executable version of docs/osworld-kvm-server.md: install the
# native dependencies, Docker, uv, OSWorld and its Python environment, fetch the
# 12 GiB VM image and the released syscity binary, then prove the VM works
# (container self-check + `syscity bench probe`). --smoke goes one task further
# and runs a scored OSWorld task end to end.
#
# Every phase is idempotent: re-running after a failure resumes where it stopped
# and never re-downloads what is already there.
#
# Usage (on the KVM host, as root):
#   sudo ./scripts/osworld-bench-setup.sh              # provision + verify
#   sudo ./scripts/osworld-bench-setup.sh --smoke      # ... and run one task
#   sudo ./scripts/osworld-bench-setup.sh --dry-run    # print the plan, run nothing
#   sudo ./scripts/osworld-bench-setup.sh --verify-only
#   sudo ./scripts/osworld-bench-setup.sh --skip-image --skip-binary
#   sudo ./scripts/osworld-bench-setup.sh --from-source   # build instead of download
#
# Environment:
#   BENCH_ROOT         /opt/bench                  root for OSWorld, binary, results
#   SYSCITY_VERSION    latest                      release to install (e.g. v0.3.16)
#   SYSCITY_BIN_URL    (unset)                     full tarball URL (mirror, file://)
#   SYSCITY_SRC_URL    (unset)                     source archive URL for bench/ glue
#   RELEASE_REPO       lightconsen/syscity         GitHub repo the release lives in
#   SYS_SRC            (unset)                     syscity tree to copy into $SYS
#   HF_TOKEN_FILE      /root/.hf_token             Hugging Face token for the image
#   PROVIDER_KEY_FILE  /root/.deepseek_key         LLM key (needed by --smoke)
#   PROVIDER           deepseek                    syscity provider preset
#   MODEL              deepseek-v4-flash           model name
#   PROVIDER_BASE_URL  https://api.deepseek.com/v1 OpenAI-compatible base URL
#   SCREEN_WIDTH/HEIGHT  1920/1080                 must match the VM's resolution
#   MAX_STEPS          60                          tool budget per task
#
# Exit code:
#   0 — every requested phase passed
#   1 — a phase failed (the message names it); re-run to resume
#
# The day-to-day path needs no Rust toolchain: it installs the release tarball
# (checksum-verified) into $BIN. --from-source builds the tree at $SYS instead,
# for a checkout that is newer than the last release; that path needs Rust and
# the libclang/GUI development packages.

set -euo pipefail

RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[1;33m'
BLUE=$'\033[0;34m'
RESET=$'\033[0m'

BENCH_ROOT="${BENCH_ROOT:-/opt/bench}"
SYS="${SYS:-$BENCH_ROOT/syscity}"
OSW="${OSW:-$BENCH_ROOT/OSWorld}"
BIN="${BIN:-$BENCH_ROOT/bin/syscity}"
RUNS="${RUNS:-$BENCH_ROOT/results}"
RELEASE_REPO="${RELEASE_REPO:-lightconsen/syscity}"
SYSCITY_VERSION="${SYSCITY_VERSION:-latest}"
RELEASE_ASSET="syscity-linux-amd64.tar.gz"
SYSCITY_TAG=""
QCOW2="$OSW/docker_vm_data/Ubuntu.qcow2"
IMG_ZIP="$OSW/docker_vm_data/Ubuntu.qcow2.zip"
IMG_URL="https://huggingface.co/datasets/xlangai/ubuntu_osworld/resolve/main/Ubuntu.qcow2.zip"
OSW_REPO="${OSW_REPO:-https://github.com/xlang-ai/OSWorld}"
OSW_DOCKER_IMAGE="happysixd/osworld-docker"

HF_TOKEN_FILE="${HF_TOKEN_FILE:-/root/.hf_token}"
PROVIDER_KEY_FILE="${PROVIDER_KEY_FILE:-/root/.deepseek_key}"
PROVIDER="${PROVIDER:-deepseek}"
MODEL="${MODEL:-deepseek-v4-flash}"
PROVIDER_BASE_URL="${PROVIDER_BASE_URL:-https://api.deepseek.com/v1}"
SCREEN_WIDTH="${SCREEN_WIDTH:-1920}"
SCREEN_HEIGHT="${SCREEN_HEIGHT:-1080}"
MAX_STEPS="${MAX_STEPS:-60}"

DRY_RUN=false
SMOKE=false
VERIFY_ONLY=false
SKIP_DEPS=false
SKIP_IMAGE=false
SKIP_BINARY=false
SKIP_VERIFY=false
SKIP_PROBE=false
REFETCH=false
FROM_SOURCE=false

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --smoke) SMOKE=true ;;
        --verify-only) VERIFY_ONLY=true ;;
        --skip-deps) SKIP_DEPS=true ;;
        --skip-image) SKIP_IMAGE=true ;;
        # --skip-build/--rebuild are the pre-download names, kept so a command
        # line copied from an older runbook still means what it says.
        --skip-binary|--skip-build) SKIP_BINARY=true ;;
        --skip-verify) SKIP_VERIFY=true ;;
        --skip-probe) SKIP_PROBE=true ;;
        --refetch|--rebuild) REFETCH=true ;;
        --from-source) FROM_SOURCE=true ;;
        -h|--help)
            awk 'NR > 1 && /^#/ {sub(/^# ?/, ""); print; next} NR > 1 {exit}' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg (try --help)" >&2
            exit 1
            ;;
    esac
done

log() { printf '%s==>%s %s\n' "$BLUE" "$RESET" "$*"; }
ok() { printf '%s ok %s %s\n' "$GREEN" "$RESET" "$*"; }
skip() { printf '%s -- %s %s (already done)\n' "$YELLOW" "$RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
    printf '%sfail%s %s\n' "$RED" "$RESET" "$*" >&2
    exit 1
}
# die, unless this is a dry run -- a dry run should be executable anywhere,
# including a laptop that has no /dev/kvm.
must() {
    if $DRY_RUN; then
        warn "$1 (ignored: --dry-run)"
    else
        die "$1"
    fi
}
have() { command -v "$1" >/dev/null 2>&1; }
# Run a command, or print it under --dry-run.
run() {
    if $DRY_RUN; then
        printf '   + %s\n' "$*"
        return 0
    fi
    "$@"
}
plan() { printf '   + %s\n' "$*"; }

HOLD_PID=""
DL_TMP=""
cleanup() {
    if [ -n "$HOLD_PID" ]; then
        kill "$HOLD_PID" 2>/dev/null || true
        wait "$HOLD_PID" 2>/dev/null || true
    fi
    if [ -n "$DL_TMP" ]; then
        rm -rf "$DL_TMP"
    fi
}
trap cleanup EXIT

# ── 0. preflight ────────────────────────────────────────────────────────────
# Cheap checks that make every later phase pointless if they fail. A cloud host
# without nested virtualisation looks fine until QEMU falls back to TCG, so the
# /dev/kvm check is the one not to soften.
preflight() {
    log "preflight"
    if $DRY_RUN; then
        # Readable on any machine, including the laptop this is planned on.
        plan "root, x86_64, vmx/svm in /proc/cpuinfo, /dev/kvm rw, >=16 GB RAM, >=50 GB free"
        return 0
    fi

    if [ "$(uname -m)" != "x86_64" ]; then
        die "x86_64 required (got $(uname -m)); the OSWorld image is x86_64"
    fi

    local flags
    flags=$(grep -cE '(vmx|svm)' /proc/cpuinfo 2>/dev/null || true)
    if [ "${flags:-0}" -eq 0 ]; then
        die "no vmx/svm in /proc/cpuinfo: no hardware virtualisation"
    fi

    if [ ! -e /dev/kvm ]; then
        die "no /dev/kvm: enable nested virtualisation or move to bare metal"
    elif [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
        die "/dev/kvm exists but is not readable/writable"
    fi

    local mem_gb
    mem_gb=$(awk '/MemTotal/ {print int($2 / 1024 / 1024)}' /proc/meminfo)
    if [ "$mem_gb" -lt 15 ]; then
        die "need >= 16 GB RAM for the 4 GB VM plus the host (have ${mem_gb} GB)"
    fi

    local free_gb
    free_gb=$(df -Pk / | awk 'NR == 2 {print int($4 / 1024 / 1024)}')
    if [ "$free_gb" -lt 50 ]; then
        die "need >= 50 GB free on / (have ${free_gb} GB)"
    elif [ "$free_gb" -lt 60 ]; then
        warn "60+ GB is safer: image + zip + container disk + recordings"
    fi

    ok "x86_64, vmx/svm, /dev/kvm, ${mem_gb} GB RAM, ${free_gb} GB free"
}

# dpkg's frontend lock is held by unattended-upgrade on a fresh cloud host -- we
# have watched it spin for 11 minutes on this exact box. Wait a little, then
# stop the offender rather than letting apt fail.
wait_for_dpkg() {
    have fuser || return 0   # psmisc; without it apt reports the lock itself
    local waited=0
    while fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1; do
        if [ "$waited" -ge 120 ]; then
            warn "dpkg lock held for ${waited}s; stopping unattended-upgrade"
            pkill -f unattended-upgrade 2>/dev/null || true
            sleep 3
            if fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1; then
                rm -f /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock /var/cache/apt/archives/lock
                run dpkg --configure -a || warn "dpkg --configure -a failed; apt may complain"
            fi
            return 0
        fi
        sleep 5
        waited=$((waited + 5))
    done
}

apt_install() {
    wait_for_dpkg
    run env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@"
}

# ── 1. native dependencies ──────────────────────────────────────────────────
# Every package here is a build-script dependency of the release binary:
# openssl-sys, alsa-sys, the X11/atspi bindings, and llama.cpp's CMake+bindgen.
install_deps() {
    log "native dependencies"
    if $SKIP_DEPS; then
        skip "native dependencies (--skip-deps)"
        return 0
    fi
    if $DRY_RUN; then
        plan "apt-get update && apt-get install -y $(deps_packages | tr '\n' ' ')"
        return 0
    fi
    wait_for_dpkg
    apt-get update -qq
    # shellcheck disable=SC2046
    apt_install $(deps_packages)
    ok "apt dependencies"
}

deps_packages() {
    printf '%s\n' \
        build-essential cmake pkg-config git curl unzip ca-certificates psmisc \
        libssl-dev libclang-dev clang libcurl4-openssl-dev \
        libasound2-dev libx11-dev libxdo-dev libxcb1-dev libxrandr-dev libxi-dev \
        libxtst-dev libatspi2.0-dev libdbus-1-dev
}

# libclang is what bindgen needs (llama-cpp-sys-2 panics without it) and it is
# not on the default linker search path on Ubuntu.
find_libclang_dir() {
    local dir
    for dir in /usr/lib/llvm-*/lib /usr/lib/x86_64-linux-gnu; do
        if compgen -G "$dir/libclang.so*" >/dev/null; then
            printf '%s' "$dir"
            return 0
        fi
    done
    return 1
}

# ── 2. Rust ─────────────────────────────────────────────────────────────────
source_cargo_env() {
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
}

install_rust() {
    log "Rust"
    if $DRY_RUN; then
        plan "curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal"
        return 0
    fi
    source_cargo_env
    if have cargo; then
        skip "cargo $(cargo -V | awk '{print $2}')"
        return 0
    fi
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    source_cargo_env
    have cargo || die "cargo still missing after rustup"
    ok "cargo $(cargo -V | awk '{print $2}')"
}

# ── 3. Docker ───────────────────────────────────────────────────────────────
install_docker() {
    log "Docker"
    if $DRY_RUN; then
        plan "apt-get install -y docker.io && systemctl enable --now docker"
        plan "docker pull $OSW_DOCKER_IMAGE"
        return 0
    fi
    if ! have docker; then
        apt_install docker.io
    fi
    if ! docker info >/dev/null 2>&1; then
        systemctl enable --now docker >/dev/null 2>&1 || true
        local waited=0
        while ! docker info >/dev/null 2>&1 && [ "$waited" -lt 30 ]; do
            sleep 1
            waited=$((waited + 1))
        done
    fi
    docker info >/dev/null 2>&1 || die "dockerd is not answering"
    if docker image inspect "$OSW_DOCKER_IMAGE" >/dev/null 2>&1; then
        skip "$OSW_DOCKER_IMAGE image"
    else
        docker pull "$OSW_DOCKER_IMAGE"
    fi
    ok "docker $(docker version --format '{{.Server.Version}}')"
}

# ── 4. uv ───────────────────────────────────────────────────────────────────
install_uv() {
    log "uv"
    export PATH="$HOME/.local/bin:$PATH"
    if $DRY_RUN; then
        plan "curl -LsSf https://astral.sh/uv/install.sh | sh"
        return 0
    fi
    if have uv; then
        skip "uv $(uv --version | awk '{print $2}')"
        return 0
    fi
    curl -LsSf https://astral.sh/uv/install.sh | sh
    have uv || die "uv still missing after install"
    ok "uv $(uv --version | awk '{print $2}')"
}

# ── 5. OSWorld checkout + Python environment ────────────────────────────────
# Upstream's pyproject lists an editable path dependency (agp-client) whose
# directory has no packaging metadata, so `uv pip install -e .` dies while
# resolving. Drop that one dependency and its [tool.uv.sources] entry.
patch_pyproject() {
    if $DRY_RUN; then
        plan "strip agp-client from $OSW/pyproject.toml"
        return 0
    fi
    OSW="$OSW" python3 - <<'PY'
import os
import pathlib

path = pathlib.Path(os.environ["OSW"]) / "pyproject.toml"
text = path.read_text()
lines = text.splitlines(keepends=True)
out = []
i = 0
while i < len(lines):
    stripped = lines[i].strip()
    if stripped == "[tool.uv.sources]":
        # Keep the table only if it still has entries after dropping agp-client.
        body = []
        j = i + 1
        while j < len(lines) and not lines[j].lstrip().startswith("["):
            body.append(lines[j])
            j += 1
        kept = [line for line in body if not line.strip().startswith("agp-client")]
        if kept:
            out.append(lines[i])
            out.extend(kept)
        i = j
        continue
    if stripped in ('"agp-client",', "'agp-client',"):
        i += 1
        continue
    out.append(lines[i])
    i += 1

patched = "".join(out)
if patched == text:
    print("   pyproject.toml already patched")
else:
    backup = path.with_name("pyproject.toml.orig")
    if not backup.exists():
        backup.write_text(text)
    path.write_text(patched)
    print("   patched pyproject.toml (agp-client removed, backup kept)")
PY
}

setup_osworld() {
    log "OSWorld checkout"
    if [ -n "${SYS_SRC:-}" ]; then
        [ -f "$SYS_SRC/Cargo.toml" ] || die "SYS_SRC=$SYS_SRC is not a syscity tree"
        run rsync -a --delete --exclude=target --exclude=node_modules --exclude=.git \
            "$SYS_SRC/" "$SYS/"
    fi

    if [ -f "$OSW/desktop_env/desktop_env.py" ]; then
        skip "OSWorld at $OSW"
    else
        run mkdir -p "$(dirname "$OSW")"
        run git clone --depth 1 "$OSW_REPO" "$OSW"
    fi

    patch_pyproject

    if [ -x "$OSW/.venv/bin/python" ]; then
        skip ".venv ($("$OSW/.venv/bin/python" -V 2>/dev/null || echo 'broken'))"
    else
        run env -C "$OSW" uv venv --python 3.12
    fi

    if ! $DRY_RUN && "$OSW/.venv/bin/python" -c 'import desktop_env' >/dev/null 2>&1; then
        skip "OSWorld python package"
    else
        run env -C "$OSW" uv pip install -e .
    fi
    ok "OSWorld at $OSW"
}

# ── 6. VM image ─────────────────────────────────────────────────────────────
fetch_image() {
    log "VM image"
    if $SKIP_IMAGE; then
        skip "VM image (--skip-image)"
        return 0
    fi
    if [ -f "$QCOW2" ] && [ "$(stat -c %s "$QCOW2")" -gt 8000000000 ]; then
        skip "$QCOW2 ($(du -h "$QCOW2" | cut -f1))"
        return 0
    fi
    if $DRY_RUN; then
        plan "curl -fL -C - $IMG_URL -> $IMG_ZIP"
        plan "unzip $IMG_ZIP -d $(dirname "$QCOW2")"
        return 0
    fi
    [ -f "$HF_TOKEN_FILE" ] || die "no HF token at $HF_TOKEN_FILE (needed to download the image)"
    mkdir -p "$(dirname "$QCOW2")"
    log "downloading ~12 GiB (resumable; Ctrl-C and re-run to continue)"
    curl -fL --retry 3 --retry-delay 5 -C - \
        -H "Authorization: Bearer $(cat "$HF_TOKEN_FILE")" \
        -o "$IMG_ZIP" "$IMG_URL"
    unzip -o "$IMG_ZIP" -d "$(dirname "$QCOW2")" >/dev/null
    rm -f "$IMG_ZIP"
    [ -f "$QCOW2" ] || die "unzip did not produce $QCOW2"
    ok "$QCOW2 ($(du -h "$QCOW2" | cut -f1))"
}

# ── 7. syscity binary ───────────────────────────────────────────────────────
# The released CLI, not a local build: release.yml builds the x86_64 artifact on
# an Ubuntu 22.04 runner precisely so its glibc floor matches this host, and
# downloading it spares the machine a Rust toolchain and a ten-minute compile.
# --from-source is for a checkout newer than the last release.
release_tarball_url() {
    if [ -n "${SYSCITY_BIN_URL:-}" ]; then
        printf '%s' "$SYSCITY_BIN_URL"
    elif [ "$SYSCITY_VERSION" = "latest" ]; then
        printf 'https://github.com/%s/releases/latest/download/%s' \
            "$RELEASE_REPO" "$RELEASE_ASSET"
    else
        printf 'https://github.com/%s/releases/download/%s/%s' \
            "$RELEASE_REPO" "$SYSCITY_VERSION" "$RELEASE_ASSET"
    fi
}

# Both installers land at $BIN, so every later phase has one path to use.
fetch_binary() {
    if $FROM_SOURCE; then
        log "syscity binary (from source)"
    else
        log "syscity binary ($SYSCITY_VERSION)"
    fi
    if $SKIP_BINARY; then
        skip "syscity binary (--skip-binary)"
        [ -x "$BIN" ] || must "--skip-binary but $BIN does not exist"
        return 0
    fi
    if $FROM_SOURCE; then
        build_syscity
        return 0
    fi
    if [ "$SYSCITY_VERSION" != "latest" ]; then
        SYSCITY_TAG="$SYSCITY_VERSION"
    fi
    if [ -x "$BIN" ] && ! $REFETCH; then
        skip "$BIN ($(du -h "$BIN" | cut -f1)); use --refetch to replace"
        return 0
    fi

    local url
    url=$(release_tarball_url)
    if $DRY_RUN; then
        plan "curl -fL $url -> $RELEASE_ASSET"
        plan "curl -fL $url.sha256, then sha256sum -c $RELEASE_ASSET.sha256"
        plan "tar xzf $RELEASE_ASSET syscity -C $(dirname "$BIN")"
        return 0
    fi

    mkdir -p "$(dirname "$BIN")"
    DL_TMP=$(mktemp -d)
    log "downloading $url"
    curl -fL --retry 3 --retry-delay 5 -o "$DL_TMP/$RELEASE_ASSET" "$url"
    curl -fL --retry 3 --retry-delay 5 -o "$DL_TMP/$RELEASE_ASSET.sha256" "$url.sha256"
    # The publisher's checksum, not ours: a truncated or substituted tarball is
    # caught here rather than by a segfault four phases later.
    (cd "$DL_TMP" && sha256sum -c "$RELEASE_ASSET.sha256" >/dev/null) ||
        die "checksum mismatch for $RELEASE_ASSET ($(cat "$DL_TMP/$RELEASE_ASSET.sha256" 2>/dev/null))"
    tar xzf "$DL_TMP/$RELEASE_ASSET" -C "$(dirname "$BIN")" syscity
    chmod +x "$BIN"
    rm -rf "$DL_TMP"
    DL_TMP=""

    local version
    if ! version=$("$BIN" --version 2>&1); then
        die "$BIN does not run on this host: $version
     (the release needs x86_64 Linux with glibc >= 2.35)"
    fi
    SYSCITY_TAG="v${version##* }"   # "syscity 0.3.16" -> v0.3.16
    ok "$BIN ($version)"
}

# ── 7b. harness glue ────────────────────────────────────────────────────────
# `bench/osworld/run.py` is Python from the repository, not a file in the
# release tarball, and a machine that installed only the binary has no checkout.
# Take bench/ out of the source archive of the same tag, so the glue and the
# binary always speak the same stdio protocol.
# Same mirror escape hatch as the binary: SYSCITY_SRC_URL replaces the archive.
release_source_url() {
    if [ -n "${SYSCITY_SRC_URL:-}" ]; then
        printf '%s' "$SYSCITY_SRC_URL"
    else
        printf 'https://github.com/%s/archive/refs/tags/%s.tar.gz' "$RELEASE_REPO" "$1"
    fi
}

fetch_glue() {
    log "OSWorld glue"
    if [ -f "$SYS/bench/osworld/run.py" ]; then
        skip "bench/osworld at $SYS"
        return 0
    fi
    if $DRY_RUN; then
        local shown_tag="${SYSCITY_TAG:-$SYSCITY_VERSION}"
        [ "$shown_tag" = "latest" ] && shown_tag="<tag>"
        plan "curl -fL $(release_source_url "$shown_tag"), copy bench/ -> $SYS/bench"
        return 0
    fi

    local tag="$SYSCITY_TAG"
    if [ -z "$tag" ]; then
        local version
        version=$("$BIN" --version 2>/dev/null) || die "cannot tell which release $BIN is; set SYSCITY_VERSION"
        tag="v${version##* }"
    fi
    local url
    url=$(release_source_url "$tag")
    DL_TMP=$(mktemp -d)
    curl -fL --retry 3 --retry-delay 5 -o "$DL_TMP/src.tar.gz" "$url" ||
        die "no source archive for $tag at $url (set SYSCITY_VERSION to the release $BIN came from)"
    mkdir -p "$DL_TMP/src" "$SYS"
    tar xzf "$DL_TMP/src.tar.gz" -C "$DL_TMP/src"
    cp -a "$DL_TMP/src/"*/bench "$SYS/"
    rm -rf "$DL_TMP"
    DL_TMP=""
    [ -f "$SYS/bench/osworld/run.py" ] || die "the $tag archive has no bench/osworld/run.py"
    ok "$SYS/bench (glue from $tag)"
}

build_syscity() {
    log "building syscity from source ($SYS)"
    if [ -x "$BIN" ] && ! $REFETCH; then
        skip "$BIN ($(du -h "$BIN" | cut -f1)); use --refetch to rebuild"
        return 0
    fi
    if $DRY_RUN; then
        plan "cargo build --release --bin syscity (cwd $SYS, LIBCLANG_PATH from find_libclang_dir)"
        plan "install $SYS/target/release/syscity -> $BIN"
        return 0
    fi
    if [ ! -f "$SYS/Cargo.toml" ]; then
        die "no syscity source at $SYS (rsync the tree or set SYS_SRC); \
drop --from-source to install the release instead"
    fi
    local libclang
    if libclang=$(find_libclang_dir); then
        export LIBCLANG_PATH="$libclang"
        log "LIBCLANG_PATH=$LIBCLANG_PATH"
    else
        die "libclang not found; install libclang-dev (bindgen needs it)"
    fi
    source_cargo_env
    log "building (first build ~10 min on 4 vCPU)"
    (cd "$SYS" && cargo build --release --bin syscity)
    [ -x "$SYS/target/release/syscity" ] || die "build finished without $SYS/target/release/syscity"
    mkdir -p "$(dirname "$BIN")"
    install -m 0755 "$SYS/target/release/syscity" "$BIN"
    ok "$BIN (built from $SYS)"
}

# ── 8. VM self-check ────────────────────────────────────────────────────────
# Starts a container, boots QEMU, and checks guest_execute, screenshot bytes,
# noVNC and guest HTTPS -- including the "engine rejects cap_add" fallback.
verify_vm() {
    log "VM self-check"
    if $SKIP_VERIFY; then
        skip "VM self-check (--skip-verify)"
        return 0
    fi
    local script="$OSW/scripts/validate_docker_capabilities_runtime.py"
    if $DRY_RUN; then
        plan "(cd $OSW && .venv/bin/python scripts/validate_docker_capabilities_runtime.py)"
        return 0
    fi
    [ -f "$script" ] || die "missing $script (is $OSW an OSWorld checkout?)"
    local log_file="$BENCH_ROOT/validate-docker.log"
    mkdir -p "$BENCH_ROOT"
    if ! (cd "$OSW" && .venv/bin/python "$script") >"$log_file" 2>&1; then
        tail -25 "$log_file" >&2
        die "VM self-check failed (full log: $log_file)"
    fi
    if ! grep -q '"all_passed": true' "$log_file"; then
        tail -25 "$log_file" >&2
        die "VM self-check did not report all_passed (full log: $log_file)"
    fi
    if grep -q '"kvm_exists": false' "$log_file"; then
        die "/dev/kvm is invisible to containers; QEMU would run under TCG"
    fi
    ok "container + VM boot, screenshot, noVNC, guest HTTPS"
}

# ── 9. probe ────────────────────────────────────────────────────────────────
# Holds a VM up, runs `bench probe` against its allocated port, tears both down.
# The port is dynamic (5000 unless taken), which is why it is read from the env
# object rather than assumed.
probe_desktop() {
    log "bench probe"
    if $SKIP_PROBE; then
        skip "bench probe (--skip-probe)"
        return 0
    fi
    if $DRY_RUN; then
        plan "hold the VM, then: $BIN bench probe --computer-url http://127.0.0.1:\$PORT --execute"
        return 0
    fi
    [ -x "$BIN" ] || die "no binary at $BIN"

    local holder="$BENCH_ROOT/hold_vm.py"
    local port_file="$BENCH_ROOT/hold_vm.port"
    local log_file="$BENCH_ROOT/hold_vm.log"
    mkdir -p "$BENCH_ROOT"
    rm -f "$port_file"

    cat >"$holder" <<PY
"""Start the OSWorld VM and keep it up until SIGTERM (written by the setup script)."""
import pathlib
import signal
import sys
import time

from desktop_env.desktop_env import DesktopEnv

port_file = pathlib.Path(sys.argv[1])
env = DesktopEnv(
    provider_name="docker",
    path_to_vm="$QCOW2",
    action_space="pyautogui",
    os_type="Ubuntu",
    headless=True,
    screen_size=($SCREEN_WIDTH, $SCREEN_HEIGHT),
    require_a11y_tree=False,
)
port_file.write_text(str(env.server_port))
print("PORT", env.server_port, flush=True)

stop = False


def _stop(*_unused):
    global stop
    stop = True


signal.signal(signal.SIGTERM, _stop)
signal.signal(signal.SIGINT, _stop)
while not stop:
    time.sleep(1)
env.close()
PY

    # env -C (not a subshell) so $! is the interpreter itself and the signal
    # reaches env.close().
    env -C "$OSW" .venv/bin/python "$holder" "$port_file" >"$log_file" 2>&1 &
    HOLD_PID=$!

    local waited=0
    while [ ! -s "$port_file" ] && [ "$waited" -lt 600 ]; do
        if ! kill -0 "$HOLD_PID" 2>/dev/null; then
            tail -25 "$log_file" >&2
            die "the VM holder died before reporting a port"
        fi
        sleep 3
        waited=$((waited + 3))
    done
    local port
    port=$(cat "$port_file" 2>/dev/null || true)
    [ -n "$port" ] || die "no port after ${waited}s (log: $log_file)"
    log "VM service on port $port"

    local probe_ok=true
    if ! "$BIN" bench probe --computer-url "http://127.0.0.1:$port" --execute; then
        probe_ok=false
    fi

    kill -TERM "$HOLD_PID" 2>/dev/null || true
    wait "$HOLD_PID" 2>/dev/null || true
    HOLD_PID=""
    # env.close() may not have run if the holder was killed mid-boot; only the
    # benchmark image's containers are touched.
    docker ps -aq --filter "ancestor=$OSW_DOCKER_IMAGE" 2>/dev/null |
        xargs -r docker rm -f >/dev/null 2>&1 || true

    $probe_ok || die "bench probe reported a failing endpoint (see above)"
    ok "all four endpoints answered"
}

# ── 10. one scored task ─────────────────────────────────────────────────────
smoke() {
    log "smoke: one OSWorld task"
    if ! $SMOKE; then
        return 0
    fi
    if $DRY_RUN; then
        plan "(cd $OSW && .venv/bin/python $SYS/bench/osworld/run.py --mode step --split test_small --ids <first id>)"
        return 0
    fi
    [ -f "$PROVIDER_KEY_FILE" ] || die "no LLM key at $PROVIDER_KEY_FILE (needed by --smoke)"

    local result_dir
    result_dir="$RUNS/smoke-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$result_dir"
    local task_id
    task_id=$(cd "$OSW" && .venv/bin/python - <<'PY'
import json

with open("evaluation_examples/test_small.json", encoding="utf-8") as handle:
    meta = json.load(handle)
print(next(iter(meta.values()))[0])
PY
)
    log "task $task_id -> $result_dir"
    (
        cd "$OSW" &&
            OPENAI_API_KEY="$(cat "$PROVIDER_KEY_FILE")" \
                SYSCITY_BASE_URL="$PROVIDER_BASE_URL" \
                .venv/bin/python "$SYS/bench/osworld/run.py" \
                --osworld-root . \
                --mode step \
                --headless \
                --provider-name docker \
                --path-to-vm "$QCOW2" \
                --split test_small \
                --ids "$task_id" \
                --max-steps "$MAX_STEPS" \
                --syscity "$BIN" \
                --provider "$PROVIDER" \
                --model "$MODEL" \
                --result-dir "$result_dir"
    )
    ok "summary: $result_dir/summary/syscity.json"
}

# ── main ────────────────────────────────────────────────────────────────────
main() {
    log "bench root: $BENCH_ROOT (syscity: $SYS, OSWorld: $OSW, results: $RUNS)"
    if $DRY_RUN; then
        log "dry run: nothing below actually executes"
    fi

    preflight

    if ! $VERIFY_ONLY; then
        install_deps
        if $FROM_SOURCE; then
            install_rust   # only the source build needs cargo
        fi
        install_docker
        install_uv
        setup_osworld
        fetch_image
        fetch_binary
        fetch_glue
    fi

    verify_vm
    probe_desktop
    smoke

    echo ""
    ok "benchmark host ready"
    cat <<EOF

Next:
  # one scored task (add --smoke to this script instead)
  cd $OSW && OPENAI_API_KEY=\$(cat $PROVIDER_KEY_FILE) SYSCITY_BASE_URL=$PROVIDER_BASE_URL \\
    .venv/bin/python $SYS/bench/osworld/run.py \\
      --osworld-root . --mode step --headless \\
      --provider-name docker --path-to-vm $QCOW2 \\
      --split test_small --max-steps $MAX_STEPS --timeout 1800 \\
      --syscity $BIN --provider $PROVIDER --model $MODEL \\
      --result-dir $RUNS/test_small-\$(date +%F)

  # test_all (369 tasks): see docs/osworld-kvm-server.md §4.3

The provider key reaches syscity only through the environment (or
~/.syscity/config.toml); the harness never passes --api-key.
EOF
}

# Sourced (rather than executed) by tests: expose the phase functions without
# running them. The only caller that sources this file is a check harness.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
