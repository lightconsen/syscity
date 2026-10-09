#!/usr/bin/env bash
#
# Seed agent issues — turn the repo's existing backlog sources into GitHub
# issues labeled `agent:ready`, which the nightly agent loop consumes.
#
# Sources:
#   evals/actions/actions.json   machine-readable action items (run
#                                `syscity eval action-items --generate` first)
#   docs/risk-register.md        open rows (status marked with the ⏳ marker)
#
# `hermes-diffs.local.md` is deliberately NOT read here: it is gitignored and
# prose-only, so its gaps are filed by hand. This script covers the structured
# sources.
#
# Usage:
#   ./scripts/seed-agent-issues.sh [--dry-run] [--source risk|actions|all]
#
# Requires: gh (authenticated), jq.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

DRY_RUN=false
SOURCE=all
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=true; shift ;;
        --source) SOURCE="${2:-all}"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--dry-run] [--source risk|actions|all]"
            exit 0
            ;;
        *) echo "unknown argument '$1'" >&2; exit 1 ;;
    esac
done

command -v gh >/dev/null || { echo "gh is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

# Titles already used by any issue (open or closed), so re-runs are idempotent.
existing="$(gh issue list --state all --limit 1000 --json title --jq '.[].title' || true)"

created=0
skipped=0

file_issue() {
    local title=$1 body=$2
    if printf '%s\n' "$existing" | grep -Fxq "$title"; then
        echo "  = exists: $title"
        skipped=$((skipped + 1))
        return
    fi
    if $DRY_RUN; then
        echo "  + would file: $title"
        created=$((created + 1))
        return
    fi
    printf '%s' "$body" | gh issue create \
        --title "$title" \
        --label agent:ready \
        --body-file - >/dev/null
    echo "  + filed: $title"
    created=$((created + 1))
}

# ── Source: eval action items ──────────────────────────────────────────────
seed_actions() {
    local file=evals/actions/actions.json
    if [ ! -f "$file" ]; then
        echo "actions: $file not found — run 'syscity eval action-items --generate' first"
        return
    fi
    echo "actions: $file"
    local count
    count=$(jq '[.[] | select(.level != "report_only")] | length' "$file")
    if [ "$count" -eq 0 ]; then
        echo "  (no actionable items)"
        return
    fi
    # One compact JSON object per line, so a multi-line body survives the loop.
    local line title body
    while IFS= read -r line; do
        title=$(jq -r '.title' <<<"$line")
        body=$(jq -r '.body' <<<"$line")
        file_issue "$title" "$body"
    done < <(jq -c '
      .[] | select(.level != "report_only") |
      {
        title: "[action] \(.id): \(.problem_summary)",
        body: (
          "Source: `evals/actions/actions.json` (\(.id), \(.priority), \(.level), owner: \(.owner))\n\n" +
          "**Root cause:** \(.root_cause)\n\n" +
          "**Suggested action:** \(.suggested_action)\n\n" +
          "**Evidence:**\n" +
          (([.evidence[]? | "- \(.[0:2000])\(if length > 2000 then " …" else "" end)"] | join("\n")) // "- (none recorded)") + "\n\n" +
          "**Done when:** \(.acceptance_criteria)\n"
        )
      }' "$file")
}

# ── Source: risk register open rows ────────────────────────────────────────
seed_risk() {
    local file=docs/risk-register.md
    if [ ! -f "$file" ]; then
        echo "risk: $file not found"
        return
    fi
    echo "risk: $file"
    # Table rows look like: | **SEC-005** | P1 | area | ⏳ ... | description |
    # Each row is a single line, so a tab-separated TITLE/BODY stream is safe.
    local kind value title
    while IFS=$'\t' read -r kind value; do
        case "$kind" in
            TITLE) title="$value" ;;
            BODY) file_issue "$title" "$value" ;;
        esac
    done < <(awk -F'|' '
      /^\|/ && $0 ~ /⏳/ {
        id=$2; level=$3; area=$4; status=$5; desc=$6
        gsub(/^[ \t]+|[ \t]+$/, "", id); gsub(/\*/, "", id)
        gsub(/^[ \t]+|[ \t]+$/, "", level)
        gsub(/^[ \t]+|[ \t]+$/, "", area)
        gsub(/^[ \t]+|[ \t]+$/, "", status)
        gsub(/^[ \t]+|[ \t]+$/, "", desc)
        if (id == "") next
        printf "TITLE\t[risk] %s: %s\n", id, area
        printf "BODY\tSource: docs/risk-register.md (%s, %s). Status: %s\n\n%s\n\n**Done when:** the item is re-verified against current source and either fixed, or the register row is updated with evidence and a new status.\n", id, level, status, desc
      }' "$file")
}

case "$SOURCE" in
    actions) seed_actions ;;
    risk) seed_risk ;;
    all) seed_actions; seed_risk ;;
    *) echo "unknown --source '$SOURCE' (expected risk|actions|all)" >&2; exit 1 ;;
esac

echo ""
if $DRY_RUN; then
    echo "dry run: $created would be filed, $skipped already exist."
else
    echo "filed $created, skipped $skipped (already exist)."
fi
