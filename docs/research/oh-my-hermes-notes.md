# oh-my-hermes 研究：编排层如何把"证据边界"变成工程约束

> 来源：`~/my/oh-my-hermes-main`（非 git 仓库快照，2026-09-15 导出；上游 [rlaope/oh-my-hermes](https://github.com/rlaope/oh-my-hermes)，v2.0.3 / stable；Python 3.11+）
> 对 Syscity 的借鉴已移出到仓库根目录的 `ohm.local.md`（gitignore，不提交）。本文件只描述 OMH 本身。
> 素材：`README.md` 与文档层级、`pyproject.toml`、`src/commands/{main,setup}.py`、`src/install/{installer,config_adapter,plugin_pack}.py`、`src/routing/*`、`src/capabilities/*`、`src/evidence/labels.py`、`src/coding/{executors,status_board,executor_skill_discovery,maestro/facade}.py`、`src/plugin_bundle/omh/plugin.yaml`、`install.sh`
> 用途：内部研究笔记。与 `environment-agent.local.md`（只读感知版）的"来源 / 时效 / 覆盖率"设计，以及网关的授权与审计面直接相关。

---

## 一、一句话结论

**OMH 不是功能增强器，是一层约束。** 它把"我准备了"和"我观测到了"做成**不可混淆的类型系统**——用 fail-closed 的封闭状态词表、每条元数据必带的 `claim_boundary` 字段、以及"默认不交接、交接只 prepare 不 dispatch"的车道分离来落地。

功能上它确实提供了路由、并行、记忆、工作流引擎，但那些都是 Hermes 已经能做的；它真正独有的是**决定什么算完成、什么不算证据**。

## 二、它是什么

Hermes Agent 之上的插件与编排层，自我定位 *"operating layer above Hermes-native skills"*，且明确 **never patching it**。

```
omh     → exec hermes（同一扇门，OMH 身份 + HUD）
hermes  → Hermes
```

`src/commands/main.py:558` 的 `_launch_hermes_tui()`：裸跑 `omh` 不带子命令时，若 stdin/stdout 是 TTY 且 PATH 有 `hermes`，直接 `subprocess.run([hermes])`——**故意不加 `--tui`**，把终端选择权交还 Hermes 自己的 `display.interface`。这就是"包装器而非替代品"的代码形态。

三块能力（README 自称"一件事交付三样"）：

| 块 | 内容 |
|---|---|
| 编码智能 | 12 个 category 的模型+effort 有序链、13 个模型族的 prompt 校准、`ulw-work` 并行 fanout、108 个 `omh-*` 专家技能 |
| 长期记忆 | 文件存储的 memory provider，reviewer-gated 写入，active → reference → archive 分级老化，**Hermes 自己的 memory 绝不读改** |
| 工作流引擎 | 9 个 `ulw-*`：context / interview / research / plan / work / maestro / loop / qa / perf |

主流程：**Understand → Research → Decide → Plan → Execute → Verify → Operate → Learn**。

## 三、四条腿：安装时到底改了什么

`omh setup`（`src/commands/setup.py:2182`）通过 Hermes 的四个官方扩展点落地，**不劫持、不替换 `hermes` 二进制**：

| 腿 | 动作 | 落点 |
|---|---|---|
| 1 配置 | 就地编辑 6 个 key | `~/.hermes/config.yaml` |
| 2 插件 | 整目录拷贝（原子写 + sha256 manifest） | `~/.hermes/plugins/omh/` |
| 3 技能 | 写 `SKILL.md`，并对每个 profile 重复注册 | `~/.omh/skills/<cat>/<label>/` |
| 4 MCP | **故意不给 Hermes** | 只给 claude-code / codex / opencode / cursor |

config.yaml 的 6 处写入（`src/commands/setup.py:1715 _apply_result`）：`skills.external_dirs`、compression 默认值、`plugins.enabled += "omh"`、`display.interface`、`display.skin`、`memory.provider → omh`（**仅在槽位空闲时抢占**，Hermes 只跑一个 provider）。

两个实现细节值得记：

- `src/install/config_adapter.py` 是**手写的按行 YAML 文本编辑器，不是 YAML 库**——为的是让用户的 config.yaml 保持 **byte-stable**；且只在真变了才写盘。
- 技能目录**故意比 tap 深一层**（`<category>/<label>/SKILL.md`），因为 Hermes 从**目录结构**推断 dashboard 分类，且只在相对路径有 3+ 段时生效。仓库根的 `skills/` 则保持扁平，因为 tap lister 只读一层。

插件本体（`src/plugin_bundle/omh/plugin.yaml`）：`kind: standalone`、`requires_hermes: ">=0.21.1,<0.22.0"`、`provides_memory_provider: omh`、18 个 tool、6 个 hook（`pre_llm_call` / `pre_tool_call` / `post_tool_call` / `transform_tool_result` / `pre_verify` / `on_session_end`）。

**没有常驻进程。** 全仓库无真实服务（所有 `daemon=True` 都是 `threading.Thread`），插件由 Hermes 在自己的进程内 import 并调 `register(ctx)`。唯一的长驻入口是 `omh mcp serve`（stdio，被其他宿主 fork，OMH 自己不跑）。

**刻意不做的事**：不改 `hermes` 二进制、不装 shell alias、**不改 `~/.hermes/state.db`（只读）**、不给 Hermes 注册 MCP server。

## 四、技能：生成物 + 确定性路由

**三个目录都是生成物，不是手写源。**

| 目录 | 数量 | 定位 |
|---|---|---|
| `skills/` | 125 | Hermes 原生 skill tap 投影 |
| `agent-skills/` | 101 | Agent Skills 格式投影（子集），给 6 个外部宿主 |
| `roles/` | 9 | 责任标签，**不是运行时 agent** |

真源是 `src/skills/catalog_definitions.py`（7798 行）+ `src/catalogs/roles.py`。`agent-skills/` 剥掉了 Hermes 专有段落（`Workflow Lane`、`Handoff policy`、`omh runtime record` 配方），换成通用措辞。

`roles/` 每个都打上 `runtime_claim: "descriptor_not_runtime_agent"`。`roles/builder.md` 的边界原文：

> A builder role label is not hidden coding execution, executor/runtime dispatch, worker start, implementation result, verification, review, CI, merge readiness, or merge evidence.

**路由是确定性整数打分，不是模型判断**（`src/routing/recommend.py:2470 _score_definition`），纯累加、`@lru_cache`：

```
显式调用（$ / ./ @ 前缀命中）   +12
trigger 短语完整匹配             +6 每条
技能名 / description / use_when  +5 / +3 / +3
trigger token 交集               +3 每 token
domain signal 命中               +54
```

排序取前 N，再由 `_confidence()` 映射 `low/medium/high`。入口 `route_chat_message()`（`src/routing/chat.py:1432`）默认 **`min_confidence: "high"`——低于阈值不 dispatch**，只返回候选或要求澄清。

三层注册表：`catalog_definitions.py`（真源）→ `capabilities/registry.py`（10 个 section 的 capability manifest）→ `capabilities/keywords.py`（keyword detector manifest）。触发词**强制英文撰写**，中日韩走 `src/routing/trigger_packs/{ja,ko,zh}.json`，在 catalog 读取**之前** merge——所以打分层、渲染出的 SKILL.md、`docs/WORKFLOWS.md` 读的是同一张表。

## 五、证据边界：最值得学的部分

没有单句定义，它是被**操作化**的四条机制：

**(a) `claim_boundary` 是 schema 必填契约字段。** `CONTEXT.md:110` 定义它，并注明 *"Avoid: disclaimer (it is a validated contract field, not prose)"*。**每个 OMH 写出的 metadata artifact 都带一句自证否定。** `src/capabilities/schema.py:51` 的 `PREPARED_NOT_OBSERVED`：

> Prepared OMH capability, handoff, topology, or routing metadata is not execution, worker dispatch, worktree creation, review, CI, merge-readiness, or merge evidence.

**(b) 封闭状态词表 + fail-closed 归一。** `src/coding/status_board.py:81`：

```python
STATUS_VOCABULARY = ("running", "completed", "failed", "worktree_failed", "prepared_not_observed")
```

未知状态**绝不被读成"发生过的事"**——`:260` 强制归一到最保守值，同时保留原词供审计。

**(c) 两轴人类标签。** `prepared_not_observed` 这种线值把两个正交问题焊成一个 token，人读不懂，于是 `src/evidence/labels.py` 拆成：

- **Phase 轴**：`Route/Plan/Code/Setup/Test/Review/Ship`——**全是名词，刻意不用动名词**，因为 "Coding" 会被扫读成"有人正在写代码"
- **Confidence 轴**：`not run / running / partly seen / reported done / seen / failed / verified / blocked / cancelled`

| 线值 | 渲染 |
|---|---|
| `prepared_not_observed` | `Plan · not run` |
| `completed` | `Code · reported done` |
| `passed` | `· verified` |
| `unknown` / `pending` | `· not run`（fail-closed） |

`labels.py:93` 的注释点明要害：

> `completed` is the **EXECUTOR'S OWN report about itself**; OMH observed the claim, never the result.

**(d) 边界能过静态检查。** `src/coding/executor_guidance_compatibility.py:238` 维护一份 `HOST_SPECIFIC_VOCABULARY` 泄漏词表（`CLAUDE.md`、`apply_patch`、`CODEX_HOME`、`TodoWrite`…）。自己的指导文本若出现不属于当前 owner 的宿主机制词，审计报 `leakage: detected`。**语义边界落成了 CI 检查。**

这套东西被约 169 条测试断言钉住（`src/evidence/labels.py:4` 自述）。

## 六、编码交接：默认不交接

容易被误读的一点。`CONTEXT.md:141`：

> 没有显式选择 coding owner 时，编码工作**跑在 Hermes harness 内部，不选任何外部编码 CLI**。这是默认也是常规路径……**下面那套 Maestro/handoff 机制完全不参与。**

交接是**显式 opt-in 的第二车道**，而且只交给两个：

```python
EXTERNAL_CLI_PROFILES = ("claude-code", "codex")   # src/coding/executors.py:38
```

**cursor / opencode / pi / openclaw 不在其中**——它们是 skill 宿主，不是 coding owner。两种角色不是同一集合。

三种 owner mode → 三种 handoff 字段（`src/coding/maestro/facade.py:21`）：`external_executor` → `executor_handoff`（codex 唯一可 dispatch）、`prompt_only_handoff`（claude-code / generic）、`runtime_handoff`（hermes / omx / omo / omc）。

而且是**只 prepare、绝不 dispatch**：`dispatch_contract: "wrapper_dispatches_to_codex; omh_does_not_execute_codex"`。Hermes 想自己选会被硬拒绝（`HermesNativeSelectionError`），保证两条车道在代码层不混淆。整个项目**唯一被批准的执行面**是 Fanout dispatch（`CONTEXT.md:170`）。

一个很干净的安全设计——`src/coding/executor_skill_discovery.py` 探测 `~/.claude/skills`、`~/.codex/prompts` 等，法则是 **"Declared, never observed"**：

> A `SKILL.md` on disk is configuration evidence that the file exists, not proof the executor loads, enables, or honours it.

配套三条规则：第三方可写的 frontmatter `description` **只与封闭角色词典匹配、永不离开分类器，只有 skill 名能进 prompt**（防 prompt 注入）；调用形式由源目录决定不猜前缀；固定探测表 + 固定深度 + 条目/字节上限。

## 七、跨宿主投影：copy-only，各带 caveat

`.claude/`、`.codex/`、`.cursor/`、`.opencode/`、`.pi/`、`.openclaw/` 六个目录各只有 `install.sh` + `install.ps1` + `manifest.json`，把同一份 `agent-skills/` **byte-exact copy-only** 投过去（六者 `source_digest` 相同）。每个 manifest 各带一条 caveat：

| host | caveat |
|---|---|
| claude | Repo discovery observed in Claude Code 2.1.270, 2026-09-13; **not workflow execution** |
| cursor | User-scope scanning not independently verified. |
| opencode | Skill tool is permission-gated. |
| pi | Trusted projects only; first duplicate name wins. |
| openclaw | Custom `OPENCLAW_STATE_DIR` skips the standard user path… |

`docs/AGENT-SKILLS.md:70` 直接写明：装文件**不证明宿主会执行**。

## 八、工程纪律与文档权威层级

```
2.0.3 · dependencies = []（运行时零第三方依赖，纯标准库）
974 个 .py / 385,374 行 / 649 个测试模块
125 skills 由单一源生成，CI drift gate —— 一个字节不一致就 fail
```

文档有**明确的权威层级**，读之前要先分清：

```
docs/DIRECTION.md    产品宪章（最高）—— AGENTS.md 要求改架构前必读
AGENTS.md            仓库操作契约
CONTEXT.md           术语表（自述 "glossary only"）
CLAUDE.md            派生，非权威，自己声明"不重复上面两份"
DESIGN.md            ⚠️ 只管静态站点，不是产品设计
REVIEW.md            评审标准，"Nothing reads it automatically"
```

README 是门面，不是设计源。它的强断言（"never hiding a coding executor behind it"）在代码里对应 `EXTERNAL_CLI_PROFILES` 那个窄元组加 `HermesNativeSelectionError`。

benchmark（`benchmarks/product-ab/v1/README.md`）用仓库自己已合并的 PR 当任务、用那些 PR 自己的测试判分，按 `task_source` 分子集，**只有 issue 来源（修复前写的）才配支撑 headline**，PR body 来源只能进二级表：

> A corpus that needs a caveat paragraph to be read correctly will be quoted without the caveat.

而它的表**目前空着**，README 写着 *"No measured run has been published yet."*

---


## 九、待确认

- 顶层子命令数量约 58（由 `src/commands/main.py:203 build_parser()` 构建的 parser 枚举得出，未逐个执行验证）。
- README 声称 108 个 `omh-*` 专家技能，与 `skills/` 的 125 个目录数的关系（子集 vs 含非 `omh-*` 前缀的工作流技能）未逐一核对。
- `memory.provider` 抢占槽位后与其他 memory provider 的共存行为未细看。
