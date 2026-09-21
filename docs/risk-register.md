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
| **SEC-002** | P1 | method scope 错标 | ✅ **已修** | `models.add/remove/set_default/fetch_remote`、`skills.install`、`mcp.call_tool` 等副作用方法已入 write 段（`src/gateway/protocol.rs:405-430`，带选择理由注释）；`SCOPE_PAIRING` 已有消费者（pairing 票据与 gate 检查）。无 CI 结构检查（"新 method 必须有显式 scope 条目"）——**该加固建议仍未实施** |
| **SEC-003** | P1 | session 对象所有权 | 🔒 **按设计关闭** | 单主体模型成立：不支持一实例多主体，"多用户是设计变更不是配置项"（`docs/security-config.md` "Deployment model" 一节）。原"owner middleware"建议作废 |
| **SEC-004** | P1 | artifact 路由 / symlink | ✅ **已修** | 路由挂 essential 公共层（项目规则有明文豁免段，`CLAUDE.md` "File/static download"）；symlink 防护已做：canonicalize 根与文件的**父目录**、final component 刻意不 canonicalize（macOS NFC/NFD，`src/gateway/handlers/artifacts.rs:182`）；404 而非 403 语义保留 |
| **SEC-005** | P1 | agent_id / 导入路径边界 | ⏳ **未清账** | 原判"中高，待 E3"未复核；若 agent id 构造与 import 参数已过 `AgentId`/canonical 约束请补充证据并改状态 |
| **SEC-006** | P1 | webhook fail-open | ✅ **已修** | `src/gateway/webhooks.rs` 全线 fail-closed：WhatsApp HMAC secret **必需**（缺失/缺头/错签一律拒，:236-258）、Lark challenge 无 secret 拒绝（:188-203）；secretless 渠道启动时统一警告（`lifecycle.rs:40 warn_on_secretless_webhook_channels`）。**时间窗防重放（Slack/Feishu）仍未确认**——登记保留 |
| **SEC-007** | P1 | CORS `*` + credentials | ⏳ **未清账** | `CorsConfig` 已是 allowlist 形状（`allowed_origins` 列表），但"默认值是否仍 `*` + mirror origin + credentials 组合"未复核。启用 cookie/session 前必须清掉这条 |
| **SEC-008** | P1/P2 | 事件广播越主体 | ⏳ **部分未清** | 按会话分发已做（WS `audience_of`：ApprovalRequired/AgentResponse/Thinking 等路由到 session）；**设备、渠道、MCP、AgentStatus 级事件仍广播全体连接**——单主体下是元数据噪音，多主体部署前必须收紧 |
| **SEC-009** | P2 | 凭据暴露 | ⏳ **未清账** | 短时 ticket 已有（`/api/v1/ws-ticket` + `?ticket=`，升级 URL 长期 token 有一次性警告）；localStorage 长期 token 与日志脱敏未复核 |
| **SEC-010** | P2 | 沙箱边界 | 📌 **文档化结论** | 沙箱能力边界已入 `audit/audit-sandbox-syscity-vs-codex-vs-claude-code.local.md`（网络 advisory、读全域开放是已知设计）；结论不变：不可信租户需要容器/microVM + 出口控制 |

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
