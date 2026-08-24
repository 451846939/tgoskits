# AICP 验证入口

本目录只保留当前 `dev` 可复现的 AxVisor 多客户机智能控制验证入口。
已验证主链路为 Linux（2 vCPU）到 ArceOS、FreeRTOS 或 Zephyr（各 1 vCPU）的 AICP v1 over TCP/IP：Linux
运行轻量神经网络控制策略，控制 Guest 接收控制参数、执行控制环并回传状态。
ArceOS 与 FreeRTOS 两组使用 AxVisor 的 `virtio-net` 虚拟设备和虚拟交换机；Zephyr
组使用 QEMU hub 与分配给两个 Guest 的 legacy VirtIO-MMIO 网卡。三组主数据通道均为
TCP/IP，不使用 vsock、共享内存或 HyperCall。

顶层只有 `aicp.sh` 这一公开命令入口。`runners/`、`build/`、`checks/`、
`analysis/` 和 `lib/` 是由该入口编排的内部实现，不构成稳定的用户接口。

## 最短复现

```sh
scripts/ai-rtos/aicp.sh doctor
scripts/ai-rtos/aicp.sh prepare
scripts/ai-rtos/aicp.sh smoke
```

`smoke` 运行主机协议测试、异常重连测试和一轮实际 QEMU 闭环。成功时结果摘要
写入 `tmp/ai-rtos/results/full-validation-*/summary.txt`，QEMU 日志中必须包含：

```text
AICP_RTOS_READY
AICP Linux guest connected
AICP_LINUX_STATUS
AICP_LINUX_DONE ok=1 failed=0
```

单独运行闭环：

```sh
scripts/ai-rtos/aicp.sh run linux arceos 20 ai 300
```

FreeRTOS 使用同一 runner；它在启动时从 AxVisor 生成的 DTB 发现动态分配的
VirtIO-MMIO v2 地址与 GIC IRQ，不依赖旧的直通设备地址：

```sh
scripts/ai-rtos/aicp.sh run linux freertos 20 ai 300
```

Zephyr 使用 legacy VirtIO-MMIO 直通兼容路径，并由 QEMU hub 连接 Linux 与 Zephyr
两块分配的网卡。执行前需要准备干净的 Zephyr v4.4.0 工作树和 AArch64 bare-metal
工具链；下列变量示例必须替换为本机实际路径：

```sh
export ZEPHYR_BASE=/path/to/zephyr
export WEST=/path/to/west
export ZEPHYR_TOOLCHAIN_VARIANT=cross-compile
export CROSS_COMPILE=/path/to/aarch64-none-elf-
scripts/ai-rtos/aicp.sh run linux zephyr 20 ai 300
```

`fixed` 可替代 `ai`，用于固定参数控制基线：

```sh
scripts/ai-rtos/aicp.sh run linux arceos 20 fixed 300
```

## 验证层级

| 命令 | 覆盖范围 |
| --- | --- |
| `aicp.sh smoke` | 主机协议、超时/重连、Linux 2-vCPU 与默认 ArceOS TCP/IP 闭环、网络隔离检查 |
| `aicp.sh full` | 在 smoke 基础上增加控制效果对比、实时 A/B 与原生 RTOS 周期基线 |
| `aicp.sh realtime` | 单独执行 AxVisor 实时路径 A/B 测量 |
| `aicp.sh baseline <rtthread|zephyr|freertos|all>` | 原生 RTOS 周期基线，不宣称为 AxVisor 网络闭环 |
| `aicp.sh reliability` | 主机侧 AICP 分帧、超时、重连和异常恢复回归 |

## 网络与资源配置

| Guest | vCPU / pCPU | IPv4 / MAC |
| --- | --- | --- |
| Linux AI Guest | 2 / pCPU2、pCPU3 | `10.0.3.3/24` / `52:54:00:aa:03:03` |
| ArceOS control Guest | 1 / pCPU1 | `10.0.3.2/24` / `52:54:00:aa:03:02` |
| FreeRTOS control Guest | 1 / pCPU1 | `10.0.3.2/24` / `52:54:00:aa:03:02` |
| Zephyr control Guest | 1 / pCPU1 | `10.0.3.2/24` / `52:54:00:aa:03:02` |

pCPU0 留给 AxVisor 管理和后台工作。可通过 `AICP_HOST_CPUS`、
`AICP_LINUX_VCPU0_PCPU`、`AICP_LINUX_VCPU1_PCPU` 和
`AICP_RTOS_VCPU0_PCPU` 覆盖默认拓扑。

## 边界说明

最新上游 AxVisor 的内建 virtio-net 是 VirtIO 1.x（MMIO version 2）。FreeRTOS
已经适配新版队列寄存器、12 字节协商后的网络头，并从 DTB 获取动态 MMIO/IRQ，
可通过 `aicp.sh run linux freertos` 复现。Zephyr 仍使用 legacy VirtIO-MMIO 直通
设备，因此 runner 为它选择 QEMU hub 兼容拓扑，而不是内建 v2 虚拟交换机；它可通过
`aicp.sh run linux zephyr` 单独复现，但不在默认 `smoke` 或 `full` 矩阵中。RT-Thread
尚未完成当前 `dev` 的网络闭环适配，原生周期基线仅用于任务一对照。

构建产物、运行日志和结果均在 `tmp/ai-rtos/`，不会写入第三方 RTOS 源码树。
