#![doc = r#"
# spark-transport-udp

## 模块使命（Why）
- **统一 UDP 通路**：为 Spark 运行时提供一套围绕 Tokio `UdpSocket` 的轻量封装，使上层能够通过 `spark-core` 的抽象以一致方式访问无连接传输能力。
- **SIP 互操作诉求**：面向 SIP 协议的 `rport` 语义进行解析与回写，保证经由 NAT 的终端也能可靠接收响应。
- **NAT Keepalive 基石**：暴露回源路由（`UdpReturnRoute`）描述，使业务侧能够基于最近一次报文持续发送心跳维持 NAT 映射。

## 核心契约（What）
- `UdpEndpoint` 负责套接字生命周期管理，并提供 `recv_from`/`send_to` 的异步接口。
- `UdpReturnRoute` 用于描述回应路径及是否需要在 SIP `Via` 头中填充 `rport`。
- 约束：调用方必须运行在 Tokio 多线程运行时，且所有报文按 UTF-8 解析 SIP 头（遇到非法 UTF-8 将优雅退化为原样转发）。

## 实现策略（How）
- 绑定及收发直接委托给 Tokio `UdpSocket`，并通过 `TransportSocketAddr` 与 `std::net::SocketAddr` 互转保持与 `spark-core` 协调。
- `recv_from` 在读取后解析首个 `Via` 头提取 `rport`，生成回源路由；`send_to` 在必要时重写 `rport` 并把报文发送至 NAT 可达端口。
- 解析与改写均采用纯字节扫描，以避免正则表达式开销，并明确记录大小写不敏感匹配逻辑，兼顾性能与可读性。
"#]
#![cfg_attr(
    not(feature = "runtime-tokio"),
    doc = r#"## 功能开关：`runtime-tokio`

默认启用 Tokio UDP 实现；在最小依赖或文档编译时可通过禁用默认特性跳过 Tokio。
停用后本 crate 仅保留契约描述，不会编译具体的 UDP 传输代码。

启用方式：`spark-transport-udp = { features = ["runtime-tokio"] }` 或沿用默认特性。
"#
)]

#[cfg(feature = "runtime-tokio")]
pub mod batch;

#[cfg(feature = "runtime-tokio")]
mod tokio_runtime;

#[cfg(feature = "runtime-tokio")]
pub use tokio_runtime::*;

pub use spark_core::transport::ShutdownDirection;
