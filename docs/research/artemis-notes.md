# Artemis 代码研究：Android 自动化怎么实现的 + 对 Syscity 的借鉴

> 来源：`~/my/artemis`（非 git 仓库；Python 3.12+，Apache-2.0；README 自称 AndroidWorld 99%+）
> 素材：`README.md`、`.env.example`、`packages/artemis-accessibility-helper/`（手机端 APK 源码）、`artemis/` 包
> 用途：内部研究笔记。与本项目 `src/computer/platform/mobile/` 的 Android 控制路径可直接对比。

---

## 一、一句话结论

**它把"看"和"动"拆成了两条不同的传输通道**：观测走**自研的手机端无障碍服务**（HTTP over `adb forward`），动作主要走最朴素的 `adb shell input`。两者互不依赖，各自可替换。

这是它和多数 Android agent 最不一样的地方——不是"截图 + 视觉模型"，也不是"uiautomator2 一把梭"，而是**自己写了一个不占用 UiAutomation 连接的无障碍服务**。

## 二、分层

```
LLM agent (LangGraph) → MCP action server → Actuator → Driver → adb shell → 手机
                                        ↘ 手机端无障碍服务（adb forward HTTP）↘ 手机
```

`artemis/clients/`（adbtunnel / 各观测后端）、`artemis/drivers/android/adb_driver.py`（动作）、`artemis/graph/`（LangGraph 状态机）、`artemis/mcp/`（动作 schema 与 in-process server）、`packages/artemis-accessibility-helper/`（手机端 APK）。

## 三、连接层：包 `adb` CLI，不直连设备

`artemis/runtime/adb_endpoint.py` 是唯一解析 adb 路径与 server 地址的地方，产出 argv 前缀：

```python
def command(self, arguments):
    return [self.adb_path, "-H", self.endpoint.host, "-P", str(self.endpoint.port), *arguments]
```

默认 `127.0.0.1:5037`（即 `.env` 的 `ADB_HOST`/`ADB_PORT`）。设计上值得注意的是 **`AdbTarget` 是不可变的 `(endpoint, serial)` 快照**，用于加锁与排队——防止任务执行到一半悄悄切到另一个 adb server。

- 设备发现：`runtime/device_pool.py` 跑 `adb devices -l`、1 秒缓存、区分 `device`/`offline`/`unauthorized`、每 serial 一把执行锁。
- 动作路径用 `adbutils.AdbClient`；一次性命令（helper 管理、screencap）直接 subprocess。
- 远端设备三条路：远端 adb server（`-H/-P` 指过去）、**WebSocket ADB 隧道**（`clients/adb_tunnel.py`：本地起 TCP 监听↔WS 双向桥接，对外契约就是 `adb connect 127.0.0.1:PORT`）、`ARTEMIS_CLOUD_MODE=1`（对接仓库外的云网关）。
- Docker（`playground/artemis_container/`）是**容器连宿主/远端的设备**，不是把模拟器塞进镜像。

## 四、观测：三个后端，默认自研无障碍服务

`ARTEMIS_HIERARCHY_BACKEND=auto|helper|uiautomator`：

| 后端 | 机制 | 位置 |
|---|---|---|
| `helper` | 手机端无障碍服务，经 `adb forward` 走 loopback HTTP | `clients/accessibility_client.py` |
| `uiautomator` | Python `uiautomator2`（会推送自己的 device-server APK） | `clients/ui_automator_client.py` |
| `auto`（默认） | 先 helper，失败**自动降级**到 uiautomator2（30s） | `clients/screen_client_factory.py` |

**为什么自研**（写在 `clients/accessibility_client.py:17-27`）：UIAutomator2 会独占单例的 `UiAutomationConnection`，**从而抑制设备上所有其他无障碍服务**；这个助手不占用它，因此能与 Mobly / Appium / Espresso 共存。这是整个设计的根因。

生命周期由 `runtime/helper_manager.py` 全权管理（包 `com.artemis.helper`，设备端口固定 **18888**）：

- **安装/启用**：`adb install -r -g <仓库内预编译 APK>` → `settings put secure enabled_accessibility_services ...`；装完瞬间会被 AccessibilityManager 剪掉，所以要重读重试；OEM ROM 拒绝写 secure settings 时打开系统无障碍设置页让用户手动开。
- **建隧道**：`adb forward --no-rebind tcp:0 tcp:18888` —— **`tcp:0` 交给 adb 分配主机端口**，因此多设备可共用一个主机；已有 forward 复用，多个进程共享一条隧道。用 `transport_id` 检测重插并重建。
- **鉴权**：loopback 端口本机**所有 app 可达**，所以除 `/ping` 外所有端点要求 `X-Artemis-Token`；token 经
  `adb shell am broadcast -n com.artemis.helper/.TokenReceiver -a …SET_TOKEN --es token <48位hex>`
  下发，Receiver 受 `WRITE_SECURE_SETTINGS` 保护——只有 adb shell/系统能设。**这个本地攻击面的处理相当扎实。**
- **防抑制**：助手没响应时用 `ps -A` 找 uiautomator2 的服务进程并**只杀它**（它的 UiAutomation 连接会把助手解绑）。

取数据：`GET /snapshot?fields=xml` —— **同一时刻**拿层级 + 截图（Android 11+ 在设备端做，保证原子性）；旧版本回退 `adb exec-out screencap -p`。两条后端最后归一化成**同一契约**（`UIAutomatorScreenData` + 扁平元素表），下游对后端无感知。

给模型的是**索引化的 "Visible UI Elements" 列表**，外加可选 OCR 融合（`graph/perception.py` 的 `fuse_ocr_with_xml`）——不是纯视觉循环。

## 五、动作：主路径是 `adb shell input`

`drivers/android/adb_driver.py`：

| 动作 | 命令 |
|---|---|
| 点击 | `input tap x y`（≥500ms 则 `input swipe x y x y <ms>`） |
| 滑动 | `input swipe x1 y1 x2 y2 <ms>` |
| 按键 | `input keyevent <code>` |
| 启动/停止 | `monkey -p <pkg> -c android.intent.category.LAUNCHER 1` / `am force-stop` |

**文本输入三级回退**（:307-367），这块最讲究：

1. **剪贴板粘贴**——经助手设 `ClipboardManager`，再 `input keyevent 279`（PASTE）。不受输入法干扰，保留多行与 Unicode；
2. **ADBKeyboard**——若当前 IME 是它，base64 后 `am broadcast -a ADB_INPUT_B64`；
3. **原生**——`input text`，配完整 shell 转义（空格→`%s`），换行处发 `keyevent 66`。

另一条通道是助手自身的**进程内手势注入**：`POST /action {"cmd":"tap"}` → 设备端 `CommandServer` 分发 → `GestureController.tap` 构造 `GestureDescription.StrokeDescription` 调 `dispatchGesture`；文本走 `ACTION_SET_TEXT`。服务配置声明 `canPerformGestures` / `canTakeScreenshot`。

## 六、Agent 循环：Operator 不做 I/O，执行在 Validator

LangGraph 状态机（`graph/graph.py`）：`planner → convergence ⇄ {operator ↔ perception, validator → summarizer} → exit`。

- **perception**：抓屏 + 建索引（每轮）。
- **Operator**（`agents/operator/operator.py`）**只翻译与记录**：LLM 的工具调用被回以 `"Action Recorded"`，动作序列化成 `structured_decisions`，**此时没有任何设备 I/O**。
- **Validator** 才执行：`to_canonical_call` 把内部动词映射到规范名（`tap→click`、`focus_and_input_text→input_text`…），经**进程内 MCP**（`create_connected_server_and_client_session`）调 actuator。
- 坐标**归一化 0–1000**，转像素发生在 actuator（`_to_px`）或 driver（`tap_normalized`）——同一份动作描述与分辨率无关。
- 动作 schema 集中在 `mcp/action_specs.py`，**一份定义三种方言**（operator / declaration / wire），专门防止三处声明漂移。
- 外部 IDE（Claude Code / Antigravity）走独立的 stdio MCP server（`mcp/adb_server.py`）。

## 七、验证：重新观测，不是回执

动作服务 docstring 直说"返回的消息只描述命令已派发，效果由后续观测验证"。执行成功后立即 `session.observe(settle_ms=400)` 重抓层级+截图、刷新索引元素表，交 Validator 判断；上层还有 Checker/Summarizer 做最终放行。

## 八、手机端组件

仓库里**真的有 app**：`packages/artemis-accessibility-helper/`（Gradle 工程 + **预编译 `ArtemisAccessibilityHelper.apk`** + `debug.keystore` + `build_apk.sh`）。`CommandServer.java` 是多线程 HTTP + 逐行 JSON-RPC 服务，**严格绑定 `127.0.0.1:18888`**，路由 `/ping /dump /dump_xml /hierarchy.xml /snapshot /action`。其余：`HierarchyDumper`/`A11yNode`/`XmlUtils`（dump 成 uiautomator 格式）、`GestureController`、`TokenReceiver`/`TokenStore`。

模拟器与真机都支持，代码层**不刻意区分**——adb 能看到的都行。

## 九、iOS：完全没有

无 WDA / idb / XCTest，`drivers/` 只有 `android/`、`cloud/`、`mock/`，README 把 "iOS Platform Expansion" 列为未勾选路线图。抽象层（`BaseDeviceDriver`）是平台中立的，但观测契约实际是 UIAutomator 形态的 XML（`resource-id`、`content-desc`、`bounds="[x,y][x,y]"`），很 Android 特定。

---

## 十、对 Syscity 的借鉴

前提：**两者都能用 adb 控制 Android**，但出发点相反——这决定了哪些经验可迁移。

| | Artemis | Syscity |
|---|---|---|
| 运行位置 | 宿主机 Python 控制手机 | **网关可跑在手机上的 APK 内**（`MainActivity.kt` + `SYSCITY_NATIVE_LIB_DIR` / `process_runner.rs:1040`） |
| 层级获取 | 自研无障碍服务（默认）/ uiautomator2 | `adb shell uiautomator dump /sdcard/window_dump.xml` 再 pull（`platform/mobile/android.rs:403,464`） |
| 动作 | `adb shell input` + 助手 `dispatchGesture` | `adb` 命令（`AndroidToolset`） |
| iOS | 无 | 有（`IosToolset`，走 libimobiledevice） |

**最值得抄的一条：观测后端不要只押 uiautomator。** 我们现在正是 `uiautomator dump`，而 Artemis 明确记录了它的代价——**占用单例 `UiAutomationConnection`，会抑制设备上其他无障碍服务**（Appium/Espresso/Mobly 都会被顶掉）。对"给真机做测试"这个场景这是硬伤。可迁移的做法：把层级读取做成**可替换后端 + 自动降级**（我们现在只有一条路），先加一层接口，再按需接第二个后端。

**第二条：文本输入不要只靠 `input text`。** 我们的 Android 路径目前是 adb 文本输入；Artemis 的三级回退（剪贴板粘贴 → ADBKeyboard → `input text`）解决的是**输入法干扰与 Unicode/多行**，这在中文场景尤其明显。剪贴板粘贴那条尤其划算——它复用了 `input keyevent 279`，不需要额外装 IME。

**第三条：观测的原子性。** Artemis 的 `/snapshot` 在同一时刻返回层级 + 截图（Android 11+ 设备端完成），避免"层级和截图对不上"；我们 `dump` 再 `screencap` 是两次独立 adb 往返，中间界面可能变（动画、Toast），模型会拿到自相矛盾的输入。

**第四条（架构层面，未必照搬）：Operator 不执行、Validator 执行。** 这个"决定"与"落盘"分离的设计让"记录的动作序列"成为可审计的中间产物，也让坐标归一化只在一个地方发生。Syscity 的 computer-use 是 `use_loop` 直接执行，若将来要做回放/审计/重试，这个分层值得参考。

**不建议抄的**：自研无障碍服务本身成本高（Gradle 工程 + 预编译 APK + keystore + 每次安装/启用/防抑制的一堆坑）。除非确定要做 Android 测试自动化，否则先用"多后端 + 降级"把接口留出来即可——真需要时再补第二个后端。

**一处可借鉴的安全实践**：Artemis 的 loopback 端口对所有本机 app 可达，因此它用 **token + `WRITE_SECURE_SETTINGS` 保护的 broadcast** 下发密钥。Syscity 若在手机上暴露任何本地端口（网关就在手机上），同一条思路适用：**loopback 不等于可信**。
