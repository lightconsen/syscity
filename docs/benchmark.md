# 公开基准（Benchmark）接入计划

> 状态：M0（契约补全）、M1（BFCL）、M2（GAIA 接入）已落地；M2 的基线分数待数据集
> 访问权，M3–M4 仍是计划。
> 最后更新：2026-10-26
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

### M2 — GAIA Level 1（接入已完成，基线待数据集访问权）

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
   整段回退给 scorer，与上游行为一致。抽取不是判分，但它决定了喂给 scorer 的字符串，
   所以对 53 条一视同仁，原始回复仍留在 `${trial_dir}/response.txt` 里可回查。
   `--self-test` 不需要数据集就能验证 vendoring 与抽取。
4. `evals/suites/gaia_l1.yaml`（清单，入仓；`trials: 3`、`min_pass_rate: 0.0`，首轮只做
   基线不定阈值）与 `evals/suites/registry.yaml` 的 `benchmark` 分组。
5. 评测工具注册表补上 `browser`（`src/eval/standalone.rs` 的
   `create_eval_tool_registry`，与网关同一 SSRF 策略）。`file_read`、`shell`、`web_fetch`、
   `web_search` 本来就在。PDF 走 `shell`（`pdftotext`）——`file_read` 不解析二进制，这是
   已知限制，PDF/音频/图片类条目的失分要按此读。

数据集是 gated 的，所以任务文件在本机生成。用 `~/.cache/huggingface/token` 对应的账号
在 <https://huggingface.co/datasets/gaia-benchmark/GAIA> 点一次授权（该数据集是
auto-approve），然后：

```bash
python3 -m venv ~/.cache/syscity-bench/gaia/venv
~/.cache/syscity-bench/gaia/venv/bin/pip install pyarrow pyyaml
~/.cache/syscity-bench/gaia/venv/bin/python evals/importers/gaia.py
syscity eval run gaia_l1 --full --trials 3
```

`pyarrow` 是读 parquet 的最低要求（Python 没有标准库实现）；导入器对 401/403 会直接
打印授权地址，不会静默产出空套件。

**当前状态（2026-10-26）**：`landoyjx` 这个 HF 账号尚未拿到 GAIA 访问权，元数据请求
返回 `403 ... not in the authorized list`，所以 53 条真实题目一次都没跑，ledger 里
也就没有 GAIA 行。导入器已用合成数据跑通整条链路：生成 YAML → Rust loader 正常加载
（2 任务 × 3 trials）→ `--full` 实跑一条 → `gaia_score.py` 判分并退出 0。拿到访问权后，
缺的只有上面最后一行。

**验收**：`syscity eval run gaia_l1 --full --trials 3` 跑完，产出 pass rate 与逐条结果。

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
  （`src/eval/harness.rs:670`，`eval_<task_id>_<trial>`），跑完即弃；外部基准若需要留存轨迹，
  应显式加 `--output <path>` 写到仓库外的目录，ledger 只留汇总行。

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
- [ ] GAIA L1 pass rate 进 ledger —— 阻塞在 HF 数据集访问权，见 M2 末段
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
