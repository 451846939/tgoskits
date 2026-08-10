# AI 与 RTOS 协同演示脚本使用说明

`scripts/ai-rtos/` 集中提供 QEMU/AxVisor 双 Guest 启动、RTOS Guest 构建、AICP 网络协议验证、YOLOv8 控制闭环、实时性 A/B、原生 RTOS 基线、长时间稳定性和结果汇总脚本。

普通复现从 [`aicp.sh`](aicp.sh) 进入即可。目录中的 `run_*.sh`、`build_*.sh`、`check_*.sh` 和数据处理脚本用于专项验证、失败重跑或底层调试，不需要按文件名逐个执行。

完整架构、代码修改、协议设计、实测数据和评分项对应关系见：

- [完整全流程实现与复现手册](../../docs/ai-rtos/完整全流程实现与复现手册.md)
- [开发记录与问题排查](../../docs/ai-rtos/开发记录与问题排查.md)
- [演示应用源码说明](../../apps/ai-rtos-demo/README.md)

## 1. 支持范围

主线数据通道为 **AICP v1 over TCP/IP**。Linux 和 StarryOS 作为智能计算 Guest，ArceOS、RT-Thread、Zephyr 和 FreeRTOS 作为控制 Guest，可组成以下八组双 Guest 闭环：

| 智能计算 Guest | 控制 Guest | 网络通道 | 启动入口 |
| --- | --- | --- | --- |
| Linux，2 vCPU | ArceOS | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run linux arceos` |
| Linux，2 vCPU | RT-Thread | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run linux rtthread` |
| Linux，2 vCPU | Zephyr | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run linux zephyr` |
| Linux，2 vCPU | FreeRTOS | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run linux freertos` |
| StarryOS | ArceOS | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run starry arceos` |
| StarryOS | RT-Thread | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run starry rtthread` |
| StarryOS | Zephyr | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run starry zephyr` |
| StarryOS | FreeRTOS | TCP/IP，隔离 QEMU 二层 hub | `aicp.sh run starry freertos` |

ArceOS UDP 路径只用于 ACK、超时、重传、重复包和乱序等故障注入验证，不替代 TCP/IP 主线。vsock、共享内存、HyperCall 和裸 MMIO 不作为主要业务数据通道。

## 2. 运行前提

所有命令均从仓库根目录执行：

```sh
cd /path/to/tgoskits
```

先检查宿主工具：

```sh
scripts/ai-rtos/aicp.sh doctor
```

必需工具包括：

- Rust/Cargo、`cargo-make`、Python 3、Git、CMake、Ninja、Perl。
- `qemu-system-aarch64`、`dtc`、`fdtoverlay`、`fdtget`。
- `mkfs.ext4`、`debugfs`、`e2fsck`、`resize2fs`。
- `cpio`、`gzip`、`timeout`、`lsof`、`curl`、`tar`。
- Rust YOLOv8 AArch64 运行包首次构建时需要 Docker。
- Zephyr 与部分 Guest 构建需要 AArch64 交叉编译器；bare-metal 工具链可由脚本下载。

工具解析遵循“显式环境变量优先，随后从 `PATH` 查找”的规则，不依赖固定的操作系统安装路径。例如：

```sh
export DEBUGFS="$(command -v debugfs)"
export E2FSCK="$(command -v e2fsck)"
export RESIZE2FS="$(command -v resize2fs)"
```

Homebrew 的 keg-only e2fsprogs 可通过动态前缀设置，不需要在脚本中写死 `/opt/homebrew` 或 `/usr/local`：

```sh
export PATH="$(brew --prefix e2fsprogs)/bin:$(brew --prefix e2fsprogs)/sbin:$PATH"
```

## 3. 最短复现路径

### 3.1 环境检查与镜像准备

```sh
scripts/ai-rtos/aicp.sh doctor
scripts/ai-rtos/aicp.sh prepare
```

`prepare` 拉取 QEMU AArch64 基础镜像，并生成 Zephyr/ArceOS 基础 Guest 配置。RT-Thread、Zephyr 网络 Guest 和 FreeRTOS 网络 Guest 会在对应双 Guest 脚本中按需构建。

### 3.2 八组双 Guest 最小验证

```sh
scripts/ai-rtos/aicp.sh smoke
```

该命令依次执行工程检查、主机协议测试以及 Linux/StarryOS 与四种控制 Guest 的八组最小闭环。每组必须出现严格完成标记且失败计数为零。

### 3.3 完整 QEMU 验收流程

```sh
scripts/ai-rtos/aicp.sh full
```

`full` 在八组闭环之外继续执行：

1. Shell、Python、架构边界和第三方源码洁净检查。
2. AICP 主机协议可靠性测试。
3. 固定参数与轻量神经网络控制效果对比。
4. RT-Thread TCP 可靠性和异常恢复测试。
5. StarryOS UDP 丢包、重传、重复包和乱序恢复测试。
6. Linux Rust YOLOv8n CPU + RT-Thread 控制闭环。
7. AxVisor 实时优化前后 idle/stress A/B。
8. RT-Thread、Zephyr、FreeRTOS 原生 20 ms 周期基线。

完整验证耗时较长。可先查看阶段计划而不运行：

```sh
AICP_FULL_DRY_RUN=1 scripts/ai-rtos/aicp.sh full
```

### 3.4 单独运行一组双 Guest

```sh
scripts/ai-rtos/aicp.sh run <linux|starry> <arceos|rtthread|zephyr|freertos> [次数] [ai|fixed] [超时秒数]
```

示例：

```sh
scripts/ai-rtos/aicp.sh run linux rtthread 40 ai 240
scripts/ai-rtos/aicp.sh run linux zephyr 20 fixed 240
scripts/ai-rtos/aicp.sh run starry freertos 20 ai 300
```

参数含义：

| 参数 | 含义 |
| --- | --- |
| `次数` | AICP CONTROL 请求次数，必须为正整数 |
| `ai` | 使用模型输出动态调整控制参数 |
| `fixed` | 使用固定参数，作为控制效果基线 |
| `超时秒数` | 等待 Guest 启动和关键完成标记的最大时间 |

### 3.5 YOLOv8 CPU 闭环

```sh
scripts/ai-rtos/aicp.sh yolov8 420
```

该入口运行 Linux 2-vCPU Guest 中的 Rust YOLOv8n + ONNX Runtime CPU 推理，将检测结果通过 AICP/TCP 发送给 RT-Thread Guest。RT-Thread 执行控制更新并回传 STATUS，形成“图片输入、模型推理、网络传输、控制动作、状态回传”闭环。

首次执行默认使用 Docker 构建 AArch64 运行包。已有有效产物时可跳过构建：

```sh
AICP_YOLO_RUST_SKIP_BUILD=1 scripts/ai-rtos/aicp.sh yolov8 420
```

### 3.6 AxVisor 实时优化 A/B

```sh
scripts/ai-rtos/aicp.sh realtime 300 360 2
```

三个参数依次为：请求次数、Guest 启动超时、Linux 压力进程数。该入口执行一轮基线/优化对比。需要多轮交替执行时直接调用底层脚本：

```sh
scripts/ai-rtos/run_axvisor_rt_before_after.sh 300 360 2 3
```

四个参数依次为：

```text
iterations boot_timeout_seconds stress_procs rounds
```

默认对比配置：

| 变体 | AxVisor board config | 含义 |
| --- | --- | --- |
| `baseline` | `os/axvisor/configs/board/qemu-aarch64-rt-shared-wait-baseline.toml` | 恢复共享 vCPU 等待队列和广播唤醒关键路径 |
| `optimized` | `os/axvisor/configs/board/qemu-aarch64-rt.toml` | 每 vCPU 等待队列、定向唤醒、IRQ 路由和亲和性优化路径 |

覆盖默认配置：

```sh
AICP_BASELINE_BOARD_CONFIG=path/to/baseline.toml \
AICP_OPTIMIZED_BOARD_CONFIG=path/to/optimized.toml \
scripts/ai-rtos/run_axvisor_rt_before_after.sh 300 360 2 3
```

这里的基线是在同一代码树、同一镜像、同一负载下恢复未优化关键路径的受控配置，不需要切换历史 commit。多轮运行会交替 baseline/optimized 顺序，降低先后顺序对 QEMU 调度的影响。

### 3.7 原生 RTOS 周期基线

```sh
scripts/ai-rtos/aicp.sh baseline rtthread
scripts/ai-rtos/aicp.sh baseline zephyr
scripts/ai-rtos/aicp.sh baseline freertos
scripts/ai-rtos/aicp.sh baseline all
```

三种基线均运行 20 ms 周期任务，分别采集 idle 和 CPU stress 场景的平均绝对抖动、p99、最大抖动和超期次数。它们用于和 AxVisor 下的同类周期任务对照，不能直接当作真实开发板的硬实时上界。

### 3.8 协议可靠性

```sh
scripts/ai-rtos/aicp.sh reliability 20 240
```

两个参数分别为请求次数和启动超时。该测试在 Linux + RT-Thread 双 Guest 上验证分帧、超时、断连重连、重复请求和异常恢复，成功条件包括：

```text
AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0
AICP_LINUX_DONE ok=<N> failed=0
```

## 4. 主入口命令字典

```sh
scripts/ai-rtos/aicp.sh --help
scripts/ai-rtos/aicp.sh list
```

| 命令 | 参数 | 用途 | 主要输出 |
| --- | --- | --- | --- |
| `doctor` | 无 | 检查宿主工具、ext4 工具和交叉编译器 | 终端检查表 |
| `prepare` | 无 | 拉取基础镜像，生成 Zephyr/ArceOS 配置 | `tmp/images/`、`tmp/ai-rtos/*.generated.toml` |
| `smoke` | 无 | 八组双 Guest 最小闭环和工程检查 | `tmp/ai-rtos/results/full-validation-<timestamp>/` |
| `full` | 无 | 完整 QEMU 验收流程 | 同上，包含每个阶段独立日志 |
| `run` | Guest 组合和可选运行参数 | 单组合演示、调试和失败重跑 | `tmp/ai-rtos/logs/` 及组合结果目录 |
| `yolov8` | `[超时秒数]` | Rust YOLOv8n CPU + RT-Thread 闭环 | Linux/RT-Thread 日志和结果摘要 |
| `realtime` | `[次数] [超时] [压力进程数]` | 一轮 AxVisor 优化前后 A/B | `rt-before-after-<timestamp>/` |
| `baseline` | `rtthread|zephyr|freertos|all` | 原生 RTOS 周期基线 | `<rtos>-periodic-<timestamp>/` |
| `reliability` | `[次数] [超时]` | RT-Thread TCP 可靠性和恢复 | `rtthread-reliability/summary.txt` |
| `list` | 无 | 显示脚本职责分类 | 终端脚本索引 |

## 5. 双 Guest 运行关系

默认四核拓扑如下：

| pCPU | 用途 |
| --- | --- |
| pCPU0 | AxVisor housekeeping 和后台工作 |
| pCPU1 | 控制 Guest vCPU0 |
| pCPU2 | Linux/StarryOS vCPU0 |
| pCPU3 | Linux/StarryOS vCPU1 |

可以通过以下环境变量调整：

```sh
export AICP_HOST_CPUS=4
export AICP_HOUSEKEEPING_PCPU=0
export AICP_RTOS_VCPU0_PCPU=1
export AICP_LINUX_VCPU0_PCPU=2
export AICP_LINUX_VCPU1_PCPU=3
```

默认网络拓扑是同一 QEMU 进程中的隔离二层 hub：

```text
Linux/StarryOS 10.0.3.3:临时端口
          |
          | AICP v1 / TCP/IP
          |
控制 Guest 10.0.3.2:8800
```

RT-Thread 和 Zephyr 的部分配置使用 `10.0.2.14/10.0.2.15` 地址对，具体地址、MAC、virtio-mmio 槽位、IRQ、内存区域和生成后的 VM 配置会写入 `tmp/ai-rtos/*.generated.toml`、设备树和运行日志。网络隔离测试会检查 Guest 设备所有权、地址规划和主数据通道类型。

## 6. 完整验证的配置

`aicp.sh smoke` 和 `aicp.sh full` 由 [`run_full_qemu_validation.sh`](run_full_qemu_validation.sh) 编排。可使用以下变量控制运行规模：

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `AICP_FULL_PREPARE_IMAGES` | `1` | 是否下载并准备 QEMU 镜像 |
| `AICP_FULL_ITERATIONS` | smoke=`1`，full=`20` | 每组双 Guest 请求次数 |
| `AICP_FULL_PROTOCOL_ITERATIONS` | smoke=`5`，full=`50` | 主机协议测试次数 |
| `AICP_FULL_BOOT_TIMEOUT` | smoke=`300`，full=`420` | 每组 Guest 启动超时秒数 |
| `AICP_FULL_STRESS_PROCS` | `2` | 实时压力场景中的 Linux busy worker 数量 |
| `AICP_FULL_INCLUDE_LONG_STABILITY` | `0` | 是否追加 1000/10000 次长稳 |
| `AICP_FULL_DRY_RUN` | `0` | `1` 时只打印阶段计划 |

示例：复用已经准备好的镜像，并追加长稳：

```sh
AICP_FULL_PREPARE_IMAGES=0 \
AICP_FULL_INCLUDE_LONG_STABILITY=1 \
AICP_FULL_BOOT_TIMEOUT=900 \
scripts/ai-rtos/aicp.sh full
```

每个阶段都写入独立日志。某一阶段失败时，`summary.txt` 会记录 `failed_stage`、`failed_command` 和 `failed_log`，可直接执行对应命令重跑，不必重复整套流程。

## 7. Guest 构建脚本

### 7.1 RT-Thread

```sh
scripts/ai-rtos/build_rtthread_aicp_guest.sh
```

默认使用 RT-Thread `v5.2.1`。脚本将上游 BSP 复制到独立 build 目录，再加入仓库自有 AICP 应用和 virtio 适配，不写回第三方源码树。

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `RTTHREAD_REVISION` | `v5.2.1` | 上游 tag 或 commit |
| `RTTHREAD_SOURCE_DIR` | `tmp/rt-thread-<revision>` | 只读上游源码目录 |
| `RTTHREAD_BUILD_DIR` | `tmp/rtthread-aicp-build` | 独立构建目录 |
| `RTTHREAD_CC_PREFIX` | 自动解析/下载 | AArch64 bare-metal 编译器前缀 |
| `RTTHREAD_RAM_BASE` | `0x40000000` | Guest RAM 基址 |
| `RTTHREAD_GIC_VERSION` | `2` | GIC 版本，可设为 `2` 或 `3` |
| `RTTHREAD_VIRTIO_MMIO_BASE` | `0x0a000000` | virtio-mmio 扫描基址 |
| `RTTHREAD_VIRTIO_MAX_NR` | `32` | 最大 virtio-mmio 槽位数 |
| `RTTHREAD_VIRTIO_IRQ_BASE` | `48` | virtio IRQ 基号 |

输出：

```text
<RTTHREAD_BUILD_DIR>/rtthread.bin
<RTTHREAD_BUILD_DIR>/rtthread.elf
```

### 7.2 Zephyr

```sh
ZEPHYR_BASE=/path/to/zephyr \
ZEPHYR_TOOLCHAIN_VARIANT=cross-compile \
CROSS_COMPILE=/path/to/aarch64-linux-musl- \
scripts/ai-rtos/build_zephyr_aicp_guest.sh qemu_cortex_a53
```

默认要求 Zephyr `v4.4.0` 且源码工作树干净。脚本只构建 [`apps/ai-rtos-demo/zephyr`](../../apps/ai-rtos-demo/zephyr)，不修改 Zephyr 源码。

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `ZEPHYR_BASE` | 无 | Zephyr 源码目录，必填 |
| `WEST` | `west` | west 命令路径 |
| `ZEPHYR_BUILD_DIR` | `apps/ai-rtos-demo/zephyr/build` | 构建目录 |
| `ZEPHYR_REQUIRED_REF` | `v4.4.0` | 要求的上游 tag 或 commit |
| `AICP_ZEPHYR_PROFILE` | `e1000` | `e1000` 或 `axvisor-virtio` |
| `CROSS_COMPILE` | 无 | AArch64 交叉编译器前缀 |

输出：

```text
<ZEPHYR_BUILD_DIR>/zephyr/zephyr.bin
<ZEPHYR_BUILD_DIR>/zephyr/zephyr.elf
```

### 7.3 FreeRTOS

```sh
scripts/ai-rtos/build_freertos_aicp_guest.sh
```

网络 Guest 使用 FreeRTOS Kernel、FreeRTOS+TCP 和仓库自有板级/virtio/AICP 适配。第三方仓库放在 `tmp/`，应用代码位于 [`apps/ai-rtos-demo/freertos`](../../apps/ai-rtos-demo/freertos)。

主要环境变量：

| 环境变量 | 含义 |
| --- | --- |
| `FREERTOS_BUILD_DIR` | 独立构建目录 |
| `FREERTOS_KERNEL_DIR` / `FREERTOS_SOURCE_DIR` | FreeRTOS Kernel 源码目录 |
| `FREERTOS_PLUS_TCP_DIR` | FreeRTOS+TCP 源码目录 |
| `FREERTOS_KERNEL_REQUIRED_REF` | Kernel 固定 tag 或 commit |
| `FREERTOS_PLUS_TCP_REQUIRED_REF` | FreeRTOS+TCP 固定 tag 或 commit |
| `CROSS_COMPILE` | AArch64 bare-metal 编译器前缀 |
| `AICP_FREERTOS_BASELINE` | `ON` 时构建原生周期基线 |
| `AICP_FREERTOS_STRESS` | 基线是否启用 CPU 压力任务 |

输出：

```text
<FREERTOS_BUILD_DIR>/aicp-freertos.bin
<FREERTOS_BUILD_DIR>/aicp-freertos.elf
```

## 8. 脚本字典

### 8.1 用户入口与完整编排

| 脚本 | 作用 | 是否通常直接执行 |
| --- | --- | --- |
| `aicp.sh` | 统一中文命令入口 | 是 |
| `run_full_qemu_validation.sh` | 编排 smoke/full 全流程并保存阶段日志 | 需要细调环境变量时 |
| `setup_qemu_rtos.sh` | 拉取镜像并生成 Zephyr/ArceOS 基础配置 | 通常由 `prepare` 调用 |
| `run_axvisor_all_guest_modes.sh` | Linux/StarryOS 与 ArceOS 的 fixed/ai 四种模式对照 | 专项对照时 |

### 8.2 Guest 构建

| 脚本 | 作用 | 主要产物 |
| --- | --- | --- |
| `build_rtthread_aicp_guest.sh` | 从干净的 RT-Thread 上游工作树复制 BSP，并构建 AICP/virtio 网络 Guest | `rtthread.bin`、`rtthread.elf` |
| `build_zephyr_aicp_guest.sh` | 使用指定 Zephyr workspace 构建仓库自有 AICP 应用 | `zephyr.bin`、`zephyr.elf` |
| `build_freertos_aicp_guest.sh` | 构建 FreeRTOS Kernel + FreeRTOS+TCP + 仓库自有板级和 AICP 适配 | `aicp-freertos.bin`、`aicp-freertos.elf` |

### 8.3 双 Guest 主线

| 脚本 | 默认参数 | 作用 |
| --- | --- | --- |
| `run_axvisor_dual_guest_aicp.sh` | `40 ai 180` | Linux + ArceOS，C/Rust 客户端可选 |
| `run_axvisor_dual_guest_aicp_rust.sh` | 透传参数 | Linux + ArceOS，强制 Rust AICP 客户端 |
| `run_axvisor_dual_guest_aicp_usernet.sh` | `40 ai 150` | QEMU user-net/hostfwd 回退拓扑，不是默认主线 |
| `run_axvisor_linux_rtthread_aicp.sh` | `40 ai 180` | Linux + RT-Thread TCP 闭环 |
| `run_axvisor_linux_zephyr_aicp.sh` | `20 ai 240` | Linux + Zephyr TCP 闭环 |
| `run_axvisor_linux_freertos_aicp.sh` | `20 ai 240` | Linux + FreeRTOS TCP 闭环 |
| `run_axvisor_starry_rtos_aicp.sh` | `40 ai 180` | StarryOS + 可选控制 Guest；原生 RTOS 使用 TCP |
| `run_axvisor_starry_udp_faults.sh` | `20` | StarryOS + ArceOS UDP 故障注入 |
| `run_axvisor_dual_guest_compare.sh` | `30 180` | Linux + ArceOS fixed/ai 控制效果和延迟对比 |

直接调用 StarryOS 通用脚本时需设置控制 Guest：

```sh
AICP_STARRY_NATIVE=1 \
AICP_QEMU_NET_BACKEND=hub \
AICP_STARRY_TRANSPORT=tcp \
AICP_RTOS_GUEST=rtthread \
scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh 20 ai 300
```

### 8.4 AI 模型闭环

| 脚本 | 作用 |
| --- | --- |
| `run_axvisor_rtthread_yolov8_rust_aicp.sh` | 统一入口使用的 Rust YOLOv8n CPU + RT-Thread 闭环包装器 |
| `run_axvisor_yolov8_rust_aicp.sh` | Rust YOLOv8n CPU + ArceOS 闭环 |
| `run_axvisor_yolov8_cpu_aicp.sh` | C++ ONNX Runtime YOLOv8 CPU + ArceOS 闭环 |

`AICP_YOLO_RUST_SKIP_BUILD=1` 和 `AICP_YOLO_CPU_SKIP_BUILD=1` 用于复用已经生成的 AArch64 运行包。只有确认模型、运行库和可执行文件完整时才应跳过构建。

### 8.5 实时性与稳定性

| 脚本 | 默认参数 | 作用 |
| --- | --- | --- |
| `run_axvisor_realtime_matrix.sh` | `200 240 2` | 同一 AxVisor 配置下运行 idle/stress 两种场景 |
| `run_axvisor_rt_before_after.sh` | `300 360 2 1` | baseline/optimized A/B，可多轮交替 |
| `run_axvisor_long_stability.sh` | `10000 4200 2` | Linux + ArceOS 万次长稳 |
| `run_axvisor_rtthread_long_stability.sh` | `1000 900 2` | Linux + RT-Thread 长稳 |
| `run_rtthread_periodic_baseline.sh` | 无 | RT-Thread 原生 20 ms idle/stress 基线 |
| `run_zephyr_periodic_baseline.sh` | 无 | Zephyr 原生 20 ms idle/stress 基线 |
| `run_freertos_periodic_baseline.sh` | 无 | FreeRTOS 原生 20 ms idle/stress 基线 |
| `run_freertos_baseline.sh` | `60` | 旧 FreeRTOS AxVisor benchmark 表格提取，区别于周期基线 |

实时报告中的主要字段：

| 字段 | 含义 |
| --- | --- |
| Linux RTT | 智能计算 Guest 发出 CONTROL 到收到 STATUS 的请求响应时间 |
| RTOS service time | 控制 Guest 解析请求、更新控制状态和生成回复的服务时间 |
| request interval deviation | 闭环请求到达间隔相对目标节拍的偏差，包含客户端处理、日志和 sleep，不等同于纯 RTOS 周期抖动 |
| RTOS periodic jitter | 控制 Guest 内独立 20 ms 周期任务的唤醒偏差 |
| max / p99 | 最坏样本和 99 分位数，用于观察尾延迟 |

QEMU 结果用于验证改造方向、回归和可复现对比。开发板最终数据还会受到真实中断控制器、缓存、内存总线、NPU、外设和宿主调度差异影响。

### 8.6 协议、可靠性和控制效果

| 脚本 | 默认参数 | 作用 |
| --- | --- | --- |
| `run_aicp_smoke.sh` | 环境变量控制，默认 `50 ai` | 主机参考客户端/服务端基础协议 smoke |
| `run_aicp_protocol_reliability.sh` | `50` | 分帧、校验、超时、重连和异常输入测试 |
| `run_axvisor_rtthread_reliability.sh` | `20 180` | Linux + RT-Thread Guest 可靠性测试 |
| `test_aicp_delayed_server_reconnect.sh` | 环境变量控制 | 延迟启动服务端后的客户端重连回归 |
| `run_aicp_control_compare.sh` | `AICP_ITERATIONS=100` | fixed/ai 控制效果对比 |
| `check_aicp_network_isolation.py` | 命令行参数 | 检查 VM 配置、设备归属、IP 通道和运行日志 |
| `compare_control.py` | CSV 参数 | 计算响应时间、控制误差、稳定时间等对比指标 |

### 8.7 单 Guest 与启动 smoke

这些脚本用于定位 Guest 自身启动、网络栈或服务端问题，不等同于最终双 Guest 评分场景。

| 脚本 | 作用 |
| --- | --- |
| `run_arceos_aicp_qemu_smoke.sh` | ArceOS AICP 服务端直接 QEMU 启动 |
| `run_rtthread_aicp_smoke.sh` | RT-Thread AICP 服务端直接 QEMU + 主机客户端 |
| `run_zephyr_aicp_smoke.sh` | Zephyr AICP 服务端直接 QEMU + hostfwd 客户端 |
| `run_freertos_aicp_guest_smoke.sh` | AxVisor 下 FreeRTOS 启动、定时器、网络和 AICP 服务检查 |
| `run_starry_aicp_smoke.sh` | StarryOS 单 Guest + 宿主参考服务端 |
| `run_rtos_boot_smoke.sh` | `freertos|zephyr|arceos-aicp` 通用启动标记检查 |
| `run_axvisor_aicp_e2e.sh` | AxVisor ArceOS Guest + 宿主 C 客户端 hostfwd 测试 |
| `run_axvisor_aicp_e2e_rust.sh` | AxVisor ArceOS Guest + 宿主 Rust 客户端 hostfwd 测试 |

### 8.8 工程检查与自测

| 脚本 | 作用 |
| --- | --- |
| `check_shell_syntax.sh` | 对所有 Shell 脚本执行 Bash 语法检查 |
| `check_demo_architecture.sh` | 检查协议、服务状态机、模型和 OS glue 的边界，防止重复实现或系统特判进入公共层 |
| `check_third_party_sources_clean.sh` | 检查 Zephyr、RT-Thread、FreeRTOS 上游版本和工作树洁净状态 |
| `test_host_tools.sh` | 宿主工具和交叉编译器解析逻辑自测 |
| `test_run_artifacts.sh` | 双 Guest 日志归档逻辑自测 |
| `test_check_aicp_network_isolation.py` | 网络隔离检查器单元测试 |

### 8.9 数据提取与汇总

| 脚本 | 输入 | 输出 |
| --- | --- | --- |
| `extract_aicp_log.py` | QEMU/Guest 运行日志 | 请求、RTOS 服务、周期抖动、CPU 负载 CSV |
| `periodic_latency.py` | 周期时间戳或日志 | 周期抖动统计 |
| `summarize_latency.py` | 提取后的 CSV | 单次运行 p50/p95/p99/max 摘要 |
| `summarize_rt_before_after.py` | baseline/optimized 结果 | 单轮 A/B 对比报告 |
| `summarize_rt_multirun.py` | 多轮 A/B 目录 | 多轮中位数、范围和最坏值报告 |
| `extract_freertos_benchmark.py` | FreeRTOS benchmark 日志 | min/avg/max 文本摘要 |
| `compare_control.py` | fixed 与 ai CSV | 控制效果指标对比 |

这些工具通常由上层 runner 调用。直接执行时先查看帮助：

```sh
python3 scripts/ai-rtos/extract_aicp_log.py --help
python3 scripts/ai-rtos/summarize_latency.py --help
python3 scripts/ai-rtos/compare_control.py --help
```

### 8.10 公共 Shell 库

`lib/` 下文件只供其他脚本 `source`，不要直接运行：

| 文件 | 作用 |
| --- | --- |
| `lib/cpu_topology.sh` | 校验并生成 pCPU/vCPU 亲和性配置 |
| `lib/dtb.sh` | 设备树生成、覆盖和属性检查 |
| `lib/host_tools.sh` | PATH/环境变量工具解析和交叉工具链安装 |
| `lib/markers.sh` | 等待 Guest 启动、网络和完成标记 |
| `lib/process.sh` | QEMU 进程树清理和镜像占用等待 |
| `lib/run_artifacts.sh` | 归档 QEMU、Linux console 和合并日志 |
| `lib/third_party_source_guard.sh` | 第三方 tag/commit 与洁净工作树检查 |

## 9. 常用环境变量

### 9.1 运行规模与客户端

| 环境变量 | 含义 |
| --- | --- |
| `AICP_STRESS_PROCS` | Linux Guest 内 busy worker 数量，范围 `0..16` |
| `AICP_CLIENT_IMPL` | Linux + ArceOS 使用 `c` 或 `rust` 客户端 |
| `AICP_RUST_CLIENT_BIN` | 复用指定的 Rust AArch64 客户端二进制 |
| `AICP_AXVISOR_BOARD_CONFIG` | 覆盖某次运行的 AxVisor board config |
| `AICP_QEMU_TRACE` | FreeRTOS 路径启用额外 QEMU trace，值为 `0` 或 `1` |

### 9.2 网络与 StarryOS

| 环境变量 | 可选值/含义 |
| --- | --- |
| `AICP_QEMU_NET_BACKEND` | `hub` 或 `mcast`；正式矩阵使用 `hub` |
| `AICP_STARRY_TRANSPORT` | `tcp` 或 `udp`；原生 RTOS 组合要求 `tcp` |
| `AICP_RTOS_GUEST` | `arceos`、`rtthread`、`zephyr`、`freertos` |
| `AICP_STARRY_NATIVE` | 原生 RTOS 组合设为 `1` |
| `AICP_STARRY_CONNECT_RETRIES` | StarryOS 客户端连接重试次数 |
| `AICP_STARRY_UDP_RETRIES` | UDP 对比路径重试次数 |
| `AICP_UDP_DROP_EVERY` | UDP 故障注入每 N 包丢弃一次 |
| `AICP_HOST_PORT` | hostfwd 测试使用的宿主端口，默认 `18800` |

### 9.3 工具和源码位置

| 环境变量 | 含义 |
| --- | --- |
| `DEBUGFS` / `E2FSCK` / `RESIZE2FS` | ext4 工具命令路径 |
| `QEMU` | 覆盖 `qemu-system-aarch64` 路径 |
| `WEST` | Zephyr west 命令路径 |
| `ZEPHYR_BASE` / `ZEPHYR_BUILD_DIR` | Zephyr 源码和构建目录 |
| `RTTHREAD_SOURCE_DIR` / `RTTHREAD_BUILD_DIR` | RT-Thread 源码和构建目录 |
| `FREERTOS_KERNEL_DIR` / `FREERTOS_PLUS_TCP_DIR` | FreeRTOS 第三方源码目录 |
| `RTTHREAD_CC_PREFIX` / `CROSS_COMPILE` / `ZEPHYR_CROSS_COMPILE` | 交叉编译器前缀 |

交叉编译器前缀既可以是绝对路径前缀，也可以是 `PATH` 中可解析的命令前缀。例如：

```sh
export CROSS_COMPILE=aarch64-none-elf-
```

## 10. 输出目录与文件含义

运行时文件集中在 `tmp/ai-rtos/`，不会写入第三方源码目录：

| 路径 | 内容 |
| --- | --- |
| `tmp/ai-rtos/logs/` | QEMU、AxVisor、Guest console 原始日志 |
| `tmp/ai-rtos/results/` | 每次测试的结果目录、CSV 和摘要 |
| `tmp/ai-rtos/*.generated.toml` | 动态生成的 VM/QEMU 配置 |
| `tmp/ai-rtos/*.dtb` / `*.dts` | 动态生成的 Guest/Host 设备树 |
| `tmp/ai-rtos/*initramfs*` | Linux/StarryOS 测试用 initramfs 或 rootfs 中间产物 |
| `tmp/images/qemu-aarch64/` | QEMU AArch64 基础镜像包 |

完整验证结果结构：

```text
tmp/ai-rtos/results/full-validation-<timestamp>/
├── summary.txt        # 总体状态、失败阶段、运行参数
├── stages.tsv         # 阶段、PASS/FAIL、耗时、日志路径
└── logs/
    └── <stage>.log    # 每个阶段完整标准输出和错误输出
```

实时 A/B 结果结构：

```text
tmp/ai-rtos/results/rt-before-after-<timestamp>/
├── baseline/          # 单轮时的未优化关键路径结果
├── optimized/         # 单轮时的优化路径结果
├── round-01/          # 多轮时按轮保存
├── round-02/
├── multirun.summary.txt
└── multirun.summary.md
```

各 runner 还会打印以下机器可读路径，供上层脚本归档：

```text
log=<qemu-log-path>
linux_console_log=<linux-console-log-path>
summary=<summary-path>
result_dir=<result-directory>
```

## 11. 成功判定

不能仅根据“QEMU 已启动”或日志仍在刷新判断测试通过。runner 会等待业务层完成标记并检查失败计数，常见标记包括：

```text
AICP_LINUX_DONE ok=<N> failed=0
AICP_STARRY_DONE ok=<N> failed=0
AICP_RTTHREAD_RELIABILITY_SUMMARY passed=8 failed=0
AICP_FREERTOS_BASELINE_DONE mode=<idle|stress> ...
```

`run_full_qemu_validation.sh` 只有所有阶段均通过时才写入：

```text
overall=PASS
```

QEMU 在部分周期基线中会由 `timeout` 主动结束，只要完成标记已出现且退出码符合脚本约定，仍属于正常结束。

## 12. 常见失败与排查

### 12.1 `debugfs`、`mkfs.ext4` 或 `e2fsck` 找不到

```sh
scripts/ai-rtos/aicp.sh doctor
export PATH="/path/to/e2fsprogs/bin:/path/to/e2fsprogs/sbin:$PATH"
export DEBUGFS="$(command -v debugfs)"
```

脚本不会扫描固定的 macOS/Homebrew 目录。应通过 `PATH` 或对应环境变量传入。

### 12.2 等待 `RTOS AICP server ready` 超时

先单独验证控制 Guest，再运行双 Guest：

```sh
scripts/ai-rtos/run_arceos_aicp_qemu_smoke.sh
scripts/ai-rtos/run_rtthread_aicp_smoke.sh 8 ai
scripts/ai-rtos/run_zephyr_aicp_smoke.sh 8 60
scripts/ai-rtos/run_freertos_aicp_guest_smoke.sh 60
```

随后检查最新 QEMU 日志中的以下层次：

1. Guest 是否启动并进入调度器。
2. virtio-net 是否发现、协商 feature 并启用 IRQ。
3. IP 地址和链路是否上线。
4. TCP 8800 是否开始监听。
5. Linux/StarryOS 是否连接到与 VM 配置一致的目标地址。

### 12.3 日志看起来在反复启动

完整验证会按阶段多次启动 QEMU，每组 Guest、fixed/ai、idle/stress、baseline/optimized 都是独立运行。查看 `stages.tsv` 可以确认当前阶段；同一阶段内部不应无限重启。

```sh
tail -f tmp/ai-rtos/results/full-validation-<timestamp>/logs/<stage>.log
```

### 12.4 YOLOv8 出现 cpuinfo 警告

QEMU 中 ONNX Runtime 可能无法完整识别虚拟 CPU 特征并打印 cpuinfo 性能警告。只要模型加载、推理、AICP CONTROL、RTOS STATUS 和完成标记均成功，该警告表示 CPU 特征检测可能降低性能，不表示推理结果无效。

### 12.5 StarryOS 能发送但收不到回复

确认主线组合使用 TCP：

```sh
AICP_STARRY_NATIVE=1 \
AICP_QEMU_NET_BACKEND=hub \
AICP_STARRY_TRANSPORT=tcp \
AICP_RTOS_GUEST=rtthread \
scripts/ai-rtos/run_axvisor_starry_rtos_aicp.sh 20 ai 300
```

UDP 日志中的 `errno=110` 表示接收超时，需结合丢包注入配置、服务端地址、ARP/MAC、重试次数和服务端完成标记判断。正式八组矩阵不依赖 UDP。

### 12.6 第三方源码检查失败

```sh
scripts/ai-rtos/check_third_party_sources_clean.sh
```

该检查要求源码位于固定 tag/commit 且工作树干净。适配代码应放在 `apps/ai-rtos-demo/`、`configs/ai-rtos/` 或独立 build 目录，不应直接修改 Zephyr、RT-Thread、FreeRTOS Kernel 或 FreeRTOS+TCP 源码。

### 12.7 实时 A/B 波动较大

QEMU 运行受宿主调度、后台进程、温控和磁盘活动影响。使用多轮交替对比，并保留每轮原始数据：

```sh
scripts/ai-rtos/run_axvisor_rt_before_after.sh 300 360 2 5
```

判断优化效果时同时查看独立 RTOS 周期抖动、Linux RTT、RTOS 服务时间和最坏值。`request interval deviation` 是闭环节拍指标，不应单独解释为 RTOS 调度退化。

## 13. 单项验证命令

修改脚本或公共库后可执行以下检查：

```sh
scripts/ai-rtos/check_shell_syntax.sh
scripts/ai-rtos/test_host_tools.sh
scripts/ai-rtos/test_run_artifacts.sh
python3 -m py_compile scripts/ai-rtos/*.py
python3 -m unittest scripts/ai-rtos/test_check_aicp_network_isolation.py
scripts/ai-rtos/check_demo_architecture.sh
scripts/ai-rtos/check_third_party_sources_clean.sh
make -C apps/ai-rtos-demo all
make -C apps/ai-rtos-demo test
```

这些检查覆盖脚本语法、工具解析、日志归档、Python 语法、网络隔离、代码边界、第三方源码洁净和 AICP 主机协议。QEMU 双 Guest 功能仍需通过 `aicp.sh smoke` 或对应单组合 runner 验证。
