# Cua 代码研究：Computer-Use 2.0 怎么实现的

> 来源：`/Users/lando/my/cua`（trycua/cua 的快照，**不是 git 仓库**，因此无法确认对应哪个版本/commit）
> 素材：`blog/computer-use-2-ai-engineer-worlds-fair.md`、`README.md`、`libs/cua-driver/`（Rust + 各平台辅助件）、`libs/cua-bench/`、`libs/python/{computer-server,mcp-server,cua-cli,som}`、`libs/fleet/`
> 用途：内部研究笔记，**只记录 cua 自身的实现**。与 Syscity 的对比、以及由此得出的动作项在配套的 `cua-notes.local.md`（被 `.gitignore` 忽略，不提交）。
> 方法：四路并行代码调研；**关键断言本人复核过**（文中标"已核实"），其余为调研结论。博客口径与代码不符处集中在第十一节——那份博客是宣传材料，不是规格。

---

## 一、一句话结论

**Computer-Use 2.0 不是新模型，是关系反转**：让用户已经在跑的 agent（Claude Code / Codex / Hermes）**拥有任务**，把桌面降级成它众多工具中的一个。代价是放弃"整屏独占 + 前台循环"，换来的是后台执行（不抢焦点、不动物理光标）与同机多 agent 并行。

技术重心随之从 `Computer Server`（1.0 循环，仍在维护）转到 **Cua Driver**：**以窗口为单位**而不是整块屏幕，跨 macOS/Windows/Linux 提供同一套命令。

对 1.0 的批评很具体，值得记：屏幕几乎是 agent 知道的全部，鼠标键盘几乎是它能做的全部；而"改了个设置没反应"这类问题，看屏幕只能猜——2.0 的 agent 可以去读日志、改配置、重启进程，再回 GUI 验收。**GUI 从"整个循环"变成"循环里的一种接口"。**

## 二、进程与传输模型

- **全程 NDJSON**（一行一个 JSON，无 Content-Length 分帧）。agent 侧只看到一个面：`cua-driver mcp`（JSON-RPC 2.0 over stdio）。
- 那个进程有两种形态：**(a) SDK 进程内的 runtime**，或 **(b) 代理到长驻的 `cua-driver serve`**（同一个 NDJSON 协议，走 Unix socket / Windows 命名管道）。
- 选哪种是一个纯函数：macOS **恒为代理**，Windows/Linux 在没给 `--socket` 时进程内直跑。原因不是性能而是 **TCC 归属**——daemon 经 LaunchServices（`open -n -g -a CuaDriver --args serve`）启动，才有稳定的辅助功能/录屏身份。
- Socket：macOS `~/Library/Caches/<ns>/<ns>.sock`、Linux `~/.cache/<ns>/<ns>.sock`、Windows `\\.\pipe\<ns>`。超时 5 s 写 / 10 s 读 / **120 s 总 deadline**；`WouldBlock|TimedOut` 被当作"daemon 还在排空"而非致命传输错误。
- CLI 的一个细节：**未识别的第一个位置参数会被当成工具名**并路由到 `call`；而 `call` **要求 daemon 在跑**（"Cua Driver daemon is not running…"），注释写明理由是让策略、session 状态、平台身份只有**一个执行点**——CLI 直连平台 helper 会绕过授权。
- SDK（Python/TS）走 **UniFFI + 稳定 C ABI**（符号 `cua_driver_*_v1`，头文件入库且 CI 校验），不是子进程、不是 JSON-RPC。两种 SDK 的 README 都写明同一句边界：**语言包是给宿主应用的，agent 的边界永远是那个可执行文件。**

## 三、窗口模型与状态读取

**窗口就是一个原生整数 id**，没有句柄对象过线：macOS `CGWindowID`、Windows `HWND`、X11 `XID`。只有 Wayland 没有这个能力（协议不给 pid/几何），于是 Sway 用 `stable_id`、Hyprland 用它自己的 handle，GNOME/KWin 必须靠 compositor 内的扩展。

`get_window_state` **一次调用同时返回元素树和截图**，两半可各自关掉，但**两个都不要是 error、不是空成功**。边界值：

| 平台 | 节点上限 | 深度 | 单节点超时 |
|---|---|---|---|
| macOS | 2000 | 25 | 2.0 s（AX 消息超时） |
| Windows | 5000 | 25 | 4 s |
| Linux | 5000 | 不限 | — |

元素寻址用**快照绑定的 token**：`s{snapshot_id:08x}:{index}`，每 pid 保留 8 个快照（LRU）。用旧 token 会被拒，错误信息直接可行动：`"element_token is stale; call get_window_state again to refresh"`。分辨率失败是一组封闭的错误码（`element_index_required` / `conflicting_element_target` / `generation_mismatch` / `stale_element_token` …）。

截图后端：macOS ScreenCaptureKit（抓完**再校验一次窗口身份**，不符即失败，防 TOCTOU）→ `screencapture -l` 兜底；Windows `PrintWindow(PW_RENDERFULLCONTENT)` → WGC → BitBlt；Linux SHM `shm_get_image` → `XGetImage` → ImageMagick。

**一处真实的自相矛盾**（对我们的含义见配套的 `cua-notes.local.md`）：「遍历是否完整」的字段 `elements_complete` 存在，但被**硬编码为 false**（已核实：`platform-linux/.../tools/impl_.rs:893`；契约里是 `Option<bool>`，`cua-driver-contract/src/windows.rs:261`），而截断**只标在给模型看的 markdown 文本里**。也就是说：结构化字段不区分"完整树"和"被截断的树"，而下游 `verify_state` 恰恰因为"遍历不穷尽"拒绝 `element.exists: false`。

## 四、动作：**不是**"阶梯"（这是最容易误读的一点）

博客的描述容易让人以为它按顺序重试。实际是**一个纯判定函数 + 一次命令式派发**：

```rust
decide_background_input(target, facts, action)
    -> Execute { verification } | Refuse(BackgroundRefusal)
```

优先级固定：窗口归属 → 元素祖先 → AX 窗口存在性 → 可见性 → 动作策略。**拒绝是终局**，不是"换下一级"。拒绝码是一组冻结的字符串（`window_not_found` / `owner_pid_mismatch` / `off_space_or_ax_unresolved` / `minimized_or_hidden_window` / `same_pid_keyboard_ambiguity` / `element_outside_target_window`），并带 `escalation:{recommended, reason}` 作为**建议**。

另外两个设计取向：

- **缺省是后台**。`InputDeliveryMode::parse` 把 `"foreground"` 之外的一切（包括没写）都当 `Background`——没想过这事的调用方拿到的是不抢焦点的那条路。
- **AX 点击"按了元素不宣称的动作"→ 报 `suspected_noop`，不是失败**。判据是 `suspected_noop = !advertised.contains(&ax_action)`。

第三级（"抬起窗口几毫秒再恢复焦点"）用的是**私有 SkyLight SPI**，不是 `AXRaise`：`SLPSSetFrontProcessWithOptions(kCPSNoWindows=0x400)` + 248 字节的 `SLPSPostEventRecordTo` 记录，激活等待 400 ms（10 ms 轮询）。还有一条独立的 reactive 防线 `focus_steal.rs`（5 s 期限、1 s tick）专门跟别的进程抢回焦点。

## 五、效果判定：confirmed / unverifiable / suspected_noop

核心是一行：

```rust
if confirmed && readback_available { Confirmed }
else if changed || !readback_available { Unverifiable }
else { SuspectedNoop }
```

真正有意思的是**哪些信号被明确排除**：原生 API 接受、事件收到、截图比对、操作者观察——这四类在投影到线上时被**过滤掉**，理由是它们"不能独立证明 confirmed"。能上线的只有 `value_readback` 与 `window_change` 两类证据。

而 `window_change` **不是像素比对**，是可见 layer-0 窗口 id 的集合差 + 前台 pid 是否变化（1000 ms 超时、50 ms 轮询）。便宜，但对"同一个窗口内部的变化"完全无感。

后置条件检查是**另一个工具** `verify_state`，三态 `satisfied | unsatisfied | unknown`，并明确规定 **`unknown` 永远不等于成功**；其中 `element.exists` 的 schema 只允许 `enum: [true]`，因为"不存在无法被证明"。

## 六、Session、多 agent、光标

- session id 是全局 `HashMap` 的键，**外加一个 per-transport 的 owner lease**。归属校验失败一律 fail-closed（"session is not available to this transport"）。tombstone 保留归属，`end_session_for_owner` 对"不存在的"和"别人的" id **返回同样的 false**（刻意不可枚举）。复活要过归属检查，注释写得很好：*"A public label is never proof that a new transport owns a prior episode."*
- 授权上下文本身是**不可序列化、无公开构造函数的证明类型**（`AuthenticatedActionConnection` / `TrustedHostLease`），注释：路径寻址的 socket 或调用方自报的 session 字符串**永远不能**伪造出它。
- 所有可变状态按 `runtime_scope_key()` = `<daemon 代际 UUID>:<public>` 命名空间隔离；daemon 重启即世代失效。
- **idle TTL 5 分钟，四处独立执行**（核心常量、SDK 清扫线程、核心 evict、授权上下文时钟）。两个时钟可能不一致，授权那个直接让调用失败。
- **彩色光标纯属视觉**：contract 原文"best-effort visual telemetry. They never affect authorization, dispatch, input delivery, or tool results."覆盖层自己维护 `pos`，初始是屏幕外哨兵 `(-200,-200)`；真实指针移动是另一段代码。
- **多 agent 的互斥不在 Rust 侧**（Rust 里根本没有指针对互斥），而在 Hyprland compositor 插件里：每个 agent **一个自己的合成 seat** + lane，`TARGET` 判定用 `agent_conflict`→`agent_target_busy`、`primary_conflict`→`primary_target_busy`、以及覆盖 grab/DnD/独占层的 `foreground_guard`。授权有时限与能力位上限（`deadline-now > 60000` 拒绝、`caps==0||caps>15` 拒绝）。设计原则一句话：**"Pointer presence is not input authority"**（被动悬停不算输入权，观察者不打架）。
  - 但项目**自己的证明脚本**里写着 `active_lease_conflict: unproven`、`same_process_sibling_window: unproven`——多 agent 隔离的强断言他们自己也没证完。

## 七、各平台后台输入：机制与硬限制

| | 机制 | 硬限制（代码/文档自己承认的） |
|---|---|---|
| **macOS** | 私有 SkyLight SPI：`SLEventPostToPid → SLEventPostToPSN → … → IOHIDPostEvent`。**不能用公开的 `CGEventPostToPid`**——它跳过 `CGSTickleActivityMonitor`，Chromium/Catalyst 不认。焦点用移植自 yabai 的 "focus-without-raise"（两条 248 字节记录：先 defocus 旧前台、再 focus 目标，**刻意不调** `SLPSSetFrontProcessWithOptions`）。Chromium 还要一个打到屏幕外 `(-1,-1)` 的 primer，用来打开 user-activation 闸门 | 没有私有 SPI 时（macOS 14）Chromium 键盘可能不落地；后台滚动对 Electron/Chromium 直接拒绝；带修饰键的点击必须 `foreground` + `window_id`；安全/画布类界面**没有**后台路径 |
| **Windows** | 后台用 `PostMessage` 打到**最深的子 HWND**（避免顶层 chrome 收到 `WM_LBUTTONDOWN` 去 `SetForegroundWindow`）。已知会被静默丢弃的目标类（Chromium/Electron/GTK/WPF/Tk/VCL）改走 `CreateSyntheticPointerDevice` + `InjectSyntheticPointerInput`（合成笔/触摸）——它走**系统输入队列**而非窗口消息队列，所以不要求目标在前台。目标被遮挡时**直接 bail**（`background_unavailable`），不抬窗；`WS_EX_NOACTIVATE` 守卫 + 事后把用户原先的前台窗口re-assert 回来 | 提权进程会被 UIPI 过滤，而 `PostMessage` **仍然返回 TRUE**，所以在投递前先比完整性 RID 并给诊断；键盘/文本的 cloaked 注入**被删除**了，理由是"后台动作不得抓焦点，隐藏的也不行" |
| **Linux X11** | XI2 **主指针** + `uinput` 从设备（真实内核输入设备，`send_core=1` 让 xterm/Tk 也收得到），warp 主指针，并装一个**设备级同步 grab**——它比 WM 的 click-to-focus grab **更早被检查**，于是那次按下 WM 从没见过，也就不会抢焦点；然后 `XIReplayDevice` 重放 | Xvfb 无 udev 热插拔；Xtigervnc 只暴露内置 VNC/XTEST 设备；**KDE Plasma 6 / Qt 6.11 X11 上 uinput 热插拔会整会话崩溃**，故禁用；无 `/dev/uinput` 时给 `uinput_unavailable`。XSendEvent 兜底对"检查 `send_event` 标记"的程序无效（xterm 默认就忽略） |
| **Wayland** | 客户端拿不到别的窗口的屏幕坐标、读不到全局光标位置、也无法按 surface 投递。所以：GNOME 靠 **Shell 扩展**（`GetRects` 从 `global.get_window_actors()` 取 frame rect；客户端查询会被隐私拒绝）、KWin 靠只读 effect（**完全不注入**，只给几何/pid/标题/token）、Hyprland 靠 **in-compositor 插件**——它自己造 `CWlSeat` 并**直接向目标窗口的 `CWLSurfaceResource` 发 `sendEnter/sendMotion/sendButton`**，绕过 compositor 焦点 | Hyprland 插件 ABI 绑定到**恰好 0.56.2**（运行时比对 git hash 与五个依赖版本）；后台输入只在原生 LibreOffice Calc / Inkscape + evdev 键位下被"资格认证"过，Chromium/Electron/XWayland **未认证**；KWin 路径明确因 TOCTOU 竞态拒绝原始输入；整个原生后端默认关闭，要 `CUA_DRIVER_RS_ENABLE_WAYLAND=1` |

贯穿四平台的口味：**做不到就给结构化拒绝**，不是悄悄退回截图。macOS 的 `degradation_for` 会返回 `degraded` + 原因 + `escalation:{recommended:"px"|"foreground"}`；空 AX 树**故意**返回空并建议升到前台，因为"后台输入在身份未证明时是被拒绝的"。

## 八、给 agent 的接口面

- **默认是 CLI，不是 MCP**。每次 `cua-driver <tool> '<json>'` 拥有一个**一次性 transport session**；只有需要跨调用共享光标/录制/浏览器状态的多步循环才用 `cua-driver mcp`。`mcp-config` 能为 11 种客户端生成配置（claude-code / codex / cursor / openclaw / opencode / hermes / antigravity / factory / qwen / prime-agent / pi）。
- **真正的交互政策在随附的 SKILL.md 里**（1123 行，`cua-driver skills install` 装进客户端）。核心不变量（`:332` 起）：每个动作**必须**被观测包夹；索引按 `(pid, window_id)` 绑定、每次快照都失效；`verify_state` 的 `unknown` 永不等于成功；**"不要让 driver 自己发明任务含义或自动重试"**。它点名的头号失败模式：*"跳过验证的 agent 会在动作被静默丢弃时报告成功。"*
- 升到 `delivery_mode:"foreground"` 被定义为**用户可见的接管边界，不是自动重试**——要么有用户授权，要么先问。
- 另一个 SKILL.md（`skills/gui-automation/`）走 `cua` CLI，政策粗得多（Look → Act → Verify + "每次 UI 变化后重新截图，坐标会过期"），而且**明确教模型做注入 fuzzing**（`type "<script>alert(1)</script>"`）——两个 skill 的安全姿态不在一个层级。
- **机器可读的工契面**：`contract/manifest.json`（已核实：**28 个工具**，`contract_version 0.8.0`），每个工具带 `platforms` / `capabilities` / `annotations{read_only,destructive,idempotent,open_world}` / `schema_mode` / `input_schema` / `success_output_schema`。两种 schema 模式（`CanonicalRuntime` / `PortableSubset`），后者给语言绑定用。
- **拒绝是一等公民**：契约里输出 schema 是 `anyOf: [success, refusal]`，拒绝信封有自己的判别键（`["refusal","status","code"]`）。校验输出 schema 的 MCP 客户端会把拒绝当作**声明过的**结果，而不是协议错误。

## 九、Cua-Bench

- **任务 = 四个装饰器**（博客说三个）：`tasks_config`(配置/variants) / `setup_task` / `solve_task`(**oracle 参考解**) / `evaluate_task`。发现靠**函数属性**标记（`_td_type` / `_td_split`），不是命名或继承——所以任务目录里只要有一个 `main.py`，装饰器怎么命名都行。
- 一个任务目录：`main.py` + `pyproject.toml`(`[tool.cua-bench]`) + `gui/index.html`。
- **evaluator 确实读应用状态**：`await session.execute_javascript(pid, "window.__submitted")` → `[1.0] if submitted is True else [0.0]`。确定性的，不比对像素。部分分是真实的：KiCad 用**网表集合比对**（元件按 `(前缀, 归一化 SI 值, 型号)` 集合、连线按 `(ref, pin)` 集合，忽略编号与网名），接错线也会给约 0.5。
- **`bench-ui` 与文档描述不符**（我核实过其公共面只有三个函数）：没有"一个 Python 文件起界面"的 App API，也没有"嵌入式 JS bridge"。真身是 **pywebview 子进程**（父进程 Popen + 握手一行 `{"pid","port"}`）+ 子进程里 aiohttp 暴露 `POST /eval`，**单向、轮询**，没有 JS→Python 推送；窗口没就绪时返回 409 让父进程重试。而 `native` provider 下它其实是装在**客体机内**、经远程 Python RPC 调用的。
- **轨迹**：累积成一个 HuggingFace `datasets.Dataset`，固定 5 列（`event_name` / `data_json` / `data_images` / `trajectory_id` / `timestamp`）。事件：`reset` / `step:before` / `step:after` / `agent_step` / `solve` / `evaluate`。每步有**全屏 PNG + 动作 repr + 结构化窗口快照（几何/标题/HTML）**，**没有视频**。落盘在 `$XDG_DATA_HOME/cua-bench/runs/<run_id>/`，训练侧直接读同一份格式——**轨迹格式是 Bench 与 RL 的共享契约**。
- evaluator 自身的质量保：**oracle 运行**（`--oracle` 跑 `solve_task`，要求 evaluator 给 1.0）是实现过的；博客说的**对抗运行 + 奖励完整性报告在这份快照里不存在**（已核实：`libs/cua-bench/` 下 `adversarial` 零命中）。
- 体量（调研清点）：3 个数据集 + 3 个集合 ≈ **51 个任务环境目录 / ~322 个变体**，实际 provider 只有 `native` 与 `simulated` 两种。

## 十、Fleets（规模化的那层）

- **`libs/fleet/` 是私有仓库的只读镜像**（CI 有 `fleet-mirror-guard` 阻止直接改）。里面有：CRD schema、Rust 客户端 SDK、Go 反向代理（把 `/api/k8s/...` 反代到 kubectl-proxy 并加 `Impersonate-User`）、OPA 策略、Terraform provider、React SPA。
- **`Pending` 是故意的**：claim 的 `bind_deadline` 默认 **900 s**，注释写明*"A Pending claim is the autoscaler's demand signal, so the claim is kept Pending across a cold VM boot + KEDA scale-up rather than failing fast."* ——**把"等待"当成扩缩容信号，而不是快速失败**，这是这一节最值得记的设计。
- 归池复用靠 `shutdownPolicy: Retain`（默认）：claim 删除后 sandbox **归池并原地重启**，KubeVirt 从 `spec.running:true` 重建 VMI、容器盘换新 overlay = 干净桌面；镜像侧要**观察到 uid 不同的 VMI** 才放行 Resetting→Ready，超时则删 CR。
- **但调度器不在快照里**：pool-operator、claim reaper、KEDA ScaledObject 全都只在注释里被引用；博客说的"延迟缩容"（stabilization window / cooldown）在仓库里**没有实现**，只有 "drain-safe" 一个词。

## 十一、文档与实现对不上（本人核实过的）

| 宣称 | 实际 |
|---|---|
| 博客：evaluator 要"正常跑一次 + 对抗跑一次"，评分器必须奖励前者惩罚后者 | `libs/cua-bench/` 下 `adversarial` **零命中**；只有 oracle 那一半和"难度标定"（每任务跑 5 次算 pass_rate） |
| 博客：在轨迹某步分叉、让模型预测下一个观测并打分 | 只有**重建**那一半（重放窗口快照与动作，且渲染时**剥掉 `<script>`** = 静态视觉复刻）；预测打分不存在 |
| 博客：130 任务 / 42 环境 / 5 平台 | 磁盘上 ~51 个环境目录 / ~322 变体；只有 `native` 与 `simulated` 两种 provider |
| `winarena_adapter` README：173 个任务 | `data/test_all.json` 里实际 **154** 个（12 个应用域，本人清点） |
| README：`--claude-code-computer-use-compat` "只改 screenshot" | 该 flag 在本快照里**已失效**（`let _ = compat;`，本人核实） |
| 契约版本 | crate 常量 `0.8.0`，`compat-fixtures/` 冻结在 `0.12.6`，文档在讲 0.14→0.15 迁移——三者互不相符 |
| `computer-server --width/--height` | 解析了但**全包从未使用**；缩放只在 MCP 层实现 |
| `tasks/slack_env/` | 引用了仓库里根本不存在的 `cua_bench_ui` 模块 |

**教训**：这套代码的注释质量很高（很多"为什么这样"写得比文档好），但**博客和 README 不能当规格**。反过来也说明一件事：注释里那句 `active_lease_conflict: unproven` 是可信的——他们对自己没做到的事会直说。

## 十二、效果：行不行，以及凭什么这么说

**一句话：没有任何第三方或实机采用数据；但它对自己效果的度量，是这份快照里最扎实的部分——而且边界画得很清楚。**

文档层有两件东西，加上可执行的证据本身：

- `docs/test-matrix.md`（223 行）：覆盖**策略**。维度的定义很讲究——OS × 窗口系统 × harness × 寻址方式（`ax`/`px`/`page`）× 投递（`background`/`foreground`）× 作用域（`window`/`desktop`）× oracle（应用状态/辅助功能状态/焦点状态/像素状态/协议状态）× 状态（`pass`/`fail`/`skip`/`environment_error`）× 观测量（`delivered`/`refused`/`no_effect`/`error`/`not_run`）。它明确区分"单元与确定性测试"和"harness E2E"，并要求后者跨过**驱动、OS、窗口系统、应用**四层边界 + 一个**外部 oracle**。
- `docs/action-support.md`（109 行）：**实测台账**，逐行带 run ID。
- 证据本身在树里：`rust/crates/cua-driver/tests/harness_{appkit,wpf,winui3,gtk3,gtk4,swiftui,web,libreoffice}_test.rs` + `cross_platform_behavior_test.rs`，配 `tests/fixtures/apps/{windows,macos,linux,cross-platform}`。

**台账的口径是重点**（`action-support.md:3-15`）：*"derived from typed `CaseSpec` rows and accepted E2E evidence, **not from a successful driver response alone**."*

- **Delivered** = 观察到 fixture 自己拥有的状态变化
- **Refused** = 精确拒绝码 + 全部要求的三方 oracle（焦点 / z 序 / 光标 / 输入泄漏）通过
- **Gap** = 未支持或未证明，且"**缺行永远不算这个动作做不到**"

已接受的基线（都带 run ID）：

| 环境 | 行数 | 结果 |
|---|---|---|
| Windows/Win32 | 122 | 122/122（99 delivered + 23 精确拒绝） |
| macOS/Quartz | 145 | 145/145（138 delivery + 6 拒绝 + 1 个被光标干扰的行单独通过） |
| Linux/X11 | 116 | 116/116（75 delivered + 41 精确拒绝） |
| Linux/Sway | 116 | 36/36（native+capture）+ 80/80（shared） |
| GNOME 46 Wayland | GTK3 31 | 31/31 |
| 嵌套 `cua-compositor` | shared | **仍 10 项失败**，明确标 experimental，拒绝晋级 |

博客说的"500+ 行为检查"就是这些数加起来——不是虚数。

**这个口径最值得学的两点**：① **拒绝也被当成一等成果来验证**——不光要拒绝码正确，还要证明拒绝**没有副作用**（没抢焦点、没改 z 序、没动光标、没漏输入）；② lanes 的 preflight 会**注入故意的焦点与输入违规，要求哨兵能检出两者**，否则不采信任何结果——先证明 oracle 有效，再相信结论。

维护规则（`action-support.md:105-109`）等于把一条原则写成了政策：

> "When an OS API reports success but offers no effect read-back, retain a visible gap rather than inventing a fixture-specific refusal in production code."

**边界，他们自己写明**：

- 全部 E2E 用的是**仓库自带的 fixture 应用**（Electron / Tauri / WPF / WinUI3 / WebView2 / AppKit / SwiftUI / WKWebView / GTK3），不是真实第三方软件。设计如此，但这就是边界。
- PX 行的明确限度（`:83-85`）：*"PX 后台左键行可能把屏幕点解析到一个可操作的 AT-SPI 节点。这样的通过只证明公开的 PX 寻址行为，**不证明**对画布或游戏的原始像素投递。"*
- SwiftUI 那行是个真实应用级缺口的例子：fixture 能证明 `popover_open=true`，但**瞬态面板仍不出现在定向 AX 枚举里**。
- gap 是公开列出的：WinUI3 后台右键/双击、WebView2 原生键盘、macOS AppKit 的原生 press key/hotkey、`background_uipi_blocked`（"今天没有可控的提权 fixture，**不得计为已覆盖**"）、Wayland 的光标保持未证明（issue #2194）。
- KWin 那条不是"没做"而是**明知做出风险后拒绝**：portal/libei 投递是焦点绑定的，激活 + 读回也无法阻止焦点在 compositor 处理前改变，所以在有 target-bound 输入路径之前**故意关闭**。

**benchmark 是另一回事，别混读**：他们的 KiCad 评测（25 题 / 7 个前沿模型 / 最好 6/25 / 空白画布 0/25）说明瓶颈在**任务难度**，不在驱动。前者衡量模型，后者才衡量驱动。

## 十三、本快照未能确定的

- **版本**：快照不是 git 仓库，无法确认对应哪个 release；契约常量 0.8.0 与 fixtures 0.12.6 的矛盾也因此无法定位。
- **Swift 参考实现缺席**：Rust 注释反复说自己是 `Sources/CuaDriverCore/Input/*.swift` 的移植，但 `libs/cua-driver/` 下没有 `Sources/`，无法交叉验证移植的正确性。
- **`confirmed` / `unverifiable` / `suspected_noop` 分类自身的准确率**：跑 E2E 的代码和台账都在树里（见第十二节），但**分类器本身**准不准，只能靠那份台账间接反映——没有一项检查是"给定一次真实动作，问它判对了吗"。
- **Hyprland 的 per-agent lane 模型在 GNOME/KWin 上是否有对应物**：插件只有 Hyprland 版，Rust 侧没有 Mutter 的等价仲裁。
- **被引用但不在快照内的组件**：`docs/decisions/*.md`、pool-operator、claim reaper、`osgym_pool_claim_demand` 的 exporter —— 代码注释里反复出现，实体不在这份树里。
