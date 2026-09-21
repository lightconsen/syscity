# Syscity 协议规范 v1.0

> **状态**: 已落地。WebSocket-native 是网关唯一的客户端协议——UI、CLI、桌面端
> 都只走 `/ws`（方法分发在 `src/gateway/ws/core.rs`，作用域表在
> `src/gateway/protocol.rs`）。REST 面收缩为 `CLAUDE.md` "API Protocol
> Convention" 列出的几类例外（OpenAI 兼容端点、OAuth 回调、外部平台 webhook、
> artifact 下载、探针与 metrics）。
>
> **这是一份设计记录。** §1.1、§9.3、§10 的"之前（当前）"描述的是**迁移前**的
> 系统，保留下来是为了解释动机；各阶段的落地程度以实现为准——本文与代码或
> `CLAUDE.md` 不一致时，按代码。

---

## 1. 概述

### 1.1 为什么采用 WebSocket-Native

Syscity **在迁移前**使用混合架构（REST API + SSE + WebSocket），这带来了以下问题：

- **协议碎片化**: Web UI 用 `POST /api/chat` + `SSE /api/events`；CLI 用裸 HTTP；WebSocket 存在但未被充分利用。
- **鉴权不一致**: Web 端用 OAuth2，CLI 无鉴权，WS 用 query token。
- **多端接入困难**: 接入 App 或 CLI 需要重新实现两套传输层。

转向 **WebSocket-native RPC 协议** 可解决这些问题：

| 问题 | 之前 | 之后 |
|---------|--------|-------|
| 传输层 | REST + SSE + WS | 单一 WebSocket |
| 鉴权 | OAuth2 / 无 / token | 统一设备配对 + 作用域 |
| 前端 | 自建 React | `assistant-ui` |
| 配置管理 | Web UI + REST API | 仅 CLI (`syscity config`) |
| 多端接入 | 每个客户端单独集成 | Web/App/CLI 共享一套协议 |

### 1.2 设计原则

1. **WebSocket 为主，HTTP 为辅**: 所有实时和交互式 API 走 WebSocket。HTTP 仅保留 OpenAI 兼容端点 (`/v1/*`) 和健康探针。
2. **统一鉴权**: 每个客户端（Web、App、CLI）使用相同的鉴权方式。
3. **配置仅限 CLI**: 管理/配置类 API 从 Web 面移除。管理员通过 `syscity` 二进制或配置文件配置 Syscity。
4. **原生集成 Assistant-UI**: Web 前端基于 `assistant-ui` 重建，直接消费 WebSocket 协议。

---

## 2. 传输层

### 2.1 WebSocket 端点

```
ws://<host>:<port>/ws
wss://<host>:<port>/ws
```

Query 参数：

| 参数 | 必填 | 说明 |
|-------|----------|-------------|
| `token` | 否 | 共享鉴权 token 或设备 token |
| `session_id` | 否 | 连接时预订阅某个 session |
| `client` | 否 | 客户端标识: `web`, `ios`, `android`, `cli` |
| `version` | 否 | 客户端请求的协议版本 |

### 2.2 保留的 HTTP 端点

| 方法 | 路径 | 用途 |
|--------|------|---------|
| GET | `/health` | 存活探针 |
| GET | `/ready` | 就绪探针 |
| POST | `/v1/chat/completions` | OpenAI 兼容 API (SSE 流式) |
| GET | `/v1/models` | OpenAI 兼容模型列表 |
| GET | `/api/v1/artifacts/*path` | 文档预览产物（`write_report` 生成的报告） |

> 其余 `/api/*` 与 `/api/v1/*` 端点已在迁移中移除；上表就是保留的全部 HTTP 面。

**产物 URL 形态**（通配路由，只读）：

- `/api/v1/artifacts/@<agent_id>/<file>` — 某个 Agent 工作区内的 artifacts（`@default` = 共享默认工作区）
- `/api/v1/artifacts/@<agent_id>/<root>/<task>/<file>` — 该 Agent 工作区内的 delegation 作用域报告
- `/api/v1/artifacts/[<root>/<task>/]<file>` — 旧版全局 `~/.syscity/artifacts/` 目录（向后兼容，迁移窗口内仍可访问）

路径段均经过遍历防护校验（拒绝绝对路径、`..`、反斜杠及百分号编码变体）；`@owner` 段仅允许 agent id 字符集（字母数字、`-`、`_`）。

---

## 3. 消息帧格式

所有 WebSocket 消息均为 JSON text 帧，采用可辨识联合类型（discriminated union）。

### 3.1 客户端 → 服务端: `req`

```json
{
  "type": "req",
  "id": "req_abc123",
  "method": "chat.send",
  "params": { ... }
}
```

| 字段 | 类型 | 必填 | 说明 |
|-------|------|----------|-------------|
| `type` | `"req"` | 是 | 帧类型辨识字段 |
| `id` | string | 是 | 客户端生成的请求 ID（用于匹配 `res`） |
| `method` | string | 是 | 方法名，点号命名空间 |
| `params` | object | 否 | 方法特定参数 |

### 3.2 服务端 → 客户端: `res`

```json
{
  "type": "res",
  "id": "req_abc123",
  "ok": true,
  "payload": { ... }
}
```

错误时：

```json
{
  "type": "res",
  "id": "req_abc123",
  "ok": false,
  "error": {
    "code": "INVALID_SESSION",
    "message": "Session not found"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|-------|------|----------|-------------|
| `type` | `"res"` | 是 | 帧类型辨识字段 |
| `id` | string | 是 | 与对应 `req.id` 一致 |
| `ok` | boolean | 是 | 成功标志 |
| `payload` | any | 否 | 响应数据（`ok: true` 时） |
| `error` | object | 否 | 错误详情（`ok: false` 时） |

### 3.3 服务端 → 客户端: `event`

```json
{
  "type": "event",
  "event": "chat.delta",
  "payload": { ... },
  "seq": 42
}
```

| 字段 | 类型 | 必填 | 说明 |
|-------|------|----------|-------------|
| `type` | `"event"` | 是 | 帧类型辨识字段 |
| `event` | string | 是 | 事件名称 |
| `payload` | any | 否 | 事件特定数据 |
| `seq` | integer | 否 | 单调递增序列号，用于排序/去重 |

---

## 4. 连接生命周期

### 4.1 握手流程

```
客户端                          服务端
  |                               |
  |  ---- 1. WebSocket upgrade ---|->
  |                               |
  |  <--- 2. Connected ---------- |  (协议已接受)
  |                               |
  |  ---- 3. connect req --------|->
  |     { auth, device, scopes }  |
  |                               |
  |  <--- 4. hello-ok res ------- |  (鉴权结果 + 特性列表)
  |                               |
  |  ==== 5. 正常业务流量 ========|
  |                               |
```

**第 1 步**: 客户端向 `/ws` 发起 WebSocket 连接。

**第 2 步**: 服务端接受 upgrade。如果协议版本不匹配，以 code `1002` 关闭连接。

**第 3 步**: 客户端 **必须** 将 `connect` 请求作为第一条 `req` 帧发送：

```json
{
  "type": "req",
  "id": "conn_1",
  "method": "connect",
  "params": {
    "protocol_version": 1,
    "client": {
      "id": "web",
      "version": "1.0.0"
    },
    "auth": {
      "token": "syscity_shared_token_<redacted>"
    },
    "device": {
      "id": "device_abc",
      "public_key": "...",
      "signature": "...",
      "nonce": "..."
    },
    "scopes": ["chat", "read"]
  }
}
```

**第 4 步**: 服务端回复 `hello-ok` 或 `connect.error`：

```json
{
  "type": "res",
  "id": "conn_1",
  "ok": true,
  "payload": {
    "protocol_version": 1,
    "session_key": "web:web_user",
    "features": ["chat", "canvas", "tools"],
    "scopes_granted": ["chat", "read"]
  }
}
```

鉴权失败时：

```json
{
  "type": "res",
  "id": "conn_1",
  "ok": false,
  "error": {
    "code": "UNAUTHORIZED",
    "message": "Invalid or missing authentication"
  }
}
```

### 4.2 Ping / Pong

客户端应定期发送 `ping` 请求：

```json
{ "type": "req", "id": "p_1", "method": "ping" }
```

服务端回复：

```json
{ "type": "res", "id": "p_1", "ok": true, "payload": {} }
```

若 60 秒内未收到任何帧，任一方可主动关闭连接。

### 4.3 重连

意外断开后，客户端应：

1. 指数退避：起始 800ms，上限 15s，乘数 1.7x
2. 使用相同的 `device.id` 重新发送 `connect`（若已配对）
3. 重新订阅之前活跃的 sessions

---

## 5. 鉴权与授权

### 5.1 鉴权模式（服务端配置）

通过 `config.toml` 的 `[security] auth_mode` 配置：

| 模式 | 说明 |
|------|-------------|
| `none` | 无鉴权。仅用于本地开发。 |
| `token` | 共享 secret token。CLI 从 `SYSCITY_GATEWAY_TOKEN` 环境变量或配置的 `security.shared_token` 读取（见 `docs/security-config.md`）；网关自身只从配置文件读 |
| `device` | 需要设备配对。新设备须经管理员批准。 |
| `tailscale` | 通过 Tailscale identity header 自动鉴权 |

### 5.2 鉴权流程

**共享 Token 模式**（最简单）：

1. 客户端在 `connect` 参数中发送 `auth.token`。
2. 服务端与配置的共享 token 进行校验。
3. 授予请求的作用域（或默认值）。

**设备配对模式**（推荐用于多端）：

1. 首次连接：客户端发送 `device.id` + `device.public_key` + 签名。
2. 服务端检查设备是否已配对。
3. 若未配对：
   - 服务端生成一个短配对码（如 `A3F7K`）。
   - 向管理员客户端广播 `device.pair.requested` 事件。
   - 管理员通过 CLI 执行 `syscity device approve <code>`。
4. 批准后，服务端向客户端颁发 **设备 token**。
5. 后续重连使用设备 token，无需再使用共享 token。

### 5.3 作用域（Scopes）

所有 API 方法均受作用域保护。默认拒绝：无作用域 = 无权限。

| 作用域 | 可访问接口 |
|-------|-----------------|
| `chat` | `chat.send`, `chat.abort`, `ask.respond` |
| `read` | 只读查询: `chat.history`, `sessions.list`, `agents.list` 等 |
| `write` | 创建/修改: `sessions.create`, `sessions.delete`, `sessions.subscribe` |
| `admin` | 完全访问（绕过所有作用域检查） |
| `pairing` | 设备配对管理 |

作用域校验按方法执行。`connect` 请求声明请求的作用域；服务端授予「请求的作用域」与「允许的作用域」的交集。

**权威来源是 `methods.list`。** 网关把它维护的方法表（`src/gateway/protocol.rs` 的
`METHOD_SCOPES`）直接发布出来——每个方法的名称与所需作用域，一处维护、可被客户端
程序化读取；一张测试同时保证它与分发器的分支一一对应。下面的接口表是按功能组织的
导读，可能有滞后，遇到不一致时以 `methods.list` 为准。

---

## 6. API 接口

### 6.1 普通使用 API（WebSocket）

这些方法对所有客户端（Web、App、CLI）开放，只需具备相应作用域。

#### 协议自省

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `methods.list` | `read` | 列出全部 WS 方法及其所需作用域（协议版本、方法与作用域的对照表） |

#### 聊天

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `chat.send` | `chat` | 向 Agent 发送消息 |
| `chat.history` | `read` | 获取某个 session 的对话历史 |
| `chat.abort` | `chat` | 中止当前生成 |
| `ask.respond` | `chat` | 回答 `ask_user` 工具挂起的问题（human-in-the-loop） |

#### Sessions

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `sessions.list` | `read` | 列出活跃 sessions |
| `sessions.create` | `write` | 创建新 session |
| `sessions.delete` | `write` | 删除 session |
| `sessions.reset` | `write` | 清空 session 上下文 |
| `sessions.subscribe` | `write` | 订阅 session 事件 |
| `sessions.unsubscribe` | `write` | 取消订阅 session 事件 |

#### Agents

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `agents.list` | `read` | 列出可用 Agents |
| `agents.get` | `read` | 获取 Agent 详情 |

#### Workspace（Agent 工作区文件浏览）

只读访问某个 Agent 的工作区目录，供 Web UI 文件浏览器使用。客户端路径始终相对于解析后的工作区根目录，并经过遍历防护校验（段级校验 + 规范化前缀比对，拒绝绝对路径、`..`、反斜杠与符号链接逃逸）。`agent_id` 缺省/空/`"default"` 解析为默认 Agent 的共享工作区；未知 Agent 返回 `AGENT_NOT_FOUND`。

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `workspace.list` | `read` | 列出目录内容（每目录最多 1000 条；目录在前，按名称排序） |
| `workspace.read` | `read` | 读取文本文件内容（超过 256 KiB 截断并置 `truncated: true`；二进制文件返回 `binary: true` 且无 `content`） |

`workspace.list` 参数：`{ agent_id?, path? }`；响应含 `root`、`path`、`entries`（每项为 `name` / `path` / `kind` / `size` / `modified`）。工作区目录不存在时返回空列表而非错误。

`workspace.read` 参数：`{ agent_id?, path }`（`path` 必填）；响应含 `path`、`size`、`truncated`、`binary`、`content`（仅文本）。路径越界返回 `PATH_FORBIDDEN`，不存在返回 `NOT_FOUND`。

#### 状态与在线

| 方法 | 作用域 | 说明 |
|--------|-------|-------------|
| `health` | (无) | 快速健康检查 |
| `system.presence` | `read` | Gateway 在线/存在状态 |

#### 事件（服务端推送）

| 事件 | 说明 |
|-------|-------------|
| `chat.delta` | 流式内容分片 |
| `chat.final` | 生成完成 |
| `chat.error` | 生成错误 |
| `tool.calling` | 工具执行开始 |
| `tool.result` | 工具执行完成 |
| `agent.thinking` | Agent 正在生成（打字指示器） |
| `session.created` | 自动创建了新 session |
| `device.pair.requested` | 新设备等待批准 |
| `ask.required` | `ask_user` 工具挂起等待人类回答（payload: `ask_id`, `session_id`, `question`, `options`, `required`, `default`） |
| `ask.resolved` | 问题已被回答或取消（payload: `ask_id`, `cancelled`） |
| `agent.usage` | 一轮 LLM 调用结束时的 token 用量（payload: `session_id`, `agent_id`, `usage: {prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, cache_creation_tokens}`）；每轮推一次，客户端据此在运行中显示实时 token 计数（credits 计量在 `chat.final` 上） |
| `delegation.updated` | 委派任务行变更（创建/状态/用量/共享状态/artifact）（payload: `session_id`（树的**根用户会话**，非子任务的 `delegation:<run_id>`）、`task_id`, `root_id`, `parent_id`, `depth`, `agent_id`, `title`, `status`, `created_at`, `updated_at`, `completed_at`, `usage_tokens`, `duration_ms`；终态行带 `duration_ms`，运行中为 `null`）。payload 自足，客户端无需按事件回查 |

### 6.2 管理 API（仅限 CLI）

这些操作从 WebSocket 协议和 HTTP 面中 **移除**。仅通过 `syscity` CLI 二进制可用。

```bash
# Agent 管理
syscity agent list
syscity agent create --name myagent --model gpt-4
syscity agent delete <id>

# Provider 管理
syscity provider list
syscity provider enable <id>
syscity provider disable <id>
syscity provider switch <alias>

# Plugin 管理
syscity plugin list
syscity plugin reload
syscity plugin enable/disable <id>

# Skill 管理
syscity skill list
syscity skill run <id>

# Cron 管理
syscity cron list
syscity cron add --schedule "0 9 * * *" --prompt "Daily summary"
syscity cron remove/enable/disable <id>

# Memory 管理
syscity memory search <query>
syscity memory add --content "..."

# 配置
syscity config get
syscity config set key=value
syscity config validate

# 初始化配置（交互式向导）
syscity setup
# 引导用户完成：
#   - 选择鉴权模式（none/token/device）
#   - 设置共享 token / 密码
#   - 配置默认模型和 Provider
#   - 设置数据目录路径
#   - 启用/禁用内置 Channel（web/cli/telegram 等）
#   - 生成初始 `config.toml` 并写入磁盘

# 设备配对
syscity device list
syscity device approve <code>
syscity device revoke <id>

# 审批（高危工具）
syscity approval list
syscity approval approve/deny <id>

# 审计
syscity audit log

# 安全
syscity security gate set <user> <level>
syscity security pairing list
syscity security pairing approve <channel> <code>
```

**理由**: 管理操作仅限管理员、频率低、需要严格校验。保留在 CLI 中可确保：

- Web UI 不会误触管理操作。
- 前端代码更简洁（无需管理后台）。
- 便于通过 shell 脚本自动化。
- 降低 Web 面的攻击面。

---

## 7. 前端: Assistant-UI 集成

### 7.1 Transport 适配器

`assistant-ui` 支持自定义 transport。Syscity 提供 `SyscityWebSocketTransport` 适配器，负责：

1. 管理 WebSocket 连接（连接、重连、鉴权）。
2. 将 `assistant-ui` 的消息流映射到 `chat.send` + `chat.delta`/`chat.final` 事件。
3. 通过 `tool.calling` / `tool.result` 事件暴露工具调用。
4. 管理 session 生命周期（创建、重置、切换）。

### 7.2 消息映射

| Assistant-UI 概念 | Syscity 协议 |
|---------------------|----------------|
| `Message` | `chat.final` payload |
| `TextStreamPart` | `chat.delta` 事件 |
| `ToolCall` | `tool.calling` 事件 |
| `ToolResult` | `tool.result` 事件 |
| `Thread` | Session |
| `CreateThread` | `sessions.create` |
| `DeleteThread` | `sessions.delete` |
| `AppendMessage` | `chat.send` |
| `CancelRun` | `chat.abort` |

### 7.3 Session 派生

Session key 统一采用 `{channel}:{user_id}` 格式：

- **Web**: `web:{device_id}`（从设备身份派生）
- **App (iOS/Android)**: `ios:{device_id}` / `android:{device_id}`
- **CLI**: `cli:{device_id}`

当调用 `chat.send` 且未显式指定 session 时，服务端自动创建或复用由 `channel + user_id` 派生的 session。

---

## 8. 迁移路线

### 第一阶段: 协议规范（本文档）

- 定稿 `docs/protocol.md`。
- 评审并冻结方法/事件名称。

### 第二阶段: WebSocket Gateway

- 实现新的 `req`/`res`/`event` 帧处理器。
- 实现 `connect` 握手 + 鉴权 + 作用域校验。
- 在 WebSocket 上实现所有 **普通使用 API** 方法。
- 废弃（但保留）现有的 REST API。

### 第三阶段: CLI 管理命令

- 将所有 **管理 API** 命令加入 `syscity` CLI。
- 从 REST 面中移除管理端点。
- 确保配置持久化到磁盘（而非仅内存）。

### 第四阶段: 前端重写

- 用 `assistant-ui` 替换现有 Web 前端。
- 实现 `SyscityWebSocketTransport`。
- 移除所有 Admin UI 组件。

### 第五阶段: 清理

- 移除废弃的 REST 端点。
- 移除 SSE handler。
- 移除旧前端代码。
- 更新文档。

---

## 9. 附录

### 9.1 错误码

| 错误码 | HTTP 等价 | 含义 |
|------|-----------|---------|
| `UNAUTHORIZED` | 401 | 鉴权缺失或无效 |
| `FORBIDDEN` | 403 | 作用域不足 |
| `INVALID_REQUEST` | 400 | 请求参数错误 |
| `METHOD_NOT_FOUND` | 404 | 未知方法名 |
| `SESSION_NOT_FOUND` | 404 | Session 不存在 |
| `AGENT_NOT_FOUND` | 404 | Agent 不存在 |
| `RATE_LIMITED` | 429 | 请求过于频繁 |
| `REVISION_CONFLICT` | 409 | 配置乐观锁冲突：`config.set` 携带的 `base_revision` 已过期，payload 含 `current_revision` / `expected_revision`，重新 `config.get` 后重试 |
| `PATH_FORBIDDEN` | 403 | 路径越出允许的根目录（如 workspace 浏览） |
| `INTERNAL_ERROR` | 500 | 服务端错误 |

### 9.2 协议版本控制

- 协议版本为整数（从 1 开始）。
- 服务端接受 `protocol_version <= server_version` 的连接。
- 若客户端版本过低，服务端回复 `connect.error`，错误码为 `VERSION_MISMATCH`。

### 9.3 向后兼容

迁移窗口已经结束：`/ws` 是主协议，REST 只剩上面列出的几类例外，它们不再标记
为废弃，也不会被移除（OpenAI 兼容端点和 webhook 是外部工具/平台硬依赖的路径）。
本节的其余内容保留为当时的过渡设想。

---

## 10. 对比: 之前 vs 之后

| 维度 | 之前（迁移前） | 之后（本规范） |
|--------|-----------------|-------------------|
| 传输层 | REST + SSE + WS | WebSocket-native |
| 鉴权 | OAuth2 / 无 / token | 统一: token + 设备配对 + 作用域 |
| Web 前端 | 自建 React | `assistant-ui` |
| 配置 UI | Web dashboard + REST | 仅 CLI |
| 多端接入 | 每个客户端单独集成 | 一套协议走天下 |
| Session Key | 随机 UUID | `{channel}:{user_id}` |
| 事件推送 | SSE 广播 | WebSocket `event` 帧 |
| API 数量 | 100+ REST 端点 | ~15 个 WS 方法 + CLI 命令 |
| 管理面 | Web + REST | 仅 CLI |
