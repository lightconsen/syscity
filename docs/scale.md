# 规模化与云端托管（Scale & Cloud Hosting）

Status: **分析（未实施）** · 记录于 2026-09-10 · 结论：**可复用，推荐走路线 A**

本文分析一个产品化问题：**能否基于 Syscity 做一个"云平台数字员工"，全部跑在服务端、用户侧不需要任何本地安装/本地模型/本地存储？如果需要，要改什么？**

文末附[证据索引](#证据索引)（file:line）。文中所有事实性断言均由该索引支撑，测量数字（调用点计数等）为 2026-09-10 在 `main`（v0.3.4 之后）上的实测值。

---

## 1. 结论

**可以复用，复用度很高——但这是"复用内核"，不是"改个配置就能上线"。** 有两条路线，成本相差一个数量级：

- **路线 A｜一员工一实例（容器化托管）**：内核几乎零改动，隔离由容器天然提供。**推荐从这里起步。**
- **路线 B｜共享多租户内核**：需要横切改造数据模型与存储、补齐隔离与平台控制面。

一句话：**能力层几乎全可复用，"没有本地"只需换 feature profile；真正要自研的是平台层（账户/配额/编排/计费）和隔离层（每租户容器）——而这两者用路线 A 就能以最小改动拿到。**

---

## 2. 方向澄清：`cloud` feature 不是"把 gateway 托管成服务"

一个容易走偏的误读：仓库里有 `cloud` feature，但它**不是**托管能力。它是**反方向**的——本地 gateway 作为**客户端**去登录 Syscity Cloud（`src/cloud/client.rs`：`/auth/me`、`/v1/chat/completions`、`/v1/models`、`/v1/embeddings`、`/v1/search`、`/api/v1/kb*`）。

它确实提供了两样可复用的东西：

1. **计量/计费范式**：per-model credit multiplier，`max(1, ceil(tokens/1000 × multiplier))`（`src/cloud/multipliers.rs`），带 10 分钟 TTL 缓存。
2. **凭据存取路径**：cloud session token 存在 `SecretStore` 的 `cloud/session` 命名空间。

但它的计量对象是"**本机用户自己的云端账户**"，网关侧完全没有"这个租户消耗了多少"的账。**服务端多租户 gateway 在这个仓库里不存在。**

---

## 3. 可以直接复用的层（占大头）

| 层 | 复用度 | 说明 |
|---|---|---|
| Web UI | 高 | 已是纯 WS 客户端：`web/src/lib/gatewayBase.ts` 支持远端 base（localStorage `syscity_gateway_base`）+ `Bearer` token；`web/src/transportCore.ts` 已有重连退避、心跳、超时。去掉 Tauri 分支即可 |
| WS RPC + 权限模型 | 高 | `src/gateway/protocol.rs` 的 scope 体系（read/chat/write/acp/pairing/admin，默认拒到 admin）设计干净，可直接挂租户维度 |
| Agent 引擎 | 高 | turn 循环、规划、delegation、skills、tool 编排、memory、goal/cron/heartbeat——最贵的资产，且与"谁在用"基本无关 |
| 能力层 | 高 | KB/RAG、office 文档、connectors、channels（inbound webhook / outbound）、plugins(wasm) 全部保留 |
| 可观测/审计 | 中 | `src/security/persistent_audit.rs` 的 sqlite `audit_log` 可复用，但 `actor` 是自由字符串、**无 tenant 列**，不能直接当账单 |
| 裁剪机制 | 高 | `Cargo.toml` 的 feature 体系已现成；`mobile` profile 就是"无本地依赖"的模板（已去掉 local-embeddings / vision / browser / keyring） |

**"没有本地的"这一条本身不难**：`--no-default-features` + 一个 headless profile，砍掉 `llama-cpp-2`（local-embeddings）、`hf-hub`、`ort`(vision)、`chromiumoxide`(browser)、`keyring`、`desktop/`、`tui/`、`update/`，LLM 与 embedding 全走远端 provider（`src/providers/`、`src/model_router/` 本就支持多 provider 路由）。**这是配置级工作。**

---

## 4. 必须修改的层（按难度排序）

### 4.1 数据模型的租户维度（最大的一块）

`sessions` 表只有 `id / agent_id / channel / channel_id / ... / bound_agent_id / transcript_id / model`，**没有 owner/user_id/tenant 列**；`session_messages`、`threads`、`subagent_runs`、`acp_sessions` 以及可观测表（`llm_calls` / `tool_call_metrics` / `turn_outcomes` / `request_snapshots`）同样如此（`src/agent/session_store/schema.rs`）。全库唯一的用户维度是 `memories.user_id`（`src/memory/db.rs:174,202`）。

文件系统同样是全局单 home（`src/dirs.rs`）：agents 在 `~/.syscity/agents/{id}`，DB 在 `~/.syscity/data/syscity.db`，**没有任何 per-user / per-tenant 路径维度**。KB 的 `collection` 也无归属维度。

→ 要么给每张表加 tenant 并改 `dirs.rs` 布局（路线 B），要么**绕过**：每租户独立进程 + 独立 `SYSCITY_HOME`（路线 A，零改动）。

### 4.2 存储与横向扩展

运行时唯一落地路径是单文件 SQLite：`SqliteVecStore` 在 `src/gateway/init/storage.rs:75`、`src/gateway/init/services.rs:542` 被直接构造。`PgVectorStore`（`src/rag/pgvector_store.rs`）**已实现但未接运行时**——除自身单测外无引用。这是现成的抓手。

此外 `ResponseCache`、`TaskRegistry`、`AuthManager` 会话全是进程内存态，多副本必须共享后端或强制 sticky session。

### 4.3 身份与账户

`AuthManager` 的用户与会话是**纯内存 HashMap**（`src/security/mod.rs:77,79`），**重启即失**。shared token 下所有请求塌缩成同一个主体 `UserId::new("shared")`（`src/gateway/ws/handshake.rs:141`），Tailscale 同理（`:49`）。没有 account/org 表，没有 OIDC。

→ 平台侧要另建账户体系；gateway 只需接受"已认证租户 + 短时 token"。

### 4.4 执行隔离（安全上最难）

工具会在宿主机跑 shell / 读写文件 / computer-use。`SandboxConfig`（`src/tools/sandbox.rs`）是**路径白名单 + advisory 的网络控制**；Landlock（Linux）/ AppContainer（Windows）只提供**写围栏**，不是 syscall jail。共享宿主上跑互不信任的租户 = 灾难。

→ 必须每租户容器/microVM（或至少独立 UID + namespace + 出口控制）。好消息：`RBAC` + 审批队列 + `SecretScanner` 已存在，可作上层策略。

### 4.5 平台控制面（当前完全空白）

没有租户生命周期 API、没有配额（CPU/内存/时长/token）、限流按 ip/device 而非账户、没有服务端用量计量与账单、没有按租户路由。**这部分是"云平台"真正的自研量。**

### 4.6 少量 OS 绑定项

`src/device/`（配对）、`src/update/`（自更新）、`daemon.rs`、`src/tui/` 在云端要么关掉要么重定义语义；`deploy/systemd/syscity.service` 是单用户单元；`wrangler.jsonc` 只做静态站点与 release 分发，不是 gateway 托管。

---

## 5. 两条路线

### 路线 A：一员工一实例（推荐起点）

把"数字员工实例"映射为"一个 syscity 进程 + 独立 `SYSCITY_HOME` + 独立账密/模型 key"，跑在每租户一个容器里，前面加控制面（账户、编排、配额、计量、反向代理 + TLS）。

产品语义天然对齐：syscity 的 **agent = 一个员工**，session = 对话，workspace = 它的工作区，skill/connector/KB = 它的能力与知识。**内核几乎不动，隔离由容器提供。**

### 路线 B：共享多租户内核（规模化后再考虑）

需要：`schema.rs` 的 tenant 化、`dirs.rs` 的 per-tenant home、secrets 的进程级单例拆除、pgvector 接线、共享 session/cache 后端、真隔离边界。收益是降低单实例成本；代价是横切改造。

---

## 6. 模块化判断：要区分两个不同的轴

一个常见误判是"B 方案接近重写 → 说明 syscity 模块化不够"。**不成立**，因为要区分两个轴：

- **模块化（关注点分离）**："能不能拆开"。syscity 这块其实**不错**：`gateway / agent / tools / rag / channels / memory / skills` 边界清楚，store 有 trait，WS 有统一的 method→scope 分发，feature flag 体系完整，`GatewayState` 做了域聚合。
- **可实例化性（无全局状态、依赖可注入）**："能不能同时存在 N 份"。这块**有欠账**。

一句话：**syscity 拆得开，但装不进第二个实例。**

### 路线的成本结构

B 方案"像重写"的构成大致是：

- **~60% 来自多租户的横切本质**：加维度 + 隔离 + 平台子系统。横切维度没有"放进某个模块"的办法；不可信代码隔离、配额计费、横向扩展，都是**新建子系统**而非重构。
- **~30% 来自"可实例化性"欠账**（见 §7）：全局单例、隐式路径、未接线的接缝、无贯穿 Context。
- **~10% 才是真正的模块边界问题**。

### 反证

路线 A 之所以便宜，**正是因为它依赖模块化足够好**：能整体起多份实例、各自独立 `SYSCITY_HOME`、互不干扰。一个模块化烂摊子做不到这一点。**所以是"模块化够好"才让低成本路线成立**，而不是"模块化不够"导致必须重写。

---

## 7. 五项"可实例化性"欠账的价值评估

即使**不做**路线 B，这五项是否值得做？答案是：**有价值，但五项的价值来源完全不同，且都不是"为多租户做准备"。**

关键前提：**路线 A 恰恰不需要它们**——容器 + `SYSCITY_HOME` 已把隔离免费给你；每进程一个 home、一份 secrets、一个 cache，**全局单例在这个形态下是正确且高效的**。

### 7.1 证据（2026-09-10 实测）

| 指标 | 实测 | 含义 |
|---|---|---|
| `dirs::` 调用点 | **201** 处 | 量大，但**全部集中在 `dirs.rs` 一个模块**——布局是集中定义的，问题只是"隐式读进程全局 root"，不是各处手搓路径 |
| `SYSCITY_HOME` 引用 | **5** 处 | 逃生舱很薄，进程级隔离几乎已够 |
| `serial_test` 引用 | **2** 处（2 文件） | 全局状态目前在测试里基本**没咬人** |

三项数字共同说明：现有痛感很轻，代码在单用户形态下工作得很好。这**削弱**了立刻重构的理由，**强化**了"等驱动出现再改"。

### 7.2 分档

**第一档｜独立成立，当下就有收益**

- **贯穿的 RequestContext**：价值不在多租户，而在**授权与可观测性正确性**——审计 `actor` 准确、按用户限流、测试可注入上下文（不用伪造全局 auth）。
- **路径显式化**：价值在**测试密闭性**（不再有代码路径偷偷写真实 `~/.syscity`）与备份/多 profile。值得，但 201 处是机械式大改；可先包一层访问器再逐步注入。

**第二档｜由一个具体 bug 成立**

- **AuthManager 会话与设备配对持久化**：`AuthManager` 纯内存、**重启即失**——这在单用户本地也是真 bug（重启就要重新登录/重新配对）。**现在就该修，与多租户无关。**
- 另一半（`ResponseCache` / `TaskRegistry` 抽象出 repository）：只在多副本时才需要，否则是白加一层间接。

**第三档｜只有规模化/运维驱动才成立**

- **Postgres 接线**：本地单人场景 SQLite 是**更优**选择（零运维）。只有托管成平台（N 租户、集中备份、HA、横向扩展）时才划算——本质是**运维决策**，不是架构缺陷修补。接线便宜，贵的是运维与特性矩阵测试。
- **进程级单例 DI 化**（`static STORE: OnceLock<Arc<dyn SecretStore>>` @ `src/secrets/store.rs:284`、`static KEY: OnceLock<Arc<MasterKey>>` @ `src/secrets/file_store.rs:348`、各类 `LazyLock` cache）：只在出现"一个进程跑多实例"（嵌入式/多网关同进程）或"测试需并发隔离"时才划算。目前 `serial_test` 仅 2 处 → 需求还没出现。

### 7.3 若当作独立工程目标的排序

1. **会话/配对持久化** —— 真 bug，立做
2. **贯穿的 RequestContext** —— 审计/限流/可测性，排期
3. **路径注入化** —— 测试密闭性，渐进做
4. **单例 DI 化** —— 等"多实例嵌入"或测试隔离需求
5. **Postgres 接线** —— 等托管/规模化需求

### 7.4 反向建议

**不要为"未来可能多租户"去改这五项。** 这是典型的 speculative generality，会引入无收益的间接层。这个仓库本身偏务实——`Cargo.toml` 明确记录过 `unused_results` lint 因约 1040 处 builder 链噪声被否掉，说明它拒绝为理论洁癖加间接层。正确做法是**让每个改动带自己的本地理由**（测试密闭、审计正确、修重启丢会话），多租户红利当副产品收下。这样即使路线 B 永远不做，投入也不浪费。

---

## 8. 不做路线 B 也值得做的任务（Backlog）

这些任务的**本地理由各自独立**（真 bug / 测试密闭 / 审计正确 / 运维灵活），不依赖"将来要做多租户"。按优先级排列；每项都标注了**触发条件**——不到触发条件就不做，避免 speculative generality。

| # | 任务 | 本地理由（与多租户无关） | 成本 | 触发条件 |
|---|---|---|---|---|
| **T1** | 会话与设备配对持久化 | 真 bug：重启即丢登录态与配对 | 小 | **已满足，立做** |
| **T2** | 贯穿请求生命周期的 RequestContext | 审计 `actor` 准确、按用户限流、测试可注入 | 中 | **已满足，排期** |
| **T3** | 路径显式化（注入 root，替代隐式全局 home） | 测试密闭性：不再污染真实 `~/.syscity` | 中偏大（机械） | 测试开始互相干扰时 |
| **T4** | SecretStore / MasterKey 去全局单例 | 多实例嵌入、测试隔离 | 中（安全敏感） | 出现"一进程多实例"或并发隔离需求 |
| **T5** | 存储接缝接线（VectorStore 可切 pgvector） | 托管/规模化时的运维灵活性与集中备份 | 小（接线）｜大（运维） | 需要集中存储/HA/横向扩展时 |

### T1｜会话与设备配对持久化

- **现状**：`AuthManager` 的 users/sessions 是纯内存 `HashMap`（`src/security/mod.rs:77,79`），进程重启后全部失效——本地单用户场景下表现为"重启就要重新登录/重新配对"。
- **改动面**：`src/security/mod.rs`、`src/security/device_pairing.rs`，落盘到既有 sqlite（沿用 `src/agent/session_store/schema.rs` 的迁移机制新增表）。
- **验收**：重启 gateway 后，已登录会话与已配对设备仍然有效；撤销（revoke）语义与过期时间可控。
- **注意**：token/配对凭据属敏感信息，落盘需走既有 `SecretStore` 的加密路径（`docs/secret-storage.md`）。

### T2｜贯穿请求生命周期的 RequestContext

- **现状**：身份只在 WS 握手处解析一次（`src/gateway/ws/handshake.rs:141` 的 `UserId::new("shared")`），之后散落各处、下游重新推导；审计表的 `actor` 因此不可靠，也无法按用户做配额/限流。
- **改动面**：`src/gateway/ws/core.rs` 的分发处构造 context → 传入 agent engine → tools → `src/security/persistent_audit.rs` 写入。
- **验收**：审计记录里的 `actor` 直接来自 context；不同用户/设备有独立的限流与配额计数；单测可直接构造 context 而不必伪造全局 auth。
- **注意**：热路径 plumbing 会触及较多函数签名，建议一次性完成、避免半途混用两种来源。

### T3｜路径显式化（注入 root）

- **现状**：`src/dirs.rs` 是一组自由函数，隐式读进程全局 home；调用点约 **201** 处（2026-09-10 实测）。好处是布局集中在一个模块，坏处是"隐式"——任何代码路径都可能悄悄写真实 `~/.syscity`，测试必须先设 `SYSCITY_HOME`（进程级、并发时互相干扰）。
- **改动面**：`src/dirs.rs` + 调用点。**建议渐进**：先包一层访问器（`Paths`/`WorkspaceRoot` 对象），新代码只用访问器，旧调用点按模块逐步迁移。
- **验收**：测试可在临时目录下完整运行，不再触碰真实 home；去掉对环境变量的依赖。
- **注意**：**不要**一次性重写 201 处；每迁一个模块都要能独立通过测试。

### T4｜SecretStore / MasterKey 去全局单例

- **现状**：`static STORE: OnceLock<Arc<dyn SecretStore>>`（`src/secrets/store.rs:284`）与 `static KEY: OnceLock<Arc<MasterKey>>`（`src/secrets/file_store.rs:348`）使"每实例一份"在设计上不可能；同类还有 `LazyLock` 的各类 cache。
- **改动面**：挂到已有的 `GatewayState`（`src/gateway/state.rs`）逐层传递——该模式仓库里已有先例，不需要发明新机制。
- **验收**：同进程内构造两个 `GatewayState`，secrets 与 cache 互不共享（集成测试覆盖）。
- **注意**：触及加密主密钥的初始化，属安全敏感改动；不做则路线 A（每进程一实例）完全够用。

### T5｜存储接缝接线（VectorStore 可切 pgvector）

- **现状**：`PgVectorStore` 已完整实现（`src/rag/pgvector_store.rs`）却**未接运行时**——运行时在两处硬编码 `SqliteVecStore`（`src/gateway/init/storage.rs:75`、`src/gateway/init/services.rs:542`）。典型"留了缝没缝针"。
- **改动面**：在初始化处按 config 选择后端，trait 已存在（`VectorStore`）。
- **验收**：config 切到 Postgres 时服务正常启动并走 `PgVectorStore`；sqlite 仍是默认且行为不变。
- **注意**：接线本身很小，真正成本在**运维与特性矩阵测试**（`pgvector` 是独立 feature）。本地单人场景 SQLite 更优，**非必需不要开**。

### 明确不做的

- **`ResponseCache` / `TaskRegistry` 抽 repository 边界**：只在多副本/共享缓存时才需要，当前单进程下内存态是正确且最快的实现——纯 speculative，不做。
- **`schema.rs` 表租户化**：只在路线 B 下成立，见 §5。

---

## 9. 落地顺序

1. **起步走路线 A**：headless feature profile（仿 `mobile`）+ 每租户独立 `SYSCITY_HOME` + 每租户独立容器，前面加控制面（账户、编排、配额、计量、反代 + TLS）。这条不需要 §8 的任何一项。
2. **顺路做 T1**（真 bug，成本最小、收益立现）。
3. **排期 T2**（审计/限流正确性）。
4. **按触发条件等 T3/T4/T5**：路径显式化、单例 DI 化、pgvector 接线——驱动出现前不做。

---

## 证据索引

| 断言 | 位置 |
|---|---|
| sessions 表无 owner/tenant 列 | `src/agent/session_store/schema.rs`（sessions / session_messages / threads / subagent_runs / acp_sessions / llm_calls / tool_call_metrics / turn_outcomes / request_snapshots） |
| 全库唯一用户维度 | `src/memory/db.rs:174,202`（`user_id TEXT NOT NULL`） |
| 全局单 home 与布局 | `src/dirs.rs`（agents / `data/syscity.db`；`SYSCITY_HOME` 覆盖） |
| 运行时为 SqliteVecStore；PgVectorStore 未接线 | `src/gateway/init/storage.rs:75`、`src/gateway/init/services.rs:542`、`src/rag/pgvector_store.rs`（仅自测引用） |
| AuthManager 纯内存 | `src/security/mod.rs:77,79` |
| shared / tailscale 单一主体 | `src/gateway/ws/handshake.rs:141,49` |
| secrets 进程级单例 | `src/secrets/store.rs:284`、`src/secrets/file_store.rs:348` |
| scope 体系与默认拒 | `src/gateway/protocol.rs`（method_scope，兜底 admin） |
| 审计表无 tenant 列 | `src/security/persistent_audit.rs`（`audit_log`） |
| 沙箱为白名单 + advisory 网络 | `src/tools/sandbox.rs`（Landlock / AppContainer 仅写围栏） |
| `cloud` 为客户端集成 | `src/cloud/client.rs`、`src/cloud/multipliers.rs`、`Cargo.toml`（`cloud` feature 默认关） |
| feature 体系与 headless 模板 | `Cargo.toml`（default / `cloud` / `mobile` / `intel-macos` profile） |
| 客户端远端连接已可用 | `web/src/lib/gatewayBase.ts`、`web/src/transportCore.ts` |
| 部署为单用户 | `deploy/systemd/syscity.service`、`scripts/install.sh`、`wrangler.jsonc`（仅静态/分发） |
| "team" 是多 agent 而非多租户 | `scripts/test_team.sh` |
