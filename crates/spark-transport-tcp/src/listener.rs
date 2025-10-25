use crate::{
    TcpChannel,
    error::{self, map_io_error},
    util::{deadline_expired, run_with_context, to_socket_addr},
};
use spark_core::contract::CallContext;
use spark_core::error::CoreError;
use spark_core::transport::TransportSocketAddr;
use tokio::net::TcpListener as TokioTcpListener;

/// 对 Tokio `TcpListener` 的语义封装。
///
/// # 教案式注释
///
/// ## 意图 (Why)
/// - 在不暴露 Tokio 具体类型的前提下，提供“监听 → 接受连接”的最小能力，
///   让上层能够以 `spark-core` 的契约管理生命周期与错误分类。
/// - `accept` 会继承 [`CallContext`] 的取消与截止语义，避免监听线程阻塞。
///
/// ## 逻辑 (How)
/// - `bind`：将 [`TransportSocketAddr`] 转换为 `std::net::SocketAddr` 并调用
///   Tokio 绑定；
/// - `accept`：调用内部监听器的异步 `accept`，并通过
///   内部工具函数 `run_with_context` 注入取消/超时；
///   成功后将底层 `TcpStream` 包装为 [`TcpChannel`]；
/// - `local_addr`：返回监听器绑定地址的结构化表示。
///
/// ## 契约 (What)
/// - **前置条件**：调用方必须在 Tokio 运行时中使用该监听器；
/// - **后置条件**：`accept` 成功返回的 [`TcpChannel`] 已携带本地/对端地址，
///   并准备好进行读写；
/// - **错误语义**：绑定/接受失败时返回 [`CoreError`]，并附带稳定错误码与
///   [`ErrorCategory`](spark_core::error::ErrorCategory)。
///
/// ## 注意事项 (Trade-offs)
/// - 当前实现未支持 `SO_REUSEPORT` 等高级套接字选项，后续可在绑定前扩展；
/// - `accept` 在循环取消时依赖定时轮询，取消响应存在毫秒级延迟。
#[derive(Debug)]
pub struct TcpListener {
    inner: TokioTcpListener,
    local_addr: TransportSocketAddr,
}

impl TcpListener {
    /// 绑定到指定地址并返回监听器。
    pub async fn bind(addr: TransportSocketAddr) -> Result<Self, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let listener = TokioTcpListener::bind(socket_addr)
            .await
            .map_err(|err| map_io_error(error::BIND, err))?;
        let local = listener
            .local_addr()
            .map_err(|err| map_io_error(error::BIND, err))?;
        Ok(Self {
            inner: listener,
            local_addr: TransportSocketAddr::from(local),
        })
    }

    /// 返回监听器实际绑定的地址。
    pub fn local_addr(&self) -> TransportSocketAddr {
        self.local_addr
    }

    /// 接受一个入站连接，并根据上下文处理取消/超时。
    pub async fn accept(
        &self,
        ctx: &CallContext,
    ) -> Result<(TcpChannel, TransportSocketAddr), CoreError> {
        if deadline_expired(ctx.deadline()) {
            return Err(error::timeout_error(error::ACCEPT));
        }
        if ctx.cancellation().is_cancelled() {
            return Err(error::cancelled_error(error::ACCEPT));
        }

        let (stream, remote) = run_with_context(ctx, error::ACCEPT, self.inner.accept()).await?;
        let local_addr = stream
            .local_addr()
            .map_err(|err| map_io_error(error::ACCEPT, err))?;
        let peer_addr = TransportSocketAddr::from(remote);
        let channel =
            TcpChannel::from_parts(stream, TransportSocketAddr::from(local_addr), peer_addr);
        Ok((channel, peer_addr))
    }
}
