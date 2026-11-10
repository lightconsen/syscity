# Computer Use 基准清单：已跑的、试跑过的、接不了的

> 状态：盘点完成（2026-11-10），三个数据集都已跑过。本文只记录**事实与成本**，不重新论证
> 接入方式。接入方法见 [`computer-use-benchmark.md`](computer-use-benchmark.md)；评测口径见
> [`eval-status.md`](eval-status.md)。
> 最后更新：2026-11-10

本文回答一个问题：**除了 `cua-bench-basic`，还有哪些 Computer Use 基准？哪些现在就能跑，哪些要写胶水，哪些在这台机器上根本跑不起来。**

---

## 〇、四档，别混着说

| 档 | 含义 | 现在有哪些 |
|---|---|---|
| **已接并出分** | 胶水在 `bench/` 里，跑过真环境，有可引用的分数 | Cua Bench `basic` |
| **接好但没跑** | 胶水写完、单测过，缺环境 | OSWorld(-Verified) |
| **零胶水、已试跑** | 同一个 `cb` 命令、同一份 `bench/cua/` 就能跑；跑了，分数分两部分看 | `cua-bench-kicad`（4 个 task 全 0）、`cua-bench-workflows`（OpenShot 最高 1.0，Unity 恒 0） |
| **未接** | 需要新环境或新胶水 | WAA、AndroidWorld、WebArena 系、WorkArena、macOSWorld 等 |

中间两档的区别容易被含混过去：OSWorld 是**代码就绪、环境缺**；kicad/workflows
在 2026-11-10 跑过之后，结论是**胶水成立、环境各自坏在不同地方**——kicad 坏在 arm64 没有
KiCad 9，Unity 坏在上游 `wget` 和 Windows-only 判分，OpenShot 是唯一真正跑起来的那个。
逐项证据在 §二。

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

## 二、零胶水成本、已试跑（2026-11-10）

`cb dataset list` 在 v0.3.1 里还有两个数据集。它们和 `basic` 用**完全相同的调用形状**：

```bash
PYTHONPATH=$PWD SYSCITY_BIN=$PWD/target/debug/syscity \
  env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
  cb run dataset cua-bench-kicad@1.0 \
  --agent-import-path bench.cua.sycity_agent:SyscityAgent \
  --on local --runtime runc -j 2 --max-steps 25 --output-dir <dir>
```

**这一天把两个数据集都跑了。** 结论先写在这里，逐条证据在下面：

| 数据集 | 跑了多少 | 分数 | 这个分数说明什么 |
|---|---|---|---|
| `cua-bench-kicad` | oracle 1 + agent 6（4 个 task） | **0/4 个 task，6 次 agent 全 0** | 环境坏：arm64 装不上 KiCad 9 |
| `cua-bench-workflows` / OpenShot | 4 个 variant（v0–v3），25 与 45 两档步数，共 8 次 | **部分通过**：`create_project` 3 跑 2 个 1.0，`add_title` 最好 0.75 | 胶水成立，卡在 a11y 子树被截断 |
| `cua-bench-workflows` / Unity | 1 个 variant | **0.0** | 环境坏 + 判分脚本是 Windows `cmd` 语法 |

最重要的单个结果是 **OpenShot 的 `create_project`（v0）拿到 1.000**：那是第一次证明
`cua_bridge` / `atspi_xml` / `pyautogui_shim` 这套胶水在一个**原生 Qt5 桌面应用**上成立，
而不只是 `bench-web` 的 pywebview。§4.2 记的"所有基准都只出 a11y-only 下限分"这条约束，
现在有了一个非 WebKit 的正面样本。

### 2.1 `cua-bench-kicad@1.0`

| 项 | 值 |
|---|---|
| 规模 | **25 个 task，每个 1 个 variant**（共 25 个 variant） |
| 镜像 | `ghcr.io/trycua/linux:24.04`（**不是** `bench-web`），4.62 GB，digest `sha256:f4414…`，`uname -m` = aarch64 |
| 应用 | KiCad：原理图编辑、网表导出 |
| 判分 | 上游的 `compare_kicad_netlists`，比对导出的 `output.net` |
| 来源 | Snorkel AI + Cua 的提交任务，`category = "eda"` |

**实测（4 个 task，6 次 agent + 1 次 oracle）：**

| 跑法 | task | 步数 | 状态 | reward |
|---|---|---|---|---|
| oracle | `154d0750` | — | completed，121 s | **1.0**（见下，这条不作数） |
| agent | `154d0750` / `a6414tw4` / `55f2eefb` / `34877be3` | 25 | `MAX_STEPS_EXCEEDED` | 0 / 0 / 0 / 0 |
| agent | `a6414tw4` / `34877be3` | 45 | `✓ completed`（496 s / 478 s） | 0 / 0 |

**oracle 的 1.0 是个假信号。** 每个 task 的 `solve_task` 直接把自带的 `reference.net`
写给 `/home/cua/kicad_project/output.net`，根本不经过 KiCad：

```python
# cua-bench-kicad/<task>/main.py
async def solve(task_cfg, session):
    await session.write_bytes(f"{_REMOTE_PROJECT_DIR}/output.net",
                              (_HARNESS_DIR / "reference.net").read_bytes())
```

所以"oracle 满分"只证明评测器能读文件，**不能**证明 GUI 路径可用。

**根因：arm64 上没有 KiCad 9。** 任务装的是 `ppa:kicad/kicad-9.0-releases`。查这个 PPA 的
`noble` 索引，两个架构不一样：

```bash
# arm64
$ curl -sL .../binary-arm64/Packages.gz | gunzip | grep -c '^Package: kicad$'
0                       # 索引总长 2974 字节
# amd64
$ curl -sL .../binary-amd64/Packages.gz | gunzip | grep -c '^Package: kicad$'
1                       # 索引总长 5575 字节，kicad 9.0.9~ubuntu24.04.1
```

arm64 上 `apt-get install kicad` 于是静默回落到 Ubuntu 24.04 自带的
**`7.0.11+dfsg-1build4`**。而数据集里的 10 张起始原理图全部是 KiCad 9 格式。直接测：

```bash
$ kicad-cli sch export netlist <starter>.kicad_sch -o /tmp/out.net
$ head -2 <starter>.kicad_sch
(kicad_sch	(version 20250114)
Failed to load schematic file          # exit=3，没有产出文件
```

这不是推断：`dpkg-query` 报 `7.0.11+dfsg-1build4`，文件头报 `20250114`，`kicad-cli` 报
`Failed to load schematic file`。模型自己的 turn summary 也是同一句话——
"the `.kicad_sch` file was created by a newer KiCad than the installed 7.0
(`20250114` or later required)"。

**25 个 task 分成两类，其中一类在这个环境下无解：**

| 类别 | 数量 | `initial/` | 本环境能不能做 |
|---|---|---|---|
| 改已有原理图（"Modify R2 to produce 1.8V"） | 10 | 1 张 `.kicad_sch`，`(version 20250114)` | **不能**——KiCad 7 打不开 |
| 从零画图（"Design a 555 timer LED flasher"） | 15 | **空** | 能，但纯画布，没测通 |

10 个 starter 是 `154d0750 / 1625e97a / 2c11fe94 / 55f2eefb / 589da233 / 733d278a /
84ec333e / 9ca3ef44 / d1c655da / d5947ae7`。剩下的 15 个连起始文件都没有。

**6 次 agent 的失败形态分两种，都值得记：**

- `a6414tw4`（45 步，`completed`）：任务是"建一个空网表"。模型把网表和 project 建到了
  `~/cua/empty_netlist`，不是 `/home/cua/kicad_project`，评测器自然找不到。
- `34877be3`（45 步，`completed`）：从零画图，BT1/R1/D1/D2 真画出来了，但最后一步
  "导出网表"没做完。画布操作本身没失败——失败在收尾。
- 25 步那 4 个全是 `MAX_STEPS_EXCEEDED`：装 KiCad + 开 GUI + 画图，25 步不够。

**这里没验证到的**：a11y 树在 Qt 画布上到底长什么样。kicad 一个 task 都没走到"用 id
点元素"的程度，所以"`atspi_xml` 在非 WebKit 应用上是否成立"这个原本要问的问题，
kicad 回答不了。OpenShot 回答了（见 2.2）。

### 2.2 `cua-bench-workflows@1.0`

| 项 | 值 |
|---|---|
| 规模 | **52 个 variant**：`openshot-tasks` 44（1 个建项目 + 43 个加字幕模板）、`unity-tasks` 8 |
| 镜像 | `ghcr.io/trycua/linux:24.04` |
| 形态 | 多步时序任务：建项目 → 加片段 → 设参数 → 保存到指定路径 |

**OpenShot 才是关键的那一半。** 它装的是 Ubuntu 的 `openshot-qt 3.1.1+dfsg1-1`，
基于 **Qt5 / PyQt5**（`libqt5core5t64 5.15.13`），不是 WebKitGTK。判分只看文件：
`.osp` 项目文件存在 → 0.25，title SVG 的 `sodipodi:docname` 匹配模板 → 0.5，SVG 里有
`Welcome` → 0.75，`.osp` 里有 `@assets/title/` 的 clip → 1.0。

逐次结果（`reward` 取 `trajectory.json` 的 `final_metrics.extra.reward`；括号里是那次
的 tool step 数与墙钟）：

| 轮次 | variant | 任务 | 步数上限 | 结果 |
|---|---|---|---|---|
| 单独跑 | v0 | 建项目 `MyFirstVideo` | 25 | **1.0**（17 步 / 786 s） |
| 4 并跑 | v0 | 同上 | 25 | **1.0**（19 步 / 834 s） |
| 3 并跑 | v0 | 同上 | 25 | 0.0（21 步 / 1288 s，模型声称存好了，评测器没找到） |
| 3 并跑 | v1 | `Bar 1` 标题 | 25 | 0.0（20 步，`MAX_STEPS_EXCEEDED`） |
| 3 并跑 | v2 | `Bar 2` 标题 | 25 | **0.25**（22 步，只存下了项目文件） |
| 4 并跑 | v1 / v2 / v3 | `Bar 1` / `Bar 2` / `Bar 3` | 25 | 全 0.0（19 / 21 / 18 步，均 `MAX_STEPS_EXCEEDED`） |
| 3 并跑 | v1 / v0 / v2 | — | 45 | 全部 `did not finish within 1800s` |
| 3 并跑 | v3 | `Bar 3` 标题 | 45 | **0.75**（37 步，`MAX_STEPS_EXCEEDED`） |

`v3` 的 0.75 是能拆开的：项目存对了、`Bar 3.svg` 也生成了，只差"把 SVG 拖到时间线
Track 5 上并改成 Welcome"。模型自己在总结里写得很清楚："No title clip is on the
timeline — Track 5 and Track 4 are still empty"。

**同一个 v0 三次跑出 1.0 / 1.0 / 0.0，说明 `create_project` 不是稳的。** 第三次那轮
（3 并跑）模型的叙述是"Save Project As → 文件名 `MyFirstVideo.osp` → 回车"，评测却读到
0.0；两次 1.0 分别只花 17 和 19 步。样本只有 3 次，不下结论，但**不要把它当成一个
1.0 的稳态**。

**45 步在 OpenShot 上大部分是空转，撞的是墙钟不是步数。** 45 步那轮 4 个里有 3 个报的是
`SyscityRunFailed: syscity did not finish within 1800s`（`timeout` 默认 1800 s，见
`bench/cua/sycity_agent.py:215`），不是 `MAX_STEPS_EXCEEDED`。setup 光装 `openshot-qt`
就要 109–272 s，剩下的时间走不完 45 步。只有排到最后的 v3 活到了评测。
**在这个数据集上调步数之前先调 `timeout`，否则加步数是空转。**

**为什么 `add_title` 卡住：OpenShot 的 a11y 子树被截断到只剩一个屏幕外的隐藏窗口。**
`screen_state` 默认 `max_tree_lines=100`，而 OpenShot 主窗口下面挂着一个坐标
`(-725,-600)`、在屏幕外的 `Emojis` 工具窗，里面有 119 个 emoji `listitem`。这 100 行
预算在到达时间线之前就用光了，模型看到的是：

```
application "openshot"
  window "* Untitled Project [HD 720p 30 fps] - OpenShot Video Editor"
    window "Emojis" at (-725,-600) 230x156
      ... 119 个 listitem "Alien"/"Smiley"/"Angry face" ...
… (tree truncated)
```

实测那一轮 openshot 子树 10862 字符、其中 46 个 emoji `listitem`；另一轮 `add_title` 的
4 次 `screen_state` 读到的子树分别是 635 / 11508 / 3890 / 5383 字符，emoji `listitem`
0 / 48 / 15 / 22 个，**四次全部带 `… (tree truncated)`，而且 "timeline" 和 "track"
一次都没出现**。这不是"模型不会拖拽"，是**它压根看不到轨道**。
（`format_element` 是从根深度优先、按行数硬砍，见 `src/tools/screen_state.rs:502`；
没有按窗口可见性裁剪，这个工具窗坐标在屏幕外但仍被渲染。为什么截断落在 emoji 窗口上
而没有落在更靠前的菜单栏上，取决于 OpenShot 暴露给 AT-SPI 的兄弟顺序——这一次实测的
形状就是上面那段，换了窗口状态会变。）

**Unity 那一半在本机恒为 0，两个独立原因，都要记：**

1. **上游 bug，不是环境缺件。** `unity-tasks` 的 setup 调 `session.apps.unity.install()`，
   用 `wget` 拉 Unity 的签名 key。**这个镜像里没有 `wget`**（有 `curl`），于是那条
   `wget ... | apt-key add` 静默失败，`Setup complete in 2.12s` 什么都没装。
   `~/Desktop/UnityHub.desktop` 却已经写好了，指向不存在的 `/opt/unityhub/unityhub`。
   模型双击它，得到 `Failed to execute child process "/opt/unityhub/unityhub"
   (No such file or directory)`。补上 `curl` + `gpg --dearmor` 后
   `apt-cache policy unityhub` 能给出 `3.22.6`（arm64 有包），所以这里是上游能修、
   我们不打算改的。
2. **判分脚本写死了 Windows。** `unity-tasks/main.py` 里 11 处 `%USERPROFILE%`、
   `if exist ... (echo FOUND) else (echo NOT_FOUND)`、`dir /S /B`、`findstr`。这些命令
   在 Linux 的 bash 下是**语法错误**（`bash: syntax error near unexpected token '('`），
   评测器第一步 `if "FOUND" not in stdout` 就返回 0.0。也就是说，**即便 Unity 装好了，
   这个数据集在本机也拿不到非零分**。Unity 任务本来就是给 Windows 写的
   （`main.py` 里那行被注释掉的 `"os_type": "windows"` 就是证据）。

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
| `screen_state` 默认 `max_tree_lines=100` | 子树大（挂着隐藏的工具窗）的原生应用会被截断到看不见主窗口 | §2.2 的 OpenShot 实测 |
| 一次任务 1800 s 墙钟上限 | 45 步跑满时的真实上限 | `bench/cua/sycity_agent.py:215` 的 `timeout` 默认值 |

---

## 五、接一个新基准，成本花在哪

Cua Bench 已经证明了"一个适配器、两个基准"这个设计：`src/computer/remote/` 里那个说
OSWorld VM HTTP 协议的适配器，同时服务两套。所以成本分两块，**不要混算**：

| 成本 | 内容 | 量级 |
|---|---|---|
| **胶水** | 把基准的任务喂给 `syscity bench run`，把结果翻译成它的判分接口 | 半天到一天（已由 OSWorld/Cua 验证） |
| **环境** | 让基准的环境在这台机器上起来 | 天到周，或者换机器 |

`cua-bench-kicad` 和 `cua-bench-workflows` 在纸面上属于**两者都为零**的那一档。2026-11-10
跑完之后，这个判断要改：**胶水确实为零（一行没改就驱动起来了），但环境不为零**——它们各自
坏在一个具体的地方（kicad 缺 arm64 的 KiCad 9；Unity 缺 `wget` 且判分是 Windows 语法；
OpenShot 缺的是 syscity 自己的 a11y 裁剪）。这三处都不是"接入成本"，是**上游/本机缺陷**，
要分开记账，否则会把它们误算成"接 kicad 要修一天胶水"。

AndroidWorld 属于**胶水小、环境大**，WAA / OSWorld 属于**胶水零（已就绪）、环境大**。

---

## 六、建议顺序（2026-11-10 跑过之后重排）

上一版把"先跑 kicad，半小时"排第一，理由是"零成本、信息量最高"。**这条被证伪了**：
kicad 花掉的是一个 4.62 GB 镜像的 ~28 分钟拉取（`ghcr.io/trycua/linux:24.04`）加上 6 次
agent 跑，换来的是"arm64 装不上 KiCad 9"这一条环境结论，**关于胶水本身一个字都没答**——
连"a11y 树在 Qt 应用上是否可用"都没答到，因为没有一个 kicad task 走到按 id 点元素。
真正回答那个问题的是一次 OpenShot 跑（v0 的 1.0）。按这个重排：

1. **OpenShot 的原生应用回归样本**：`create_project`（v0）单跑一次约 13 分钟
   （setup 里约 2 分钟装 `openshot-qt`），3 跑 2 中。适合每次动 `atspi_xml.py` /
   `pyautogui_shim.py` 之后重跑一轮——它是唯一一个不依赖 `bench-web` 的正面样本。
   注意它**不是稳态**（见 §2.2），重跑时按"3 次里中几次"记，不要按单次记。
2. **修 `screen_state` 的树裁剪**，再跑 `add_title`。§2.2 的截断是当前**唯一一个可以在本机
   修掉**的、真实挡住分数的缺陷。修完后 `add_title` 的 43 个 variant 才是可测的。
3. **再跑一轮 `cua-bench-basic` 全量。** 把 0.926 从"一次"变成"两三次里的一次"，别让单轮
   结果当稳态。
4. **kicad 在 arm64 上放弃**，除非拿到 x86 机器或升级到有 arm64 KiCad 9 的镜像。10 个
   starter task 在本机是硬阻塞，15 个 blank task 只测纯画布、信息量低于成本。
5. **Unity 子集在 Linux 上恒为 0**，判分脚本就是 Windows 语法的。要用它得先上游改成
   跨平台，不在我们这一档里。
6. **等 x86 + `/dev/kvm` 的机器就绪，跑 OSWorld `test_small` 5 题。** 胶水已经在仓库里，
   缺的只是机器。
7. **WAA 排在 OSWorld 之后**，两者同源，信息增量小于成本。
8. **其余（AndroidWorld、WebArena 系、WorkArena、macOSWorld、WebVoyager）暂不接**，
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
| kicad 的 6 次 agent 与 1 次 oracle | `cb run dataset cua-bench-kicad@1.0 --task-filter <id> --on local --runtime runc --max-steps {25,45}`；结果读 `<run>/summary.json` 与每个 variant 的 `trajectory.json` 的 `final_metrics.extra.reward` |
| KiCad PPA 的架构差异 | `curl -sL http://ppa.launchpad.net/kicad/kicad-9.0-releases/ubuntu/dists/noble/main/binary-{arm64,amd64}/Packages.gz`，比字节数与 `^Package: kicad$` 计数 |
| `kicad-cli` 打不开起始原理图 | 在 `ghcr.io/trycua/linux:24.04` 容器里 `apt-get install kicad`，`dpkg-query -W kicad` 得 `7.0.11+dfsg-1build4`，对 `initial/**/*.kicad_sch`（头行 `(version 20250114)`）跑 `kicad-cli sch export netlist`，exit 3 + `Failed to load schematic file` |
| kicad 的 10/15 拆分 | 逐个数每个 task 的 `initial/**/*.kicad_sch`，非空 10 个、空 15 个 |
| OpenShot 是 Qt5 不是 WebKitGTK | 沙箱里 `apt-cache policy openshot-qt` → `3.1.1+dfsg1-1`；`dpkg -l libqt5core5t64` → `5.15.13+dfsg-1ubuntu1` |
| OpenShot a11y 子树被截断 | 读 `~/.syscity/turns/<date>/<uuid>/full.json` 里 `screen_state` 的 `result`：`application "openshot"` 子树 10862 字符、46 个 emoji `listitem`、无 timeline / track 节点；`add_title` 那轮 4 次读取全部 `… (tree truncated)` |
| Unity 缺 `wget` | 容器里 `command -v wget` 无输出、`command -v curl` 有；沙箱里 `ls /opt/unityhub` 不存在；模型双击桌面图标得 `Failed to execute child process "/opt/unityhub/unityhub"` |
| Unity 补 `curl`+`gpg` 后能装 | 同一容器里 `curl -fsSL https://hub.unity3d.com/linux/keys/public` + `gpg --dearmor` → `apt-cache policy unityhub` 给出 `3.22.6`（arm64） |
| Unity 判分是 Windows 语法 | `unity-tasks/main.py` 里 11 处 `%USERPROFILE%`；把那条 `if exist ... (echo FOUND)` 直接交给 bash，得 `syntax error near unexpected token '('` |

上游都在快速迭代。**接任一基准之前把上面这几条命令重跑一遍**，别信本文的旧数。
