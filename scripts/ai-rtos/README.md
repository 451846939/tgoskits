# AICP 验证入口

本目录只保留当前 `dev` 可复现的 AxVisor 多客户机智能控制验证入口。
主链路是 Linux（2 vCPU）到 ArceOS（1 vCPU）的 AICP v1 over TCP/IP：Linux
运行轻量神经网络控制策略，ArceOS 接收控制参数、执行控制环并回传状态。
两个 Guest 使用 AxVisor 的 `virtio-net` 虚拟设备和虚拟交换机；主数据通道
不使用 vsock、共享内存或 HyperCall。

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

`fixed` 可替代 `ai`，用于固定参数控制基线：

```sh
scripts/ai-rtos/aicp.sh run linux arceos 20 fixed 300
```

## 验证层级

| 命令 | 覆盖范围 |
| --- | --- |
| `aicp.sh smoke` | 主机协议、超时/重连、Linux 2-vCPU 与 ArceOS TCP/IP 闭环、网络隔离检查 |
| `aicp.sh full` | 在 smoke 基础上增加控制效果对比、实时 A/B 与原生 RTOS 周期基线 |
| `aicp.sh realtime` | 单独执行 AxVisor 实时路径 A/B 测量 |
| `aicp.sh baseline <rtthread|zephyr|freertos|all>` | 原生 RTOS 周期基线，不宣称为 AxVisor 网络闭环 |
| `aicp.sh reliability` | 主机侧 AICP 分帧、超时、重连和异常恢复回归 |

## 网络与资源配置

| Guest | vCPU / pCPU | IPv4 / MAC |
| --- | --- | --- |
| Linux AI Guest | 2 / pCPU2、pCPU3 | `10.0.3.3/24` / `52:54:00:aa:03:03` |
| ArceOS control Guest | 1 / pCPU1 | `10.0.3.2/24` / `52:54:00:aa:03:02` |

pCPU0 留给 AxVisor 管理和后台工作。可通过 `AICP_HOST_CPUS`、
`AICP_LINUX_VCPU0_PCPU`、`AICP_LINUX_VCPU1_PCPU` 和
`AICP_RTOS_VCPU0_PCPU` 覆盖默认拓扑。

## 边界说明

最新上游 AxVisor 的内建 virtio-net 是 VirtIO 1.x（MMIO version 2）。此前
RT-Thread、Zephyr 和 FreeRTOS 的旧网络 Guest 依赖 legacy VirtIO-MMIO 直通
设备，不能与当前上游设备模型混用。因此它们不在 `aicp.sh run`、`smoke`、
`full` 或 PR CI 中声明为已验证闭环；保留的原生周期基线仅用于任务一对照。

构建产物、运行日志和结果均在 `tmp/ai-rtos/`，不会写入第三方 RTOS 源码树。
