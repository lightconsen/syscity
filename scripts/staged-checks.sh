#!/usr/bin/env bash
#
# Fast staged-content checks, run by the pre-commit hook before the
# (slower) security audit.
#
# Checks the INDEX (what is about to be committed), not the working tree:
#   1. merge-conflict markers
#   2. oversized files (default limit 5 MB; override with SYSCITY_MAX_FILE_SIZE)
#   3. rustfmt --check on staged .rs files
#   4. likely secrets (gitleaks if installed, otherwise built-in patterns;
#      install gitleaks for broader coverage: brew install gitleaks)
#
# Usage: scripts/staged-checks.sh

set -uo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
RESET='\033[0m'

MAX_FILE_SIZE="${SYSCITY_MAX_FILE_SIZE:-5242880}" # 5 MB

# Anchor to the repository root so Cargo.toml / rustfmt.toml resolve
# regardless of the caller's working directory.
ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$ROOT" ]; then
    echo "staged-checks: must run inside a git repository" >&2
    exit 1
fi
cd "$ROOT"

errors=0
fail() {
    echo -e "${RED}FAIL${RESET}  $1"
    errors=$((errors + 1))
}
pass() { echo -e "${GREEN}PASS${RESET}  $1"; }

STAGED=()
while IFS= read -r f; do
    [ -n "$f" ] && STAGED+=("$f")
done < <(git diff --cached --name-only --diff-filter=ACMR)

if [ "${#STAGED[@]}" -eq 0 ]; then
    echo "staged-checks: nothing staged to check"
    exit 0
fi

echo "🧹 staged checks (${#STAGED[@]} files)"

# ── 1. Merge-conflict markers ──────────────────────────────────────────────
# A lone `=======` is legitimate in markdown; only check unambiguous markers.
conflicts=$(git grep --cached -I -l -E '^(<{7}([[:space:]]|$)|\|{7}|>{7})' -- "${STAGED[@]}" 2>/dev/null || true)
if [ -n "$conflicts" ]; then
    fail "merge-conflict markers in:"
    printf '%s\n' "$conflicts" | sed 's/^/       /'
else
    pass "no merge-conflict markers"
fi

# ── 2. Oversized files ──────────────────────────────────────────────────────
big=""
for f in "${STAGED[@]}"; do
    blob=$(git rev-parse -q --verify ":$f" 2>/dev/null) || continue
    size=$(git cat-file -s "$blob" 2>/dev/null || echo 0)
    if [ "$size" -gt "$MAX_FILE_SIZE" ]; then
        big+="       $f ($((size / 1024 / 1024)) MB)"$'\n'
    fi
done
if [ -n "$big" ]; then
    fail "files over ${MAX_FILE_SIZE}-byte limit:"
    printf '%s' "$big"
    echo "       intentional? retry with SYSCITY_MAX_FILE_SIZE=<bytes>, or git commit --no-verify"
else
    pass "no oversized files"
fi

# ── 3. rustfmt on staged Rust sources ───────────────────────────────────────
rs_files=()
for f in "${STAGED[@]}"; do
    [[ $f == *.rs ]] && [ -f "$f" ] && rs_files+=("$f")
done
if [ "${#rs_files[@]}" -gt 0 ]; then
    edition=$(sed -n 's/^edition[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)
    edition="${edition:-2021}"
    unformatted=""
    for f in "${rs_files[@]}"; do
        if ! rustfmt --edition "$edition" --check "$f" >/dev/null 2>&1; then
            unformatted+="       $f"$'\n'
        fi
    done
    if [ -n "$unformatted" ]; then
        fail "not rustfmt-clean (run cargo fmt):"
        printf '%s' "$unformatted"
    else
        pass "rustfmt clean (${#rs_files[@]} files)"
    fi
fi

# ── 4. Secrets ───────────────────────────────────────────────────────────────
if command -v gitleaks >/dev/null 2>&1; then
    if leaks=$(gitleaks protect --staged --redact 2>&1); then
        pass "no secrets (gitleaks)"
    else
        fail "potential secrets detected by gitleaks:"
        printf '%s\n' "$leaks" | head -40 | sed 's/^/       /'
    fi
else
    # Scan staged content for one POSIX-ERE pattern; drop lines the author
    # explicitly suppressed with a trailing `# secret-scan-ok` marker.
    scan() { # $1 = pattern; $@ = extra git-grep flags (e.g. -i)
        local pattern="$1"
        shift
        local out
        out=$(git grep --cached -I -n -E "$@" -e "$pattern" -- "${STAGED[@]}" 2>/dev/null || true)
        if [ -n "$out" ]; then
            # Suppression markers are language-aware: `#` for shell/config,
        # `//` for Rust/C-style sources, `--` for SQL.
        out=$(printf '%s\n' "$out" | grep -vE '(#|//|--)[[:space:]]*secret-scan-ok[[:space:]]*$' || true)
        fi
        printf '%s' "$out"
    }

    # label<TAB>POSIX-ERE pattern; scanned case-sensitively except the last pair.
    secret_patterns=(
        "pem-private-key	-----BEGIN [A-Z ]*PRIVATE KEY( BLOCK)?-----"
        "minisign-secret-key	minisign encrypted secret key" # secret-scan-ok
        "aws-access-key	(AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}"
        "github-token	gh[posur]_[A-Za-z0-9]{36}"
        "slack-token	xox[abprs]-[A-Za-z0-9-]{10,}"
        "google-api-key	AIza[0-9A-Za-z_-]{35}"
        "openai-api-key	sk-(proj-)?[A-Za-z0-9_]{20}"
        "jwt-token	eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"
    )
    hits=""
    for entry in "${secret_patterns[@]}"; do
        label=${entry%%$'\t'*}
        pattern=${entry#*$'\t'}
        found=$(scan "$pattern")
        [ -n "$found" ] && hits+="[$label]"$'\n'"$found"$'\n'
    done
    # Generic key=value assignments, case-insensitive, 20+ char values so
    # placeholders like "your-api-key-here" don't trip it.
    found=$(scan '(api[_-]?key|apikey|secret|passwd|password|token)["'"'"']?[[:space:]]*[:=][[:space:]]*["'"'"'][A-Za-z0-9/+_-]{20,}["'"'"']' -i)
    [ -n "$found" ] && hits+="[generic-secret-assignment]"$'\n'"$found"$'\n'

    if [ -n "$hits" ]; then
        fail "potential secrets:"
        printf '%s\n' "$hits" | head -30 | sed 's/^/       /'
        echo "       false positive? refine patterns in scripts/staged-checks.sh or bypass with --no-verify"
    else
        pass "no secrets matched built-in patterns"
    fi
fi

echo ""
if [ "$errors" -gt 0 ]; then
    echo -e "${RED}${BOLD}$errors staged-check(s) failed.${RESET}"
    exit 1
fi
exit 0
