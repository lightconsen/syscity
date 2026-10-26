# 公开基准（Benchmark）接入计划

> 状态：计划稿，尚未实施。
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
     （接受但不强制）、`mode: OpenAiMode`（`agent` 缺省 / `raw`）。
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
   - `mode_defaults_to_agent`、`agent_mode_response_omits_tool_calls`、`no_default_agent_returns_503`
     — 缺省行为不变（回归）
   - `raw_mode_does_not_need_a_default_agent`、`raw_mode_rejects_streaming`、
     `raw_mode_rejects_unknown_roles`

**验收**：原计划是 `curl` 打通。实际用进程内 handler 测试覆盖了同一条路径（真实
`ModelRouter` + mock provider），比 curl 更严格，也不需要起网关。真正的对外验证留给 M1。

### M1 — BFCL（0.5 天）

选它作第一步，因为它**只验证 M0 是否正确**，不引入任何环境。

1. `pip install bfcl-eval`，把 `OPENAI_BASE_URL` 指向 `http://127.0.0.1:18080/v1`，
   `model` 填 `syscity-agent`，`mode=raw`。
2. 先跑 `BFCL_v4_simple_python`（最基础的一档），确认解析通过；再跑 `multi_turn` 和
   `irrelevance`（后者能顺带暴露"不该调工具时乱调"的问题）。
3. 结果进 `evals/ledger.md`，标注日期、模型、commit。

**验收**：官方 evaluator 输出 accuracy 报告，无需人工看任何一条输出。

### M2 — GAIA Level 1（1–2 天）

单轮任务 + 官方 scorer，是"端到端能不能干完活"最直接的可比数字。

1. 新增 `evals/importers/gaia.py`：读 HF 上的 `gaia-benchmark/GAIA` validation 集，
   过滤 `Level == 1`（53 条），生成 `evals/capability/gaia_l1.yaml`。
   每条映射为：

   ```yaml
   - id: gaia_l1_0001
     input: "<question>"
     conditions:
       - type: script
         command: >-
           python3 evals/importers/gaia_score.py --task ${task_id}
           --answer ${trial_dir}/response.txt
         expected: 0
   ```

2. `evals/importers/gaia_score.py`：薄封装官方 `question_scorer`，只做 CLAUDE.md 风格的
   退出码映射（命中 0，未命中 1）。
3. 注册到 `evals/suites/registry.yaml`，`trials: 3`、`min_pass_rate` 先留空（首轮只做
   基线，不定阈值）。
4. 注意：L1 有相当比例需要下载文件/读 PDF，确认 `file_read`、`browser` 在评测工具注册表里
   （`create_eval_tool_registry`），否则这批任务会系统性失败——那测的就不是模型了。

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
- [ ] BFCL `simple_python` 能在无人工干预下跑出 accuracy 数字（M1，尚未做）

M2/M3 完成视为"有对外可比的数字"达成：

- [ ] GAIA L1 pass rate 进 ledger
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
  └─ M1 BFCL（0.5d）──────────── 验证契约，最快拿到一个可比数字
       └─ M2 GAIA L1（1–2d）──── 端到端能力，进 ledger
            └─ M3 τ²-bench（3–5d）── 多轮 + 策略，最像真实使用的场景
                 └─ M4 Terminal-bench（可选）
```

先做 M0 和 M1。如果 M0 做不通（例如 provider 层对 `tool_calls` 有改写），后面所有
出站接入都不用谈了——这就是把它放在最前面的原因。
