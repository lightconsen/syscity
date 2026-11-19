# Computer Use — 从"会点按钮"到"目标进、结果出"

> 状态：A–F 全部落地，含 B′（成本）、B″（视觉）、D′（学习闭环）、D″（并行边界）、
> E（录放）、G 的"看"这一半。**隔离与 trajectory 级策略学习明确不做**（§7）。
> 本文替代 `computer-todo.local.md`（已删除）。原计划里尚未落地的条目收集在 §6。

这是一份**实现现状 + 剩余缺口**的文档，不是一份新计划。计划本身的论证过程
（为什么先堵安全洞、为什么 B′ 不能和 B 拆开交付）保留在下面的"为什么"段落里，
但每条结论现在都附带它在 `src/` 里的落点。

---

## 0. 一句话结论

Computer use 的**单步质量**早就是第一梯队：动作效果三态验证、失败沿
`ax_action → pixel → keyboard → page` 逐级升级、自动回滚、敏感界面识别、
跨进程桌面输入锁。原计划的判断是"缺的不是会不会点，而是把写好的能力接上生产"。

这件事已经做完。今天的状态是：

- **一个目标进、结构化结果出的闭环**：`computer.run` / `computer_run` 工具 →
  后台 run → `computer.status` / `computer.step` 事件 → 交接单。
- **成本受控**：文本优先的感知策略、按图片预算选的触发条件、run 级调用/图片上限、
  截图内容去重。
- **过程可查**：`turns.replay` 把一轮的动作和它启动的 run 的步骤并成一条时间线；
  Web UI 的右侧分栏面板把当前会话的 run 逐步画出来（动作、截图、成本，只读），
  消息操作栏的"回看"按钮把**一轮**的动作按顺序播放（同样是只读）。
- **跨重启可续**：检查点落盘，重启只提醒不自动续跑，人确认后从第 N+1 步接着走。
- **越用越好**：失败 → badcase → 人工确认终态 → 回归用例 → shadow 验证 → 回滚。
- **能录能放**：录制 `computer` 工具的成功动作，抽参数，回放前做歧义预检。

真正剩下的能力上限（视觉）、以及明确排除的东西（隔离、策略学习），见 §7。

---

## 1. 模块地图

| 文件 | 作用 |
|---|---|
| `src/computer/use_loop.rs` | perceive → decide → act → verify 的循环骨架；三态验证、熔断 |
| `src/computer/driver.rs` | 把 `LoopState` 渲成决策提示，问 provider，解析成 `LoopDecision`；文本优先、解析失败 = `Unverifiable` |
| `src/computer/run_executor.rs` | 一次 run 的执行：预算、感知规划、截图去重、事件、交接单（`execute` 在 `:204`） |
| `src/computer/run_budget.rs` | `StopReason`（`done` / `max_steps` / `budget` / `need_help` / `cancelled`）、`ImageTrigger` 四条触发条件、run 级计数器 |
| `src/computer/run_store.rs` | `~/.syscity/computer-runs/<run_id>.json` 检查点；`load_for_turn` 是 `turns.replay` 的连接键 |
| `src/computer/run_registry.rs` | 进程内活着的 run；`computer.status` / `computer.control` 的数据源 |
| `src/computer/handoff.rs` | 没完成就停下时给人的四段交接单，同时投进审批队列 |
| `src/computer/failure.rs` | run 失败 → `RunFailureRecord`（badcase），不碰数据库也不碰桌面 |
| `src/computer/tree_budget.rs` | 屏外窗口折叠成一行 + 按 focus/面积重排，让行预算先给主窗口 |
| `src/computer/image_budget.rs` | 给**模型上下文**压缩截图（1024px / JPEG 75），与按带宽压缩的 `screenshot_encoder` 分开 |
| `src/computer/vision/resolver.rs` | `VisionResolver`：`auto` / `local` / `api` / `off` 四模式，不可用时明确说原因 |
| `src/computer/vision/look.rs` | 触发视觉时的那一次扫描，产物是**语义元素**而不是裸坐标 |
| `src/computer/app_scope.rs` | `[computer] allow_apps` 的边界，在派发之前拒绝 |
| `src/computer/replay_preflight.rs` | 回放前的歧义预检（`REPLAY_AMBIGUOUS`） |
| `src/computer/recording.rs` | 录制的视频旁路：一录一个 ffmpeg 进程，不是整段存内存 |
| `src/computer/watch.rs` / `watch_registry.rs` | `computer.watch`：只看不动，帧事件广播 |
| `src/tools/computer_run.rs` | agent 侧的 `computer_run` 工具：从 trace 读 `turn_id`，不阻塞 |
| `src/gateway/ws/admin_ws/computer.rs` | 全部 `computer.*` WS 处理器；`start_run`（`:268`）是两个入口共用的唯一启动路径 |
| `src/gateway/ws/admin_ws/traces.rs` | `turns.replay` / `turns.rerun`；`run_timeline`（`:274`）负责把 run 的步骤接进一轮 |

---

## 2. 闭环：目标进、结果出

### 2.1 入口有两个，路径只有一条

- **WS**：`computer.run { goal, session_id?, turn_id? }`，立刻返回 `run_id`、
  实际模型、`model_supports_vision`、`max_steps` 与预算上限。
- **工具**：`computer_run { goal }`（`src/tools/computer_run.rs:43`）。agent 已经在看屏幕时
  没有连接也没有"我要一个 run"的念头，它只有一个工具注册表；没有这个工具，它只能
  一次点一下。

两者都走 `start_run`（`src/gateway/ws/admin_ws/computer.rs:268`），因此拒绝条件、
事件、注册表不会各走各的。`turn_id` 在工具一侧是从 `ToolContext::trace` 读出来的
（模型不知道自己是第几轮，也不该被要求传），没有 trace 就是 `None`。

**为什么工具不阻塞**：`Tool::execute` 外面包着 `ToolContext::timeout()`，默认 120 秒；
一次 run 受 `max_steps`（默认 30）和预算约束，可以跑几分钟。阻塞式工具会被自己的
超时杀掉，而且会把整轮对话按在那儿。所以工具返回的是 `run_id`，不是结果。

### 2.2 决策与验证

- `driver.rs` 把状态渲成提示：目标 + 最近 6 步 + 当前 AX 大纲 + `last_error` +
  连续失败数。**默认不带截图**。
- 解析失败按 `Unverifiable` 处理，绝不当作 `Done`。这条是单测锁死的，不是注释里的好意。
- `NeedHelp` 是一等结果：提示词里允许模型说"我做不到"，解析器尊重它。

### 2.3 停下来的原因必须能分开

`stop_reason` 五值，`is_a_ceiling()` 区分"多给点就能做完"（`max_steps` / `budget`）
与"这就是结论"（`done` / `need_help` / `cancelled`）。混在一起就没法判断
该调步数还是该调预算。

没完成就停下时写**交接单**：完成了什么、卡在哪一步、试过什么、下一步做什么。
四段都来自 run 自己的记录，不是"please check"。同时投进审批队列，所以接手的人
在同一个地方看到"要我做什么"和"为什么"。

### 2.4 控制

`computer.control { run_id, action }`，`action` ∈ `pause` / `resume` / `step` /
`cancel` / `steer`。**直接复用 `ExecutionController`**（`src/acp/controller.rs`），
和 ACP agent 循环是同一个控制器，暂停语义没有第二套实现。未知 action 一律
`INVALID_PARAMS`——"我说停它却继续跑"是这条路径最坏的失败。

---

## 3. 成本：四层降级 + 去重

事实基线（改之前）：30 步 × 每步一次模型调用 × 每张全屏 1920px/JPEG90 的图。
`materialize_history` 限的是"单次请求 8 张"，不是"一次 run 共几张"——30 步每步都
合法满足单次上限，仍然烧掉 30 张。

### L1 · 文本优先

默认输入是 **AX 树大纲 + 上一步结果 + 目标**，不含截图。理由不只是省钱：树给的是
语义（角色 / 标签 / `element_id`），模型据此产出的 `ClickTarget::ElementId` 比从像素
估坐标可靠得多。

⚠️ 但 L1 的前提是"树是完整的"。OpenShot 实测：`max_tree_lines=100` 时，屏幕外的
`Emojis` 工具窗含 119 个 `listitem`，预算在到达时间线之前就耗尽，`add_title` 卡在
0.75。**树被截断时，纯文本给出的是残缺且误导的世界模型，比给图更糟。** 这就是 L0。

### L0 · 树预算按可见性裁剪（优先级高于 L1）

`src/computer/tree_budget.rs`，`prune` 在 `:113`：

1. 每个 root 与 `display_geometry` 求交；完全在外面的折叠成一行
   `window "Emojis" at (-725,-600) [119 children omitted, off-screen]`。
2. 剩下的按 `priority_rank` 重排：**focus 压过面积**，面积从大到小，没量到几何的排最后。
3. 折叠的 root 显式带 flag 排最后——不靠 label 里有没有 `[off-screen]` 判断，
   因为真有窗口可以叫这个名字。

折叠只作用于**顶层** root：可见窗口里的控件即便自己的 box 越界，也仍然可见。
没有几何的元素永不折叠——不知道它在哪，不等于知道它不在屏幕上。

调用点：`src/tools/screen_state.rs:311`（`screen_state` 工具）和
`src/computer/run_executor.rs`（run 每步）。

### L2 · 只在有理由的情况下给图

`plan_perception`（`src/computer/run_budget.rs`）的触发条件
（`ImageTrigger`）：

| 触发 | 含义 |
|---|---|
| `TreeIncomplete` | 树被深度/元素上限截断，语义不足以定位 |
| `TreeUnreadable` | 树根本读不出来（不是短，是没有） |
| `TreeUnread` | 读成功但一个 root 都没有——桌面没发布树，和"树是空的"不是一回事 |
| `LastStepNotVerified` | 上一步没验证——没落地，或者没东西能验证 |
| `RepeatedFailures` | 连续失败 ≥ 2，当前通道不管用 |
| `HoverProbe` | 上一步是 hover——"控件有没有反应"只写在下一帧里 |

`HoverProbe` 是唯一一个**不是"出了事"**的触发。hover 的判定是
`Unverifiable`（反应发生在指针到达之后，而这一步的截图里指针是刚到的），
所以 `last_verified` 是 `None`、失败计数不动——没有这条触发，探测的那一步
后面就会落到"不发图"，等于付了一次测量的钱却不去读结果。它排在两个失败触发
之后，两者同时成立时按更大的事实报；也**不受**"整棵树 + 视觉扫描可以替代整图"
那条规则的抑制（扫描能说"那里有什么"，说不了"它刚才变了没有"）。
跟随 hover 的 `pointer_move` 会结束这个探测——移开正是为了关掉 tooltip，
移开之后的帧就不再是那一次测量的答案。

触发原因写进 run 的步骤记录，所以"这一步为什么带图"事后能查。

### L2.5 · 验证读"变化在哪里"，不只是"有没有变化"

`ScreenDiff` 除了统计像素变化，还带着 `aim`（`AimCheck`）：动作瞄准的框，
以及变化区域是否落在框里。框由 `session.rs` 的 `aim_in_image` 从目标算出来
（坐标点按 32pt、元素按 48pt 膨胀后映射回图像像素），交给
`ScreenDiff::with_aim`。

验证裁决因此有三条：

| 情况 | 裁决 |
|---|---|
| 变化落在瞄准框内，或树上有结构变化 | `Verified` |
| 屏幕变了，但只在框外（时钟跳动、别的窗口重绘） | `Unverifiable`——"不是我证实的"，不是"失败了" |
| 屏幕没变 | `Failed` |

第二种情况不推任何计数，因为它证明的是"证据落在别处"，不是"动作没发生"——
在长页面上点一个链接，链接自己的像素往往不变，整个页面却导航走了。

两个保守处：变化区域列表最多留 8 个最大的框（`regions_truncated` 为真时，
"框里没变化"不算数）；没有 `vision` 特征时字节采样给不出区域，`aim` 记为
`None`，裁决退回原来的"有没有变化"。**不知道 ≠ 在别处。**

**这条事实要走到三个读者手里，缺一个就等于没接线**（这是初版的实际缺陷：
session 判出了"在别处"，`StepRecord` 没接，run 的决策prompt 只能看到
`unchecked`，模型于是反复重试同一次点击）：

| 读者 | 载体 |
|---|---|
| `computer` 工具（单步） | `diff.summary_for_action()` 追加的整句 |
| run 的下一步决策 prompt | `StepRecord::aim_missed` → `driver.rs` 在历史行下追加同一句 |
| 断点续跑 / 时间线 / 交接单 | `CompletedStep::aim_missed` → `resume_history` 与 `turns.replay` 的 `aim_missed` |

三处共用 `off_target_note(&Rect)`，措辞只有一份。Web 的 `verdict()` 因此有
第四态 `elsewhere`（琥珀色）：把"变化在别处"画成和"没人验证"一样的灰字，
等于把唯一可行动的那种 `null` 藏起来。

### L3 · 给图就给小的

`image_budget.rs`：默认 **1024px / JPEG / quality 75**。**刻意不复用**
`screenshot_encoder::NetworkCondition`——那个轴是带宽，本地机器给最高质量
（1920px/PNG/90），恰好和这里要的相反。两套语义不同的缩放分开住，否则以后每个
读者都得猜当前这个数是什么意思。

### L4 · run 级预算

`[computer.budget] max_calls_per_run`（默认 40）、`max_images_per_run`（默认 15）。
超限**不静默停**：走交接单，明说"因预算停在第 N 步，已完成 X，剩余 Y"。
静默停会让用户以为任务做完了，这是最坏的结果。

### 截图去重

`Screenshot.digest` 是 `sha256:<64 hex>`，与附件 CAS 的 key 同构
（`src/computer/types.rs:615`，`Screenshot::new` 在构造时算好）。连续两步屏幕没变
时复用上一步的 ref：`driver.rs` 的 `attach()` 按 digest 命中缓存，不解码、不重新
编码；附件 CAS 也只存一份。

被重复的图**会计费但不算进 run 的图片额度**——所以 `RunOutcome` 有两个数：
`images_attached`（真正消耗额度的）和 `images_repeated`（被计费但没消耗额度的）。
一个卡住的 run 靠后者认出来，而不是靠总数。

### 可观测性

run 结束时写一条 `RunSummary`：
`{steps, model_calls, images_attached, images_repeated, tokens_in, tokens_out, stop_reason}`。
检查点里同样记着这些数，所以**跑完之后**"这个 run 花了多少"仍然答得出来——
内存里的 summary 随着 run 离开注册表就没了。

---

## 4. 视觉：本地优先，API 兜底，可配置

`src/computer/vision/resolver.rs` 的 `VisionResolver` 回答"谁来看"，四模式：

| `[computer.vision] mode` | 行为 |
|---|---|
| `auto`（默认） | 本地 ONNX 有模型就用；否则 API 配了就走 API；都没有就 `Unavailable` |
| `local` | **只用本地。失败也不回落 API**——这是离线/隐私设置，回落就是违约 |
| `api` | 只用 API（本机没下模型时的显式选择） |
| `off` | 完全不看，只有 AX 树 |

`Unavailable` 带原因（`DisabledByConfig` / `FeatureDisabled` / `LocalModelMissing` /
`ApiNotConfigured`），不是一个通用的"失败了"。一个因为"没人能看图"而停下的 run，
不能和一个是"步数用完"的 run 长得一样——`computer.status` 的 `vision` 字段就是
为此存在的。

视觉路径的产物是**语义元素**（`to_ui_elements` → 并入 `UiSnapshot`，带合法
`element_id`），不是裸坐标。这让视觉路和树路同构，模型拿到的仍是可寻址的东西。

本地模型有两个，各自能单独缺席：

| 模型 | 作用 | 产物 |
|---|---|---|
| `UiDetect`（OmniParser 检测） | 找控件框 | 控件元素，`label` 来自重叠的 OCR 文本 |
| `Ocr`（RapidOCR） | 读屏幕上的字 | `role: "text"` 的元素，`label` 就是读出来的那行字 |

`VisionLook` 按 `LocalModel` 分派：检测器给出框后，若本机也装了 OCR，
就用重叠的文字给这些框补 `label`（检测器本身不认字，`ui_onnx.rs` 过去恒发
`label: None`，等于发出一屏无名控件）；只有 OCR 没检测器时，整屏文字就是
元素。`gen` 的原始来源（`"local"` / `"local-ocr"` / `"api"`）记在 run 状态里；
渲染大纲时 `outline_source()` 把它翻成信任行认得的三个词——`"omniparser"` /
`"ocr"` / `"api-vision"`，于是每种来源都有自己的信任行：
"transcribed from pixels" 和 "inferred from pixels" 对模型的含义不同，而
`"api-vision"` 不会被当成 accessibility 树说成 "authoritative"。

视觉调用有超时（`[computer.vision] timeout_secs`，默认 10）：超时即降级，
**绝不让视觉卡住整个 loop**——和 `update.status` 那次事故同一个教训。

补 `label` 这件事有**两个消费者**，不是只有 run 循环。单发工具 `screen_state`
的 `ui_fallback` 走的是同一个 `to_ui_elements`，拿到的也是同一批 `label: None`
的框。它现在同样用 OCR 补名（`label_from_text`，`pub(crate)` 就是为此），并且
在**没跑 OCR** 时报告还有多少框没有名字——那不是一个错误（纯图标按钮本来就没有
文字），而是决定模型能不能说"第二个按钮"的事实。数字不给，模型只会自己猜。

### 4.1 同一帧只扫一次

无树 app 的每一步都会触发扫描（树永久不完整，`ImageTrigger` 一直成立），而
本地检测加 OCR 是秒级的。屏幕没变，答案就不可能变——第二次扫是纯成本。

`VisionLook` 因此记住**上一次**扫描（不是一张表）：命中要同时满足四个键——

| 键 | 为什么要它 |
|---|---|
| `Screenshot.digest` | 同样的像素；摘要缺失（file-backed 截图）时不缓存，也没得比 |
| 起始 rung（`local` / `api`） | 同样的像素可以问不同的问题；API 那档是带着目标问的 |
| goal | 目标变了，API 那档的问法就变了 |
| `DisplayGeometry` | 同样的像素在几何变化下会映射到不同的屏幕坐标 |

**只缓存成功的扫描**，失败的不记——下一次该重试，而不是复读同一次失败。
回落链命中时按**起始** rung 记，不按应答的 rung：下一次在这些像素上会走同一条
链，命中就该短路整条链，而不是重放一个已知在这一屏上失败的档位。

命中不计预算（没有调用发生）。`run_budget` 的图片额度也按摘要去重，两条
去重的键是同一个 `digest`。

---

## 5. 持久化、回放、学习闭环、并行、录放

### 5.1 跨重启（`run_store.rs`）

每完成一步原子写（tmp+rename）`~/.syscity/computer-runs/<run_id>.json`。
重启时：

- **不自动续跑。** 桌面 run 会在别人正在用的机器上真实动手，重启不是同意。
- 把仍处于"被中断"状态的 run 写一行通知进发起它的会话
  （`src/gateway/lifecycle.rs:938`），并列进 `computer.runs`。

`computer.resume { run_id, confirm_sensitive? }` 从第 N+1 步接着跑：

- **不复放**已完成动作，只把它们作为历史交给决策者；
- 步数、预算计数器从检查点接着算（重启不会悄悄把额度翻倍）；
- 检查点里记过命中敏感控件的步骤时，必须带 `confirm_sensitive: true`——
  续跑批准的是目标，不是上一次模型凑巧瞄准的那个密码框。

`RunStore::load` / `delete` 都过 `valid_run_id`（拒绝空、`/`、`\`、`..`、`\0`）。
`run_id` 从客户端来，不过这一关就是一条路径穿越。

### 5.2 回放与时间线（`turns.replay`）

`full.json` 里 `type == "action"` 的行是时间线条目，形状
`{step, action, effect, verified, before_ref, after_ref, ...}`。
run 有自己的检查点、**不写进**那一轮的 `full.json`，所以 `Checkpoint.turn_id`
是两条记录唯一的连接键，`RunStore::load_for_turn` 用它取回。

合并后：

- `step` 是**整条时间线**的零基位置，不按来源重新编号；
- run 来的步骤额外带 `source: "run"`、`run_id`、`run_step`（run 自己的编号）。

`turns.rerun` 重放一轮，用于"改了提示词，这轮还会不会做对"。它需要能调桌面工具，
所以 `RERUN_DESKTOP_TOOLS` 里有 `computer`、`computer_run`、`screen_*`、平台原生工具——
少了 `computer_run`，重跑只能重演走了 `computer` 的那一半，另一半静默漏掉。
重跑没有 assertions/criteria 就拒绝（`NO_ASSERTIONS`）：一个空断言的用例无条件通过，
测的是审批策略而不是提示词。

**非目标**：不做"精确重放鼠标坐标"——DPI/窗口尺寸不稳定，重放以语义动作为准。

### 5.3 学习闭环（D′）

```
run 失败（need_help / budget / max_steps）
  ↓  failure.rs: failure_of()，只转不改
PendingBadcaseStore（去重、SQLite）
  ↓  人工确认 ExpectedEndState（没有确认人 / 没有条件 → 拒绝生成）
回归用例（可失败的 EvalTask）
  ↓  ShadowEval 对比
通过 → apply；变差 → 自动回滚
```

边界写清楚：**现有优化器只调标量参数**（temperature、token budget、上下文预算、
并发上限），**不优化提示词，也不优化工具描述**。所以这条链路能验证"改了这些之后
回归集没变差"，不能验证"学到了一种新策略"。见 §7。

三个反噪声的设计：

- 只有失败进池：`done` 不是 bug 的证据，`cancelled` 是人的决定。
- 去重按 `sha256(input || '|' || response)`，同一失败重复 5 次只有一行。
- 期望终态必须人工确认：模型的"done"不是终态。一条没有条件的用例会让通过率虚高
  且什么也抓不住。

### 5.4 并行：读并行、写串行（D″）

先纠正一个容易再犯的判断："并行操作桌面不成立"是错的。
`src/planner/executor.rs` 是真并行的 DAG 执行器（`tokio::spawn` + 批次并发），
执行的就是 `DesktopAction`。

准确表述：**读/想可并行，单桌面的输入必须串行**（`src/tools/target_lock.rs` 的
`DESKTOP` 是全局单锁）。`DesktopAction::needs_input_lock()`（`src/computer/types.rs:834`）
给每个变体分类：

| 类别 | 变体 | 持锁 |
|---|---|---|
| 输入 | `Click`, `DoubleClick`, `Type`, `KeyPress`, `Drag`, `Scroll` | 是 |
| 观察 | `Screenshot`, `ReadUiTree`, `ListWindows`, `GetWindowGeometry` | 否 |
| 窗口状态 | `Activate`/`Close`/`Move`/`Resize`/`Minimize`/`Maximize`/`LaunchApp` | 是 |
| 无副作用 | `Wait` | 否 |

`Scroll` 与窗口操作极易被误判为无害：滚动会把内容移走，改焦点会让后续的盲输入
落到别处。默认按持锁处理，宁可串行。

**边界**：`ComputerUseLoop` 是单目标串行闭环，不改。并行发生在它**之上**：
`GoalPlanner` 拆 DAG → 多个子任务 → 每个子任务一个 loop → 输入步骤经锁串行。

仍然不做：**多桌面的真并行**（需要多机/多显示器隔离）。

### 5.5 录制与回放（E）

- `computer.record_start { name? }`：录制期间 `computer` 工具的每一次**成功且可重放**
  的动作进 `Recording`。观察类动作不算——`screenshot` / `read_ui_tree` 是决策依据，
  不是做过的事。
- **全 gateway 同时只能有一个录制**：录制挂在 gateway 的工具状态上（WS 发起，
  动作来自另一条 agent 轮次），第二次 `record_start` 返回 `ALREADY_RECORDING`，
  不默默丢掉前一个。
- `computer.record_stop { name, description? }`：`Workflow::parameterize` 把输入的
  内容抽成 `{{参数名}}`（名字取自目标控件的 label，没有则 `text`），存到
  `~/.syscity/workflows/<name>.json`。响应直接列出 `parameters`，调用方不必开文件
  就知道回放要绑什么。
- 元素 id 在录制当下被解析成 `role + label`（`stabilize`）：快照 id 下一次读树就
  指不到了，只有名字能活到另一次运行。

**回放前的歧义预检**（`replay_preflight.rs`）：名字不是身份。回放跑在的屏幕不是
录制时的屏幕，如果上面现在有两个 "New Folder"，第 2 步就是没有授权的抛硬币。
`computer.play` 在第一步之前把目标对当前树解析一遍，有歧义就整单拒绝
（`REPLAY_AMBIGUOUS`，消息点名步骤、匹配数、候选 id）。

只查歧义、不查存在：多数工作流第 1 步就是打开后面要点的窗口，开跑前要求目标存在
会把最该保护的工作流全拒掉。树读不出来也不拒绝——回放照跑，丢掉的只是提前警告。

**视频旁路**（`recording.rs`，`[computer] record_video`，默认关）：一录一个 ffmpeg
进程逐帧喂，不是整段存内存（1920×1080 / 5fps 约 40 MB/s）。supervisor 轮询录制
session 而不是等 `record_stop` 通知——录制会因为好几种原因结束（gateway 关停时
句柄被丢掉也是一种），只认铃铛的视频会在忘响铃的时候被截断。

默认关的理由不是贵：它要一个别的 computer use 都不需要的权限（macOS Screen
Recording），而且会起进程。会起进程的特性应该被要求，而不是被假设。

### 5.6 只看不动（G 的一半）

`computer.watch { interval_ms? }` / `computer.unwatch { watch_id }`：按间隔截图，
每帧作为 `computer.frame` 事件广播（`data:` URL，可直接进 `<img>`）。

- 采集失败广播 `computer.frame_failed`，**不静默跳过**：盯着最后一帧卡住的查看者
  需要分清"画面没变"和"采集坏了"。
- 连续 3 次失败自停；5 分钟自停（"开着的"不能是一个比请求它的人活得更久的状态）；
  每轮循环挂 shutdown。
- 模块里**没有任何派发桌面动作的路径**——"能看不能动"是结构性的，不是一个开关。

输入端仍走 SSH。WebRTC/VNC 不做（见 §7）。

---

## 6. 剩下的缺口

原计划里没有兑现的条目，以及兑现了但故意只做到一半的。每条标注性质：
**缺行为**（要写代码）、**缺测试**（行为在，但没有能锁住它的用例），
或**明确不做**（做了会与别的东西冲突，理由在 §7）。

### 6.1 前端：看当前、回看过去 —— 播放器已接线，剩两块不给

两个界面，各答一个问题：

- **现在**：标题栏的 Monitor 按钮打开右侧分栏（`web/src/components/computer/ComputerPanel.tsx`），
  列出本会话每次 run 的目标、阶段、结束说明、每步动作与结果、rung / 敏感 / verified·refused
  判定、点开才加载的 before/after 截图，以及成本行（步数 · 模型调用 · 图片 · tokens）。
- **过去**：助手消息的操作栏里有一个"回看"按钮（`TurnReplayPlayer.tsx`），打开后是这一轮
  `turns.replay` 的时间线——本轮 `computer` 工具的步骤 + 它起的 run 的步骤，按发生顺序排成一条，
  左边是步列表、右边是当前步的画面；`←/→` 或按钮逐步走，`before`/`after` 两帧可切换，底部是
  被并入的 run 的成本。**两个界面都只读。**

播放器是弹窗而不是右分栏：回看属于"这条消息"，挂在消息上比切走整个右栏更贴近它回答的问题；
也是这个原因它没有放进移动端不存在的分栏里。

数据来自两条路，这是面板能跨重载仍然对的原因：
`computer.step` 事件（进程还活着时）+ `computer.runs` 落盘检查点（进程没了之后）。
两条路都按 `session_id` 过滤：一个 run 属于发起它的会话，共享网关上别人的桌面
不出现在你的面板里；没有 session 的 run（CLI、定时任务）保留，因为它的
事件本来就广播给所有客户端。播放器则按 `turn_id` 过滤——它显示的是**这一轮**做了什么，
而 run 检查点记录 `turn_id` 正是为了这个连接。

**还没做的两块，都不是小缺口，是明确的取舍：**

| 没做 | 现状 | 为什么现在不做 |
|---|---|---|
| **移动端的常驻入口** | 面板只在 `md` 以上的分栏里存在（`App.tsx:1070` 的 `!isMobile`）；播放器本身是整屏弹窗、手机上能用，但入口在消息操作栏，那里是 hover 才出现的 | 手机上分栏没有位置，塞进去会挤掉对话。移动端需要一个独立的全屏路由和一个不带 hover 的常驻入口，不是把面板挪个位置 |
| **`desktop/` 的独立实现** | `desktop/src` 里对 `computer.*` 零命中——但 `tauri.conf.json` 的 `frontendDist` 是 `../dist`，即桌面端跑的就是这份 web 产物 | 没有第二份 UI 要写，不是缺口 |

最后一行值得说清楚：旧版本这里写的是"`web/` 和 `desktop/` 都是零命中"。前半句
已经变成事实的反面，后半句从来就不成立——桌面端不自己写界面。

关于播放器本身的两条边界：它**不是像素回放**（动作以语义描述为准，帧只用于对照，见 §5.2）；
每一帧在选中那一步时才通过 `attachment.get` 取一次，因为一轮可以有三十步，先把图全取回来
是为没人看的画面付费。

### 6.2 明确能接受的几条"做不到"

不是工作量问题，是领域硬限制。

**无 AX 树的界面仍然不可靠。** 画布类与自绘界面（Figma、Photoshop、游戏、大量
非 Electron 的 app）AX 树是空的，只剩视觉检测。B″ 让这件事好很多：视觉输出被转成
`UiElement` 并带 `element_id`，与树路径同构。但视觉会漏元素、会认错图标，也没有
"这些 children 就是全部"这种结构性保证（`is_complete()` 只对树有意义）。
计划能做的是更早触发 L2、更频繁验证、更早交还人类。**这仍是领域短板。**

这里曾经还有一条**接线**缺陷，已经修掉：`tree_is_complete(&[])` 恒为 `true`，
所以"读成功但一个 root 都没有"被当成完整树，第 0 步不触发视觉扫描——换再强的
模型也永远不会被调用。现在三件事分开：读失败（`TreeUnreadable`）、读成功但零
root（`TreeUnread`）、被深度/数量截断（`TreeIncomplete`），零 root 由
`ImageTrigger::TreeUnread` 在会上第 0 步就要图。

**"最小权限"是应用级，不是数据级。** `[computer] allow_apps` 能写"允许操作 Safari、
禁止操作 1Password"，**不能**写"允许在这个 app 里点，但不允许把数据带出去"——
剪贴板、输入法、截图通道在 OS 层共享。数据级隔离需要 VM/容器，而隔离明确不做。

**控件的两件事平台不保证读得回来，只能测或绕。** 这不是接线问题，是把
`UiElement` 三个字段各自的来源摊开看之后剩下的：

- **`enabled` 可能根本没有。** 很多自绘控件不发布它，`Tri::Unknown` 就是真话——
  硬填一个 `Known(true)` 等于替平台撒谎，而填 `false` 会让层梯去点一个本来能点的
  东西。替代品是 `hover`：**测**它有没有反应，不是**读**它的标志位。这也是为什么
  hover 的裁决只能是 `Unverifiable`——反应在下一帧，这一帧只证明指针到了。
- **`value` 读回来有密码风险，所以只回传布尔。** 验证器知道"字段的内容是 / 不是
  我写进去的那串"，而它读到的文本不进任何日志、不进模型的上下文。
  `judge_read_back` 因此只产出一个 `EffectHint`，不产出原文（`session.rs` 有专门的
  测试守着这条）。item 4 的局部截图是这条限制下的一条**合法旁路**：它看的是像素，
  不是字段值，所以不经过 `value` 这条路。
- **结构完整性只对树成立。** 视觉扫描能说"这里有什么"，说不了"就这些了"——
  `is_complete()` 在扫描上没有意义。所以"没在扫描里看到"永远不能当成"不存在"。

这三条是领域限制，不是待修的缺陷；改的只能是措辞和替代路径。

### 6.3 `app_scope` 的两条边缘 —— 已收口

这条原本列在 §6.2 里作为第三条"接受的粗糙"，现在两条都关掉了，所以单独写。
规则仍然是"名单非空时只能操作点名的 app"，但现在区分了两种"没点名"：

- **坐标点击、裸按键、无目标输入**——不再一律拒绝。派发前问平台"这个点 / 当前焦点
  归谁"（`ComputerAdapter::application_at` / `active_app`），拿到 app 名就在名单里
  正常执行，不在名单里就拒绝并**点名那个会收到事件的 app**。平台答不出来（没有
  AX 授权、窗口服务器不可用）仍然是拒绝，且拒绝理由说的是"无法确定谁会收到"，
  不是"这个动作没点名"——"查不到"从来不是放行。
- **点名了允许的 app、却画在别人窗口上**——逐个 rung 检查。pixel / keyboard 这两个
  fallback 是在别的 rung 无效果之后才跑的，正是覆盖窗口或焦点被抢走的那一刻；在
  它们派发之前比对"会收到事件的 app"和 action 点名的 app，不一致就拒绝。AX 与
  page rung 不做此检查：它们作用在元素上，前台是谁与之无关，所以不会被提前拦掉。

平台实现：macOS 用 `AXUIElementCopyElementAtPosition` + `AXParent` 走上到
`AXApplication`（点），以及 System Events 的 frontmost process（焦点）。其他平台
没有实现，`active_app` / `application_at` 返回 `None`，于是回落到"拒绝"——行为与
改动前一致，不会因为平台答不出来而放宽。

仍然抓不到的：**应用级之外的粒度**（一个允许的 app 的窗口里嵌着另一个 app 的内容——
web view、内嵌浏览器），以及上面说的数据级隔离；还有**时间差**——接收方是在派发前
一刻读的，焦点或叠放顺序在那之后仍可能变化。它把"打到不该打的 app"的窗口收窄，
没有关死，这正是"不是沙箱"的意思。

---

## 7. 明确不做

| 不做 | 理由 |
|---|---|
| **容器/VM 隔离** | 技术可行（`ComputerAdapter` 是 trait），但与产品形态冲突：每任务起容器 = 冷启动几十秒 + 用户看不到过程，与"关窗后任务继续""出事能接管"直接对立 |
| **trajectory 级策略学习** | 需要仓库里不存在的基础与大量标注数据；D′ 只做 badcase → 回归用例 → shadow 验证 |
| WebRTC/VNC/自研串流 | 成本高，且与最小权限直接冲突 |
| 精确重放鼠标坐标 | DPI/窗口尺寸不稳定，重放以语义动作为准 |
| 复用 `screenshot_encoder::NetworkCondition` 做成本控制 | 轴是带宽不是上下文，本地恰恰给最贵的图 |
| **时序界面（动画、视频、游戏）** | 验证是"两帧一致就算停"：`SettleConfig` 等的是画面停下来，而这类界面永远不停。停不下来的画面会让每次点击都读到"像素变了"，于是 `Verified` 变成不区分"点中了"和"点歪了"的橡皮图章。要支持得先有另一套证据（目标读回、应用自己的状态），不是把等待调长 |
| 让 `ComputerUseLoop` 内部并行 | 它是单目标串行闭环；并行在其上层的 DAG |
| 合并 `GoalPlanner` 与 `ComputerUseLoop` | planner 是 DAG 任务规划，loop 是单目标 GUI 闭环，语义不同，只是可互相调用 |
| 改 `lifecycle.rs` 的"不自动 re-arm" | 只加了"中断清单"，决定本身是对的 |

**隔离不做的代价要写清楚**：没有隔离就没有可重复性。同样的目标，窗口位置、通知弹窗、
系统负载不同，路径就不同。所以无法提供可重复的评估——"能不能可靠完成"只能靠
轨迹 + 回归集回答，不能靠可重跑的基准。这是自觉接受的限制，不是遗漏。

---

## 8. 契约与门

### 8.1 WS 方法

十个 `computer.*` 方法（scope 与参数见 `docs/protocol.md` §6.1）：

| 方法 | scope |
|---|---|
| `computer.run` / `computer.control` / `computer.resume` | write |
| `computer.record_start` / `computer.record_stop` / `computer.play` / `computer.unwatch` | write |
| `computer.status` / `computer.runs` / `computer.watch` | read |

`turns.replay`（read）、`turns.rerun`（write）在 `traces.rs`。

每个新方法四件套：handler + `ws/core.rs` dispatch + `protocol.rs` `method_scope()`
+ `#[tokio::test]`。

### 8.2 审批与信任是两道门

别把这两件事混在一起：

- **审批门**（`requires_approval`）：`src/computer/platform/capabilities.rs` 的
  构造函数是唯一出口。`desktop_input` / `arbitrary_code` / `clipboard_write` /
  `system_change` 全部 `requires_approval: true` 且 `!read_only`；
  `observe` 则反之，因为给只读工具要审批会训练人闭眼点同意。
- **信任门**（`mark_privileged`）：`src/gateway/agent_spawn.rs:1408` 起把 computer
  工具族加进名单——`SkillTrust::Trusted` 的社区 skill 只能看只读工具。这不是审批门。

两道门各有一个回归测试：`no_platform_tool_delivers_input_without_approval` 遍历
**所有编译进来的** toolset（不只本机的 `cfg(target_os)`），列在 `MUST_ASK` 里的
名字必须是 `requires_approval`；反向的 `observation_tools_stay_unapproved` 防止
"全标成要审批"这种通过方式。`a_desktop_input_tool_does_not_run_unattended` 把真工具
放进真 registry，用 `system` 用户 + `cron:` 会话（无人可问）断言它**根本没跑**——
广告了 `requires_approval` 但 registry 不看的实现，前面几条测试都会过。

Android 是例外：它在工具内部**按动作**门（`android/gate.rs`），因为整工具级别的
`requires_approval` 要么每次滚动都问，要么一次都不问。

### 8.3 动手指令清单

每个 PR 之前：

- [ ] `cargo fmt` / `cargo clippy --all-features -- -D warnings` / `cargo test`
- [ ] 新 WS 方法四件套齐全
- [ ] 所有 `tokio::spawn` 进 `TaskRegistry`，长循环 `select!` 挂 shutdown
- [ ] 审计/事件发送失败用 `warn!`，不 `let _ =` 吞掉
- [ ] e2e 的 skip 必须大声（`skip_if_no_screen_capture` / `skip_if_no_chrome`）
- [ ] `./scripts/self-check.sh` + `./scripts/static-analysis.sh --full`

---

## 9. 测试地图

| 层 | 位置 | 覆盖什么 |
|---|---|---|
| 单测 | `app_scope.rs` | 点名的 app 通过/拒绝、大小写与 glob、名单为空即无边界、`refusal_reaching` 的三种回答（允许 / 点名拒绝 / 无法确定拒绝） |
| 契约 | `tests/computer_contract.rs` | 跨平台的 action 契约（假设没有产成品）：歧义标签拒绝、陈旧 id、禁用控件、occluder、未观测的 AX 不算确认、预算拒绝；以及 `allow_apps` 的两条边缘——坐标/裸按键按**接收方**判定、允许的 app 不会被 fallback 打进别人的窗口 |
| 单测 | `run_executor.rs` | 感知触发、预算停、截图去重（`looking_twice_at_an_unchanged_screen_attaches_one_image`）、无 vision 时零图片 |
| 单测 | `tree_budget.rs` | 屏外折叠、部分可见保留、focus 与面积排序、紧预算下主窗口仍在（含两个窗口都在屏上的用例） |
| 单测 | `vision/resolver.rs` | 四模式；`local_mode_never_falls_back_to_the_api` |
| 单测 | `planner/executor.rs` | 读并行 / 写串行、`DESKTOP` 锁任一时刻只有一个持有者 |
| 单测 | `platform/capabilities.rs` | 审批门两道方向 + 无人可问时真不跑 |
| 单测 | `run_store.rs` | `valid_run_id` 路径穿越、`load_for_turn`、`is_resumable` |
| e2e | `tests/e2e/computer_run_tests.rs` | 无 vision 时以 `need_help` 停下并说明原因；三步 run 每步一个有序事件；**turn 内的工具有 `turn_id`，`turns.replay` 能看到 run 的每一步** |
| e2e | `tests/e2e/computer_resume_tests.rs` | 中断 → 重启 → `computer.runs` 列出 → resume 从下一步继续 |
| e2e | `tests/e2e/computer_play_tests.rs` | 录制 → 带新参数回放；视频开关；歧义预检拒单；watch 广播帧与停止 |
| e2e | `tests/e2e/learning_loop_tests.rs` | 失败 → badcase → 人工确认 → 回归用例 → shadow 变差 → 回滚 |
| e2e | `tests/e2e/computer_calculator_tests.rs` | 真屏幕（`skip_if_no_screen_capture`）：目标 → `Done` → 截图可读 |
| e2e | `tests/e2e/planner_dag_tests.rs` | 3 读 + 1 写的真 DAG：读确实重叠（峰值并发 > 1，总耗时 < 串行）且写独占 |
| UI | `web/e2e/computer.spec.mjs` | 面板：本会话的 run 可见、别的会话的 run 不可见、与 workspace 面板互斥（写一个检查点走真实的 `computer.runs` 读路径） |
| UI | `web/e2e/turn-replay.spec.mjs` | 播放器：一轮自己的步骤 + 它起的 run 的步骤按顺序排成一条、帧按 digest 经 `attachment.get` 取回、逐步前进、成本行；没有桌面动作的一轮说"这轮没操作桌面"而不是空屏（写真实 turn trace 走真实的 `turns.replay` 读路径） |

---

## 10. 一句话回答"这是不是极限"

> 这是**当前能力的工程上限**，不是**计算机操作的能力上限**。

骨架已经完整：目标进、结果出、过程可查、出事能接管、越用越好、能并行。
**不能兑现的是"无树界面同等可靠"和"数据级隔离"。**

能力上探剩下的真正大头只有一个：**trajectory 级策略学习**——它需要仓库里不存在
的基础和标注数据，不是接线能解决的。视觉能力（B″）已纳入并实现，因为当前默认模型
没有 vision，今天所有基准分数都还是 **a11y-only 的下限分**。

任何声称 computer use 能 100% 可靠的方案都在骗人。这套东西的目标是**把失败变成
可见、可接管、可复盘的失败，并让它随使用次数变少**——而不是消灭失败。
