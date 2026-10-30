# 在一台支持 KVM 的服务器上跑 OSWorld × Syscity

这是一份可照着敲的操作手册：从一台空 Ubuntu 服务器，到 `bench/osworld/run.py`
跑出第一个 OSWorld 分数。设计与契约在
[computer-use-benchmark.md](computer-use-benchmark.md)，胶水层的接口说明在
[bench/README.md](../bench/README.md)；本文只讲"机器怎么装、怎么验、怎么跑、跑挂了怎么办"。

**为什么不能在本机跑**：OSWorld 的镜像是 x86_64，Apple Silicon 只跑 arm64 客户机，
QEMU 的 TCG 能起 x86_64 但慢 10–50 倍，LibreOffice/Chrome 会直接超时。宿主机必须是
x86_64 Linux 且 `/dev/kvm` 可用（`computer-use-benchmark.md` §六 有反例记录：普通云主机
默认不开嵌套虚拟化）。

---

## 〇、从笔记本到第一个分数

下面六步是在**一台刚开的服务器**上从零走到"一道题被评分"的完整序列。前两步在
**笔记本**上敲，后面的在服务器上敲（标了 ⬅ 笔记本 / ➡ 服务器）。

### 第 0 步 ⬅ 给机器起个 ssh 别名

`~/.ssh/config` 里加一段，之后所有命令都能写成 `ssh bench`，不用每次背 IP 和端口：

```
Host bench
  HostName <服务器 ip>
  User root
  Port 14513
```

`bench` 只是**客户端这边的别名**，服务器自己不知道这个名字。不想加也行，把下文所有
`bench` 换成 `-p 14513 root@<ip>`，效果一样。

```bash
ssh bench 'uname -m; free -g | head -2'     # 通了再往下走
```

### 第 1 步 ⬅ 把仓库同步到服务器

**为什么需要**：要跑的 `scripts/osworld-bench-setup.sh` 和 `bench/` 胶水都在仓库里，
**release 的产物只有二进制**，没有这两样。同步相当于把"操作手册 + 驱动脚本"送到机器上。

```bash
cd ~/work/syscity/syscity
tar -cz --exclude=./target --exclude=./target-linux --exclude=./.git \
    --exclude='*/node_modules' --exclude=./web/dist . \
  | ssh bench 'mkdir -p /opt/bench/syscity && tar -xz -C /opt/bench/syscity'
```

排除的是本地编译产物（`target/` 2.8 GB + `target-linux/` 3.9 GB）和依赖树：整棵树
7.6 GB，排完只剩 18 MB。走 ssh 的 stdin，不落临时文件，也不要求服务器能连 GitHub。

验证：

```bash
ssh bench 'ls /opt/bench/syscity/scripts/osworld-bench-setup.sh /opt/bench/syscity/bench/run.py'
```

想用 rsync 也行，但 `SYS_SRC` 指的是**服务器上**已有的另一份源码树（脚本内部
`rsync -a $SYS_SRC/ $SYS/`），不是笔记本上的路径——跨机同步仍得走上面的 tar 管道，
或者 `rsync -az --exclude target --exclude .git . bench:/opt/bench/syscity/`（两端都要有
rsync）。

### 第 2 步 ⬅ 放两个密钥（走 stdin，不进命令行）

```bash
# Hugging Face：下 12 GiB 的 Ubuntu.qcow2.zip 用
sed -n '2p' hf.secret | tr -d '[:space:]' | ssh bench 'umask 077; cat > /root/.hf_token'

# DeepSeek：只有 `--smoke` 的一题评分需要
ssh bench 'umask 077; cat > /root/.deepseek_key'     # 粘贴 key，Ctrl-D
```

这两条命令**在笔记本上敲**，数据经 ssh 送进服务器的 `cat`，落到服务器磁盘；本地不留副本。

| 文件 | 谁读 | 不给会怎样 |
|---|---|---|
| `/root/.hf_token` | `fetch_image`（`HF_TOKEN_FILE`） | 匿名下载被限速甚至 403 |
| `/root/.deepseek_key` | `smoke`（`PROVIDER_KEY_FILE`） | 只影响 `--smoke`，其余步骤照常 |

`umask 077` 保证落地是 `-rw-------`；用 stdin 而不是 `ssh bench "echo $KEY > f"`，是因为
后者会把密钥塞进命令行，两端的 shell 历史和服务器上的 `ps` 都能看到。

验证：

```bash
ssh bench 'ls -l /root/.hf_token /root/.deepseek_key'    # 期望 -rw------- 1 root root
```

这两个文件已经在服务器上的话，跳过本步。

### 第 3 步 ➡ 看它要做什么（什么都不改）

```bash
ssh bench
tmux new -s bench            # 后面的步骤都在 tmux 里跑，SSH 断线不影响
sudo /opt/bench/syscity/scripts/osworld-bench-setup.sh --dry-run
```

预检会硬性卡四件事：`x86_64`、`/proc/cpuinfo` 里有 `vmx|svm`、`/dev/kvm` 可读写、
≥16 GB 内存 + ≥50 GB 磁盘。`/dev/kvm` 不过就别往下走了——后面全是 TCG 慢放。

### 第 4 步 ➡ 装依赖 + 验二进制（先不下 12 GB 镜像）

```bash
sudo /opt/bench/syscity/scripts/osworld-bench-setup.sh \
     --skip-image --skip-verify --skip-probe
```

装 `ca-certificates curl unzip git psmisc libgomp1` → Docker → uv → clone OSWorld 建 venv
→ 从 release 下 `syscity-linux-amd64.tar.gz` 校验 sha256 → 取同 tag 的 `bench/` 胶水。

```bash
ldd /opt/bench/bin/syscity | grep 'not found' || echo "no missing libs"
/opt/bench/bin/syscity --version                          # syscity 0.3.16
```

`no missing libs` + 版本号正确，说明二进制和系统依赖这一环通了。这一步故意跳过镜像，
让"环境不对"在 10 分钟内暴露，而不是等 30 分钟下完 12 GB 之后。

### 第 5 步 ➡ 全量：下镜像、起 VM、跑探针（不花模型钱）

```bash
sudo /opt/bench/syscity/scripts/osworld-bench-setup.sh --skip-deps
```

`--skip-deps` 只是省一遍 apt。接着下 `Ubuntu.qcow2.zip`（≈12 GiB，断了重跑会
`curl -C -` 续传）、解出 qcow2、起容器/VM 自检、跑探针。

验收只有两条：

```
PR660_FINAL_RESULT {"all_passed": true, "kvm_exists": true}     # 3.1 容器/VM 自检
 ok all four endpoints answered                                  # 3.2 探针
```

### 第 6 步 ➡ 一题真实评分（花模型钱，10–30 min）

```bash
sudo /opt/bench/syscity/scripts/osworld-bench-setup.sh --verify-only --smoke
```

`--verify-only` 跳过安装，只重做自检 + 探针 + 一题。结果目录每次都是新的，
名字带时间戳（`$RUNS/smoke-20261030-141203`），脚本会打印 `task <id> -> <dir>`：

```bash
ls -dt /opt/bench/results/smoke-* | head -1          # 刚才那次
cat "$(ls -dt /opt/bench/results/smoke-* | head -1)/summary/syscity.json"
```

- `"scored": 1` —— 真跑完并被 OSWorld 评了分（**分数 0 也算成功**，那才是模型能力）
- `"infra_errors": 1` —— 环境/配置问题，去 `/opt/bench/results/smoke-<时间戳>/<domain>/<id>/`
  看 `serve.stderr.log` 和 `error.json`

到这里通关。再往上就是 4.2 的 `test_small`（39 题）和 4.3 的全量。

### 一张表：全部参数

| 用法 | 作用 |
|---|---|
| `--dry-run` | 只打印每个阶段要执行的命令，什么都不改（在笔记本上也能跑） |
| `--verify-only` | 跳过安装，只做自检 + 探针（+ `--smoke`） |
| `--skip-image` / `--skip-binary` / `--skip-probe` / `--skip-verify` / `--skip-deps` | 跳过已知完成的部分 |
| `--refetch` | 强制重新下载二进制（`--from-source` 时是重编） |
| `--from-source` | 不用 release，在 `$SYS` 编源码；要 Rust 和 2.1 的编译期依赖 |
| `SYSCITY_VERSION=v0.3.16` | 装指定版本，默认 `latest` |
| `SYSCITY_BIN_URL=…` | 换下载源（镜像或 `file://`），校验文件取 `<url>.sha256` |
| `SYSCITY_SRC_URL=…` | 换 `bench/` 胶水用的源码包地址（镜像或 `file://`） |
| `SYS_SRC=/path/to/syscity` | 把**服务器上**的另一份源码树 rsync 到 `$SYS`（不是笔记本上的路径） |

默认装的是 GitHub release 里的 `syscity-linux-amd64.tar.gz`（校验 `sha256` 后才安装），
**不需要在那台机器上装 Rust 或编译**。v0.3.16 起这个产物是在 Ubuntu 22.04 runner 上
编的，glibc 下限 2.35；v0.3.14 及更早是在 24.04 上编的，需要 GLIBC_2.39，在 22.04 上
根本起不来——所以别手抄旧教程里的下载链接。

下面各节是它每一步的**理由**和手工等价命令：脚本某个阶段报错时，对着看那一节。

**手工路径（等价）**：

```bash
# 0. 确认机器真的能跑（第一节）
ls -l /dev/kvm; egrep -c '(vmx|svm)' /proc/cpuinfo

# 1. 系统依赖（2.1）
# 2. 下载 syscity 二进制（2.2；要自己编就 --from-source）
# 3. Docker + happysixd/osworld-docker（2.3）
# 4. OSWorld + Python 3.12 环境（2.4）
# 5. 12 GB 的 Ubuntu.qcow2（2.5）
# 6. 密钥，走 stdin，不落在命令行（2.6）

# 7. 验收：容器/VM 自检 + 探针（第三节）——不花模型钱
# 8. 跑：一题 → test_small（39）→ test_all（369）（第四节）
```

下面统一用这几个变量，按自己机器改：

```bash
export BENCH_HOST=bench                 # ssh 别名；没有别名就写 root@<host> 并自己加 -p
export OSW=/opt/bench/OSWorld           # OSWorld 检出目录
export SYS=/opt/bench/syscity           # bench/ 胶水所在的目录（源码或解包）
export BIN=/opt/bench/bin/syscity        # 安装的 release 二进制
export RUNS=/opt/bench/results           # 结果目录
```

验收通过的标志只有两条：自检脚本打印 `"all_passed": true, "kvm_exists": true`，
`syscity bench probe --execute` 打印 `ok`。这两条不过，跑任务只会得到一堆 infra error。

---

## 一、机器硬指标

| 项 | 要求 | 检查 | 为什么 |
|---|---|---|---|
| CPU 架构 | x86_64 | `uname -m` | 官方分数口径和 docker provider 用的都是 x86_64 的 `Ubuntu.qcow2`（HF 上另有一个 arm 包，与这套流程无关） |
| 硬件虚拟化 | `egrep -c '(vmx|svm)' /proc/cpuinfo` > 0 | 同上 | QEMU 没有它就走 TCG |
| `/dev/kvm` | 存在且可读写 | `ls -l /dev/kvm` | Docker provider 检测到才把设备传给容器；没有则日志 warning、慢到不可用 |
| 内存 | ≥ 16 GB | `free -g` | 容器默认 `RAM_SIZE=4G`，宿主上还跑着 Python、Docker、评测器；实测 49 GB 的机器很宽松 |
| CPU 核数 | ≥ 4 | `nproc` | 容器默认 `CPU_CORES=4` |
| 磁盘 | ≥ 60 GB 空闲 | `df -h /` | `Ubuntu.qcow2` 12 GiB + 下载中的 12 GiB 压缩包 + 容器 `DISK_SIZE=32G` + 录屏和截图 |
| Docker | Engine ≥ 24 | `docker info` | docker provider 用 Python docker SDK 建容器 |
| Python | ≥ 3.12 | `.venv/bin/python -V` | OSWorld 依赖（用 uv 装 3.12 最省事） |
| Rust | 只有 `--from-source` 需要 | `cargo -V` | 默认装 release，不在机器上编译 |

云主机先确认嵌套虚拟化。反例（2026-10-29 实测）：某云 Ubuntu 22.04、8 vCPU EPYC、
`systemd-detect-virt` 显示 `kvm`，但 `/proc/cpuinfo` 里 0 个 `vmx|svm`，没有 `/dev/kvm`，
`modprobe kvm_amd` 报 `could not insert`，dmesg 是 `kvm: no hardware support`。
**租机器之前先看 `/dev/kvm`。**

---

## 二、装机器

### 2.1 基础依赖

装 release 二进制时只要下面这一行（脚本自己会装）：

```bash
apt-get update
apt-get install -y ca-certificates curl unzip git psmisc libgomp1
```

| 包 | 谁要它 |
|---|---|
| `curl` / `unzip` / `ca-certificates` | 下二进制、下 `bench/` 源码包、下 12 GiB 的 `Ubuntu.qcow2.zip` |
| `git` | `git clone` OSWorld |
| `psmisc` | 脚本等 `dpkg` 锁用的 `fuser`（没有就只等，不影响成败） |
| **`libgomp1`** | 二进制运行时依赖：`local-embeddings` 里的 llama.cpp 链接 OpenMP，缺了就是 `libgomp.so.1: cannot open shared object file`，`--version` 都起不来 |

**只有 `--from-source` 才需要**下面这些编译期依赖：

```bash
apt-get install -y build-essential cmake pkg-config \
  libssl-dev libclang-dev clang libcurl4-openssl-dev \
  libasound2-dev libx11-dev libxdo-dev libxcb1-dev libxrandr-dev libxi-dev \
  libxtst-dev libatspi2.0-dev libdbus-1-dev
```

| 包 | 谁要它 |
|---|---|
| `build-essential` / `cmake` / `libcurl4-openssl-dev` | `llama-cpp-sys-2` 里的 llama.cpp 用 CMake 构建 |
| `libclang-dev` / `clang` | `llama-cpp-sys-2` → `bindgen`，缺了直接 panic（见故障表） |
| `libssl-dev` / `pkg-config` | `openssl-sys` |
| `libasound2-dev` | `alsa-sys`（音频设备） |
| `libx11-dev` `libxdo-dev` `libxcb1-dev` `libxrandr-dev` `libxi-dev` `libxtst-dev` | 桌面控制的 X11 绑定 |
| `libatspi2.0-dev` `libdbus-1-dev` | a11y 树 / D-Bus |

### 2.2 syscity 二进制

不要编。装 GitHub release 里的产物，脚本做的就是这件事：

```bash
mkdir -p /opt/bench/bin
curl -fL --retry 3 \
  -o /tmp/syscity-linux-amd64.tar.gz \
  https://github.com/lightconsen/syscity/releases/latest/download/syscity-linux-amd64.tar.gz
curl -fL -o /tmp/syscity-linux-amd64.tar.gz.sha256 \
  https://github.com/lightconsen/syscity/releases/latest/download/syscity-linux-amd64.tar.gz.sha256
(cd /tmp && sha256sum -c syscity-linux-amd64.tar.gz.sha256)
tar xzf /tmp/syscity-linux-amd64.tar.gz -C /opt/bench/bin syscity
chmod +x $BIN
$BIN --version            # syscity 0.3.16
```

- **不带 `vision`**：x86_64 Linux 的产物用 `linux-amd64` profile 编，去掉了
  ONNX 的 OCR/UI 检测（那个预编译库要 glibc ≥ 2.38，22.04 上链接都过不去）。
  benchmark 用不到它；要 OCR 就在 24.04 上用 `--features vision` 自己编，
  或者用 arm64 产物。
- **glibc 下限 2.35**：v0.3.16 起这个产物在 `ubuntu-22.04` runner 上编，22.04 能直接跑。
  v0.3.14 及更早是在 24.04 上编的，要 GLIBC_2.39，22.04 上连 `--version` 都起不来。
- **运行时要 `libgomp1`**：不是在 CI 里编进去的，也不是 glibc；缺了报
  `libgomp.so.1: cannot open shared object file`。`ldd $BIN | grep 'not found'` 只有这一条时，
  `apt-get install -y libgomp1` 即可（脚本已包含）。
- 指定版本：把两处 `latest/download` 换成 `download/v0.3.16`；脚本用
  `SYSCITY_VERSION=v0.3.16`。
- **`bench/` 是 Python，不在二进制里**。脚本会再从同一个 tag 的源码包里取出
  `bench/` 放到 `$SYS/bench`（`run.py` 靠它驱动）。手动做（在服务器上）：

```bash
curl -fL -o /tmp/syscity-src.tar.gz \
  https://github.com/lightconsen/syscity/archive/refs/tags/v0.3.16.tar.gz
mkdir -p /tmp/syscity-src && tar xzf /tmp/syscity-src.tar.gz -C /tmp/syscity-src
mkdir -p $SYS && cp -a /tmp/syscity-src/*/bench $SYS/
```

  如果本地工作树比 release 新（比如刚改了 `scripts/` 或 `bench/`），与其等发版，
  不如直接把树推过去——**也只有这条路能把 `scripts/osworld-bench-setup.sh` 弄上去，
  它不在 release 产物里**（在笔记本上执行，排除掉编译产物后 7.6 GB 只剩 18 MB）：

```bash
cd ~/work/syscity/syscity
tar -cz --exclude=./target --exclude=./target-linux --exclude=./.git \
    --exclude='*/node_modules' --exclude=./web/dist . \
  | ssh $BENCH_HOST 'mkdir -p /opt/bench/syscity && tar -xz -C /opt/bench/syscity'
```

- GitHub 慢或不通时，用 `SYSCITY_BIN_URL` 换源（镜像、内网、`file://` 都行），
  `.sha256` 会按 `<url>.sha256` 取：

```bash
export https_proxy=http://127.0.0.1:1087 http_proxy=http://127.0.0.1:1087   # 或者
export SYSCITY_BIN_URL=https://<镜像>/syscity-linux-amd64.tar.gz
```

**要自己编**（源码比最后一个 release 新）：`--from-source`，或者手动

```bash
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
cd $SYS
LIBCLANG_PATH=/usr/lib/llvm-14/lib cargo build --release --bin syscity
install -m 0755 target/release/syscity $BIN
```

`LIBCLANG_PATH` 在 Ubuntu 22.04 上是 `/usr/lib/llvm-14/lib`；装在别处就
`ls /usr/lib/llvm-*/lib/libclang.so` 找。4–5 vCPU 上第一次 release 构建约 10 分钟，
失败信息先看 `--- stderr`。

### 2.3 Docker 与 OSWorld 镜像

```bash
apt-get install -y docker.io        # 或官方脚本；用 engine 即可，不需要 Desktop
docker info | head -5
docker pull happysixd/osworld-docker   # provider 缺镜像时也会自动拉，先拉能把网络问题提前暴露
```

### 2.4 OSWorld 与 Python 环境

```bash
git clone https://github.com/xlang-ai/OSWorld $OSW
cd $OSW

# 上游 pyproject 里有一个坏掉的 editable 路径依赖，先删掉再装：
cp pyproject.toml pyproject.toml.orig
#   1) dependencies 里删掉：  "agp-client",
#   2) 文件末尾删掉这一段：
#      [tool.uv.sources]
#      agp-client = { path = "mm_agents/surferH/agp_client", editable = true }

uv venv --python 3.12
uv pip install -e .
.venv/bin/python -c "import desktop_env; print('desktop_env ok')"
```

`mm_agents/surferH/agp_client` 没有 `pyproject.toml`/`setup.py`，`uv pip install -e .`
会在解析阶段失败——这是上游的问题，与本项目无关，删掉即可（保留 `.orig` 备份，
以后同步上游时能一眼看出改了什么）。

### 2.5 VM 镜像

```bash
cd $OSW
mkdir -p docker_vm_data
HF_TOKEN=$(cat /root/.hf_token)      # 见 2.6
curl -L --fail --retry 3 -H "Authorization: Bearer $HF_TOKEN" \
  -o docker_vm_data/Ubuntu.qcow2.zip \
  https://huggingface.co/datasets/xlangai/ubuntu_osworld/resolve/main/Ubuntu.qcow2.zip
unzip -o docker_vm_data/Ubuntu.qcow2.zip -d docker_vm_data/
rm docker_vm_data/Ubuntu.qcow2.zip
ls -lh docker_vm_data/Ubuntu.qcow2     # ≈ 12 GiB
```

- 文件名就是 `Ubuntu.qcow2.zip`（12,273,896,463 字节），解开得到同目录的 `Ubuntu.qcow2`。
- 匿名下载会被限速/拒绝，带 token。
- Docker provider 把这个文件**只读** bind 到容器里的 `/System.qcow2`：
  `--path-to-vm` 必须是这个 `.qcow2` 的路径，给 `.vmx` 是 VMware provider 的用法。

### 2.6 密钥：走 stdin，不进命令行

```bash
# 在笔记本上执行（不是登录服务器后执行）；粘贴后按 Ctrl-D，文件落在服务器上
ssh $BENCH_HOST 'umask 077; cat > /root/.hf_token'       # 12 GiB 镜像下载用
ssh $BENCH_HOST 'umask 077; cat > /root/.deepseek_key'   # 只有 --smoke 的一题评分用
```

本地有现成文件时用管道，连粘贴都省了（`hf.secret` 第一行是账号，第二行才是 token）：

```bash
sed -n '2p' hf.secret | tr -d '[:space:]' | ssh $BENCH_HOST 'umask 077; cat > /root/.hf_token'
```

三个细节：

- `umask 077` 让落地文件是 `-rw-------`；这不是可选项，密钥文件不该让同机其他用户读到。
- 走 stdin 是因为 `ssh host "echo $KEY > f"` 会把密钥塞进命令行——本地 shell 历史、
  服务器上的 `ps aux` 都能看到。
- 别先 `ssh` 进去再 `echo`：那样服务器 shell 历史里也多一条。

验证（`-rw------- 1 root root` 就对了）：

```bash
ssh $BENCH_HOST 'ls -l /root/.hf_token /root/.deepseek_key'
```

syscity 取 key 的顺序（`src/bench/run.rs` `build_provider`）：

1. `--api-key` 参数；
2. `~/.syscity/config.toml`（或 `./config.toml`、`./.config/config.toml`）里
   `[providers.<名字>].api_key`；
3. `ANTHROPIC_API_KEY`，然后 `OPENAI_API_KEY`。

base URL 的顺序是 `--base-url` → 配置里的 `[providers.<名字>].base_url` → `SYSCITY_BASE_URL`。
**harness 不会传 `--api-key`**（`run.py` 只把 `--provider/--model/--base-url` 转给 syscity），
所以密钥只能走环境变量或配置文件。DeepSeek 是 OpenAI 兼容协议，用环境变量最短：

```bash
export OPENAI_API_KEY=$(cat /root/.deepseek_key)
export SYSCITY_BASE_URL=https://api.deepseek.com/v1
```

要写在配置文件里就这样（注意 `provider_type` 不能省，字段名不是 `type`）：

```toml
# /root/.syscity/config.toml
model_provider = "deepseek"
model = "deepseek-v4-flash"

[providers.deepseek]
provider_type = "open_ai"
base_url = "https://api.deepseek.com/v1"
api_key = "<key>"
```

配置文件里错一个字段，`GatewayConfig` 会整份解析失败并被**静默忽略**（只有日志里一条
warn），表现是"没有 key"——所以第一次跑完一题就去看 `<result-dir>`，别等一小时之后。

---

## 三、验收：VM 能起来、桌面能应答

### 3.1 容器能力自检

```bash
cd $OSW
.venv/bin/python scripts/validate_docker_capabilities_runtime.py
```

它会起一个容器、等 QEMU 里的 VM 就绪，检查 guest_execute、截图字节、noVNC 200、
guest HTTPS 200，并覆盖"引擎不支持 `cap_add`"时的回退路径。期望最后一行：

```
PR660_FINAL_RESULT {"all_passed": true, "kvm_exists": true}
```

`kvm_exists: false` 就别往下走了——先解决 `/dev/kvm`。

### 3.2 探针（不花模型钱）

先拿到这台 VM 的服务端口。Docker provider **动态分配**端口（从 5000/8006/9222/8080
开始找空闲的，冲突就顺延），不要写死 5000：

```bash
cd $OSW
cat > /tmp/hold_vm.py <<'PY'
from desktop_env.desktop_env import DesktopEnv
env = DesktopEnv(provider_name="docker", path_to_vm="docker_vm_data/Ubuntu.qcow2",
                 action_space="pyautogui", os_type="Ubuntu", headless=True,
                 screen_size=(1920, 1080), require_a11y_tree=False)
print("PORT", env.server_port, flush=True)
input("Enter 结束\n")
env.close()
PY
.venv/bin/python /tmp/hold_vm.py      # 首次启动 1–2 分钟
```

另开一个 shell，把 `PORT` 设成上面打印的数字：

```bash
export PORT=<打印出来的数字>
$BIN bench probe --computer-url http://127.0.0.1:$PORT --execute
```

四个端点各一行，最后一行 `ok` / `not ready`；失败会点名是 `/screen_size`、
`/screenshot`、`/accessibility` 还是 `/execute`。`--json` 给脚本用。

值得留心的是 warning 而不是 failure：`/accessibility` 空树（Electron/Canvas 应用）
意味着只能走坐标；截图尺寸和 `/screen_size` 对不上意味着坐标会偏——后者要调
`--screen-width/--screen-height` 跟 VM 实际分辨率对齐。

---

## 四、跑任务

所有命令都建议放在 `tmux` 或 `nohup` 里：SSH 会话断掉（我们遇到过多次 exit 255）
不该等于一次跑掉一半的评测。

### 4.1 一题（smoke）

```bash
cd $OSW
export OPENAI_API_KEY=$(cat /root/.deepseek_key)
export SYSCITY_BASE_URL=https://api.deepseek.com/v1

# 取一个 task id
TASK_ID=$(.venv/bin/python -c \
  "import json;d=json.load(open('evaluation_examples/test_small.json'));print(d['chrome'][0])")
echo "$TASK_ID"

.venv/bin/python $SYS/bench/osworld/run.py \
  --osworld-root . \
  --mode step \
  --provider-name docker \
  --path-to-vm docker_vm_data/Ubuntu.qcow2 \
  --split test_small --domain chrome --ids "$TASK_ID" \
  --max-steps 60 --timeout 1800 --headless \
  --syscity $BIN \
  --provider deepseek --model deepseek-v4-flash \
  --result-dir $RUNS/smoke
```

跑完直接看 `$RUNS/smoke/summary/syscity.json`。`scored: 1` 说明这一题真的跑完并被评分；
`infra_errors: 1` 说明 syscity 或 VM 出了问题，去 `$RUNS/smoke/chrome/<id>/` 看
`serve.stderr.log` 和 `result.json`。

### 4.2 test_small（39 题，10 个 domain）

```bash
mkdir -p $RUNS
nohup .venv/bin/python $SYS/bench/osworld/run.py \
  --osworld-root . \
  --mode step \
  --provider-name docker --path-to-vm docker_vm_data/Ubuntu.qcow2 \
  --split test_small \
  --max-steps 60 --timeout 1800 --headless \
  --syscity $BIN \
  --provider deepseek --model deepseek-v4-flash \
  --result-dir $RUNS/test_small-$(date +%F) \
  > $RUNS/test_small.log 2>&1 &
tail -f $RUNS/test_small.log
```

中断了用同一个 `--result-dir` 加 `--resume`：已经有 `result.txt` 的题会跳过。

### 4.3 全量（369 题）

同上，`--split test_all`。369 题串行是"天"级，按 domain 分段跑更实际：

十个 domain 及题数：`chrome` 46、`gimp` 26、`libreoffice_calc` 47、
`libreoffice_impress` 47、`libreoffice_writer` 23、`multi_apps` 101、`os` 24、
`thunderbird` 15、`vlc` 17、`vs_code` 23。

```bash
for d in chrome gimp libreoffice_calc libreoffice_impress libreoffice_writer \
         multi_apps os thunderbird vlc vs_code; do
  .venv/bin/python $SYS/bench/osworld/run.py \
    --osworld-root . --mode step --headless \
    --provider-name docker --path-to-vm docker_vm_data/Ubuntu.qcow2 \
    --split test_all --domain "$d" --max-steps 60 --timeout 1800 \
    --syscity $BIN --provider deepseek --model deepseek-v4-flash \
    --result-dir $RUNS/all-$d || echo "domain $d failed"
done
```

一次只跑一个进程：端口和容器是独占的，两个 runner 并行只会互相抢端口。
另有 `test_nogdrive`（361，剔除 GDrive 依赖）和 `test_infeasible`（25，验证"该拒绝时
拒绝"，`FAIL` 在这里算 1 分）两个分片。

### 4.4 replay 还是 step

| | `--mode replay`（默认） | `--mode step` |
|---|---|---|
| 谁执行动作 | syscity 先想完整套，harness 事后按顺序重放 | harness 每步执行一次，再让 syscity 看下一屏 |
| 进程 | 一题一个 `bench run` | 一题一个常驻 `bench serve` |
| 需要等待/异步加载的题 | 被 `--sleep-after-execution`（默认 0.5 s）拍平，会低估 | 顺序和 OSWorld 自己的循环一致 |
| 建议 | 快速回归 | 正式分数 |

---

## 五、看结果

`$RUNS/<名字>/<domain>/<id>/` 下：

| 文件 | 谁写的 | 看什么 |
|---|---|---|
| `result.json` | syscity | `actions`（replay 用）、`answer`、`error`、步数、证据档位 |
| `serve.stderr.log` | syscity（step） | 协议错误、panic；空文件就是没抱怨 |
| `traj.jsonl`、`step_*.png`、`recording.mp4` | OSWorld | 逐步截图和录屏，判断"错在哪一步" |
| `result.txt` | OSWorld evaluator | 官方分数 0/1 |
| `error.json` | OSWorld | 这一题被算成 infrastructure error 的原因 |

`$RUNS/<名字>/summary/syscity.json`：`scored`、`passed`、`success_rate`、`infra_errors`、
`mode`、`split`、`provider`、`model`。**infra error 不在分母里**——一次网络抖动不该
被读成模型变差。`run.py` 的退出码只看"跑没跑起来"，不看分数。

---

## 六、故障对照表

| 现象 | 原因 | 处理 |
|---|---|---|
| `apt` 卡死，`Could not get lock /var/lib/dpkg/lock-frontend` | `unattended-upgrade` 持有锁（我们见过它 100% CPU 空转 11 分钟） | `ps aux \| grep unattended` → kill；`rm -f /var/lib/dpkg/lock*`；`dpkg --configure -a`；再 `apt-get update` |
| `libssl-dev` 404 | 包列表过期 | `apt-get update` 后重试 |
| `openssl-sys` 构建失败 | 缺 `libssl-dev` 或 `pkg-config` | 见 2.1 |
| `alsa-sys` / `x11` / `xdo` 的 `pkg-config` 报找不到 | 缺 GUI/音频开发包 | 见 2.1 的包表 |
| `llama-cpp-sys-2` panic：`Unable to find libclang` | `bindgen` 需要 libclang，且不在默认搜索路径 | `apt-get install libclang-dev` 并 `LIBCLANG_PATH=/usr/lib/llvm-14/lib` |
| `uv pip install -e .` 说 `agp-client` 不是合法包 | 上游 pyproject 的 editable 路径依赖指向没有 pyproject 的目录 | 删依赖行和 `[tool.uv.sources]` 段（见 2.4） |
| 容器起不来，日志提示 KVM 不可用 | 宿主没有 `/dev/kvm` 或没权限 | 换开了嵌套虚拟化的机型/裸金属；确认容器带了 `--device /dev/kvm`（provider 自己加） |
| `docker create` 报 capabilities 不支持（非特权 docker-in-docker） | 引擎不允许 `cap_add: NET_ADMIN` | provider 会自动回退到用户态网络并转发 5000/9222/8080；这条路径已实测验过，只需确认端口能通 |
| 探针全绿但动作点偏 | 截图尺寸 ≠ `/screen_size` | 按探针 warning 调整 `--screen-width/--screen-height` |
| 全部算成 infra error | syscity 起不来：路径错、没 key、provider 解析失败 | 手工跑一次 `$BIN bench run --instruction ... --computer-url ...` 看 stderr；再看 `serve.stderr.log` |
| 端口连不上 | 端口是动态分配的 | 从 `PORT <n>` 或日志 `Started container with ports - VNC: …, Server: …` 取真实端口 |
| 容器越积越多、磁盘吃满 | 中断后没清理 | `docker stop $(docker ps -q) && docker rm $(docker ps -a -q)`；删 `.qcow2.zip` 和旧 results 里的 png/mp4 |
| SSH 中途 exit 255 | 连接抖动 | 用 `tmux`/`nohup` 跑，断线只影响观察不影响评测 |
| `$BIN --version`：`version 'GLIBC_2.39' not found` | 装的是 24.04 runner 编的旧产物（v0.3.14 及更早） | 换 v0.3.16+：`SYSCITY_VERSION=v0.3.16`，或删掉 `$BIN` 后 `--refetch` |
| `$BIN`：`libgomp.so.1: cannot open shared object file` | 最小化安装的系统没有 OpenMP 运行库（llama.cpp 需要） | `apt-get install -y libgomp1`；`ldd $BIN \| grep 'not found'` 应只剩空 |
| 下 GitHub 超时 / 403 | 网络 | 先 `export https_proxy=…`，或 `SYSCITY_BIN_URL=<镜像>`；`file://` 也可 |
| `sha256sum -c` 失败 | 下载被截断，或代理返回了错误页 | `--refetch` 重下；还失败就换源 |
| `run.py` 不存在 | 只装了二进制，没取 `bench/` 胶水 | 重跑脚本（会自动取同 tag 的 `bench/`），见 2.2 |

Docker provider **没有快照**（`save_state` 直接 `NotImplemented`）：`revert_to_snapshot`
等于停掉容器重建，所以每道题都要重新等 QEMU 启动 1–2 分钟。这是 OSWorld 的行为，
不是配置项，估算时间时要算进去。

---

## 七、成本与时间（估算，不是实测）

单题：容器重启 + 启动 1–2 min，加 60 步预算 × 每步（截图 + a11y 树 + 一轮模型）10–30 s
≈ 10–25 min。于是：

| 范围 | 题数 | 串行估算 |
|---|---|---|
| smoke（单题） | 1 | 15–30 min |
| `test_small` | 39 | 8–16 h |
| `test_all` | 369 | 3–7 天 |

所以顺序永远是 探针 → 一题 → `test_small` → 全量，别反过来。省钱的三件事：
探针不花模型钱；`--max-steps` 是真的预算上限；先只跑一个 domain 看趋势。

准备阶段本身现在很快：不编 Rust、不装工具链，二进制 47 MB 十分钟内下完；
真正的时间花在 12 GiB 镜像和每一题的 VM 重启上。

主分数用哪个模型（`deepseek-v4-flash` 无 vision，只有 a11y 下限分；接 vision 模型是
官方口径）见 `computer-use-benchmark.md` §六 的两个待定项——两种都要记，报告里分开列。

---

## 八、收尾

```bash
docker ps --format '{{.ID}} {{.Image}} {{.Status}}'
docker stop $(docker ps -q) && docker rm $(docker ps -a -q)   # 中断过的残留
```

结果目录（`$RUNS`）保留，`docker_vm_data/Ubuntu.qcow2` 保留（重建要再下 12 GB）。
一个已评分的 run 至少值得留：`summary/syscity.json` + 每题的 `result.txt`；
`step_*.png`/`recording.mp4` 是排障用的，占空间可以后清。
