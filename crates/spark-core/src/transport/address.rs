use alloc::{format, string::String, vec::Vec};
use core::fmt;
#[cfg(feature = "std")]
use core::net::Ipv6Addr;

/// `TransportSocketAddr` 在 `no_std` 场景下提供统一的 Socket 地址表达。
///
/// # 设计初衷（Why）
/// - **生产实践吸收**：对齐 gRPC、NATS、Envoy 等平台常用的 IPv4/IPv6 表示，避免绑定到 `std::net::SocketAddr`，以便运行在 unikernel、DPDK 或 SGX 等环境。
/// - **前沿演进支撑**：预留未来扩展空间（如 QUIC Connection ID、WebTransport Origin），通过枚举让实验性变体按需添加。
///
/// # 契约定义（What）
/// - `V4`/`V6` 分别存储 IPv4 与 IPv6 原始字节；端口号使用网络序 `u16`。
/// - `Display`/`Debug` 提供人类可读格式，便于可观测性平台解析。
/// - **前置条件**：调用方需确保地址来自可信来源或已进行校验。
/// - **后置条件**：格式化输出稳定，可用于日志聚合或指标标签。
///
/// # 设计取舍与风险（Trade-offs）
/// - 未对 IPv6 进行零压缩优化，优先保证直观可读；若需最小化字符串长度，可在上层做缓存。
/// - 暂未内建 Unix Domain Socket/管道支持，避免在 `no_std` 环境引入额外依赖；可在未来新增枚举变体。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum TransportSocketAddr {
    /// IPv4 地址。
    V4 { addr: [u8; 4], port: u16 },
    /// IPv6 地址。
    V6 { addr: [u16; 8], port: u16 },
}

impl TransportSocketAddr {
    /// 将 IPv6 地址从 8 段转换为 `Ipv6Addr`，便于上层需要时与标准库交互。
    ///
    /// # 逻辑解析（How）
    /// 1. 将内部 `u16` 数组直接传入 `Ipv6Addr::from` 构造。
    /// 2. 该方法在 `no_std` 场景下不可用，因此标记为 `cfg(feature = "std")`。
    #[cfg(feature = "std")]
    pub fn as_ipv6_addr(&self) -> Option<Ipv6Addr> {
        match self {
            Self::V6 { addr, .. } => Some(Ipv6Addr::from(*addr)),
            _ => None,
        }
    }
}

impl fmt::Display for TransportSocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportSocketAddr::V4 { addr, port } => {
                write!(
                    f,
                    "{}.{}.{}.{}:{}",
                    addr[0], addr[1], addr[2], addr[3], port
                )
            }
            TransportSocketAddr::V6 { addr, port } => {
                let segments: Vec<String> = addr
                    .iter()
                    .map(|segment| format!("{:x}", segment))
                    .collect();
                write!(f, "[{}]:{}", segments.join(":"), port)
            }
        }
    }
}

#[cfg(feature = "std")]
impl From<std::net::SocketAddr> for TransportSocketAddr {
    fn from(addr: std::net::SocketAddr) -> Self {
        match addr {
            std::net::SocketAddr::V4(v4) => Self::V4 {
                addr: v4.ip().octets(),
                port: v4.port(),
            },
            std::net::SocketAddr::V6(v6) => Self::V6 {
                addr: v6.ip().segments(),
                port: v6.port(),
            },
        }
    }
}
