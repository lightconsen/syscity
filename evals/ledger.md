# Eval Ledger

Durable record of eval suite runs — one row per completed
`syscity eval run` (appended automatically, best-effort).

Why a file: files and git history survive context compaction and CI log
expiry; conversation memory does not. When judging "which version is best",
compare against this ledger, not memory. See `docs/harness.md` §8.

| Date | Commit | Suite | Executor | Judge | Mode | Passed | Rate | Gate | Failed tasks |
|------|--------|-------|----------|-------|------|--------|------|------|--------------|

## 外部基准（benchmark）

外部 harness 经 `/v1/chat/completions`（`mode: raw`）驱动网关跑出的分数。行粒度与上面
不同：上面是"一次套件运行"，这里是"一次运行 × 一个基准子类目"，判定由各基准的**官方
evaluator** 给出，不自写判分器（见 `docs/benchmark.md`）。

| Date | Benchmark | Version | Endpoint model | Commit | Category | Correct | Accuracy | Notes |
|------|-----------|---------|----------------|--------|----------|---------|----------|-------|
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | `syscity-agent` → deepseek-v4-flash | 0c362599 | simple_python | 367/400 | 91.75% | 失败几乎全是参数值：`value_error:string` 13、`value_error:others` 9、`value_error:list/tuple` 5、`type_error:nested` 4、`wrong_count` 2 |
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | 同上 | 0c362599 | irrelevance | 172/240 | 71.67% | 68 条全部是 `irrelevance_error:decoder_success`——不该调工具时调了 |
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | 同上 | 0c362599 | multi_turn_base | 123/200 | 61.50% | 多轮 `role: tool` 结果经网关往返；部分轮次出现 `Failed to decode the model response` |

口径：`raw` 模式，网关只做 provider 透传、工具由 BFCL 自己执行，所以这组数字衡量的是
"默认模型 + OpenAI 兼容线格式"，**不是** syscity 内置 Agent 循环的水平。复现步骤见
[`docs/benchmark.md`](../docs/benchmark.md) M1，注册脚本 `evals/benchmarks/bfcl_syscity.py`。
