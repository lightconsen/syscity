# Syscity 环境感知 Agent / Managed Sensor 设计文档

> **定位**：经过授权、可审计、可卸载的环境感知 Agent / Managed Sensor  
> **目标**：驻留在主机或受控网络中，持续建立环境模型，并提供运维、安全、资产、故障诊断和自动化服务。  
> **原则**：显式安装、明确授权、最小权限、默认只读、持续审计、可撤销、可卸载、不自传播、不绕过安全控制。

---

## 1. 摘要

Syscity 可以从“本地 AI Agent Runtime”演进为一个**环境感知 Agent 平台**：在经过授权的笔记本、服务器、业务系统、Kubernetes 集群、云账户或受控网络区域中部署 Agent/Managed Sensor，持续收集经过许可的环境事实，形成带来源、时间和置信度的环境模型，并通过 Agent 提供：

- 主机和软件资产管理；
- 网络和服务拓扑理解；
- 配置与版本基线；
- 故障诊断和影响分析；
- 安全基线检查；
- 变更前评估和变更后验证；
- 受审批的自动化运维；
- 资源、性能、容量和成本分析；
- 业务系统和基础设施的自然语言查询。

该系统不应设计为隐蔽植入、自动复制或无边界扫描程序。对于企业和生产环境，正确的产品形态是：

```text
安装 → 注册 → 授权 → 观察 → 建模 → 分析 → 建议 → 审批 → 执行 → 验证 → 审计 → 撤销/卸载
```

而不是：

```text
未知驻留 → 自主扩散 → 无限探测 → 无审批控制
```

---

## 2. 目标与非目标

### 2.1 产品目标

1. **理解环境**：建立主机、网络、服务、应用、身份和变更的结构化模型。
2. **服务运维**：回答环境问题，发现异常，定位故障，生成操作计划。
3. **辅助安全**：执行授权范围内的资产盘点、基线检查、配置核验和风险分析。
4. **安全自动化**：在批准后执行有限、可验证、可回滚的运维动作。
5. **持续可见**：让管理员始终知道 Agent 在哪里、拥有何种能力、观察了什么、执行了什么。
6. **可控生命周期**：支持暂停、撤销、隔离、升级、降权和卸载。

### 2.2 非目标

以下能力不属于本产品目标，也不应作为隐含能力实现：

- 未经授权的主机或网络探测；
- 自主复制、横向传播或隐蔽安装；
- 绕过 EDR、杀毒、审计或访问控制；
- 隐藏进程、隐藏文件、隐藏网络连接或删除痕迹；
- 未经审批的凭据收集、权限提升或 root 扩张；
- 对整个公网进行无边界扫描；
- 将任意自然语言请求直接转换为高风险生产变更；
- 将所有主机数据、日志和秘密默认上传给模型。

---

## 3. 适用环境

| 环境 | 可行性 | 推荐形态 | 主要约束 |
|---|---:|---|---|
| 笔记本/个人电脑 | 高 | 本地 Endpoint Agent | 文件隐私、桌面控制、用户授权 |
| 单台服务器 | 高 | 受管系统服务 | 最小权限、生产变更、回滚 |
| 多服务器业务系统 | 中高 | 每节点 Agent + 控制面 | 节点身份、版本、租户和数据隔离 |
| 企业网络 | 中高 | 网络区域 Collector + Endpoint Agent | CIDR 授权、限速、拓扑敏感性 |
| Kubernetes 集群 | 高 | ServiceAccount + Namespace/Cluster Connector | RBAC、Secret、容器边界 |
| 云账户 | 高 | Cloud API Connector + 最小 IAM | 账户范围、成本和凭据安全 |
| 生产系统 | 中高 | 默认只读 + 变更审批 | 可用性、维护窗口、补偿/回滚 |
| 受控互联网测量范围 | 中 | 多测量点 + 明确资产清单 | 合规、速率、误伤、数据量 |
| 无边界公网探测 | 不推荐 | 不作为默认产品能力 | 未授权侦察、滥用和合规风险 |

“完整理解环境”不应理解为“无限制地读取和扫描一切”。实际理解范围应由授权的资源集合、数据源、时间窗口和能力策略共同定义。

---

## 4. 总体架构

```text
┌─────────────────────────────────────────────────────────────┐
│                    Policy / Control Plane                    │
│  租户 · 环境 · 节点 · 身份 · 授权 · 任务 · 审批 · 审计 · 撤销 │
└──────────────────────────┬──────────────────────────────────┘
                           │
          ┌────────────────┼────────────────┐
          │                │                │
┌─────────▼────────┐ ┌─────▼──────────┐ ┌───▼──────────────┐
│ Endpoint Agent    │ │ Network Zone   │ │ Platform         │
│ 笔记本/服务器      │ │ Collector      │ │ Connectors       │
│                   │ │ 受控网段/区域    │ │ K8s/Cloud/CMDB   │
└─────────┬─────────┘ └─────┬──────────┘ └───┬──────────────┘
          │                 │                │
          └─────────────────┼────────────────┘
                            │
              ┌─────────────▼─────────────┐
              │ Environment Knowledge     │
              │ Graph / Inventory / State │
              │ Facts / Changes / Alerts  │
              └─────────────┬─────────────┘
                            │
              ┌─────────────▼─────────────┐
              │ Agent Runtime             │
              │ Retrieval / Planning      │
              │ Diagnosis / Recommendation│
              │ Approved Automation       │
              └─────────────┬─────────────┘
                            │
              ┌─────────────▼─────────────┐
              │ Clients and Services      │
              │ Web / TUI / CLI / Chat    │
              │ Ticket / Alert / Reports  │
              └───────────────────────────┘
```

### 4.1 Endpoint Agent

部署在明确授权的笔记本、服务器或虚拟机上，负责：

- 主机身份和操作系统信息；
- 硬件、磁盘、进程和服务状态；
- 网络接口、路由、DNS、代理和连接状态；
- 已授权目录中的文件和配置；
- 日志、指标和事件；
- 经过审批的本机运维动作。

Endpoint Agent 不应默认拥有全盘读取、任意 Shell 或桌面控制能力。每一项能力均应显式声明、授权和审计。

### 4.2 Network Zone Collector

部署在经过授权的网络区域、VPC、数据中心或测试网段中，负责：

- 读取已有网络管理和监控数据；
- 执行受控的服务健康检查；
- 对授权 CIDR 进行有限资产测量；
- 记录 DNS、TLS、路由和服务元数据；
- 将观测结果上传到环境模型。

Collector 必须具有独立身份和明确的授权范围，不得通过“发现新网段”自动扩大扫描范围。

### 4.3 Platform Connectors

优先使用受控 API，而不是通过 Shell 模拟管理员操作：

- Kubernetes API；
- 云平台资产 API；
- CMDB；
- Prometheus/OpenTelemetry；
- DNS 和证书管理；
- Git/CI/CD；
- 负载均衡和服务注册中心；
- 工单、告警和发布系统；
- 数据库健康检查 API。

每个 Connector 使用独立凭据、最小 IAM/RBAC 范围和资源过滤条件。

### 4.4 Knowledge Plane

环境知识平面保存结构化事实、历史变化、关系和证据，不能只依赖聊天 transcript。

它应支持：

- 当前环境快照；
- 历史版本和变更 diff；
- 资产和服务关系图；
- 事件、告警和故障上下文；
- 事实来源和置信度；
- 数据保留、删除和导出；
- 按租户、环境、区域、节点和主体隔离。

### 4.5 Agent Plane

Agent 负责：

- 查询和解释环境事实；
- 关联日志、指标、变更和拓扑；
- 生成诊断假设；
- 规划检查或修复步骤；
- 请求必要的额外授权；
- 在获批后执行动作；
- 验证执行结果并生成报告。

LLM 不应直接决定越权读取、扩大扫描、获得 root、禁用审计或绕过审批。确定性 Policy Engine 必须位于 Agent 与能力层之间。

---

## 5. 环境身份与注册

每个部署实例都应拥有稳定、可撤销的身份：

```text
tenant_id
  └── environment_id
      └── site_id
          └── zone_id
              └── node_id
                  └── agent_instance_id
```

注册信息建议包括：

- `tenant_id`；
- `environment_id`；
- `node_id`；
- 设备公钥或证书；
- Agent 版本；
- 平台和架构；
- 能力清单；
- 授权范围；
- 所属区域；
- 最后心跳时间；
- 撤销状态；
- 管理责任人；
- 数据保留策略。

不应只依赖：

- 全局 shared token；
- 客户端自报的 `client.id`；
- `UserId("shared")`；
- 长期 URL token。

需要支持：

- 初始注册批准；
- 短期访问凭据；
- 证书轮换；
- 撤销和强制下线；
- 节点隔离；
- 版本淘汰；
- 失联告警。

---

## 6. 能力模型与最小权限

Agent 应先声明能力，再根据策略启用能力：

```json
{
  "node_id": "node-123",
  "platform": "linux",
  "capabilities": [
    "system.read",
    "process.list",
    "network.interface.read",
    "network.route.read",
    "service.status.read",
    "filesystem.read:/srv/app"
  ],
  "not_available": [
    "desktop.control",
    "packet.capture",
    "filesystem.read:/home/other-user",
    "system.write"
  ]
}
```

每项能力应包含：

- 唯一 ID；
- 读/写/执行分类；
- 资源范围；
- 生效时间和过期时间；
- 授权者；
- 审批级别；
- 数据敏感级别；
- 审计要求；
- 撤销方法。

建议能力命名：

```text
host.identity.read
host.os.read
host.hardware.read
host.processes.read
host.services.read
host.network.read
host.files.read:<scoped-path>
host.logs.read:<scoped-source>
network.healthcheck:<approved-service>
kubernetes.read:<namespace>
cloud.assets.read:<account/resource-group>
service.restart:<approved-service>
```

默认原则：

- Agent 默认只读；
- 默认不读全盘；
- 默认不抓包；
- 默认不做网络扫描；
- 默认不上传原始日志；
- 默认不执行任意 Shell；
- 高风险能力必须单独审批；
- 授权不能由模型自己扩大。

---

## 7. 观测分级

### L0：被动观察

风险最低，优先使用：

- 操作系统 API；
- 已有监控和日志；
- CMDB；
- 云平台资产 API；
- Kubernetes API；
- DNS、证书和路由数据；
- 事件和指标订阅。

### L1：受控服务检查

只对已登记资源执行：

- 健康检查；
- DNS 查询；
- TLS 证书检查；
- 固定健康路径请求；
- 已知服务版本查询；
- 只读 Kubernetes 查询。

### L2：授权网络测量

必须满足：

- 明确的 CIDR、资产组或服务清单；
- 明确的任务负责人；
- 最大速率和最大并发；
- 执行时间窗口；
- 影响预算；
- 可中止；
- 全量审计；
- 结果脱敏和保留期限。

### L3：高风险验证或变更

包括漏洞验证、生产配置修改、服务重启、流量切换等。必须具备：

- 显式审批；
- 变更单或任务 ID；
- 维护窗口；
- 变更前快照；
- 幂等性；
- 回滚或补偿动作；
- 执行后验证；
- 超时和 kill switch；
- 失败时的人工升级。

默认 Agent 只运行 L0/L1；L2 需要管理员批准；L3 不得由普通自然语言请求直接触发。

---

## 8. 环境知识模型

建议新增结构化实体：

```text
Environment
 ├── Hosts
 │    ├── OS / hardware
 │    ├── interfaces / routes / DNS
 │    ├── processes / services
 │    ├── packages / versions
 │    └── owners
 ├── Network Zones
 │    ├── CIDR
 │    ├── gateways
 │    ├── ACL boundaries
 │    └── observed services
 ├── Applications
 │    ├── dependencies
 │    ├── ports
 │    ├── deployment units
 │    └── health signals
 ├── Accounts / identities
 ├── Credential references
 ├── Alerts / incidents
 └── Change history
```

每个事实都必须带有：

- `observed_at`；
- `source`；
- `confidence`；
- `scope`；
- `expires_at`；
- `provenance`；
- `sensitivity`；
- `tenant_id`；
- `environment_id`；
- `node_id`。

示例：

```json
{
  "fact": "service.api listens on 10.0.4.12:8443",
  "source": "authorized_service_inventory",
  "observed_at": "2026-09-16T08:00:00Z",
  "confidence": 0.98,
  "scope": "zone-prod-a",
  "sensitivity": "internal"
}
```

LLM 不应把一次命令输出当成永久事实。每次回答都应尽量给出：

- 证据来源；
- 观测时间；
- 置信度；
- 是否可能过期；
- 未覆盖范围；
- 建议的下一步观察。

---

## 9. Agent 工作模式

### Observe

只观察，不改变目标环境：

- 资产盘点；
- 软件和版本清单；
- 配置基线；
- 服务健康；
- 性能和容量；
- 日志和事件；
- 网络连接和拓扑事实。

### Explain

解释环境和变化：

- “当前生产 API 依赖哪些服务？”
- “为什么最近两小时延迟增加？”
- “这次部署改变了哪些主机和配置？”
- “如果重启这个服务，哪些业务可能受到影响？”
- “哪些资产没有纳入基线？”

### Recommend

提出建议但不执行：

- 扩容建议；
- 配置修复建议；
- 补丁建议；
- 网络策略建议；
- 回滚计划；
- 监控和告警建议。

### Act

仅在获批后执行：

- 重启服务；
- 修改受控配置；
- 回滚部署；
- 切换流量；
- 更新 DNS；
- 清理受控临时资源；
- 生成并应用明确的变更计划。

标准变更计划：

```text
1. 读取 service-api 当前配置
2. 保存当前版本、健康状态和依赖快照
3. 修改一个明确配置项
4. 重启或重新加载 service-api
5. 执行健康检查和关键业务验证
6. 失败时恢复配置并执行回滚
7. 输出结果、证据和审计记录
```

---

## 10. 数据安全与隐私

环境感知会接触高度敏感数据：

- IP、拓扑和网络边界；
- 用户、账户和权限；
- 进程命令行；
- 业务服务和依赖；
- 日志和配置；
- 证书；
- token、Cookie、私钥和密码；
- 生产变更和故障信息。

建议分级：

```text
public
internal
confidential
restricted
secret
```

规则：

1. Secret、私钥、Cookie、token 不进入 LLM context；
2. 原始日志默认只在本地或授权存储中保留；
3. 上传给模型前执行 secret/PII 脱敏；
4. 环境、租户、区域、节点和 session 必须隔离；
5. 每条事实带来源和保留期限；
6. 支持按租户、环境、节点删除和导出；
7. 支持离线运行或仅本地模型路径；
8. 明确哪些数据会发送到外部 Provider、Cloud、MCP 或 Connector。

---

## 11. 生命周期与运维控制

安全的驻留必须是**可管理的服务生命周期**，而不是隐藏驻留：

```text
安装
  ↓
签名和完整性校验
  ↓
注册和人工批准
  ↓
能力授予
  ↓
持续观测
  ↓
配置/版本升级
  ↓
暂停、降权或隔离
  ↓
证书/token 撤销
  ↓
卸载与数据清理
```

必须提供：

- 官方签名安装包；
- 可见的服务名称和运行状态；
- `status`、`pause`、`revoke`、`uninstall`；
- kill switch；
- 证书和 token 轮换；
- 版本和能力清单；
- 健康检查与心跳；
- 运行日志；
- 资源预算；
- 网络出口白名单；
- 卸载后残留数据清理；
- 管理员可见的审计记录。

Agent 不得自行修改自身的撤销、审计、升级或卸载控制。

---

## 12. 对现有 Syscity 的演进路线

### 阶段一：单主机 Environment Agent

优先新增：

```text
EnvironmentProfile
CapabilityManifest
ObservationJob
ObservationScope
EnvironmentFact
FactSource
FactConfidence
ActionApproval
```

默认只读，覆盖：

- 主机身份；
- OS、硬件、磁盘；
- 进程和服务；
- 网络接口和路由；
- 授权目录；
- 系统资源；
- 受控日志源。

### 阶段二：平台 Connector

增加受控 Connector：

- Docker/Kubernetes；
- systemd/launchd/Windows Service；
- Cloud API；
- CMDB；
- Prometheus/OpenTelemetry；
- DNS/TLS；
- Git/CI/CD；
- 负载均衡；
- 工单和告警平台；
- 数据库健康 API。

每个 Connector 都应独立进行身份认证、范围授权、限流和审计。

### 阶段三：网络区域 Collector

为每个受控区域部署明确的 Collector：

```text
zone-a-collector
zone-b-collector
prod-vpc-collector
```

Collector 必须拥有：

- 独立身份；
- 明确 CIDR/资源范围；
- 受控测量模板；
- 最大并发和速率；
- 执行时间窗口；
- 任务审批；
- 详细审计；
- 自动停止；
- 版本完整性验证。

发现新网络范围时只能产生“待授权建议”，不能自动扫描新范围。

### 阶段四：环境知识图谱

将当前环境事实、历史变化、事件和依赖关系放入独立的 Knowledge Plane。不要把环境状态全部塞进会话历史或 prompt。

### 阶段五：受控自动化

先实现 Observe → Explain → Recommend，再逐步开放 Act。所有生产动作都必须有审批、快照、回滚、验证和审计。

---

## 13. 现有项目必须先解决的问题

在扩大到主机和网络环境之前，必须优先修复 Syscity 当前控制面风险：

1. 禁止 WS 客户端自授 `admin` scope；
2. 修复错误的 `method_scope`；
3. 修复 session/chat/artifact 的对象级授权；
4. 修复跨连接事件过滤；
5. 给 WS 增加连接级和消息级限流；
6. 降低 query token 的日志和代理泄露风险；
7. 将 Webhook 验证改为 fail-closed；
8. 明确 Agent、Tool、MCP、Plugin 的权限继承；
9. 建立 tenant/environment/zone/node/job scope；
10. 修复 pending RPC、断线恢复和任务撤销；
11. 建立观察数据的保留、删除和导出策略；
12. 用统一 Policy Engine 管理所有高风险动作。

如果不先解决这些问题，环境 Agent 会把现有的授权、数据隔离和事件泄露问题扩展到更多主机和更大网络范围。

---

## 14. 安全边界

本项目应坚持以下边界：

### 允许

- 经过授权的环境注册；
- 受控主机和网络区域的只读观察；
- 有范围的资产盘点；
- 受限健康检查；
- 环境事实建模；
- 故障诊断和运维建议；
- 审批后的变更；
- 变更验证和回滚；
- 公开、可见、可审计的服务驻留；
- 管理员主动撤销和卸载。

### 不允许

- 未授权安装或持久化；
- 自主传播或横向扩散；
- 隐藏进程、隐藏文件或隐藏网络连接；
- 凭据窃取；
- 权限提升；
- 绕过 EDR、审计或访问控制；
- 删除痕迹；
- 超出授权范围的扫描；
- 无边界公网探测；
- 由模型自行扩大权限或观测范围。

---

## 15. 最终判断

Syscity 完全可以演进为一个：

> **经过授权、可审计、可卸载的环境感知 Agent / Managed Sensor 平台。**

其核心产品形态可以是：

```text
Endpoint Agent
+ Network Zone Collector
+ Cloud/Platform Connectors
+ Environment Knowledge Graph
+ Policy Engine
+ Agent Planner
+ Approval & Audit Control Plane
```

可服务于：

- 笔记本和个人电脑助手；
- 服务器运维；
- 企业资产管理；
- Kubernetes 和云资源管理；
- 业务系统故障诊断；
- 生产系统 SRE；
- 经过授权的网络测量；
- 环境知识图谱和数字员工。

可行性评级：

| 场景 | 评级 |
|---|---:|
| 单主机环境 Agent | 高 |
| 单业务部署环境 | 高 |
| 多服务器业务系统 | 中高 |
| 企业受控网络 | 中高，需要多节点和强授权 |
| 生产系统自动化 | 中高，需要变更控制和回滚 |
| 受控网络区域测量 | 中，需要明确范围、限速和审计 |
| 无边界公网探测 | 不作为默认产品能力 |

最终建议是：先把 Syscity 做成**受管的环境数字员工**，从单主机只读观察开始，逐步扩展到 Connector、网络区域 Collector、环境知识图谱和审批后自动化；不要实现自传播、隐蔽驻留或无边界探测机制。

---

# 16. 工程落地路线：从现有 Syscity 到 Environment Agent

本节将前面的产品目标转换为可执行的代码、数据、控制面、部署和验证路线。核心顺序是：

```text
M0 安全闸门
  → M1 领域模型与持久化
  → M2 注册/身份/能力
  → M3 单主机只读采集
  → M4 观察任务与事实查询
  → M5 平台 Connector
  → M6 受控网络区域 Collector
  → M7 Recommend → Act
  → M8 生产部署与规模化
```

**必须先完成 M0，才能开启任何默认启用的环境采集能力。**

## 16.1 M0：安全闸门和控制面硬化

### 目标

在 Agent 接触更多主机、环境和网络资源之前，先保证 Gateway 控制面的身份、授权和撤销正确。

### 必须完成的工作

1. **服务端签发 scope**
   - 修复 `src/gateway/ws/handshake.rs`，客户端请求 scope 只能与服务端已授权 scope 求交集，不能增加权限；
   - shared token、device、session、Tailscale 不能依赖客户端声明的 `params.scopes`；
   - `admin` 不能由匿名或普通设备请求得到。

2. **统一 method scope manifest**
   - 修复 `src/gateway/protocol.rs` 中把模型变更、MCP 执行、Skill 安装和 pairing 信息读取标成 `read` 的问题；
   - 为环境方法预留 `environment.read`、`environment.write`、`sensor.register` 等细粒度 scope，或在现有 scope 层次中做明确映射；
   - 未知方法继续默认拒绝。

3. **对象级授权**
   - 为 session、agent、artifact、approval、environment、node、observation job 增加 owner/tenant/environment 检查；
   - 修复 `sessions.list`、`chat.history`、`chat.abort`、session CRUD 和 artifact 访问的 IDOR；
   - 不能只检查“连接具有 write scope”，还要检查“连接有权操作这个对象”。

4. **事件隔离与限流**
   - 事件增加 audience、environment、node、session 信息；
   - `ws/core.rs` 不能用 `_ => true` 将敏感事件广播给所有连接；
   - `/ws` 增加连接级、消息级、用户级和方法级限流；
   - 所有长期 observation/connector task 纳入 `TaskRegistry` 和 `CancellationToken`。

5. **Webhook、token 和数据边界**
   - Webhook 凭据缺失时 fail-closed；
   - query token 采用短期 ticket、日志脱敏或 header/mTLS；
   - 统一 secret/PII 脱敏；
   - 明确 facts、raw observations、transcripts 和 LLM context 的 retention/delete/export 策略。

### M0 验收闸门

- 两个不同主体不能互读、互改、互删 session/artifact/environment；
- 客户端发送 `admin` 不会提升权限；
- 越权 environment/node/job method 返回结构化拒绝；
- 事件只发给授权 audience；
- revoked registration 会立即停止新任务并取消可取消任务；
- 所有高风险请求都有 actor、scope、decision、result 和 audit entry；
- 没有 M0 通过证明前，不开启网络区域采集和写操作。

## 16.2 M1：领域模型、配置和持久化

### 新增模块建议

新增 `src/environment/`，第一版建议按以下边界拆分：

```text
src/environment/
├── mod.rs
├── model.rs              # typed IDs、profiles、registrations、grants
├── identity.rs           # tenant/environment/site/zone/node/agent identity
├── capabilities.rs       # capability manifest and grants
├── scope.rs              # path/CIDR/service/namespace/time/rate scope
├── facts.rs              # FactEnvelope, entities, relationships, evidence
├── observation.rs        # collector contract and observation batch
├── jobs.rs               # job/run state machine and cancellation
├── policy.rs             # deterministic allow/deny/needs-approval engine
├── registration.rs       # pending/approved/active/revoked lifecycle
├── redaction.rs          # secret/PII/sensitivity processing
├── store.rs              # EnvironmentStore repository facade
├── scheduler.rs          # observation scheduling facade
├── collectors/
│   ├── mod.rs
│   ├── host.rs
│   ├── network.rs
│   ├── filesystem.rs
│   └── services.rs
└── connectors/
    ├── mod.rs
    ├── kubernetes.rs
    ├── cloud.rs
    ├── cmdb.rs
    ├── observability.rs
    └── dns_tls.rs
```

### 核心数据类型

建议使用强类型 ID 和 serde tagged enums，避免在 JSON/String 中混用租户和节点身份：

```text
TenantScope
  tenant_id
  environment_id
  site_id?
  zone_id?
  node_id?
  job_id?

EnvironmentProfile
  id, name, kind, owner, status, retention, allowed_egress

SensorRegistration
  agent_instance_id, node_id, public_key/cert_fingerprint
  version, platform, capabilities, scopes
  heartbeat, last_seen, revoked_at

CapabilityGrant
  capability_id, read/write/execute kind, resource selector
  sensitivity, valid_from/to, approver, approval requirement
  audit policy, revocation state

ObservationScope
  explicit paths/services/CIDRs/namespaces
  max_rate, max_concurrency, time_window, impact_budget

ObservationJob / ObservationRun
  collector, level L0-L3, scope, trigger, status
  started_at, finished_at, cancellation, error, bounded run history

EnvironmentFact
  subject/predicate/object or typed payload
  observed_at, source, confidence, expires_at
  provenance, sensitivity, redaction status, scope

ActionRequest / ActionApproval / ActionRun / Verification
  action hash, exact args, actor, scope, approval, snapshot
  idempotency key, precondition, result, rollback, verification
```

### SQLite 迁移

使用现有 `src/memory/db.rs` 和 `src/agent/session_store/schema.rs` 的迁移/事务模式，新增表而不是把环境事实塞进普通 memory 或 transcript：

```text
environments
sensor_registrations
capability_grants
observation_scopes
observation_jobs
observation_runs
environment_entities
environment_facts
fact_evidence
environment_relationships
environment_changes
action_requests
action_approvals
action_runs
sensor_credentials_metadata
```

所有表至少需要：

- `tenant_id`、`environment_id`、必要时 `zone_id`/`node_id`；
- owner/audience 索引；
- observed/expiry/retention 索引；
- 状态和撤销索引；
- 外键和事务一致性；
- additive、可回滚的 schema migration。

凭据本体只保存于 `src/secrets/` 或外部 Secret/Vault；环境数据库只保存 SecretRef、指纹或元数据。

## 16.3 M2：注册、身份、能力和生命周期

### 注册状态机

```text
pending
  → approved
  → active
  → paused
  → isolated
  → revoked
  → uninstalled
```

每次状态变化必须：

- 有授权 actor；
- 生成审计事件；
- 使旧凭据失效或降权；
- 影响 observation jobs；
- 影响可用的 capability grants；
- 可从控制面查询。

### 控制面复用和扩展

复用：

- `src/security/device_pairing.rs` 的设备身份/批准思路；
- `src/security/auth_store.rs` 的持久化 session 模式；
- `src/security/persistent_audit.rs`；
- `src/security/request_context.rs`；
- `src/gateway/task_registry.rs`；
- `src/gateway/lifecycle.rs` 的 shutdown 结构；
- `src/config/types.rs` 的配置和 SecretRef 模式。

新增 WS handler：

```text
environment.register
 environment.register.list
 environment.register.get
 environment.register.approve
 environment.register.revoke
 environment.register.pause
 environment.register.resume
 environment.register.isolate
 environment.register.rotate
 environment.register.uninstall
 environment.status
 environment.capabilities
```

按项目 WS 约定完成：

1. `src/gateway/ws/admin_ws/environment.rs`；
2. `src/gateway/ws/admin_ws/mod.rs` 导出；
3. `src/gateway/ws/core.rs` dispatch；
4. `src/gateway/protocol.rs` scope；
5. 对应 `tokio::test` 和双主体隔离测试。

### Agent 不能只依赖现有 capability profile

当前 `src/config/types.rs` 的默认 capability profile/max scope 偏宽，Linux toolset 还会把系统观察和修改工具放在同一组。环境 Agent 应新增明确的：

```text
environment-observer
```

profile：

- default read-only；
- 不注册 service/package/firewall/user mutation tools；
- 不注册 arbitrary shell；
- 不注册 desktop control；
- 只注册 typed collectors；
- L2/L3 能力默认 disabled。

不能只使用 `mark_privileged`；必须有稳定的 capability ID 和 PolicyEngine 判断。

## 16.4 M3：单主机 L0/L1 只读采集

### Collector trait

建议定义：

```text
trait EnvironmentCollector {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &[CapabilityId];
    fn required_scope(&self) -> ObservationScope;
    async fn collect(&self, ctx: &ObservationContext)
        -> Result<ObservationBatch>;
}
```

`ObservationContext` 必须带：

- Tenant/Environment/Node identity；
- CapabilityGrant；
- deadline、cancellation、rate budget；
- redaction policy；
- audit/job ID。

### 首批 typed collectors

优先复用：

- `src/computer/system.rs` 的 `sysinfo` 结构化 host 信息；
- `src/computer/network.rs` 的 cross-platform `NetworkInspector`；
- `src/computer/types.rs` 的系统抽象；
- `src/computer/platform/registry.rs` 的平台选择；
- `src/computer/platform/linux/system_inspect.rs` 中的采集思路，但不要把原始 Shell 输出直接进入 Agent；
- `src/security/content_filter.rs`、`src/security/pii.rs` 做脱敏。

首批覆盖：

- host identity、OS、架构、版本；
- CPU、内存、磁盘和资源指标；
- 进程列表（默认隐藏命令行敏感参数）；
- 服务清单和状态；
- 网络接口、地址、路由、DNS、代理元数据；
- 明确 allowlist 目录的文件 metadata；
- 明确 allowlist 日志源的 bounded tail/summary。

初版禁止：

- 全盘扫描；
- packet capture；
- 任意 Shell；
- 原始 secret/config/log 全量上传；
- 自动发现并扩大 CIDR；
- 未经批准的写操作。

### 观察结果处理

```text
typed collector
  → ObservationBatch
  → normalize
  → redaction/sensitivity classification
  → validate scope and size
  → EnvironmentFact + evidence hash
  → transactional upsert
  → diff/change event
  → persistent audit
```

SystemInspectTool 的原始命令和日志输出不能直接成为模型输入；事实层必须保留结构化字段、证据引用和敏感级别。

## 16.5 M4：观察任务、调度和事实查询

### JobManager

`src/environment/jobs.rs` 管理：

- 手动一次性采集；
- interval/cron 调度；
- 最大运行时长；
- 最大并发和速率；
- 不允许同 scope 重叠；
- cancellation/kill switch；
- 重启恢复和 checkpoint；
- 运行结果和错误；
- revoked/expired grant 自动停止。

复用 `src/cron/cron/types.rs`、`src/cron/cron/scheduler.rs` 的调度模式，但不要直接把普通 cron command 当 sensor job。Sensor job 必须携带 ObservationScope、level、collector、budget 和审计元数据。

### WS API

```text
environment.observation.start
 environment.observation.cancel
 environment.observation.status
 environment.observation.jobs.list
 environment.observation.jobs.get
 environment.observation.jobs.create
 environment.observation.jobs.enable
 environment.observation.jobs.disable
 environment.observation.jobs.run
 environment.facts.query
 environment.facts.snapshot
 environment.facts.compare
 environment.topology.query
 environment.changes.list
```

所有查询必须按 `RequestContext` 和对象 scope 过滤；返回 facts 时带 source、observed_at、confidence、expires_at、coverage 和 sensitivity。

### Query Agent

只新增只读 Agent tools：

- `environment.fact_read`；
- `environment.compare`；
- `environment.topology_read`；
- `environment.explain`。

LLM 只负责关联、解释和推荐；PolicyEngine 决定能否观察或执行，不能通过 prompt 让模型自行扩大范围。

## 16.6 M5：知识平面和图谱

复用 `src/memory/dreaming/knowledge_graph.rs` 的图算法思路，但不要直接复用它作为生产环境事实存储：

- 现有图谱偏 memory/dreaming；
- 缺少 tenant/environment scope；
- 缺少 provenance、TTL、冲突解决、事实查询 API；
- 目前不是环境资产 inventory。

环境知识平面需要同时保存：

1. **不可变 observation/evidence**：原始观测的受控摘要、hash、source、job、时间；
2. **当前 materialized facts**：经过归一化、冲突解决的当前状态；
3. **relationships**：runs_on、depends_on、listens_on、managed_by、observed_from；
4. **changes**：快照 diff、变更来源、影响范围；
5. **stale/tombstone**：过期、删除或撤销后的事实状态。

事实冲突采用确定性规则优先：

```text
授权可信源 > 受认证 Connector > 本机 typed collector > 低可信推断
```

LLM 生成的实体关系只能标为推断，不能覆盖高可信事实。

## 16.7 M6：Platform Connectors

Connector 基础 trait：

```text
name()
capabilities()
required_credentials()
read_scope()
observe(scope, cursor)
health()
shutdown()
```

优先级：

1. Kubernetes read-only namespace snapshot；
2. Prometheus/OpenTelemetry；
3. CMDB；
4. DNS/TLS；
5. Cloud asset inventory；
6. Git/CI/CD、LB、service registry、ticketing、database health。

复用：

- `src/mcp/connectors/manifest.rs`：manifest validation；
- `src/mcp/connectors/catalog.rs`：签名、hash、归档和路径安全；
- `src/mcp/connectors/lifecycle.rs`：生命周期和 timeout；
- `src/mcp/connectors/state.rs`：持久化 Installed/Enabled/Disabled/Error 状态；
- `src/cloud/`：云会话和客户端基础；
- `src/secrets/`：SecretRef 和凭据存储。

Connector 特有约束：

- 每个 Connector 独立 credential reference；
- 明确 tenant/environment/resource filter；
- pagination/cursor/idempotency；
- timeout、retry、rate limit；
- 不把 credential 或原始敏感返回放进 LLM；
- MCP connector 不能绕过 Environment PolicyEngine；
- 默认 off，不能安装即自动连接。

## 16.8 M7：Network Zone Collector

只有在 M0–M6 稳定后实现。每个 Collector 必须绑定：

- zone identity；
- signed authorization；
- CIDR/service/port allowlist；
- L1/L2 level；
- max rate/concurrency；
- execution window；
- impact budget；
- cancellation/kill switch；
- retention and redaction；
- audit job/actor/reason。

优先被动或低影响数据源：

- DNS；
- CMDB；
- 云 API；
- LB 和 service registry；
- Prometheus/OTel；
- 已知服务的健康检查。

主动网络测量只能对明确授权的目标运行，不能由发现结果自动扩大范围。

验证重点：

- scope 外目标绝不触达；
- 速率和并发硬上限；
- timeout/partial failure；
- 取消后无残留；
- 断线和 revoke 会停止任务；
- 结果可按 job/actor/reason 审计。

## 16.9 M8：Recommend → Act

### Recommend

Agent 生成持久化 `ActionPlan`，包括：

- 证据和事实来源；
- preconditions；
- predicted impact；
- exact target scope；
- action hash；
- snapshot requirement；
- verification；
- rollback/compensation；
- maintenance window；
- required approval level。

### Act

只扩展现有 `ApprovalQueue` 为 durable, scoped action approval：

- exact immutable args；
- one-time nonce；
- action hash；
- actor and approver；
- grant expiry/revocation check；
- idempotency key；
- pre-change snapshot；
- timeout/kill switch；
- post-check and rollback。

第一批仅允许低风险、可验证动作：

- 授权服务重启；
- 明确配置 reload；
- 受控临时文件清理；
- 既有部署回滚；
- 监控/告警规则调整。

初版明确禁止：

- 提权；
- 创建新凭据；
- 关闭 EDR/审计；
- 删除证据；
- 扩大环境 scope；
- 任意 Shell；
- 未经审批的生产变更。

复用 `src/tools/approval.rs`、`src/tools/rbac.rs`、`src/tools/target_lock.rs`、`src/computer/rollback.rs`、`src/computer/verification.rs`、`src/planner/` 和 `src/goal/`。

## 16.10 配置和部署

### 配置

在 `src/config/types.rs` 增加独立环境配置，默认关闭：

```text
environment.enabled = false
environment.mode = "observer"
environment.registration_required = true
environment.default_level = "L0"
environment.retention_days = ...
environment.max_concurrency = ...
environment.max_rate = ...
environment.egress_allowlist = ...
environment.redaction = ...
environment.feature_flag = ...
```

不要继承当前 capability profile 的 `full/root` 默认值作为 sensor 权限；必须有显式的 `environment-observer` profile。

### 部署

复用：

- `deploy/systemd/syscity.service` 的非 root、ProtectSystem、ReadWritePaths、restart-on-failure；
- `deploy/README.md` 的路径和健康检查；
- `scripts/install.sh`/release 签名和升级流程。

新增：

- signed endpoint package；
- visible service name/status；
- `environment-agent install/status/pause/revoke/uninstall`；
- mTLS/短期 token；
- control-plane egress allowlist；
- CPU/memory/disk budget；
- key/cert rotation；
- offline/local mode；
- staged fleet rollout；
- upgrade/rollback。

安装必须明确显示：

- 将收集哪些数据；
- 拥有哪些 capability；
- 发送到哪里；
- 保留多久；
- 谁可以批准动作；
- 如何暂停、撤销和卸载。

## 16.11 观测、指标和审计

复用：

- `src/observe/{record,collector,writer,prune}.rs` 的记录和保留模式；
- `src/security/runtime_audit.rs`、`persistent_audit.rs`；
- `src/gateway/handlers/health.rs` 的 health/metrics；
- `src/agent/session_store/metrics.rs` 的事务化 metrics。

新增 bounded-label metrics：

```text
environment_observation_started_total
environment_observation_completed_total
environment_observation_failed_total
environment_observation_duration_seconds
environment_observation_queue_depth
environment_fact_upsert_total
environment_fact_conflict_total
environment_fact_stale_total
environment_connector_health
```

不要将 hostnames、path、raw command、IP 列表或 secret 放入高基数 labels。

每次注册、授权、scope 变更、观察、事实冲突、Connector 访问、审批、动作、验证、回滚、撤销和卸载都要有持久化审计。

## 16.12 测试和里程碑退出条件

### 单元测试

- typed IDs、scope containment/expiry/revocation；
- capability grant 和 policy precedence；
- path/CIDR/namespace/resource selector；
- redaction/sensitivity；
- fact TTL、merge、conflict、snapshot diff；
- registration/job/action state machine；
- connector cursor/retry/idempotency；
- migration roundtrip。

### 集成测试

- register → approve → observe → persist → query → revoke；
- 两 environment/tenant facts 隔离；
- revoked registration 停止 jobs；
- pending job cancellation 和 TaskRegistry shutdown；
- connector credential 不进入 prompt/log；
- scope 外资源拒绝；
- event audience isolation；
- approval failure 保留 retry。

### E2E

- Linux/macOS/Windows host collector；
- systemd/launchd/Windows service install/uninstall；
- Kubernetes namespace-only ServiceAccount；
- cloud read-only IAM；
- authorized zone collector mock network；
- action preflight/snapshot/verify/rollback；
- kill switch、certificate rotation、offline mode、credential revoke。

每个里程碑的退出条件：

1. 无 live external dependency 的 deterministic fixture tests；
2. tenant/environment/node boundary tests；
3. no secret/raw log enters LLM context；
4. cancellation/shutdown tests；
5. audit completeness tests；
6. `cargo fmt`、`cargo clippy -- -D warnings`、目标 `cargo test` 和 `scripts/self-check.sh` 通过。

## 16.13 发布顺序

```text
M0  安全基线/对象授权/WS scope 修复
M1  typed environment domain + additive schema/config
M2  registration/identity/capability + WS/CLI lifecycle
M3  single-host L0/L1 read-only collectors
M4  observation scheduler + facts query + retention/stale/revoke
M5  Web/TUI/CLI environment view and audit
M6  Kubernetes/cloud/CMDB/observability connectors
M7  authorized Network Zone Collector
M8  approval-backed Recommend → Act + verify/rollback
M9  multi-node control plane and production hardening
M10 shared multi-tenancy only if scale requires it
```

**M0–M4** 完成后，系统可以安全地作为单主机 Environment Agent 试用；**M5–M7** 完成后，具备多节点和受控网络区域的环境理解能力；**M8–M9** 完成后，才适合在生产中提供有限自动化；共享多租户必须最后评估，不能提前引入复杂的 tenant 化横切改造。

## 16.14 实施原则总结

```text
身份 → 能力 → 范围 → 观察任务 → Typed Collector
  → 脱敏/归一化 → Facts/Entities/Changes
  → 证据检索 → 确定性 Policy → Agent Explain/Recommend
  → 精确审批 → 受控 Action → 验证/回滚
  → 指标/审计 → 撤销/卸载
```

最佳 MVP 不是一个“探测整个环境”的大工具，而是一个默认只读、范围明确、事实可追溯、任务可取消、权限可撤销的单主机 Sensor。现有 Syscity 的 Gateway、Agent、computer、memory、observe、cron、approval、RBAC、secrets 和 audit 可以作为基础，但必须通过新的环境域模型和确定性 PolicyEngine 组合，不能直接把当前 full/root 工具集合暴露给模型。

---

# 17. Beta 发布实施路径

本节专门定义 Beta 版本的实现范围。Beta 不是把所有环境能力一次性打开，而是一个**默认关闭写操作、只读优先、范围明确、可回滚、可审计**的受控发布。

## 17.1 Beta 的目标和明确边界

### Beta 包含

- Kubernetes read-only connector；
- cloud inventory、CMDB、Prometheus/OpenTelemetry 等只读 Connector；
- 多节点 Environment → Site → Zone → Node → Asset/Service 环境树；
- Environment Facts 的当前状态、历史 observations、TTL、来源和差异比较；
- 只读诊断 Agent；
- 对已登记目标执行只读网络 healthcheck；
- 完整的 mock-based E2E 和安全回归测试；
- 可见的注册、心跳、暂停、撤销和卸载路径。

### Beta 不包含

- 任意 Kubernetes Secret 内容读取；
- Kubernetes 写操作或任意资源变更；
- Cloud/CMDB/Observability Connector 写操作；
- 无边界网络扫描；
- 发现新 CIDR 后自动扩大目标范围；
- 任意 Shell 作为默认 Sensor API；
- Packet capture；
- 漏洞利用或高风险主动验证；
- 生产服务自动重启、配置变更、流量切换或 DNS 变更；
- 未经审批的 Action 执行。

Beta 的 feature flag 必须默认关闭；没有注册、scope 和授权的节点不能运行环境采集。

## 17.2 Beta 总体架构

```text
Operator / CLI / Web / TUI
          │ authenticated WS
          ▼
Environment Control Plane
  registration · grants · jobs · approvals · audit · revoke
          │
          ├──────────────┬─────────────────┐
          │              │                 │
   Endpoint Sensor   Zone Healthcheck   Platform Connectors
   host facts        approved targets    K8s/Cloud/CMDB/OTel
          │              │                 │
          └──────────────┴─────────────────┘
                         │
              Normalized Fact Pipeline
        validate → redact → provenance → persist
                         │
              Environment Knowledge Plane
         tree · topology · snapshot · diff · TTL
                         │
                  Diagnostic Agent
       evidence-aware explain/recommend, read-only
```

原则：

1. Connector 不直接把任意 JSON 暴露给 Agent；必须进入统一事实管道。
2. Agent 不直接向 Connector 传递新 scope；scope 由控制面和 PolicyEngine 决定。
3. 所有环境事件带 `tenant_id/environment_id/zone_id/node_id/audience`。
4. 任何 healthcheck 和 Connector job 都有 `job_id`、`actor`、`scope`、`deadline` 和审计记录。

## 17.3 Beta 依赖闸门

Beta 的实现顺序不是按 UI 功能，而是按安全依赖顺序：

```text
B0 安全控制面
  → B1 Domain/Store/Policy
  → B2 Registration/Capability
  → B3 Connectors/Healthcheck
  → B4 Multi-node tree
  → B5 Facts diff
  → B6 Diagnostic Agent
  → B7 E2E/Security gate
  → B8 Beta rollout
```

任何前置阶段未通过，不得开启后续阶段的生产 feature flag。

## 17.4 B0：Beta 前安全控制面

Beta 之前必须完成：

- WS handshake 不信任客户端 requested scopes；
- method scope 重新审核；
- session/chat/artifact/environment 对象级授权；
- WS 连接和消息限流；
- 事件 audience 过滤；
- Connector 凭据隔离；
- Webhook fail-closed；
- secret/query token 日志脱敏；
- revoked/paused sensor 立即停止新 job；
- pending job 能够取消；
- 所有新增环境方法默认拒绝，直到显式注册 scope；
- environment facts 查询严格按 tenant/environment/zone/node 过滤。

相关实现边界：

```text
src/gateway/ws/handshake.rs
src/gateway/ws/core.rs
src/gateway/protocol.rs
src/security/request_context.rs
src/security/persistent_audit.rs
src/gateway/rate_limit.rs
src/tools/rbac.rs
src/tools/approval.rs
src/gateway/task_registry.rs
```

验收：

- 普通 read 连接不能读取其他 environment/node；
- 客户端不能自授 admin；
- revoked registration 的 job 在下一次 policy check 被拒绝；
- 所有拒绝都有 audit actor、target、scope、reason；
- 双连接无法接收对方的 environment event。

## 17.5 B1：统一 Connector Contract

新增 `src/environment/connectors.rs` 或独立 connector crate，统一所有 Beta Connector 的生命周期和输出：

```rust
trait EnvironmentConnector {
    fn id(&self) -> &str;
    fn version(&self) -> &str;
    fn capabilities(&self) -> &[CapabilityId];
    fn required_scopes(&self) -> &[ResourceSelector];
    fn required_credentials(&self) -> &[SecretRef];
    async fn health(&self, ctx: &ConnectorContext) -> Result<ConnectorHealth>;
    async fn observe(
        &self,
        scope: &ObservationScope,
        cursor: Option<Cursor>,
        ctx: &ConnectorContext,
    ) -> Result<ObservationBatch>;
    async fn shutdown(&self);
}
```

`ConnectorContext` 必须包含：

- `RequestContext`；
- tenant/environment/zone/node scope；
- job ID 和 correlation ID；
- deadline/cancellation；
- rate and concurrency budget；
- redaction/sensitivity policy；
- SecretRef resolver（不把 secret 值交给 Agent）。

复用现有：

- `src/mcp/connectors/manifest.rs` 的 manifest validation；
- `src/mcp/connectors/catalog.rs` 的 hash/signature/archive 安全；
- `src/mcp/connectors/lifecycle.rs` 的 timeout/lifecycle；
- `src/mcp/connectors/state.rs` 的 Installed/Enabled/Disabled/Error 状态；
- `src/secrets/` 的 SecretRef；
- `src/security/persistent_audit.rs` 的审计。

Connector 必须声明：

```text
category
read/write capabilities
required resource filters
data sensitivity
observation templates
rate limits
credential references
```

Beta 所有 Connector 只允许 read capability；manifest 未签名、来源不受信任或资源过滤为空时拒绝启用。

## 17.6 B2：Kubernetes read-only connector

### 目标

先支持一个受控 Kubernetes namespace 快照，而不是全 cluster 任意访问。

### 范围

允许读取：

- namespace metadata；
- Nodes 的非敏感摘要（按显式 cluster/node scope）；
- Pods；
- Deployments；
- StatefulSets；
- DaemonSets；
- Services；
- Ingress；
- ConfigMap metadata（默认不读 value）；
- Events；
- resource requests/limits；
- labels/annotations（需要敏感字段过滤）。

默认禁止：

- Secret data；
- exec/attach/port-forward；
- create/update/delete/patch；
- cluster-wide list（除非显式 grant）；
- 读取其他 tenant/namespace。

### 实现边界

新增：

```text
src/environment/connectors/kubernetes.rs
src/environment/connectors/kubernetes_model.rs
src/environment/connectors/kubernetes_scope.rs
```

Connector 参数：

```text
cluster_id
namespace_allowlist
resource_allowlist
field_redaction_policy
page_size
request_timeout
rate_limit
```

认证：

- 首选 Kubernetes ServiceAccount 的 read-only RBAC；
- credential 通过 SecretRef；
- connector 不能读取自身 ServiceAccount token 内容并写入 facts；
- namespace 和 resource scope 由 Environment PolicyEngine 再次校验。

### 数据流

```text
Kubernetes API
  → paginated snapshot/cursor
  → normalize K8s objects
  → redact Secret-like fields
  → EnvironmentEntity/Fact/Relationship
  → upsert + change diff
  → audit
```

初版使用 snapshot/pagination，不实现 watch；watch 在 Beta 后再引入，因为 watch 需要处理 reconnect、resourceVersion、重复事件和背压。

### 测试

- mock Kubernetes HTTP server；
- namespace allowlist；
- cluster-wide 请求拒绝；
- Secret value 永不进入 facts/prompt/log；
- pagination/cursor；
- 429/5xx/timeout/retry；
- duplicate snapshot 幂等；
- deleted object/tombstone；
- revoked connector 停止请求；
- tenant/environment 不能跨读。

## 17.7 B3：Cloud、CMDB、Observability Connectors

### Cloud inventory

先做只读资源 inventory，不实现云资源写操作：

- account/project/subscription metadata；
- regions/zones；
- instances/nodes；
- networks/subnets/security groups 的摘要；
- load balancers；
- managed databases；
- tags/owners；
- deployment/version metadata。

每个云 provider 独立 adapter，但输出统一：

```text
Asset
  provider
  account_ref
  region
  resource_id
  resource_type
  owner
  tags
  state
  observed_at
```

必须有：

- account/resource-group allowlist；
- read-only IAM；
- pagination；
- API rate limit；
- credential rotation；
- provider-specific redaction；
- cost/quotas 只读。

复用 `src/cloud/` 的客户端和 session 基础，但云端 credential 只作为 Connector credential，不进入 Agent prompt。

### CMDB Connector

支持：

- asset inventory；
- owner/team；
- environment/site/zone；
- service/application；
- dependency references；
- lifecycle state；
- change/ticket references。

要求：

- `updated_since` cursor；
- idempotent upsert；
- source priority；
- conflict handling；
- source record URL/id；
- read-only API token；
- 结果按租户和环境过滤。

### Prometheus/OpenTelemetry Connector

支持：

- metric metadata；
- bounded-range queries；
- alert/recording rule metadata；
- service/target health；
- trace/span summary metadata（默认不拉原始 payload）；
- time window、step 和 sample limit。

不能允许 Agent 自由构造无限时间范围或高基数查询。Connector 必须对：

- query length；
- time range；
- step；
- sample count；
- concurrent queries；
- label cardinality

进行硬限制。

## 17.8 B4：多节点环境树

### 统一树模型

```text
Tenant
└── Environment
    └── Site
        └── Zone
            └── Node / Cluster / Cloud Account
                └── Asset
                    └── Service / Application / Deployment
```

每个节点都有：

- stable ID；
- display name；
- parent scope；
- registration/source；
- status/last_seen；
- capability summary；
- sensitivity；
- owner；
- observed_at；
- stale/expired state。

### 事件和查询

环境事件必须带：

```text
tenant_id
environment_id
site_id?
zone_id?
node_id?
audience
job_id?
```

新增 read-only WS 方法：

```text
environment.tree.get
environment.nodes.list
environment.nodes.get
environment.node.health
environment.node.capabilities
environment.assets.list
environment.services.list
```

事件发送路径 `src/gateway/ws/core.rs` 必须按 audience 和 `RequestContext` 过滤，不能把 environment status 当作全局广播。

### 测试

- 多节点注册和 heartbeat；
- 节点 pause/isolate/revoke；
- stale heartbeat；
- 双 environment 查询隔离；
- zone/node resource authorization；
- tree parent/child consistency；
- 删除/卸载后的 tombstone 和历史查询。

## 17.9 B5：Facts diff 和诊断 Agent

### Fact 数据层

保留两类数据：

1. immutable observation/evidence：来源、job、时间、证据 hash、受控摘要；
2. materialized current facts：当前有效值、source priority、TTL、confidence、diff 状态。

Facts diff 算法必须是确定性的：

```text
same        → no change
added       → added
removed     → removed/tombstone
value diff  → changed
expired     → stale
conflict    → conflict with source precedence
```

冲突优先级：

```text
授权可信源 > 认证 Connector > typed host collector > 低可信推断
```

LLM 不能覆盖高可信事实；LLM 推断必须标记为 hypothesis。

### 诊断 Agent

只提供：

- `environment.fact_read`；
- `environment.snapshot_compare`；
- `environment.topology_read`；
- `environment.metric_query`；
- `environment.diagnose`。

诊断 Agent 输入必须由 PolicyEngine 过滤，并带：

```text
source
observed_at
confidence
expires_at
scope
coverage_gaps
```

诊断输出格式建议：

```text
症状
证据
时间线
可能原因（按置信度排序）
影响范围
未知信息/覆盖缺口
建议的下一步只读观察
是否需要管理员审批
```

Beta 诊断 Agent 不执行 write action；如果需要修复，只生成 ActionPlan。

### 测试

- facts added/removed/changed/stale/conflict；
- snapshot diff 稳定性和幂等性；
- source precedence；
- stale fact 不作为当前事实；
- 诊断输出必须引用证据；
- 证据不足时不能编造确定结论；
- 诊断不得获得 Connector 写权限；
- 跨 environment facts 不得进入上下文。

## 17.10 B6：只读网络 healthcheck

### 目标

只检查已登记、已授权的目标，不实现无界 network scanner。

### 目标模型

```text
HealthcheckTarget
  id
  environment_id
  zone_id
  hostname/ip
  port
  protocol: tcp | http | https | dns
  path?
  expected_status?
  tls_policy?
  timeout
  owner
  expires_at
```

### 执行策略

- 目标必须在 allowlist；
- 只允许 L1 controlled check；
- max concurrency；
- max requests/sec；
- per-target timeout；
- total deadline；
- cancellation/kill switch；
- DNS rebinding/SSRF 防护；
- 不跟随未授权跳转；
- 不下载大响应体；
- 不保存认证 header/body；
- 结果只保留状态码、延迟、证书摘要、错误类别和 evidence hash。

复用 `src/computer/network.rs` 的结构化 `NetworkInspector` 和 `src/computer/platform/linux/network_diag.rs` 的 timeout/诊断模式，但不要直接暴露任意 `ping/traceroute/ss/dig/curl` 参数给 LLM。

### WS API

```text
environment.healthcheck.targets.list
environment.healthcheck.target.get
environment.healthcheck.run
environment.healthcheck.cancel
environment.healthcheck.runs.list
environment.healthcheck.status
```

### 测试

- fake TCP/HTTP/TLS/DNS targets；
- allowlist 外目标拒绝；
- redirect/SSRF/IPv6/localhost/metadata endpoint 防护；
- timeout、partial result、cancellation；
- concurrency/rate budget；
- response-size limit；
- no credential/body persistence；
- audit 和 environment event audience。

## 17.11 B7：完整 E2E 与安全审计

### E2E 主流程

```text
显式安装
  → registration request
  → operator approve
  → short-lived credential
  → heartbeat
  → host/K8s/CMDB/metrics observation
  → normalize/redact
  → persist facts
  → environment tree query
  → snapshot diff
  → diagnosis Agent
  → read-only healthcheck
  → revoke
  → jobs cancelled
  → facts stale/tombstone
  → uninstall
```

### 测试环境

不使用真实企业或公网：

- in-memory SQLite；
- fake GatewayState；
- mock Kubernetes API；
- mock CMDB HTTP API；
- mock cloud inventory API；
- mock Prometheus/OTel API；
- fake DNS/TCP/HTTP/TLS endpoints；
- test sensor process；
- deterministic clock；
- fixed UUID/keys for fixtures。

### 安全回归矩阵

必须覆盖：

| 类别 | 场景 |
|---|---|
| 身份 | invalid signature、wrong node、expired credential、revoked registration |
| scope | self-admin、scope widening、path/CIDR/namespace escape |
| 对象授权 | cross-tenant、cross-environment、cross-zone、cross-node |
| 数据保护 | secrets/PII/raw logs not in facts/prompt/audit |
| Connector | wrong credential、pagination、rate limit、SSRF、timeout |
| Jobs | overlap、cancel、restart recovery、revoked job |
| Events | audience leak、unauthorized subscription、stale node events |
| Facts | conflict、expiry、delete/export、source precedence |
| Agent | evidence requirement、uncertainty、no write tool、no scope expansion |
| Lifecycle | pause、isolate、revoke、uninstall、credential rotation |
| Operations | migration、backup/restore、health/readiness/metrics、shutdown |

### 安全审计输出

Beta 发布前生成：

- Threat model；
- Data-flow/trust-boundary diagram；
- Capability/scope matrix；
- Connector credential matrix；
- Audit-event catalog；
- Retention/delete/export policy；
- Known limitations；
- Incident/revoke runbook；
- E2E test report；
- `cargo audit`/`cargo deny`/static-analysis report；
- SBOM 和签名验证结果。

## 17.12 Beta Release Gate

Beta 只有在以下条件全部满足后发布：

1. M0 control-plane security gate 通过；
2. Beta feature flag 默认关闭并有 operator-visible enablement；
3. Kubernetes read-only connector 通过 mock contract 和 RBAC tests；
4. cloud/CMDB/observability connectors 均只读、独立 credential、有限 resource scope；
5. multi-node tree 通过对象授权和 audience tests；
6. facts diff 通过 TTL/conflict/provenance tests；
7. diagnosis Agent 只能读取证据和生成建议，不能执行 write；
8. healthcheck 只访问 allowlisted targets 并有硬速率/并发/超时限制；
9. revoke/pause/isolate 能停止 jobs 和 connector calls；
10. E2E 主流程通过 deterministic fixtures；
11. 安全审计无 P0/P1 未接受风险；
12. migration/rollback/backup/restore 已验证；
13. health/readiness/metrics/alerting 已接线；
14. operator runbook、数据保留、卸载和 incident runbook 完整；
15. 没有未记录的 secret、raw restricted logs 或跨环境 facts 泄露。

## 17.13 Beta Rollout

推荐分批发布：

```text
internal single-host observer
  → 3–5 个测试环境
  → 单一 Kubernetes cluster / namespace
  → 单一 cloud account/resource group
  → 多节点 staging environment
  → 低敏感生产只读环境
  → 受控生产 Beta
```

每一批次都要求：

- 明确 owner；
- 明确 scope；
- healthcheck 和 audit 正常；
- 无跨环境告警；
- 资源和成本预算未超限；
- 可一键 pause/revoke；
- 可以回滚到禁用 feature flag 的版本。

Beta 不应一次覆盖整个企业或整个区域网络。

## 17.14 Beta 的已知限制

Beta 发布时应明确标注：

- Kubernetes watch、自动修复和写操作不支持；
- Cloud/CMDB/Observability 数据可能有延迟和冲突；
- Facts 不是完整 CMDB 替代品；
- healthcheck 不是漏洞扫描器；
- 诊断 Agent 只能基于可见证据，不能保证根因正确；
- 多节点树依赖 registration/heartbeat 质量；
- 未授权范围不会被自动发现；
- L2 network measurement 和 L3 production action 默认关闭；
- 真实环境 E2E 只能在明确授权的 staging/production 试点中运行。

## 17.15 Beta 交付顺序

```text
B0  安全控制面和 feature gate
B1  Connector contract + normalized fact schema
B2  Kubernetes read-only connector
B3  Cloud/CMDB/Prometheus/OTel connectors
B4  Environment tree + node heartbeat/revoke
B5  Facts snapshot/diff/TTL/conflict
B6  Read-only diagnosis Agent
B7  Allowlisted network healthcheck
B8  Deterministic E2E + security regression
B9  Operator runbook + rollout/rollback
B10 Beta release gate and staged deployment
```

## 17.16 Beta 结论

Beta 的正确路径不是先做“扫描能力”，而是先做**受控事实生产和证据查询**：

```text
注册身份
  → 显式能力/范围
  → 只读 Connector/Collector
  → 统一 FactEnvelope
  → 多节点/拓扑视图
  → 确定性 snapshot diff
  → 有证据的诊断 Agent
  → allowlisted read-only healthcheck
  → 撤销、审计和 E2E 安全闸门
```

完成 B0–B6 后，Syscity 才具备可供内部试点的环境理解能力；完成 B7–B9 后，才适合在低敏感、明确授权的 staging 或生产环境发布 Beta；任何生产写操作、无界网络测量或自动修复都应留在 Beta 之后并单独进行安全评审。

---

# 18. 企业网络整体理解：实施路径研究

本节进一步研究“探测整个企业网络”的落地方式。这里的“探测”定义为：**在企业明确授权的资产、网络区域、云账户和平台边界内，持续汇聚被动事实、受控 Connector 数据和低影响健康检查，形成可追溯的环境拓扑与诊断能力**。

它不是一个单一的网络扫描器，也不是把一台主机 Agent 变成可触达所有网段的超级权限进程。企业网络理解必须是一个分布式、分区、分权限、分数据源的控制系统。

## 18.1 企业网络理解的正确目标

企业网络通常不是一个平面 CIDR，而是多个不同信任边界：

```text
企业
├── 总部/办公网络
├── 数据中心
│   ├── 生产区
│   ├── 预发布区
│   ├── 测试区
│   └── 管理区
├── 云 VPC / VNet
│   ├── 公共子网
│   ├── 私有子网
│   └── 数据层
├── Kubernetes 集群
│   ├── control plane
│   ├── namespaces
│   └── service mesh
├── 分支机构 / 门店
└── 第三方互联 / VPN / 专线
```

Beta 或第一版企业网络能力应回答：

- 哪些节点、服务和应用属于当前环境；
- 每个事实来自哪里、何时观察、可信度如何；
- 哪些系统之间存在依赖关系；
- 最近发生了什么变化；
- 哪些节点失联或事实过期；
- 哪些已登记服务可以健康检查；
- 一个故障可能影响哪些服务和区域；
- 当前观测覆盖了什么、没有覆盖什么；
- 下一步需要哪一种只读观测。

不应承诺：

- 一次运行就得到“完整网络真相”；
- 自动发现所有未登记资产；
- 对所有公网或企业网段无限制探测；
- 单凭 LLM 推断准确的依赖关系；
- 以网络可达性替代业务授权。

## 18.2 企业网络架构：控制面、区域面、节点面

```text
                         ┌──────────────────────────┐
                         │ Enterprise Control Plane │
                         │ tenant / policy / audit  │
                         │ registration / jobs      │
                         └─────────────┬────────────┘
                                       │
       ┌───────────────────────────────┼──────────────────────────────┐
       │                               │                              │
┌──────▼──────┐                 ┌──────▼──────┐                ┌──────▼──────┐
│ Endpoint     │                 │ Zone        │                │ Platform    │
│ Sensors      │                 │ Collectors  │                │ Connectors  │
│ servers/laptop│                │ VPC/DC/site  │                │ K8s/cloud   │
└──────┬──────┘                 └──────┬──────┘                └──────┬──────┘
       │                               │                              │
       └───────────────────────────────┼──────────────────────────────┘
                                       │
                         ┌─────────────▼────────────┐
                         │ Normalization / Fact Bus  │
                         │ redact / validate / diff  │
                         └─────────────┬────────────┘
                                       │
                         ┌─────────────▼────────────┐
                         │ Environment Knowledge    │
                         │ assets / topology / change│
                         └─────────────┬────────────┘
                                       │
                         ┌─────────────▼────────────┐
                         │ Diagnostic Agent          │
                         │ evidence / explain / plan │
                         └───────────────────────────┘
```

### 控制面

控制面负责：

- 企业、环境、站点、区域、节点注册；
- 公钥/证书、短期凭据和撤销；
- capability grants 和 ObservationScope；
- Connector credentials 的 SecretRef；
- 观察 job、限流、预算和取消；
- 事实存储、diff、retention 和审计；
- 查询和诊断 Agent 的数据授权；
- 健康、指标、告警和 rollout。

### 区域面

每个网络区域只部署一个或多个明确身份的 Zone Collector：

```text
zone-prod-a-collector
zone-staging-b-collector
vpc-private-c-collector
branch-07-collector
```

它只访问控制面下发的目标集合，并且不能把发现的邻居自动转化为新的扫描目标。

### 节点面

Endpoint Sensor 负责节点本地事实：

- 主机、进程、服务、网络接口和路由；
- 节点上已授权的应用/部署元数据；
- 本地监控或日志摘要；
- 节点自身的 heartbeat、能力和版本。

### 平台面

Platform Connector 负责通过受控 API 查询：

- Kubernetes；
- 云平台资产；
- CMDB；
- Prometheus/OpenTelemetry；
- DNS/TLS；
- Git/CI/CD；
- 负载均衡和服务注册中心；
- 工单和告警系统。

三者输出必须进入同一个 Fact/Entity/Relationship pipeline。

## 18.3 企业网络理解不是单次扫描，而是多源事实汇聚

建议把企业网络状态分成四类来源：

### S0：权威清单

优先级最高：

- CMDB；
- 云资源 API；
- Kubernetes API；
- 服务注册中心；
- 企业 IPAM/DNS；
- 资产管理系统。

### S1：平台观测

- Prometheus/OTel；
- Load Balancer；
- Service Mesh；
- Firewall/VPC flow metadata；
- CI/CD deployment metadata；
- Alerting systems。

### S2：节点观测

- Endpoint Sensor；
- `sysinfo` 主机状态；
- 本地服务管理器；
- 网络接口/路由/监听 socket；
- 已授权日志摘要。

### S3：受控主动检查

- TCP connect；
- HTTP/HTTPS healthcheck；
- DNS lookup；
- TLS certificate metadata；
- 已登记的服务 ping。

事实冲突的默认优先级：

```text
S0 > S1 > S2 > S3 > LLM hypothesis
```

主动检查只证明“某个时间点从某个 collector 可达”，不能覆盖服务归属、业务依赖和授权边界。

## 18.4 分区和授权模型

企业网络必须用层级范围表达，而不是单一 `read_network=true`：

```text
TenantScope
├── EnvironmentScope
│   ├── SiteScope
│   │   └── ZoneScope
│   │       ├── NodeScope
│   │       ├── CIDRScope
│   │       ├── ServiceScope
│   │       ├── NamespaceScope
│   │       └── ConnectorResourceScope
```

每个 Scope 需要：

- `scope_id`；
- 资源选择器；
- 父 scope；
- operation：read/healthcheck/measure；
- valid_from/expires_at；
- max_rate；
- max_concurrency；
- max_duration；
- sensitivity ceiling；
- approver；
- policy version；
- revocation generation。

有效目标必须满足：

```text
requested_target ⊆ granted_scope
```

不能因为某个传感器位于生产网段，就默认它可以观察整个生产网段。Collector 的网络位置只是能力前提，不是授权本身。

## 18.5 网络理解的分层实施路径

### N0：被动资产和平台数据（最先实施）

接入：

- CMDB/IPAM/DNS；
- Cloud inventory；
- Kubernetes；
- Prometheus/OTel；
- Load Balancer/service registry；
- CI/CD；
- Endpoint heartbeat。

产出：

- 资产树；
- 节点和服务清单；
- 资源 owner；
- 环境/区域归属；
- 部署版本；
- 已知依赖；
- 观测时间和来源。

N0 不主动访问未知目标，风险最低，价值最高，应作为企业网络 MVP。

### N1：已登记服务健康检查

只对 CMDB、K8s、云或管理员显式登记的目标运行：

- TCP connect；
- HTTP/HTTPS fixed health path；
- DNS；
- TLS certificate；
- service registry health；
- Kubernetes readiness/liveness metadata。

每次检查要带：

```text
source_collector
zone_id
job_id
target_id
observed_at
latency
result_class
error_class
certificate_fingerprint?
```

### N2：授权网络区域测量

仅在明确授权 CIDR 和维护窗口内运行。要求：

- 目标来自 signed ObservationScope；
- 速率/并发/时间上限；
- allowlist/denylist；
- 危险目标保护（localhost、云 metadata、管理面）；
- cancellation 和 kill switch；
- 每个请求或批次审计；
- 结果脱敏；
- 失败时不自动扩大范围。

N2 不是 Beta 默认能力；需单独的企业安全评审。

### N3：高风险验证或变更

不属于企业网络 Beta。包括：

- 漏洞验证；
- 大范围端口/服务枚举；
- 防火墙规则修改；
- 流量切换；
- DNS 修改；
- 生产服务变更；
- 任意包安装或远程执行。

N3 需要独立的变更管理、审批、快照、回滚和 kill switch 设计。

## 18.6 现有 Syscity 模块如何复用

### 主机和网络基础

复用：

- `src/computer/network.rs` 的 `NetworkInspector`；
- `src/computer/system.rs` 的 sysinfo 结构化主机信息；
- `src/computer/types.rs` 的平台抽象；
- `src/computer/platform/registry.rs` 的平台能力选择；
- `src/computer/platform/linux/network_diag.rs` 的 timeout/诊断经验；
- `tests/integrations/network_tests.rs` 作为网络 contract 测试起点。

不要直接把 Linux `network_diag` 的 `traceroute/ss/dig/curl` 任意 action 暴露给 LLM；应包装为固定参数、allowlist 目标和显式 healthcheck job。

### Connector 基础

复用：

- `src/mcp/connectors/manifest.rs`：声明与验证；
- `src/mcp/connectors/catalog.rs`：包 hash、签名、归档和 zip-slip 防护；
- `src/mcp/connectors/lifecycle.rs`：启动、停止、timeout、dry-run；
- `src/mcp/connectors/state.rs`：Connector 状态和迁移；
- `src/cloud/`：Cloud session/client/provider；
- `src/secrets/`：SecretRef 和 credential storage。

MCP Connector 的生命周期不能代替 Environment PolicyEngine；Connector 只能在已批准的 environment/zone scope 内运行。

### 观测和持久化

复用：

- `src/observe/record.rs`、`collector.rs`、`writer.rs`、`prune.rs`；
- `src/security/persistent_audit.rs`；
- `src/gateway/handlers/health.rs`；
- `src/memory/db.rs` 的 SQLite migration 模式；
- `src/memory/dreaming/knowledge_graph.rs` 的图关系算法。

环境 facts 不应直接写入普通 conversation memory。应使用独立 `EnvironmentStore`，随后按授权将脱敏摘要投影到 Agent context。

## 18.7 企业拓扑数据模型

建议把拓扑拆为实体和关系，而不是把网络输出保存成一段文本：

```text
Entity
  id
  kind: environment/site/zone/node/cluster/asset/service/app/deployment
  name
  provider
  owner
  status
  sensitivity
  scope
  observed_at
  expires_at
  source
```

```text
Relationship
  id
  from_entity
  relation: contains/runs_on/listens_on/depends_on/routes_to/managed_by
  to_entity
  confidence
  source
  observed_at
  expires_at
  evidence_hash
```

```text
Change
  id
  entity_or_relationship
  kind: added/removed/changed/stale/conflict
  before
  after
  source
  observed_at
  job_id
  actor
```

拓扑查询必须能够回答：

- 从某个 service 到数据库的已知依赖是什么；
- 某个 node 属于哪个 zone/environment；
- 某次部署影响哪些节点；
- 某个 zone 当前哪些节点失联；
- 哪些关系是权威事实，哪些只是低置信度推断。

## 18.8 Enterprise Network Collector 的任务模型

每次网络观测都建成一个可取消、可审计的 Job：

```text
NetworkObservationJob
  job_id
  tenant_id
  environment_id
  zone_id
  collector_id
  target_selector
  level: N0 | N1 | N2
  schedule
  max_rate
  max_concurrency
  deadline
  impact_budget
  approval_id?
  status
  created_by
  cancelled_by?
```

Job 状态：

```text
pending → approved → running → completed
                    ├→ partial
                    ├→ cancelled
                    ├→ timed_out
                    ├→ failed
                    └→ revoked
```

执行器必须在每个目标前重新检查：

- registration active；
- scope 未过期；
- target 仍在 allowlist；
- job 未取消；
- rate/concurrency budget 未超；
- collector 未被 revoke/isolate。

## 18.9 网络安全边界

必须防护：

### SSRF 和地址绕过

- 禁止任意 URL；
- 解析后校验 IP；
- 处理 IPv4/IPv6、IPv4-mapped IPv6、DNS rebinding；
- 默认拒绝 loopback、link-local、云 metadata 和控制面地址；
- redirect 每跳重新校验；
- 不使用 DNS 解析前的 hostname 校验作为唯一防护。

### 范围扩张

- 不允许 `0.0.0.0/0` 作为默认 scope；
- CIDR 必须 canonicalize；
- 子网必须属于授权 zone；
- 服务/端口必须显式 allowlist；
- 目标集合版本化，任务启动后不可由 LLM 修改。

### 资源和影响

- 每 zone 最大并发；
- 每 collector 最大速率；
- 每 job 最大请求数；
- 每目标超时；
- 总 deadline；
- 响应体大小限制；
- 重试上限和退避；
- 取消后确认任务停止。

### 数据保护

- 不保存认证 header/body；
- 不把证书私钥、token、Cookie、密码写入 fact；
- 原始响应先过 redaction；
- metrics labels 不使用资源 ID/path/IP 等高基数字段；
- 限制 topology/fact 导出权限。

## 18.10 诊断 Agent 如何理解企业网络

诊断 Agent 不直接“看网络”，而是读取经过 PolicyEngine 筛选的 facts：

```text
用户问题
  → 解析为只读查询计划
  → 校验 environment/zone/node scope
  → 查询事实、指标、变化和健康检查
  → 关联拓扑和时间线
  → 评估证据质量
  → 输出诊断假设和缺口
```

输出格式：

```text
问题摘要
已确认事实
时间线
相关节点/服务/区域
可能原因（置信度排序）
证据引用
覆盖缺口和过期事实
建议的下一项只读检查
需要的额外授权（如有）
```

Agent 不应说“企业网络完整理解完成”，而应说明：

- 当前已覆盖的 environment/zone/node；
- 最近观察时间；
- 数据源健康；
- stale facts 数量；
- 未登记节点和未知关系；
- 结论置信度。

## 18.11 企业网络实施里程碑

```text
EN0  Control-plane security gate
     scope / registration / object auth / event isolation / revoke

EN1  Passive inventory plane
     CMDB / DNS / Cloud / K8s / Prometheus/OTel / endpoint heartbeat

EN2  Normalized entity/fact store
     topology tree / relationships / provenance / TTL / changes

EN3  Node and zone registration
     zone collector identity / heartbeat / signed scope / pause/revoke

EN4  N1 controlled healthchecks
     registered service targets / bounded TCP-HTTP-DNS-TLS checks

EN5  Diagnostic Agent
     facts query / snapshot diff / evidence-aware diagnosis

EN6  N2 authorized measurement pilot
     explicit CIDR / maintenance window / rate / concurrency / kill switch

EN7  Production read-only Beta
     E2E / security audit / runbook / rollback / staged rollout

EN8  Act review
     separate change-control and action executor review
```

每个里程碑都有独立 feature flag 和退出标准。EN6、EN7 之前不允许把网络采集能力默认开启。

## 18.12 企业网络 E2E 验收流程

使用完全可控的测试环境：

```text
创建 tenant/environment/zone
  → 注册两个 Endpoint Sensor 和一个 Zone Collector
  → 授权不同节点和不同 CIDR
  → 从 CMDB/K8s/Cloud/Prometheus mock 导入资产
  → 运行 N0 passive inventory
  → 构建多节点环境树
  → 运行 N1 healthcheck
  → 生成 facts snapshot
  → 修改 mock 资源
  → 生成 deterministic diff
  → 运行诊断 Agent
  → 验证 evidence/coverage/confidence
  → revoke 一个 node
  → 验证 jobs/events/queries 被隔离或停止
  → 删除/导出指定环境数据
  → uninstall sensor
```

必须测试：

- 两 tenant 之间不能互查；
- 两 zone 之间不能互测；
- 一个 node 不能代理另一个 node 的凭据；
- scope 外 target 不触达；
- revoked collector 不继续运行；
- connector credential 不进入 logs/facts/prompt；
- facts diff 在重复输入下幂等；
- partial failure 不污染有效 facts；
- healthcheck 超时和取消不会留下后台任务；
- 事件只到达被授权的 operator。

## 18.13 发布与运行手册

企业网络 Beta 必须随代码提供：

- 资产/区域授权模板；
- Collector 安装和卸载说明；
- Kubernetes ServiceAccount/RBAC 模板；
- Cloud read-only IAM 模板；
- CMDB/Prometheus credential 配置模板；
- zone allowlist 和 healthcheck target 模板；
- 速率/并发/维护窗口默认值；
- 数据 retention、删除和导出流程；
- revoke/isolate/kill switch 流程；
- 故障、Connector 429、凭据失效和数据冲突 runbook；
- 备份、迁移、rollback 和版本兼容说明。

推荐 rollout：

```text
单机内部环境
  → 一个 staging zone
  → 一个 Kubernetes namespace
  → 一个 cloud resource group
  → 多 zone staging
  → 低敏感生产只读
  → 明确授权的生产 Beta
```

不要一开始覆盖整个企业、所有 VPC 或所有分支机构。

## 18.14 企业网络 Beta 退出条件

只有以下条件全部满足，才能称为“企业网络只读 Beta”：

1. 所有 connector 和 collector 均有独立 identity、scope、credential 和 audit；
2. N0/N1 数据源可稳定生成统一 entities/facts/relationships；
3. 多节点环境树支持 parent/child、owner、stale、revoked 和 audience isolation；
4. facts diff、TTL、conflict 和 evidence 能确定性回放；
5. 诊断 Agent 只读取 policy-filtered facts，不拥有 write/scan-expansion 工具；
6. healthcheck 目标和范围全部 allowlisted，并有 SSRF、速率、并发、deadline 和 cancellation 防护；
7. connector API error、pagination、retry、cursor、credential failure 有可观测状态；
8. revoke/pause/isolate 能停止采集和健康检查；
9. 跨 tenant/environment/zone/node 的安全测试通过；
10. E2E 主流程和安全回归矩阵通过；
11. health/readiness/metrics/audit/runbook/rollback 完整；
12. 没有未接受的 P0/P1 授权、数据泄露、范围扩张或失控任务风险。

## 18.15 企业网络目标的最终结论

“探测整个企业网络”的正确工程目标是：

> **建立一个授权范围内、多数据源、可持续更新、带来源和置信度的企业环境模型，并让 Agent 基于证据进行解释、诊断和建议。**

实施顺序必须是：

```text
被动权威数据
  → 多节点/区域身份
  → 统一 facts/entities/relationships
  → 变更和过期管理
  → 登记服务的低影响 healthcheck
  → 证据驱动诊断
  → 明确授权的有限主动测量
  → 独立安全评审后再考虑生产写操作
```

这样既能覆盖企业主机、网络区域、Kubernetes、云账户、CMDB、监控和服务注册中心，又不会把 Syscity 变成一个无边界扫描器。企业网络“完整理解”应当是**逐步提高覆盖率和证据质量**，而不是一次性扩大权限和探测范围。  

---

## 17.17 Beta 阶段必须落地的关键文件与验收门

### 代码落点

- `src/environment/`：Environment domain、事实模型、PolicyEngine、jobs、collectors、connectors；
- `src/config/types.rs`：Beta feature flag、Connector/healthcheck/retention/limits 配置；
- `src/gateway/state.rs`、`src/gateway/init/`：EnvironmentState、store、registry、scheduler、shutdown；
- `src/gateway/ws/admin_ws/environment.rs`：注册、节点、tree、facts、jobs、healthcheck handlers；
- `src/gateway/ws/core.rs`、`src/gateway/protocol.rs`：dispatch、scopes、unknown/default-deny；
- `src/memory/db.rs` 或独立 `EnvironmentStore`：事实、observations、entities、relationships、diff、jobs 迁移；
- `src/mcp/connectors/`、`src/cloud/`、`src/computer/network.rs`：Connector 和只读 healthcheck 适配；
- `src/security/request_context.rs`、`src/security/persistent_audit.rs`、`src/tools/rbac.rs`、`src/tools/approval.rs`：资源授权、审计、审批和撤销；
- `tests/integrations/`、`tests/e2e/`、`tests/security_audit_tests.rs`、`evals/environment/`：Beta 合同、安全和端到端测试。

### 关键验收门

1. **Registration gate**：未批准 registration 不能采集；revoked/pause/isolate 后新任务被拒且运行任务可取消。
2. **Scope gate**：Kubernetes namespace、Cloud resource group、CMDB environment、healthcheck target 不得越界；客户端不能自授 scope。
3. **Data gate**：Secret data、token、Cookie、私钥、受限 raw log 不进入 facts、prompt、audit 或 metrics。
4. **Fact gate**：每条事实都有 source、observed_at、confidence、expires_at、provenance、sensitivity 和 scope；diff/TTL/conflict 结果稳定可重放。
5. **Connector gate**：mock contract、pagination/cursor、retry/429/timeout、idempotency、credential isolation 和 connector shutdown 全部有测试。
6. **Healthcheck gate**：只访问 allowlist；有 deadline、rate、concurrency、response-size、redirect/SSRF 防护和 cancellation。
7. **Tree/event gate**：跨 node/environment 的 facts、tree、changes 和 events 均按 RequestContext/audience 隔离。
8. **Agent gate**：诊断 Agent必须引用证据并标注时间/置信度/coverage gap；Beta 没有 write tool，不能扩大 scope。
9. **E2E gate**：完成 install/register/approve/heartbeat/observe/persist/tree/query/diff/diagnose/healthcheck/revoke/uninstall 主流程。
10. **Release gate**：无未接受 P0/P1 安全风险；migration/rollback/backup/restore、health/readiness/metrics、operator runbook 和 SBOM 均通过。

### Beta 与现有能力的对应关系

| Beta 能力 | 当前可复用基础 | 必须新增的边界 |
|---|---|---|
| Kubernetes read-only | Connector manifest/lifecycle/state、SecretRef、Gateway WS | Kubernetes API client、namespace/resource scope、Secret field redaction |
| Cloud inventory | `src/cloud/` client/session/provider | resource-group scope、normalized asset model、read-only IAM |
| CMDB | Connector trait/state patterns | cursor/idempotency、source priority、conflict model |
| Observability | `src/observe/`、health/metrics、OTel-compatible data paths | bounded query、sample/time/cardinality limits、fact normalization |
| Multi-node tree | device pairing、Tailscale/node data、Gateway events | first-class node registry、environment ownership、audience filtering |
| Facts diff | memory DB、knowledge graph、observe retention | temporal facts、provenance、TTL、conflicts、tombstones |
| Diagnosis Agent | Agent engine、Memory/RAG、eval harness | evidence-only context、freshness/confidence、read-only tools、diagnosis evals |
| Network healthcheck | `NetworkInspector`、Linux network diagnostics | allowlisted target model、SSRF/redirect defense、rate/deadline/cancel |
| E2E/security audit | Gateway state tests、security audit tests、MCP/integration tests | mock external services、cross-scope matrix、revocation and data-leak regressions |

## 17.18 Beta 发布后的观测指标

Beta 发布后至少持续观察：

```text
environment_registration_pending_total
environment_registration_active_total
environment_sensor_heartbeat_age_seconds
environment_observation_started_total
environment_observation_completed_total
environment_observation_failed_total
environment_observation_duration_seconds
environment_observation_queue_depth
environment_connector_health
kubernetes_connector_api_errors_total
connector_rate_limit_total
facts_upsert_total
facts_conflict_total
facts_stale_total
facts_diff_added_total
facts_diff_removed_total
facts_diff_changed_total
environment_healthcheck_started_total
environment_healthcheck_failed_total
environment_healthcheck_timeout_total
environment_policy_denied_total
environment_policy_approval_total
environment_audit_write_failed_total
```

指标标签必须有界，只允许固定的 `connector_id`、`environment_kind`、`result`、`reason_class` 等标签；不能把 hostname、path、CIDR、raw query、resource ID 或 secret 放进高基数标签。

## 17.19 Beta 运行手册

Beta 发布前必须有 operator runbook，至少包括：

1. 如何安装和验证签名；
2. 如何查看 registration 和 capability manifest；
3. 如何审批或拒绝节点；
4. 如何暂停、隔离、撤销和卸载；
5. 如何轮换 credential/certificate；
6. 如何限制 Kubernetes namespace、Cloud resource group 和 healthcheck target；
7. 如何处理 connector 429、timeout、schema drift 和凭据失效；
8. 如何查看 observation job、facts source、diff 和 stale 状态；
9. 如何处理诊断 Agent 的低置信度或证据冲突；
10. 如何执行 emergency kill switch；
11. 如何恢复数据库和回滚版本；
12. 如何导出或删除指定环境的 facts 和 audit；
13. 如何确认卸载后没有残留 job、credential 和 connector session。

## 17.20 Beta 研究结论

Beta 的合理目标不是“完成环境探测”，而是验证一条完整、受控、可证据化的闭环：

```text
已批准身份
  → 明确能力和资源范围
  → 只读 Connector/Collector
  → 统一 FactEnvelope
  → 多节点环境树
  → 可重放的 snapshot diff
  → 证据驱动的诊断 Agent
  → allowlisted read-only healthcheck
  → 可撤销、可审计的 E2E 生命周期
```

当 B0–B6 完成后，可以在内部环境进行只读 Beta；当 B7–B9 完成后，可以在低敏感、明确授权的 staging 或生产试点中发布；任何生产写操作、无界网络测量、漏洞验证或自动修复都应留在 Beta 之后，并单独进行安全评审。

---

# 19. 技术设计：单环境企业网络 Agent

> 本节是设计和技术设计，不是实现承诺。本轮不修改 Rust 源码、不新增模块、不接入真实企业网络。实现时应逐阶段通过安全闸门和 Beta Release Gate。

## 19.1 范围决策：不做共享多租户

本路线明确排除共享多租户内核：

- 一个企业/受控环境对应一个独立 Syscity 实例、进程或容器；
- 一个实例内部可以管理多个 site、zone、node、collector 和 connector；
- 外部编排系统（如未来需要）负责实例创建、注册、升级、暂停、撤销、配额和发布；
- Syscity 实例内部不同时承载多个互不信任租户；
- 不实现共享数据库的跨租户查询隔离；
- 不实现跨租户共享 cache、TaskRegistry、事件总线或 AuthManager；
- 不把“共享多租户”作为 Beta 或企业网络第一阶段的前置条件。

因此，设计中的 `tenant_id` 在本路线中表示**部署边界或外部企业环境标识**，主要用于审计、导出、编排和防止误关联；实例内部真正的安全层级是：

```text
environment → site → zone → node → collector/job/asset/service
```

如果未来需要共享多租户，必须另立架构项目，不得在本设计中隐式实现。

## 19.2 目标架构

```text
┌──────────────────────────────────────────────────────────┐
│                  External Operator / Control              │
│  install · approve · pause · revoke · upgrade · export   │
└────────────────────────────┬─────────────────────────────┘
                             │ authenticated WS / mTLS
┌────────────────────────────▼─────────────────────────────┐
│                    Syscity Environment Gateway             │
│  registration · grants · jobs · policy · audit · events    │
└───────────────┬──────────────────┬────────────────────────┘
                │                  │
      ┌─────────▼────────┐ ┌───────▼──────────┐
      │ Endpoint Sensors  │ │ Zone Collectors  │
      │ host facts        │ │ bounded checks   │
      └─────────┬────────┘ └───────┬──────────┘
                │                  │
                └────────────┬─────┘
                             │ ObservationBatch
                ┌────────────▼────────────────┐
                │ Normalize / Redact / Verify │
                └────────────┬────────────────┘
                             │
                ┌────────────▼────────────────┐
                │ EnvironmentStore             │
                │ facts · entities · edges     │
                │ evidence · changes · jobs   │
                └────────────┬────────────────┘
                             │ policy-filtered query
                ┌────────────▼────────────────┐
                │ Agent / Diagnostic Plane     │
                │ explain · compare · diagnose │
                │ recommend only               │
                └──────────────────────────────┘
```

### 数据流原则

1. Collector 永远不直接写 Agent prompt；
2. Connector 永远不直接返回任意 JSON 给模型；
3. 所有结果先进入 `ObservationBatch`；
4. 所有 batch 经过 scope 校验、大小限制、脱敏、归一化和 provenance 标注；
5. 只有 EnvironmentStore 的 policy-filtered query 结果才能进入 Agent context；
6. Agent 的诊断和推荐不改变授权范围；
7. 推荐转 ActionPlan 后必须停止，Beta 不执行生产写操作。

## 19.3 Rust 领域模块设计

建议新增独立 `src/environment/`，避免把企业环境状态混入普通 conversation memory、turn observation 或 dreaming graph：

```text
src/environment/
├── mod.rs
├── ids.rs              # EnvironmentId, SiteId, ZoneId, NodeId, JobId
├── model.rs            # profiles, registrations, grants, states
├── scope.rs            # resource selectors and containment
├── capabilities.rs     # capability manifest and grants
├── registration.rs     # enrollment, approval, heartbeat, revoke
├── policy.rs           # deterministic PolicyEngine
├── observation.rs      # Collector, ObservationContext, ObservationBatch
├── jobs.rs             # ObservationJobManager and run lifecycle
├── facts.rs            # facts, evidence, provenance, TTL, changes
├── topology.rs         # entities and relationships
├── store.rs            # EnvironmentStore facade
├── redaction.rs        # secret/PII/sensitivity handling
├── audit.rs            # environment audit helpers
├── healthcheck.rs      # registered-target L1 checks
├── collectors/
│   ├── mod.rs
│   ├── host.rs
│   ├── network_local.rs
│   ├── services.rs
│   └── filesystem.rs
└── connectors/
    ├── mod.rs
    ├── kubernetes.rs
    ├── cloud_inventory.rs
    ├── cmdb.rs
    ├── observability.rs
    ├── dns_tls.rs
    └── load_balancer.rs
```

### 19.3.1 强类型标识

不使用任意字符串作为安全边界。建议：

```rust
struct EnvironmentId(String);
struct SiteId(String);
struct ZoneId(String);
struct NodeId(String);
struct CollectorId(String);
struct ObservationJobId(String);
struct FactId(String);
struct ConnectorId(String);
```

所有 ID 应校验：

- 长度上限；
- 字符集；
- 不含路径分隔符；
- 不含控制字符；
- 不把 secret/token 编码进 ID；
- 对外序列化稳定、对内比较精确。

### 19.3.2 环境层级

```rust
struct EnvironmentScope {
    environment_id: EnvironmentId,
    site_id: Option<SiteId>,
    zone_id: Option<ZoneId>,
    node_id: Option<NodeId>,
    job_id: Option<ObservationJobId>,
}
```

作用域关系必须显式实现：

```text
environment grant 允许其子 site/zone/node
site grant 只允许其子 zone/node
zone grant 不自动允许另一个 zone
node grant 不自动允许同环境其他 node
job grant 只能用于该 job 的 collector/run
```

## 19.4 Capability 和 ResourceSelector

### Capability ID

```text
host.identity.read
host.os.read
host.hardware.read
host.network.interface.read
host.network.route.read
host.services.read
host.processes.read
host.files.read
host.logs.read
network.healthcheck
kubernetes.inventory.read
cloud.inventory.read
cmdb.inventory.read
observability.query.read
topology.query.read
facts.query.read
```

Beta 禁止默认启用：

```text
arbitrary.shell
packet.capture
credential.read
filesystem.read.root
service.restart
config.write
firewall.write
dns.write
traffic.switch
```

### ResourceSelector

ResourceSelector 是 PolicyEngine 的确定性输入，不是 prompt 文本：

```rust
enum ResourceSelector {
    Path { root: PathBuf },
    Cidr { network: IpNet },
    Host { hostname: String },
    Service { service_id: String },
    Port { host: String, port: u16, protocol: Protocol },
    Namespace { cluster_id: String, namespace: String },
    CloudResource { account: String, region: Option<String>, kind: String },
    CmdbQuery { collection: String, filter_id: String },
    MetricQuery { source_id: String, matcher_id: String },
}
```

任何 selector 都必须经过：

1. canonicalize；
2. parent scope containment；
3. sensitivity check；
4. rate/size budget check；
5. expiration/revocation check。

## 19.5 PolicyEngine 技术契约

```rust
enum PolicyDecision {
    Allow(PolicyGrant),
    Deny { reason: DenyReason, policy_version: String },
    NeedsApproval { approval: ApprovalRequirement, policy_version: String },
}
```

```rust
struct PolicyRequest {
    actor: PrincipalId,
    scope: EnvironmentScope,
    capability: CapabilityId,
    selector: ResourceSelector,
    operation: OperationClass,
    sensitivity: Sensitivity,
    job_id: Option<ObservationJobId>,
    approval_id: Option<String>,
    policy_version: String,
}
```

PolicyEngine 必须拒绝：

- 未注册 collector；
- paused/isolated/revoked registration；
- capability 未声明或未授予；
- selector 超过 parent scope；
- 过期 grant；
- 过期 job；
- 超过 rate/concurrency/bytes/target budget；
- L2/L3 无审批；
- sensitivity 高于授权上限；
- 不能验证的 DNS/目标；
- 试图由模型扩大 scope。

PolicyDecision 必须写入审计，包含：

```text
actor
registration
capability
selector
scope
policy_version
decision
reason
approval_id
correlation_id
```

## 19.6 ObservationBatch 和事实管道

```rust
struct ObservationBatch {
    run_id: ObservationRunId,
    collector_id: CollectorId,
    source_id: String,
    observed_at: DateTime<Utc>,
    coverage: Coverage,
    facts: Vec<EnvironmentFactInput>,
    entities: Vec<EntityInput>,
    relationships: Vec<RelationshipInput>,
    evidence: Vec<EvidenceRef>,
    warnings: Vec<ObservationWarning>,
    budget: BudgetUsage,
}
```

处理管道：

```text
Collector
  → batch size/bytes validation
  → scope validation
  → schema validation
  → secret/PII redaction
  → sensitivity classification
  → canonical ID normalization
  → provenance/evidence hash
  → idempotent upsert
  → deterministic diff
  → audit and metrics
```

部分失败必须显式标记：

```text
complete
partial
truncated
failed
cancelled
```

不能把“只返回了前 1000 个对象”报告成完整库存。

## 19.7 企业网络源适配器设计

### CMDB/IPAM/DNS

- 显式 collection/zone allowlist；
- server-side pagination；
- `updated_since` 或 change token；
- ETag/If-Modified-Since；
- record type allowlist；
- 每 zone 最大记录数和 bytes；
- DNS 只查询配置的 zone/name，不做 wordlist 子域枚举；
- A/AAAA/CNAME/SRV/HTTPS 关系归一化；
- TTL 和 source provenance。

### Kubernetes

- cluster/namespace/kind/label/field selector；
- 初始 paginated list；
- Beta 后再 watch；
- resourceVersion checkpoint；
- watch 断线 bounded relist；
- object bytes/count cap；
- Secret data 永不读取；
- Service → EndpointSlice → Pod → Node 关系；
- Ingress/Gateway → Service 关系。

### Cloud inventory

- account/project/subscription allowlist；
- region allowlist；
- resource kind allowlist；
- provider pagination token；
- retry-after/circuit breaker；
- stable provider resource ID；
- read-only IAM；
- Instance/Network/LB/Database/ManagedService normalization。

### Prometheus

- endpoint 明确配置；
- metric name/matcher allowlist；
- 时间窗口和 step 上限；
- series/sample/response bytes 上限；
- label 数量和长度上限；
- 禁止空 matcher 的全量 `/series` 查询；
- 只将显式允许的 service/target/health metrics 变成 facts。

### OpenTelemetry

- bounded OTLP body/batch；
- attributes allowlist；
- resource attribute 长度和数量限制；
- queue/backpressure；
- tail/probabilistic sampling；
- 只从明确 semantic conventions 派生关系；
- 不把每个 URL/label 自动创建成实体。

### Load Balancer/Service Registry

- 只查询配置的 LB/listener/backend pool；
- VIP → listener → route → target group → backend；
- 优先使用 provider reported health；
- 不从发现的 VIP 自动生成健康检查目标。

## 19.8 Enterprise Network Coordinator

所有 source adapter 都通过统一 coordinator 调度：

```rust
struct SyncBudget {
    max_entities: usize,
    max_relationships: usize,
    max_pages: usize,
    max_bytes: u64,
    max_concurrency: usize,
    max_requests_per_second: u32,
    deadline: Duration,
    max_retries: u32,
}
```

Coordinator 负责：

- 全局/每 source semaphore；
- deadline/cancellation；
- cursor/checkpoint；
- retry/backoff/circuit breaker；
- bounded queue/batch；
- idempotency key；
- partial/truncated 状态；
- stale/tombstone policy；
- source health；
- job audit。

建议 `SyncResult`：

```text
run_id
source_id
started_at
finished_at
pages_fetched
entities_seen
entities_applied
relationships_seen
relationships_applied
rejected_limit
rejected_validation
bytes_read
cursor/checkpoint
truncated
status
error_category
```

## 19.9 可观测性和操作控制

新增 bounded-label 指标：

```text
enterprise_source_sync_started_total
enterprise_source_sync_completed_total
enterprise_source_sync_failed_total
enterprise_source_sync_duration_seconds
enterprise_source_sync_queue_depth
enterprise_entities_applied_total
enterprise_relationships_applied_total
enterprise_facts_conflict_total
enterprise_facts_stale_total
enterprise_healthcheck_started_total
enterprise_healthcheck_failed_total
enterprise_healthcheck_timeout_total
enterprise_policy_denied_total
enterprise_connector_health
```

标签只能使用固定的 source class、connector ID、environment kind、result class、reason class。禁止 hostname、raw IP、path、CIDR、URL、resource ID 作为高基数 labels。

所有长期同步任务必须注册到 `TaskRegistry`，所有 source adapter 必须响应 cancellation/shutdown。当前 channels/MCP 中存在未统一 shutdown 的 loop，不能直接复制到企业 coordinator。

## 19.10 Enterprise E2E 与安全审计矩阵

### 主流程

```text
create environment/site/zone
  → register sensors/collectors
  → approve grants
  → passive sync S0/S1/S2
  → normalize/persist topology
  → build tree
  → run N1 healthchecks
  → mutate fake source
  → resume cursor
  → diff facts/relationships
  → diagnose from evidence
  → revoke a node/zone
  → cancel active jobs
  → verify events/query isolation
  → export/delete scoped data
  → uninstall
```

### 必须覆盖

- self-admin scope escalation；
- cross-environment facts/events；
- CIDR/path/namespace/resource selector escape；
- DNS rebinding/SSRF/redirect；
- empty matcher/high cardinality query；
- oversized page/object/body/label；
- pagination/cursor duplicate/replay；
- connector 429/5xx/timeout/credential failure；
- job overlap/retry/deadline/cancellation；
- revoke/pause/isolate during active sync；
- secret/PII/raw log absence from facts/prompt/audit；
- source conflict and tombstone; 
- restart/recovery and database failure；
- audit persistence and redaction；
- no implicit CIDR expansion；
- metrics cardinality；
- WS event audience；
- health/readiness/metrics and operator kill switch。

### 证据和测试环境

只使用：

- in-memory/test SQLite；
- deterministic clock；
- fake Kubernetes API；
- fake CMDB/Cloud/Prometheus/OTel/LB APIs；
- fake DNS/TCP/HTTP/TLS targets；
- fixed keys and fixtures；
- mock collectors and connector adapters。

不要将真实企业网段、真实 token、真实 Kubernetes Secret 或真实公网作为自动化测试目标。

## 19.11 企业网络 Beta Release Gate

只有以下条件全部满足才能发布“企业网络只读 Beta”：

1. M0 control-plane security gate 通过；
2. 被动 source adapter 可产生带 provenance/freshness 的统一 facts；
3. multi-node/zone tree 具有 owner、stale、revoked、audience isolation；
4. N1 healthcheck 只访问 allowlisted registered targets；
5. coordinator 所有 budgets、cursor、deadline、cancel、retry 和 partial status 有测试；
6. Facts diff/TTL/conflict/evidence 可确定性回放；
7. Diagnostic Agent 只能读取 policy-filtered facts，不拥有 write 或 scope expansion tool；
8. Connector credentials 不进入 LLM、raw facts、audit details 或 metrics；
9. revoke/pause/isolate 能停止同步和 healthchecks；
10. 跨 environment/site/zone/node 的安全矩阵通过；
11. E2E 主流程、migration/backup/restore、health/readiness/metrics、runbook、rollback 通过；
12. 无未接受的 P0/P1 授权、数据泄露、范围扩张或任务失控风险。

## 19.12 企业网络 rollout

```text
internal passive inventory
  → one staging zone
  → one Kubernetes namespace
  → one cloud account/resource group
  → multi-zone staging
  → low-sensitivity production read-only
  → approved production Beta
```

每一批都要：

- 明确 owner/scope；
- 观察 source 健康；
- facts freshness 可接受；
- 无跨环境泄露；
- 预算和资源使用稳定；
- 可一键 pause/revoke；
- 可回滚到 feature flag disabled 版本。

N2 主动测量、N3 变更和自动修复不在第一版企业网络 Beta 中。

## 19.13 本目标是否已有实现路径

结论分三层：

### 路径完整、可进入实现设计

- 单主机 L0/L1 只读 sensor；
- identity/capability/scope/job/fact domain；
- EnvironmentStore 和 facts provenance/TTL；
- registration/heartbeat/revoke；
- N0 passive inventory；
- N1 registered healthcheck；
- topology tree、facts diff 和只读诊断 Agent；
- mock-based E2E 和安全矩阵。

### 路径明确，但需要外部协议/适配器设计

- Kubernetes API connector；
- Cloud inventory adapters；
- CMDB/IPAM/DNS；
- Prometheus/OTel；
- Load Balancer/Service Registry；
- 多源事实冲突和 source precedence；
- 区域 Collector 的 checkpoint/lease/协调。

### 必须独立安全评审后实现

- N2 广泛主动测量；
- N3 生产变更；
- 自动修复；
- 跨区域分布式协调；
- 任何共享多租户内核。

因此，`environment-agent.local.md` 已经覆盖了目标的**设计路径和实施顺序**，但当前代码并没有完成这些能力。当前没有 `src/environment/`、EnvironmentStore、PolicyEngine、registration registry、topology store、企业 Connector 或 Network Zone Collector。后续实现应从 M0/EN0 安全控制面开始，而不是直接写网络扫描逻辑。

---

# 20. Gamma：现实电子环境与网络环境扩展设计

本部分定义传统 IT、云、Kubernetes、CMDB、监控和受控企业网络之外，现实物理世界中还应纳入设计考虑的电子环境。Gamma 不是 Beta 的默认交付范围，而是后续环境适配和行业化设计的边界。

## 20.1 环境分类

```text
Endpoint / Server / Application
DataCenter / OfficeNetwork / BranchNetwork
CloudAccount / CloudRegion / VpcOrVnet / KubernetesCluster / NetworkZone
TelecomNetwork / WirelessNetwork / IoTNetwork
IndustrialOt / BuildingManagement / SecuritySystem
VehicleOrRobotFleet / MedicalSystem
CriticalInfrastructure / SatelliteGroundSegment
InternetMeasurementScope
```

每种环境类型都必须声明：

- 数据源和 Connector；
- 资产、实体和关系模型；
- 默认观察级别；
- 能力和敏感级别；
- 是否允许主动 healthcheck；
- 是否需要特殊审批、维护窗口或合规审查；
- 是否可以生成 ActionPlan；
- 是否允许写操作；
- retention/delete/export 策略；
- 事件和告警模型。

不能把所有电子环境简化成“一个 IP + 一组端口”。数字身份、物理位置、所有者、业务用途、控制能力、安全等级和生命周期同样属于环境模型。

## 20.2 传统 IT、业务和数据中心环境

### 终端和服务器

包括笔记本、台式机、Linux/macOS/Windows 服务器、虚拟机、CI/CD Runner、跳板机、应用服务器、数据库和缓存节点。

可观察：

- 主机、OS、内核、架构；
- CPU、内存、磁盘和资源；
- 进程、服务和版本；
- 网络接口、地址、路由、DNS 和代理；
- 明确授权目录的文件 metadata；
- 明确授权的日志和监控摘要；
- 节点 heartbeat、版本和 capability。

默认 `L0`、只读、非 root。不要默认全盘读取、任意 Shell、桌面控制、抓包或收集命令行中的秘密。

### 业务应用和服务

包括 API、Web、数据库、缓存、队列、Service Registry、Load Balancer、Service Mesh、Deployment、CI/CD 和变更系统。

重点是形成证据驱动的依赖关系：

```text
Application
  ├── runs_on → Node
  ├── depends_on → Database/Cache/Queue
  ├── routes_to → LoadBalancer
  └── deployed_by → Release
```

优先使用 CMDB、Kubernetes、Cloud API、Service Registry、Prometheus/OTel、CI/CD 和工单，而不是自动扫描所有端口。

### 数据中心物理基础设施

包括机柜、PDU、UPS、制冷、BMS、DCIM、温湿度传感器、电力监控、门禁、资产标签和配线架管理系统。

默认只读：设备位置、资产编号、电源状态、功耗、温湿度、机柜占用、告警和维护窗口。通用 Agent 不得自动关闭 PDU、断电、修改冷却或门禁策略；这些属于独立的物理控制域。

## 20.3 IoT、无线和电信环境

### IoT 和智能设备

包括摄像头、NVR、门禁、打印机、NAS、智能电视、会议设备、照明、门锁、环境传感器、IoT 网关和嵌入式设备。

模型应包括：

```text
IoTDevice
  ├── owner/site/zone
  ├── vendor/model/firmware
  ├── network_identity
  ├── management_plane
  ├── data_sensitivity
  └── control_capabilities
```

Gamma 初期只读取资产、型号、固件、在线状态、证书、容量、健康和告警；不修改固件、管理员、网络、Wi-Fi，也不直接控制门锁、照明或摄像头。

### 电信和通信网络

包括 4G/5G RAN/核心网、基站、Wi-Fi Controller、WLAN、SD-WAN、MPLS、BGP、DNS、CDN、NTP、VoIP/SIP/SBC、短信网关、VPN、专线和通信卫星链路。

除主机外还要建模：

```text
subscriber / access_network / radio_core_network
transport / service_edge / ASN / route_prefix
VRF / VLAN / VXLAN / wireless_controller / link
```

优先通过 NMS/OSS/BSS、SNMP、NETCONF、RESTCONF、gNMI 和供应商 API 只读获取链路、路由、设备、SLA、版本、证书和告警。修改 BGP、无线、SIP 或核心网参数必须独立审批。

### 无线、电磁和射频环境

包括 Wi-Fi、Bluetooth、Zigbee、Thread、LoRaWAN、RFID/NFC、5G/4G、GNSS、专网无线、无线传感器和频谱设备。

优先读取 Wireless Controller、频谱平台或合法被动测量设备的数据：AP/设备、频道、覆盖、信号、干扰、认证、漫游和固件。默认禁止主动发射、干扰、伪装接入和未授权射频控制。

## 20.4 OT/ICS 和关键物理系统

### 工业控制网络

包括 PLC、SCADA、DCS、RTU、HMI、Historian、工业交换机/防火墙、生产线、能源和交通控制系统：

```text
Enterprise IT → IT/OT DMZ → SCADA/Historian
                         → Control Zone → PLC/RTU/Field Devices
```

默认仅 `L0`：读取资产清单、只读监控、被动流量摘要、设备配置摘要、告警和变更。禁止通用 Agent 主动协议探测、写 PLC/RTU、改控制参数、改联锁或重启控制设备。

### 车辆、机器人和移动设备

包括车队、车载网关、无人机、AGV/AMR、工业机器人、物流设备、移动终端、船舶和航空电子系统。

使用 `CyberPhysicalNode`：

```text
identity / interfaces / firmware / physical_location
owner/operator / safety_classification
control_capabilities / allowed_action_window
```

默认只读资产、位置、软件/固件、在线、电量、传感器摘要、告警和连接质量；不执行运动控制、方向/速度、电源、固件刷写或传感器禁用。

### 楼宇、安防和物理控制

包括门禁、摄像头、NVR、人脸识别、入侵报警、电梯、访客、门锁、BMS、消防和 HVAC。

能力必须分开：

```text
security.inventory.read
security.health.read
security.event.read
security.video.metadata.read
security.access.write
security.lock.write
security.alarm.write
building.hvac.write
```

Gamma 默认只读资产、健康、事件 metadata、在线状态、容量、固件、证书和审计摘要。开门、报警、消防、电梯和 HVAC 控制属于独立高风险物理动作域。

### 医疗系统

包括医疗设备、病房监控、影像/PACS、HIS/EMR、实验室、药房、手术设备、病人监护和医疗 IoT。

默认只读设备资产、系统健康、网络状态、固件、证书、非患者事件摘要、容量和可用性。患者数据和设备控制必须有独立的合规、身份、数据和安全边界，不能进入普通 Environment Agent 默认上下文。

### 交通、能源和关键基础设施

包括铁路信号、交通信号、公交调度、机场地面系统、港口、水务、电力、燃气、能源调度和应急通信。

推荐：

```text
Passive inventory → Vendor/NMS API → Read-only telemetry
→ Maintenance-window healthcheck → Human-reviewed recommendation
```

通用 LLM Agent 不得直接连接关键控制面，也不能把 IT 网络扫描策略复制到关键基础设施。

### 卫星和空间通信

包括卫星地面站、链路、遥测、航天器控制网络、卫星互联网、GNSS、地面终端和天线系统。

需要建模轨道/站点、地面站、链路窗口、频段、Telemetry、Command Channel、可见性、延迟和安全等级。默认只读遥测和地面资产，禁止连接 command channel 或执行天线、轨道和航天器控制。

## 20.5 Gamma 环境类型的技术策略

```text
EnvironmentProfile
  kind
  default_observation_level
  allowed_data_sources
  capabilities
  active_check_policy
  compliance_class
  action_policy
  retention_policy
  connector_set
  event_policy
```

示例：

```text
IndustrialOt
  default_level = L0
  active_probe = disabled
  write_action = disabled
  special_review = required

OfficeNetwork
  default_level = N0
  healthcheck = registered_targets_only
  write_action = disabled

KubernetesCluster
  default_level = passive_api_read
  namespace_scope_required = true
  secret_read = disabled
  write_action = disabled

CriticalInfrastructure
  default_level = passive_only
  active_probe = special_approval
  act = disabled
```

## 20.6 Gamma 分阶段实施

```text
Gamma-0  IT/Cloud/Kubernetes/CMDB/Observability
Gamma-1  IoT、无线、边缘、数据中心设施（只读）
Gamma-2  Telecom、SD-WAN、MPLS、BMS、安防（专用 Connector）
Gamma-3  OT/ICS、医疗、车辆、机器人（独立安全边界）
Gamma-4  交通、电力、水务、关键基础设施（独立产品线）
Gamma-5  卫星、空间通信和特殊电子系统（专用审查）
```

每个 Gamma 阶段都需要独立的：

- threat model；
- capability matrix；
- data sensitivity policy；
- vendor/API contract；
- read-only adapter；
- maintenance-window policy；
- E2E fixture；
- safety review；
- uninstall/revoke runbook。

## 20.7 Gamma 最终边界

电子环境范围可以扩展到：

```text
传统 IT
+ Cloud/Kubernetes
+ Enterprise Network
+ Telecom/Wireless
+ IoT
+ Edge/Branch
+ Data Center Facilities
+ Building/Security Systems
+ OT/ICS
+ Vehicles/Robotics
+ Medical Systems
+ Critical Infrastructure
+ Satellite Ground Systems
```

但这些环境不能共享同一套默认权限和主动探测策略。正确技术路线是：

```text
环境类型识别
  → 专用身份
  → 专用 Connector
  → 专用数据模型
  → 专用观察级别
  → 专用合规/安全闸门
  → 只读事实汇聚
  → 证据驱动诊断
  → 独立审批后的有限动作
```

> **电子环境越接近真实物理世界，Agent 越应从“主动执行者”退回到“被动观察者、解释者和建议者”；任何可能改变物理状态、生命安全、生产连续性或公共基础设施的操作，都必须离开通用 Agent 默认能力，进入独立的专业控制和人工变更流程。**

本 Gamma 部分是环境分类和技术边界设计，不代表这些环境的 Connector、协议适配器或控制能力已经实现。

---

# 21. 环境感知之后：如何利用环境提供 Agent 服务

环境感知的目的不只是“知道环境里有什么”，而是把环境转化为 Agent 的工作上下文、决策依据、风险边界和验证对象。核心闭环是：

```text
Observe → Understand → Explain → Recommend → Approve → Act → Verify
```

更完整的数据和控制流为：

```text
观察环境
  → 建立环境模型
  → 查询相关事实
  → 理解当前状态
  → 生成诊断/计划
  → 请求授权
  → 执行受控动作
  → 验证结果
  → 更新环境模型和审计记录
```

核心原则：

> **环境事实可以改变 Agent 的工作策略，但不能自动改变 Agent 的权限。**

## 21.1 将环境作为 Agent 的工作上下文

普通聊天 Agent 主要知道用户当前说了什么；Environment Agent 还可以在授权范围内知道：

- 当前所在的环境、site 和 zone；
- 当前节点和 collector 状态；
- 当前运行的服务、应用和部署；
- 服务之间的依赖关系；
- 最近发生的版本、配置和拓扑变化；
- 哪些 facts 是新鲜、过期或冲突的；
- Agent 当前拥有的 capability 和 resource scope；
- 哪些资源明确不在授权范围内。

例如用户询问：

> “为什么 API 变慢了？”

Agent 不应只给出泛化的排查清单，而应构建基于证据的上下文：

```text
目标服务：api-gateway
所在区域：production-zone-a
运行节点：node-12, node-13
最近变化：
  - 15 分钟前发布 version 2.8.1
  - node-13 CPU 从 42% 升到 91%
  - 数据库连接池使用率达到 96%
  - 两个后端服务出现 timeout
健康检查：
  - api-gateway HTTP 200，但 P95 延迟增加
  - database TCP 可达
  - cache health 正常
```

输出应包含：

- 已确认事实；
- 可能原因；
- 证据来源；
- 置信度；
- 覆盖缺口；
- 还需要哪些只读观察；
- 是否需要额外授权。

## 21.2 环境驱动工具选择

Syscity 目前工具数量很多。环境模型可以帮助 Agent 从“所有工具”中选择当前节点和任务实际允许的工具。

示例能力清单：

```text
当前环境：Linux server
可用：
  - system.read
  - process.list
  - service.status.read
  - network.interface.read
  - host.logs.read:/var/log/app
不可用：
  - desktop.control
  - browser.control
  - filesystem.read:/
  - service.restart
```

Agent 不应尝试调用：

- 不适用于当前平台的桌面工具；
- 浏览器控制；
- 全盘读取；
- 未批准的服务重启；
- 不属于当前环境或 zone 的网络资源。

工具选择流程：

```text
环境类型
+ 节点能力
+ 用户请求
+ 资源范围
+ 风险等级
+ policy grant
→ 可用工具集合
```

环境事实可以影响工具的**可用性和选择**，但不能自动新增 capability grant。

## 21.3 业务系统理解

Environment Agent 可以把业务系统建模为服务和依赖图：

```text
用户请求
  → Web/API
  → API Gateway
  → Service A
  → Cache
  → Queue
  → Database
  → External Provider
```

Agent 可以回答：

- 业务由哪些服务组成；
- 某服务运行在哪些节点；
- 某数据库被哪些服务依赖；
- 某次发布影响了哪些应用；
- 某节点下线会影响什么；
- 哪些依赖没有冗余；
- 哪些服务没有健康检查；
- 哪些证书、镜像或软件版本即将过期；
- 哪些资源没有 owner。

这将 Syscity 从“会调用工具的 Agent”提升为：

> **能够理解业务拓扑的 Environment Agent。**

## 21.4 故障诊断

### 诊断链路

```text
故障现象
  → 查询相关环境事实
  → 读取时间线
  → 对比最近变化
  → 检查依赖关系
  → 运行受控 healthcheck
  → 形成诊断假设
  → 给出证据和置信度
```

### 示例

用户：

> “生产订单服务无法访问。”

Agent 可以生成只读诊断计划：

```text
1. 查询订单服务当前节点和部署版本
2. 检查 Load Balancer 状态
3. 检查 API Service/Ingress 路由
4. 检查订单服务 Pod 状态
5. 检查数据库和消息队列健康
6. 查询最近 30 分钟发布和变更
7. 运行已登记的 HTTP healthcheck
8. 对比最近一次正常快照
```

诊断结果应类似：

```text
初步判断：订单服务不可用更可能由 deployment rollout 卡住导致。

证据：
- 期望副本数：6
- 当前可用副本数：2
- 最近部署版本：order-api:2.8.1
- 两个 Pod 处于 CrashLoopBackOff
- Load Balancer 健康
- 数据库连接正常
- 故障开始时间与部署相差 3 分钟

置信度：0.86

尚未确认：
- CrashLoopBackOff 的具体应用原因
- 是否应回滚

建议下一步：
- 读取两个 Pod 的受控错误摘要
- 生成回滚 ActionPlan，但暂不执行
```

## 21.5 变更影响分析

环境模型可以在动作前估算影响范围：

> “如果把这个数据库节点下线，会影响哪些服务？”

Agent 查询：

```text
Database
  → dependent services
  → deployments
  → zones
  → owners
  → active healthchecks
  → recent metrics and changes
```

输出应包含：

- 直接和间接依赖；
- 受影响的节点、服务和区域；
- 是否存在冗余；
- 当前负载和连接峰值；
- 风险等级；
- 维护窗口建议；
- 快照和验证方案；
- 回滚或补偿方案。

环境感知在这里用于：

- impact analysis；
- blast radius estimation；
- dependency discovery；
- change planning；
- rollback preparation。

## 21.6 持续资产和配置管理

Agent 可以持续维护资产状态，而不是只在安装时观察一次：

```text
资产新增：node-27、service-report-v3
资产删除：old-cache-02
版本变化：nginx 1.25.3 → 1.25.4
拓扑变化：report-service 从 node-14 移到 node-18
健康变化：zone-b 的 3 个节点超过 5 分钟未上报
配置变化：Load Balancer 新增 backend
```

可以发现：

- 未登记资产；
- 影子服务；
- 版本漂移；
- 无 owner 资源；
- 过期证书；
- 单点故障；
- 未纳入监控的服务；
- 不符合基线的节点。

这些结果必须标注来源、时间、置信度和覆盖范围，不能把“未观察到”直接解释为“不存在”。

## 21.7 安全基线和风险发现

环境 Agent 可以比较：

```text
实际环境状态
  vs
期望安全基线
```

检查项可以包括：

- 生产节点是否启用磁盘加密；
- SSH 是否只允许管理网络；
- 服务是否使用 TLS；
- Kubernetes namespace 是否配置资源限制；
- 云存储是否公开；
- 节点是否运行过期软件；
- 是否存在未登记端口；
- 是否存在无 owner 资产；
- 是否存在过期证书；
- 是否存在未受监控服务。

输出格式：

```text
检查项
实际值
期望值
差异
证据
严重性
影响范围
修复建议
是否需要人工审批
```

Beta 只生成风险发现和建议，不自动修改防火墙、访问控制、EDR 或生产配置。

## 21.8 容量、资源和成本优化

Environment Agent 可以结合事实和历史变化提供：

- CPU/内存/磁盘趋势；
- 集群容量预测；
- 未使用资源识别；
- 云成本分析；
- 节点碎片化分析；
- 负载分布分析；
- 容量和冗余检查；
- 资源标签和 owner 完整性检查；
- 云配额和预算风险分析。

示例输出：

```text
当前容量预计可以支撑 18% 的增长，但 zone-b 的冗余不足。
建议增加 2 个节点，而不是直接扩大现有实例。
此建议不包含自动购买、自动扩容或自动变更。
```

## 21.9 事件响应和故障协同

Environment Agent 可以为故障创建结构化 Incident Context：

```text
Incident
  ├── symptoms
  ├── affected_services
  ├── affected_nodes
  ├── recent_changes
  ├── related_alerts
  ├── probable_causes
  ├── evidence
  ├── owner/team
  ├── timeline
  └── suggested_next_actions
```

它可以：

- 汇总相关事实；
- 关联告警、发布、日志摘要和拓扑；
- 生成事件时间线；
- 找出影响服务和区域；
- 推荐只读验证步骤；
- 生成工单或事故报告；
- 通知负责人；
- 等待批准后进入下一步流程。

它不应默认：

- 删除日志；
- 重启整个集群；
- 切换全部流量；
- 关闭告警；
- 禁用安全控制；
- 修改防火墙；
- 扩大网络范围。

## 21.10 环境自适应

### 允许的自适应

- Linux 节点选择 Linux typed collector；
- Kubernetes 环境显示 Kubernetes 资源；
- 无桌面能力时隐藏 desktop tools；
- staging 使用 staging healthcheck policy；
- facts 过期时建议重新观察；
- Connector 失联时降低结论置信度；
- 低带宽节点降低同步频率；
- 资源紧张时降低 observation 并发。

### 不允许的自适应

- 自行获得 root；
- 自行访问新网段；
- 自行启用抓包；
- 自行安装 Connector；
- 自行读取 Secret；
- 自行关闭审计；
- 自行修改撤销策略；
- 自行绕过审批；
- 自行从拒绝状态切换为允许状态。

## 21.11 Agent 角色分层

### Observer

只读环境、资产、健康、拓扑、变化、指标、证书和版本。

### Diagnostician

读取 facts、metrics、changes，生成故障假设和下一步观察。

### Planner

生成维护、扩容、回滚、修复和迁移计划，但不执行。

### Operator

只有在 action 获得明确批准、目标 scope 正确、存在 snapshot、rollback、verification 和 kill switch 时，才允许执行有限动作。

### Auditor

读取观察、变更、审批、事实来源、策略决策和运行结果，但不能修改环境。

不同角色必须由 capability/policy 决定，不能只靠 system prompt 区分。

## 21.12 环境数字孪生

当 facts、entities、relationships、changes、health 和 evidence 长期积累后，可以形成轻量 Environment Digital Twin：

```text
Environment Digital Twin
  ├── topology
  ├── assets
  ├── service dependencies
  ├── runtime state
  ├── ownership
  ├── historical changes
  ├── health timeline
  ├── policy posture
  ├── capacity
  └── evidence
```

它可以回答：

- node-12 失效会影响什么；
- 哪些服务没有冗余；
- 最近一次变更影响了什么；
- 哪个 zone 的事实已经过期；
- 哪些资源没有 owner；
- 哪些服务只位于一个故障域；
- 哪些依赖是权威事实，哪些只是低置信推断；
- 如何把应用从 zone-a 迁移到 zone-b。

数字孪生必须：

- 带时间；
- 带证据；
- 带置信度；
- 能表示未知；
- 能表示冲突；
- 能表示过期；
- 按 environment/site/zone/node 隔离。

不能只是 LLM 生成的静态拓扑图。

## 21.13 环境 Agent 服务闭环

成熟的 Environment Agent 可以按如下流程运行：

```text
1. 读取已批准 EnvironmentProfile
2. 检查 Sensor/Collector/Connector 状态
3. 执行被动 inventory 同步
4. 更新 facts/entities/relationships
5. 计算 snapshot diff
6. 更新 topology 和 health
7. 对已登记目标运行 N1 healthcheck
8. 发现异常或变化
9. 生成 evidence-aware diagnosis
10. 通知负责人或生成工单
11. 给出 Recommend
12. 如需要，生成 ActionPlan
13. 等待精确审批
14. 执行有限 action
15. 验证状态
16. 失败时 rollback/compensation
17. 写入 audit
18. 更新环境模型
```

## 21.14 不同环境的服务示例

### 笔记本

“我的机器为什么变慢？”

Agent 读取经过授权的资源、进程、启动项、磁盘和网络事实，给出诊断建议；不自动杀进程或删除文件。

### Kubernetes

“为什么订单服务失败率上升？”

Agent 对比 Deployment、Pod、Service、EndpointSlice、Prometheus 指标和事件，生成时间线与诊断；不自动回滚。

### 企业网络

“生产区哪些服务没有有效健康检查？”

Agent 对比 CMDB、Service Registry、Load Balancer 和已登记 healthcheck，标出缺失项并显示覆盖率；不自动扫描未知地址。

### 数据中心

“哪些机柜存在功耗或温度风险？”

Agent 读取 DCIM/PDU/BMS，按机柜聚合历史和告警，给出运维建议；不自动断电或修改制冷。

### OT/ICS

“哪些生产区域存在设备离线或版本漂移？”

Agent 读取工业资产和只读监控，关联维护记录，给出建议；不主动扫描控制网络，不连接 PLC 控制接口。

## 21.15 环境感知的核心服务

1. **Environment Search**：查询当前环境和资产；
2. **Environment Explain**：解释拓扑、依赖、状态和变化；
3. **Environment Diagnose**：结合证据分析故障和异常；
4. **Environment Recommend**：生成维护、扩容、修复和迁移建议；
5. **Environment Operate**：在审批、快照、验证和回滚保护下执行有限运维动作。

最终闭环：

```text
环境感知
  → 环境建模
  → 环境理解
  → 环境诊断
  → 环境建议
  → 环境审批
  → 环境操作
  → 环境验证
  → 环境记忆
```

## 21.16 关键边界

> **Observe、Explain、Recommend 可以作为 Beta 核心；Act 必须是最后阶段，并且不能由通用 Agent 默认拥有。**

环境事实可以改变 Agent 的查询、工具选择、诊断策略和同步频率，但不能自动改变 capability grant、scope、审批要求或撤销状态。越接近 OT、医疗、交通、电力、车辆、机器人或卫星控制，Agent 越应退回为观察者、解释者和建议者；可能改变物理状态、生命安全、生产连续性或公共基础设施的操作，必须进入独立的专业控制和人工变更流程。

---

# 22. 编码前必须补齐的技术契约

本节将前面各章的架构设计进一步收敛为编码前必须确定的接口、状态机、数据库、策略和生产运维契约。它不是新的产品范围，也不是实现代码，而是后续实现必须遵守的设计基线。

## 22.1 单环境部署边界

本路线不实现共享多租户内核。推荐的部署边界是：

```text
一个 Syscity 实例 / 进程 / 容器
  → 一个 EnvironmentProfile
  → 多个 Site
  → 多个 Zone
  → 多个 Node
  → 多个 Sensor / Collector / Connector
```

设计约束：

- 一个实例初始化时绑定一个 `environment_id`；
- Site/Zone/Node 可以在该 EnvironmentProfile 内增加、暂停、隔离和撤销；
- 一个 Sensor 默认只属于一个 node 和一个 zone；
- 一个 Zone 可以拥有多个 Collector，但每个 Collector 有独立身份；
- EnvironmentProfile 发生归属变化时，相关 jobs 应暂停并重新审批；
- Node/Collector 被卸载后，历史 facts 是否保留由 EnvironmentProfile retention policy 决定；
- 外部控制面只负责实例编排、注册、升级、暂停、撤销和发布；
- 不实现跨环境共享 Cache、TaskRegistry、Event Bus、AuthManager 或 EnvironmentStore 查询。

`tenant_id` 在本设计中是外部部署边界/企业标识，用于审计、编排和导出；实例内部真正的授权层级是：

```text
environment → site → zone → node → collector/job/asset/service
```

## 22.2 强类型 ID、命名和生命周期

建议定义以下不可混用的 ID：

```text
EnvironmentId
SiteId
ZoneId
NodeId
CollectorId
ConnectorId
ObservationJobId
ObservationRunId
EntityId
RelationshipId
FactId
EvidenceId
ActionPlanId
ApprovalId
```

ID 规则：

- 内部主键使用 UUID 或同等不可预测 ID；
- 对外显示名与内部 ID 分离；
- 名称不能作为授权依据；
- 长度、字符集、控制字符和路径分隔符必须校验；
- 不允许把 token、secret、IP 或 hostname 直接当作安全身份；
- 已删除 ID 不复用；
- 所有跨表引用使用对应强类型 ID；
- 所有外部输入 ID 在进入 store 和 policy 前 canonicalize。

生命周期状态也必须是 typed enum，而不是散落的字符串：

```text
Environment: Draft | Active | Paused | Archived | Uninstalled
Registration: Pending | Approved | Active | Paused | Isolated | Revoked | Uninstalled
Job: Draft | PendingApproval | Approved | Scheduled | Running | Partial | Completed | Failed | TimedOut | Cancelled | Revoked | Expired
Fact: Current | Conflicting | Stale | Expired | Tombstoned | Unverified | Inferred
```

## 22.3 EnvironmentStore 数据库契约

环境事实不能直接塞进 conversation history、普通 Memory 或 turn observability。建议使用独立 `EnvironmentStore`，复用 `src/memory/db.rs` 的 SQLite pool、WAL、foreign key、migration 和 transaction 模式。

建议表：

```text
environment_profiles
sites
zones
nodes
collector_registrations
connector_registrations
capability_grants
observation_scopes
observation_jobs
observation_runs
environment_entities
environment_relationships
environment_facts
fact_evidence
fact_changes
healthcheck_targets
healthcheck_runs
action_plans
action_approvals
action_runs
audit_events
```

### facts 最小字段

```text
id
environment_id
site_id?
zone_id?
node_id?
entity_id
predicate
value_json
value_hash
source_id
source_kind
observed_at
expires_at
confidence
sensitivity
provenance_json
evidence_id
status
version
created_at
updated_at
```

### 数据库约束

- 所有查询都带 environment scope；
- 重要表具有 environment/site/zone/node/status/expiry 索引；
- 外键和事务保证 registration/job/fact 关联一致；
- 增量 migration，不能破坏普通 Syscity 启动；
- 支持 optimistic version 或 CAS，避免并发同步覆盖新事实；
- facts current view 与 immutable evidence 分离；
- 删除环境时必须处理 facts、evidence、changes、edges、jobs、checkpoints、缓存和导出物；
- SecretRef、指纹或 credential metadata 可以持久化，secret 原文不能进入环境数据库。

### current facts 与 evidence

必须保留两层：

1. **Immutable observation/evidence**：来源、collector、job、时间、摘要、hash 和受控 payload reference；
2. **Materialized current facts**：最新有效值、TTL、confidence、source priority 和状态。

不能只保存最新状态，否则无法回答“事实来自哪里”“什么时候变化”“为什么诊断得出这个结论”。

## 22.4 Fact 冲突、来源优先级和过期语义

来源优先级固定为：

```text
权威 CMDB / Cloud API / Kubernetes
  > 认证 Platform Connector
  > Endpoint Sensor
  > 主动 healthcheck
  > LLM hypothesis
```

发生冲突时：

- 不静默覆盖；
- 保存各来源 evidence；
- current fact 标记 `Conflicting`；
- 使用 source priority 生成建议值；
- Agent 输出冲突来源和置信度；
- 低可信推断不能覆盖高可信事实。

过期规则：

- `expires_at` 之后不能当作当前事实使用；
- 过期 facts 可以作为历史证据，但必须明确标记 `Expired/Stale`；
- source 失联时降低 freshness，不应把“没有数据”变成“资源不存在”；
- healthcheck 失败只生成 observation/result，不直接删除权威资产；
- 删除资源使用 tombstone 和 grace period，避免短暂 API 故障导致资产闪删；
- 恢复数据源时使用 source version/cursor 和 deterministic merge 恢复。

## 22.5 Connector 接口和错误契约

所有 Kubernetes/Cloud/CMDB/Observability/DNS/LB Connector 遵守统一接口：

```text
id()
version()
capabilities()
required_credentials()
required_scopes()
health(ctx)
observe(scope, cursor, ctx)
checkpoint()
cancel()
shutdown()
```

统一结果类型：

```text
Success
Partial
RateLimited
Unauthorized
Forbidden
NotFound
Timeout
InvalidData
Truncated
Cancelled
Unavailable
```

每个 Connector 必须明确：

- snapshot / delta / watch 支持情况；
- pagination/cursor 的持久化格式；
- cursor 失效后的恢复方式；
- 空响应与数据删除的区别；
- retryable 与 non-retryable 错误；
- retry-after 和最大重试次数；
- 最大 page/entity/object/bytes；
- 并发和速率限制；
- 是否允许部分提交；
- Connector shutdown 是否真正终止网络请求。

Beta 先实现 `snapshot + cursor`，Kubernetes watch/informer 等增量流在 snapshot 稳定后再加入。

## 22.6 凭据和认证契约

不同来源使用不同的只读认证方式：

| 来源 | 首选认证 |
|---|---|
| Kubernetes | namespace-scoped ServiceAccount / mTLS / OIDC |
| Cloud | read-only IAM / workload identity |
| CMDB | short-lived API token / OAuth |
| Prometheus | mTLS / scoped bearer reference |
| OTel | mTLS / signed ingestion token |
| DNS | provider read-only API key |
| Load Balancer | provider read-only IAM |
| Endpoint Sensor | mTLS / device certificate |

规则：

- SecretRef 只解析给对应 Connector；
- secret 原文不进入 facts、audit、metrics、prompt 或错误消息；
- 内存中保留时间受限；
- token 轮换支持短暂双 token 窗口；
- credential revoke 会停止 Connector jobs；
- Connector credential 必须绑定 environment/zone/resource scope；
- Connector 不能使用一个环境的凭据读取另一个环境；
- Connector 失去凭据时进入可见 `Unavailable/Unauthorized` 状态，不 fallback 到更宽权限。

## 22.7 PolicyEngine 规则优先级

固定决策顺序：

```text
Global deny
  > Registration status
  > Revocation / expiry
  > Environment scope
  > Zone / Node scope
  > Capability grant
  > Resource selector containment
  > Sensitivity ceiling
  > Rate / concurrency / budget
  > Approval requirement
  > Allow
```

必须区分：

```text
method authorization
resource authorization
data authorization
action authorization
```

例如：

- `environment.read` 不代表可以读取所有 node；
- 读取 node facts 不代表可以读取 raw logs；
- Kubernetes namespace read 不代表可以读取 Secret；
- topology query 不代表可以运行 healthcheck；
- 可以生成 ActionPlan 不代表可以执行 Action。

冲突和无法判断时必须 `Deny`，不能使用模型判断作为 fallback。

## 22.8 事件、订阅和重连契约

环境事件分类：

```text
GlobalSafeEvent
EnvironmentEvent
SiteEvent
ZoneEvent
NodeEvent
JobEvent
FactChangeEvent
HealthcheckEvent
ApprovalEvent
AuditEvent
```

每类事件定义：

- audience；
- 必需 scope；
- 是否允许广播；
- 是否需要订阅；
- sequence；
- replay cursor；
- 重连补偿；
- payload sensitivity。

默认规则：

```text
默认 deny
明确 audience
只订阅已授权 scope
重连使用 cursor/revision 恢复
敏感事件不走全局 broadcast
```

事件必须包含：

```text
event_id
sequence
tenant/environment/site/zone/node scope
audience
job_id?
observed_at
schema_version
```

## 22.9 Observation Job 状态机和幂等性

```text
Draft
  → PendingApproval
  → Approved
  → Scheduled
  → Running
  → Partial | Completed | Failed | TimedOut | Cancelled | Revoked | Expired
```

每次运行都使用独立 `run_id`，并带：

- `job_id`；
- idempotency key；
- scope version；
- policy version；
- cursor/checkpoint；
- started/finished time；
- cancellation reason；
- result summary；
- error category。

规则：

- 同一 job 默认不并发；
- 重复触发返回现有运行或明确冲突；
- retry 不得超过原 job budget；
- Gateway 重启后按 checkpoint 恢复或明确标记 interrupted；
- revoke/expiry 会取消未完成运行；
- fact upsert 和 run status 需要定义事务边界；
- partial 结果必须显式标记，不得冒充完整同步。

## 22.10 Sensor/Collector 与 Gateway 通信协议

第一版推荐 Sensor 主动建立到 Gateway 的认证连接：

```text
Sensor → Gateway outbound authenticated connection
```

原因：

- 适合 NAT、分支和受限网络；
- Gateway 不需要主动连接每台主机；
- 便于 heartbeat、revoke、backpressure 和断线恢复；
- 与当前 WS-first 架构兼容。

最低协议帧：

```text
registration_request
registration_approved
heartbeat
capability_report
observation_batch
observation_ack
checkpoint
pause
resume
revoke
uninstall
```

每个 batch 必须包含：

```text
environment_id
node_id
collector_id
run_id
sequence
schema_version
created_at
expires_at
digest
```

协议必须支持：

- 最大 batch bytes；
- 最大 facts/entities/edges；
- ack 和重试；
- replay/idempotency；
- backpressure；
- sequence gap；
- credential rotation；
- clock skew 容忍窗口；
- revoked 后拒绝继续提交；
- 不把 raw secrets 写入 error/ack。

## 22.11 Agent Context 和证据格式

进入 LLM context 的 facts 使用显式证据块，例如：

```text
[FACT]
subject: service:order-api
predicate: available_replicas
value: 2
source: kubernetes:cluster-a
observed_at: 2026-09-16T08:00:00Z
expires_at: 2026-09-16T08:05:00Z
confidence: 0.98
scope: environment/prod/zone-a
evidence_ref: evidence-123
[/FACT]
```

Context Builder 必须：

- 只读取 policy-filtered facts；
- 限制 facts 数量和总 bytes；
- 脱敏 Secret/PII/restricted raw payload；
- 标注 stale/expired/conflict；
- 区分 confirmed fact 和 hypothesis；
- 为诊断结论保留 evidence refs；
- 不把完整企业拓扑一次性塞入 prompt；
- 不把缺失事实当作资源不存在。

## 22.12 数据保留、删除、导出和灾备

需要分别定义：

```text
observation retention
fact retention
topology retention
audit retention
raw evidence retention
```

删除 Environment 或 uninstall Sensor 时，必须处理：

- current facts；
- immutable evidence；
- relationships/edges；
- changes/tombstones；
- observation jobs/runs；
- connector checkpoints；
- local sensor cache；
- exported artifacts；
- search indexes；
- audit records（依法需保留的记录除外）。

还需要定义：

- backup encryption；
- restore authorization；
- legal hold；
- deletion confirmation；
- export format/version；
- restore 后的 scope 验证；
- 备份中 secret/reference 的处理。

## 22.13 生产运维、版本和迁移契约

定义独立版本：

```text
sensor_protocol_version
observation_schema_version
fact_schema_version
policy_version
connector_contract_version
```

需要支持：

- Sensor/Gateway 兼容矩阵；
- Connector 版本 pinning；
- schema migration；
- policy 版本回放；
- staged rollout；
- canary；
- rollback/downgrade；
- certificate rotation；
- offline upgrade；
- 失联节点降级；
- feature flag；
- 版本过期和 deprecation；
- 迁移失败恢复。

新版本不得默默增加 capability；capability 变化需要重新注册或重新审批。

## 22.14 Gamma SafetyClass

Gamma 环境不能只使用 IT 的 `read/write/admin` scope。新增安全等级概念：

```text
SafetyClass
  Informational
  Operational
  SafetyRelevant
  LifeCritical
```

建议默认策略：

```text
Informational
  passive/read-only, normal review

Operational
  passive/read-only, controlled healthcheck

SafetyRelevant
  passive-only by default, special review for active checks

LifeCritical
  no generic Agent action, recommendation only
```

适用范围包括 OT/ICS、医疗设备、车辆/机器人、交通系统、电力/水务/能源、消防/门禁/电梯和卫星 command channel。

## 22.15 编码前最终契约清单

在真正开始写实现代码前，必须完成并评审：

1. EnvironmentStore 完整 schema、索引、迁移和删除策略；
2. Registration/Sensor/Collector state machine；
3. CapabilityGrant 和 ResourceSelector 规则；
4. PolicyEngine 决策优先级和拒绝码；
5. Connector error/pagination/cursor/bytes/timeout contract；
6. ObservationJob/ObservationRun 幂等和恢复语义；
7. Sensor ↔ Gateway batch/ack/replay/backpressure 协议；
8. Event audience/subscription/replay contract；
9. Fact conflict/TTL/tombstone/source precedence 规则；
10. LLM context/evidence/citation/redaction 格式；
11. NetworkTargetPolicy、SSRF、DNS rebinding 和 response limit 设计；
12. credential rotation/revoke/offline 行为；
13. retention/delete/export/backup/restore 方案；
14. sensor/gateway/connector/schema/policy 版本兼容矩阵；
15. Gamma SafetyClass 和行业安全审查边界；
16. Beta E2E/security matrix 和 operator runbook。

## 22.16 最终判断

`environment-agent.local.md` 当前已经具备完整的产品目标、架构方向、阶段路线和 Beta/Gamma 边界；本节补齐了编码前必须明确的接口级、状态机级、数据库级、策略级、协议级和生产运维级设计。

当前仍然**没有开始实现 Environment Agent**。代码仓库中尚不存在完整的 `src/environment/`、EnvironmentStore、PolicyEngine、Registration Registry、Topology Store、企业 Connector 或 Network Zone Collector。后续如果开始编码，必须从 M0 安全控制面和 M1 领域契约开始，而不是直接实现扫描逻辑。

---

# 23. 与现有 Syscity 实现的整合设计

本节回答：`environment-agent.local.md` 的目标能否契合当前 Syscity，以及如何在不重写现有 Runtime 的情况下整合。

## 23.1 总体判断

可以整合，而且契合度较高。但整合方式不是“增加几个网络工具”，也不是把当前所有工具直接开放给 Agent，而是：

```text
现有 Syscity Runtime
  Gateway / Agent / Tools / Security / Memory / Observe / Cron / Secrets
                         +
              Environment Domain
```

推荐目标架构：

```text
Clients: Web / TUI / CLI / Desktop / Channels
                         │
                         ▼
Existing Gateway / WS Control Plane
 auth · RequestContext · scopes · rate limit · audit · events
                         │
                         ▼
Environment Control Domain
 registration · grants · policy · jobs · approvals · revoke
                         │
              ┌──────────┼──────────┐
              ▼          ▼          ▼
       Endpoint Sensor  Zone      Platform
       host facts       Collector  Connectors
                         │          │
                         └────┬─────┘
                              ▼
                 Normalize / Redact / Validate
                              │
                              ▼
               EnvironmentStore / Facts / Topology
                              │
                  policy-filtered retrieval
                              ▼
                 Existing Agent Runtime
             explain · diagnose · recommend
                              │
                 later, approved only
                              ▼
                  Bounded Action Executor
```

当前仓库已经具备大量可复用基础，但以下核心组件目前不存在：

- `src/environment/`；
- EnvironmentStore；
- EnvironmentPolicyEngine；
- Registration Registry；
- Environment Facts/Topology Store；
- 企业 Kubernetes/Cloud/CMDB/Observability Connector；
- Network Zone Collector；
- 企业网络专用 E2E 和安全测试矩阵。

因此，这是一个**高复用、需要新增受控领域层**的演进，不是改几个配置就完成的功能。

## 23.2 现有模块复用矩阵

| Environment Agent 需求 | 现有 Syscity 基础 | 整合方式 |
|---|---|---|
| WS 控制面 | `src/gateway/ws/`、`protocol.rs`、`core.rs` | 新增 environment handlers、dispatch、scope 和对象授权 |
| 身份与 session | `src/security/`、`AuthManager`、`AuthStore` | 扩展 registration/node/environment 维度 |
| 设备注册 | `src/security/device_pairing.rs` | 复用 pending/approve/revoke 模式，补证书、grant、heartbeat 和 environment binding |
| Request context | `src/security/request_context.rs` | 扩展 environment/site/zone/node/job/audience/policy version |
| 审计 | `runtime_audit.rs`、`persistent_audit.rs` | 增加 scope、job、registration、policy、correlation 维度和脱敏 |
| 工具授权 | `src/tools/rbac.rs`、`ToolPolicy` | 叠加 EnvironmentPolicyEngine 和 resource selector |
| 人工审批 | `src/tools/approval.rs`、`ApprovalQueue` | Beta 只读；后续为 ActionPlan 增加 durable exact-args approval |
| 主机信息 | `src/computer/system.rs` | 作为 HostCollector 的 typed snapshot 来源 |
| 网络信息 | `src/computer/network.rs` | 作为低层 adapter，不能直接暴露任意目标 |
| 平台能力 | `src/computer/platform/`、registry | 新增独立 `environment-observer` read-only profile |
| 调度任务 | `src/cron/`、`src/heartbeat/` | 复用 timer/cancel/persistence 模式，新增 scoped ObservationJobManager |
| 任务取消 | `TaskRegistry`、`CancellationToken` | 统一应用到 collector/connector/job/healthcheck |
| 观测记录 | `src/observe/` | 复用写入/保留/指标模式，不把 turn record 当 facts store |
| SQLite | `src/memory/db.rs`、session schema | 新增独立 EnvironmentStore tables/migrations |
| Connector 生命周期 | `src/mcp/connectors/` | 复用 manifest/catalog/lifecycle/state，补 scope/sensitivity/read-write metadata |
| Cloud 基础 | `src/cloud/` | 复用 client/session 风格，新增 provider inventory adapters |
| Secrets | `src/secrets/` | Connector 使用 SecretRef，secret 原文不进入 facts/prompt/audit |
| Agent 推理 | `src/agent/`、`model_router/` | 只接收 policy-filtered facts，输出 explain/diagnose/recommend |
| UI | Web/TUI/CLI/Desktop | 增加 environment tree、facts、jobs、health、audit、registration |

## 23.3 新增 Environment Domain

建议新增：

```text
src/environment/
├── mod.rs
├── ids.rs
├── model.rs
├── identity.rs
├── registration.rs
├── capabilities.rs
├── scope.rs
├── policy.rs
├── observation.rs
├── jobs.rs
├── facts.rs
├── topology.rs
├── redaction.rs
├── store.rs
├── audit.rs
├── healthcheck.rs
├── collectors/
│   ├── mod.rs
│   ├── host.rs
│   ├── network_local.rs
│   ├── services.rs
│   └── filesystem.rs
└── connectors/
    ├── mod.rs
    ├── kubernetes.rs
    ├── cloud_inventory.rs
    ├── cmdb.rs
    ├── observability.rs
    ├── dns_tls.rs
    └── load_balancer.rs
```

不要把环境逻辑直接塞入 `src/computer/`、`src/tools/`、`src/memory/`、`src/agent/` 或 `src/gateway/mod.rs`；这些模块继续提供底层能力，Environment Domain 负责身份、范围、事实、拓扑、任务和生命周期。

## 23.4 不应直接复用的能力

### 不能直接继承 full/root capability

新增 `environment-observer` profile，默认只包含：

```text
host.identity.read
host.os.read
host.hardware.read
host.network.interface.read
host.network.route.read
host.services.read
host.processes.read
topology.query.read
facts.query.read
```

默认不包含：

```text
arbitrary.shell
filesystem.read:/
packet.capture
desktop.control
service.restart
firewall.write
dns.write
credential.read
```

不能只依赖 `mark_privileged()`；必须使用稳定 capability ID 和 EnvironmentPolicyEngine。

### 不能直接暴露 network_diag

`src/computer/platform/linux/network_diag.rs` 是底层诊断能力，不是企业网络权限系统。正确调用路径：

```text
RegisteredHealthcheckTarget
  → EnvironmentPolicyEngine
  → NetworkTargetPolicy
  → deadline/rate/concurrency budget
  → bounded adapter
  → normalized HealthcheckResult
  → audit
```

不能让 LLM 直接传入任意 target/action。

### 不能直接使用 dreaming graph 作为拓扑库

`src/memory/dreaming/knowledge_graph.rs` 可以参考图算法，但 EnvironmentTopologyStore 还必须增加 source identity、environment scope、provenance、TTL、conflict、tombstone、incremental sync 和 query API。

## 23.5 企业网络整合流

### N0：被动资产

```text
CMDB/IPAM/DNS/Cloud/Kubernetes/Prometheus/OTel
  → passive adapters
  → normalize/redact
  → EnvironmentStore
```

### N1：登记服务健康检查

```text
RegisteredHealthcheckTarget
  → policy and target containment
  → TCP/HTTP/HTTPS/DNS/TLS adapter
  → bounded result
  → facts/health history/audit
```

### N2：授权区域测量

```text
SignedObservationScope
  → approval
  → rate/concurrency/deadline/impact budget
  → ZoneCollector
  → partial/truncated result
  → audit and retention
```

N2 不作为 Beta 默认能力。

### N3：生产动作

```text
ActionPlan
  → exact approval
  → snapshot
  → bounded executor
  → verify
  → rollback/compensation
  → audit
```

N3 不属于第一版 Environment Agent。

## 23.6 Agent 使用环境的服务层

环境感知整合到现有 Agent 后，提供：

1. **Environment Search**：查询 policy-filtered 的资产、节点、服务、依赖、zone、health、变化和 facts 来源；
2. **Environment Explain**：解释拓扑、依赖、状态、版本变化、资源分布、影响范围、过期和冲突 facts；
3. **Environment Diagnose**：结合 facts、metrics、events、topology、change history 和 healthcheck，输出证据驱动的故障假设；
4. **Environment Recommend**：生成观察、维护、扩容、修复、回滚和迁移计划，但不执行；
5. **Environment Operate**：经过 PolicyEngine、精确审批、snapshot、verification、rollback 和 kill switch 后，才允许有限动作。

## 23.7 整合后的数据和控制边界

```text
User request
  → Existing Agent planner
  → policy-filtered environment query
  → Evidence Context Builder
  → Explain / Diagnose / Recommend
  → ActionPlan (optional)
  → human approval (later phase)
  → bounded executor (later phase)
  → verification
  → EnvironmentStore + AuditStore
```

Agent 不能直接：

- 从 Connector 读取任意 JSON；
- 访问未授权 node/zone；
- 修改 ObservationScope；
- 创建 capability grant；
- 读取 Secret；
- 自动扩大 CIDR；
- 绕过审批；
- 修改自身审计、撤销或卸载策略。

## 23.8 当前整合成熟度

| 目标 | 当前判断 |
|---|---|
| 产品目标 | 已定义 |
| 总体架构 | 已定义 |
| 单主机 typed collector | 基础能力存在，环境域未实现 |
| Registration/Capability/Policy | 基础零件存在，统一环境模型未实现 |
| EnvironmentStore | 尚未实现 |
| Facts/Topology | 有 Memory/Graph 参考，环境事实库尚未实现 |
| ObservationJob | Cron/TaskRegistry 可复用，环境任务未实现 |
| Kubernetes Connector | 设计已明确，代码尚未实现 |
| Cloud inventory Connector | Cloud 基础存在，资产适配器尚未实现 |
| CMDB/IPAM/DNS Connector | 尚未实现 |
| Prometheus/OTel ingestion | 尚未实现 |
| Network Zone Collector | 尚未实现 |
| Healthcheck policy | 低层网络原语存在，企业 policy/target model 尚未实现 |
| Diagnostic Agent | Agent runtime 可复用，environment context/query 尚未实现 |
| E2E/security audit | 测试路线已定义，企业环境测试尚未实现 |
| Gamma environments | 分类和安全边界已定义，Connector 尚未实现 |
| Shared multi-tenancy | 明确排除 |

准确结论：

> **environment-agent.local.md 的目标可以契合现有 Syscity，并且已经有完整的整合路线；但当前代码完成度仍然很低。现有代码提供的是 Gateway、Agent、Tools、Security、Observe、Cron、Secrets 和 Connector 生命周期基础，不是已经完成的 Environment Agent。**

后续真正编码时，应从 M0 控制面安全和 M1 EnvironmentStore/领域契约开始，而不是先写网络探测循环。

---

# 24. 持续环境探测：低干扰、非阻塞和可恢复设计

本节回答两个运行时问题：

1. Syscity 如何持续了解环境变化，而不把目标环境压垮或干扰业务；
2. 环境观测任务如何运行，而不阻塞 Agent 对话、Gateway、TUI 或其他任务。

核心原则：

> **持续探测不是持续扫描。默认使用被动事件、增量同步和已登记目标的低影响检查；所有主动观察都必须有预算、截止时间、取消路径和降级策略。**

## 24.1 持续探测的优先级

按照对环境的干扰程度，从低到高分层：

```text
P0  被动事件和已有指标
    CMDB change feed / Kubernetes event / Prometheus / OTel / heartbeat

P1  增量 API 同步
    updated_since / ETag / cursor / resourceVersion / checkpoint

P2  已登记对象的低影响健康检查
    fixed TCP / HTTP health path / DNS / TLS metadata

P3  明确批准的有限主动测量
    固定目标 / 维护窗口 / 最大速率 / 最大并发 / kill switch

P4  生产变更或高风险验证
    不属于初始 Environment Agent，必须独立变更流程
```

默认只运行 P0/P1；P2 只访问已登记的目标；P3 需要明确批准；P4 默认关闭。

## 24.2 观测调度器与 Agent 解耦

环境观测不能在用户发送消息的 Agent turn 中同步执行。推荐使用独立的 `ObservationJobManager`：

```text
User chat / Agent turn
        │
        │ submits query or observation request
        ▼
EnvironmentJobManager
        │
        ├── scheduler queue
        ├── policy check
        ├── per-source worker
        ├── bounded result channel
        └── EnvironmentStore
                         │
                         ▼
                 Agent query facade
```

Agent 只执行：

- 提交一个已授权的 observation job；
- 查询 job 状态；
- 查询已经落库且经过 policy 过滤的 facts；
- 解释结果或生成下一步建议。

Agent 不应持有 collector 的长时间 Future，也不应在一次对话请求中等待整个企业同步完成。

### 状态返回

启动任务后立即返回：

```json
{
  "job_id": "job-123",
  "status": "accepted",
  "scope": "environment/prod/zone-a",
  "estimated": {
    "max_targets": 42,
    "deadline_seconds": 30
  }
}
```

随后通过：

- `environment.observation.status`；
- `environment.observation.events`；
- `environment.facts.query`；
- `environment.changes.list`

异步取得结果。这样 Gateway、Agent loop、TUI 和 Web UI 都不会被长时间同步探测阻塞。

## 24.3 分层调度队列

建议使用优先级队列，而不是一个无界任务队列：

```text
Priority 0: revoke / pause / kill-switch
Priority 1: heartbeat / security status / active incident health
Priority 2: user-approved one-shot observation
Priority 3: scheduled incremental sync
Priority 4: background enrichment / low-priority topology refresh
```

规则：

- 高优先级任务不能无限挤压低优先级任务；
- 低优先级同步可以被暂停；
- 同一 scope 默认禁止重叠运行；
- 同一 source 使用独立队列和预算；
- 队列长度有硬上限；
- 队列满时拒绝、合并或延迟任务，而不是无限堆积；
- revoke/pause/kill-switch 可以抢占并取消普通观测。

现有 `src/cron/cron/scheduler.rs` 可复用定时和取消模式，`src/gateway/task_registry.rs` 用于注册长期任务；但环境任务需要额外的 scope、budget、priority 和 checkpoint。

## 24.4 频率、抖动和增量同步

不要让所有节点在整点同时探测。每个任务应使用：

```text
next_run = base_interval + deterministic_jitter(node_id, source_id)
```

推荐行为：

- heartbeat：短周期、轻量、固定上限；
- host inventory：分钟级到小时级，依据变化频率调整；
- CMDB/cloud：使用 change token 或 `updated_since`；
- Kubernetes：先 snapshot/cursor，稳定后再 watch；
- Prometheus：短时间窗口、固定 matcher、受限 sample；
- DNS/TLS：按 TTL 或证书临近过期时间调度；
- 健康检查：仅对已登记目标，按服务重要性安排；
- 低变化资源使用更长间隔；
- 高频变化源使用事件/增量而非重复全量列表。

### 自适应频率

频率可以依据确定性规则调整：

```text
最近发生变化       → 临时提高频率
连续稳定            → 逐步降低频率
连续失败            → 指数退避
源被限流            → 尊重 retry-after
环境处于维护窗口    → 暂停非关键任务
collector 资源紧张  → 降低采集量
```

LLM 可以提出“建议提高频率”，但不能自行改变 job 的最大速率、scope 或权限；策略变化必须经过 PolicyEngine。

## 24.5 资源预算和背压

每一层都需要硬预算：

### Job 预算

```text
max_duration
max_targets
max_requests
max_bytes
max_entities
max_relationships
max_retries
```

### Source 预算

```text
max_concurrency
requests_per_second
max_page_size
max_response_bytes
max_cursor_pages
```

### Environment 预算

```text
max_running_jobs
max_network_requests
max_connector_sessions
max_fact_write_rate
max_storage_growth
```

### Sensor 预算

```text
max_cpu_percent
max_memory_bytes
max_disk_bytes
max_upload_bytes
max_local_queue
```

当预算耗尽时，采集器必须返回：

```text
status = truncated
reason = budget_exhausted
coverage = partial
```

不能静默丢弃，也不能将部分结果标记为完整结果。

### 背压策略

```text
collector → bounded channel → batch writer → store
```

- channel 必须有界；
- writer 慢时 collector 降速或暂停；
- 单个 source 不能阻塞所有 source；
- batch 写入失败时使用有限重试和磁盘 checkpoint；
- 队列达到上限时合并相同 job、丢弃低优先级重复刷新或显式报告 dropped；
- 不使用无界 channel 存储原始企业网络数据。

## 24.6 并发模型：不阻塞 Agent 和 Gateway

推荐分层：

```text
Gateway / WS event loop
  └── Job submit/status only

ObservationJobManager
  └── bounded JoinSet / semaphore

Source worker
  └── source-local concurrency limit

Target check
  └── per-target timeout + cancellation

Store writer
  └── bounded batch transaction
```

约束：

- Gateway handler 只做快速验证和入队；
- 不在 `dispatch_method()` 内等待完整同步；
- 不在 Agent `process_message()` 内直接执行企业扫描；
- 不持有 `RwLock`/`Mutex` 跨网络 await；
- 每个 source 通过 semaphore 隔离；
- 每个 target 有独立 deadline；
- 所有 Future 都能接收 cancellation；
- 长期 loop 注册到 `TaskRegistry`；
- Gateway shutdown 时先停止新 job，再取消运行 job，最后 flush store。

## 24.7 取消、撤销和失联

### 用户取消

```text
environment.observation.cancel(job_id)
  → mark cancellation requested
  → cancel token
  → stop new target work
  → await bounded drain
  → persist Cancelled run
  → audit
```

### Registration revoke

```text
revoke registration
  → invalidate credential
  → increment revocation generation
  → cancel node jobs
  → stop connector calls
  → reject late observation batches
  → mark node facts stale
  → emit audit/event
```

### Collector 失联

```text
heartbeat timeout
  → mark collector stale
  → do not delete facts immediately
  → mark freshness uncertainty
  → stop scheduling active checks from that collector
  → alert operator
```

失联不等于资源消失；资源事实应进入 `Stale`，而不是立即变成 `Removed`。

## 24.8 低干扰网络健康检查

健康检查只使用已登记的 `HealthcheckTarget`，不从用户自然语言直接解析任意目标。

### 目标限制

- hostname/IP 必须属于已批准 environment/zone；
- port 必须在 target allowlist；
- protocol 必须是固定集合；
- HTTP path 必须预先登记；
- redirect 每跳重新校验；
- 禁止 loopback、link-local、metadata endpoint 和未授权 private range；
- DNS 解析失败默认 deny；
- DNS 结果必须重新校验；
- 不保存 request body、authorization header 或响应原文。

### 减少干扰

- TCP 只进行一次短连接或配置的最小重试；
- HTTP 只请求固定 health endpoint；
- 不下载完整业务页面；
- 响应体有 streaming size cap；
- 使用 HEAD 只有在服务正确支持时才采用，否则使用固定 GET；
- ping 默认关闭，除非目标明确允许 ICMP；
- 不运行 traceroute 作为默认健康检查；
- 每个 zone 有并发和速率上限；
- 维护窗口外只执行低成本检查；
- 对业务关键目标提供 cooldown，避免连续重复请求。

## 24.9 被动观察优先

优先级应为：

```text
已有事件/指标/CMDB change feed
  > 增量 API
  > Endpoint heartbeat
  > 已登记 healthcheck
  > 明确批准的主动测量
```

具体方式：

- Kubernetes：事件、snapshot cursor，稳定后才 watch；
- CMDB：`updated_since`、change feed、ETag；
- Cloud：provider inventory pagination 和 event/change API；
- Prometheus：显式 matcher 和 bounded time window；
- OTel：资源属性 allowlist 和 bounded batch；
- DNS：指定 zone/name 与 TTL；
- Load Balancer：provider reported backend health；
- Endpoint：heartbeat 和本地 typed snapshots。

目标是持续更新事实，不是反复重新扫描全部资产。

## 24.10 线程、进程和部署隔离

为避免环境探测影响 Syscity 本身和业务环境，建议：

### 进程内第一版

- ObservationJobManager 独立任务组；
- 单独 semaphore 和 bounded channels；
- 独立 store writer；
- 明确 CPU/memory/bytes budget；
- 不阻塞 Agent/Gateway event loop；
- 所有任务有 cancellation token。

### 生产部署

- Sensor/Zone Collector 作为独立受管进程或容器；
- 与 Gateway 使用 mTLS/短期凭据；
- 独立 filesystem/workspace；
- 独立 connector credentials；
- egress firewall；
- systemd/container resource quota；
- collector 崩溃不影响 Gateway 和 Agent；
- Gateway 崩溃后 collector 使用有界本地队列，不能无限缓存。

## 24.11 Agent 非阻塞交互

用户发起环境查询时，Agent 采用异步任务：

```text
用户问题
  → 生成只读查询计划
  → PolicyEngine 检查
  → 提交 observation job
  → 立即返回 job_id
  → 通过 event/status 观察进展
  → 分批查询 facts
  → 生成诊断结果
```

UI 显示：

- queued；
- running；
- current source；
- current page/entity count；
- partial/truncated；
- retry/backoff；
- cancelled；
- completed；
- failed。

Agent 可以在 job 运行期间继续处理其他对话，但任何新的诊断结论都必须标记所使用的 facts revision 和 freshness。

## 24.12 持续探测的正确性指标

应监控：

```text
environment_job_queue_depth
environment_job_running
environment_job_cancelled_total
environment_job_timeout_total
environment_job_truncated_total
environment_source_sync_duration_seconds
environment_source_sync_error_total
environment_source_rate_limited_total
environment_collector_heartbeat_age_seconds
environment_fact_freshness_seconds
environment_fact_stale_total
environment_fact_conflict_total
environment_healthcheck_latency_seconds
environment_healthcheck_timeout_total
environment_policy_denied_total
environment_audit_write_failed_total
```

标签必须有界：

- 允许固定 `source_kind`、`result_class`、`reason_class`、`environment_kind`；
- 不允许 hostname、raw IP、CIDR、URL、path、resource ID、query 或 secret 作为高基数 label。

## 24.13 持续探测的失败降级策略

| 故障 | 行为 |
|---|---|
| Source API timeout | 记录失败，有限退避，不阻塞其他 source |
| Source API 429 | 尊重 retry-after，暂停该 source |
| Connector credential 失效 | 标记 Unauthorized，停止 source jobs，通知 operator |
| Collector 失联 | 标记 Stale，保留旧事实，不宣称资源消失 |
| Store 写入失败 | 停止扩大采集，保留 bounded checkpoint，发出 high-severity alert |
| 队列满 | 拒绝/合并低优先级 job，不能无限增长 |
| Policy unavailable | fail-closed，不能 fallback 到允许 |
| Audit unavailable | 对合规要求的观察/动作 fail-closed；普通健康信息可降级但必须显式报警 |
| Gateway 断线 | collector 有界缓存和 backoff，不能无限上传重试 |
| 单个 target 异常 | 标记 partial，继续其他 target，不重试无限次 |
| 结果超出限制 | 标记 truncated，保存 coverage gap |

## 24.14 最终运行时模型

```text
被动事件 / 增量同步
  → bounded scheduler
  → per-source semaphore
  → policy check
  → typed collector
  → per-target deadline/cancellation
  → bounded ObservationBatch
  → redaction/normalization
  → transactional EnvironmentStore
  → fact diff/topology update
  → audit/metrics/event
  → policy-filtered Agent context
```

该模型同时满足：

- 持续更新环境事实；
- 不把所有任务堆在 Agent turn 中；
- 不让单一 source 阻塞整个系统；
- 不因网络异常无限重试；
- 不因断线无限积压；
- 不因发现新目标自动扩大范围；
- 不让 Agent 自行获得更高权限；
- 能暂停、撤销、回滚和卸载。 

## 24.15 验收标准

在实现持续探测前，必须能通过以下测试：

1. 慢速 Connector 不阻塞聊天、Gateway 和其他 source；
2. 一个 source 超时不会拖垮其他 source；
3. queue/channel 有界且压力过大时有明确降级；
4. 同一 scope 的 job 不重复并行；
5. 取消在 bounded deadline 内终止网络和子进程工作；
6. revoke/pause/isolate 后不能产生新的 observation；
7. Gateway 重启后 job 能恢复、取消或明确标记 interrupted；
8. partial/truncated 结果不会伪装成完整 inventory；
9. facts stale 不会被诊断 Agent 当成当前事实；
10. PolicyEngine 不可用时 fail-closed；
11. audit 记录允许/拒绝/取消/超时/截断/撤销；
12. 真实外部依赖使用 mock fixture，不对未授权企业或公网执行测试。

---

# 25. 完整目标的优化版实施蓝图

前面的章节分别从产品、Beta、企业网络、Gamma、Agent 服务和技术契约描述目标。本节将它们收敛成一条**唯一推荐实施路径**，用于后续编码、评审、发布和验收。后续实现应以本节为主索引；前文是背景和专题设计，不应再形成第二套互相冲突的路线。

## 25.1 完整目标的定义

完整目标不是“扫描所有网络”，而是建立一个单环境、受控、持续更新的环境智能闭环：

```text
显式安装
  → 注册身份
  → 管理员授权能力和范围
  → 被动/增量观察
  → 统一事实和拓扑模型
  → 低影响健康检查
  → 差异和诊断
  → Agent 解释和建议
  → 精确审批
  → 有界动作
  → 验证/回滚
  → 审计/指标
  → 撤销/暂停/卸载
```

完整目标包含六个交付面：

```text
A. Control Plane      注册、授权、策略、审批、撤销
B. Collection Plane   Endpoint、Zone、Platform Connector
C. Knowledge Plane    Facts、Evidence、Topology、Changes
D. Agent Plane        Search、Explain、Diagnose、Recommend
E. Action Plane       受审批的 Snapshot、Act、Verify、Rollback
F. Operations Plane   部署、升级、指标、保留、备份、恢复
```

**A–D 是只读 Environment Agent 的核心；E 必须后置；F 是生产化必需。**

## 25.2 不变的范围决策

以下决策覆盖全部实现阶段：

1. **不实现共享多租户内核**：一个 Syscity 实例/进程/容器服务一个受控 EnvironmentProfile；
2. **默认只读**：环境事实、Connector、健康检查和 Agent 工具默认不产生写操作；
3. **不允许无边界探测**：所有目标来自已批准的 environment/site/zone/node/service scope；
4. **不允许模型授权自己**：LLM 只能请求观察或提出建议，不能创建 grant、扩大 CIDR、修改 policy；
5. **不把环境事实放进普通 transcript**：EnvironmentStore 独立保存 facts/evidence/topology；
6. **不把任意 Shell 当 Sensor API**：优先 typed collector/API Connector；Shell 只能是独立审批的后置 fallback；
7. **不把 Gamma 控制域当普通 IT**：OT、医疗、车辆、关键基础设施和卫星系统默认 passive-only；
8. **不以真实企业/公网做自动化测试**：所有测试使用 mock、fixture 和明确授权的试点；
9. **不以发现结果自动扩大范围**：新资产只能生成待授权建议；
10. **共享多租户不是未来 Beta 的隐式任务**：如未来需要，另开架构项目。

## 25.3 唯一推荐里程碑

### G0：安全闸门和现有控制面修复

**目标**：保证新增环境能力不会放大现有 Gateway 和权限问题。

必须完成：

- requested scopes 与服务端 grants 求交集；
- 修复错误的 method scope；
- session/chat/artifact/approval/environment 对象授权；
- audience-filtered events；
- WS connection/message rate limit；
- Webhook fail-closed；
- query token/secret 日志脱敏；
- RequestContext 支持 environment/site/zone/node/job/audience/policy version；
- revoke/pause/isolate 取消任务；
- audit 具备目标、scope、policy、approval、correlation 维度。

**退出条件**：双主体、双环境、双 zone 的越权测试全部失败闭环；未通过则不得开启 G1。

### G1：领域契约和 EnvironmentStore

**目标**：先冻结数据和策略接口，再写采集器。

交付：

- 强类型 IDs；
- EnvironmentProfile/Site/Zone/Node/Collector/Connector；
- CapabilityGrant/ResourceSelector；
- ObservationScope/Job/Run；
- Fact/Evidence/Entity/Relationship/Change；
- EnvironmentStore SQLite schema、migration、索引、CAS；
- Fact TTL/conflict/source precedence/tombstone；
- PolicyEngine `Allow/Deny/NeedsApproval`；
- redaction/sensitivity contract；
- Event audience/sequence/replay contract。

**退出条件**：schema round-trip、migration、scope containment、policy precedence、fact diff、TTL、redaction 和 deletion/export 测试通过。

### G2：注册、能力和生命周期

**目标**：让环境节点成为可见、可授权、可撤销的受管实例。

交付：

- registration pending/approved/active/paused/isolated/revoked/uninstalled；
- 本地 key/certificate 和短期凭据；
- capability manifest；
- heartbeat/last-seen/stale；
- credential rotation；
- pause/isolate/revoke/kill switch；
- CLI 和 WS registration/status/capability APIs；
- feature flag 默认关闭；
- 可见 install/status/uninstall。

**退出条件**：注册、批准、心跳、失联、撤销、隔离、卸载在重启后状态一致，旧凭据不可继续提交。

### G3：单主机 L0/L1 只读 Sensor

**目标**：完成第一个可交付垂直切片。

交付：

- Host/OS/hardware/resource collector；
- local network interface/route/service/process collector；
- 明确目录 metadata collector；
- typed ObservationBatch；
- normalize/redact/evidence hash；
- facts upsert 和 source diff；
- observation job/scheduler/cancel；
- environment facts WS query；
- TUI/CLI/Web 基础状态和 facts 展示。

初版禁止全盘、任意 Shell、packet capture、未知目标网络访问和写操作。

**退出条件**：一个干净环境实例完成 `install → register → approve → observe → persist → query → revoke → uninstall`，且所有事实带来源和时间。

### G4：被动企业资产和多节点环境树

**目标**：不扫描网络也能理解企业环境结构。

首批 source：

1. Endpoint heartbeat/facts；
2. CMDB/IPAM；
3. DNS provider/API；
4. Cloud inventory；
5. Kubernetes snapshot；
6. Prometheus/OTel metadata；
7. Service Registry/Load Balancer metadata。

交付：

- Environment → Site → Zone → Node → Asset → Service tree；
- source-qualified identity；
- parent/child and relationship merge；
- owner/status/stale；
- source priority/conflict；
- snapshot/diff/change；
- paginated WS query；
- facts/topology export/delete/retention。

**退出条件**：多源 mock 数据能幂等合并、冲突可见、来源可追溯、节点 stale/revoke 正确传播。

### G5：只读 Platform Connectors

**目标**：从外部权威系统补全环境模型。

Connector 顺序：

1. Kubernetes namespace/kind read-only；
2. Prometheus/OTel bounded metadata and metrics；
3. CMDB/IPAM；
4. Cloud account/resource-group inventory；
5. DNS/TLS；
6. Load Balancer/Service Registry；
7. Git/CI/CD/change/ticket metadata。

统一要求：

- credential isolation；
- resource allowlist；
- pagination/cursor；
- max page/entity/bytes；
- timeout/retry/circuit breaker；
- idempotent upsert；
- normalized facts；
- source health；
- audit。

**退出条件**：每个 Connector 有 mock contract、错误和 cursor 测试，不能返回 secret 或绕过 PolicyEngine。

### G6：低影响网络 Healthcheck

**目标**：验证已登记服务是否健康，不进行无界扫描。

只允许：

- fixed TCP target；
- fixed HTTP/HTTPS health path；
- DNS；
- TLS metadata；
- provider/Kubernetes reported health。

必须有：

- RegisteredHealthcheckTarget；
- NetworkTargetPolicy；
- SSRF/DNS rebinding/metadata 防护；
- rate/concurrency/deadline/response-size；
- cancellation/kill switch；
- redirect 每跳重新校验；
- audit 和 bounded result。

**退出条件**：fake targets 的 allow/deny/timeout/cancel/redirect/oversize/partial 测试通过；无目标范围外请求。

### G7：Evidence-aware Diagnostic Agent

**目标**：让 Agent 使用环境，而不是直接控制环境。

只读工具：

```text
environment.fact_read
environment.topology_read
environment.snapshot_compare
environment.metric_query
environment.diagnose
environment.healthcheck_status
```

所有输入经过 Context Builder：

- scope filter；
- freshness/expiry filter；
- sensitivity/redaction；
- evidence refs；
- confidence；
- coverage gaps；
- context size limit。

输出必须区分：

```text
confirmed facts
hypotheses
unknowns
coverage gaps
next read-only checks
recommendations
```

**退出条件**：诊断结果能回指 facts/evidence；证据不足时显式表达不确定；没有 write/scan-expansion tools。

### G8：受审批 Action（Beta 后）

**目标**：只开放少数可验证、可回滚操作。

必须具备：

- immutable ActionPlan；
- exact action hash；
- target scope；
- approval authority；
- change/ticket ID；
- maintenance window；
- pre-snapshot；
- idempotency key；
- timeout/kill switch；
- post-check；
- rollback/compensation；
- durable audit。

初始仅考虑服务重启、明确配置 reload、受控临时文件清理、已有部署回滚等低风险动作。Gamma 和关键基础设施的写操作继续禁用。

### G9：生产运维和 Gamma 扩展

交付：

- signed package/visible service；
- staged rollout/canary；
- certificate rotation；
- offline/reconnect；
- backup/restore；
- schema/policy/protocol compatibility；
- metrics/health/readiness/alerting；
- operator/incident/revoke/uninstall runbook；
- Gamma SafetyClass 和行业专用审查。

OT、医疗、交通、电力、水务、车辆、机器人、卫星等环境默认 passive-only，不能由通用 Agent 直接控制。

## 25.4 完整目标的定义完成条件

只有以下条件全部满足，才称为“完整目标实现”：

### Control Plane

- 注册、授权、撤销、隔离、卸载完整；
- CapabilityGrant 和 ResourceSelector 生效；
- PolicyEngine 对 method/resource/data/action 全部生效；
- 事件按 audience 隔离；
- job 和 action 可取消；
- audit 完整、脱敏、可查询。

### Collection Plane

- Endpoint Sensor、Zone Collector、Platform Connector 统一遵守 Collector/Connector contract；
- N0/N1 稳定；
- N2 有独立批准、预算和 kill switch；
- 所有结果可追溯、可过期、可重放；
- 所有任务有 bounded concurrency/backpressure/deadline。

### Knowledge Plane

- Facts、Evidence、Entities、Relationships、Changes 独立持久化；
- TTL、conflict、source precedence、tombstone 可用；
- 多节点环境树和拓扑查询稳定；
- facts export/delete/retention/backup/restore 可用；
- Agent 不依赖未授权或过期事实。

### Agent Plane

- Search、Explain、Diagnose、Recommend 只读且 evidence-aware；
- 诊断输出有来源、时间、置信度和覆盖缺口；
- LLM 不能授予自己权限或扩大 scope；
- ActionPlan 与执行动作严格分离。

### Action Plane

- 所有写操作需精确审批；
- action hash、snapshot、verification、rollback、kill switch 完整；
- 失败和撤销行为可恢复；
- Gamma/关键基础设施写操作仍需独立专业控制。

### Operations Plane

- 资源预算和指标；
- health/readiness；
- staged rollout；
- 版本兼容和迁移；
- backup/restore；
- 证书轮换；
- incident/revoke/uninstall runbook；
- E2E/security audit 全部通过。

## 25.5 当前可行性结论

当前状态应准确描述为：

```text
产品目标：已明确
架构路线：已明确
安全边界：已明确
实现契约：已基本明确
现有可复用基础：充足
Environment Agent 代码：尚未实现
企业 Connector：尚未实现
Topology/Facts Store：尚未实现
Network Zone Collector：尚未实现
完整 E2E：尚未实现
```

因此，`environment-agent.local.md` 已经具备**分阶段开始实现的可行性**，但不能理解为“完整功能已经实现”或“现在可以直接部署到整个企业网络”。真正的起点是 G0/G1；真正的 Beta 是 G4–G7；G8/G9 必须经过额外安全和生产评审。

后续实现必须遵循：

```text
先控制面
  → 再事实和存储
  → 再单主机只读
  → 再被动企业数据
  → 再多节点拓扑
  → 再低影响 healthcheck
  → 再诊断 Agent
  → 最后才考虑受审批动作
```

共享多租户、自传播、无边界扫描、模型自主扩权和未审批生产变更始终不在本实施范围内。

---

# 26. 复用现有工具：不要重复建造环境感知基础设施

## 26.1 核心判断

企业现实中已经存在大量成熟的资产、拓扑、监控、CMDB、云平台、Kubernetes、网络管理和安全工具。因此 Environment Agent 不应重新实现“发现每台机器、扫描每个端口、读取每个平台”的底层轮子。

推荐定位为：

> **Environment Intelligence and Operations Orchestration Layer**  
> **环境智能与运维编排层**

现有专业系统负责采集、管理和控制；Syscity 负责：

```text
Connector Orchestration
+ Resource Policy
+ Normalized Facts
+ Topology Reconciliation
+ Evidence-aware Agent
+ Cross-source Diagnosis
+ Recommendation
+ Approval Workflow
+ Verification
+ Audit
```

## 26.2 可复用的外部工具类别

### CMDB / IT Asset Management

典型数据：主机、软件、服务、owner/team、environment/site/zone、依赖、生命周期、变更和合规属性。Syscity 应读取 CMDB/IPAM 的权威信息、增量变更和 owner 关系，不重新实现完整 CMDB。

### ITOM / AIOps / Observability

已有系统提供指标、日志、trace、告警、服务依赖、应用性能、事件关联和 SLO/SLA。Syscity 应把告警、指标、日志摘要、Trace、CMDB、部署记录和拓扑关联成 evidence-aware 诊断，而不是替代采集器。

### Kubernetes 和容器平台

使用 Kubernetes API 的 read-only adapter：

```text
Kubernetes API
  → namespace/kind/resource allowlist
  → pagination/cursor
  → redaction
  → normalized facts
  → topology relationships
```

不重新实现 Kubernetes discovery，也不默认读取 Secret、exec、attach、port-forward 或写资源。

### Cloud Resource Graph / Inventory

云平台已经提供 Account/Project/Subscription、Region、VPC/VNet、Subnet、VM、Load Balancer、Database、Queue、Storage、Kubernetes、IAM、Tag、Quota 和 Health API。Syscity 应实现 provider-specific read-only adapters，不通过逐台登录主机、猜测公网端口或万能管理员凭据发现云资产。

### 网络管理和网络监控

优先接入：

```text
IPAM/DNS
+ Network Controller
+ Firewall Manager
+ Cloud Network API
+ Flow Metadata
+ CMDB
```

### Service Discovery 和 Service Mesh

优先复用 Consul、Eureka、etcd、Kubernetes Service、Istio/Linkerd、Envoy、Cloud Service Discovery、API Gateway 和 Load Balancer 的服务实例、endpoint、健康、版本、路由和依赖数据。

### 安全资产和漏洞平台

复用 EDR/XDR、CSPM、CWPP、SIEM、SOAR、证书管理和 Identity Governance。Syscity 将安全 finding 与资产、依赖、变更、owner 和环境 criticality 关联，生成解释和建议，不重新实现漏洞扫描器。

### OT/ICS、IoT、BMS/DCIM 和物理系统

优先读取已有 OT Asset Manager/NMS、SCADA/DCS/Historian、IoT Platform、Fleet Platform、BMS/DCIM、PDU/UPS、Camera/NVR 和 Access Control API。Gamma 环境必须使用独立安全边界，不能复制 IT 网络主动探测策略。

## 26.3 不应重复实现的能力

| 能力 | 优先复用 |
|---|---|
| 主机信息 | OS API、sysinfo、Endpoint Agent |
| Kubernetes 发现 | Kubernetes API |
| 云资产 | 云厂商 Resource API/Graph |
| CMDB 资产 | CMDB/IPAM API |
| DNS | DNS Provider/API |
| 服务发现 | Consul/Eureka/K8s/Service Mesh |
| 指标 | Prometheus/OpenTelemetry |
| 日志 | 企业日志平台 |
| Trace | OTel/Tracing 平台 |
| 网络设备 | NMS/SDN/Firewall Controller |
| 漏洞和安全告警 | EDR/CSPM/SIEM/SOAR |
| OT 资产 | OT Asset Manager/NMS |
| IoT 设备 | IoT/Fleet Platform |
| 楼宇设备 | BMS/DCIM |
| 证书 | Certificate Manager/CA |
| 凭据 | Vault/Secret Manager |
| 工单 | ITSM/Change Management |

## 26.4 Syscity 应自行实现的核心价值

### 统一身份和资源范围

不同系统的 ID 映射到：

```text
Environment → Site → Zone → Node → Asset → Service → Dependency
```

保留：

```text
canonical_id
source_id
source_type
source_record_id
```

### 统一事实和来源

当 CMDB、Kubernetes、Cloud、Prometheus、DNS、Endpoint Sensor 对同一资源给出不同答案时，Syscity 负责 source priority、freshness、conflict、evidence、confidence、tombstone 和时间线。

### 跨源拓扑关联

```text
BusinessApplication
  → Service
  → Deployment
  → Kubernetes/VM
  → Cloud Resource
  → Network Endpoint
  → Metric/Alert
  → Owner
```

### 证据驱动 Agent

Syscity 负责回答：

- 故障最可能原因；
- 哪个最近变更影响最大；
- 哪些 facts 冲突/过期；
- 还应该观察什么；
- 修复会影响哪些服务；
- 是否需要审批。

所有结论带 source、observed_at、confidence、scope 和 coverage gaps。

## 26.5 Connector 优先级

### 第一优先级：权威来源

```text
CMDB/IPAM
→ Kubernetes
→ Cloud inventory
→ DNS
→ Service Registry
→ Prometheus/OpenTelemetry
→ CI/CD
→ Load Balancer
```

### 第二优先级：Endpoint Sensor

Linux、macOS、Windows、systemd/launchd/Windows Service、本地网络和资源状态、只读应用 metadata。

### 第三优先级：登记服务 Healthcheck

固定 TCP、固定 HTTP health path、DNS、TLS、Kubernetes reported health、Load Balancer reported health。

### 第四优先级：特殊环境

OT/ICS、医疗、IoT、BMS/DCIM、安防、机器人、关键基础设施和卫星系统。每类需要独立 threat model、SafetyClass、Connector contract、数据边界和安全评审。

## 26.6 “所有服务”必须定义为可观测覆盖范围

不能无条件发现企业所有服务。系统必须报告：

```text
已覆盖：environment / zone / node / service / source
未覆盖：未连接区域、认证失败源、stale 节点、无 owner 服务、未知关系
```

Agent 不应声称“企业所有服务已发现”，而应输出覆盖率、数据源健康、最近同步时间、stale/unknown 数量、未登记节点和诊断置信度。

## 26.7 服务发现、理解和使用分层

### Level 1：服务元数据读取

服务名、版本、owner/team、endpoint、health、依赖、部署和变更记录。这是 Beta 默认能力。

### Level 2：固定只读 ServiceContract

发现服务后不自动调用任意 API。每个可使用服务必须声明：

```text
service_id
endpoint_allowlist
protocol
method_allowlist
path_allowlist
credential_ref
response_schema
max_response_bytes
timeout
rate_limit
redaction_policy
```

### Level 3：只读业务查询

需要业务 API Connector、method/path allowlist、参数范围、response schema、敏感级别、credential、audit 和 rate limit。

### Level 4：变更和操作

必须通过：

```text
ActionPlan → impact analysis → exact approval → snapshot
→ execution → verification → rollback
```

发现服务不等于获得服务写权限。

## 26.8 云账户中的业务理解

云资源不自动等于业务系统。业务模型需要关联：

```text
Cloud Resources + Tags + CMDB + Kubernetes + DNS
+ Load Balancer + Service Registry + CI/CD
+ Prometheus/OTel + Cost/Owner data
        ↓
BusinessApplication
```

没有 owner、tag、CMDB、部署记录或运行指标时，Agent 只能标记“可能相关”并给出置信度，不能根据命名相似直接断言资源归属于某个业务。

## 26.9 服务层闭环

```text
Environment Search
  → Environment Explain
  → Environment Diagnose
  → Environment Recommend
  → exact approval
  → bounded operation
  → verification
  → environment model update
```

## 26.10 最终产品定位

Syscity 不负责重新实现所有监控、扫描、CMDB、云管理和安全平台。它应作为：

> **Environment Intelligence and Operations Orchestration Layer**  
> **环境智能与运维编排层**

专业系统负责采集、管理和控制；Syscity 负责统一接入、身份和资源政策、事实归一化、跨源拓扑关联、变化检测、证据驱动诊断、Agent 解释、建议、精确审批、有限操作、验证和审计。

这也是本设计最适合落地的完整方向。

---

# 27. 受控环境接管：人类通过对话管理环境

## 27.1 目标重新定义

本设计的最终目标不是让 Syscity 成为无边界的网络扫描器或无限权限控制器，而是让它成为：

> **一个在明确授权、可审计、可撤销和可卸载条件下，由人类通过自然语言管理受控环境的 Environment Intelligence and Operations Agent。**

人类不需要分别操作 CMDB、Kubernetes Dashboard、Cloud Console、Prometheus、Grafana、DNS、Service Registry、ITSM、主机 Shell 和部署平台；Syscity 将这些专业系统连接起来，统一提供：

```text
查询 → 证据 → 解释 → 诊断 → 计划 → 审批 → 执行 → 验证 → 审计
```

## 27.2 “接管”分级

“接管”必须拆成不同能力等级，不能理解为默认获得所有权限。

### Level 0：观察接管

Syscity 可以在授权范围内：

- 读取资产、节点、服务和环境树；
- 查询 Kubernetes、Cloud、CMDB、监控和 DNS；
- 查看部署和变更；
- 查询登记服务健康状态；
- 维护 facts、topology、changes；
- 报告 stale、conflict、coverage gap。

这是 Environment Agent 的第一阶段核心能力。

### Level 1：诊断接管

Syscity 可以：

- 关联告警、指标、日志摘要和 trace 元数据；
- 分析发布记录和环境变化；
- 检查服务依赖；
- 对登记目标执行低影响 healthcheck；
- 生成事件时间线；
- 输出故障假设、证据和置信度；
- 建议下一步只读检查。

### Level 2：规划接管

Syscity 可以生成：

- 维护计划；
- 扩容计划；
- 回滚计划；
- 迁移计划；
- 配置修复计划；
- 依赖切换计划；
- 故障恢复计划。

但默认只生成 `ActionPlan`，不执行动作。计划必须包含：

```text
目标
前置条件
影响范围
具体步骤
风险
快照要求
验证方式
回滚方案
所需权限
维护窗口
```

### Level 3：受控执行接管

仅当以下条件全部满足时，Syscity 才能执行动作：

1. 目标资源已登记；
2. Action 在 capability grant 内；
3. 目标属于当前 environment/site/zone/node scope；
4. ActionPlan 已固定；
5. action hash 与批准内容一致；
6. 审批人具备足够权限；
7. 存在变更单或维护窗口；
8. 已创建前置 snapshot；
9. 有验证步骤；
10. 有 rollback/compensation；
11. 有 timeout 和 kill switch；
12. 全过程写入 audit。

Level 3 不属于默认 Beta 能力；Gamma 和关键基础设施环境默认只允许 Level 0/1/2。

### Level 4：完全接管（Full Environment Operations）

Level 4 表示：在一个**明确界定、明确授权、可暂停、可撤销的 EnvironmentProfile 内**，Syscity 可以作为主要的自然语言运维入口，持续执行观察、诊断、计划、审批后的操作、验证、回滚、事件协同和生命周期管理。

Level 4 **不表示无限权限、无边界网络控制或脱离人类治理的自主运行**。它表示的是：

```text
完整环境服务闭环
  + 受治理的操作自治
  + 明确的能力边界
  + 可验证的安全约束
  + 随时可暂停/撤销/卸载
```

#### Level 4 可以做什么

在 EnvironmentProfile 的授权范围内，Syscity 可以：

- 持续维护环境资产、服务、拓扑、部署、健康和变化模型；
- 通过对话回答环境、业务和基础设施问题；
- 自动执行已批准的低风险观察和健康检查；
- 发现异常后生成诊断、影响分析和修复计划；
- 根据预先批准的 Runbook 执行有限的标准化操作；
- 在满足前置条件时执行经过策略批准的变更；
- 执行 post-check、SLO/健康验证和业务验证；
- 失败时执行预先定义的 rollback/compensation；
- 处理事件、创建工单、通知负责人和更新状态；
- 管理 Sensor、Connector、ObservationJob 和凭据轮换；
- 根据确定性策略调整观察频率、重试和降级；
- 在发现超出范围的资源时提出授权申请，而不是自动扩大权限。

#### Level 4 的四类操作策略

Level 4 内部仍必须区分操作策略：

```text
Auto-safe
  预先批准的低风险、幂等、可验证操作

Approval-required
  每次执行都需要人类精确批准的操作

Change-window-only
  只能在维护窗口、变更单和双重审批下执行的操作

Disabled
  当前环境类型或产品版本永久/暂时禁止的操作
```

一个 Action 必须先匹配 `ActionPolicy`，再决定是否需要当前审批。模型不能通过自然语言将 `Disabled` 提升为 `Approval-required` 或 `Auto-safe`。

#### Level 4 的必要前提

Level 4 只能在以下条件全部满足后启用：

1. EnvironmentProfile 已建立并由责任人批准；
2. 所有 Site/Zone/Node/Collector/Connector 已注册并有稳定身份；
3. capability grants、resource selectors 和 sensitivity ceilings 已冻结；
4. PolicyEngine 已覆盖 method/resource/data/action 四类授权；
5. 所有 Action 都有 action hash、幂等键、前置条件和最大影响范围；
6. 生产操作具有 snapshot、verification、rollback/compensation 和 kill switch；
7. 审批 authority、维护窗口、变更单和责任人已定义；
8. 审计记录完整、脱敏、持久化并且可查询；
9. 任务、连接器和 Sensor 支持取消、暂停、撤销和断线恢复；
10. 经过 staging/canary 试运行和故障演练；
11. 备份、恢复、版本回滚和证书轮换已验证；
12. Environment、Gamma SafetyClass 和业务连续性策略允许该级别。

#### Level 4 的硬性禁止事项

即使在 Level 4，也不允许通用 Agent：

- 自动扩大 Environment、Zone、CIDR、Node 或 Connector scope；
- 自行授予 capability、admin 或 root；
- 读取 Secret、私钥、Cookie、token 或不必要的凭据原文；
- 禁用 EDR、审计、访问控制或撤销机制；
- 删除或篡改 evidence、audit、变更记录或故障记录；
- 自主传播、隐藏安装、横向复制或规避检测；
- 对未授权公网、第三方网络或未知资产执行探测；
- 对 OT、医疗、交通、电力、水务、车辆、机器人或卫星控制域执行通用写操作；
- 在 PolicyEngine、AuditStore 或 RevocationService 不可用时 fallback 到允许；
- 将“Agent 认为安全”当作人类审批或安全授权。

#### Level 4 的降级和紧急控制

Level 4 必须支持立即降级：

```text
Full
  → Recommend-only
  → Read-only
  → Paused
  → Isolated
  → Revoked
  → Uninstalled
```

触发条件包括：

- 大量 policy deny 或异常请求；
- audit 写入失败；
- facts/Connector 数据严重过期；
- Connector 凭据异常；
- 任务超预算或持续重试；
- 发现 scope 漂移；
- 关键验证失败；
- 节点身份变化；
- 操作员触发 kill switch；
- 环境进入维护、应急或安全事件状态。

降级必须是 fail-closed 的：停止新 Action，取消可取消任务，保留必要审计，标记未完成操作，并等待人工恢复。

#### Level 4 的适用范围

| 环境类型 | Level 4 建议 |
|---|---|
| 个人/开发环境 | 可在明确授权后启用，但仍保留撤销和审计 |
| 测试/预发布 | 可作为自动化运维试验场，先验证 rollback |
| 普通企业 IT 生产 | 可逐步启用，优先 Auto-safe 和 Approval-required |
| 高敏感生产系统 | 仅限严格 Runbook、维护窗口和多级审批 |
| OT/ICS | 默认不启用通用 Level 4 |
| 医疗/生命安全系统 | 默认不启用通用 Level 4 |
| 交通/电力/水务/关键基础设施 | 需要独立行业控制面和安全审查 |
| 车辆/机器人/卫星控制域 | 仅可提供专用、隔离、人工治理的控制能力 |

Level 4 的最终含义是“完整受治理的环境服务”，不是“无限制的自动管理员”。

## 27.3 目标产品架构

```text
┌─────────────────────────────────────────────────────────┐
│                 Human / Operator                        │
│ Web · TUI · CLI · Desktop · Chat                       │
└────────────────────────┬────────────────────────────────┘
                         │ natural language / WS RPC
┌────────────────────────▼────────────────────────────────┐
│                 Syscity Conversation Layer              │
│ session · context · streaming · user confirmation       │
└────────────────────────┬────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────┐
│              Environment Control Plane                   │
│ registration · grants · PolicyEngine · jobs · audit      │
│ approvals · revoke · pause · isolate · uninstall         │
└───────────────┬───────────────────────┬─────────────────┘
                │                       │
      ┌─────────▼────────┐   ┌────────▼─────────┐
      │ Collection Plane  │   │ Knowledge Plane   │
      │ Endpoint Sensor   │   │ Facts/Evidence    │
      │ Zone Collector    │   │ Topology/Changes  │
      │ Platform Connector│   │ Health/Ownership  │
      └─────────┬────────┘   └────────┬─────────┘
                │                     │
                └──────────┬──────────┘
                           │ policy-filtered context
                ┌──────────▼──────────┐
                │ Existing Agent Plane│
                │ Search/Explain      │
                │ Diagnose/Recommend  │
                └──────────┬──────────┘
                           │ exact approval only
                ┌──────────▼──────────┐
                │ Bounded Action Plane │
                │ Snapshot/Act/Verify │
                │ Rollback/Audit      │
                └─────────────────────┘
```

## 27.4 现有 Syscity 的整合方式

### 现有 Runtime 继续负责

- Gateway 和 WS 协议；
- Agent turn、上下文、流式响应；
- ToolRegistry 和工具生命周期；
- Provider/model routing；
- Memory 和通用会话；
- Cron、Heartbeat、TaskRegistry；
- TUI/Web/CLI/桌面入口；
- 通用审批、审计和日志。

### Environment Domain 新增负责

- EnvironmentProfile；
- Site/Zone/Node/Collector/Connector；
- Registration 和 capability grants；
- ObservationScope/Job/Run；
- EnvironmentStore；
- Facts/Evidence/Topology/Changes；
- Service Inventory 和 ServiceContract；
- NetworkTargetPolicy；
- EnvironmentPolicyEngine；
- Environment-specific retention/revoke/health。

这使得 Syscity 变成：

```text
已有 Agent Runtime
  + Environment Control/Knowledge Domain
  + Existing Enterprise System Connectors
  + Bounded Action Executor
```

## 27.5 人类对话与环境服务示例

### 查询环境

```text
人类：生产环境现在有哪些服务？

Syscity：
当前已覆盖：
- 7 个 zone
- 142 个 node
- 480 个 service
- 9 个数据源

发现：
- 35 个 service 没有 owner
- 12 个 node 的 heartbeat 已过期
- 4 个 CMDB 资源与 Kubernetes 状态冲突
- 最近一次完整同步在 8 分钟前完成
```

### 诊断故障

```text
人类：生产订单服务为什么不可用？

Syscity：
已确认：
- 期望副本数：6
- 当前可用副本数：2
- 最近部署版本：order-api:2.8.1
- 两个 Pod 处于 CrashLoopBackOff
- Load Balancer 健康
- 数据库连接正常

初步判断：rollout 或应用启动错误，置信度 0.86。

缺少证据：Pod 错误摘要尚未读取。
建议：执行只读日志摘要查询，不修改系统。
```

### 生成计划

```text
人类：生成回滚 payment-api 的计划，但不要执行。

Syscity：
ActionPlan：
- 目标：payment-api deployment
- 版本：4.1.2 → 4.1.1
- 影响：payment-api、payment-worker
- 前置条件：确认旧镜像存在
- 快照：deployment spec、replica、service endpoint
- 验证：HTTP health、错误率、P95 延迟
- 回滚：验证失败时恢复原版本
- 所需审批：生产变更审批
- 当前状态：待审批
```

### 精确执行

```text
人类：批准，只允许操作 staging，不允许修改数据库和网络。

Syscity：
批准范围：
- service: payment-api
- environment: staging
- action: deployment rollback
- database/network actions: denied

正在执行前置快照。
```

### 撤销环境 Agent

```text
人类：暂停 zone-b 所有环境观察任务。

Syscity：
已暂停：
- 3 个 Endpoint Sensor
- 1 个 Zone Collector
- 12 个 observation jobs

正在取消 2 个运行中的 healthcheck。
历史 facts 保留，状态标记为 paused。
```

## 27.6 接管边界

### 可以管理

- 已登记主机；
- 已登记网络区域；
- 已授权 Kubernetes；
- 已授权 Cloud Account；
- 已授权 CMDB/IPAM；
- 已授权 Observability；
- 已登记服务；
- 已批准 healthcheck；
- 已批准有限 Action。

### 默认不管理

- 未登记主机；
- 未登记网络；
- 未授权 Cloud Account；
- 未授权 Kubernetes Cluster；
- 任意公网；
- 未授权 IoT/OT/医疗/关键设施；
- Secret 内容；
- 隐藏控制面；
- 未审批生产写操作。

## 27.7 完整接管闭环

```text
1. 创建 EnvironmentProfile
2. 注册 Site / Zone / Node / Collector / Connector
3. 管理员审批 capability 和 resource scope
4. 执行被动 inventory 同步
5. 更新 Facts / Entities / Relationships
6. 计算 snapshot diff
7. 对已登记服务执行低影响 healthcheck
8. 发现异常、变更或覆盖缺口
9. 生成 evidence-aware diagnosis
10. 通知负责人或生成工单
11. 生成 Recommend 或 ActionPlan
12. 等待精确审批
13. 保存前置 snapshot
14. 执行有限 action
15. 验证服务和业务状态
16. 失败时 rollback/compensation
17. 写入审计和指标
18. 更新环境模型
19. 支持 pause/revoke/isolate/uninstall
```

## 27.8 重要技术判断

- `Environment Agent` 不是另一个聊天 UI，而是 Environment Control/Knowledge Domain；
- `Endpoint Sensor` 不是任意 Shell 远程执行器，而是受 scope 和 policy 约束的 collector；
- `Zone Collector` 不是网络扫描器，而是授权区域内的 bounded observation worker；
- `Connector` 不是任意 API proxy，而是声明 capability、resource filter、credential ref 和输出 schema 的数据适配器；
- `EnvironmentStore` 不是普通 Memory，而是带 provenance、TTL、conflict 和 scope 的事实库；
- `Diagnostic Agent` 不是自动管理员，而是 evidence-aware explain/diagnose/recommend 层；
- `Action Plane` 不是默认开启的工具集合，而是后置、精确审批、可验证、可回滚的执行域。

## 27.9 最终整合结论

> **按 `environment-agent.local.md` 演进 Syscity 是正确方向，但必须将 Syscity 定位为“连接现有企业工具、统一环境事实、理解跨源拓扑、辅助人类决策并在批准后安全编排动作的 Environment Intelligence Agent”，而不是重新实现所有资产采集系统或成为无限权限的超级管理员。**

Level 4 不是对安全边界的取消，而是对“完整环境服务能力”的定义：Syscity 可以在一个明确的 EnvironmentProfile 内，持续观察、理解、诊断、规划、执行允许的操作、验证结果、回滚失败、处理事件并管理 Sensor/Connector 生命周期；但它始终受 capability grant、resource scope、ActionPolicy、approval authority、SafetyClass、audit、kill switch 和 revoke 约束。

完整对话入口的目标是：

```text
人类：为什么订单服务不可用？
Syscity：基于当前 facts、metrics、changes 和 healthchecks 诊断。

人类：生成恢复计划。
Syscity：生成带影响、快照、验证和回滚的 ActionPlan。

人类：批准，只允许操作 staging。
Syscity：只执行批准范围内的动作，并完成验证和审计。

人类：停止这个环境 Agent。
Syscity：暂停 jobs、撤销凭据、隔离节点并保留审计记录。
```

因此，Level 4 应作为 G8 以后、经过 staging/canary、故障演练和独立安全评审的能力，而不是 Beta 默认能力。共享多租户、自传播、无边界扫描、模型自主扩权和未审批生产变更仍不在任何接管等级内。 

当前最安全、最现实的路线仍然是：

```text
G0 控制面安全
  → G1 EnvironmentStore/领域契约
  → G2 Registration/Capability/Policy
  → G3 单主机只读 Sensor
  → G4 被动企业资产和多节点拓扑
  → G5 Kubernetes/Cloud/CMDB/Observability Connectors
  → G6 已登记服务 Healthcheck
  → G7 诊断 Agent
  → G8 受审批 Action
  → G9 生产部署和 Gamma 扩展
```

当前代码可复用 Runtime 基础，但 `src/environment/`、EnvironmentStore、PolicyEngine、Registration Registry、Topology Store、企业 Connector 和 Network Zone Collector 仍未实现。共享多租户、自传播、无边界扫描、模型自主扩权和未审批生产变更继续不在范围内。  