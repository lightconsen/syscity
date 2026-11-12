#!/usr/bin/env bash
#
# Disk headroom preflight for the build-heavy checks.
#
# A cargo build with `--all-features` on this project needs roughly 20 GB of
# free space (the dependency tree is ~1100 crates, and `target/debug/deps` for
# one feature set alone is ~49 GB once incremental and instrumented copies
# pile up). When the filesystem quietly fills, the *linker* is what fails:
#
#   ld: write() failed, errno=28 (No space left on device)
#   clang: error: linker command failed with exit code 1
#
# which surfaces as "could not compile `syscity` (bin)" — indistinguishable
# from a real code error, and it took down five unrelated checks in one run
# before anyone read the errno. Coverage is the worst offender because
# llvm-cov builds a second, fully instrumented copy of every dependency.
#
# This check turns that into one clear message naming the cause and the fix.
# It is advisory by construction: it prints and fails only when free space is
# genuinely too low, so a healthy machine pays milliseconds for it.
#
# Usage: scripts/check-disk-space.sh [--min-gb N] [--quiet]
#   --min-gb N  required free space in GB (default 20)
#   --quiet     print nothing when there is enough space (for hook use)
# Exit 0 = enough space; exit 1 = not enough.

set -uo pipefail

RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
RESET='\033[0m'

MIN_GB=20
QUIET=false
while [ $# -gt 0 ]; do
    case "$1" in
        --min-gb)
            MIN_GB="${2:-}"
            shift 2
            ;;
        --quiet)
            QUIET=true
            shift
            ;;
        -h|--help)
            sed -n '2,/^# Exit 0/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "check-disk-space: unknown argument '$1'" >&2
            exit 2
            ;;
    esac
done

if ! [[ "$MIN_GB" =~ ^[0-9]+$ ]]; then
    echo "check-disk-space: --min-gb needs a whole number, got '$MIN_GB'" >&2
    exit 2
fi

# Measure the filesystem that actually holds the build output. On macOS
# `target/` lives under the data volume, not `/`, and `/` reports its own
# read-only snapshot — querying the right path matters more than the command.
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
[ -n "$ROOT" ] || ROOT="$(pwd)"
PROBE="$ROOT"
while [ ! -d "$PROBE" ] && [ "$PROBE" != "/" ]; do
    PROBE="$(dirname "$PROBE")"
done

free_gb=$(
    df -Pk "$PROBE" 2>/dev/null \
        | awk 'NR==2 { printf "%d", $4 / 1024 / 1024 }'
)
if [ -z "$free_gb" ]; then
    # Cannot measure — do not block a build on a missing `df`.
    [ "$QUIET" = true ] || echo -e "${YELLOW}WARN${RESET}  could not measure free disk space at $PROBE"
    exit 0
fi

if [ "$free_gb" -lt "$MIN_GB" ]; then
    echo ""
    echo -e "${RED}FAIL${RESET}  only ${free_gb} GB free on the volume holding the build tree"
    echo "        A cargo --all-features build needs about ${MIN_GB} GB; below that the"
    echo "        linker dies with 'No space left on device' and the failure is reported"
    echo "        as a compile error in unrelated modules."
    echo ""
    echo "        Reclaim space, then retry:"
    echo "          rm -rf target/llvm-cov-target    # the instrumented copy (largest)"
    echo "          rm -rf target/debug/incremental  # per-crate incremental cache"
    echo "          ./scripts/clean.sh --cargo       # the whole target/"
    echo ""
    exit 1
fi

[ "$QUIET" = true ] || echo -e "${GREEN}PASS${RESET}  ${free_gb} GB free (need ${MIN_GB} GB)"
exit 0
