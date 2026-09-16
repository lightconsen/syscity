#!/usr/bin/env bash
# Verifies the invariant-declaration convention (#16): every top-level module
# (a directory under src/, or a file module at src/*.rs) must either register
# runtime invariants with `core::invariants` or carry an explicit
# `INVARIANTS-NONE:` marker explaining why it holds none. Nothing is silently
# unchecked — which is exactly what this script itself prevents.
#
# Shared by `scripts/static-analysis.sh --full` and the CI job, so the two can
# never drift apart. Exit 0 = every module is covered; exit 1 = list the rest.
set -euo pipefail

# A top-level container counts as covered when any file under it registers a
# check (a definition or call of `invariant_checks`, or a `register_builtins`
# consumer) or carries the marker. A bare mention of the framework's name in a
# comment is NOT enough — that is the weakness a comment could trivially
# satisfy.
declare_declared() {
    git grep -q -E '(INVARIANTS-NONE:|invariant_checks\(|register_builtins)' -- "$1" 2>/dev/null
}

missing=''
for dir in src/*/; do
    declare_declared "$dir" || missing="$missing $dir"
done
# Top-level file modules used to escape the check entirely (the directory loop
# above never looked at them). The crate entry points own nothing, so they are
# excluded, not marked.
for file in src/*.rs; do
    case "$(basename "$file")" in
        lib.rs|main.rs) continue ;;
    esac
    declare_declared "$file" || missing="$missing $file"
done

if [[ -n "$missing" ]]; then
    echo "modules without invariant registration or an explicit INVARIANTS-NONE marker:" >&2
    for m in $missing; do
        echo "  $m" >&2
    done
    exit 1
fi
echo "all top-level modules carry an invariant declaration or a NONE marker"
