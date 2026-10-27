# 公开基准（Benchmark）接入计划

> 状态：M0（契约补全）、M1（BFCL）、M2（GAIA 接入，基线 70.4% → P0–P3 后 78.0%，
> 答案格式契约已在 9 题子集上验证）已落地；M3–M4 仍是计划。
> 最后更新：2026-10-28
> 相关文档：[`harness.md`](harness.md)（评测闭环设计）、[`eval-status.md`](eval-status.md)（方法论与落地状态）、
> [`../evals/README.md`](../evals/README.md)（本地套件用法）

本文回答一个问题：**如何把 τ²-bench、GAIA、BFCL 之类的公开基准接进来，做到通用、自动、不依赖人工判读。**

结论先给：通用性不来自"为每个基准写一套集成"，而来自**一个最小的接口契约**——把
syscity 暴露成一个可被外部驱动的 agent，工具由调用方注入。契约补全后，接入新基准的
边际成本主要是胶水代码。

---

## 一、目标与非目标

### 目标

1. 外部基准能通过一个稳定接口驱动 syscity，无需改 syscity 代码。
2. 判分全部自动化，复用各基准的官方 evaluator（不自写判分器）。
3. 结果可复现（固定 seed + 固定 trials）、可比较（进 `evals/ledger.md` 基线）。
4. 能进 CI 夜跑，采样规模可控，成本可预算。

### 非目标

- **不做 leaderboard 提交门禁**。模型输出有随机性，外部网络不可控，跑分波动 ±5% 是常态；
  设成 PR 阻塞条件只会带来误报。定位是"夜跑趋势 + 发版前人工看一眼"。
- **不先做 B 类重环境基准**（WebArena / OSWorld / SWE-bench）。它们各自需要自建站点或
  VM 镜像，投入以周计，而覆盖的能力（浏览器、shell）已经能被 GAIA / Terminal-bench 部分覆盖。

---

## 二、现状盘点

### 可以直接复用的

| 资产 | 位置 | 用途 |
|------|------|------|
| 无头 Agent 运行器 | `src/eval/standalone.rs:111` `run_suite` | 不启 daemon 就能跑完整 Agent 循环 |
| 工具注册表构造 | `src/eval/standalone.rs:773` `create_eval_tool_registry` | 评测用工具集的唯一边界 |
| 套件 YAML + 条件检查 | `evals/`、`src/goal/condition.rs` | setup / cleanup / 退出码 / 文件 / 模式断言 |
| 套件注册表 | `evals/suites/registry.yaml` | 按类别组织、权重、min_pass_rate、trials |
| trials / 采样 / badcase | `syscity eval run --trials --sampling-rate` | 统计口径已经统一 |
| 基线对比 | `./evals/run.sh compare <baseline>`、`evals/ledger.md` | 趋势记录 |
| OpenAI 兼容入口 | `src/gateway/lifecycle.rs:1639` → `src/gateway/handlers/openai.rs` | A 类基准的接入点（M0 已补齐） |

### M0 之前的关键缺口（已全部补齐）

`/v1/chat/completions` 曾是"单轮聊天代理"，不足以驱动任何 A 类基准。下表是补齐前的状态，
留作背景；当前行为以 §四 M0 与 [`protocol.md`](protocol.md) 的 `mode` 表为准。

| 缺口 | 原位置 | 后果 |
|---|---|---|
| 请求体不解析 `tools` / `tool_choice` | `src/gateway/types.rs`（`OpenAiChatRequest` 只有 `model`/`messages`/`stream`） | serde 静默丢弃，模型永远拿不到工具定义 |
| 只取最后一条 user message | `src/gateway/handlers/openai.rs` | 多轮历史丢失，τ² 这类基准无法对话 |
| 响应无 `tool_calls` | `src/gateway/types.rs`（`OpenAiResponseMessage` 只有 `role`/`content`） | 调用方拿不到工具调用，只能拿到最终文本 |
| 无单步/无状态模式 | `handlers/openai.rs`（直接 `AgentCommand::ProcessMessage`） | 无法"问一句、答一句"地由 harness 控制节奏 |

---

## 三、设计：一个契约，两类接入

### 3.1 契约

所有 A 类基准（BFCL、τ²-bench、AppWorld）本质上都在问同一个问题：

```
输入：messages（完整历史） + tools（工具 schema 列表）
输出：assistant turn —— 要么文本，要么 tool_calls
约定：工具由调用方执行
```

这正是 OpenAI Chat Completions 的形状。**所以不新增端点**，只把已有的
`/v1/chat/completions` 补成它该有的样子。这同时符合 CLAUDE.md 的 REST 例外条款
（"外部工具硬编码这些路径"）。

### 3.2 两类接入

```
                        ┌───────────────────────────────┐
   A 类（工具调用型）    │  benchmark harness            │
                        │  （自带 user simulator/判分）  │
                        └──────────────┬────────────────┘
                                       │ OpenAI 兼容 HTTP
                                       ▼
                        ┌───────────────────────────────┐
                        │  syscity /v1/chat/completions │
                        │  ├─ mode=raw   透传 tools     │
                        │  └─ mode=agent 内置循环       │
                        └───────────────────────────────┘

   B/C 类（任务完成型）  外部任务 JSON ──importer──▶ evals/capability/*.yaml
                                                 （复用现有 runner + 官方 scorer）
```

- **出站（A 类）**：benchmark 把 syscity 当模型端点调。需要 M0。
- **入站（B/C 类）**：写 importer 把外部任务转成本地 suite。不需要 M0，但只能单轮。

---

## 四、里程碑

### M0 — 补全 Agent 端点契约（0.5–1 天）✅ 已完成

**这是所有自动化的前提。**

实现结果：

1. `src/gateway/types.rs`
   - `OpenAiChatRequest` 增加 `tools: Vec<ToolDefinition>`、`tool_choice: Option<Value>`
     （接受但不强制）、`mode: Option<OpenAiMode>`。缺省规则见
     `effective_mode()`：**带 `tools` 缺省 `raw`，不带 `tools` 缺省 `agent`**，显式 `mode`
     优先。补这条是因为 BFCL 这类 harness 只发 `tools`、不发 `mode`——最初把缺省定成
     `agent`，M1 一开始就撞上了"工具被静默丢弃"。
   - `OpenAiMessage` 的 `content` 改为 `Option<String>`，并增加 `tool_calls` / `tool_call_id` /
     `name` —— 否则 `"content": null` 的 assistant 轮和带 `tool_call_id` 的 tool 轮都解析失败。
   - `OpenAiResponseMessage` 增加 `tool_calls`，`content` 改为 `Option<String>`（纯工具调用轮
     输出 `null`）；新增 `OpenAiToolCall` / `OpenAiFunctionCall`（`"type": "function"`）。
2. `src/gateway/handlers/openai.rs`
   - 按 `mode` 分派：`raw_completion()` / `agent_completion()`（原逻辑原样保留）。
   - `raw` 不走 Agent、不要求存在 default agent；完整 `messages` 与 `tools` 交给
     `model_router.complete()`，`tool_calls` 原样回填。`finish_reason` 以"是否有工具调用"为准，
     不信任 provider 在工具轮返回的 `"stop"`。
   - `model` 支持哨兵值 `syscity-agent` / `syscity` / `default` → 网关默认模型。
   - `raw` + `stream: true` 返回 400（工具调用的 SSE 分片暂不实现）。
   - 请求里出现未知 `role` 返回 400，而不是静默丢弃该轮。
3. 鉴权：`/v1/chat/completions` 已在认证 tier，`raw` 同样花运维方 key，见
   `middleware.rs:512-531` 的说明。
4. 测试（`src/gateway/handlers/openai.rs` tests，11 条）：
   - `raw_mode_passes_tools_through_and_returns_tool_calls` — 端到端：mock provider 收到
     caller 的 `tools` 与完整历史，响应含 `tool_calls` 且 `finish_reason == "tool_calls"`
   - `tool_turns_deserialize_from_the_openai_wire` — `content: null` + `tool_call_id` 往返
   - `provider_messages_keep_tool_calls_and_results` — 多轮不丢工具结果
   - `response_tool_calls_use_the_openai_type_field` — 线格式（`type`、arguments 为字符串、
     不泄漏 `call_type` / `result`）
   - `mode_defaults_to_agent`、`mode_defaults_to_raw_when_tools_are_present`、
     `explicit_mode_wins_over_the_tools_default`、
     `agent_mode_response_omits_tool_calls`、`no_default_agent_returns_503`
     — 缺省行为不变（回归）
   - `raw_mode_does_not_need_a_default_agent`、`raw_mode_rejects_streaming`、
     `raw_mode_rejects_unknown_roles`

**验收**：原计划是 `curl` 打通。实际用进程内 handler 测试覆盖了同一条路径（真实
`ModelRouter` + mock provider），比 curl 更严格，也不需要起网关。真正的对外验证留给 M1。

### M1 — BFCL（0.5 天）✅ 已打通

选它作第一步，因为它**只验证 M0 是否正确**，不引入任何环境。

实际做法（脚本已入库：`evals/benchmarks/bfcl_syscity.py`，文件头有完整用法）：

1. 隔离安装 `bfcl-eval`（`~/.cache/syscity-bench/bfcl`），`BFCL_PROJECT_ROOT` 指到仓库外，
   跑分与结果都不入仓。PyPI 包漏声明了 `soundfile`（经 `qwen_agent` 间接 import），
   不装则 `bfcl` 命令直接 import 失败。
2. `bfcl_eval` 只认它自己 `model_config.py` 里注册过的模型，自建网关必须先把条目注入
   `MODEL_CONFIG_MAPPING`。`bfcl_syscity.py` 就做这一件事，之后原样转交上游 CLI，
   所以打印出来的数字就是官方 evaluator 的结果。
3. `OPENAI_BASE_URL` 指向网关，`model` 填 `syscity-agent`（哨兵值 → 网关默认模型），
   `tools` 自动触发 `raw` 模式。

过程中被基准逼出来的两个约定：

| 现象 | 原因 | 处理 |
|---|---|---|
| 工具定义被静默丢弃，每轮只回文本 | harness 只发 `tools`、不发 `mode` | 缺省规则改为"带 `tools` 即 `raw`"（见 M0） |
| `math.factorial` 明明调对了却判错 | OpenAI 线格式不许函数名带 `.`，BFCL 发的是 `math_factorial`；checker 默认按带点的名字找 | 注册条目里 `underscore_to_dot=True` |

**验收**：官方 evaluator 输出 accuracy 报告，无需人工看任何一条输出。✅

两轮结果（2026-10-26，`bfcl-eval 2026.3.23`，`syscity-agent` → deepseek-v4-flash，
840 条全部跑完、无人工干预）。第二轮是 `temperature` 透传修复（`27f9bcb6`）后
用同一批题重跑，用于把"契约缺陷"与"模型能力"分开：

| 子类目 | 首轮 `0c362599`（t 被丢弃 → 实际 0.7） | 复跑 `27f9bcb6`（t=0.001 生效） | 变化 |
|---|---|---|---|
| `simple_python` | 367/400 = 91.75% | 366/400 = 91.50% | −1 |
| `irrelevance` | 172/240 = 71.67% | 175/240 = 72.92% | +3 |
| `multi_turn_base` | 123/200 = 61.50% | 132/200 = 66.00% | +9 |
| 合计 | 662/840 = 78.81% | 673/840 = 80.12% | +11 |

**结论：温度不是失分主因。** 合计 +11 条在 840 条样本上约 0.9 个标准误，单类目最明显的
`multi_turn_base` +9 也只有 1.3 个标准误——不构成统计上可分辨的改进。失分结构也印证这一点：
`simple_python` 的两轮错误构成基本不变（仍是参数值与类型判定），`irrelevance` 的 65 条
仍然是"不该调工具时调了"。

**测到的噪声地板（比上述差值更重要）**：逐题比对两轮判定，翻转的题数是

| 子类目 | 由错转对 | 由对转错 | 净变化 |
|---|---|---|---|
| `simple_python` | 7 | 8 | −1 |
| `irrelevance` | 17 | 14 | +3 |
| `multi_turn_base` | 22 | 13 | +9 |

即在 `temperature=0.001`（几乎贪婪解码）下，仍有 15/400、31/240、35/200 条判定翻转——
说明抖动主要不来自采样，而来自 provider 侧的批处理/专家路由与多轮对话的分叉。**单轮运行的
类目分数自带约 ±2–4 个百分点的抖动**，所以今后任何 A/B（换模型、改提示词、改工具描述）
都必须重复 3 轮以上或扩大到全类别，否则小于 4pt 的"提升"没有意义。这条规则和
`evals/ledger.md` 的"外部基准"一节一起，是给下一轮调优定的口径。

读法见 `evals/ledger.md` 的"外部基准"一节：`raw` 模式测的是"默认模型 + OpenAI 兼容线格式"，
不是 syscity 内置 Agent 循环的水平。`simple_python` 的失分几乎全在参数值（类型/枚举），
`irrelevance` 的失分全部是"不该调工具时调了"——两者都是模型行为的基线，与网关实现无关，
今后正是拿来对照"换模型 / 改提示词有没有变好"的那个数。

### 已做的两个探针（附带结论）

全量跑一轮 640–840 条要 1.5 小时，所以调优用**配对子集**：取该轮已判错的题 + 等量随机通过
的题作对照，同一批题在新配置下重跑，只看"修好 / 弄坏"的条数，不看绝对分数（子集被构造成
50% 失败率，绝对分数没有意义）。

| 探针 | 子集 | 修好 | 弄坏 | 结论 |
|---|---|---|---|---|
| JSON Schema 类型名规整（`75275de8`） | `simple_python` 34 错 + 34 对照，跑两次 | 11、12 | 1、1 | 有效：两次同向，非对称抖动 |
| 同上 | `irrelevance` 65 错 + 65 对照，跑两次 | 12、8 | 5、7 | 无效，落在噪声内 |
| 换 `deepseek-v4-pro` | `simple_python` 34 错 + 34 对照 | 17 | 2 | 明显更强 |
| 同上 | `irrelevance` 65 错 + 65 对照 | 18 | 1 | 明显更强 |

外推：pro 在 `irrelevance` 上约 72.9% → 80%，`simple_python` 约 91.5% → 95%，代价是单请求
~8s（flash ~1.5s）与更高单价。

**决定（2026-10-26）：默认模型保持 `deepseek-v4-flash`。** 分数上的差距是真的，但默认模型是
成本/延迟决策，不是基准决策；pro 作为可选模型保留在 `[providers.deepseek]` 里，用户可自行
`syscity model switch`。因此全量 × pro 的正式数字没有跑完，探针结论足够回答"值不值"。
要重开这个决定，用上表的配对子集重测，不要凭单轮全量分数。

### M2 — GAIA Level 1（✅ 已接入，基线 70.4%）

单轮任务 + 官方 scorer，是"端到端能不能干完活"最直接的可比数字。

落地结果（相对计划改了两处，理由见下）：

1. `evals/importers/gaia.py` — 读 HF 的 `2023/validation/metadata.level1.parquet`
   （validation 的 Level 1，53 条），生成 `evals/capability/gaia_l1.yaml` 和
   `gaia_l1_answers.json`，并把题目引用的附件下载到仓库外
   （默认 `~/.cache/syscity-bench/gaia/files`）。**两个生成文件都不入仓**：GAIA 的使用
   条款禁止再分发，附件路径又是本机绝对路径。任务 id 取 `gaia_l1_<task_id 前 8 位>`，
   重跑后仍能对回官方 `task_id`。
2. `evals/importers/gaia_scorer.py` — 官方 `question_scorer`，取自 GAIA leaderboard
   Space 的 `scorer.py`（2026-10-26），逐行保留，只删掉上游未使用的 `numpy` import，
   让裸 Python 也能跑。**没有自写判分逻辑。**
3. `evals/importers/gaia_score.py` — 退出码映射（命中 0、未命中 1、无法判分 2；2 单独
   分出来，是为了不让"harness 坏了"被记成"模型答错"），外加**答案抽取**：每条题目的
   prompt 都要求以 `FINAL ANSWER: <答案>` 结尾，包装层取该行交给官方 scorer；没有标记时
   先试最后一行（去掉 `The answer is` / `答案：` 之类引导语后再试一次），最后才整段回退，
   每个候选都交给同一个官方 scorer，命中即过。抽取不是判分，但它决定了喂给 scorer 的字符串，
   所以对 53 条一视同仁；原始回复写在 `${trial_dir}/response.txt`（trial 目录在系统临时
   目录下，跑完即弃；要留证据用 `--collect-badcases`，失败轨迹会落到 `evals/badcases/`）。
   `--self-test` 不需要数据集就能验证 vendoring 与抽取。
4. `evals/suites/gaia_l1.yaml`（清单，入仓；`trials: 3`、`min_pass_rate: 0.0`，首轮只做
   基线不定阈值）与 `evals/suites/registry.yaml` 的 `benchmark` 分组。
5. 评测工具注册表补上 `browser`（`src/eval/standalone.rs` 的
   `create_eval_tool_registry`，与网关同一 SSRF 策略）。`file_read`、`shell`、`web_fetch`、
   `web_search` 本来就在。PDF 走 `shell`（`pdftotext`）——`file_read` 不解析二进制，这是
   已知限制。图片条目走 `image` 工具的本地 OCR（见下），不再只有元数据。
6. **评测工具集在无人值守下预批准**（`eval_permissions`）。第一次真跑就撞上了这一点：
   无头评测没有人类在审批队列另一头，`shell`、`file_write`、`browser` 这类
   `requires_approval` 工具会 fail closed，日志里是
   `Tool 'shell' denied: … no human is present to grant it`。若不修，测的是本机
   `[permissions]` 配置而不是模型——正是上面第 5 条要避免的那类系统性失分。
   规则**由注册表自身导出**（`registry.list()` → 每条一个 allow 规则），而不是另写一份
   工具名清单，所以增删工具不会和权限悄悄脱节；未注册的工具名字照样不可达。
   用户自己的 `~/.syscity/config.toml` 不参与评测运行，也不会被改写。

数据集是 gated 的，所以任务文件在本机生成。用一个已在
<https://huggingface.co/datasets/gaia-benchmark/GAIA> 点过授权的账号，token 走
`HF_TOKEN`（或 `--token`）传给导入器，然后：

```bash
python3 -m venv ~/.cache/syscity-bench/gaia/venv
~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow pyyaml
HF_TOKEN=... ~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py
syscity eval run gaia_l1 --full --trials 3
```

`pyarrow` 是读 parquet 的最低要求（Python 没有标准库实现）；导入器对 401/403 会直接
打印授权地址，不会静默产出空套件。token 只经环境变量传入，本机的凭据文件在仓库根
（`hf.secret`，被 `*.secret` 规则 gitignore），不入仓。

**基线（2026-10-26，`deepseek-v4-flash`，内置 Agent 循环，53 条 × 3 trials = 159）**：

| 指标 | 数值 |
|---|---|
| trial 通过 | **112/159 = 70.4%** |
| 3 次全过的任务 | 29/53 = 54.7% |
| 至少过 1 次的任务 | 45/53 = 84.9% |
| 3 次全挂的任务 | 8/53 |
| 耗时 / 成本 | 3h39m，49.8M token（约 313k/trial，含每轮重传的上下文） |

trial 口径不是独立样本（同题 3 次共享 prompt 与模型），**稳定读数是任务口径的 54.7% /
84.9%**；70.4% 用于和其他 harness 的汇总口径对齐，不要拿它做小样本 A/B。分数也不能与
公开 leaderboard 直接比——GAIA 对 scaffolding（工具集、迭代上限、重试）极敏感。
P0–P3 之后同口径复跑为 78.0% / 62.3% / 94.3%，对照见本节末尾的「验收结果」。

失分结构（24 个任务至少挂 1 次：8 全挂、7 个 1/3、9 个 2/3）：

- 带附件的 11 条 **78.8%**（26/33）高于纯文本 42 条的 68.3%（86/126）。瓶颈不是解析文件，
  而是多跳检索加计算——8 个全挂任务里 7 个是"查网页 → 再查 → 算/比"型。
- 剩下那个全挂任务是**图片题**（`9318445f`，读图里的分数）：`image` 工具只返回尺寸/格式
  等元数据，并没有把像素交给模型的通道，agent 只能拿 `shell` 试 OCR（Swift/Apple Vision、
  tesseract 均不可用），3 次全挂。这不是模型视力问题，是评测工具集缺一条"看图"通道，
  在补上之前，带图条的分数按低估计。
- 6 次 trial 撞到 30 轮工具上限（5 挂 1 过）。上限本身不是主因，但长搜索链会吃掉预算。
- 调用量：`shell` 462、`web_fetch` 293、`browser` 118（均成功调用）。`browser` 在 L1
  不是可选项——有题的文件只有点击/JS 渲染后才出现。
- **已知环境偏差**：`shell` 用生产白名单环境（`env_clear` + PATH/HOME 等），不含
  `http_proxy`；本机对部分站点必须走代理（`example.com` 直连可通，`archive.org` /
  `bbc.co.uk` 直连 20s 超时，代理下 429 / 200）。于是 shell 里的 `curl` 下载路线会挂：
  4 个失败 trial 出现 exit 28（`23dd907f` 2 次 —— 该题 3 次全挂，`4b6bb5f7`、`99c9cc74`
  各 1 次），另有 4 个 trial 同样超时但改走 `web_fetch`/`browser` 后通过。换到 shell 能
  直连（或注入代理）的机器复跑，可以把这部分偏差剥掉——**2026-10-27 起父进程的代理变量会
  自动注入 shell 工具环境**，复跑时在启动 eval 的 shell 里 `export` 即可。
- 用户级 personality memory 目录为空，未混入 prompt；系统提示来自本机
  `default_agent` 配置（评测附加了一段工具使用指引），换机器复跑前应对一下配置差异。

**提分改造（2026-10-27，P0–P3）**

基线里 84.9% 的题至少过过一次、只有 54.7% 三次全过，说明**主要缺口是可靠性，不是能力**。
这一轮按这个判断改了四组东西，判分口径未动（官方 scorer 仍是唯一裁判）：

- **P0 确定性**：评测运行固定 `temperature = 0.0`（此前 0.7）；工具预算 30 → 60 轮，并在用掉
  70% 时给一条"该收口"的提醒；预算耗尽时的合成结果明确要求按题面格式作答；系统提示改为由
  注册表导出**真实**工具名（此前写死了五个并未注册的工具名），并说明 shell 环境受限、联网优先
  用 `web_fetch`。父进程的 `http_proxy`/`https_proxy` 现在注入 shell 工具环境
  （`AgentConfig.tool_environment` → `ToolContext`），curl 路线不再因这个环境偏差失分。
- **P1 检索质量**：`web_search` 新增 `depth=advanced`（Tavily 走 advanced + raw content，一次
  拿到正文）和 `fetch_top=N`（一次调用把前 N 个结果页正文读回来，复用 `web_fetch` 的 SSRF
  守卫与 HTML→markdown），`limit` 上限 10 → 20；`web_fetch` 对超时/连接失败/5xx 重试一次；
  JS 渲染的空壳页面会提示改用 `browser`，而不是让模型以为页面没内容。
- **P2 能力缺口**：`image` 工具不再只返回元数据——开着 `vision` 特性时用本地 RapidOCR 读出图
  上文字并按阅读顺序返回（模型自动缓存到 `~/.syscity/models/vision/`，下载很轻，不含
  OmniParser）。基线里唯一一个非检索类零通过任务就是图片题。这一轮顺带把 OCR 自身修到能用：
  - 识别模型换成 PP-OCRv3 English。v5 mobile 在本 crate 链接的 ONNX Runtime 上会把一行干净
    的 `Invoice total: 1234.50 USD` 读成 `In)Ce tOtal: 1234.5O USD`（同一输入在 Python 侧
    的各版本 ORT 上逐位一致），v3 在同一个 runtime 上退化成 `1234.5o USD`，可以用。
  - 字符表与模型绑定并从磁盘加载：识别模型输出的是类别下标，用猜的字母表解码只会得到
    "自信的乱码"。类别数与字符表不匹配时直接报错，而不是继续解出碎片。
  - DBNet 的框改成按 `unclip_ratio · area / perimeter` 各边外扩（原来是宽高各乘 1.5，长行
    会被横向拉长、纵向压扁，识别裁剪把字切掉）；识别输入宽度上限 1600 → 4096，长行此前
    被压扁是文档截图读错的主因——同一张 GAIA 截图整图置信度 0.75 → 0.80，正文从乱码变成
    可读（`3/4`、`1/4`、`2/4`、`5/35`、`7/21`、`30/5` 全部读对）。
  - **`9318445f` 从"必挂"变成"能过但不稳"**：单题实跑两次分别是 1/2（`448bd3d8`）与 1/1
    （`b7922116`），但 P0–P3 后的全量跑里 3 次全挂。三次的 OCR 读数各不相同，最好的一次是
    `3/4,1/4,…,30/5,3/4,1/15,…`——只比标准答案少一个重复的 `30/5`。也就是说"读得出"这关
    基本过了，"读全、按序、不漏重复项"还不过关。题面要读的是竖式分数（分子/横线/分母），
    检测会把它们拆成独立小块，10px 高的数字仍会连 `8`/`0`、`340`/`30` 都分不清，且答案要
    "每个分数 + 化简结果"，需要判断而不只是识字——这是"文档截图 OCR"与"看懂图表"的边界。
    稳定解要靠多模态通道（把图片交给模型），属产品决策。附件（xlsx/docx/pptx/mp3）解析
    不是瓶颈（带附件条目 78.8% > 纯文本 68.3%），本轮未改。换模型（`deepseek-v4-pro`）的
    收益已由前面的配对探针量化，默认模型仍保持 flash，理由见上文。
- **P3 度量**：suite 摘要增加任务口径——三次全过 / 至少一次 / 零通过（附任务清单）。trial
  口径会把"1/3 通过"和"0/3 通过"混在一起，而这两类要修的东西完全不同。另外加了一道
  **outage 闸**：连续 3 个任务的所有 trial 都零 token 时直接报错退出，不写 ledger 行。
  这是被一次真实事故逼出来的——2026-10-27 14:53 的验收跑撞上 DeepSeek 余额耗尽
  （HTTP 402 `insufficient_balance_error`），148/159 个 trial 是瞬时失败、零 token，
  汇总却安静地报出 `7/159 = 4.4%`。那次结果已从 `evals/ledger.md` 剔除。

**验收结果（2026-10-27，P0–P3 后全量复跑）**：53 条 × 3 trials 全部跑完，无 provider
故障（0 次 402），3h09m / 58.6M token。

| 指标 | 基线 2026-10-26 | P0–P3 后 2026-10-27 | 差 |
|---|---|---|---|
| trial 通过 | 112/159 = 70.4% | **124/159 = 78.0%** | +7.6pt |
| 3 次全过的任务 | 29/53 = 54.7% | **33/53 = 62.3%** | +4 题 |
| 至少过 1 次的任务 | 45/53 = 84.9% | **50/53 = 94.3%** | +5 题 |
| 3 次全挂的任务 | 8/53 | **3/53** | −5 题 |
| 耗时 / 成本 | 3h39m、49.8M token | 3h09m、58.6M token | −30min、+18% token |

逐题配对（同一批 53 题）：**13 题变好、5 题变坏**，配对符号检验单侧 p = 0.048——边缘
显著，且样本只有 53 题，因此按"提升成立但不大"记账，不要按 +7.6pt 精确外推。trial 口径
的 Wilson 95% 区间是 70.9%–83.7%（基线 62.9%–77.0%），两段区间仍重叠。

变好的 13 题里有 5 题是 0%→33% 以上（`0383a3ee`、`23dd907f`、`5d0080cb`、`c365c1c7`、
`e1fc63a2`），正对应 P1 的检索改动（`fetch_top` 一次拿回正文、`depth=advanced`）。

**3 个全挂任务的失败性质**（看 `evals/badcases/`，不是同一类问题）：

| 任务 | 现象 | 归类 |
|---|---|---|
| `7d4a7d1d` | 3 次都答 `22 years`，标准答案是 `22` | **答案格式**：值对了，多带单位，官方 scorer 按数字比对直接不认 |
| `9318445f` | 3 次 OCR 读了三个版本；最好的一次是 `3/4,1/4,…,30/5,3/4,1/15,…`，只比标准答案少一个重复的 `30/5` | **OCR 覆盖度**：整体已读对，差在重复项/顺序 |
| `46719c30` | 3 次都答 `A New Software Agent 'Learning' Algorithm`，标准答案是 `Mapping Human Oriented Information to Software Agents for Online Systems Usage` | **检索**：找到的是同主题的另一篇论文 |

这与基线"7/8 全挂都是多跳检索"的结构不同：检索类只剩 1 个，另外 2 个是格式与 OCR。

**答案格式那一轮（2026-10-28）**：把 28 条失败回复逐条交给官方 scorer 复核，发现损失
主要不在能力而在包装——6 条只要去掉括号补充或破折号后的解释就能过
（`Rockhopper penguin (Eudyptes chrysocome)` → `Rockhopper penguin`、
`No — Earl cannot do it; …` → `No`、`Research (as in "No original research")` →
`Research`），另有 4 条是"数值 + 单位"型（`22 years`、`90 participants`、`0.1777 m³`、
`INT. THE CASTLE - DAY`）。于是把其中 9 道挂过的题做成题集，先改提示再验证：评测提示里
新增 `## Final Answer Format`，明确 `FINAL ANSWER:` 之后由自动判分器读，只放题面要的值：
不带单位（`22` 而非 `22 years`）、不带解释、不带括号补充、列表按题面要求的顺序与分隔符
且**重复项要保留**；题面自带格式时以题面为准。判分器不动。

9 题 × 3 trials 的前后对比（同一批题，`--only` 跑）：

| 任务 | 改前 | 改后 |
|---|---|---|
| `7d4a7d1d` | 0% | **100%** |
| `a0068077` | 33% | **100%** |
| `9d191bce` | 33% | **100%** |
| `5cfb274c` | 67% | **100%** |
| `5d0080cb` | 67% | **100%** |
| `0383a3ee` | 67% | **100%** |
| `935e2cff` | 33% | 67% |
| `23dd907f` | 33% | 33% |
| `4b6bb5f7` | 33% | 33% |
| **合计** | **40.7%** | **81.5%** |

7 题变好、0 题变差，trial 口径 22/27 = 81.5%。剩下 5 个失败 trial 逐条看过：

| 任务 | 失败 trial 的 `FINAL ANSWER` | 标准答案 | 性质 |
|---|---|---|---|
| `23dd907f` | `</｜｜DSML｜｜ calls>` | `2` | DSML 收口 bug（已修） |
| `23dd907f` | `4` | `2` | 数错了节号——读了诗但定位错，非格式 |
| `4b6bb5f7` | `INT. THE CASTLE - DAY` ×2 | `THE CASTLE` | 答了整个场景标题，题面要的是标题里的地点 |
| `935e2cff` | `</｜｜DSML｜｜ calls>` | `research` | DSML 收口 bug（已修） |

也就是说格式契约把"值对、包装错"这一类基本清掉了：剩下的两个 DSML 是收口轮缺守卫（已在
`strip_tool_markup` 修掉，`935e2cff` 因此下次应到 100%），一个是真的数错，只有
`4b6bb5f7` 仍属"答多了"——它把 `INT.` / `- DAY` 也带上，而题面说的是"first scene
heading 里的 setting"。这条在提示里加"不要场景前缀后缀"也未必管用，因为模型是**根本没
把标题拆成地点**，属于理解，不是包装。

注意这是**被挑出来的 9 题**，前测本来就 40.7%，不能当成全量提分；要报全量数字需要重跑
53 题。测得的结论是：格式契约有效，且没有把这 9 题弄坏。

环境偏差这一项确认修好了：基线里 shell 的 `curl` 有 4 次 exit 28（代理不通），本轮
只有 1 次；工具调用量为 `shell` 837、`web_search` 652、`web_fetch` 494、`browser` 104
（基线 462 / — / 293 / 118）——`web_fetch` 上去、`browser` 下来，与 P1 让检索工具直接
拿正文的方向一致；`shell` 变多则是 60 轮预算放宽后的探索。

复跑与留证：

```bash
export http_proxy=http://127.0.0.1:1087 https_proxy=http://127.0.0.1:1087
syscity eval run gaia_l1 --full --trials 3 --collect-badcases
```

`--collect-badcases` 把失败 trial 的输入、回复和工具轨迹落到 `evals/badcases/`（已 gitignore）；
`--only <id>,<id>` 可以只跑摘要里列出的零通过/抖动题做快速回归。**先读任务口径，再读 trial
口径**：任务口径回答"整份套件能解多少"，trial 口径回答"一次运行有多稳"。

**验收状态（2026-10-27）：已完成**，即上表 78.0%。这次跑之前曾被 provider 余额耗尽打断
（见 P3 的 outage 闸），充值后按同一条命令复跑拿到对照数字。后续任何改动仍按任务口径
（62.3% / 94.3% / 3 个全挂）加 trial 口径一起读。

### M3 — τ²-bench（3–5 天）

多轮 + 模拟用户 + 策略遵循，当前工具 Agent 最认的榜单。它自带 UserSimulator 和
数据库状态判分器，**你要写的只有 Agent 一侧**。

1. 确认它能以"模型端点"方式工作：实现它的 `Agent` 接口（方法名以仓库当前 API 为准，
   结构大致为 `get_init_state(message)` / `generate_next_message(message, state)`），
   内部就是一次 `POST /v1/chat/completions`（`mode=raw`，`tools` 来自
   `state.tools`，`messages` 为完整历史）。
2. 若 `raw` 模式够用（工具由 τ² 执行），到此为止。这是**首选路线**，一天能通。
3. 若要让 syscity 自己的循环去调它的领域工具（更接近真实形态），再走 MCP 桥：
   把 τ² 的 airline/retail/telecom 域包成 MCP server，syscity 通过 MCP 连上，
   用户消息走 WS `chat.send` 注入。多 2–4 天，收益是"测的是 syscity 而不是裸模型"。
4. 运行：

   ```bash
   python -m tau2.cli run --domain airline --agent syscity \
     --num-tasks 20 --num-trials 5 --seed 42 --output results/airline.json
   ```

**验收**：airline / retail 各 20 条 × 5 trials 自动跑完，产出官方判分结果。

### M4（可选）— Terminal-bench

对应 `shell` 工具。需要 docker，每条任务一个容器。只有在 shell 能力要单独量化时才做；
否则 M2 已经能反映"会不会用工具干活"。

---

## 五、自动化与 CI

### 分层频率

| 层 | 何时 | 规模 | 阻塞 |
|---|---|---|---|
| `ci_smoke`（现有） | 每次 PR | 3 trials × 30% 采样 | 是 |
| GAIA L1 采样 | 每夜 | 10 条 × 1 trial | 否 |
| BFCL 单档 | 每周 | simple_python | 否 |
| τ²-bench | 发版前 / 手动 | 20 条 × 5 trials | 否 |
| 全量 | 发版前 | 全跑 | 否 |

按 M2 实测折算：GAIA L1 一条 trial 约 83 秒、313k token，所以夜跑 10 条 × 1 trial ≈ 14
分钟 / 3.1M token；全量 53 条 × 3 trials 是 3h39m / 49.8M token（2026-10-26 基线）。

### 必须的三件套

1. **固定口径**：seed、trials、任务子集，写进 `registry.yaml`，不靠命令行记忆。
2. **结果落盘**：每次跑输出一份 JSON（`{bench, model, commit, date, seed, pass_rate, per_task}`），
   追加到 `evals/ledger.md`，才能用 `run.sh compare` 看趋势。
3. **成本上限**：外部基准一条任务动辄 20–50 次模型往返。夜跑用采样，并在 job 上加
   `timeout-minutes`（对齐 `.github/workflows/ci.yml` 的既有做法），避免跑飞。

### 不要做的事

- **不自写判分器**。BFCL / τ² / GAIA 都有官方 evaluator；自写的判分器往往比被测系统
  更不可靠，是这类项目最常见的坑。
- **不把跑分体积进仓库**。目前每次 trial 的输出写在系统临时目录
  （`src/eval/harness.rs:670`，`eval_<task_id>_<trial>`），跑完即弃；只有
  `--collect-badcases` 会把失败轨迹落到 `evals/badcases/`（已 gitignore），ledger 只留
  汇总行。

---

## 六、验收标准

M0 完成即视为"可自动化"达成，判断依据：

- [x] 带 `tools` 调 `/v1/chat/completions`（`mode: raw`），返回含 `tool_calls` 的响应
      —— 由 `raw_mode_passes_tools_through_and_returns_tool_calls` 覆盖
- [x] 多轮 `role: tool` 消息不丢失 —— 由 `tool_turns_deserialize_from_the_openai_wire` /
      `provider_messages_keep_tool_calls_and_results` 覆盖
- [x] 缺省行为不变（现有 OpenAI 兼容调用方无感）—— `mode_defaults_to_agent` 等回归测试
- [x] BFCL `simple_python` 能在无人工干预下跑出 accuracy 数字 —— 两轮 91.75% / 91.50%
      （367、366 / 400），同批还跑了 `irrelevance` 71.67% → 72.92%、`multi_turn_base`
      61.50% → 66.00%；两轮逐题比对给出噪声地板（±2–4pt）

M2/M3 完成视为"有对外可比的数字"达成：

- [x] GAIA L1 套件可由 `syscity eval run gaia_l1 --full` 驱动，判分走官方 scorer
      （合成数据端到端验证过）
- [x] GAIA L1 pass rate 进 ledger —— 112/159 = 70.4%，任务口径 29/53 全过、45/53 至少过 1 次
- [ ] τ²-bench airline/retail 判分结果进 ledger
- [ ] 任一基准重跑一次，差异在 ±5% 内（说明可复现）

---

## 七、风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| `raw` 模式绕开 Agent 循环，测的是"裸模型 + 工具定义" | BFCL/τ² 的分数不代表 syscity 真实水平 | 同时保留 `agent` 模式；τ² 可选走 MCP 桥（M3.3）测真实形态 |
| 基准版本漂移（BFCL v4 → v5 等） | 历史分数不可比 | ledger 记录基准版本号与 commit |
| 外部数据集/网络不稳定 | 夜跑偶发全灭 | 采样 + 失败重试 1 次；失败归因写进报告，不静默 |
| 成本超预算 | 账单 | 采样规模写进 registry，夜跑上限固定 |
| `tools` 透传面扩大攻击面 | 安全 | `raw` 模式落在认证 tier，且明确花运维方 key（见 `middleware.rs:516` 的既有说明） |

---

## 八、执行顺序

```
M0 契约补全 ✅
  └─ M1 BFCL ✅ ─────────────── 验证契约，最快拿到一个可比数字
       └─ M2 GAIA L1 接入 ✅ ── 端到端能力；基线待 HF 访问权，跑完进 ledger
            └─ M3 τ²-bench（3–5d）── 多轮 + 策略，最像真实使用的场景
                 └─ M4 Terminal-bench（可选）
```

先做 M0 和 M1。如果 M0 做不通（例如 provider 层对 `tool_calls` 有改写），后面所有
出站接入都不用谈了——这就是把它放在最前面的原因。
