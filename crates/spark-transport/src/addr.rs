use alloc::{format, string::String, vec::Vec};
use core::fmt;
#[cfg(feature = "std")]
use core::net::Ipv6Addr;

/// `TransportSocketAddr` 在 `no_std` 场景下提供统一的 Socket 地址表达。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - **统一抽象**：在 TCP、QUIC、UDP 乃至内存通道之间提供一致的地址结构，避免调用方直接依赖 `std::net::SocketAddr`。
/// - **环境适配**：支持 `no_std + alloc`，使得内核态、嵌入式或 SGX 等环境可重用相同表示。
/// - **可扩展性**：保留 `non_exhaustive`，为未来扩展（如 QUIC Connection ID、WebTransport Origin）预留空间。
///
/// ## 体系定位（Architecture）
/// - 属于 `spark-transport` 基础层，被 Listener/Connection 接口与具体实现共同依赖。
/// - 取代旧版 `spark_core::transport::TransportSocketAddr`，作为跨 crate 共享的唯一地址类型。
///
/// ## 合同（What）
/// - `V4` 与 `V6` 分别表示 IPv4/IPv6，端口号使用网络序 `u16`。
/// - `Display`/`Debug` 提供稳定字符串格式，适合日志、指标标签等使用。
/// - **前置条件**：调用者需确保传入的字节序列已完成合法性校验。
/// - **后置条件**：枚举值保持不可变；格式化输出不变。
///
/// ## 设计权衡与风险（Trade-offs）
/// - 目前未对 IPv6 进行零压缩优化，优先保障可读性；若需最短表示，可在上层缓存。
/// - 暂不包含 Unix Domain Socket/管道等变体，避免在无文件系统场景增加复杂度；未来可按需扩展。
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
    /// 1. 匹配枚举，仅在 `V6` 分支执行转换；
    /// 2. 通过 `Ipv6Addr::from` 构造标准库表示。
    ///
    /// # 合同
    /// - **返回值**：当枚举为 `V6` 时返回 `Some(Ipv6Addr)`，否则为 `None`；
    /// - **前置条件**：调用方无需额外约束；
    /// - **后置条件**：不修改原地址。
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
            TransportSocketAddr::V4 { addr, port } => write!(
                f,
                "{}.{}.{}.{}:{}",
                addr[0], addr[1], addr[2], addr[3], port
            ),
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

#[cfg(feature = "std")]
impl From<TransportSocketAddr> for std::net::SocketAddr {
    fn from(addr: TransportSocketAddr) -> Self {
        match addr {
            TransportSocketAddr::V4 { addr, port } => std::net::SocketAddr::from((addr, port)),
            TransportSocketAddr::V6 { addr, port } => {
                let ipv6 = std::net::Ipv6Addr::from(addr);
                std::net::SocketAddr::from((ipv6, port))
            }
        }
    }
}
