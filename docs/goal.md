# Goal-Based Execution (`/goal`)

`/goal` runs an agent autonomously until structured stop conditions are met.
Unlike turn-based chat where the user drives each round, the goal runner
iterates — agent acts, checks all conditions, feeds back failures, retries —
until all pass or a guardrail trips.

---

## Quick Start

```
/goal 给 src/module.rs 写测试，覆盖所有公有函数 --max-rounds 3
```

response: `🎯 Goal started: ... ID: goal_<uuid>`

Events stream back to the session WebSocket as the goal progresses.

---

## Subcommands

| Command | Description |
|---|---|
| `/goal <description> [--max-rounds N] [--fresh]` | Start a new goal; `--fresh` enables the fresh-context (Ralph) loop |
| `/goal cancel <goal_id>` | Cancel a running goal; also discards a suspended goal's checkpoint |
| `/goal list` | List running goals and suspended goals (with round and blocked reason) |
| `/goal resume <goal_id>` | Re-arm a suspended goal from its persisted checkpoint |

---

## How It Works

```
User: /goal 给 module.rs 写测试
  ↓
LLM 解析目标 → GoalPlan {
    description: "给 module.rs 写测试",
    conditions: [
      { type: "file_exists", path: "tests/module.rs" },
      { type: "exit_code", command: "cargo test" },
      { type: "numeric", command: "grep -c 'fn test_' tests/module.rs", operator: ">=", threshold: 5 }
    ],
    max_rounds: 3
  }
  ↓
GoalRunner 循环:
  round 1: agent 行动 → 检查条件 → 2/3 通过 → 反馈失败项
  round 2: agent 修复 → 检查条件 → 3/3 通过 → ✅ Done
  (或达到 max_rounds / 检测到死循环 → ❌ Aborted)
```

### Event Sequence

Each goal emits `goal.progress` gateway events with a nested `event` field:

```
goal.started → goal.retry → goal.check → (goal.done | goal.aborted)
```

- **goal.started**: goal 已创建，包含描述和条件列表（恢复执行时描述带 `(resumed, round N/M)` 后缀）
- **goal.retry**: 新一轮开始，包含上一轮反馈
- **goal.check**: 条件检查结果，含通过数/总数
- **goal.done**: 所有条件通过
- **goal.aborted**: 达到 max_rounds / 死循环 / 手动取消 / 错误 / 无效 handoff。除 `reason` 字符串外还带结构化的
  `blocked_reason: {code, message}` 字段，code 为 kebab-case：
  `loop-detected`、`max-rounds`、`agent-error`、`cancelled`、`invalid-handoff`、`fatal-config-error`

---

## Fresh-Context Mode (Ralph, `--fresh`)

`/goal <description> --fresh` runs the goal in fresh-context mode: every round
spawns a brand-new **seedless sub-agent** — system prompt plus a single user
message, with no parent conversation prefix, no session history, and no
personality/memory seeding. The workspace on disk is the only long-term
memory; the agent is instructed to persist durable findings to files.

Between rounds the only LLM-produced state carried is a bounded, strictly
validated structured handoff. The round agent must end its final reply with
exactly one fenced block tagged `handoff`:

````text
```handoff
{"status": "continue", "summary": "...", "next_steps": ["..."], "evidence": ["..."]}
```
````

Validation rules (violations fail the whole round — never truncated or guessed):

- The whole block must be ≤ 16,384 characters (`MAX_HANDOFF_CHARS`)
- `status` is one of `continue` / `complete` / `failed`
  - `continue` requires a non-empty `next_steps` array (seeds the next round)
  - `complete` requires a non-empty `evidence` array (paths, command output)
  - `failed` stops the loop for human review (aborts with `agent-error`)
- `summary` must be non-empty; unknown fields are rejected

Deterministic conditions remain authoritative: a `complete` handoff whose
conditions still fail does not end the loop — the next round sees the failed
check results as ground truth.

Fresh-context failures map to dedicated blocked reasons:

- **invalid-handoff** — the final reply had no valid `handoff` block, or it
  was over-limit/schema-invalid. Policy stop; the checkpoint is kept.
- **fatal-config-error** — missing model/provider configuration aborts loudly
  instead of being retried as a transient agent failure. Fix the config, then
  `/goal resume <id>`.

After each validated handoff the runner also writes a human-readable round
note to `<workspace>/goals/<goal-id>/round-N.md` (summary, evidence, next
steps), so long-running goal progress can be browsed from the workspace. This
is best-effort: a failed write is logged, never fatal.

---

## Condition Types

LLM 根据目标自动生成条件。支持的类型：

| Type | JSON | 检查方式 |
|---|---|---|
| `exit_code` | `{"type":"exit_code","command":"cargo test","expected":0}` | 命令退出码 |
| `file_exists` | `{"type":"file_exists","path":"tests/module.rs"}` | 文件存在性 |
| `numeric` | `{"type":"numeric","command":"wc -l","operator":">=","threshold":5}` | 命令输出数值比较 |
| `pattern` | `{"type":"pattern","command":"cat src/lib.rs","must_contain":"fn test_"}` | 输出包含字符串 |
| `static_analysis` | `{"type":"static_analysis","command":"cargo clippy -- -D warnings"}` | 退出码 0 |

### 条件评估

所有条件是 AND 关系——全部通过才算达标。检查过程是确定性的（无 LLM 调用），直接在本地执行命令。

---

## Safety Guardrails

| Guardrail | Default | 说明 |
|---|---|---|
| `max_rounds` | 5 | 最大重试轮数，`--max-rounds` 可覆盖 |
| Loop detection | 3 rounds | 同一条件连续 3 轮相同失败原因 → abort (checkpoint 保留，可 `/goal resume`) |
| Cancellation | — | `/goal cancel <id>` 手动中止 |
| Tool iterations | 25 | 单轮内最大 LLM→tool 调用次数 |
| Tool result size | 8 KB | 单个工具结果进入 round context 前截断头部 |
| Handoff validation | fresh mode | 缺失 / 超限 (>16K) / 违反 schema 的 handoff → 该轮整体失败 |

---

## Model Override

GoalPlan 支持 `model_override` 字段，LLM 可以在返回的计划中指定子 agent 使用低成本模型：

```json
{
  "description": "code review",
  "conditions": [...],
  "max_rounds": 3,
  "model_override": "gpt-4o-mini",
  "fresh_context": true
}
```

未指定时使用 session 默认模型。`fresh_context`（默认 `false`）由 `/goal --fresh` 设置，见上文
Fresh-Context Mode。

---

## Persistence

Goal 状态在每个 round 结束后 checkpoint 到 `~/.syscity/goals/<goal_id>.json`，内容包括
plan、当前 round、condition history、累计 token 花销，以及（fresh-context 模式）最近一次
通过校验的 handoff。写入是原子的：先写 `{file}.tmp` 再 `rename` 落盘，进程在写盘中途
崩溃只会留下上一轮的完好 checkpoint，不会截断现有文件。

**轮内恢复**：每轮内每完成一次工具迭代还会追加一次 mid-round 快照（携带当轮消息历史），
崩溃/停机后 `/goal resume` 从恢复的消息前缀**续跑当轮**而不是重头再来；轮末与终态
checkpoint 会清掉该字段。

**重启后不再自动恢复**：gateway 启动时只记录 `N persisted goal(s) suspended`，这些
*suspended* goal 出现在 `/goal list` 中（含 round/max_rounds 和 blocked reason），需要显式
`/goal resume <id>` 才会重新拉起 runner。

Checkpoint 的去留取决于终止方式：

- **完成 / 取消 / agent 错误**：状态文件删除
- **Policy stop**（loop detected、max rounds、invalid handoff、fatal config error）：状态文件
  **保留**，并写入结构化的 `blocked_reason {code, message}`，以便人工排查后通过
  `/goal resume <id>` 继续
- **Gateway 停机**：runner 走 abort（≈崩溃语义）——上一轮的 checkpoint 幸存，goal 变为
  suspended。这与 `/goal cancel`（用户显式作废、删除 checkpoint）是两条不同路径

### Token 成本上报

Runner 对每轮 LLM 调用累计 token 消耗（`TurnUsage`），随 checkpoint 持久化（resume 续算）
并附在 `goal.done` / `goal.aborted` 事件上。goal 终态还会回写一条摘要消息到父会话线程
（含轮数与 token 花销），重启后聊天记录仍保留完整结局。

### CLI

```
syscity goal list            # 运行中 + 挂起的 goal
syscity goal resume <id>     # 从 checkpoint 恢复挂起的 goal
syscity goal cancel <id>     # 取消运行中的 goal / 丢弃挂起的 checkpoint
```

CLI 走 WebSocket `commands.execute`（与 `/goal` 斜杠命令同一通路），无新增 WS 方法或 REST
端点。

---

## Architecture

```
src/goal/
├── mod.rs          # 模块入口，重导出
├── condition.rs    # GoalCondition enum + check() 执行
├── plan.rs         # GoalPlan + LLM 解析（含 fresh_context / model_override）
├── runner.rs       # GoalRunner 子 agent 执行循环（legacy + fresh-context 两种模式）
├── handoff.rs      # RoundHandoff schema + ```handoff 提取与严格校验
├── event.rs        # GoalEvent 事件类型 + BlockedReason
└── persist.rs      # GoalStore 持久化/恢复（含 blocked_reason、last_handoff）
```

Gateway 集成：

- `src/gateway/commands/agents.rs` — `/goal` 命令处理（start / cancel / list / resume）
- `src/gateway/goal_spawn.rs` — suspended goal 列表、从 checkpoint 重建 runner（runner 注册为
  `goal:{id}`、事件 relay 注册为 `goal-relay:{id}`，停机统一排空）、终态回写父会话
- `src/gateway/lifecycle/`（`start_gateway` / `stop_gateway`）— 启动时记录 suspended goals（不自动恢复）；停机时以 abort
  语义排空所有 goal 任务
- `src/gateway/protocol.rs` — `gateway.progress` 事件序列化
- `src/gateway/ws.rs` — WebSocket 路由到订阅 session
- `src/cli/goal.rs` — `syscity goal` CLI 子命令

---

## 与服务模式的对应关系

| 模式 | 对应关系 |
|---|---|
| Turn-based | 默认聊天，用户驱动每轮 |
| Goal-based | `/goal`，条件驱动终止 |
| Time-based | `/schedule` / cron / heartbeat |
| Proactive | 事件驱动编排（未实现） |

Goal-based 填补了 turn-based 和 time-based 之间的空白：不需要用户每轮点击发送，也不需要定时器，而是"做完就算完"。
