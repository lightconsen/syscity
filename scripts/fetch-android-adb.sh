#!/usr/bin/env bash
#
# Fetch a runnable `adb` client for the Syscity Android app (mobile-migration
# §4.5 "loopback-ADB self-pairing").
#
# On Android there is no adb *client* on the phone — only the adbd *server*
# (wireless debugging). To self-pair over loopback the app needs a client
# binary bundled in the APK. Termux's android-tools ships one built against
# bionic (interpreter /system/bin/linker64), so it can exec from the app's
# nativeLibraryDir via AndroidShellRunner — but it is dynamically linked, so
# its runtime libraries must travel with it.
#
# This script downloads the Termux aarch64 packages, verifies every download
# against the sha256 recorded in Termux's signed package index (packages.termux.dev),
# and installs the adb client + its dependency closure into
#   desktop/gen/android/app/src/main/jniLibs/arm64-v8a/
#
# Nothing is installed unless every sha256 matches (all-or-nothing).
#
# Usage:
#   ./scripts/fetch-android-adb.sh            # fetch + verify + install (aarch64)
#   ./scripts/fetch-android-adb.sh --verify   # check the installed closure only
#   ./scripts/fetch-android-adb.sh --cache <dir>   # override the download cache
#
# Notes:
#   - aarch64/arm64-v8a only (the default APK ABI). x86_64 emulators are not
#     covered — on those the tools degrade gracefully (`has_adb()` is false).
#   - The bundle keeps the app's own libc++_shared.so (NDK build) untouched;
#     adb resolves `libc++_shared.so` from nativeLibraryDir at exec time.
#   - Re-run the script to refresh / self-heal. Deleting jniLibs/arm64-v8a/adb
#     reverts to "not available" without touching any other behaviour.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── pinned package manifest ────────────────────────────────────────────────
# name | repo path under packages.termux.dev/apt/termux-main | sha256
# (sha256 values come from the Termux stable repo Packages index, which is
#  cryptographically signed by the Termux maintainers.)
BASE_URL="https://packages.termux.dev/apt/termux-main"
PACKAGES=(
  # android-tools 37 also needs pcre2 and liblzma (via pcre2), which 36 did
  # not: the closure below is the transitive Depends of android-tools, taken
  # from the same index the hashes come from.
  "android-tools|pool/main/a/android-tools/android-tools_37.0.0p1-3_aarch64.deb|534e90d338bd432c06677cb402d0e9ed98a82a74de4bf5f90e643f15d8fc4274"
  "libprotobuf|pool/main/libp/libprotobuf/libprotobuf_2:36.2-1_aarch64.deb|daa3b9169e094644ba95c35d40a5cc283473144decc7dae8396c49cc51440815"
  "abseil-cpp|pool/main/a/abseil-cpp/abseil-cpp_20260817.0_aarch64.deb|b698963bb7096494ad928ce89cd869a53e6adf0b05f5f407b40fda9c85c6e2b0"
  "zlib|pool/main/z/zlib/zlib_1.3.2_aarch64.deb|75e7d0af17fcc3b40004309fdc00a1ddb9ae08346dce5e269902c34ac3966ac9"
  "brotli|pool/main/b/brotli/brotli_1.2.1_aarch64.deb|4fba29438b2b44f8470d52173b4548e938542dc2fc632f8699be5a284cbb9485"
  "liblz4|pool/main/libl/liblz4/liblz4_1.10.0-1_aarch64.deb|09b9449418d5c2dc4f5c1c140ba8138d56be3e9ae5fd3be3318825ec9f8a0499"
  "zstd|pool/main/z/zstd/zstd_1.5.7-1_aarch64.deb|e1b4a5113648da8de189620ba1fce74c48b2d0833d9043391b9a1c91fb606fd3"
  "libc++|pool/main/libc/libc++/libc++_30_aarch64.deb|53d0b84a7ba7459024257cb94d5b136fe13ef858567f65a8064b35950799f2ca"
  "pcre2|pool/main/p/pcre2/pcre2_10.49_aarch64.deb|c27995e5f52b8ecc9b8b3e662eb08e49f82a18bfd3f34343936acc7b23964724"
  "liblzma|pool/main/libl/liblzma/liblzma_5.8.4_aarch64.deb|44e95e6e60dddb3705e60a344a4f009a1085a210797342b721eaff41b44033c0"
  "fmt|pool/main/f/fmt/fmt_1:12.2.0_aarch64.deb|4a2cb43ab167b32daf0d7978f21c5391d17bce47dae6aa2a8b3a8f958e663ce7"
)

JNI_LIB_DIR="${SYSCITY_JNI_DIR:-$REPO_ROOT/desktop/gen/android/app/src/main/jniLibs/arm64-v8a}"

# Two of these ship under a plain `.so` name even though their SONAME keeps the
# version (`libz.so.1`, `libzstd.so.1`): Android's APK packaging only extracts
# files whose name ends in `.so`, so a file called `libz.so.1` never reaches the
# device. The app preloads them by path (see `apply_bundled_library_path`),
# which registers them under the SONAME the executable asks for.
#
# Files to install, mapped from the extracted package layout. "->" rewrites a
# versioned file to an unversioned SONAME as a real copy (Android APK native
# lib extraction does not reliably preserve symlinks).
INSTALL_RULES=(
  # `adb` is installed as `libadb.so`: Android's APK packaging keeps only
  # `*.so` in jniLibs, and silently drops anything else — so a file called
  # `adb` never reaches the device. The executable is an ordinary ELF either
  # way; `AndroidShellRunner` resolves the name.
  "android-tools|data/data/com.termux/files/usr/bin/adb|libadb.so"
  "libprotobuf|data/data/com.termux/files/usr/lib/libprotobuf.so|libprotobuf.so"
  "libprotobuf|data/data/com.termux/files/usr/lib/libutf8_validity.so|libutf8_validity.so"
  "zlib|data/data/com.termux/files/usr/lib/libz.so.1.3.2|libz.so"
  "brotli|data/data/com.termux/files/usr/lib/libbrotlicommon.so|libbrotlicommon.so"
  "brotli|data/data/com.termux/files/usr/lib/libbrotlidec.so|libbrotlidec.so"
  "brotli|data/data/com.termux/files/usr/lib/libbrotlienc.so|libbrotlienc.so"
  "liblz4|data/data/com.termux/files/usr/lib/liblz4.so|liblz4.so"
  "zstd|data/data/com.termux/files/usr/lib/libzstd.so.1.5.7|libzstd.so"
  "fmt|data/data/com.termux/files/usr/lib/libfmt.so|libfmt.so"
  "libc++|data/data/com.termux/files/usr/lib/libc++_shared.so|libc++_shared.so"
)
# NOTE: `libc++_shared.so` IS installed, from Termux's package.
#
# This used to be skipped on the assumption that the app shipped its own NDK
# copy, which adb's `NEEDED libc++_shared.so` would resolve. It does not: the
# Rust build links libc++ statically, so nothing named `libc++_shared.so`
# existed, and adb failed to start with "library libc++_shared.so not found".
# The file is only reachable through the bundled-exec path, so it cannot affect
# the app's own code.

# libprotobuf's abseil dependency closure: every libabsl_*.so it links.
# Collected with llvm-readelf -d on libprotobuf.so (see fetch script header).
# Every libabsl_*.so the abseil package ships, rather than a hand-kept list.
#
# The list used to be read off `libprotobuf.so` with llvm-readelf, which was
# true until abseil and android-tools moved a major version independently:
# `adb` then failed at startup with `library "libabsl_vlog_config_internal.so"
# not found`. The package is the authority on what it provides.
ABSEIL_GLOB="libabsl_*.so"

# The libraries AndroidShellRunner must make discoverable via LD_LIBRARY_PATH
# when exec'ing the bundled adb (its RUNPATH points at a Termux path that does
# not exist on a stock device). The app's own libc++_shared.so is NOT copied;
# adb resolves it from nativeLibraryDir at exec time.
INSTALLED_LIBS=(
  "libprotobuf.so"
  "libutf8_validity.so"
  "libz.so"
  "libbrotlicommon.so"
  "libbrotlidec.so"
  "libbrotlienc.so"
  "liblz4.so"
  "libzstd.so"
)

help() {
  sed -n '2,32p' "$0"
}

verify_only() {
  local missing=()
  [[ -f "$JNI_LIB_DIR/libadb.so" ]] || missing+=("libadb.so")
  compgen -G "$JNI_LIB_DIR/libabsl_*.so" >/dev/null || missing+=("libabsl_*.so")
  for lib in "${INSTALLED_LIBS[@]}"; do
    [[ -f "$JNI_LIB_DIR/$lib" ]] || missing+=("$lib")
  done
  for lib in "${ABSEIL_DEPS[@]}"; do
    [[ -f "$JNI_LIB_DIR/$lib" ]] || missing+=("$lib")
  done
  if [[ ${#missing[@]} -eq 0 ]]; then
    echo "OK: bundled adb + dependency closure present in $JNI_LIB_DIR"
    return 0
  fi
  echo "MISSING in $JNI_LIB_DIR:"
  printf '  %s\n' "${missing[@]}"
  return 1
}

# ── arg parsing ─────────────────────────────────────────────────────────────
MODE="install"
CACHE_DIR="${SYSCITY_ADB_CACHE:-${TMPDIR:-/tmp}/syscity-adb-cache}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --verify)
      MODE="verify"
      shift
      ;;
    --cache)
      CACHE_DIR="$2"
      shift 2
      ;;
    -h|--help)
      help
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      help >&2
      exit 1
      ;;
  esac
done

if [[ "$MODE" == "verify" ]]; then
  verify_only
  exit $?
fi

mkdir -p "$JNI_LIB_DIR" "$CACHE_DIR"

# ── download + verify (all-or-nothing) ──────────────────────────────────────
declare -A DEB_PATH=()
for entry in "${PACKAGES[@]}"; do
  IFS='|' read -r name rel want <<< "$entry"
  deb="$CACHE_DIR/$name.deb"
  DEB_PATH["$name"]="$deb"
  echo "fetching $name ..."
  if [[ ! -f "$deb" ]]; then
    curl -fsSL "$BASE_URL/$rel" -o "$deb"
  fi
  got="$(shasum -a 256 "$deb" | awk '{print $1}')"
  if [[ "$got" != "$want" ]]; then
    echo "ERROR: sha256 mismatch for $name" >&2
    echo "  expected: $want" >&2
    echo "  got:      $got" >&2
    echo "  Delete $deb and re-run to re-download." >&2
    exit 1
  fi
done
echo "all $(echo "${#PACKAGES[@]}") packages verified against the Termux signed index"

# ── stage extraction ────────────────────────────────────────────────────────
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

extract_deb() {
  local name="$1" dest="$2"
  local deb="${DEB_PATH[$name]}"
  # A .deb is an ar archive: unpack the inner data.tar.xz (no dpkg needed).
  local ar_dir="$dest/_ar"
  mkdir -p "$ar_dir"
  tar -xf "$deb" -C "$ar_dir"
  tar -xJf "$ar_dir/data.tar.xz" -C "$dest"
  rm -rf "$ar_dir"
}

install_file() {
  local src="$1" dst="$2"
  mkdir -p "$(dirname "$dst")"
  cp -f "$src" "$dst"
}

for entry in "${INSTALL_RULES[@]}"; do
  IFS='|' read -r pkg rel dst <<< "$entry"
  pkg_dir="$STAGE/$pkg"
  [[ -d "$pkg_dir" ]] || extract_deb "$pkg" "$pkg_dir"
  install_file "$pkg_dir/$rel" "$JNI_LIB_DIR/$dst"
done

# abseil closure — whatever the package provides
absl_dir="$STAGE/abseil-cpp"
[[ -d "$absl_dir" ]] || extract_deb "abseil-cpp" "$absl_dir"
absl_libs_dir="$absl_dir/data/data/com.termux/files/usr/lib"
# shellcheck disable=SC2206  # the glob is meant to expand into the array
mapfile -t absl_libs < <(compgen -G "$absl_libs_dir/$ABSEIL_GLOB" || true)
[[ ${#absl_libs[@]} -gt 0 ]] || die "abseil-cpp shipped no libraries — the pin is wrong"
for lib_path in "${absl_libs[@]}"; do
  install_file "$lib_path" "$JNI_LIB_DIR/$(basename "$lib_path")"
done

# ── provenance report ───────────────────────────────────────────────────────
echo ""
echo "Installed into $JNI_LIB_DIR:"
echo "  libadb.so ($(du -h "$JNI_LIB_DIR/libadb.so" | awk '{print $1}')) — android-tools 37.0.0p1-3"
echo "  libprotobuf.so + libutf8_validity.so — libprotobuf 2:35.1"
echo "  $(find "$JNI_LIB_DIR" -maxdepth 1 -name 'libabsl_*.so' | wc -l | tr -d ' ') abseil libs — abseil-cpp 20260526.0 (libprotobuf closure)"
printf '  %s\n' "${INSTALLED_LIBS[@]}"
echo ""
echo "Provenance: every package sha256 is pinned from the Termux stable repo"
echo "Packages index (packages.termux.dev), which is signed by the maintainers."
echo ""
echo "Next: the app discovers adb via has_adb() and exec's it with"
echo "LD_LIBRARY_PATH=nativeLibraryDir (AndroidShellRunner). If adb does not"
echo "run on a given device, the 4.5 tools simply stay hidden (graceful"
echo "degradation) — no other behaviour is affected."
