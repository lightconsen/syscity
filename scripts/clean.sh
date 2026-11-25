#!/usr/bin/env bash
#
# clean.sh — Clean build output, regenerable artifacts, and demo scratch.
#
# Removes compiled output for the Rust workspace (shared by the desktop app,
# the Android build, and the iOS simulator build), the Android gradle build,
# the iOS/xcode + Swift staging build, the two front-end builds, and — on
# request — the regenerable odds and ends that none of the above covers.
#
# Only gitignored output is touched; tracked sources (gen/apple/xcodeproj,
# gen/android Kotlin sources, main.mm, ...) are never deleted.
#
# Usage:
#   ./scripts/clean.sh [flags]
#
# Flags:
#   -c, --cargo         Clean only the Rust workspace target/.
#   -a, --android       Clean only Android gradle build outputs.
#   -i, --ios           Clean only iOS/xcode + Swift staging outputs.
#   -w, --web           Clean the front-end builds (repo dist/ + site/dist/).
#   -r, --artifacts     Clean regenerable non-build output: lcov.info, the
#                       wasm example target/ dirs, .e2e-tmp/.
#   -d, --demo-capture  Delete .demo-capture/ — READ THE WARNING BELOW FIRST.
#   -A, --all           Everything above.
#   -n, --dry-run       Show what would be removed, with sizes, and stop.
#   -y, --yes           Skip the confirmation prompt.
#   -h, --help          Show this help.
#
# With no flags, the build outputs are cleaned (cargo + android + ios), which
# is what this script has always done. The front-end builds (-w), the mixed
# artifacts (-r) and the demo takes (-d) are opt-in: none of them is implied
# by the bare command, because none of them is build output.
#
# ---------------------------------------------------------------------------
# WARNING — --demo-capture
#
# `.demo-capture/` holds the raw screen recordings the published landing-page
# clip was cut from. The published clip itself is `site/src/assets/
# demo-desktop.{mp4,webm}` plus its poster, and it is a *hand-cut composite*:
# there is no script in this repo that reproduces it from these takes (see the
# note at the top of site/scripts/optimize-assets.sh). Deleting this directory
# therefore throws away the only means of re-cutting the demo without
# re-recording it. Build output can be regenerated in minutes; this cannot.
# ---------------------------------------------------------------------------
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DO_CARGO=0
DO_ANDROID=0
DO_IOS=0
DO_WEB=0
DO_ARTIFACTS=0
DO_CAPTURE=0
DRY_RUN=0
ASSUME_YES=0
SAW_FLAG=0

usage() {
  cat <<'EOF'
clean.sh — Clean build output, regenerable artifacts, and demo scratch.

Usage:
  ./scripts/clean.sh [flags]

Flags:
  -c, --cargo         Clean only the Rust workspace target/.
  -a, --android       Clean only Android gradle build outputs.
  -i, --ios           Clean only iOS/xcode + Swift staging outputs.
  -w, --web           Clean the front-end builds (repo dist/ + site/dist/).
  -r, --artifacts     Clean lcov.info, wasm example target/ dirs, .e2e-tmp/.
  -d, --demo-capture  Delete .demo-capture/ (the hand-cut demo's only source).
  -A, --all           Everything above.
  -n, --dry-run       Show what would be removed, with sizes, and stop.
  -y, --yes           Skip the confirmation prompt.
  -h, --help          Show this help.

With no flags, the build outputs are cleaned (cargo + android + ios).
The front-end builds (-w), the mixed artifacts (-r) and the demo takes (-d)
are opt-in: none of them is implied by the bare command.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -c|--cargo)        DO_CARGO=1; SAW_FLAG=1; shift ;;
    -a|--android)      DO_ANDROID=1; SAW_FLAG=1; shift ;;
    -i|--ios)          DO_IOS=1; SAW_FLAG=1; shift ;;
    -w|--web)          DO_WEB=1; SAW_FLAG=1; shift ;;
    -r|--artifacts)    DO_ARTIFACTS=1; SAW_FLAG=1; shift ;;
    -d|--demo-capture) DO_CAPTURE=1; SAW_FLAG=1; shift ;;
    -A|--all)
      DO_CARGO=1; DO_ANDROID=1; DO_IOS=1
      DO_WEB=1; DO_ARTIFACTS=1; DO_CAPTURE=1
      SAW_FLAG=1; shift ;;
    -n|--dry-run)      DRY_RUN=1; shift ;;
    -y|--yes)          ASSUME_YES=1; shift ;;
    -h|--help)         usage; exit 0 ;;
    *)
      echo "Unknown flag: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# No target flag → the historical default: build outputs only.
if [ "$SAW_FLAG" -eq 0 ]; then
  DO_CARGO=1
  DO_ANDROID=1
  DO_IOS=1
fi

# Gitignored build-output directories (verified against .gitignore / per-gen .gitignore).
ANDROID_DIRS=(
  "$REPO_ROOT/desktop/gen/android/app/build"
  "$REPO_ROOT/desktop/gen/android/build"
  "$REPO_ROOT/desktop/gen/android/.gradle"
)

IOS_DIRS=(
  "$REPO_ROOT/desktop/gen/apple/build"
  "$REPO_ROOT/desktop/gen/apple/Externals"
  "$REPO_ROOT/desktop/mobile-ios/.build"
  "$REPO_ROOT/desktop/mobile-ios/.tauri"
)

# `web/vite.config.ts` sets `outDir: '../dist'`, so the web build lands in the
# repo-root `dist/`, not in `web/dist/` — the previous version of this script
# pointed at the latter and therefore cleaned nothing. The landing page builds
# into `site/dist/` of its own accord.
WEB_DIRS=(
  "$REPO_ROOT/dist"
  "$REPO_ROOT/site/dist"
)

# Mixed, regenerable, and expensive to leave lying around when disk is tight.
# None of it is source: lcov.info is a coverage report, the two example crates
# are separate build targets outside the workspace, and .e2e-tmp is test output.
ARTIFACT_FILES=(
  "$REPO_ROOT/lcov.info"
)

ARTIFACT_DIRS=(
  "$REPO_ROOT/.e2e-tmp"
)

# Every `examples/wasm-plugins/*/` crate is its own workspace and keeps its own
# `target/`; the root `cargo clean` does not reach them.
for d in "$REPO_ROOT"/examples/wasm-plugins/*/target; do
  [ -d "$d" ] && ARTIFACT_DIRS+=("$d")
done

CAPTURE_DIRS=(
  "$REPO_ROOT/.demo-capture"
)

# Refuse to remove anything outside the repo.
assert_in_repo() {
  local path="$1"
  case "$path" in
    "$REPO_ROOT"/*) : ;;
    *)
      echo "Refusing to remove path outside the repo: $path" >&2
      exit 1
      ;;
  esac
}

human_size() {
  # KB rounded for a person; callers pass the raw KB from du -sk so the tree is
  # walked once, not once per unit.
  local kb="$1"
  if [ "$kb" -eq 0 ]; then
    printf "empty"
  elif [ "$kb" -ge 1048576 ]; then
    printf "%d GB" $((kb / 1048576))
  elif [ "$kb" -ge 1024 ]; then
    printf "%d MB" $((kb / 1024))
  else
    printf "%d KB" "$kb"
  fi
}

# The workspace target/ can be redirected; `cargo clean` honours that, and so
# must the preview, or it would report the size of a directory nobody is using.
cargo_target_dir() {
  local dir="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
  case "$dir" in
    /*) echo "$dir" ;;
    *)  echo "$REPO_ROOT/$dir" ;;
  esac
}

remove_path() {
  local path="$1"
  assert_in_repo "$path"
  if [ -d "$path" ]; then
    rm -rf "$path"
  elif [ -f "$path" ]; then
    rm -f "$path"
  fi
}

# Collect (label, path) pairs so the preview and the action cannot disagree.
LABELS=()
PATHS=()
add() {
  LABELS+=("$1")
  PATHS+=("$2")
}

if [ "$DO_CARGO" -eq 1 ]; then
  add "cargo target/ (workspace)" "$(cargo_target_dir)"
fi
if [ "$DO_ANDROID" -eq 1 ]; then
  for d in "${ANDROID_DIRS[@]}"; do add "android gradle build" "$d"; done
fi
if [ "$DO_IOS" -eq 1 ]; then
  for d in "${IOS_DIRS[@]}"; do add "ios xcode + swift staging" "$d"; done
fi
if [ "$DO_WEB" -eq 1 ]; then
  for d in "${WEB_DIRS[@]}"; do add "front-end build" "$d"; done
fi
if [ "$DO_ARTIFACTS" -eq 1 ]; then
  for d in "${ARTIFACT_FILES[@]}"; do add "coverage report" "$d"; done
  for d in "${ARTIFACT_DIRS[@]}"; do add "regenerable artifact" "$d"; done
fi
if [ "$DO_CAPTURE" -eq 1 ]; then
  for d in "${CAPTURE_DIRS[@]}"; do add "DEMO SOURCE (irreplaceable)" "$d"; done
fi

# Anything that exists is listed; the count tells the caller whether there is
# a job at all. A run where nothing exists exits 0 — "already clean" is a
# success, not a failure, and a script that errors on it is awkward in `&&`
# chains and CI.
PRESENT=0
TOTAL_KB=0
echo "Will remove:"
for i in "${!PATHS[@]}"; do
  p="${PATHS[$i]}"
  shown="${p#"$REPO_ROOT"/}"
  [ "$shown" = "$p" ] && shown="$p"
  if [ -e "$p" ]; then
    size="$(du -sk "$p" 2>/dev/null | cut -f1)"
    TOTAL_KB=$((TOTAL_KB + size))
    printf "  %-46s %8s  %s\n" "$shown" "$(human_size "$size")" "${LABELS[$i]}"
    PRESENT=$((PRESENT + 1))
  else
    printf "  %-46s %8s  %s\n" "$shown" "-" "absent"
  fi
done

if [ "$PRESENT" -eq 0 ]; then
  echo
  echo "Nothing to clean."
  exit 0
fi

echo
echo "Total: $(human_size "$TOTAL_KB") across $PRESENT path(s)."

if [ "$DO_CAPTURE" -eq 1 ] && [ -d "$REPO_ROOT/.demo-capture" ]; then
  echo
  echo "!! .demo-capture/ is the raw footage the landing-page demo was cut from."
  echo "!! The published clip is hand-cut and there is no script that rebuilds it,"
  echo "!! so deleting this means re-recording the demo to change it later."
fi

if [ "$DRY_RUN" -eq 1 ]; then
  echo
  echo "Dry run — nothing removed."
  exit 0
fi

if [ "$ASSUME_YES" -eq 0 ]; then
  read -r -p "Proceed? [y/N] " ans
  case "$ans" in
    y|Y) : ;;
    *)
      echo "Aborted."
      exit 1
      ;;
  esac
fi

echo
FREED_KB=0
for i in "${!PATHS[@]}"; do
  p="${PATHS[$i]}"
  [ -e "$p" ] || continue
  size="$(du -sk "$p" 2>/dev/null | cut -f1)"
  # `cargo clean` is preferred over `rm -rf` for the workspace target: it knows
  # about CARGO_TARGET_DIR and per-profile overrides. It is silent by default,
  # and the preview above already reported the size.
  if [ "${LABELS[$i]}" = "cargo target/ (workspace)" ] \
     && command -v cargo >/dev/null 2>&1; then
    if ! (cd "$REPO_ROOT" && cargo clean --quiet); then
      remove_path "$p"
    fi
  else
    remove_path "$p"
  fi
  if [ -e "$p" ]; then
    echo "  could not remove: ${p#"$REPO_ROOT"/}" >&2
  else
    FREED_KB=$((FREED_KB + size))
  fi
done

echo
echo "Done. Freed $(human_size "$FREED_KB")."
