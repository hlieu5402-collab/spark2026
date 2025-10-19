use crate::{BoxFuture, SparkError, pipeline::PipelineFactory};
use alloc::format;
use alloc::{boxed::Box, collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::{fmt, time::Duration};

/// `SparkSocketAddr` 在 `no_std` 环境中表示通用 Socket 地址。
///
/// # 设计背景（Why）
/// - 避免直接依赖 `std::net::SocketAddr`，以便在嵌入式或 unikernel 环境部署。
/// - 使用枚举存储原始字节，确保序列化时无需动态分配。
///
/// # 契约说明（What）
/// - `V4`、`V6` 分别表示 IPv4、IPv6；暂未包含 UDS，可在上层扩展。
/// - Display 实现遵循人类可读格式，方便日志输出。
///
/// # 风险提示（Trade-offs）
/// - 未对 IPv6 地址进行零压缩优化，如需更紧凑表示可在外部自行处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SparkSocketAddr {
    /// IPv4 地址。
    V4 { addr: [u8; 4], port: u16 },
    /// IPv6 地址。
    V6 { addr: [u16; 8], port: u16 },
}

impl fmt::Display for SparkSocketAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SparkSocketAddr::V4 { addr, port } => write!(
                f,
                "{}.{}.{}.{}:{}",
                addr[0], addr[1], addr[2], addr[3], port
            ),
            SparkSocketAddr::V6 { addr, port } => {
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
impl From<std::net::SocketAddr> for SparkSocketAddr {
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

/// `ParamMap` 为 Endpoint 附带的键值参数集合。
///
/// # 设计背景（Why）
/// - 统一管理可选参数（超时、认证方式、负载均衡策略），便于传输实现解析。
/// - 选择 `BTreeMap` 保证遍历顺序稳定，方便调试。
///
/// # 契约说明（What）
/// - 所有键值均为 UTF-8 字符串；解析方法根据不同类型进行转换。
/// - 当前实现示例解析逻辑简化为整数/布尔/毫秒，真实实现可替换。
///
/// # 风险提示（Trade-offs）
/// - 解析失败时返回 `None`，调用者需提供默认值或错误处理。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ParamMap(BTreeMap<String, String>);

impl ParamMap {
    /// 创建空参数表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入键值对。
    pub fn insert(&mut self, key: String, value: String) {
        self.0.insert(key, value);
    }

    /// 获取字符串值。
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|value| value.as_str())
    }

    /// 获取布尔值。
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get_str(key)
            .and_then(|value| value.parse::<bool>().ok())
    }

    /// 获取整数值。
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get_str(key)
            .and_then(|value| value.parse::<u64>().ok())
    }

    /// 获取持续时间，当前约定单位为毫秒。
    pub fn get_duration(&self, key: &str) -> Option<Duration> {
        self.get_u64(key).map(Duration::from_millis)
    }

    /// 获取字节大小，当前简单按十进制字节解析。
    pub fn get_size_bytes(&self, key: &str) -> Option<u64> {
        self.get_u64(key)
    }
}

/// `Endpoint` 将逻辑协议与物理地址绑定。
///
/// # 设计背景（Why）
/// - 在分布式场景中，`srv://service` 等逻辑地址需要与 `tcp://host:port` 等物理地址统一表达。
/// - 通过参数表扩展可选配置，减少接口破碎化。
///
/// # 契约说明（What）
/// - `scheme` 指明协议或解析方式，`host` 可为域名或逻辑服务名，`port` 为物理端口。
/// - `params` 允许实现者挂载额外调度信息（权重、超时）。
///
/// # 风险提示（Trade-offs）
/// - 若 `scheme` 为 `srv`，调用者应结合服务发现组件解析后再建立连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    scheme: String,
    host: String,
    port: u16,
    params: ParamMap,
}

impl Endpoint {
    /// 构造新的端点。
    pub fn new(scheme: String, host: String, port: u16, params: ParamMap) -> Self {
        Self {
            scheme,
            host,
            port,
            params,
        }
    }

    /// 返回协议方案。
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// 返回主机名或服务名。
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 返回端口号。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 返回参数集合。
    pub fn params(&self) -> &ParamMap {
        &self.params
    }
}

/// `ServerTransport` 抽象监听端口。
///
/// # 设计背景（Why）
/// - 统一封装监听套接字生命周期，支持宿主在 `no_std` 环境下自定义实现。
///
/// # 契约说明（What）
/// - `local_addr` 必须返回监听时绑定的地址，便于观测。
/// - `close_graceful` 应在截止时间内停止接受新连接，并通知现有通道进入排空阶段。
///
/// # 风险提示（Trade-offs）
/// - 若底层 API 不支持优雅关闭，实现者需模拟并在截止超时时返回 `SparkError`。
pub trait ServerTransport: Send + Sync + 'static {
    /// 返回本地绑定地址。
    fn local_addr(&self) -> SparkSocketAddr;

    /// 发起优雅关闭。
    fn close_graceful(&self, deadline: Duration) -> BoxFuture<'static, Result<(), SparkError>>;
}

/// `TransportFactory` 统一构造不同传输协议的入口。
///
/// # 设计背景（Why）
/// - 通过统一工厂接口，便于在运行时选择 TCP、QUIC 或内存通道等实现。
/// - `srv://` 语义要求与服务发现集成，因此接口额外接受 `ServiceDiscoveryProvider`。
///
/// # 契约说明（What）
/// - `bind` 与 `connect` 均返回 `BoxFuture`，确保 `no_std + alloc` 环境下的异步能力。
/// - 若 `endpoint.scheme()` 不匹配工厂 `scheme()`，实现应直接返回 `SparkError`。
///
/// # 风险提示（Trade-offs）
/// - 连接过程可能涉及多次重试或 DNS 解析，调用方应结合 `ParamMap` 中的超时参数防止阻塞。
pub trait TransportFactory: Send + Sync + 'static {
    /// 返回支持的 scheme。
    fn scheme(&self) -> &'static str;

    /// 绑定服务端端点。
    fn bind(
        &self,
        endpoint: Endpoint,
        pipeline_factory: Arc<dyn PipelineFactory>,
    ) -> BoxFuture<'static, Result<Box<dyn ServerTransport>, SparkError>>;

    /// 连接客户端端点。
    fn connect(
        &self,
        endpoint: Endpoint,
        discovery: Option<Arc<dyn crate::distributed::ServiceDiscoveryProvider>>,
    ) -> BoxFuture<'static, Result<Box<dyn crate::pipeline::Channel>, SparkError>>;
}
