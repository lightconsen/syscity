# 风险登记表（Risk Register）

> **来源**：2026-09-16 的全仓审计（原 `audit/audit-gpt.local.md`，已删除——正文结论过期，
> 登记表是唯一有跨时间追踪价值的部分，收编于此）。
> **状态核实日期**：2026-09-21。
> **维护规则**：这是**活登记表**。一条风险只有"修复后把状态改为已修（附落点）"或
> "按设计关闭（附依据）"两种消账方式，不删除条目；新发现的登记为新 ID。

严重度口径沿用原审计：P0（默认条件下可越权/泄露/不可恢复）、P1（远程部署前必须修）、
P2（有规避方案的功能/可靠性缺陷）、P3+（维护性）。

## 安全

| ID | 等级 | 领域 | 状态（2026-09-21 核实） | 说明 / 修复落点 |
|---|---|---|---|---|
| **SEC-001** | P0/P1 | WS scope 自授 | ✅ **已修** | `resolve_scopes`（`src/gateway/protocol.rs:270`）语义改为"请求只能**收窄**entitlement，不能加码"；空请求 = 全额授予；回归测试 `resolve_scopes_narrows_but_never_widens`。客户端提示 `web/src/transportCore.ts` 仍请求 `["chat","read","write"]`，但现在只是"申请"，不改变授予 |
| **SEC-002** | P1 | method scope 错标 | ✅ **已修**（2026-10-15 复核） | `models.add/remove/set_default/fetch_remote`、`skills.install`、`mcp.call_tool` 等副作用方法已入 write 段（write 块现起于 `src/gateway/protocol.rs:395`，错标理由注释在 `:391`）；`SCOPE_PAIRING` 已有消费者（pairing 票据与 gate 检查）。**原记的"无 CI 结构检查"已过期**：`918a580d`（2026-09-22，即登记核实次日）补上了 `method_table_matches_the_dispatcher`——它用 `include_str!("ws/core.rs")` 解析 dispatch arm，与 `METHOD_SCOPES` **双向集合相等**（多一个、少一个都失败），外加 `every_dispatched_method_declares_a_scope` 与 `method_table_entries_are_well_formed`；三者都在 CI 的 Test job 里跑。另外补了一个**行为**检查 `every_method_in_the_scope_table_dispatches`：遍历全表逐条探针派发，断言不是 `METHOD_NOT_FOUND`——它不依赖源码写法，堵住"解析漏掉一个 arm、表却继续描述无人处理的方法"。**剩余的真实缺口是标签正确性**：只有 `sensitive_methods_require_the_scope_they_declare` 里硬编码的 25 对在验证"这个 read/write 标得对不对"，其余 ~197 个方法只保证"有标签" |
| **SEC-003** | P1 | session 对象所有权 | 🔒 **按设计关闭** | 单主体模型成立：不支持一实例多主体，"多用户是设计变更不是配置项"（`docs/security-config.md` "Deployment model" 一节）。原"owner middleware"建议作废 |
| **SEC-004** | P1 | artifact 路由 / symlink | ✅ **已修** | 路由挂 essential 公共层（项目规则有明文豁免段，`CLAUDE.md` "File/static download"）；symlink 防护已做：canonicalize 根与文件的**父目录**、final component 刻意不 canonicalize（macOS NFC/NFD，`src/gateway/handlers/artifacts.rs:182`）；404 而非 403 语义保留 |
| **SEC-005** | P1 | agent_id / 导入路径边界 | ⏳ **未清账** | 原判"中高，待 E3"未复核；若 agent id 构造与 import 参数已过 `AgentId`/canonical 约束请补充证据并改状态 |
| **SEC-006** | P1 | webhook fail-open | ✅ **已修** | `src/gateway/webhooks.rs` 全线 fail-closed：WhatsApp HMAC secret **必需**（缺失/缺头/错签一律拒，:236-258）、Lark challenge 无 secret 拒绝（:188-203）；secretless 渠道启动时统一警告（`lifecycle.rs:40 warn_on_secretless_webhook_channels`）。**时间窗防重放（Slack/Feishu）仍未确认**——登记保留 |
| **SEC-007** | P1 | CORS `*` + credentials | ⏳ **未清账** | `CorsConfig` 已是 allowlist 形状（`allowed_origins` 列表），但"默认值是否仍 `*` + mirror origin + credentials 组合"未复核。启用 cookie/session 前必须清掉这条 |
| **SEC-008** | P1/P2 | 事件广播越主体 | ⏳ **部分未清** | 按会话分发已做（WS `audience_of`：ApprovalRequired/AgentResponse/Thinking 等路由到 session）；**设备、渠道、MCP、AgentStatus 级事件仍广播全体连接**——单主体下是元数据噪音，多主体部署前必须收紧 |
| **SEC-009** | P2 | 凭据暴露 | ⏳ **未清账** | 短时 ticket 已有（`/api/v1/ws-ticket` + `?ticket=`，升级 URL 长期 token 有一次性警告）；localStorage 长期 token 与日志脱敏未复核 |
| **SEC-010** | P2 | 沙箱边界 | 🔒 **按设计关闭**（2026-10-15 复核） | 命令工具的边界已从"应用级允许列表"变为**内核强制**：`workspace_only` 时 `shell` / `code_exec` / `process` 各自附加 `WriteFence`（`src/tools/process_runner/mod.rs`）——macOS 把 argv 改写成 `/usr/bin/sandbox-exec -p <profile>`、Linux 在 `pre_exec` 装 Landlock（不可用时 fail-closed）、Windows AppContainer；`.git` 与 `.syscity` 即使在被授予的根内也恒不可写（`PROTECTED_WRITE_NAMES`）。在此之上还有三层可选/附带约束：`[security] fence_network`（macOS `(deny network*)`、Linux seccomp 拒 `socket(AF_INET\|AF_INET6)` 而保留 AF_UNIX，默认 false）、`[security] fence_namespaces`（只读根 + 私有 `/tmp` 的 namespace 视图，默认 auto）、以及每次围栏运行都装的 seccomp 逃逸向量拒列表。**读仍全域开放是设计**（围栏约束的是写）。结论不变：不可信租户仍需容器/microVM + 出口控制。（原引用的本地审计 `audit/audit-sandbox-*.local.md` 已删除——它"网络在内核层完全不管"的结论在 `fence_network` 落地后即过期——本行改为自足陈述，不再指向本地笔记。） |

## 可靠性 / 正确性 / 运维

| ID | 等级 | 领域 | 状态（2026-09-21 核实） | 说明 / 修复落点 |
|---|---|---|---|---|
| **REL-001** | P2 | shutdown drain | ⏳ 未复核 | 原判"静态证实"；`writes::pending` guard 与 TaskRegistry 机制存在，drain 完整性未重验 |
| **REL-002** | P2 | WS 可靠性 | ⏳ **未清账** | 结构化错误已做（WsError code 体系）；**lag/resync 与 backpressure 行为未复核**（`FrameLimiter` 是限流不是背压） |
| **COR-001** | P2 | UTF-8 分片 | ⏳ 未复核 | TUI 侧多字节 delta 边界有测试；**WS 层的 char-boundary chunker 未确认** |
| **OPS-001** | P1/P2 | effective config | ⏳ 未复核 | `config.get` 返回 revision + 投影、CAS 有；启动时"effective config 报告"未做 |
| **TEST-001** | P2 | 质量证据 | ⏳ 未复核 | CI 结构（fmt/clippy/audit/deny + 分套测试）已运转；覆盖率阈值与 E2E 跳过矩阵未重审 |
| **ARCH-001** | P2 | 多状态路径 | ⏳ 未复核 | MemoryManager 迁移与单一持久化契约未重验 |
| **ARCH-002** | P2/P3 | 共享多租户 | 🔒 **已决策** | 单主体一进程/容器是既定路线（`docs/scale.md` 路线 A；多用户骨架已删）；tenant 化内核明确不做 |

## 已知未修复项的优先建议

1. **SEC-007**：在启用任何 cookie/浏览器可读认证之前复核并收紧默认 CORS。
2. **SEC-008**：多主体部署前的阻断项（单主体下可接受为噪音）。
3. **SEC-005 / REL-002 / COR-001 / OPS-001**：低成本复核项——每条半小时内可用当前源码判定并改状态。
