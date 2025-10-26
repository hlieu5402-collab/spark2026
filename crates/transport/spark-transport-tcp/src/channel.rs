use crate::{
    backpressure::BackpressureState,
    error::{self, CONFIGURE, map_io_error},
    util::{deadline_expired, deadline_remaining, run_with_context, to_socket_addr},
};
use socket2::SockRef;
use spark_core::{
    context::ExecutionContext,
    contract::CallContext,
    error::CoreError,
    status::ready::{PollReady, ReadyCheck, ReadyState},
    transport::{ShutdownDirection, TransportSocketAddr},
};
use std::{
    io::{self, IoSlice},
    net::Shutdown as StdShutdown,
    ops::DerefMut,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream as TokioTcpStream,
    sync::Mutex as AsyncMutex,
};

/// TCP 套接字级配置项，实现对内核行为的显式控制。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 将“优雅关闭需等待对端 EOF、超时后通过 RST 释放资源”这一契约显式配置化，
///   避免调用方直接操作 `socket2` 或平台相关常量；
/// - 为未来扩展更多套接字选项（如 `TCP_NODELAY`、`SO_KEEPALIVE`）预留统一入口。
///
/// ## 体系定位（Architecture）
/// - 该结构位于传输实现层，对 `TcpChannel` 的构造及检查流程提供只读依赖；
/// - `TcpListener::accept_with_config`、`TcpChannel::connect_with_config` 使用它决定每条
///   连接的 `SO_LINGER` 行为，从而影响关闭阶段的资源回收时序。
///
/// ## 核心逻辑（How）
/// - `linger` 字段存储超时时长；当值为 `Some(dur)` 时，通过 `socket2::SockRef::set_linger`
///   `SO_LINGER`，使得 `close`/`drop` 阶段在 `dur` 后未完成就发送 RST；
/// - `None` 表示遵循内核默认策略，通常为“立即返回并由内核异步完成发送”。
///
/// ## 契约说明（What）
/// - `with_linger`：输入 `Option<Duration>`，`Duration` 必须为非负值；返回新的配置实例；
/// - `linger`：读取当前配置值；
/// - **前置条件**：调用 `apply` 前，`TokioTcpStream` 必须已成功创建；
/// - **后置条件**：若 `apply` 返回 `Ok(())`，则套接字选项已落地，失败时原配置不生效。
///
/// ## 设计取舍与注意事项（Trade-offs）
/// - `SO_LINGER` 在不同平台的精度不同（Linux 取整到秒），测试与生产环境需选择合适超时；
/// - 若设置过小，可能导致仍在发送缓冲区的数据被丢弃并触发对端 `ECONNRESET`；
/// - 目前仅封装 `linger`，未来扩展需注意保持向后兼容与 API 对称性。
#[derive(Clone, Debug, Default)]
pub struct TcpSocketConfig {
    linger: Option<Duration>,
}

impl TcpSocketConfig {
    /// 创建默认配置，等价于 `linger = None`。
    pub const fn new() -> Self {
        Self { linger: None }
    }

    /// 设置 `SO_LINGER` 超时时长。
    pub fn with_linger(mut self, linger: Option<Duration>) -> Self {
        self.linger = linger;
        self
    }

    /// 读取当前配置的超时时长。
    pub fn linger(&self) -> Option<Duration> {
        self.linger
    }

    fn apply(&self, stream: &TokioTcpStream) -> io::Result<()> {
        let sock = SockRef::from(stream);
        sock.set_linger(self.linger)
    }
}

#[derive(Debug)]
struct TcpChannelInner {
    stream: AsyncMutex<TokioTcpStream>,
    backpressure: Mutex<BackpressureState>,
    peer_addr: TransportSocketAddr,
    local_addr: TransportSocketAddr,
    config: TcpSocketConfig,
}

/// TCP 通道的最小实现，封装读写、半关闭与背压探测。
///
/// # 教案式注释
///
/// ## 意图 (Why)
/// - 为上层 Handler 提供对单个 TCP 连接的直接控制，同时贯彻
///   `CallContext` 的取消/超时语义；
/// - 在无须了解 Tokio 具体类型的情况下，完成字节流读写与半关闭。
///
/// ## 逻辑 (How)
/// - 内部以 `tokio::sync::Mutex` 包裹 `TcpStream`，确保多线程调用 `&self`
///   方法时的互斥；
/// - 读写操作通过内部工具函数 `run_with_context` 注入取消
///   与截止时间；
/// - 背压信号由内部 `BackpressureState` 统计 `WouldBlock` 与锁竞争次数，并映射为
///   [`ReadyState`]；
/// - [`spark_core::transport::ShutdownDirection`] 封装标准库的半关闭语义，便于调用方显式声明关闭方向。
///
/// ## 契约 (What)
/// - `connect`：根据 `CallContext` 建立到目标地址的连接；
/// - `read`/`write`/`writev`：执行一次 IO 操作，返回实际读写字节数；
/// - `shutdown`：执行半关闭；
/// - `poll_ready`：无阻塞地返回当前写路径的背压状态；
/// - `peer_addr`/`local_addr`：提供结构化的地址元数据。
///
/// ## 注意事项 (Trade-offs)
/// - 由于使用互斥锁序列化读写，无法与 `TcpStream::split` 一样实现真正的
///   全双工；若需要高并发，可在未来引入独立的读/写半部实现；
/// - `poll_ready` 的取消响应通过检查 `ExecutionContext` 的截止与取消标志，
///   若调用频率极低可能感知不及时；
/// - `writev` 目前只执行一次 vectored 写入，如需完全写满需上层循环调用。
#[derive(Clone, Debug)]
pub struct TcpChannel {
    inner: Arc<TcpChannelInner>,
}

/// 将通道拆解为裸 `TcpStream` 与地址元数据的结果结构。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 支持 TLS/QUIC 等更高层协议在握手阶段直接控制底层 `TcpStream`。
/// - 保留本地与对端地址，使握手完成后重建的加密通道仍能复用原有元数据。
///
/// ## 契约（What）
/// - `stream`：原始 Tokio `TcpStream`；
/// - `local_addr`：监听端地址；
/// - `peer_addr`：远端地址；
/// - **前置条件**：调用方已经放弃对原 `TcpChannel` 的其他克隆；
/// - **后置条件**：所有权完全转移至该结构体，由上层决定后续处理方式。
#[derive(Debug)]
pub struct TcpChannelParts {
    pub stream: TokioTcpStream,
    pub local_addr: TransportSocketAddr,
    pub peer_addr: TransportSocketAddr,
}

impl TcpChannel {
    pub(crate) fn from_parts(
        stream: TokioTcpStream,
        local_addr: TransportSocketAddr,
        peer_addr: TransportSocketAddr,
        config: TcpSocketConfig,
    ) -> Result<Self, CoreError> {
        config
            .apply(&stream)
            .map_err(|err| map_io_error(CONFIGURE, err))?;
        Ok(Self {
            inner: Arc::new(TcpChannelInner {
                stream: AsyncMutex::new(stream),
                backpressure: Mutex::new(BackpressureState::new()),
                peer_addr,
                local_addr,
                config,
            }),
        })
    }

    /// 根据上下文建立到目标地址的连接。
    pub async fn connect(ctx: &CallContext, addr: TransportSocketAddr) -> Result<Self, CoreError> {
        Self::connect_with_config(ctx, addr, TcpSocketConfig::default()).await
    }

    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 为调用方提供在建连阶段即可指定套接字行为（如 `linger`）的能力，确保后续
    ///   优雅关闭遵循一致策略；
    /// - 避免上层再重复封装 `socket2`，降低错误配置风险。
    ///
    /// ## 契约（What）
    /// - `ctx`：携带取消/截止语义的 [`CallContext`]；
    /// - `addr`：目标地址；
    /// - `config`：本次连接使用的 [`TcpSocketConfig`]，其中 `linger=None` 表示沿用内核默认；
    /// - **前置条件**：`ctx` 未取消且截止未过期；
    /// - **后置条件**：成功返回的通道已应用 `config` 并可立即读写。
    ///
    /// ## 实现逻辑（How）
    /// - 通过 `run_with_context` 把建连过程与 `ctx` 绑定，继承取消/超时；
    /// - 成功后设置本地/对端地址并调用 `config.apply` 写入 `SO_LINGER`；
    /// - 失败时将错误映射为 [`CoreError`]，错误码覆盖“连接失败”“配置失败”两类。
    ///
    /// ## 注意事项（Trade-offs）
    /// - `SO_LINGER` 仅在 `close`/`drop` 时生效，建连成功后修改需要重新创建通道；
    /// - 若 `config` 设置了很短的超时，调用方需确保优雅关闭在该时间内完成，否则对端将收到 RST。
    pub async fn connect_with_config(
        ctx: &CallContext,
        addr: TransportSocketAddr,
        config: TcpSocketConfig,
    ) -> Result<Self, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let stream =
            run_with_context(ctx, error::CONNECT, TokioTcpStream::connect(socket_addr)).await?;
        let local = stream
            .local_addr()
            .map_err(|err| map_io_error(error::CONNECT, err))?;
        let peer = stream
            .peer_addr()
            .map_err(|err| map_io_error(error::CONNECT, err))?;
        Self::from_parts(
            stream,
            TransportSocketAddr::from(local),
            TransportSocketAddr::from(peer),
            config,
        )
    }

    /// 读取数据到缓冲区。
    pub async fn read(&self, ctx: &CallContext, buf: &mut [u8]) -> Result<usize, CoreError> {
        run_with_context(ctx, error::READ, async {
            let mut guard = self.inner.stream.lock().await;
            guard.read(buf).await
        })
        .await
    }

    /// 将整个缓冲区写入套接字。
    pub async fn write(&self, ctx: &CallContext, buf: &[u8]) -> Result<usize, CoreError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len();
        let written = run_with_context(ctx, error::WRITE, async {
            let mut guard = self.inner.stream.lock().await;
            guard.write_all(buf).await.map(|_| len)
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(written)
    }

    /// 使用 vectored IO 写入多个缓冲区。仅执行一次写入尝试。
    pub async fn writev(
        &self,
        ctx: &CallContext,
        bufs: &[IoSlice<'_>],
    ) -> Result<usize, CoreError> {
        if bufs.is_empty() {
            return Ok(0);
        }
        let written = run_with_context(ctx, error::WRITE_VECTORED, async {
            let mut guard = self.inner.stream.lock().await;
            guard.write_vectored(bufs).await
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(written)
    }

    /// 返回构造时使用的套接字配置。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 在调试或测试场景下快速确认某条连接继承的 `linger` 等策略，避免通过
    ///   平台工具逐一排查；
    /// - 为未来的观测指标提供数据来源，可据此统计不同配置的关闭耗时分布。
    ///
    /// ## 契约（What）
    /// - 返回值为 [`TcpSocketConfig`] 的引用，仅反映构造时的静态配置；
    /// - **前置条件**：通道已成功构造；
    /// - **后置条件**：不会触发 IO，也不会修改内部状态。
    ///
    /// ## 注意事项（Trade-offs）
    /// - 该方法不读取内核实时状态；若外部在运行时修改了 `SO_LINGER`，需结合
    ///   [`TcpChannel::linger`] 校验实际值。
    pub fn config(&self) -> &TcpSocketConfig {
        &self.inner.config
    }

    /// 查询底层套接字当前的 `SO_LINGER` 设置。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 验证配置是否已经被内核接受，便于在 TCK 或运维排障时检测平台差异；
    /// - 当运行时支持动态调整套接字选项时，可用于观测最终状态。
    ///
    /// ## 契约（What）
    /// - 返回 `Option<Duration>`：`Some(dur)` 表示在 `dur` 秒后发送 RST；`None`
    ///   表示沿用内核默认策略；
    /// - **前置条件**：调用方需保证通道仍持有有效的内核句柄；
    /// - **后置条件**：不会改变套接字状态，失败时返回 [`CoreError`] 并保持原状。
    ///
    /// ## 注意事项（Trade-offs）
    /// - Linux 会将 `Duration` 向下取整到秒，测试断言需据此选择阈值；
    /// - 若调用时通道已被其他线程并发关闭，可能返回 `Err`，应结合关闭流程处理。
    pub async fn linger(&self) -> Result<Option<Duration>, CoreError> {
        let guard = self.inner.stream.lock().await;
        SockRef::from(&*guard)
            .linger()
            .map_err(|err| map_io_error(CONFIGURE, err))
    }

    /// 根据方向执行半关闭。
    pub async fn shutdown(
        &self,
        ctx: &CallContext,
        direction: ShutdownDirection,
    ) -> Result<(), CoreError> {
        run_with_context(ctx, error::SHUTDOWN, async {
            let mut guard = self.inner.stream.lock().await;
            match direction {
                ShutdownDirection::Write => AsyncWriteExt::shutdown(&mut *guard).await,
                ShutdownDirection::Read => sync_shutdown(&guard, StdShutdown::Read),
                ShutdownDirection::Both => {
                    AsyncWriteExt::shutdown(&mut *guard).await?;
                    sync_shutdown(&guard, StdShutdown::Read)
                }
            }
        })
        .await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(())
    }

    /// 执行契约化的优雅关闭流程：发送 FIN、等待对端 EOF 再释放资源。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 满足“先写半关闭、等待对端确认后再释放资源”的协议契约，确保数据不会
    ///   在关闭过程中被截断；
    /// - 为 [`GracefulShutdownCoordinator`](spark_core::host::shutdown::GracefulShutdownCoordinator)
    ///   等上层组件提供可等待的半关闭原语。
    ///
    /// ## 体系位置（Architecture）
    /// - `GracefulShutdownCoordinator` 在触发 `close_graceful` 时会调用该方法；
    /// - 方法内部复用 `run_with_context`，因此会继承 `CallContext` 的取消/超时语义。
    ///
    /// ## 契约（What）
    /// - `ctx`：关闭流程的上下文视图；
    /// - **前置条件**：调用方必须确保不会在关闭过程中继续写入数据；
    /// - **后置条件**：对端 EOF 被确认后返回 `Ok(())`，若超时/取消则返回对应的
    ///   [`CoreError`]；函数返回后即可安全地 `drop` 通道。
    ///
    /// ## 逻辑解析（How）
    /// 1. 先复用 [`TcpChannel::shutdown`] 触发写半关闭（发送 FIN）；
    /// 2. 调用内部的 `await_peer_half_close` 循环读取直至获得对端 EOF；
    /// 3. 若读取过程中出现 `Interrupted`/`WouldBlock`，会自动重试；其它错误会被
    ///    映射为 `CoreError`。
    ///
    /// ## 设计取舍（Trade-offs）
    /// - 为避免长期持有锁阻塞其他调用，读取阶段每次尝试都会释放互斥锁；
    /// - 若对端长时间不发送 FIN，等待将受 `ctx.deadline` 限制，届时建议结合
    ///   `TcpSocketConfig::linger` 触发 RST。
    pub async fn close_graceful(&self, ctx: &CallContext) -> Result<(), CoreError> {
        self.shutdown(ctx, ShutdownDirection::Write).await?;
        self.await_peer_half_close(ctx).await?;
        if let Ok(mut state) = self.inner.backpressure.lock() {
            state.on_ready();
        }
        Ok(())
    }

    /// 获取对端地址。
    pub fn peer_addr(&self) -> TransportSocketAddr {
        self.inner.peer_addr
    }

    /// 获取本地地址。
    pub fn local_addr(&self) -> TransportSocketAddr {
        self.inner.local_addr
    }

    /// 将通道尝试拆解为 [`TcpChannelParts`]。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - TLS 握手阶段需要直接操作底层 `TcpStream`，通过本方法可在保持连接连续性的同时交由
    ///   上层协议驱动；
    /// - 若拆解失败（例如通道已被克隆），返回原始 `TcpChannel`，调用方可决定是否降级或延后
    ///   握手，避免出现“半拆解”导致的资源泄露。
    ///
    /// ## 逻辑（How）
    /// - 使用 `Arc::try_unwrap` 检查是否存在唯一所有者；
    /// - 成功时获取内部互斥锁并调用 `into_inner` 取得 `TcpStream`；
    /// - 将地址字段连同 `TcpStream` 一并返回，便于 TLS 层继续记录观测数据。
    ///
    /// ## 契约（What）
    /// - 返回 `Ok(parts)` 表示拆解成功，原通道不再可用；
    /// - 返回 `Err(self)` 表示仍有其他持有者，调用方仍可继续以明文方式使用通道；
    /// - **前置条件**：调用方必须确保没有未完成的读写操作；
    /// - **后置条件**：成功拆解后，内部互斥锁与背压状态将被丢弃。
    pub fn try_into_parts(self) -> Result<TcpChannelParts, Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => {
                let stream = inner.stream.into_inner();
                Ok(TcpChannelParts {
                    stream,
                    local_addr: inner.local_addr,
                    peer_addr: inner.peer_addr,
                })
            }
            Err(inner) => Err(Self { inner }),
        }
    }

    /// 检查写通道的即时背压状态。
    pub fn poll_ready(&self, ctx: &ExecutionContext<'_>) -> PollReady<CoreError> {
        if deadline_expired(ctx.deadline()) {
            return Poll::Ready(ReadyCheck::Err(error::timeout_error(error::POLL_READY)));
        }
        if ctx.cancellation().is_cancelled() {
            return Poll::Ready(ReadyCheck::Err(error::cancelled_error(error::POLL_READY)));
        }

        if let Some(remaining) = deadline_remaining(ctx.deadline())
            && remaining.is_zero()
        {
            return Poll::Ready(ReadyCheck::Err(error::timeout_error(error::POLL_READY)));
        }

        let mut state = match self.inner.backpressure.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.refresh();

        match self.inner.stream.try_lock() {
            Ok(guard) => match guard.try_write(&[]) {
                Ok(_) => {
                    state.on_ready();
                    Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let ready_state = state.on_would_block();
                    Poll::Ready(ReadyCheck::Ready(ready_state))
                }
                Err(err) => {
                    drop(guard);
                    let core_error = map_io_error(error::POLL_READY, err);
                    Poll::Ready(ReadyCheck::Err(core_error))
                }
            },
            Err(_) => {
                let ready_state = state.on_manual_busy();
                Poll::Ready(ReadyCheck::Ready(ready_state))
            }
        }
    }

    async fn await_peer_half_close(&self, ctx: &CallContext) -> Result<(), CoreError> {
        run_with_context(ctx, error::READ, async {
            let mut guard = self.inner.stream.lock().await;
            read_until_eof(guard.deref_mut()).await
        })
        .await
    }
}

fn sync_shutdown(stream: &TokioTcpStream, direction: StdShutdown) -> io::Result<()> {
    let sock = SockRef::from(stream);
    sock.shutdown(direction)
}

async fn read_until_eof(stream: &mut TokioTcpStream) -> io::Result<()> {
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => return Ok(()),
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(err),
        }
    }
}
