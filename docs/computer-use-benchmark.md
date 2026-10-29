# Computer Use 基准接入：OSWorld-Verified 与 Cua Bench

> 状态：M0、M1 已落地（远程桌面适配器 + `syscity bench run`）；M2–M5 是计划。
> 最后更新：2026-10-29
> 相关文档：[`benchmark.md`](benchmark.md)（GAIA/BFCL 的接入方法与本仓库的基准口径）、
> [`modules/computer.md`](modules/computer.md)（动作阶梯、五种效果、后台投递）、
> [`eval-status.md`](eval-status.md)（评测方法论）。

本文回答一个问题：**怎么把 Computer Use 的能力放到外部基准上评，而不是只靠 5 个本地场景。**

结论先给：**syscity 只做"远程桌面客户端"。** 实现一个说 OSWorld VM HTTP 协议的
`ComputerAdapter`，基准侧用一个薄 Python 胶水把任务喂进来。Agent 循环、动作阶梯、
元素寻址、效果验证都不改——它们正是要评的东西。同一份适配器同时服务 OSWorld 和
Cua Bench，因为两者最终都是"截图 + 动作 + 切换上下文"。

---

## 〇、先看这里

### 现在能测什么

`tests/gui_baseline.rs` 是唯一的真实 GUI 回归：5 个场景，headless Chrome，跑的是**页面这一档**
（按 role + name 找元素、点、填、拒绝不存在的名字）。它快、可进 CI，但它回答的是
"元素寻址对不对"，不是"一个跨应用任务能不能完成"。

### 两个基准各补什么

| 基准 | 形态 | 规模 | 覆盖 | 接入角色 |
|---|---|---|---|---|
| **OSWorld-Verified** | Ubuntu 虚拟机 + 真实桌面应用 | `test_all` 369 题 / 10 个域；`test_small` 39；`test_nogdrive` 361 | 端到端任务：浏览器、LibreOffice、GIMP、VS Code、终端、文件管理、VLC、Thunderbird | 主分数（整条栈） |
| **Cua Bench** | 容器或 VM 里的 taskset，`cb` 调度与判分 | `cua-bench-basic` 13 题 | 轻量桌面任务，官方跑法统一 | 回归档（改动后 1 小时内可跑完） |

两者都用**官方 evaluator** 判分。syscity 不写判分器——OSWorld-Verified 本身就是
"修 evaluator 的社区分支"，自己再写一套只会得到不可比的数字。

### 一句话架构

```
Python（基准仓库）                         Rust（syscity）
┌────────────────────────┐  每个任务一次   ┌──────────────────────────────┐
│ OSWorld env / Cua cb   │ ───────────────▶│ syscity bench run             │
│  （起 VM / 判分 / 轨迹）│                 │  Agent（computer + 观测工具） │
└───────────┬────────────┘                 │   └ ComputerSession（阶梯）    │
            │ HTTP（VM 桌面服务）           │       └ RemoteDesktopAdapter   │
            └──────────────────────────────┴────────────── HTTP ───────────┘
```

---

## 一、目标与非目标

### 目标

1. **官方 evaluator 判分**，不自写。
2. syscity 以**完整 Agent** 参评：系统提示、工具描述、动作阶梯、迭代预算都在场内。
3. **一个适配器，两个基准**。接入第二个基准只写胶水，不改适配器。
4. 回归可跑：`cua-bench-basic` 13 题在 1 小时内跑完；OSWorld 全量人工触发。
5. 结果进 `evals/ledger.md`，与 GAIA/BFCL 同一套记录口径（模型、commit、基准版本、样本数）。
6. **动作阶梯的分布本身是指标**：哪种动作走 `ax_action`、多少落到 `pixel`、多少 `refused`，
   比总分更能指导下一轮改动。

### 非目标

- **不做 leaderboard 提交门禁**（与 `benchmark.md` 同一条）：波动大、外部依赖多。
- **不把"模型直接写 pyautogui"当作接入方式**。那测的是代码生成和坐标猜测，绕开了阶梯、
  元素 id、效果验证——恰好是 syscity 相对裸模型的差异所在。
- **不在 M0–M2 追求视觉模型分数**。默认模型 `deepseek-v4-flash` 没有 vision
  （`default_supports_vision = false`），先跑 a11y-only 的下限分；视觉模型的口径作为
  第二个数字单列，不混着比。
- **不 vendor 上游仓库**。`bench/osworld/` 与 `bench/cua/` 只放胶水，依赖上游 `pip install`。

---

## 二、两个基准的实际契约

> 事实核对于 2026-10-29。两个上游都在快速迭代，**接入前用源码/`-h` 再对一遍**，以本节的
> 表为检查清单。

### 2.1 OSWorld-Verified

一次完整的评测长这样（`run.py` 的结构）：

```python
obs = env.reset(task_config)                 # 起/还原 VM，返回初始观测
score = 0
while not done:
    action = agent.predict(instruction, obs)  # 你的 agent 说下一步做什么
    obs, reward, done, info = env.step(action, sleep=0.5)
score = env.evaluate()                        # 每个任务自己的 evaluator
```

对 syscity 来说，`predict` 需要的能力只有三样：**看**（截图、可访问性树）、**做**
（点、输入、按键、滚动、拖拽）、**等**。这三样正好是 `ComputerAdapter` 的接口。

VM 里跑着一个 HTTP 桌面服务，`env.vm_ip` 给出地址。syscity 要说的话：

| 端点 | 方法 | 请求 | 响应 | 对应 `ComputerAdapter` |
|---|---|---|---|---|
| `/screenshot` | GET | — | `{"screenshot": "<b64 png>", ...}` | `screenshot` |
| `/accessibility` | GET | — | `{"AT": "<AT-SPI XML>", ...}` | `read_ui_tree` |
| `/screen_size` | POST | `{}` | `{"output": {"width": W, "height": H}}` | `display_geometry` |
| `/execute` | POST | `{"command": "<python>", "shell": false}` | `{"output": "...", "error": ...}` | `execute` |

要点：

- **`/execute` 是 RPC，不是让模型写代码。** 模型说"点 Save 按钮"，由 session 决定用元素 id、
  坐标还是键盘，适配器再把它编成一段 pyautogui 片段发过去。
- 响应都带 `output` / `error` 信封。`error` 非空就是失败；`output` 有时是字符串化 JSON，
  解析要容错。
- `/accessibility` 给的是 **AT-SPI XML**（Linux 的可访问性树），节点带 `role`、`name`、
  `description`、`states`、`value` 和边界框。这是 a11y-only 路线的观测来源。
- 动作空间与 pyautogui 对齐：`click(x, y)`、`write("...")`、`hotkey("ctrl","c")`、
  `scroll(dy)`、`dragTo(...)`。键名用 pyautogui 的拼写，不是 X11/evdev 的。

**环境要求**：官方支持 VMware / VirtualBox / AWS / Azure / Docker(KVM)。这台 macOS 上
VMware Fusion 与 KVM 都没有 → 见 §六。

### 2.2 Cua Bench

跑法：

```bash
cb run cua-bench-basic \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --kind container --runtime gvisor \
  --attempts 1 --retries 1
```

- agent 接口是 `BaseAgent.perform_task(task_description, session, logging_dir, tracer)`：
  基准给一个 `session`（截图 / 点击 / 输入），agent 做完返回。
- 结果在 `~/.local/share/cua-bench/runs/<run id>/<task>_v<n>/`：
  `result.json`、`trajectory.json`（ATIF v1.8）、`summary.json`（`schema_version: 2`，
  含 `pass_at_k`）。
- `--kind container` 走 gvisor/runc，不需要硬件虚拟化 → **在这台 M2 Pro 上可以真跑**
  （OrbStack）。VM 模式走 qemu/lume。
- 容器 sandbox 需要 `cua-spacesd`；`cb` 本身要先装（以官方 README 为准）。
- `cua-bench-basic` 是 13 题的冒烟集，够判断"接得通、能跑、不崩"，不足以谈能力。

`session` 的具体方法名（`screenshot` / `click` / `type` / `key`）随版本可能变；胶水层以
运行时 introspection 为准，不硬编码。

---

## 三、设计

### 3.1 为什么是适配器，不是"模型写动作代码"

syscity 的 Computer Use 有四个档：

| 档 | 机制 | 远程桌面里对应什么 |
|---|---|---|
| `ax_action` | AT-SPI `doAction` / `setValue` | `/execute` 里跑 `pyatspi`（M3 可选） |
| `pixel` | 按元素中心或坐标点 | `pyautogui.click(x, y)` |
| `keyboard` | 聚焦元素后按键 | `pyautogui.click` + `hotkey` |
| `page` | 浏览器 CDP | VM 里没有 syscity 的 browser pool，返回"不可用" |

模型面对的是 `computer` 工具（角色/名字/元素 id），不是 pyautogui。这条边界必须守住，
否则基准测的是另一个产品。

### 3.2 每次任务一个新适配器

OSWorld 的 `env.reset` 会把 VM 还原到任务初始状态。适配器的 `SnapshotCache`、截图缓存都
绑在"这一次桌面会话"上，所以 `bench run` 每次新建 adapter，不复用进程内状态。元素 id
（`s<snapshot>:<path>`）只在一次任务内有意义——这与本地行为一致，id 本来就是快照内的地址。

### 3.3 观测与坐标

- 截图由 VM 给，分辨率就是 `/screen_size` 报告的屏幕像素；pyautogui 的坐标也是同一套像素。
  因此 `display_geometry` 返回单个 `0,0,W×H` 显示器，`CoordinateSpace` 退化成恒等映射。
- 若某镜像开了 HiDPI 导致截图尺寸 ≠ 屏幕尺寸，`CoordinateSpace` 会算缩放；`screen_state`
  工具会在输出里写清楚，模型按映射点。
- **截图缓存必须被动作失效**：阶梯用"动作前后两帧是否相同"判断 `SuspectedNoop`。缓存
  不失效，after 帧就是 before 帧，每次成功的点击都会被判成空操作。

### 3.4 判定失败的边界

| 情况 | 适配器返回 | 阶梯的反应 |
|---|---|---|
| HTTP 连不上 / 超时 | `ToolFailed` | 重试一次，仍失败则任务失败（infra error，单列） |
| `/execute` 回 `error` 非空 | `ToolFailed` | 动作失败，模型看到原因 |
| 动作本身不支持（窗口管理、进程） | `UnsupportedPlatform` | 该档不可用，落到下一档 |
| 元素 id 过期 | `StaleElement` | 要求重读树，不猜坐标 |
| a11y 树为空 | `Ok(vec![])` | 模型只用截图 + 坐标（真实的降级，不是错误） |

---

## 四、实现

### 4.1 `src/computer/remote/mod.rs`

```rust
pub struct RemoteDesktopConfig {
    pub base_url: String,          // http://<vm_ip>:5000
    pub auth_token: Option<String>,// Bearer；自建服务用
    pub request_timeout: Duration, // 30s
    pub screenshot_ttl: Duration,  // 250ms（被动作失效）
}

pub struct RemoteDesktop {
    client: reqwest::Client,
    config: RemoteDesktopConfig,
    snapshots: SnapshotCache,      // 容量 8，和本地适配器一致
    screenshot: tokio::sync::Mutex<Option<(Instant, Screenshot)>>,
}
```

- 不实现"远程传输层"抽象。HTTP 够用；VNC/RDP 是另一条产品线（`remote_control` 已有），
  不要为了复用把它硬套进来。
- `resolve_target` 复制本地适配器的规则：平台 id / 无快照的 id 先懒读一次树；快照 id 不懒读，
  过期就是过期（`ResolveError::Stale → ComputerError::StaleElement`）。

### 4.2 `src/computer/remote/osworld.rs`

**解析**

- 信封：`{"output": ..., "error": ...}`；`output` 可能是对象、字符串或字符串化 JSON。
- PNG 尺寸：直接读 IHDR（前 24 字节），不引 `image` 依赖。
- AT-SPI XML：`roxmltree`。节点名不可靠（`<frame>`、`<push-button>` 都见过），**只看属性**。
  边界框接受两种写法：属性 `x/y/width/height`，或子节点 `<bbox x= y= width= height=/>`。
  `states` 里的 `"enabled"` / `"focused"` 决定 `UiElement::enabled/focused`。

**角色映射**（AT-SPI → syscity 词汇；session 按键激活用它）

| AT-SPI role | `UiElement::role` |
|---|---|
| `push button` / `toggle button` | `button` |
| `entry` / `text` / `password text` | `text_field` |
| `check box` | `checkbox` |
| `radio button` | `radio_button` |
| `combo box` | `combo_box` |
| `menu item` / `check menu item` / `radio menu item` | `menu_item` |
| `page tab` / `page tab list` | `tab` / `tab_group` |
| `frame` / `dialog` | `window` |
| `label` | `static_text` |
| `link` | `link` |
| `slider` / `spin button` | `slider` / `spin_button` |
| `table cell` / `table row` | `table_cell` / `table_row` |
| 其他 | 空格换下划线，原样保留 |

**动作 → Python**

| `DesktopAction` | 生成的片段 |
|---|---|
| `Click{Coordinate(p)}` | `pyautogui.click(x, y)` |
| `Click{Element*}` | 先 `resolve_target` 取元素中心，再 `click(x, y)` |
| `DoubleClick` | `pyautogui.doubleClick(x, y)` |
| `Type{text}` | `pyperclip.copy(t); pyautogui.hotkey('ctrl','v')`，`ImportError` 时退 `pyautogui.write(t)` |
| `KeyPress{keys}` | `pyautogui.hotkey('ctrl','c')`（单键则 `press`） |
| `Scroll` | `pyautogui.scroll(dy, x=x, y=y)`（down 为负） |
| `Drag` | `moveTo` → `mouseDown` → `moveTo` → `mouseUp` |
| `Wait{ms}` | 本地 `sleep`，不发 RPC |
| `ClipboardGet/Set` | `pyperclip.paste()/copy()`；get 打印到 stdout |
| 窗口 / 进程 / 系统状态 | `UnsupportedPlatform`（拒绝，不是静默成功） |

键名：先过 `canonical_key`，再映射到 pyautogui 拼写（`enter→enter`、`escape→esc`、
`backspace→backspace`、`cmd→ctrl`（Linux VM 没有 ⌘））。

**文本输入的一个坑**：`pyautogui.write` 只能打 ASCII，且逐字符有键盘延迟。中文/长文本走
剪贴板粘贴；这也是本地适配器 `type_text` 的思路。粘贴前不额外聚焦——`Type{target}` 的
聚焦由 session 先做一次 `Click`。

**能力声明**

- `perform_ax_action` → `UnsupportedPlatform`（M0）。M3 可选：`/execute` 里用 `pyatspi`
  按 `path` 找到对象执行 `doAction("click")`，让 `ax_action` 档在远程也成立。
- `supports_background_input` → `false`。VM 是独占的，没有"别人在用"的语义。
- `perform_background` → 拒绝（默认实现），理由同上，不要假装支持。

### 4.3 `syscity bench run`

```text
syscity bench run \
  --instruction "Open LibreOffice and export the document to PDF" \
  --computer-url http://127.0.0.1:5000 \
  --task-id 7d8a1c \
  --max-steps 60 \
  --out /tmp/traj/7d8a1c/result.json \
  [--provider deepseek] [--model deepseek-v4-flash] [--api-key ...] [--base-url ...]
```

**输出契约**（Python 胶水只依赖这个）：

```json
{
  "schema": 1,
  "task_id": "7d8a1c",
  "instruction": "...",
  "answer": "final assistant text",
  "stop_reason": "final_answer | max_steps | error",
  "steps": 17,
  "tool_calls": 24,
  "elapsed_ms": 183000,
  "provider": "deepseek",
  "model": "deepseek-v4-flash",
  "error": null,
  "started_at": "2026-10-29T10:00:00Z"
}
```

- 同时 stdout 打印 `### ANSWER ###\n<answer>\n### END ANSWER ###`，方便人肉看日志。
- `--max-steps` 写进 `SYSCITY_MAX_TOOL_ITERATIONS`。默认 60，与 eval 一致。
- **工具集刻意最小**：`computer`、`screen_state`、`time`、`todo`。
  不注册 `shell` / `file` / `browser` / `web`——它们作用在**宿主机**上，注册了等于允许
  绕过 VM 做事，分数就不可信。这一条写进文档也写进代码注释。
- 会话 id `bench-<task_id>`；`--fresh` 时每次新目录，`--resume` 时跳过已有 `result.json`。

**创建 adapter**：`RemoteDesktop::osworld(url)` → `Arc<dyn ComputerAdapter>` → `ComputerTool::new(Some(adapter))`。
不挂 page actor（VM 里没有 syscity 的 browser pool）。

### 4.4 `bench/osworld/run.py`

薄胶水，依赖上游 OSWorld checkout（`pip install -e .`）：

- `class SyscityAgent(Agent)`：实现 `predict(instruction, obs)`。
- **M2 形态（回放）**：第一次 `predict` 时把整条 instruction 交给 `syscity bench run`，
  跑完后把 agent 记录下来的每一步（`click/type/key/scroll/wait`）排进队列；之后每次
  `predict` 弹一条，转成 pyautogui 片段返回，队列空则 `{"action_type": "DONE"}`。
  - 优点：一次进程跑完整题，没有 50 次进程启动的 1–2 分钟开销。
  - 缺点：syscity 的观测是它当初决策时的截图，不是 `predict` 当时的 `obs`；涉及
    "等页面加载完再看"的任务会吃亏。**这一条必须在报告里写明。**
- **M3 形态（步进）**：`syscity bench serve --computer-url ...` 用 stdin/stdout JSON-lines：
  `{"obs": {...}}` → `{"action": ...}`。步进语义与 OSWorld 完全对齐，回放模式的偏差消失。
  两个形态共用适配器，只换外层。
- 支持 `--split test_small|test_all|test_nogdrive|test_infeasible`、`--domain`、`--ids`、
  `--parallel`、`--resume`。每题写 `result.txt`（OSWorld 约定）与 `traj.jsonl`。
- infra 错误（VM 起不来、服务 500、超时）单独计 `infra_errors`，**不计入失败率的分母**，
  但在报告里单列——否则一次网络抖动看起来像模型变笨。

### 4.5 `bench/cua/sycity_agent.py`

- `SyscityAgent(BaseAgent)`，`perform_task` 里：
  - 优先走 `syscity bench serve` 的 stdio 协议（M3）；
  - 否则 `syscity bench run` 一次跑完，再按轨迹回放（M2）。
- 轨迹、ATIF、`summary.json` 都由 `cb` 写，不自己造。
- `session` 的方法名用 `getattr` 探测（`screenshot`/`click`/`type_text`/`press_key`），
  上游改名时只改一个映射表。

---

## 五、里程碑

| 阶段 | 内容 | 验收 | 工作量 |
|---|---|---|---|
| **M0** ✅ | `src/computer/remote/{mod,osworld}.rs`；fake HTTP server 单测（截图、a11y XML、执行、过期 id、错误信封） | `cargo test` 全绿；假服务器上能完成"读树 → 按 id 点击 → 截图变化" | 1–2 天 |
| **M1** ✅ | `syscity bench run` + `### ANSWER ###` 契约 | 对着假服务器跑通一次完整 Agent 任务；输出 JSON 可被 `jq` 读 | 0.5 天 |
| **M2** | `bench/osworld/run.py` 回放模式 | `test_small` 抽 5 题手跑，动作真的落在 VM 上（截图对比 + 人工看） | 1 天 |
| **M3** | `syscity bench serve` 步进协议；可选 `pyatspi` ax 动作 | 同一批 5 题在步进模式下重跑，结果与回放模式差异可解释 | 1–1.5 天 |
| **M4** | `bench/cua/sycity_agent.py` + `cua-bench-basic` | 13 题跑完，`summary.json` 有 `pass_at_k` | 0.5–1 天 |
| **M5** | 全量：OSWorld 369 题 × 1 trial（先 a11y-only），结果进 ledger | 分数、infra 错误、阶梯分布三张表齐全 | 人工触发 |

### 已经落地的部分（2026-10-29）

- `src/computer/remote/{mod,osworld}.rs`：`OsworldDesktop` 实现 `ComputerAdapter`，单测里用一个
  axum 假 VM 覆盖 `/screenshot`、`/accessibility`、`/screen_size`、`/execute`（缓存与失效、
  元素 id 解析、过期 id、错误信封、等待条件、角色映射、动作代码生成）。
- `src/bench/{mod,run}.rs` + `src/cli/bench.rs`：`syscity bench run`，输出 §4.3 的 schema-1
  JSON 与 `### ANSWER ###`。单测用 `MockProvider` + 假 VM 跑完整任务，断言工具集只有
  `computer/screen_state/time/todo`、四个工具都预授权、计数来自线程日志、结果文件可解析。
- 真实模型冒烟（`deepseek-v4-flash`，假 VM，预算 6）：3 轮、3 次工具调用。按元素 id
  `s00000001:0.0.0.0` 点击，实际落到 `pyautogui.click(100, 56)`（按钮中心）；`ax_action` 档
  不可用后阶梯降到 `pixel`，动作后验证为 `suspected_noop`——假 VM 画面本来就不会变，这个
  判定是对的。

**顺序理由**：M0 的适配器是唯一的新代码，也是唯一能在这台机器上验证的部分（假服务器）。
M1 让适配器进入真实 Agent 循环。M2 才需要 VM。M3 修 M2 已知的形态偏差。M4 是第二个
基准，验证"一个适配器"的说法。

---

## 六、环境与前置条件

### 这台开发机（macOS 15.6 / Apple M2 Pro）

| 需求 | 现状 | 结论 |
|---|---|---|
| VMware Fusion / KVM 跑 OSWorld 镜像 | 都没有（`vmrun` 不存在，Docker 未启） | OSWorld 跑在 **AWS**（官方 AMI）或装 Fusion（许可成本） |
| Docker（Cua container 模式） | OrbStack 已装、未启 | 启用后可用 `--runtime gvisor` |
| `cb` / `cua-spacesd` | 未装 | M4 前 `uv tool install`（以官方 README 为准） |
| `uv` | 有 | — |
| 默认模型 | `deepseek-v4-flash`，无 vision | a11y-only 下限分；视觉口径单列 |

### 两个待定项（实施到 M2 前必须定）

1. **OSWorld 跑在哪**：AWS 官方 AMI（按小时计费，最省事）还是本机装 VMware Fusion。
   推荐 AWS：`test_all` 369 题串行要几十小时，本机的睡眠/网络都会打断。
2. **主分数用哪个模型**：`deepseek-v4-flash`（a11y-only，便宜，可复现）还是接一个带
   vision 的模型（官方口径，贵）。建议**两个都记**，报告里分开列，不混着比。

---

## 七、指标

### 外部分数

| 指标 | 口径 |
|---|---|
| OSWorld 成功率（总 + 按 domain） | 官方 evaluator，`env.evaluate()` 返回 0/1 |
| Cua `pass@1` / `pass@k` | `summary.json` 的 `pass_at_k` |
| infra 错误 | VM 起不来 / 服务 5xx / 超时；单列，不进分母 |

### 内部信号（从 syscity 轨迹里统计）

| 指标 | 来源 | 用途 |
|---|---|---|
| 阶梯分布 `ax_action/pixel/keyboard/page` | 工具结果里的 `evidence.summary()` | 看哪种档是主力、哪种从没成功 |
| `refused` / `partial` 占比 | 同上 | 拒绝率高说明元素寻址或角色映射有问题 |
| 每任务步数、token、墙钟 | `bench run` 输出 + provider usage | 成本与效率 |
| 元素找不到的次数 / 重读树次数 | `ComponentNotFound`、`StaleElement` | a11y 树质量 |

### 与本地 baseline 的分工

`tests/gui_baseline.rs`（秒级、CI 可跑）守"页面档"的正确性；OSWorld/Cua（小时级、人工触发）
回答"整条栈能不能完成任务"。前者回归，后者定位。

---

## 八、风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 上游 API 漂移（OSWorld 端点、`cb` 参数、`session` 方法） | 胶水失效 | 契约集中在一张表 + 一个映射函数；漂移只改一处；接入前重新核对 |
| 回放模式与步进语义不一致 | 涉及等待/异步加载的任务被低估 | M3 做步进；M2 报告里标注形态 |
| 部分应用 a11y 树为空（Electron/Canvas） | 这些题分数低，但不是代码 bug | 退化为坐标 + 截图，属真实降级；报告里按应用分组看 |
| 截图像素差受光标/动画影响 | 把成功判成 `SuspectedNoop`，触发无谓升级 | 动作后先等 200ms 再截；必要时忽略指针附近区域 |
| evaluator 自身有 bug | 分数不可比 | 记录上游 commit；OSWorld-Verified 就是修复版，优先用它 |
| 成本 | 369 题 × 3 trials + VM 小时 | 先 1 trial、先 `test_small`、夜里跑；预算写进 runner 参数 |
| 模型无 vision | 依赖视觉的题注定失败 | 明确标注 a11y-only，不和视觉模型混比 |

---

## 九、执行顺序

```
M0 远程桌面适配器（假服务器可验证）      ← 唯一的新代码
  └─ M1 bench run（接进真实 Agent 循环）
       ├─ M2 OSWorld 回放（需要 VM） ── 拿到第一个外部数字
       │    └─ M3 bench serve（步进，修形态偏差）
       └─ M4 Cua Bench（验证"一个适配器"）
            └─ M5 全量 + ledger
```

先做 M0 和 M1：它们不依赖任何 VM，能在这台机器上完整验证。VM 环境（§六）可以在 M2 之前
并行准备。
