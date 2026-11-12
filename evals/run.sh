#!/usr/bin/env bash
# ===========================================================================
#  Syscity Eval Runner
#  统一的评测执行脚本，覆盖所有常用场景。
#
#  用法:
#    ./evals/run.sh <command> [options]
#
#  命令:
#    quick               快速冒烟测试 (ci_smoke, 3 trials, 30% 采样)
#    full                全量运行能力集 (capability 套件)
#    regression          回归套件 (高标准, 含 badcase 收集)
#    release-gate        发布门禁评估
#    skill               Skill 专项评测 + 详细输出
#    calibrate           Judge 校准
#    drift               检查 Judge 漂移
#    badcase             查看/聚类 badcase
#    action-items        从 badcase 生成 action items
#    feedback            查看反馈生产线统计
#    ci                  CI 完整检查 (validate + quick + calibrate)
#    compare <before>    与历史基线对比
#    list                列举所有可用套件
#    validate            验证所有 YAML 格式
#    clean               清理 badcases/review 运行时数据
# ===========================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="${PROJECT_DIR}/target/release/syscity"
CARGO="cargo"

# ── Color helpers ──────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERROR]${NC} $*"; }

# ── Ensure binary is built ─────────────────────────────────────────────────
ensure_built() {
    if [ ! -f "$BINARY" ]; then
        info "Building release binary..."
        cargo build --release -q
    fi
}

# ── Eval home isolation ────────────────────────────────────────────────────
# `SYSCITY_HOME` is read exactly once, at process start, and every later path
# (config, database, workspace, secrets, models) hangs off it. Pointing it at a
# throwaway directory is what keeps an eval run out of the operator's real
# `~/.syscity` — before this, an eval fixture's "我叫李四…" became the live
# user profile and an adversarial task wrote its own prompt text into the real
# `workspace/memory/`.
#
# The pieces a run genuinely needs are carried over: the config (provider,
# model, API key refs) and read-only links to the downloaded models and the
# secret store. Everything the run writes lands in the temp root.
setup_eval_home() {
    if [ -n "${SYSCITY_EVAL_HOME:-}" ]; then
        # Reuse a directory across invocations (debugging a suite).
        mkdir -p "$SYSCITY_EVAL_HOME"
        export SYSCITY_HOME="$SYSCITY_EVAL_HOME"
        info "Eval home: $SYSCITY_HOME"
        return
    fi

    local real_home="${SYSCITY_HOME:-$HOME/.syscity}"
    # Not `local`: the trap runs after this function has returned, and a local
    # would be unbound under `set -u` — which both leaks the directory and
    # turns a passing run into a failure at exit.
    EVAL_HOME="$(mktemp -d "${TMPDIR:-/tmp}/syscity-eval-home.XXXXXX")"
    local eval_home="$EVAL_HOME"
    trap 'rm -rf "$EVAL_HOME"' EXIT

    if [ -f "$real_home/config.toml" ]; then
        cp "$real_home/config.toml" "$eval_home/config.toml"
    fi
    # Read-only access to things the run must not re-download or duplicate.
    # `config.toml` holds absolute paths for the embedding model, so the model
    # link is belt-and-braces for any config that uses a relative one.
    #
    # `secrets` is linked because provider keys can live in the file store.
    # `agents` and `skills` deliberately are *not*: they are writable
    # configuration, and linking them would hand an eval run the real ones.
    local entry
    for entry in models secrets; do
        if [ -e "$real_home/$entry" ] && [ ! -e "$eval_home/$entry" ]; then
            ln -s "$real_home/$entry" "$eval_home/$entry"
        fi
    done

    export SYSCITY_HOME="$eval_home"
    info "Eval home isolated at $SYSCITY_HOME"
}

# ── Commands ───────────────────────────────────────────────────────────────

cmd_quick() {
    # 快速冒烟测试 — 3 trials, 30% 采样, skill breakdown
    info "快速冒烟测试 (ci_smoke, trials=3, sampling=30%)"
    "$BINARY" eval run ci_smoke --full --trials 3 --sampling-rate 0.3 --skill-breakdown
}

cmd_full() {
    # 全量运行 — capability 套件
    info "全量能力评测 (capability)"
    if [ $# -ge 1 ] && [ -n "$1" ]; then
        local rate="$1"
        info "  采样率: $rate"
        "$BINARY" eval run registry --full --trials 5 --sampling-rate "$rate"
    else
        "$BINARY" eval run registry --full --trials 5
    fi
}

cmd_regression() {
    # 回归套件 — badcase 收集开启
    info "回归评测 (release_gate, 含 badcase 收集)"
    "$BINARY" eval run release_gate --full --trials 5 --collect-badcases --skill-breakdown
}

cmd_release_gate() {
    # 发布门禁
    info "发布门禁评估 (release_gate)"
    "$BINARY" eval run release_gate --full --trials 5 --collect-badcases
}

cmd_skill() {
    # Skill 专项评测
    info "Skill 专项评测 (skills)"
    # 直接跑 skill 任务文件
    for skill_file in "$SCRIPT_DIR"/skills/*.yaml; do
        local name
        name=$(basename "$skill_file" .yaml)
        info "  Skill: $name"
        "$BINARY" eval run "$name" --full --trials 5 --skill-breakdown || warn "  Skill $name 有失败项"
    done
}

cmd_calibrate() {
    info "Judge 校准"
    "$BINARY" eval calibrate
    info "校准完成。查看历史: $BINARY eval calibrate --history"
}

cmd_drift() {
    info "Judge 漂移检查"
    "$BINARY" eval calibrate --drift
}

cmd_badcase() {
    local mode="${1:-list}"
    case "$mode" in
        list)
            info "Badcase 列表:"
            "$BINARY" eval badcase-list --verbose
            ;;
        cluster)
            info "Badcase 聚类 (按现象×模块):"
            "$BINARY" eval badcase-list --cluster
            ;;
        *)
            err "未知 badcase 模式: $mode (可用: list, cluster)"
            exit 1
            ;;
    esac
}

cmd_action_items() {
    info "从 badcase RCA 结果生成 action items..."
    "$BINARY" eval action-items --generate
    echo ""
    info "Action items 详情:"
    "$BINARY" eval action-items --verbose
}

cmd_feedback() {
    local channel="${1:-eval}"
    info "反馈生产线: $channel"
    "$BINARY" eval feedback "$channel" --verbose
}

cmd_ci() {
    # CI 完整检查
    local failed=0

    info "========== CI 完整检查 =========="

    info "[1/5] YAML 格式验证..."
    "$BINARY" eval validate || { err "YAML 验证失败"; failed=1; }

    info "[2/5] 快速冒烟测试..."
    cmd_quick || { err "冒烟测试失败"; failed=1; }

    info "[3/5] Judge 校准..."
    cmd_calibrate || { err "校准失败"; failed=1; }

    info "[4/5] Judge 漂移检查..."
    cmd_drift || { err "漂移检查失败"; failed=1; }

    info "[5/5] 生成 action items..."
    cmd_action_items || warn "action items 生成失败（无 badcase 时正常）"

    echo ""
    if [ "$failed" -eq 0 ]; then
        ok "CI 检查全部通过"
    else
        err "CI 检查有失败项"
        exit 1
    fi
}

cmd_compare() {
    if [ $# -lt 1 ]; then
        err "用法: $0 compare <baseline_name>"
        echo "  e.g. $0 compare v1.0"
        exit 1
    fi
    local baseline="$1"
    info "与基线对比: $baseline"
    "$BINARY" eval compare --baseline "$baseline"
}

cmd_list() {
    info "可用套件:"
    "$BINARY" eval list
}

cmd_validate() {
    info "验证所有 YAML 格式..."
    "$BINARY" eval validate
}

cmd_clean() {
    info "清理运行时数据..."
    rm -rf "$SCRIPT_DIR"/badcases/*
    rm -rf "$SCRIPT_DIR"/review/*
    rm -rf "$SCRIPT_DIR"/actions/*
    ok "已清理 badcases/ review/ actions/"
}

# ── Help ───────────────────────────────────────────────────────────────────

show_help() {
    sed -n '2,/^# =======/p' "${BASH_SOURCE[0]}" \
        | sed 's/^# //; s/^#//' \
        | head -n -1
}

# ── Main ───────────────────────────────────────────────────────────────────

main() {
    if [ $# -eq 0 ]; then
        show_help
        exit 1
    fi

    local cmd="$1"
    shift

    # 不需要 build 的命令
    case "$cmd" in
        help|--help|-h)
            show_help
            exit 0
            ;;
        list|validate|clean)
            ensure_built
            "cmd_$cmd" "$@"
            exit $?
            ;;
    esac

    # 需要 build + API key 的命令
    if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ]; then
        err "需要设置 ANTHROPIC_API_KEY 或 OPENAI_API_KEY"
        exit 1
    fi

    setup_eval_home
    ensure_built

    if declare -F "cmd_$cmd" > /dev/null; then
        "cmd_$cmd" "$@"
    else
        err "未知命令: $cmd"
        echo ""
        show_help
        exit 1
    fi
}

main "$@"
