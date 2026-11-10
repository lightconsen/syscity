# Computer Use 基准清单：已跑的、零成本能跑的、接不了的

> 状态：盘点完成（2026-11-10）。本文只记录**事实与成本**，不重新论证接入方式。
> 接入方法见 [`computer-use-benchmark.md`](computer-use-benchmark.md)；评测口径见
> [`eval-status.md`](eval-status.md)。
> 最后更新：2026-11-10

本文回答一个问题：**除了 `cua-bench-basic`，还有哪些 Computer Use 基准？哪些现在就能跑，哪些要写胶水，哪些在这台机器上根本跑不起来。**

---

## 〇、四档，别混着说

| 档 | 含义 | 现在有哪些 |
|---|---|---|
| **已接并出分** | 胶水在 `bench/` 里，跑过真环境，有可引用的分数 | Cua Bench `basic` |
| **接好但没跑** | 胶水写完、单测过，缺环境 | OSWorld(-Verified) |
| **零胶水可跑** | 同一个 `cb` 命令、同一份 `bench/cua/` 就能跑，但从没跑过 | `cua-bench-kicad`、`cua-bench-workflows` |
| **未接** | 需要新环境或新胶水 | WAA、AndroidWorld、WebArena 系、WorkArena、macOSWorld 等 |

中间两档的区别容易被含混过去：OSWorld 是**代码就绪、环境缺**，kicad/workflows
是**环境就绪、代码未经检验**。两者都不是"没做"。

---

## 一、`bench/` 里已经有的两套

### 1.1 Cua Bench `basic`

| 项 | 值 |
|---|---|
| 数据集 | `cua-bench-basic@1.0` |
| 环境 | Linux 容器，`--on local --runtime runc` |
| 镜像 | `ghcr.io/trycua/bench-web@sha256:3b7e1d9d…`（pywebview + WebKitGTK） |
| 规模 | 13 个 task / **68 个 variant** |
| 最新结果 | **raw 0.926（63/68）**、**reachable 1.000（63/63）** |
| 唯一减项 | `drag-drop`（0/5，oracle 也 0/5），机制见 §四 |

分数、历史区间、每个失败 variant 的归因都在
[`bench/cua/README.md`](../bench/cua/README.md)；上限声明的机器可读版本是
[`bench/cua/ceiling.json`](../bench/cua/ceiling.json)。

### 1.2 OSWorld(-Verified)

| 项 | 值 |
|---|---|
| 环境 | Ubuntu 虚拟机 + 真实桌面应用（VMware / VirtualBox / AWS / Azure / KVM） |
| 规模 | `test_all` 368 题 / 10 域；`test_small` 39；`test_nogdrive` 360；`test_infeasible` 25 |
| 胶水 | `bench/osworld/`（回放 + 步进两种模式）、`src/computer/remote/osworld.rs` |
| 状态 | **代码写完、单测过，从未在真 VM 上跑过** |
| 阻塞 | 本机是 arm64 macOS，无 `/dev/kvm`；OSWorld 镜像是 x86_64 |

题数有两套，别混：本文 2026-11-10 在 `main` HEAD（`8903ebe`）数到 368/39/360/25；
`computer-use-benchmark.md` 记的是钉住的 commit `044bc33` 上的 369/39/361/25。差的那一题是
上游在两次之间退役的，引用时**连同 commit 一起引**。

这是"整条栈"的主分数——浏览器、LibreOffice 三个组件、GIMP、VS Code、终端、VLC、
Thunderbird 十个域。它和 Cua Bench 共用同一个适配器和同一份 `syscity bench run`，所以接入
第二套只花胶水钱，这一点已经在 Cua Bench 上兑现了。

---

## 二、零胶水成本、但从未跑过

`cb dataset list` 在 v0.3.1 里还有两个数据集。它们和 `basic` 用**完全相同的调用形状**：

```bash
PYTHONPATH=$PWD SYSCITY_BIN=$PWD/target/debug/syscity \
  env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
  cb run dataset cua-bench-kicad@1.0 \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --on local --runtime runc -j 2 --max-steps 25 --output-dir <dir>
```

### 2.1 `cua-bench-kicad@1.0`

| 项 | 值 |
|---|---|
| 规模 | **25 个 task，每个 1 个 variant**（共 25 个 variant） |
| 镜像 | `ghcr.io/trycua/linux:24.04`（**不是** `bench-web`） |
| 应用 | KiCad：原理图编辑、网表导出 |
| 判分 | 上游的 `compare_kicad_netlists`，比对导出的 `output.net` |
| 来源 | Snorkel AI + Cua 的提交任务，`category = "eda"` |

任务分两类，正好压到两套不同的能力：

- **改值/改参数**（"Modify R2 to produce an output of 1.8V"、"Change the voltage source
  from 9V to 5V"）——在已有原理图里定位一个元件、双击、改属性。
- **从零画图**（"Design a microphone preamplifier…"、"Design a 555 timer-based LED
  flasher…"）——纯画布 + 对话框 + 元件库搜索。

**为什么它值得先跑**：这是唯一能立刻回答"`bench/cua/` 这套胶水是不是只对 WebKitGTK 成立的
数据集"。`atspi_xml.py` 里 `queryText()` 那个修复、`_bounds` 的 `visible`/`showing` 区分、
`pyautogui_shim` 的键名表，全都是在 `bench-web` 的 pywebview 窗口上验证出来的。KiCad 是
Qt/GTK 原生应用，a11y 树的形状不一样。

**已知的风险**：`pyautogui_shim` 只实现了 syscity 片段用到的那几个 pyautogui 名字。KiCad 的
画布拖线是 `dragTo`，而且很可能同样撞上 §四 的合成输入问题。这不是 bug，是没测过。

### 2.2 `cua-bench-workflows@1.0`

| 项 | 值 |
|---|---|
| 规模 | **52 个 variant**：`openshot-tasks` 44（1 个建项目 + 43 个加字幕模板）、`unity-tasks` 8 |
| 镜像 | `ghcr.io/trycua/linux:24.04` |
| 形态 | 多步时序任务：建项目 → 加片段 → 设参数 → 保存到指定路径 |

它考的是**跨应用、多步、有中间状态**的任务，跟 `basic` 的单窗口小部件是两个轴。代价是这类
任务对实现细节（对话框的层级、保存路径、模板名字）极其敏感，失败时很难分清是模型错还是
胶水错。

**建议**：先跑 kicad。workflows 的定位更像"压力测试"，不适合当第一个新轴。

### 2.3 registry 里还有但装不了

`cuabench.ai/registry` 提到了 `cua-bench-real`，但 `cb 0.3.1` 的 registry 里没有它：

```
Error: dataset 'cua-bench-real@1.0' is not in the registry
       (known: cua-bench-basic@1.0, cua-bench-kicad@1.0, cua-bench-workflows@1.0)
```

要跑得升级 `cb`，那会同时动 `cua_sandbox` 的行为——不在"零成本"这一档里。

---

## 三、未接入的业界基准

| 基准 | 环境 | 规模 | 这台机器上能不能跑 | 阻塞点 |
|---|---|---|---|---|
| **WindowsAgentArena** (WAA) | Windows 11 VM | `test_all` 154 题 / 12 域；`test_small` 6 题 | ✗ | x86 Windows VM / Azure |
| **AndroidWorld** | 活的 Android 模拟器 | 116 题 / 20 个 app（参数随机生成上百万变体） | 勉强 | 官方 Docker 要 `--privileged`；arm64 上 `emulator` 包有问题，要构建 amd64 镜像 |
| **WebArena** | 自己托管的 6 类站点 | 812 题 | 可跑但重 | 要起 shopping / CMS / Reddit / GitLab / 地图 / Kiwix 一整套容器 |
| **VisualWebArena** | 同上 + 视觉站点 | 910 题 | 同上 | 同上 |
| **WorkArena / WorkArena++** | ServiceNow 实例 | L1：33 任务 / 19,912 实例；++：682 任务 | ✗ | 需 ServiceNow 账号与实例 |
| **WebVoyager** | 15 个真实线上网站 | 643 题（另有 90 题取自 GAIA） | 能跑但不可复现 | 站点会变，分数不可比 |
| **macOSWorld** | AWS 上的 macOS 实例 | 202 个任务文件 / 7 类 / 30 个原生应用 | ✗ | 需 AWS macOS；多语言 |
| **Mind2Web / Mind2Web-2** | 离线人类轨迹 | 数据集 | — | 不是活环境，评的是动作预测不是执行结果 |
| **ScreenSpot / SeeClick** | 静态截图 | — | — | grounding 基准（"该点哪"），不是 agent 基准 |

几点判断，免得这张表被当成"都该接"：

- **WAA 与 OSWorld 同类**（VM 里的完整桌面），障碍也是同一个——x86 + 虚拟化。接了
  OSWorld 就基本具备接 WAA 的条件，反之亦然。**先接一个就够**，两个都接的信息增量小于成本。
- **AndroidWorld 是移动端唯一有代表性的**，但它的运行前提（`--privileged` 起模拟器）和
  `cb` 封死的沙箱边界直接冲突，见 §四。真要跑得绕开 `cb`，那样分数就不可比了。
- **WebArena 系**在 [`benchmark.md`](benchmark.md) 里已被明确划到"暂不做"：它们需要自建站点，
  投入以周计，而覆盖的能力（浏览器 + 页面操作）GAIA 的浏览器路径已经覆盖了一部分。
- **WebVoyager 的分数不该和上面任何一个并列**。真实站点会改版，同一份代码今天跑 60 分、
  下个月跑 40 分，那不是模型变了。
- **Mind2Web / ScreenSpot 不是这个层级的基准**。它们是组件级测量（动作预测、元素定位），
  syscity 的对应物是 `tests/gui_baseline.rs`，不是外部基准。

---

## 四、这台机器的硬约束

### 4.1 `/dev/uinput` 为什么没有

这是 `drag-drop` 0/5 的根因，也是理解"哪些基准跑不了"的钥匙。三层，逐层都有实测：

**第一层：内核有这个设备。** 容器跑在 OrbStack 的 Linux VM 里（kernel
`7.0.11-orbstack-…`），`uinput` 是内核的 misc 设备（major 10, minor 223）：

```bash
$ docker run --rm --privileged alpine:3.20 sh -c 'ls /sys/class/misc | grep input'
uinput
```

Darwin 本机的 `/dev/uinput` 不存在，但那无关——容器用的是 VM 的内核。

**第二层：默认的 runc 容器既没有节点，设备 cgroup 也拒绝。** 这是两层，必须分开测，
否则会误判成"只是少了个文件"：

```bash
# (a) 默认容器里没有这个节点
$ docker run --rm --runtime runc alpine:3.20 sh -c 'ls /dev'
fd full mqueue null ptmx pts random shm stderr stdin stdout tty urandom zero

# (b) 只 bind-mount 节点 —— 节点在，打开被拒
$ docker run --rm -v /dev/uinput:/dev/uinput --runtime runc alpine:3.20 \
    sh -c 'exec 3>/dev/uinput'
sh: can't create /dev/uinput: Operation not permitted

# (c) --device 同时给节点和 cgroup 允许规则
$ docker run --rm --device /dev/uinput --runtime runc alpine:3.20 \
    sh -c 'exec 3>/dev/uinput && echo write-open OK'
write-open OK
```

cgroup v2 用 eBPF 设备过滤器管 major:minor，默认允许表里不含 10:223。(b) 的
`Operation not permitted` 就是它。

**再往里一步：驱动本身是好的。** 特权容器里走完 `UI_SET_EVBIT` → `UI_DEV_SETUP` →
`UI_DEV_CREATE`，返回成功，虚拟输入设备真的建起来了。所以不是内核缺功能，是容器没被授权。

**第三层：`cb` 主动不提供这个口子。** `cb run task --help` 没有 `--device` /
`--privileged` / `--volume`；SDK 里 `DockerRuntime.__init__` 更是显式拒绝：

```python
unsupported = [flag for flag, value in (
    ("volumes", volumes), ("devices", devices),
    ("platform", platform), ("privileged", privileged),
) if value]
if unsupported:
    raise ValueError(f"DockerRuntime no longer supports {', '.join(unsupported)}: containers run "
                     "sandboxed through the cua SDK (gVisor when available). ...")
```

这是**有意的沙箱边界**，不是遗漏。想加要么 fork `cua_sandbox`，要么绕开 `cb` 自己起容器——
后者会让分数和数据集不可比，正是 `bench/README.md` 里"不要本地复刻评测器"那条要防的。

**它和 `drag-drop` 怎么接上。** 镜像里 `cua-spacesd` 自己的诊断字符串就写着：

```
uinput device unavailable: ... Grant this process access to /dev/uinput (modprobe ...)
WebKitGTK rejects synthetic XSendEvent input and this session has no real target-address…
```

我们这条路走的是 `xdotool`（XTEST）。在真实容器里注入页面级监听测过：`mousedown` /
`mousemove` / `mouseup` 都到达页面，`dragstart` 从不触发，30 个密集插值点也一样。HTML5
拖拽要 GTK 在"真实输入设备"上认出拖拽手势，XTEST 合成的事件不算。`/dev/uinput` 是绕过
这一层的唯一路——事件从内核 evdev 进来，和真鼠标没有区别。

> **哪部分是推断**：能测的是"页面鼠标事件齐全、拖拽事件没有"和"本机内核能建虚拟输入设备"。
> **没测的是**"注入 `/dev/uinput` 之后 `dragstart` 会触发"。这需要在手工容器里跑那 5 个
> variant 的 oracle 才能坐实，那是一次独立实验，不改变现有评分。

### 4.2 其它约束

| 约束 | 影响 | 依据 |
|---|---|---|
| 无 `/dev/kvm`（arm64 macOS） | OSWorld / WAA 只能当控制面，不能当宿主机 | [`osworld-kvm-server.md`](osworld-kvm-server.md) |
| `runsc` 未安装 | `--runtime gvisor` 不可用；`runc` 可用 | `docker info` / `runsc` 不存在 |
| arm64 | AndroidWorld 官方镜像要 amd64；emulator 包在 arm64 上有已知问题 | 上游 README |
| 无 ServiceNow 实例 | WorkArena 系不可接 | — |
| 默认模型无 vision（`deepseek-v4-flash`） | 所有基准都只出 a11y-only 下限分 | 见 `computer-use-benchmark.md` §一 |

---

## 五、接一个新基准，成本花在哪

Cua Bench 已经证明了"一个适配器、两个基准"这个设计：`src/computer/remote/` 里那个说
OSWorld VM HTTP 协议的适配器，同时服务两套。所以成本分两块，**不要混算**：

| 成本 | 内容 | 量级 |
|---|---|---|
| **胶水** | 把基准的任务喂给 `syscity bench run`，把结果翻译成它的判分接口 | 半天到一天（已由 OSWorld/Cua 验证） |
| **环境** | 让基准的环境在这台机器上起来 | 天到周，或者换机器 |

`cua-bench-kicad` 和 `cua-bench-workflows` 属于**两者都为零**的那一档——这就是它们排在最
前面的原因。AndroidWorld 属于**胶水小、环境大**，WAA / OSWorld 属于**胶水零（已就绪）、
环境大**。

---

## 六、建议顺序

1. **跑 `cua-bench-kicad@1.0` 的两三个 variant。** 零新代码，半小时，直接回答"胶水是不是
   只对 bench-web 成立"。这是当前信息量最高的一步，也是唯一能在不换机器的前提下开新轴的。
2. **再跑一轮 `cua-bench-basic` 全量。** 把 0.926 从"一次"变成"两三次里的一次"，别让单轮
   结果当稳态。
3. **等 x86 + `/dev/kvm` 的机器就绪，跑 OSWorld `test_small` 5 题。** 胶水已经在仓库里，
   缺的只是机器。
4. **`cua-bench-workflows` 当压力测试，排在 kicad 之后。**
5. **WAA 排在 OSWorld 之后**，两者同源，信息增量小于成本。
6. **其余（AndroidWorld、WebArena 系、WorkArena、macOSWorld、WebVoyager）暂不接**，
   理由见 §三。

---

## 七、核对记录

本文的事实核对于 **2026-11-10**，采集方式与来源：

| 事实 | 怎么核的 |
|---|---|
| `cb` 版本与数据集清单 | `cb --help`、`cb dataset list`（v0.3.1） |
| kicad / workflows 的题数与镜像 | `cb run dataset <name> --dry-run --on local --runtime runc`，逐行数 `v<n>:`；题面读 `~/.cua/cbregistry/<ds>/…/main.py` 与 `pyproject.toml` |
| `basic` 的 68 个 variant | 同上，按 task 名分组 |
| OSWorld 题数 | `raw.githubusercontent.com/xlang-ai/OSWorld/main/evaluation_examples/{test_all,test_small,test_nogdrive,test_infeasible}.json`；`main` HEAD = `8903ebe`（2026-11-02），钉住版本 = `044bc33` |
| WAA 题数与域 | `microsoft/WindowsAgentArena` 的 `evaluation_examples_windows/{test_all,test_small}.json` |
| AndroidWorld 规模与 Docker 前提 | 上游 README（`116 tasks across 20 apps`；`docker run --privileged`；arm64 需构建 amd64 镜像） |
| WebArena / VisualWebArena / WorkArena / WebVoyager | 各自仓库 README |
| macOSWorld 规模 | 按 `tasks/<类别>` 下的文件数合计：38+29+35+12+29+30+29 = 202 |
| `/dev/uinput` 三层 | §4.1 里的四条 `docker run` 命令，加上特权容器里的 `UI_DEV_CREATE` 探针，均在本机执行 |

上游都在快速迭代。**接任一基准之前把上面这几条命令重跑一遍**，别信本文的旧数。
