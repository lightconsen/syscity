#!/bin/bash
# Syscity Release Script
# Bumps the version, updates the CHANGELOG, and publishes a release tag.
#
# Usage:
#   ./scripts/release.sh                # patch bump (default): 0.3.1 → 0.3.2
#   ./scripts/release.sh --patch        # patch bump:   0.3.1 → 0.3.2
#   ./scripts/release.sh --minor        # minor bump:   0.3.1 → 0.4.0
#   ./scripts/release.sh --major        # major bump:   0.3.1 → 1.0.0
#   ./scripts/release.sh --tag v1.2.3   # custom tag (no bump / changelog / commit)
#   --no-edit                           # don't open an editor on CHANGELOG.md
#   --skip-tests                        # skip the pre-release cargo test run
#
# Release flow (bump modes):
#   1. Preconditions: clean tree, on main, in sync with origin/main, target tag free
#   2. cargo test --all-features --lib (unless --skip-tests)
#   3. Promote the CHANGELOG `## [Unreleased]` section to `## [X.Y.Z] - <date>`;
#      if that section is empty, draft one from the changes since the previous
#      tag (full commit messages + diffstat + code diff, lockfiles excluded)
#      via the LLM configured in scripts/.env (SYSCITY_* vars, gitignored;
#      OpenAI- or Anthropic-compatible), falling back to mechanical
#      ✨→Added / 🐛→Fixed / rest→Changed grouping of commit subjects when it's
#      absent or fails.
#      Opens $EDITOR unless --no-edit or non-interactive.
#   4. Bump the version in Cargo.toml, desktop/Cargo.toml, web/package.json
#      and refresh Cargo.lock
#   5. Commit `🔖 chore(release): vX.Y.Z`, push main, create the tag and push it
#      (pushing the tag triggers the Release workflow that builds artifacts)

set -euo pipefail

BUMP=""
CUSTOM_TAG=""
NO_EDIT=false
SKIP_TESTS=false

while [ $# -gt 0 ]; do
  case "$1" in
    --patch|--minor|--major)
      if [ -n "$BUMP" ] && [ "$BUMP" != "${1#--}" ]; then
        echo "✗ Pick only one of --patch/--minor/--major" >&2
        exit 1
      fi
      BUMP="${1#--}"
      ;;
    --tag)
      CUSTOM_TAG="${2:-}"
      [ -n "$CUSTOM_TAG" ] || { echo "✗ --tag requires a value" >&2; exit 1; }
      shift
      ;;
    --no-edit) NO_EDIT=true ;;
    --skip-tests) SKIP_TESTS=true ;;
    -h|--help)
      echo "Usage: ./scripts/release.sh [--patch|--minor|--major|--tag <tag>] [--no-edit] [--skip-tests]"
      echo ""
      echo "  --patch       bump the patch version (default): 0.3.1 → 0.3.2"
      echo "  --minor       bump the minor version:           0.3.1 → 0.4.0"
      echo "  --major       bump the major version:           0.3.1 → 1.0.0"
      echo "  --tag <tag>   publish a custom tag without bumping/committing"
      echo "  --no-edit     skip opening an editor on CHANGELOG.md"
      echo "  --skip-tests  skip the pre-release test run"
      echo ""
      echo "  Draft release notes summarize the changes since the last tag"
      echo "  (commit messages + diff) via the LLM configured in scripts/.env"
      echo "  (SYSCITY_* vars) when present; falls back to mechanical grouping."
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
  shift
done
BUMP="${BUMP:-patch}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

echo "🧭 Checking preconditions..."
if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "✗ Not in a git repository" >&2
  exit 1
fi
if ! git diff-index --quiet HEAD --; then
  echo "✗ Uncommitted changes to tracked files — commit or stash first" >&2
  git status --short >&2
  exit 1
fi
branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$branch" != "main" ]; then
  echo "✗ Releases are cut from main (currently on $branch)" >&2
  exit 1
fi
git fetch -q origin main
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "✗ main is out of sync with origin/main — push/pull first" >&2
  exit 1
fi

if [ -z "$CUSTOM_TAG" ]; then
  current="$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2)"
  desktop_current="$(grep -m1 '^version = ' desktop/Cargo.toml | cut -d '"' -f2)"
  web_current="$(grep -m1 '"version"' web/package.json | sed 's/.*"version": *"\([^"]*\)".*/\1/')"
  if [ "$current" != "$desktop_current" ] || [ "$current" != "$web_current" ]; then
    echo "✗ Version files disagree: Cargo.toml=$current desktop=$desktop_current web=$web_current" >&2
    exit 1
  fi

  IFS='.' read -r MA MI PA <<< "$current"
  case "$BUMP" in
    major) MA=$((MA + 1)); MI=0; PA=0 ;;
    minor) MI=$((MI + 1)); PA=0 ;;
    patch) PA=$((PA + 1)) ;;
  esac
  next="$MA.$MI.$PA"
  TAG="v$next"
  echo "🚀 Releasing $current → $next"
else
  TAG="$CUSTOM_TAG"
  echo "🚀 Publishing custom tag: $TAG"
  if ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    printf "⚠️  Tag '%s' is not plain semver with a v prefix. Continue? (y/N) " "$TAG"
    read -r -n 1 reply
    echo
    case "$reply" in
      y|Y) ;;
      *) exit 1 ;;
    esac
  fi
fi

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1; then
  echo "✗ Tag $TAG already exists locally" >&2
  exit 1
fi
if git ls-remote --tags origin "refs/tags/$TAG" | grep -qF "refs/tags/$TAG"; then
  echo "✗ Tag $TAG already exists on origin" >&2
  exit 1
fi

if [ -z "$CUSTOM_TAG" ] && [ "$SKIP_TESTS" = false ]; then
  echo "🧪 Running cargo test --all-features --lib ..."
  cargo test -q --all-features --lib
fi

if [ -n "$CUSTOM_TAG" ]; then
  git tag "$TAG"
  git push -q origin "$TAG"
  echo "✅ Published $TAG"
  echo "   The Release workflow is building artifacts — track it with:"
  echo "     gh run watch \$(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
  exit 0
fi

echo "📝 Updating CHANGELOG.md..."
release_date="$(date +%F)"
added=""
fixed=""
changed=""
if git rev-parse -q --verify "refs/tags/v$current" >/dev/null 2>&1; then
  subjects="$(git log "v$current..HEAD" --pretty=%s)"
  # Only pipe non-empty subjects: `echo ""` emits an empty line and the
  # exclusion grep (-Ev) would pass it through, producing a phantom "- "
  # bullet that defeats the "Nothing to release" guard below.
  if [ -n "$subjects" ]; then
    added="$(echo "$subjects" | grep '^✨' | sed 's/^[^ ]* //' | sed 's/^/- /' || true)"
    fixed="$(echo "$subjects" | grep '^🐛' | sed 's/^[^ ]* //' | sed 's/^/- /' || true)"
    changed="$(echo "$subjects" | grep -Ev '^(✨|🐛|🔖)' | sed 's/^[^ ]* //' | sed 's/^/- /' || true)"
  fi
fi

# Concrete change material for the LLM draft: full commit messages plus the
# diffstat and patch since the previous tag (lockfiles excluded). The patch is
# truncated to a conservative context budget when needed.
changes_file="$(mktemp)"
if git rev-parse -q --verify "refs/tags/v$current" >/dev/null 2>&1; then
  diff_paths=(. ':(exclude)Cargo.lock' ':(exclude)desktop/Cargo.lock' ':(exclude)web/pnpm-lock.yaml')
  {
    echo "=== commit messages v$current..HEAD ==="
    git log "v$current..HEAD" --pretty='--- %h %s%n%b'
    echo
    echo "=== diffstat v$current..HEAD ==="
    git diff --stat "v$current..HEAD" -- "${diff_paths[@]}"
  } > "$changes_file"
  patch_file="$(mktemp)"
  git diff "v$current..HEAD" -- "${diff_paths[@]}" > "$patch_file"
  max_chars=120000
  base_len="$(wc -c < "$changes_file")"
  if [ "$base_len" -lt "$max_chars" ]; then
    room=$((max_chars - base_len))
    if [ "$(wc -c < "$patch_file")" -le "$room" ]; then
      echo "=== patch v$current..HEAD ===" >> "$changes_file"
      cat "$patch_file" >> "$changes_file"
    else
      echo "=== patch v$current..HEAD (truncated) ===" >> "$changes_file"
      head -c "$room" "$patch_file" >> "$changes_file"
      printf '\n[... patch truncated ...]\n' >> "$changes_file"
    fi
  fi
  rm -f "$patch_file"
fi

# Local LLM config for drafting release notes (gitignored, optional).
# Exported so the python step below reads SYSCITY_* via os.environ.
if [ -f scripts/.env ]; then
  set -a
  # shellcheck disable=SC1091
  source scripts/.env
  set +a
fi

# Draft the section in python. The heredoc must stay OUTSIDE command
# substitution: /bin/bash 3.2 (macOS shebang) mis-parses backticks inside
# $(...) heredocs, and the LLM fence-stripping below uses them.
notes_file="$(mktemp)"
if python3 - "$next" "$release_date" "$current" "$added" "$fixed" "$changed" "$changes_file" > "$notes_file" <<'PY'
import json
import os
import re
import sys
import urllib.request

next_ver, date, current, added, fixed, changed, changes_path = sys.argv[1:8]
path = "CHANGELOG.md"
text = open(path, encoding="utf-8").read()

marker = "## [Unreleased]"
try:
    start = text.index(marker)
except ValueError:
    sys.exit("CHANGELOG.md has no ## [Unreleased] section")
rest = text[start + len(marker):]
m = re.search(r"\n## \[", rest)
body, tail = (rest[: m.start()], rest[m.start():]) if m else (rest, "")
body = body.strip("\n")


def llm_release_notes():
    """Draft release notes via the SYSCITY_* provider config (scripts/.env).

    The material is the changes file written by the shell above: full commit
    messages plus the diffstat and patch since the previous tag. Returns
    (model, text) on success; None when unconfigured, the request fails, or
    the output is unusable (a warning goes to stderr on failure).
    """
    base = os.environ.get("SYSCITY_BASE_URL", "").strip().rstrip("/")
    key = os.environ.get("SYSCITY_API_KEY", "").strip()
    model = os.environ.get("SYSCITY_MODEL", "").strip()
    changes = ""
    if os.path.exists(changes_path):
        changes = open(changes_path, encoding="utf-8").read().strip()
    if not (base and key and model and changes.strip()):
        return None
    is_anthropic = os.environ.get("SYSCITY_IS_ANTHROPIC", "").strip().lower() in ("true", "1")
    prompt = f"""You are drafting user-facing release notes for Syscity, version {next_ver} (previous release v{current}).

Changes since v{current} — full commit messages, then the diffstat and code diff (lockfiles excluded; the patch may be truncated):

{changes}

Rewrite them as Keep-a-Changelog release notes:
- Base each bullet on the concrete changes visible in the messages and diff (what a user can now do, what behaves differently, what was fixed) — never copy commit titles verbatim.
- Group related commits into one bullet when they form a single user-visible change; phrase bullets as user-visible behavior, not commit-speak.
- Drop release chores (🔖 version bumps), lockfile churn, and CI-only changes unless they matter to users.
- Write in the same language as the commit subjects.
- Do not invent changes that are not supported by the material.
- No preamble, no top-level "##" version header, no dates.
"""
    if is_anthropic:
        url = base + "/v1/messages"
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01",
                   "content-type": "application/json"}
    else:
        url = base + "/chat/completions"
        headers = {"Authorization": "Bearer " + key,
                   "content-type": "application/json"}
    payload = {"model": model, "max_tokens": 8000, "temperature": 0.2,
               "messages": [{"role": "user", "content": prompt}]}
    # Generous budget + timeout: reasoning models spend their completion
    # tokens on invisible CoT before writing any content.
    req = urllib.request.Request(url, data=json.dumps(payload).encode("utf-8"),
                                 headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            data = json.load(resp)
    except Exception as exc:
        print(f"⚠️  LLM draft failed ({exc}) — falling back to mechanical grouping",
              file=sys.stderr)
        return None
    if is_anthropic:
        out = "".join(block.get("text", "") for block in data.get("content", []))
    else:
        out = data.get("choices", [{}])[0].get("message", {}).get("content", "")
    out = out.strip()
    if out.startswith("```"):  # strip a wrapping markdown fence, if any
        out = re.sub(r"^```[^\n]*\n", "", out)
        out = re.sub(r"\n?```\s*$", "", out).strip()
    if not out or "## [" in out or "###" not in out:
        print("⚠️  LLM draft output unusable — falling back to mechanical grouping",
              file=sys.stderr)
        return None
    return model, out


if not body.strip():
    llm = llm_release_notes()
    if llm:
        body = llm[1]
        source = ("Drafted release notes with " + llm[0]
                  + " (scripts/.env) from commits since v" + current)
    else:
        parts = []
        if added.strip():
            parts.append("### Added\n\n" + added.strip())
        if fixed.strip():
            parts.append("### Fixed\n\n" + fixed.strip())
        if changed.strip():
            parts.append("### Changed\n\n" + changed.strip())
        if not parts:
            sys.exit(
                "Nothing to release: the Unreleased section is empty and there are "
                "no commits since v" + current
            )
        body = "\n\n".join(parts)
        source = "Drafted release notes from commits since v" + current
else:
    source = "Promoted the Unreleased section to " + next_ver

section = f"## [{next_ver}] - {date}\n\n{body}\n"
open(path, "w", encoding="utf-8").write(text[:start] + marker + "\n\n" + section + tail)
print(source)
PY
then
  source_note="$(cat "$notes_file")"
else
  source_note=""
fi
rm -f "$notes_file" "$changes_file"
if [ -z "$source_note" ]; then
  exit 1
fi

if ! grep -q "^## \[$next\]" CHANGELOG.md; then
  echo "✗ $source_note" >&2
  exit 1
fi
echo "   $source_note"

if [ "$NO_EDIT" = false ] && [ -t 0 ]; then
  echo "✏️  Review CHANGELOG.md (${EDITOR:-vi})..."
  "${EDITOR:-vi}" CHANGELOG.md
fi

echo "🔢 Bumping versions to $next..."
perl -pi -e 's/^version = "[^"]*"/version = "'"$next"'"/' Cargo.toml desktop/Cargo.toml
perl -pi -e 's/("version":\s*)"[^"]*"/$1"'"$next"'"/' web/package.json
cargo check -q  # refresh Cargo.lock + catch compile breakage before publishing

git add CHANGELOG.md Cargo.toml Cargo.lock desktop/Cargo.toml web/package.json
git commit -m "🔖 chore(release): $TAG"
git push -q origin main
git tag "$TAG"
git push -q origin "$TAG"

echo "✅ Released $TAG"
echo "   The Release workflow is building artifacts — track it with:"
echo "     gh run watch \$(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
