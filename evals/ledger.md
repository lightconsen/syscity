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
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | 同上 | 27f9bcb6 | simple_python | 366/400 | 91.50% | 复跑：`temperature` 透传修复后 t=0.001 真正生效（首轮被丢弃、实际 0.7）。7 条翻对、8 条翻错 |
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | 同上 | 27f9bcb6 | irrelevance | 175/240 | 72.92% | 65 条仍是 `irrelevance_error:decoder_success`。17 条翻对、14 条翻错 |
| 2026-10-26 | BFCL | bfcl-eval 2026.3.23 | 同上 | 27f9bcb6 | multi_turn_base | 132/200 | 66.00% | 22 条翻对、13 条翻错；`instance_state_mismatch` 63→60、`execution_response_mismatch` 8→4、`empty_turn_model_response` 6→4 |

**两轮合计**：662/840 = 78.81% → 673/840 = 80.12%，+11 条 ≈ 0.9 个标准误。结论是
**temperature 不是失分主因**；逐题比对同时给出了**噪声地板**——即使 t=0.001 近贪婪解码，
两轮仍有 15/400、31/240、35/200 条判定翻转，即单轮类目分数自带 ±2–4pt 抖动。因此本表
两轮之间的差值不可当作"改进"，且今后任何 A/B 都需重复 3 轮以上再下判断。

**配对子集探针（不进上表）**：子集 = 已知错题 + 等量随机对照，绝对分数被构造成 50%，只用
"修好 / 弄坏"计数。`deepseek-v4-pro` 对 `deepseek-v4-flash`：`irrelevance` 18 修好 / 1 弄坏，
`simple_python` 17 修好 / 2 弄坏；JSON Schema 类型名规整（`75275de8`）对 `simple_python`
11、12 修好 / 各 1 弄坏（两次同向），对 `irrelevance` 12/5 与 8/7（噪声）。**2026-10-26 决定：
默认模型保持 flash**——差距真实，但默认模型是成本/延迟决策；全量 × pro 未跑完，探针足够。

口径：`raw` 模式，网关只做 provider 透传、工具由 BFCL 自己执行，所以这组数字衡量的是
"默认模型 + OpenAI 兼容线格式"，**不是** syscity 内置 Agent 循环的水平。复现步骤见
[`docs/benchmark.md`](../docs/benchmark.md) M1，注册脚本 `evals/benchmarks/bfcl_syscity.py`。
