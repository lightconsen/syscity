# Computer Use 基准接入：OSWorld-Verified 与 Cua Bench

> 状态：M0、M1 已落地（远程桌面适配器 + `syscity bench run`）；M2 的 Python 胶水和 M3 的
> 步进协议都已写完并过了单测，**还没在真 VM 上跑过**；M4、M5 是计划。
> 最后更新：2026-10-29
> 相关文档：[`benchmark.md`](benchmark.md)（GAIA/BFCL 的接入方法与本仓库的基准口径）、
> [`computer-use-benchmark-catalog.md`](computer-use-benchmark-catalog.md)（有哪些基准、
> 哪些本机能跑、kicad/workflows 的实测结果、`/dev/uinput` 与 a11y 树截断这两条硬约束）、
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

一次完整的评测长这样（`lib_run_single.run_single_example`，`run.py` 只是它的批处理外壳）：

```python
env.reset(task_config=example)                     # 还原 VM
agent.reset(vm_ip=env.vm_ip)                       # agent 拿到地址
obs = env._get_obs()                               # 截图 / a11y 树 / 终端
while not done and step_idx < max_steps:
    response, actions = agent.predict(instruction, obs)   # 返回 (文本, 动作列表)
    for action in actions:
        obs, reward, done, info = env.step(action, args.sleep_after_execution)
result = env.evaluate()                            # 任务自己的 evaluator
```

**核对到的上游事实（commit `044bc33`，2026-10-24）**——和本文早先按 README 写的不一样：

- 没有 `Agent` 基类。agent 是**鸭子类型**：只要 `predict(instruction, obs)` 和
  `reset(_logger=None, vm_ip=None, **kwargs)` 存在就行；`PromptAgent`（`mm_agents/agent.py`）
  也是普通类，不是继承来的。
- `predict` 返回**二元组** `(response, actions)`，`actions` 是 pyautogui 代码字符串的列表；
  特殊动作是字符串 `"WAIT"` / `"DONE"` / `"FAIL"`（或 `{"action_type": ...}`）。
- `run.py` 顶部注释说"almost deprecated，用 `run_multienv_*.py`"，但**仓库里没有这些文件**。
  现在能用的就是 `run.py` + `lib_run_single.py`。
- 每题的 `result.txt`、`traj.jsonl`、`step_*.png` 都由 `lib_run_single` 写；基础设施错误走
  `record_example_error`，不记分。
- 桌面服务的端口来自 `env.server_port`（默认 5000，云 provider 会从地址串里解析），
  不要硬编码 5000。

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

已核对的接口（`libs/cua-bench/cua_bench/agents/base.py`，2026-10-29）：

```python
@register_agent("syscity")            # 从 ..agents 导入，名字即 --agent 参数
class SyscityAgent(BaseAgent):
    @staticmethod
    def name() -> str: ...

    async def perform_task(self, task_description: str, session: DesktopSession,
                           logging_dir: Path | None = None, tracer=None) -> AgentResult: ...
```

- `AgentResult(total_input_tokens, total_output_tokens, failure_mode)`，
  `failure_mode` 是 `FailureMode` 枚举：`NONE` / `MAX_STEPS_EXCEEDED` / `API_ERROR` / `UNKNOWN`。
  基础设施问题应报 `API_ERROR`/`UNKNOWN`，不要伪装成"任务做错了"。
- 结果在 `~/.local/share/cua-bench/runs/<run id>/<task>_v<n>/`：
  `result.json`、`trajectory.json`（ATIF v1.8）、`summary.json`（`schema_version: 2`，
  含 `pass_at_k`）。
- `--kind container` 走 gvisor/runc，不需要硬件虚拟化 → **在这台 M2 Pro 上可以真跑**
  （OrbStack）。VM 模式走 qemu/lume。
- 容器 sandbox 需要 `cua-spacesd`；`cb` 本身要先装（以官方 README 为准）。
- `cua-bench-basic` 是 13 题的冒烟集，够判断"接得通、能跑、不崩"，不足以谈能力。

`session` 暴露的是 cua SDK 的 `DesktopSession`（`sandbox` / `spacesd`），**不是** OSWorld 的
HTTP 桌面协议，所以 M4 的胶水要先把 session 接成一个 `syscity bench run --computer-url`
够得着的地址；这一步必须在真 sandbox 上做，不能靠猜——这也是胶水留到 M4 的原因。

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
  "actions": ["import pyautogui\npyautogui.click(100, 56, button='left')\n", "..."],
  "elapsed_ms": 183000,
  "provider": "deepseek",
  "model": "deepseek-v4-flash",
  "error": null,
  "started_at": "2026-10-29T10:00:00Z"
}
```

- 同时 stdout 打印 `### ANSWER ###\n<answer>\n### END ANSWER ###`，方便人肉看日志。
- `actions` 是**发给 VM 的每段 `/execute` 片段，按执行顺序**。它不是调试信息：回放形态的
  胶水就是把它逐条塞回基准自己的 `env.step`。为空时这个字段不出现（`skip_serializing_if`），
  schema 仍是 1。
- `--max-steps` 写进 `SYSCITY_MAX_TOOL_ITERATIONS`。默认 60，与 eval 一致。
- **工具集刻意最小**：`computer`、`screen_state`、`time`、`todo`。
  不注册 `shell` / `file` / `browser` / `web`——它们作用在**宿主机**上，注册了等于允许
  绕过 VM 做事，分数就不可信。这一条写进文档也写进代码注释。
- 会话 id `bench-<task_id>`；`--fresh` 时每次新目录，`--resume` 时跳过已有 `result.json`。

**创建 adapter**：`RemoteDesktop::osworld(url)` → `Arc<dyn ComputerAdapter>` → `ComputerTool::new(Some(adapter))`。
不挂 page actor（VM 里没有 syscity 的 browser pool）。

### 4.3.1 `syscity bench probe`（先问，再花钱）

```bash
syscity bench probe --computer-url http://<vm_ip>:5000 [--execute] [--json]
```

四个端点各问一次，几秒出结果；任何一项失败就非零退出，并在失败行里点名是哪个端点。

| 检查 | 端点 | 失败意味着 |
|---|---|---|
| `screen` | `/screen_size` | 服务连不上，或它不认这个协议 |
| `shot` | `/screenshot` | 截图坏了；尺寸与 `screen` 不一致时给 warning（坐标会偏） |
| `tree` | `/accessibility` | 树取不到/解析不了；**空树只是 warning**（Electron/Canvas 常态，agent 退回坐标档） |
| `execute` | `/execute`（仅 `--execute`） | 服务跑不了 pyautogui——这条不过，`bench run` 一定白跑 |

`--execute` 只跑一句只读的 `print(pyautogui.size())`，而且**不记进动作日志**：`actions` 是
agent 自己决定的动作，探针不是。理由直接写在 `OsworldDesktop::probe_execute` 的注释里。

为什么要它：`bench run` 的失败通常发生在模型已经跑了几轮之后（一次任务 = VM 分钟数 +
token）。探针把这段等待换成 3 秒，并且**先跑再开任务**是 M2 验收的第一步。

### 4.4 `bench/osworld/`（已写完，未在 VM 上验证）

薄胶水，依赖上游 OSWorld checkout（`pip install -e .`），不 vendor 任何东西：

| 文件 | 作用 |
|---|---|
| `sycity_agent.py` | `class SyscityAgent`（回放）与 `class SyscityStepAgent`（步进）：鸭子类型的 OSWorld agent（`predict` + `reset`） |
| `run.py` | 批处理外壳，直接调上游 `lib_run_single.run_single_example`；`--mode replay\|step` 选形态 |
| `test_sycity_agent.py` | 单测：用假 syscity 可执行文件，无 VM、无模型、无 Rust 构建 |

- **回放**：第一次 `predict` 把 instruction 交给 `syscity bench run`（对着
  `http://<vm_ip>:<env.server_port>`），读回结果文件的 `actions` 排进队列；之后每次弹一条，
  队列空时返回 `"DONE"`（`stop_reason == final_answer`）或 `"FAIL"`（预算耗尽/出错）。
  - 优点：一次进程跑完整题，没有几十次进程启动的开销。
  - 缺点：syscity 的观测是它当初决策时的截图，不是 `predict` 当时的 `obs`；涉及
    "等页面加载完再看"的任务会吃亏。报告里必须写明形态。
- **步进**（`--mode step`）：第一次 `predict` 起一个 `syscity bench serve` 并送
  `task`，之后每一轮拿一条 `action` 交回 `env.step`，下一次 `predict` 先回
  `{"type":"result","ok":true}` 再读下一条。syscity 在拿到 `result` 之前**阻塞**在
  发送点，所以它的下一次观测一定发生在 harness 走完这一步之后——这正是要测的顺序。
  - 一个进程一题、一条 `action` 一次 harness 迭代，所以 `harness_steps = max_steps + 10`。
  - 每次读都带 deadline（整题一个 `--timeout`），进程挂住时抛 `SyscityRunFailed` 而不是
    拖住 VM。
- **边界**：syscity 起不来、进程非零退出、结果文件缺失、结果里 `error` 非空、协议里的
  `error` 行——一律抛 `SyscityRunFailed`，走上游的 `record_example_error`，**不计入分母**。
  不返回 `FAIL`，因为在 infeasible 题里 `FAIL` 是满分。两种形态共用这一条。
- 支持 `--split test_small|test_all|test_nogdrive|test_infeasible`、`--meta`、`--domain`、
  `--ids`、`--resume`；报告写 `<result-dir>/summary/syscity.json`，带 `mode` 字段。

怎么跑、每题产出什么文件，见 [`bench/README.md`](../bench/README.md)。

### 4.5 `bench/cua/sycity_agent.py`（M4）

- `@register_agent("syscity") class SyscityAgent(BaseAgent)`，实现 `name()` 与
  `async perform_task(...)`，返回 `AgentResult`（token 数 + `FailureMode`）。
- **地址怎么接出来**（这是 M4 唯一真正的未知，已在真 sandbox 上试通）：
  `cua_bridge.py` 在 guest 里跑一个 HTTP 服务应答 `/screenshot`、`/accessibility`、
  `/screen_size`、`/execute`，`sandbox.tunnel.forward(8765)` 把它暴露到 host，那个 URL
  就是 `--computer-url`。guest 没有 pyautogui，`pyautogui_shim.py` 用 xdotool/xclip 顶上；
  `atspi_xml.py` 把 AT-SPI 树渲染成 `parse_at_spi` 读得懂的 XML。
- **形态是 direct，不是 replay**：`/execute` 真的在 guest 里执行片段，所以 syscity 决策
  时看到的画面就是动作改变的画面上。回放会是更弱的测量（决策时的屏幕和动作生效时的屏幕
  不再是同一块），因此这里刻意不用。
- 轨迹、ATIF、`summary.json` 都由 `cb` 写，不自己造；轨迹通过 `Tracing.record(...)`
  （不是可调用对象）交出去。
- **shim 必须自己验证，不能相信退出码**：xdotool 遇到不认识的键名只往 stderr 打一句
  `No such key name`、退出码仍是 0；`xclip -loops 1` 在 Chromium/GTK 里粘不出东西，因为
  浏览器先问 `TARGETS` 再要正文，一次性的服务端被这一步用光。两者都会让模型"看起来"
  做错了它其实做对了的题——`typing-input` 0/5、`fill-form` 0/3 全部是剪贴板那一处，
  date-picker 里失效的 `page_up` 全部是键名那一处。因此 `_xdotool` 把 `No such key name`
  当失败，`_key_name` 遇到没有显式映射的名字直接抛错，`copy()` 要读回来确认才返回。
  浏览器拖拽还要求中间有足够多的 move 事件，一次 origin→target 的跳跃会被当成点击，
  `drop` 永远不触发，所以 `moveTo(duration=…)` 至少要插值 `DRAG_MIN_STEPS` 个点。
- syscity 起不来 / 跑挂 / 结果里 `error` 非空，一律抛 `SyscityRunFailed`，记成基础设施
  错误而不是"做错了一题"。
- 跑法见 [`bench/README.md`](../bench/README.md)。`PYTHONPATH=$PWD` 是必需的：`cb` 用点分
  路径 import agent，但不会把工作目录放进 `sys.path`。

---

## 五、里程碑

| 阶段 | 内容 | 验收 | 工作量 |
|---|---|---|---|
| **M0** ✅ | `src/computer/remote/{mod,osworld}.rs`；fake HTTP server 单测（截图、a11y XML、执行、过期 id、错误信封） | `cargo test` 全绿；假服务器上能完成"读树 → 按 id 点击 → 截图变化" | 1–2 天 |
| **M1** ✅ | `syscity bench run` + `### ANSWER ###` 契约 | 对着假服务器跑通一次完整 Agent 任务；输出 JSON 可被 `jq` 读 | 0.5 天 |
| **M2** ⏳ | `bench/osworld/run.py` 回放模式（代码写完，单测过；等 VM） | `test_small` 抽 5 题手跑，动作真的落在 VM 上（截图对比 + 人工看） | 1 天 |
| **M3** ⏳ | `syscity bench serve` 步进协议（代码写完，单测过；等 VM）；可选 `pyatspi` ax 动作 | 同一批 5 题在步进模式下重跑，结果与回放模式差异可解释 | 1–1.5 天 |
| **M4** ✅ | `bench/cua/sycity_agent.py` + guest bridge + `cua-bench-basic` | 在真 sandbox 上跑通并出分，`summary.json` 有 `pass_at_k` | 0.5–1 天 |
| **M5** | 全量：OSWorld 369 题 × 1 trial（先 a11y-only），结果进 ledger | 分数、infra 错误、阶梯分布三张表齐全 | 人工触发 |

### M4 实测结果（2026-11-09，本机 arm64 Mac，`--runtime runc`）

`cua-bench-basic` 全量 68 个 variant，`deepseek-v4-flash`，`--max-steps 25`。
两个数，含义不同，不能互相替代：

| | 数 | 说明 |
|---|---|---|
| 原始分 | **63/68 = 0.926**（九次全量区间 0.72–0.93，最近三次 0.910 / 0.912 / 0.926） | `summary.json` 的 `avg_reward`，**报分用这个** |
| 可达分 | **63/63 = 1.000** | 剔除 `drag-drop` 后的模型分；见 `bench/cua/ceiling.json` |
| 环境上限 | 1 个任务（5 variant） | 只有 `drag-drop`，且有机制层面的证据 |

```bash
python3 bench/cua/score.py <run-dir>     # 两个数并列输出
```

**单轮总分不能用来判断一次改动的好坏。** 最早四次全量（设置完全一致）分别得 0.794、
0.794、0.735、0.721，前后差 7 分而中间没有任何改动。逐格比对四轮：65 个格子
只有 **48 个每次都同答案**，17 个至少翻转过一次。四轮多数票合成是 56/65 = 0.8615，
比任何单轮都高——单轮分数**低估**了模型。

所以判断一个修复有没有用，要**重跑它针对的那几个 variant、多跑几次**，不要拿两轮总分
相减。两次差点误判：`click-button` v5/v6 在一轮里 0/2、重跑后 7/7；`right-click-menu`
一轮 2/5、单独重跑三次 3/3。两个都是抽样噪声，不是回归。

**0.838 那一次全量里的 `date-picker` 5/5 是真涨，但涨在 agent 循环上，不是胶水。** 原因是
`src/agent/engine` 里那个 `(工具名, 参数)` 完全相同就跳过的去重保护——它本是为"模型对着
一个失败的 `grep` 死循环"写的，在桌面上是错的：日历"上个月"箭头点第二下是**第二个月**，
`screen_state` 读第二遍是**另一块屏**。现在由 `ToolCapabilities::deduplicate_identical_calls`
逐工具决定，默认 `true` 保住纯读的循环保护，`computer` / `screen_state` 显式设
`false`。见 `bench/cua/README.md` 的 "A repeated action is not a repeated call"。

**最近三轮（`--max-steps 25`、同一二进制）是 0.910 / 0.912 / 0.926。** 前两轮在都计分的 67 个
variant 上只差两格（`fill-form v0` 一过一不过、`typing-input v4` 反过来），刚好抵消；
0.912 看着高 0.002，是因为 0.910 那轮的 `right-click-menu v2` 撞上了一次基础设施错误
（completion stream 中途断开），根本没进分母。从 0.838 到 0.91，变了 5 格、全往上：
`toggle-switch` 的四个变体回来、`typing-input v2` 补上。前者就是下一节说的"页面没加载
样式"那轮环境样本恢复了，不是模型变强。

第三轮（0.926）是 webview 取值修复之后跑的，除 `drag-drop` 外全部通过，原始分和可达分
第一次同为 63/63。**这并不说明那个修复值 1.4 分**：相对 0.912 那一轮，计分的格子里只差
一格（`fill-form v0` 回来）；相对 0.910 那一轮也只差一格（`typing-input v4` 回来，正是
修复针对的那格——0.910 那轮的 `right-click-menu v2` 撞上基础设施错误没计分，不能算进
差异）。而 `fill-form v0` 最近四轮是 0、1、0、1 交替，`typing-input v4` 在 0.912 那轮
本来就过了——两格都在修复前各自的历史范围内。拿修前的代码副本做了三轮对照，
`typing-input v4` 2/3 → 3/3、`fill-form v0` 3/3 → 3/3、p = 1.0，给的也是同一个界限。
修复真正改变的是"打对了字，这次 report 成已验证"，不再掉进看不见 43 字符改动的像素比对里，
因此模型不会在已经落地的文本上重打一遍——同一组对照量到的是树里带值的 turn 从 0/6 到
5/6、p = 0.015（见下文那一节）。所以要说的是"连续三轮 0.91–0.93、可达上限 1.000
（唯一剔除项就是 `drag-drop`）"，而不是把 0.838 当成基线。

**先前给出的 0.927 作废。** 那是按"整题扣除 `date-picker`/`fill-form`/`drag-drop`"
算的，两处都错：

1. **`fill-form` 的 oracle 实际全过（3/3）**，模型也过了 v0/v1 —— 它根本不在上限里。
2. **`date-picker` 的 oracle 五个 variant 全是 0，但模型拿到过 1**（v4 在第一次全量里
   得 1；第二次全量里它退化成 0，而 `video-player` v3 又补上一个 1）。说明日期选择器
   在这个环境里是好用的，坏的是 oracle 自己：`solve_task` 用 `TypeAction` 往
   `input[type=date]` 里打字，WebKitGTK 不认（复跑两次均为 0）。模型则是点开取器、
   按调节按钮选中的。把整题当作环境上限，等于把模型真做出来的题算成"不可达"。

同样的反例还有 `spreadsheet-cell` 和 `video-player`：oracle 为 0 的
`spreadsheet-cell v4` 两次全量模型都过了，oracle 为 0 的 `video-player v2` 第一次也过了。
所以 **oracle=0 不等于环境不可达**，它也可能只是 oracle 坏了。

现在 `ceiling.json` 只收满足三个条件的任务，`score.py` 在报分时逐条校验：

- oracle 在该任务的**每一个** variant 上都是 0（`null` 是"没测"，不是 0，会直接否决）；
- 模型在**任何一个** variant 上都没得 1；
- 失败机制清楚，写进文件。

三条缺一不可。第一条防止按 variant 挑着扣（那正好会专扣模型做错的题，等于作弊），
第二条防止坏 oracle 冒充环境上限（模型做出来了，就证明环境通得过）。

**实测的 oracle 基线**：全量 68 个 variant 的 oracle 平均分 **0.824**（56/68）；
12 个 variant 的 oracle 为 0，其中模型反而解出了 7 个（`date-picker` 全部五个、
`spreadsheet-cell v4`、`video-player v2`）。

`drag-drop`（0/5，oracle 也 0/5）是唯一进入上限的任务。机制确认过：在真实 bench-web
容器里注入页面级监听，`mousedown`/`mousemove`/`mouseup` 都到达页面，`dragstart` 从不
触发，30 个密集插值点也一样。`cua-spacesd` 自己的诊断字符串写着
`WebKitGTK rejects synthetic XSendEvent input`，真实拖拽路径要 `/dev/uinput`，容器里没有。
拖放目标只对 `drop` 事件有反应，没有键盘/点击的绕行路径。

对照 `drag-slider` 5/5：它同样是拖拽，但模型用"点击滑块 + 方向键"绕过，说明任务本身
在键盘下可达——所以它不在上限里。

#### 失败变体的归因

0.72–0.91 原始分下，每轮有 5–19 个变体失败。逐个回放轨迹、并在**真实 bench-web 容器**
里复现验证之后，失败分成几类；但先说一件比分类更重要的事：

**同一轮的总分和失败清单都是抽样。** 最早四次全量（设置完全一致）分别得 0.794、0.794、
0.735、0.721，前后差 7 分而中间没有任何改动。逐格比对：65 个共同格子
只有 **48 个每次都同答案**，17 个至少翻过一次。按多数票合成是 56/65 = 0.8615，
比任何单轮都高——说明单轮分数**低估**了模型，低的那部分正是翻转。

最早那次观察到的"总分稳定、格子不稳定"仍然成立：背靠背两轮都是 0.794，68 个变体里
只有 58 个一致，10 个翻转，正好五上五下：

```
date-picker v4      1 -> 0        right-click-menu v3  0 -> 1
right-click-menu v4 1 -> 0        select-dropdown   v1 0 -> 1
select-dropdown v2  1 -> 0        spreadsheet-cell  v2 0 -> 1
video-player v2     1 -> 0        video-player      v3 0 -> 1
video-player v5     1 -> 0        fill-form         v2 0 -> 1
```

其中两个格子有**已知原因**（`select-dropdown` v1 和 `fill-form` v2 的胶水 bug，已修）；
`click-button` v5/v6 后来也查出是胶水 bug（已修）。其余是 25 步预算下模型提前收工造成的
采样噪声。这组经验后来再次兑现：`click-button` 在一轮里读成 0/2、重跑后 7/7；
`right-click-menu` 一轮 2/5、单独重跑三次 3/3。

所以判断一个修复有没有用，要**重跑它针对的那几个 variant、多跑几次**，不要拿两轮总分
相减。这也是上限必须按**任务**而不是按变体声明的理由：按格子判，第一次运行会得出
"`date-picker` 不可达"，第二次会得出"`right-click-menu` 不可达"，两个结论都过不了对方
那一轮。

分类如下（哪一变体落在哪一类会随运行变化，所以读作"这类失败意味着什么"）：

| 失败类 | 归因 | 证据 |
|---|---|---|
| `drag-drop`（5/5） | **环境**：合成拖拽不可达 | 见上 |
| `video-player` 的音量变体 | 模型用错层 | 要求音量 75%，它开终端跑 `pactl set-sink-volume`，而评测看的是页内播放器，不是 ALSA mixer |
| `fill-form` / `spreadsheet-cell` 的漏（点错位置那类） | 点错位置 | 点偏一格/一个字段，然后报告完成。`fill-form` v0 最近两次的漏是另一类（webview 取值），见下 |
| `click-button` v5/v6 | **胶水 bug（已修）** | 见下 |
| `select-dropdown` v1 | **胶水 bug（已修，重跑得 1）** | 见下 |
| `date-picker` 全部五个 | **agent 循环 bug（已修，5/5）** | 见下 |
| `toggle-switch`（5 个里 4 个） | **环境：页面没加载样式** | 见下 |
| `fill-form` v0 / `typing-input` v4 的漏 | **胶水 bug（已修）：webview 文本框的值从来没进过树** | 见下 |

除**四个**胶水 bug 和一个 agent 循环 bug 之外，这些都不是胶水的锅：是模型提前停止、
点偏格子、或者用错了一层去解题——真实的模型结果，也正是原始分该拿来报的原因。

`typing-input v4`（0.910 那轮）和 `fill-form v0`（0.912 那轮）都是
`MAX_STEPS_EXCEEDED`、reward 0，从里面都报了"按键没干净落地"，一开始记成未定。
实测之后结论是**胶水 bug**，不是 shim、不是页面、也不是模型。

在 `bench-web` 容器里用任务自己的 pywebview 窗口复现：同样的 `pyautogui_shim.write`
连跑六次，43 个字符每次都完整落进 DOM；`fill-form` 四个字段同样都落对了。
没落地的是**读回来的那一半**：

- `atspi_xml.py::_value` 只问 `queryValue()`。WebKitGTK 的 `entry`（任务窗口里每个
  `input`/`textarea` 都是它）在这个方法上直接抛 `NotImplementedError`，真值是
  `queryText()` 才答。于是 webview 文本框在树里**没有任何 value**。
- 这直接关掉了 `type` 验证的树那一路：`typed_value_hint` 要在一个值变过的可编辑字段上
  比对文本，字段全是 `None` 时它返回 `None`，验证退回像素 diff。
- 像素 diff 看不见往短文本框里打字。用真帧量过：43 个字符打进 352×42 的框，按验证器
  的 32×32 块、阈值 30 算是 **0.0%**；清空它（`Delete`）才是 0.9%——失败那轮里唯一
  验证通过的一步，正是把字删掉的那一步。

所以文字早就进去了、屏幕没报出来、模型被告知"no visible change detected"，于是它压在
已有文本上重打：探针里 type 再 retype 正好得到 43+43 个字符，`ctrl+a` 也确实全选，
但一个报"无变化"的 `Home` 会诱出一个只删一个字符的 `Delete`。`#The quick…` 里的 `#`
是模型自己为了排查打的，不是重复按键。

修法是 `atspi_xml.py` 里一处回退：`entry` 和 `text field` 在 `queryValue()` 不可用时
改问 `queryText()`。有两个角色故意排除：静态 `text`（读取器会把它映射成 `text_field`，
给了值每个段落都会变成可编辑字段）和 `password text`（那里 `queryText` 返回明文，会把
凭据写进 turn 日志和轨迹）。date 输入框改前改后都不报值，`date-picker` 路径不受影响。

`fill-form v0` 的 Age 字段同理：`25` 第一次就对了，所谓"重复写入"是对着已经正确的内容
又重打了三次。

**对照组（十二轮，交错执行）。** 每臂只有一次观测说明不了什么，所以做了一组控制：
三轮，每轮先跑修后的 `bench/cua/`、再跑一份修前副本（除 `atspi_xml.py` 回到修复前那个
commit 外，其余文件完全相同），两个 variant 各一次。交错是为了让模型、镜像、宿主的任何
漂移同时落在两臂上。

| | `typing-input v4` | `fill-form v0` |
|---|---|---|
| 修前 | 1、1、**0**（74s、57s、184s） | 1、1、1（120s、148s、128s） |
| 修后 | 1、1、1（35s、80s、31s） | 1、1、1（103s、166s、99s） |

**奖励上这不是提升**：5/6 对 6/6，Fisher 精确检验 p = 1.0。两格修前就在翻，每臂三个
样本动不了一个二成的格子。别拿这张表当涨分证据。

**能量出来的是机制**，而那里分得很干净：

| | 修前 | 修后 | p |
|---|---|---|---|
| 树里带有所键入文本的 turn（`typing-input`） | 0/3 | 3/3 | 0.10 |
| 树里带有所键入文本的 turn（`fill-form`） | 0/3 | 2/3 | 0.40 |
| 两题合计 | **0/6** | **5/6** | **0.015** |
| `type` 步骤验证为 `confirmed` 的比例 | 9/22（41%） | 14/17（82%） | 0.020 |

值在两条读取路径里都会出现——`screen_state` 的 `ui_tree` 段和 `computer` 的
`read_ui_tree` 都是——所以 `fill-form` 是三轮里两轮带值而不是三轮：有一轮根本没读树。

重打循环只出现在修前那一侧。修前失败的那次 `typing-input v4` 跑了 184s、18 次调用：
`type` 报 `suspected_noop`，接着 `ctrl+a`、`ctrl+c`、`clipboard_get`、点一个过期的
element id、`shift+Home`、`BackSpace`，再 `type`、再 `ctrl+c`。修后三次分别是
**3、6、2** 次调用（修前是 8、5、18），均值 105s → 49s。和 0.910 那轮是同一种失败，
现在有对照了。

运行目录：作业临时目录下的 `ab-data/prefix-r{1,2,3}` 和 `ab-data/fix-r{1,2,3}`，
每个 variant 一个 `result.json`。

`date-picker` 不能说成"环境不可达"：oracle 五个 variant 全 0，是因为 `solve_task` 往
`input[type=date]` 键入，WebKitGTK 不认；而模型现在**五个全过**。环境和任务都是好的，
坏的是 oracle。修前模型每轮 0–1 个，修后 5/5——改动在 `src/agent/engine` 的重复调用
守卫，不在 `bench/cua`。

`toggle-switch` 也**不是模型的锅**，但和 `drag-drop` 不同，它不进上限：连续六次全量它
都是 5/5，第七次突然 1/5。原因是那一轮**页面没加载样式**——任务 HTML 在 sandbox 里从
`cdn.jsdelivr.net` 取 Tailwind，取不到时页面照样渲染，只是裸文本。可访问性树里直接
看得到：

```
有样式   button "Toggle notifications" at (785,282) 44x24
无样式   button "Toggle notifications" at (428,353) 16x10
```

16×10 的开关没有可见的开关状态，模型点的是旁边的文字标签。同一轮里 `click-button`
的按钮是一行灰字（有样式时是蓝色两列网格）、`select-dropdown` 是纯文本（有样式时是
卡片），所以这是那一轮的属性，不是任务的。同二进制重跑五个 variant 得 **5/5、
每次一键**；之后两轮全量它也重新是 5/5。

`select-dropdown` 的 bug 是我这边的，值得记下来：折叠状态的 `<select>` 里，AT-SPI 把
**每一个 option 都报告在 combo box 自己的框上**——四个选项坐标完全相同。`atspi_xml.py`
原样输出，模型无从区分，点 "Banana" 实际点在 combo box 上，于是把菜单关掉了。

展开后真实坐标在另一个顶层 `window` → `tree table` → `table cell` 子树里：

```
combo box 'Fruit selection dropdown' (213,187,185,52)
  menu item 'Banana' (213,187,185,52)      ← 折叠态假坐标，仍在树里排在前面
window '' (213,239,185,102)
  table cell 'Banana' (236,291,159,23)     ← 真实位置
```

`real_option_boxes()` 现在把这类 option 的框换成真实那个，规则收得很紧：只有在
combo box 自己框上的、且 popup 里该标签唯一时才替换；popup 关着时（0×0 的 menu）
不产出任何东西，树保持原样。修完后在容器里点一次真实坐标，`<select>` 的值确实变成
了 Banana；该变体重跑得 1。所以 `select-dropdown` 不是环境问题。

`click-button` 是另一个胶水 bug，而且坏了两处。它的窗口是 256×256，按钮网格最多长到
八个，目标可能落在**折叠线以下的第四行**。树里量到第四行的按钮 top 在 575，而窗口的
视口到 557 结束；AT-SPI 仍然给出那一行的布局框——`visible` 但没有 `showing`——按那个
坐标点下去，落在窗口后面的裸桌面上（实际轨迹里模型点的是 y=620/608）。

- `atspi_xml.py` 现在对"`visible` 但无 `showing`"的节点不输出框。元素仍在树里（它
  存在），只是这一次读不到能点的屏幕点。实测八个按钮里六个保留真实框，裁剪行的两个
  失去框。
- `parse_at_spi` 不再把 AT-SPI 的 `active` 当作键盘焦点。`active` 标的是**指针注意力
  所在的窗口**，frame 在内部什么都没聚焦时也带着它。把它读成焦点，意味着**点在窗口
  外**（窗口丢掉 `active`）会被判定成"动作生效"——这正是 y=620 的点击越过 y=557 的
  窗口边界却回报"点了 Close"、而真按钮毫发未动的原因。原生 x11 reader 一直只认
  `STATE_FOCUSED`，两者现在一致；`tree_diff` 另外只在"能拿键盘焦点的角色"上统计焦点
  迁移，免得窗口失去激活被当成变化。

修完之后下一轮全量里 `click-button` 7/7。

`fill-form` v2 曾经的理由是环境问题，其实是胶水问题：模型先滚一次 `scroll(-500)`，再滚
`scroll(-40000)`，然后转去拖滚动条，最后写下"Scrolling via wheel/drag is refused by the
remote endpoint"。端点没有拒绝——syscity 把滚动 `amount` 换算成 `clicks * 100` 个滚轮
click（`osworld.rs:758`），而 `xdotool click --repeat` 默认**每两次之间停 100 ms**。于是
`scroll(-500)` 要 51 秒，超过 HTTP 的 30 秒超时，请求被掐断，模型看到的就是一个失败。
实测（容器内，用 CDP 读 `window.scrollY`）：

```
xdotool click --repeat 500 5            51.07s   scrollY 0 -> 5369
xdotool click --delay 0 --repeat 500 5   0.27s   scrollY 0 -> 5369
```

距离完全一样，Chromium 只是把密集的滚轮事件合并了。`pyautogui_shim` 的 `scroll` /
`hscroll` 现在显式传 `--delay 0`，`scroll(-500)` 从 51 秒降到 0.24 秒。

但修完不会自动变成稳定通过：修完立刻单独重跑三遍 v2 仍然都是 0（有一遍填完并提交了，
但字段值不对），下一次全量里它却是 1、整个任务 3/3。页面现在可达了，剩下的漏是模型的。
这一条也正是上限规则要求"机制必须写清楚"的原因——模型的解释（"端点拒绝"）和真实机制
（"请求超时"）完全是两回事。

另外记录一个**与胶水无关**、但这次一起修掉的真 bug：`sensitive_ui` 把 `"pin"` 当子串匹配，
于是 `spin_button` 命中——date-picker 的月/日/年数字调节器、任何数量框都被标成密码字段，
模型收到"不要在此输入凭据"的警告后放弃输入。改成整词匹配后 `spin_button` 不再误报，
真正的 `pin` 字段仍然命中。这一处在 `date-picker` 上当时直接可见：修前 0/5（且两次打满
25 步），修后 1/5、其余都以 `final_answer` 正常收尾。把 `date-picker` 带到 5/5 的是后来
那个重复调用守卫的修复（见上），不是这一处。


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

### 胶水层（2026-10-29，M2 未验证）

- `bench/osworld/sycity_agent.py`：第一次 `predict` 跑一次 `syscity bench run`，之后逐条回放
  结果文件的 `actions`，队列空时给 `DONE`/`FAIL`。syscity 起不来或报错一律抛
  `SyscityRunFailed`，走上游 `record_example_error`，不进分母。
- `bench/osworld/run.py`：直接复用上游 `lib_run_single.run_single_example`，只换 agent；
  支持 split/domain/ids/resume，产出 `<result-dir>/summary/syscity.json`。
- `bench/osworld/test_sycity_agent.py`：7 个用例，用假 syscity 可执行文件验证命令行、
  回放顺序、`FAIL` 与 `DONE` 的分界、infra 错误不进分母。全部通过。
- 为了回放，`OsworldDesktop` 现在记录每一段 `/execute` 片段，`bench run` 写进结果文件的
  `actions` 字段（可选字段，schema 仍是 1）。
- 用假的 OSWorld 目录（同 `lib_run_single` 接口）端到端跑过一遍：2 题里 1 题成功、
  1 题模拟进程失败 → `scored=1, passed=1, infra_errors=1`，`traj.jsonl` 里是
  `click → write → DONE`。

### 步进层（2026-10-29，M3 未在 VM 上验证）

- `src/bench/serve.rs`：`syscity bench serve`，stdin/stdout 一行一条 JSON。协议是
  `ready` → `task` → (`action` ⇄ `result`)* → `done` | `error`，两手约定都写进模块文档。
  一个进程一题（VM 每题还原，元素 id 只在那份状态里有效）。
- 关键接缝是 `ActionSink`（`src/computer/remote/osworld.rs`）：动作往哪送由它决定，
  默认走 HTTP（回放形态），`bench serve` 换成「写到 stdio 并等 harness 回话」。观测
  （`/screenshot`、`/accessibility`）始终走 HTTP，agent 看到的东西不因形态而变。
- 单测断言了那条不变量：步进模式下一次任务跑完，假 VM 的 `/execute` 计数是 **0**——
  动作全部经过 harness，一条都没直连 VM。另有「一步失败要回给 agent 且整题照常结束」
  和「没收到 task 就断管是错误，不是 0 分」两个用例。
- Python 侧 `SyscityStepAgent`：起子进程、读 `ready`、送 `task`；每次 `predict` 先回上一
  步的 `result` 再读下一条 `action`；`done` 时把结果写进 `result.json`、让子进程退出、
  再按 `stop_reason` 给 `DONE`/`FAIL`。读行在后台线程上做，整题一个 deadline，进程哑了
  就抛错而不是拖住 VM。
- `bench/osworld/run.py --mode step`：换 agent 类、`harness_steps = max_steps + 10`、
  finally 里先 `agent.close()` 再 `env.close()`（别让子进程攥着正在拆的 VM）。
- **stdout 只走协议**：`init_logging` 原来默认写 stdout，那会让每一条 tracing 行都变成
  对端的解析错误。现在 `bench` 子命令的日志一律走 stderr，`tests/bench_serve_stdio.rs`
  用真二进制守着这条（关掉 stdin 后 stdout 必须恰好一行 `ready`，且非零退出）。
- 单测 14 个（Python）全过；假 OSWorld 目录下两种形态各端到端跑过一遍，`mode` 字段在
  报告里。

### 探针（2026-10-29）

- `syscity bench probe`（§4.3.1）：四个端点各探一次，失败点名端点并非零退出；`--json`
  给脚本用。`--execute` 走一句只读的 `pyautogui.size()`，不记进 `actions`。
- 顺带发现并确认一条契约：`/accessibility` 回的是**裸 AT-SPI XML**（`text/plain`），不是
  JSON。拿 JSON 假数据去探会直接被拒——探针的价值就在这里，它把这个错误提前到 3 秒内。
- 单测 5 个（假 VM：健康 / 不执行 / 连不上 / 渲染 / JSON 往返）。真二进制对着本地假服务
  跑过：四项全绿、退出码 0；把树换成 JSON 后退出码 3，失败行写着 `/accessibility`。

**顺序理由**：M0 的适配器是唯一的新代码，也是唯一能在这台机器上验证的部分（假服务器）。
M1 让适配器进入真实 Agent 循环。M2 才需要 VM。M3 修 M2 已知的形态偏差。M4 是第二个
基准，验证"一个适配器"的说法。

---

## 六、环境与前置条件

### 这台开发机（macOS 15.6 / Apple M2 Pro）

这台机器只能当控制面。OSWorld 镜像是 x86_64，M2 Pro 是 arm64，VMware Fusion /
VirtualBox 在 Apple Silicon 上只跑 arm64 客户机，OrbStack 没有嵌套虚拟化；QEMU 的 TCG 能起
x86_64，但慢 10–50 倍，跑 LibreOffice/Chrome 没有意义。结论：**宿主机必须是 x86_64 Linux
且 `/dev/kvm` 可用**。

已验证的反例（2026-10-29）：腾讯云 Ubuntu 22.04 实例（8 vCPU AMD EPYC 9K65、15 GiB、
`systemd-detect-virt` = kvm）——`/proc/cpuinfo` 里 0 个 `vmx|svm`，没有 `/dev/kvm`，
`modprobe kvm_amd` 报 "could not insert"，dmesg 是 "kvm: no hardware support"。普通云主机
默认不开嵌套虚拟化，**租之前先 `ls /dev/kvm`**。

| 需求 | 现状 | 结论 |
|---|---|---|
| VMware Fusion / KVM 跑 OSWorld 镜像 | 都没有（`vmrun` 不存在，Docker 未启） | 换一台 x86_64 + `/dev/kvm` 的机器（裸金属或开了嵌套虚拟化的云主机） |
| Docker（Cua container 模式） | OrbStack 已装并启用 | `--runtime runc` 可用；`gvisor` 需要 `runsc`，本机没装 |
| `cb` / `cua-spacesd` | `cb` 已装（v0.3.1，`uv tool install cua-bench`）；spacesd 不需要 | 已跑通 `cua-bench-basic` |
| `uv` | 有 | — |
| 默认模型 | `deepseek-v4-flash`，无 vision | a11y-only 下限分；视觉口径单列 |

### 两个待定项（实施到 M2 前必须定）

1. **OSWorld 跑在哪**：AWS 官方 AMI（按小时计费，最省事）还是自备 x86_64 + KVM 的机器。
   国内云主机的普通实例不给嵌套虚拟化（见上），要么开嵌套虚拟化的机型/裸金属，要么做
   香港/海外 VPS。`test_all` 369 题串行要几十小时，本机的睡眠/网络都会打断，所以不放在
   开发机上。装机、验收、跑任务的完整步骤见
   [osworld-kvm-server.md](osworld-kvm-server.md)。
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
