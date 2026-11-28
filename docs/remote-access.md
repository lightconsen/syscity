# 远程访问（Remote Gateway Access）

本文说明如何让 Syscity Gateway 监听远程客户端，以及客户端如何连接远程 Gateway。

## 场景

- 在办公室/家里的机器上跑 Gateway，人在外面用 Desktop / Mobile / 浏览器连过来。
- 多个设备共享同一个 Gateway（同一份会话、配置、渠道），而不是各跑一个。

## 一、开启远程监听（Gateway 侧）

### 1. 绑定所有网卡

Gateway 默认只监听 `127.0.0.1`。要接受远程连接，绑定 `0.0.0.0`：

```bash
syscity start --host 0.0.0.0
```

> ⚠️ `config.toml` 里的 `host` 会被 `syscity start` 的 `--host` **覆盖**：CLI 参数带有
> 默认值（`127.0.0.1`），因此不加 `--host 0.0.0.0` 时无论文件里写了什么，都只监听回环。
> （配置文件本身是会被解析的——顶层的 `host`/`port` 由 CLI 覆盖，其余字段以文件为准。）

### 2. 开启鉴权（必须）

远程监听默认 `auth_mode = "none"`，**裸奔**。在 `config.toml` 的 `[security]` 里启用：

```toml
[security]
auth_mode = "token"
shared_token = "<你的token>"
```

`[security]` 段会被正常解析；也可以用同名环境变量覆盖（`credential_precedence` 决定谁优先）：

```bash
SYSCITY_SECURITY_AUTH_MODE=token \
SYSCITY_SECURITY_SHARED_TOKEN=<你的token> \
syscity start --host 0.0.0.0
```

- `SYSCITY_SECURITY_AUTH_MODE`：`none` | `token` | `device` | `tailscale`
- `SYSCITY_SECURITY_SHARED_TOKEN`：客户端需要出示的共享令牌

### 2b. 让某个客户端只连一个 agent（Agent Token）

想让别人用自己的电脑连接这台 Gateway，但**只能使用某一个 agent**，就为该 agent 发一个
agent token，而不必给他共享令牌（共享令牌能看到所有 agent）：

```toml
[security]
auth_mode = "token"

[security.agent_tokens.alice]
# token 建议走环境变量：SYSCITY_AGENT_TOKEN_ALICE=...
agent_id = "secretary"
# principal 留空 = 使用表标签（alice），对应 [agent_overrides.secretary] allowed_users
# scopes 默认 ["chat", "read"]——这是刻意的：agent token 的持有人不是运维本人
```

要点：

- **必须** `auth_mode = "token"`（或 `device`）。`auth_mode = "none"` 下不检查任何凭据，
  `agent_tokens` 会让启动直接报错，而不是看起来"限制了"。
- token 放在环境变量 `SYSCITY_AGENT_TOKEN_<LABEL>`（标签大写、`_` 可代 `-`），
  不要写进会被备份 / `git add -A` 的文件。
- 连接建立后 Gateway 在 `connect` 握手里返回 `bound_agent`，客户端据此隐藏 agent 切换器。
- 被绑定的连接能调用的是"单 agent"方法集（聊天、会话、审批、模型选择等），
  `config.set` / `agents.update` / `computer.run` 等运维方法一律 `FORBIDDEN`；
  请求里出现别的 `agent_id` / `session_id` 也会被拒绝。

> 更多鉴权模式（Device 配对、Tailscale）见 `docs/security-config.md`。

### 3. 防火墙

确保机器防火墙放行 `18080`（或你配置的端口），并且该端口对目标网络可达。

### 4. 验证

```bash
curl --noproxy "*" http://127.0.0.1:18080/live   # 200
# WS 握手：
#   无 token → 401
#   带 Authorization: Bearer <token> → 101
```

## 二、客户端连接远程 Gateway

客户端启动时选择「连远程 Gateway」或「本地运行」。远程模式需要填：地址（host:port）+ token，
可勾选 TLS 走 `https://` / `wss://`。token 可以是共享令牌，也可以是某个 agent 的令牌
（见 2b），填 agent 令牌时客户端只会列出并使用那个 agent。

| 客户端 | 状态 |
|---|---|
| **Desktop** | 首次启动即可连远程（欢迎页「选择网关」，或启动页的「检查连接设置」）；设置页「Connection」同样可选本地/远程（`~/.syscity/client.toml`）；远程模式连接远程并携带 token，支持 TLS 与 agent token |
| **Mobile** | 与 Desktop 相同（首次启动与设置页 Connection，配置存应用沙盒）；本地模式用内嵌 gateway + per-install token |
| **Web（浏览器）** | 两种方式：直接打开远程 Gateway 的 UI（同源），或「Connection」设置里填远程 base URL + token（存 localStorage）作为独立客户端 |

首次启动时连远程，走的是同一个 Connection 表单，两点区别值得知道：

- **保存后需要重启**。连接（地址、凭据、端口）在启动时确定，改完表单点「保存并重启」；
  只保存不重启，下一次启动才生效。
- **远程模式不会回退到本地 gateway**。地址填错或网关没起来，界面会停在启动页——
  所以启动页在等待超过 6 秒后会给出「检查连接设置」入口。表单本身不依赖网关
  （数据来自 `get_connection` 命令），网关连不上也能改。无法连接的配置（空 host、
  把 `http://` 粘进 host 字段、端口为 0）在保存时就被拒绝，不会写进 `client.toml`。

Desktop 的 Connection 设置里勾选 **Use TLS** 后，会以 `https://` / `wss://` 连接——Gateway
本身不带 TLS 监听，这个开关是给前置反向代理（Caddy / nginx / Cloudflare Tunnel 等）用的。

### 安全建议

- 局域网内可用明文 `http://`。
- 跨公网必须走 `https://`（WSS）或 Tailscale/VPN，避免 token 明文暴露。
- 远程模式下客户端填写的 token 要么是 Gateway 的 shared token，要么是某个
  `[security.agent_tokens.<label>]` 里的 agent token；由 Gateway 判断是哪种，
  客户端无法把 agent token "升格"为共享令牌。

## 三、已知问题

- **`--host` 会覆盖 `config.toml` 的 `host`**：`syscity start` 的 `--host`/`--port`
  带有默认值，且无条件写入生效配置，因此只改文件里的 `host` 不会改变监听地址。
  其余字段（含整个 `[security]` 段）以文件为准。
