# Eval Ledger

Durable record of eval suite runs — one row per completed
`syscity eval run` (appended automatically, best-effort).

Why a file: files and git history survive context compaction and CI log
expiry; conversation memory does not. When judging "which version is best",
compare against this ledger, not memory. See `docs/harness.md` §8.

| Date | Commit | Suite | Executor | Judge | Mode | Passed | Rate | Gate | Failed tasks |
|------|--------|-------|----------|-------|------|--------|------|------|--------------|
| 2026-10-27 02:25 | cc03c7b7 | GAIA Level 1 (validation) | deepseek/deepseek-v4-flash | none | full | 112/159 | 70.4% | PASS | gaia_l1_0383a3ee, gaia_l1_1f975693, gaia_l1_23dd907f, gaia_l1_305ac316, gaia_l1_3cef3a44, gaia_l1_46719c30, gaia_l1_4b650a35, gaia_l1_4b6bb5f7, gaia_l1_5d0080c… |
| 2026-10-27 10:55 | 448bd3d8 | GAIA Level 1 (validation) | deepseek/deepseek-v4-flash | none | only:2 | 1/2 | 50.0% | PASS | gaia_l1_9318445f |
| 2026-10-27 14:01 | b7922116 | GAIA Level 1 (validation) | deepseek/deepseek-v4-flash | none | only:1 | 1/1 | 100.0% | PASS | - |

## 备注：被剔除的运行

新行由 `append_eval_ledger` 插在本节之前，所以上表可以继续向下追加；本节只记
"跑了但没有测量的运行"，它们不进表，免得被当成分数。

**被剔除的一次运行（2026-10-27 14:53）**：同 commit 的 P0–P3 验收全量跑在 06:48Z 撞上
provider 余额耗尽（DeepSeek HTTP 402 `insufficient_balance_error`），159 个 trial 里有
148 个是瞬时失败、零 token（日志 148 条 402）。只有前 4 个任务有模型参与：
`0383a3ee` 0%、`11af4e1a` 100%、`1f975693` 100%、`23dd907f` 33.3%（该题第 3 个 trial
开始 402），其余 49 个任务全部零通过。产出行 `7/159 = 4.4%` 已从表中删除——它衡量的是
账单，不是能力。harness 已加闸：连续 3 个任务的所有 trial 都零 token 时直接报错退出、
不写 ledger 行（`src/eval/standalone.rs` 的 `provider_silent`），所以同样的故障下次会在
第 7 个任务前后停下，而不是安静跑完 53 个。**验收跑需在充值后重跑**：
`syscity eval run gaia_l1 --full --trials 3 --collect-badcases`。

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

### GAIA Level 1（validation，53 条 × 3 trials）

| Date | Benchmark | Version | Executor | Commit | Trials | Passed | Rate | Notes |
|------|-----------|---------|----------|--------|--------|--------|------|-------|
| 2026-10-26 | GAIA L1 | validation（HF `gaia-benchmark/GAIA`） | syscity 内置 Agent 循环 → deepseek-v4-flash | cc03c7b7 | 159 | 112 | 70.4% | 29/53 任务 3 次全过，45/53 至少过 1 次，8/53 全挂；3h39m、49.8M token |

口径：与 BFCL 相反，这里跑的是 **syscity 自己的 Agent 循环**（工具由 syscity 执行，
`file_read` / `shell` / `web_search` / `web_fetch` / `browser` 等），判分是 GAIA 官方
`question_scorer`。所以它衡量的是整条栈：系统提示、工具描述、循环终止条件、模型。

**读法**：同一题的 3 次 trial 高度相关（同一份 prompt、同一个模型），所以 70.4% 的
trial 口径不是独立样本；稳定的读数是任务口径——**3 次全过 54.7%，至少过 1 次 84.9%**。
这个数字**不能和公开 leaderboard 直接比**：GAIA 的分数对 scaffolding（工具集、重试、
迭代上限、是否允许多次提交）高度敏感，换一套 harness 就是另一个数。它的用途是给本仓库
自己定基线，看改动是升还是降。

失分结构（24 个任务至少挂 1 次：8 个全挂、7 个 1/3、9 个 2/3）：

- 带附件的 11 条反而更高（26/33 = 78.8%），纯文本 42 条是 68.3%（86/126）。瓶颈不在
  读文件，而在多跳检索 + 计算：8 个全挂任务里 7 个是"查网页 → 再查 → 计算/比对"型。
- 第 8 个是图片题（`9318445f`）：`image` 工具只返元数据，没有把图像交给模型的通道，
  agent 只能 `shell` 试 OCR，3 次全挂。评测工具集的已知缺口，带图条按低估计。
- 6 次 trial 撞到 30 轮工具上限（5 挂 1 过）；上限本身不是主因，但长搜索链会吃掉预算。
- 工具用量：`shell` 462 次、`web_fetch` 293 次、`browser` 118 次成功调用——`browser`
  在 L1 是必需项（JS 渲染、点击后才出现的文件），没有它这批题会系统性失分。
- **已知环境偏差**：`shell` 用生产白名单环境（`env_clear` + PATH/HOME 等），不含
  `http_proxy`；本机对部分站点必须走代理（实测 `example.com` 直连 200，但 `archive.org`、
  `bbc.co.uk` 直连 20s 超时，走 127.0.0.1:1087 分别 429/200）。因此 shell 里的
  `curl` 下载类路线会挂：4 个失败 trial 出现 exit 28（`23dd907f` 2 次、`4b6bb5f7` 1 次、
  `99c9cc74` 1 次），另有 4 个 trial 同样超时但改走 `web_fetch`/`browser` 后通过。
  `web_fetch`/`browser` 走进程环境带代理，能兜住一部分；要彻底剥掉这部分偏差，得在
  shell 能直连（或注入代理）的机器上复跑。

复现：`syscity eval run gaia_l1 --full --trials 3`（任务文件按 `docs/benchmark.md` §M2
在本机生成）。
