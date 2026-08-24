# AI/RTOS 多客户机控制演示

该目录保存 AICP 应用层协议、Linux 智能计算程序、ArceOS/FreeRTOS/RT-Thread/Zephyr 控制服务、YOLOv8 推理程序、原生 RTOS 基线和主机测试工具。当前最新 `dev` 已实际验证 Linux 分别与 ArceOS、FreeRTOS、RT-Thread、Zephyr 的虚拟化闭环。

## 工程分层

| 目录 | 层次 | 唯一职责 |
| --- | --- | --- |
| `aicp/` | 公共协议层 | C 版 AICP v1 线格式、TCP 字节流与 UDP 数据报编解码、POSIX 适配和客户端事务 |
| `rtos-core/` | 公共控制层 | 与操作系统无关的服务状态机、重复/乱序处理和控制环 |
| `ai-model/` | 公共模型层 | 轻量神经网络、目标轨迹和 fixed/AI 控制参数映射 |
| `linux-init/` | AI Guest 适配层 | Linux/StarryOS 共用 PID 1；Linux 使用默认 profile，StarryOS 使用 `starry_profile.h` |
| `linux-client/`、`rust-client/` | 用户态客户端 | 主机/Linux 上的 C、Rust AICP 客户端与协议互操作验证 |
| `yolov8-onnx-cpu/`、`yolov8-rust-onnx/` | AI 推理层 | C++/Rust YOLOv8n ONNX Runtime CPU 推理和 AICP 输出 |
| `yolov8-init/` | AI Guest 启动层 | 在 Linux Guest 中启动 YOLOv8 客户端并管理退出状态 |
| `freertos/` | FreeRTOS glue | FreeRTOS+TCP、VirtIO、GIC/IRQ、启动代码和 AICP 服务接入 |
| `rtthread-aicp/` | RT-Thread glue | RT-Thread SAL/lwIP、VirtIO/IRQ 和 AICP 服务接入 |
| `zephyr/` | Zephyr glue | Zephyr socket、网络配置和可选 AxVisor legacy VirtIO profile |
| `rtthread-baseline/`、`zephyr-baseline/` | RTOS 基线 | 与虚拟化测试同周期、同负载口径的原生 RTOS 测量程序 |
| `host-tools/`、`model-runner/`、`tests/` | 主机验证工具 | 参考服务端、控制效果对比以及协议/客户端/服务回归测试 |

`aicp/aicp_client.c` 是 Linux PID1、普通 C Client、C++ YOLOv8 和板端 RKNN 共用的客户端事务核心，统一处理 HELLO、CONTROL_SET、STATUS/ERROR、序号校验和 RTT 采样。`aicp/aicp_stream.h` 定义平台无关的 TCP 字节流能力，`aicp/aicp_datagram.h` 统一 UDP 单报文编解码和 CRC 校验；POSIX、RT-Thread、Zephyr、FreeRTOS 只在各自适配层连接实际 socket API。

`ai-model/control_policy.c` 是轻量控制演示的唯一参数映射实现。Linux Client、Linux/StarryOS PID 1 和单机对照程序共同调用它，固定参数基线与神经网络路径不会因复制代码而产生公式漂移。各 RTOS 目录中保留的 socket、网络设备、IRQ 和日志代码是平台适配，不复制协议解析或控制状态机。

Rust 公共协议 crate 位于 `components/aicp-protocol/`；ArceOS 服务端位于 `apps/arceos/aicp-server/`。第三方 RTOS 源码由构建脚本下载到 `tmp/`，不在此目录复制或修改。

## 快速验证

主机协议与服务状态机：

```sh
make -C apps/ai-rtos-demo clean all test
scripts/ai-rtos/aicp.sh reliability
```

结构检查会验证公共层不存在 OS 特判、三种原生 RTOS glue 均接入同一服务状态机、Linux/StarryOS 与 C++ YOLOv8 共用客户端事务核心、旧重复入口已经移除，并阻止第三方 RTOS 源码进入演示目录。

当前 QEMU 双 Guest最小闭环：

```sh
scripts/ai-rtos/aicp.sh smoke
```

包含闭环、实时 A/B、三种原生 RTOS 周期基线、可靠性和控制效果的完整验证：

```sh
scripts/ai-rtos/aicp.sh full
```

已验证的统一入口：

| 智能计算 Guest | 控制 Guest | 主数据通道 |
| --- | --- | --- |
| Linux（2 vCPU） | ArceOS（1 vCPU） | AICP v1 over TCP/IP |
| Linux（2 vCPU） | FreeRTOS（1 vCPU） | AICP v1 over TCP/IP |
| Linux（2 vCPU） | RT-Thread（1 vCPU） | AICP v1 over TCP/IP |
| Linux（2 vCPU） | Zephyr（1 vCPU） | AICP v1 over TCP/IP |

ArceOS 与 FreeRTOS 使用 AxVisor 的 VirtIO-MMIO v2 虚拟设备和虚拟交换机。RT-Thread
与 Zephyr 使用 QEMU hub 连接两块分配给 Guest 的 legacy VirtIO-MMIO 设备；三组
均以 TCP/IP 作为主数据通道，但后两组兼容路径不宣称使用 AxVisor 内建 v2 虚拟交换机。

所有新增代码采用 Apache-2.0 许可证。
