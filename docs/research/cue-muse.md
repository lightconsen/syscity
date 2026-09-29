# Cue 与 Muse 研究：个人 agent 怎么"打电话"和"发邮件"

> 来源：2026-09-29 对公开报道与厂商文档的网络调研（链接见文末）。
> **没有代码级素材** —— Cue 与 Muse 均为闭源商业产品，本文**全部为二手信息，无一条经逐字代码复核**。
> 用途：内部研究笔记，**只记录 Cue 与 Muse 自身的实现**。与 Syscity 的对比、以及由此得出的
> 动作项在配套的 `cue-muse.local.md`（被 `.gitignore` 忽略，不提交）。
> 相关：`manus.local.md`（Manus 与 Syscity 的对比，同样不提交）。

标注约定：

- `[官方]` = 来自厂商官方发布说明或文档；
- `[媒体]` = 来自第三方报道（数据类断言全归此类）；
- `[未披露]` = 公开材料未说明 —— **这类不要对外引用**；
- `[存疑]` = 来源之间有出入或明确提示未经验证。

---

## 一、一句话结论

**两家都不"操作 App"。**

Cue 给每个 agent 一套**自己的数字身份**（独立手机号、邮箱、钱包、云电脑），Muse 给 agent 一台
**云 VM** 并让它接管**用户自己的**账号。两者都用"绕开 App"取代了"驱动 App"——不需要拨号器、
不需要操作短信界面、不需要在任何客户端里点按钮。

这直接回答了那条路径为什么在商业产品里看不到：**给 agent 一个号码，比教它点按拨号盘便宜得多，
也可靠得多。**

## 二、前提：Meta 与 Manus 的关系（极易被误引）

Cue 与 Muse 在同一个九月先后发布，**但它们是竞品，不是同一家的两个产品**。中间那段收购
被中国监管强制解除：

| 时间 | 事件 |
|---|---|
| 2022 | Manus（Butterfly Effect 的产品）在中国成立，2025 年总部与核心员工迁至新加坡 `[媒体]` |
| 2025-12 | Meta 宣布以约 **$20 亿**收购 Manus，当时是 Meta 史上第三大收购 `[媒体]` |
| 2026-04 | 中国发改委（NDRC）下令**全额解除**该交易 —— 罕见的强制逆转 `[媒体]` |
| 2026-06 | Meta 完成运营拆分，切断数据互访，内部停用 Manus `[媒体]` |
| 2026-08 | Manus 宣布恢复独立运营；腾讯领投回购，继续在新加坡运营；ARR 据称接近 $5 亿 `[媒体]` |
| 2026-09-08 | **Meta 发布 Muse**（跑自研 Muse Spark 模型）`[媒体]` |
| 2026-09-28 | **Manus 发布 2.0 与 Cue** `[媒体]` |

> 注：早先有报道称"Manus 被 Meta 收购并跑在 Meta 模型栈上"，那是**收购解除前的口径**，
> 现在已不成立。引用时不要再用。

---

## 三、Cue（Manus）

**形态**：独立的个人 agent 应用，与 Manus 共用基础设施，但面向个人生活场景。
Web / 桌面 / 移动端（iOS 版发布时仍在 App Store 审核中）。口号："give it a cue, and it gets to work"。`[媒体]`

### 3.1 机制：不做"操作"，做"身份"

每个 Cue agent 拥有一套**独立的数字身份**：自己的**手机号、邮箱地址、钱包、云电脑**。`[媒体]`

官方给的理由是**权限边界更清楚** —— agent 以自己的身份跟商户/机构打交道，而不是借用用户的账号。`[媒体]`
这是"操作 App"的完全反面：agent 有自己的号码，就不需要碰用户的拨号器。

### 3.2 电话

借助独立号码，agent 可以：`[媒体]`

- **拨打与接听电话**，并**收发短信**；
- **代用户接电话** —— 报道举的例子：诊所回电，agent 接起来确认改约细节，然后把纪要写进应用；
- 通话结束后留下一份**通话纪要**。

`[未披露]` **具体使用哪家号码/VoIP 供应商，中文与英文来源都没有公开说明。不要对外断言。**

### 3.3 邮件

用自己的邮箱地址收发。`[媒体]` 具体接入方式（自建 SMTP 还是第三方）`[未披露]`。

### 3.4 付款

用户设定预算，每笔交易前 agent 把预期金额呈现给用户，用户选"允许/拒绝"后才确认。`[媒体]`

### 3.5 其它

- **多 agent 协作**：多个 agent 可放进同一个群聊分工，互相交接工作，用户设定方向并做最终决定。`[媒体]`
- **线下场景**：扫餐厅二维码让 agent 点餐或排队。`[媒体]`

### 3.6 可用性与定价

邀请制早期访问，**免费**；Manus 放出邀请码 `MEETCUE` 供前 1000 名用户，获准用户会拿到可分享的码。
**正式定价未公布。** `[媒体]`

---

## 四、Muse（Meta）

**形态**：个人 AI agent 应用，2026-09-08 在**美国与加拿大**上线，**限 18 岁以上**。
免费 + 周用量上限，付费档 $20/月（Power，5 亿 token/周）、$100/月（Max，30 亿 token/周）。
可在 Mac、WhatsApp、web 使用，智能眼镜集成在计划中。上线后成为 App Store 免费榜第一，
首几周美国下载超 250 万。`[媒体]`

### 4.1 运行环境：Muse Secure VM

Muse 跑在 **Muse Secure VM** 上 —— Meta 云中一台**隔离的 Linux 虚拟机**，带浏览器、存储、记忆。
VM 里装着 agent、用户文件/工作区，以及每个已连接服务的凭证。**关掉 App 后它继续工作。** `[媒体]`

### 4.2 两个分区

VM 内部划分为两个区：`[媒体]`

- **密封的运行时 cell** —— agent 与它的工具在这里处理**不可信数据**；
- **cell 之外的服务层** —— 保管密码，并决定 agent 能做什么。

cell 内的 admin 权限**伸不到宿主机**，且 cell 只能通过本地通道访问外部服务。

### 4.3 Sentinel：独立的审批 agent

**Sentinel** 是跑在同一 VM 上、但**系统级与 Muse 隔离**的第二个 agent。`[媒体]`

- Muse 的任何行为**未经 Sentinel 批准到不了互联网**；
- Sentinel 逐条检查出网请求，把每个连接器的调用对照用户设定的权限，然后**放行 / 拒绝 / 交给用户决定**；
- 关键设计：**审批提示是 Muse 应用里的系统对话框，不是对话流中的消息** —— 因此提示注入产生的
  文本**伪造不出用户的同意**。

### 4.4 凭证处理

- **Muse Spark 从不接触真凭证**：cell 之外的凭证服务处理密码与 access token，agent 手上只有**代理 token**。
- 请求离开 VM 时，Sentinel 通过 `authd` 做**即时凭证注入**，只在网络边缘把真凭证换进去。
- 用户把密钥粘贴进 **Secure Credentials Store**，而不是粘进对话。
- **eBPF 内核监控**跟踪数据流，发现不可信内容可吊销权限。

Meta 的说法是这个设计让"提示注入窃取凭证"成为不可能 —— 因为 agent 手上根本没有凭证。`[存疑]`

### 4.5 邮件：用**你的**账号

这是与 Cue 最大的机制差异。`[媒体]`

- 连接**默认只读**；**发信需要显式批准**，除非用户建了更宽的常驻规则。用户自行选择 Muse 是只读信箱，
  还是可以代其发信。
- 邮件连接器在 agent 读到之前，会**剥离一次性验证码、密码重置链接、登录 magic link**（固定规则 + 分类器）。
  理由是：**一个收件箱能重置你拥有的每一个账号**。
- 发信走的是**用户的 Gmail** —— 于是发出的内容顶着**用户自己的名义、地址与信誉**。

### 4.6 已知代价（第三方分析）

- Muse 的活动形态是"Meta 云上一台 Linux VM、服务端登录、固定时间表"，**在 Google 看来像账号被盗**，
  可能触发登录验证甚至停用账号。`[媒体]`
- 剥离验证码的副作用：**Muse 无法通过 Gmail 完成邮箱验证流程**，凡是用邮件发码的注册都会卡住。`[媒体]`
- **Amazon 封禁了 Muse** 访问其站点，指控它未表明自己是 AI agent、且存储了客户凭证。`[媒体]`

### 4.7 商业化与后续

- Meta Connect 2026 宣布购物合作：Stripe、Shopify（Shop Pay）、PayPal、Walmart、Best Buy、
  Sephora、Expedia 等；Zuckerberg 暗示未来可能抽取少量交易费。`[媒体]`
- 计划推出 **Muse Confidential VM**：用用户持有的密钥在可信执行环境中加密整台 VM，**连 Meta 也访问不了**。`[媒体]`
- 上线后为 Muse 补了**专属邮件分页**。`[媒体]`

### 4.8 重要免责

**以上整套安全设计全部是 Meta 自己的描述，尚未经独立研究者检验。** `[存疑]`

---

## 五、机制对照

| | **Cue**（Manus） | **Muse**（Meta） |
|---|---|---|
| 操作 App | **不操作** | **不操作** |
| 核心机制 | 给 agent **独立数字身份**（号码/邮箱/钱包/云电脑） | 云 VM + **接管用户自己的账号** |
| 必需云 | 是（身份在云） | 是（Secure VM） |
| 电话 | **有自己的号码**，可拨打/接听/代接 + 纪要 | **公开材料未见电话能力** `[未披露]` |
| 邮箱 | **自己的邮箱地址** | **用户的 Gmail**，默认只读、发信需批准 |
| 凭证归属 | agent 自有身份，不借用户账号 | 用户账号，代理 token + 出网时注入真凭证 |
| 权限闸门 | 交易级用户确认（预算内） | **独立 agent（Sentinel）**做系统级仲裁 |
| 收件箱风险处理 | — | 读前剥离验证码/重置链接/magic link |
| 协作 | 多 agent 群聊分工、互相交接 | 未在材料中强调 |
| 定价 | 邀请制免费，**定价未公布** | 免费（有周限）+ $20 / $100 月费 |
| 上线 | 2026-09-28 | 2026-09-08 |

---

## 六、未确认清单（不要引用）

- Cue 的**电话/VoIP 供应商** —— 中英文来源均未披露。
- Cue 的电话是否为真 PSTN 号码 —— 中文来源提到收发短信，**暗示**是真号码，但无官方确认。
- Cue 邮箱的接入方式（自建 SMTP / 第三方）。
- **Muse 是否支持打电话** —— 公开材料只讲邮件、订票、购物、填表、谈账单、约时间，**没有电话**。
  不要假设它有。
- Muse 整套安全架构 —— 全部是 Meta 单方描述，无独立验证。
- Cue 的正式定价与后续商业模式。
- 各来源的下载量/股价涨幅/ARR 数字均为 `[媒体]` 口径，差异较大（股价涨幅报道从 20% 到 30% 不等）。

---

## 七、来源

- [Manus Launches Cue Agents With Phone Numbers and Wallets — implicator.ai](https://www.implicator.ai/manus-cue-agents-phone-numbers-wallets/)
- [Manus 2.0 Launch: Studio, Cue & Automations — explainx.ai](https://www.explainx.ai/blog/manus-2-0-studio-cue-cascade-cloud-computer-2026)
- [Manus推出个人Agent应用Cue：每个Agent拥有独立电话、邮箱、钱包与电脑 — AIHub](https://www.aihub.cn/news/manus-cue/)
- [Cue - Manus 推出的个人 AI 智能体应用 — AIHub](https://www.aihub.cn/agents/cue/)
- [Meta's Muse Agent Uses Your Accounts and Payments to Take Action — deeplearning.ai](https://charonhub.deeplearning.ai/how-to-secure-agents-for-the-masses/)
- [Should You Connect Meta Muse to Gmail? — AgentMail](https://www.agentmail.to/blog/muse-gmail-vs-agentmail)
- [Zuckerberg pitches Muse as a privacy-first AI agent — 4sysops](https://4sysops.com/archives/zuckerberg-pitches-muse-as-a-privacy-first-ai-agent/)
- [What is Meta Muse? The AI agent explained — Trusted Reviews](https://www.trustedreviews.com/explainer/what-is-meta-muse)
- [Meta's viral AI agent can now book travel and shop for you. Amazon says it crossed a line — LA Times](https://www.latimes.com/business/story/2026-09-23/meta-muse-can-now-book-travel-shop-for-you-amazon-says-it-crossed-line)
- [Manus to return as independent company after China forced Meta to unwind $2 billion deal — CNBC](https://www.cnbc.com/2026/08/11/manus-china-meta-acquisition.html)
- [Meta Severs Manus Data Access After China Orders Buyout Unwound — Bloomberg](https://www.bloomberg.com/news/articles/2026-06-11/meta-severs-manus-data-access-after-china-orders-buyout-unwound)
