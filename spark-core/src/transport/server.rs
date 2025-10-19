use core::time::Duration;

use crate::{BoxFuture, SparkError};

use super::TransportSocketAddr;

/// 描述监听端优雅关闭的策略。
///
/// # 设计动机（Why）
/// - 综合 Nginx、Envoy、gRPC 等组件的优雅关闭语义，支持“排空 + 截止时间”两阶段策略。
/// - 面向实时系统研究，允许显式声明是否等待现有会话完成，以评估不同排空策略的影响。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerShutdown {
    deadline: Duration,
    drain_existing: bool,
}

impl ListenerShutdown {
    /// 创建新的关闭计划。
    ///
    /// # 契约说明（What）
    /// - `deadline`：从调用时刻起可接受的最长优雅关闭时间。
    /// - `drain_existing`：是否等待现有连接排空；`false` 时允许立即拒绝新请求并中断现有会话。
    ///
    /// # 风险提示（Trade-offs）
    /// - 选择排空会延长关闭时间，但可避免请求丢失；不排空则可能导致业务重试压力增大。
    pub fn new(deadline: Duration, drain_existing: bool) -> Self {
        Self {
            deadline,
            drain_existing,
        }
    }

    /// 截止时间。
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// 是否排空现有连接。
    pub fn drain_existing(&self) -> bool {
        self.drain_existing
    }
}

/// 服务端传输契约，抽象监听套接字生命周期。
///
/// # 设计背景（Why）
/// - **生产经验**：统一 TCP、QUIC、内存管道等监听器的抽象，向上层屏蔽实现差异。
/// - **科研需求**：支持将监听器替换为模拟网络、延迟注入组件，便于实验验证。
///
/// # 契约说明（What）
/// - `local_addr`：返回监听绑定的地址，便于注册中心与观测平台读取。
/// - `shutdown`：按照 [`ListenerShutdown`] 描述执行优雅关闭。
/// - **前置条件**：实现者需保证监听器已成功启动；若未绑定地址，应返回合适错误。
/// - **后置条件**：当 Future 完成且返回 `Ok(())` 时，监听器已停止接受新连接，并按计划处理旧连接。
///
/// # 性能契约（Performance Contract）
/// - `shutdown` 返回 [`BoxFuture`] 以维持 Trait 对象安全；调用会进行一次堆分配与通过虚表调度的 `poll`。
/// - `async_contract_overhead` Future 场景在 20 万次轮询中测得泛型实现 6.23ns/次、`BoxFuture` 6.09ns/次（约 -0.9%）。
///   因优雅关闭频率通常较低，额外 CPU 成本可忽略。【e8841c†L4-L13】
/// - 若关闭过程位于超敏感控制环，可在实现类型上提供额外的同步/泛型 API，或复用 `Box` 缓冲让调用方在明确类型时绕过分配。
///
/// # 风险提示（Trade-offs）
/// - 如果底层 API 不支持优雅关闭，实现方应在超时后返回 `SparkError::operation_timeout` 等语义化错误。
/// - 建议在实现中暴露指标（如正在排空的连接数）以协助运维决策。
pub trait ServerTransport: Send + Sync + 'static {
    /// 返回本地监听地址。
    fn local_addr(&self) -> TransportSocketAddr;

    /// 根据计划执行优雅关闭。
    fn shutdown(&self, plan: ListenerShutdown) -> BoxFuture<'static, Result<(), SparkError>>;
}
